# Native Code Mode: ToolProgram (runtime action 30)

`RuntimeActionId::ToolProgram` implements protocol version 1 on the existing
extension runtime-action ABI. No MCP connection, subprocess model, or second
assistant turn is involved. Older hosts reject action 30 explicitly.

This documents the **implemented first full-parity increment**, not the complete
parity plan. The companion extension requires four exact version-1 capabilities;
large-result transport, complete child progress/history/recovery and Web/ACP
child cards/reload remain outstanding.

A tool receives opaque `__rpi.runtimeHandle` and `__rpi.runtimeKey` from its native
driver. Forward the handle as runtime-action user_data and the key as the reserved
`__rpiRuntimeKey` argument. Keep both out of guest JavaScript, model-visible
results, and logs. A registration pointer, guessed key, or retired bridge is not
a tool invocation. Synchronous callbacks inherit their actual driver identity;
foreign workers authenticate the captured invocation, including child subsets.

Operations:

- `list`: `{op:"list", offset?:0, limit?:32}` returns
  `{protocolVersion:1, capabilities:{resultEnvelope:1,hostAccounting:1,parentProgress:1,losslessValues:1},
   tools:[{name,description,parameters}], nextOffset}`.
  Pages allow 1..128 entries; catalog maximum is 4096 entries and 1 MiB.
  The executing tool and ancestor names are excluded; this is not 100% of the
  native full tool set. Catalogs match native batch boundaries. Each invocation
  uses its batch's effective tool snapshot; later invocations see legitimate
  native batch updates, not authority granted by `addedToolNames`.
- `start`: `{op:"start", name, arguments:{...}, timeoutMs?:30000}` returns
  `{requestId,status:"running"}`. Deadlines allow 1..120000 ms. IDs are host-owned.
- `poll`: `{op:"poll",requestId}` returns running, done, or failed. Done includes
  `isError` and the finalized result with content, details, terminate,
  addedToolNames, and usage (null when absent). Failed denotes a lifecycle error,
  cancellation, timeout, panic, or oversized result, not a normal tool error.
- `cancel` and `dispose`: signal cancellation and return cancelled/disposed.
  Disposal is idempotent and never unloads live foreign code. Cancellation does
  not imply rollback. Cleanup remains possible on an authenticated live binding
  after its parent completes/cancels; new work does not.

Limits are 128 starts and 8 active native request futures across an invocation
and its descendants, 16 retained requests per invocation, and 1 MiB JSON
arguments/results (reject, do not truncate). Cancelled/disposed workers retain
native ownership until their execution future stops. Native plugin drivers keep
their existing library lifetime/cleanup ownership through foreign destroy.

## Execution and persistence

`rpi-agent::tool_program::ToolProgram` is a host-owned capability captured from
the *executing* loop's tools, context, configuration, hooks, cancellation, and
child execution scope. CLI dispatch must never reconstruct it from the root
registry or JSON tool names. Tools retain their own project environment/cwd.
The legacy names-only GetActiveTools action also uses this effective snapshot.

Nested calls reuse prepare_arguments, schema validation, before/after hooks, and
tool execution. A lifecycle-only emitter prevents synthetic nested ToolExecutionStart or
MessageEnd events from entering the ordinary settlement/transcript stream.
Sibling requests use a per-invocation read/write gate: Sequential calls exclude
parallel siblings, and a Sequential loop serializes every sibling. The executing
parent program itself does not take that gate, avoiding self-deadlock.
`start` therefore fails closed unless the executing tool declares Sequential or
its loop is Sequential. This applies at every nesting level, so independent
program groups in a model batch cannot bypass ordering. Old SDK plugins can
opt in without an ABI change using the JSON Schema extension keyword
`parameters["x-rpi-execution-mode"] = "sequential"` in their raw tool registration.
The native adapter reads this keyword; unknown/missing values remain Parallel.

Cancellation/deadline publishes a failed poll result promptly, cancels the native
signal, and continues polling the same execution future through cleanup. Disposing
a request does not release its active permit or gate. The outer program also
cancels and drains its descendants before returning, so a Sequential peer cannot
start while a previous group's foreign destroy callback is still executing.
The composition owns a child cancellation token: closing its plugin binding
retires nested work without cancelling the containing tool's native image
postprocessing. The containing native token still retires after execute and
postprocessing finish. Caller cancellation propagates down through the tree.
Noncooperative tools can consequently hold the gate indefinitely (fail closed).
Rust tool-execution panics also cancel and drain descendants at the
capability-owning layer before unwinding to the existing panic handler; an
exception cannot release the containing gate early. Finalized termination is
latched before fallible reply serialization/bounds, so an oversized terminating
result cannot reopen admission.

The harness requires an already checkpointed **Never-replay** root parent.
Replay-Safe parents fail closed before the nested effect. The ordinary parent
ToolStarted record remains the only assistant-associated tool-start record.
Before each authorized nested effect, the harness acknowledges a bounded custom
`rpi.tool_program.intent.v1` entry naming run/root/parent/request/tool/effective
arguments. Successful execution adds `rpi.tool_program.finished.v1` audit metadata.
These custom entries are not fabricated assistant/tool-result messages and do
not alter provider history. Interrupted parent programs remain unknown outcomes
and are never automatically replayed. No automatic retry is provided.

## Native result envelope and host accounting

Raw registration schemas opt into strict Done decoding with
`parameters["x-rpi-result-envelope"] = 1`. Both the original polling driver and
wake-capable driver accept this payload without changing any SDK ABI slot:

```json
{"$rpiToolResult":1,"isError":true,"result":{"content":[{"type":"text","text":"bounded error summary"}],"details":{"summary":"retained"},"terminate":false,"usage":null,"addedToolNames":[]}}
```

`content`, `details` and boolean `terminate` are required; `usage` (nullable)
and `addedToolNames` are optional. The envelope requires version 1 and boolean
`isError`. Only text and image blocks are accepted, with typed text/MIME/base64
fields. The complete envelope is bounded to 1 MiB and content to 256 blocks;
name metadata is bounded to 256 names of at most 256 UTF-8 bytes each. Malformed
opted-in payloads fail closed. All non-opted-in Done parsing and ABI Err handling
remain unchanged. An extension must negotiate **all four exact integer version-1**
list capabilities before guest execution; an older action-30 host might support protocol 1 without
these additive semantics. Older hosts without action 30 still reject it.

Failed envelopes become native `AgentError::StructuredTool(Box<AgentToolResult>)`.
The agent loop preserves the native content, details, termination and metadata
while marking the outcome erroneous, **before** running ordinary after hooks.
The native image wrapper normalizes successful and structured-error image
content through the same pipeline while preserving error status and metadata.
Those hooks retain their existing final authority over content/details, usage,
termination and error status. Infrastructure failures can still use ABI Err.
Oversized extension errors should compact their summaries without converting
failure to success or discarding termination.

The host retains finalized direct-child usage, added tool names and termination
exactly once, before reply encoding, independently of polling, disposal or guest
return values. After descendant drain it merges these into the containing native
outcome, including ordinary and structured errors. Cancellation/deadline results
published early do not prevent retaining metadata finalized during cleanup.
Oversized result frames likewise do not discard accounting or reopen termination
admission. Each nesting level rolls up only its own direct children; **extensions
must not sum or re-emit child usage as their own usage**. Added names are metadata,
not permission to refresh or widen the effective catalog. Panic outcomes without
finalized results remain lifecycle failures, with no replay.

Accounting uses checked i64 additions (including optional reasoning/cache-write
fields) and finite cost sums. Overflow/non-finite usage becomes unavailable and
the containing outcome becomes an error; it never panics. Names are deduplicated
and sorted. An invalid/oversized name set is omitted as a whole, with a native
accounting error, rather than retaining a completion-order-dependent prefix.
Available valid usage and termination remain intact. After hooks can override
these outcomes exactly as for direct tool execution. Tools that compose no
children retain their own metadata unchanged, including original name order.

## Lossless business values and native output hygiene

`losslessValues:1` preserves legitimate business schema/argument/result properties
named `runtimeKey`, `runtimeHandle`, `__rpi` and `__rpiRuntimeKey`, including nested
data. There is no blanket recursive property-name blacklist. The native extension
ABI still owns injected top-level `__rpi`, exactly as for direct plugin calls;
it is not a business argument slot.

An invocation-local public-output guard captures actual injected metadata and
credential values. Both polling and wake drivers sanitize callbacks, Pending
payloads, legacy Done, opted-in envelopes including structured errors, and plain
tool errors. Exact injected metadata objects and credential values, including
ordinary serialized JSON and aliases/collisions, are guarded without stripping
unrelated business properties. Ordinary bare numbers are not blanket-redacted.
This is output hygiene, **not a sandbox against malicious native plugins**:
arbitrary encoding and direct plugin I/O are unguarded.

## Parent-associated progress

Nested work reports through the **existing outer native update callback**,
captured only after its normal completion/cancellation gate exists. It does not
emit synthetic nested tool starts, messages or transcript results. Partial
content is empty; `details.toolProgram` carries bounded `rootId`, `parentId`,
`requestId`, `tool`, `phase` (`started`, `progress`, `completed`, `failed`) and
`updateCount`. Identity strings are limited to 256 characters. No nested raw
arguments, invocation credentials, partial payloads or images are forwarded.
The ordinary outer event retains its normal native tool identity and arguments.

An entire invocation tree shares 256 projected updates / 64 KiB of encoded
lifecycle metadata. Raw callback count is retained even if projections hit the
budget. Cancellation, completion and late callbacks are suppressed through the
native gates, and observer panics cannot release a worker before cancellation
drain. Retained native callbacks keep only a weak reference to their program,
so storing a late callback does not create a registry ownership cycle.
Action polling and SDK ABI are unchanged.

Native content selection (`tools.emit` and returned native content objects) is
implemented by the companion Code Mode extension: selected image bytes occur
once in outer native content, not in repeated JSON summaries. Intermediate
images are not automatically promoted. This host preserves selected native
blocks; it does not infer images from arbitrary nested JSON.

The companion extension additionally implements
`tools.result(nativeResultOrErrorResult)`: a captured WeakMap associates finalized
native child objects (including caught `error.result`) with authoritative
`isError`. Exactly one selection is allowed, without `tools.emit` or returning
another content object. Fabricated/cloned inputs, conflicting/multiple selections
and invalid serialization cause sticky failure even if caught. Normal completion
keeps exact supported content/details/isError, with no summary in selected details
and no synthetic error text appended to exact errors. Ordinary return/content
emission behavior is unchanged. Usage/names/termination stay host-authoritative;
a later throw/cancel/timeout/OOM/lifecycle failure/terminal child wins. The
extension remains inline at 128 blocks / 1 MiB content, plus the 1 MiB envelope;
this is not large-result support.

Selection uses the current guest-mutated content/details snapshot, not an
immutable copy. Missing selected `details` (including deletion or `undefined`)
is normalized to native `null` in the required result-envelope field.
Authoritative native `isError` remains independent of guest mutations.

## Validation and current boundaries

Focused tests cover invocation authentication/staleness, narrowed catalogs,
ancestor denial, blocked approvals, schema errors, finalized hooks, deadlines,
cancellation/cleanup, limits, Sequential-parent liveness, real persistence intent
before effect, transcript integrity, and double-restore without replay. Existing
agent and harness recovery/identity suites are regression coverage.

Invocation groups do not share a process-global gate across unrelated agent
loops; ordering follows the native loop's Sequential batch semantics. Starting
program tools must opt into those semantics as described above. Foreign native code remains
trusted: cancellation cannot forcibly stop arbitrary DLL code or undo effects.
Tests additionally cover image/usage/terminate finalization, custom-marker storage
faults, shared descendant active limits, and child execution scope propagation.
Live DLL reload races still need additional integration coverage. The new
interruption integration test uses in-memory storage; existing recovery regression
tests exercise JSONL and SQLite as well.

Three new neutral-layer differential tests compare direct and nested execution:
business JSON and native text signatures, before/after-hook rewrites/finalized
results, and effective catalogs across native batch boundaries. Announced names
do not authorize calls. These tests do not qualify a real DLL/provider or UI.

Complete child progress and original native history snapshots/recovery are not
implemented by bounded lifecycle updates and intent/finish audit markers. Values
over 1 MiB still need host-backed references/chunks/attachments. Web/ACP full child
cards/reload are outstanding; a third UI dev worktree is approved, but no UI
changes are part of this increment. Protected self/ancestor exclusion remains.

## Real native smoke

Build this host and the companion `rpi-package` checkout's `rpi-codemode` and
`rpi-permission-gate` libraries. Optionally build the
[test-only parity fixture](../../scripts/fixtures/codemode-parity/README.md)
for trusted accounting and native partial-callback coverage:

```sh
cargo build --locked --manifest-path scripts/fixtures/codemode-parity/Cargo.toml \
  --target-dir target/codemode-parity-fixture
```

Then run with explicit local artifact paths (never install the fixture):

```sh
python -B scripts/codemode-smoke.py --rpi /path/to/rpi \
  --extension /path/to/rpi_codemode.dll \
  --permission-gate /path/to/rpi_permission_gate.dll \
  --fixture-extension target/codemode-parity-fixture/debug/codemode_parity_fixture.dll \
  --evidence-dir /new/empty/evidence-directory
```

Use `.so` / `.dylib` on Linux/macOS. The smoke starts only an isolated native
RPC process and a bounded loopback provider fixture (no real model credentials).
It creates fresh agent settings/workspace/transcripts, rejects the nested write
approval explicitly via RPC, and retains binary SHA-256, commands, RPC logs,
provider requests, messages and `results.json`. Existing evidence directories
are rejected rather than overwritten. The approval case is explicitly skipped
if no gate DLL is supplied.

**Previous-increment evidence only:** local Windows validation passed **25 cases**
with freshly built native host,
Code Mode DLL, test fixture and permission gate. Coverage includes the original
ten safety/composition cases plus real PNG return/emission, image output after
JS failure, no implicit image forwarding, malformed text signatures, sticky
emission failure, ordinary JSON and forged accounting, both guest-size and
final-envelope-size limits, and real ABI accounting on success/JS failure/caught
native error/terminal error. The image-capable loopback provider verifies native
image delivery; terminal failure must not trigger another model turn. Fixture
partials verify parent-linked lifecycle/count progress without synthetic calls.
Checks also confirm durable nested markers, bounded message reads, no orphan
provider results and no persisted invocation capabilities. Evidence is in local
`target/codemode-parity-native-smoke-5/`; it is not a release asset.

This is local source-build qualification, not installation/deployment, a
performance benchmark, live-provider validation, or proof that Linux/macOS
native integration/reload races have passed. Existing CI configuration is not
evidence that the new increment is green.
Real native host-plus-DLL qualification recorded above remains Windows-only.

**Current-increment local evidence:** fresh actual Windows host + Code Mode DLL,
test-only fixture and permission gate passed **35 real smoke cases**: 20 base,
14 fixture and one denial. Ten additions over the original 25 cover direct
success/error baselines, exact success/error selection, deleted/undefined selected
details normalized to native null, business JSON, actual binding success/error
echoes and helper-name aliases (`call/list/emit/result` via `tools.call`).
Final `results.json` reports PASSED for all 35 cases; `provenance.json` retains
all four artifact SHA-256 values. Evidence directory: `target/codemode-full-parity-smoke-5/`.
Earlier full-parity runs `-1/`, `-2/` and `-4/` failed; `-3/` passed 33 cases
before the final missing-details fix. They are not the final qualification.
A read-only review found that omitted selected details produced an invalid
required-field envelope; engine/final-envelope normalization to null is covered
by unit checks and two real cases in the final run.

Host `cargo test --locked -p rpi-agent -p rpi-extensions -p rpi-plugin-sdk -p rpi-harness`
passed **1109 tests, 6 ignored, 0 failed**; log: `target/codemode-full-parity-tests.log`.
Host `cargo fmt --all -- --check` passed. Package evidence is **72 passing tests**
and strict all-target Clippy passed with
`cargo clippy --locked -p rpi-codemode --all-targets -- -D warnings`.
This is Windows local source-build qualification, not deployment, installation,
live-provider or Linux/macOS integration qualification, or a new CI-green claim.
