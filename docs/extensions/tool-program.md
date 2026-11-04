# Native Code Mode: ToolProgram (runtime action 30)

`RuntimeActionId::ToolProgram` implements protocol version 1 on the existing
extension runtime-action ABI. No MCP connection, subprocess model, or second
assistant turn is involved. Older hosts reject action 30 explicitly.

This documents the **implemented third full-parity increment**, not the complete
parity plan. The companion extension requires all **six exact integer v1**
capabilities: resultEnvelope, hostAccounting, parentProgress, losslessValues,
resultReferences and the additive sixth contentAttachments. SDK pin **0.3.20**
and native ABI/layout are unchanged. Older/mismatched hosts fail closed without
fallback. Non-reference legacy clients/non-opted tools keep their old behavior.
Complete child events/history/recovery and Web/ACP cards/reload remain outstanding.

A tool receives opaque `__rpi.runtimeHandle` and `__rpi.runtimeKey` from its native
driver. Forward the handle as runtime-action user_data and the key as the reserved
`__rpiRuntimeKey` argument. Keep binding metadata, keys and private request IDs out of guest/model APIs
and logs. A numeric handle alone is not a credential or authorization. A registration pointer, guessed key, or retired bridge is not
a tool invocation. Synchronous callbacks inherit their actual driver identity;
foreign workers authenticate the captured invocation, including child subsets.

## Latest native observation seam (not durable history)

The host now provides an **optional native observation seam**, not a production
child journal or new RPC capability. It observes genuine requested/prepared
arguments, original signed partials and the actual post-hook final result.
Existing private `tp-*` request/progress/audit identity is unchanged; a future
native durable entry ID is separate. Default emitters return no observer, so the
normal CLI still has **no installed child-history writer**. Six negotiated Code
Mode capabilities and the SDK ABI/layout remain unchanged.

`AgentEmitter::tool_program_observer()` is forwarded through native settling,
frame and extension tee wrappers. Nested captures inherit their real parent;
guest JSON cannot select lineage or an observer. At the harness execution
barrier, effective arguments are observed only after the existing Never-replay
checkpoint ACK and before effects. Prepared-observer errors, including future
construction/poll panics, refuse effects and report a harness persistence error.

The driver retains finalized usage/names/termination **before** fallible observer
finish/serialization. An optional atomic admission/retirement gate drains accepted
in-flight callbacks before finish/close; saved callbacks hold only weak runtime
and writer references. One absolute timeout covers work and writer shutdown.
Cancellation/timeout replies remain prompt even during close, but the same native
work/close future and active permits are retained through drain. One tee observer
is forwarded; identical shared owner Arcs are deduplicated and distinct owners
fail closed before either begin. Ordinary event fan-out is unchanged.

The observer owns partially-created resources until begin returns success, and
must clean them up on error/panic. Close must drain/join even when returning an
error. These contracts and mock failure tests do **not** prove cleanup by an
as-yet-unimplemented production writer.

Qualification: default-parallel SDK/agent/extensions/harness **1,146 passed,
0 failed, 6 ignored**, twice; logs `target/codemode-native-observation-tests-final-repeat.log`
and `target/codemode-native-observation-tests-qualified-repeat.log`. This includes
**18 new tests** (10 agent observation, 6 real-harness observation, 2 tee), not
additional counts to sum again. Tests cover after-hook authority, prepared
checkpoint ordering, denial/invalid preparation, retained accounting/termination,
weak ownership, panic/failure, in-flight updates, and cancellation during close.
An independent second review accepted the optional seam after the four discovered
lifecycle/composition/panic issues were fixed.

Fresh Windows host plus unchanged Code Mode DLL/fixture/gate passed the existing
**59/59 cases**: `pi-rust-codemode/target/codemode-native-observation-smoke-1/{results.json,provenance.json}`.
All four actual artifact hashes were independently verified. This smoke proves
unchanged normal host/DLL behavior, **not production child archival capture**.
Native workspace fmt and CLI build pass. Strict native all-target Clippy remains
blocked by existing agent/extensions/harness diagnostics; the two introduced
manual-inspect diagnostics were fixed. Package runtime/checkpoint remains the
unchanged **124 tests plus strict all-target Clippy**. No deployment, installation,
production restart, live-provider, Linux/macOS or full-parity claim.

**Still pending:** native primary-session durable body/record writer, bounded
lazy queries and explicit capabilities/events, interruption/restart/fork and
snapshot/live continuity, neutral extcore/thin HTTP/ACP integration, and Web
cards/reload/reconnect/HMR in the approved `rpi-package-dev` worktree. Do not infer
any of these from this seam or the old intent/finish markers.

## Historical native result-codec checkpoint

The committed native codec follow-up adds camelCase `Serialize` for `AgentToolResult` and
untagged `Serialize` for `TextContentOrImage`. ToolProgram reference mode now
serializes the borrowed native result without an intermediate payload `Value`;
native RPC uses the same canonical serializer, fixing dropped optional
`textSignature` in `ToolExecutionUpdate` / `ToolExecutionEnd`. The native field
is `Option<String>`: even JSON-formatted signature strings retain their exact
string bytes, not a parsed/reformatted object. Result wire fields are unchanged
except this additive optional RPC field. Reference JSON now follows canonical
struct field order; byte-identical object-key order across the upgrade is **not**
promised. No `Deserialize` or new authority is added; `isError` remains external
to the result and host-authoritative. Native SDK ABI/layout and the Code Mode
DLL/source are unchanged; that DLL was not rebuilt for this follow-up.

This is a codec foundation for a **future native child journal**, not a fourth
history increment or full child-history parity. Actual child events/records,
bounded history/query/replay/recovery, neutral core/adapter integration and UI
remain pending. No install, deployment or running-service upgrade is implied.

Historical codec Windows evidence:
`pi-rust-codemode/target/codemode-native-codec-smoke-2/{results.json,provenance.json}`: **59 cases PASSED, all true**, with all four
artifact hashes verified. SDK/agent/extensions/harness passed **1,128 tests,
0 failed, 6 ignored, twice**; focused CLI protocol tests passed **8** (not the
full CLI suite). Package **124 tests and strict all-target Clippy remain passing**
from unchanged package evidence; **native strict Clippy is blocked**, not green.
See the [latest matrix checkpoint](../../../rpi-package-codemode/docs/codemode-full-parity-matrix.md#historical-native-result-codec-checkpoint)
for hashes, logs, focused codec checks and native lint blockers. The earlier
third-increment 59-case / 1,123-test evidence and `5dba…` host hash are
historical, not qualification of the rebuilt host.

## Operations

| Operation | Request and response |
| --- | --- |
| `list` | `{op:"list",offset?:0,limit?:32}` → `{protocolVersion:1,capabilities,tools:[{name,description,parameters}],nextOffset:number\|null}`. Only the effective active invocation tool set is visible; self/ancestors are excluded. |
| `start` | `{op:"start",name,arguments:object,timeoutMs?:30000,resultMode?:"reference"}` → `{requestId,status:"running"}`. Identity is host-generated. Omitting resultMode preserves the original inline native behavior and its 1 MiB frame bound. Other supplied modes are rejected. The extension always starts in reference mode. |
| `poll` | `{op:"poll",requestId}` → running, done or failed. Original inline done contains `{requestId,status:"done",isError,result:{content,details,terminate,addedToolNames,usage}}`. Reference-mode done contains `{requestId,status:"done",isError,resultReference:{version:1,id,byteLength,contentBlocks,isError,terminate}}`, not an inline result. Failed denotes lifecycle/bridge failure, not a finalized ordinary tool error. |
| `read_result` | `{op:"read_result",resultId,offset?:0,limit?:65536}` → `{resultId,offset,data,byteLength,nextOffset:number\|null}`. Offset is a strict nonnegative integer UTF-8 **byte** boundary at or before EOF; limit is a strict integer **4..65536**. Data ends at a UTF-8 boundary, at most limit bytes. Reads are stable and resumable while retained; EOF reads return empty data and null nextOffset. Past-EOF/nonboundary offsets fail. Each encoded response is at most 1 MiB. |
| `measure_result_content` | `{op:"measure_result_content",resultId,indices?:null\|[nonnegative integers]}` → `{resultId,contentBlocks,encodedBytes}`. Omitted/null selects all, `[]` none; reorder/repeats are valid. Native indices are bounded to 256, validated for range and owning live invocation. `encodedBytes` sums canonical serialized **selected blocks**, excluding array brackets/commas; repeated blocks count repeatedly. Uses `serde_json::to_writer` with a byte counter, not a huge temporary allocation. |
| `release_result` | `{op:"release_result",resultId}` → `{resultId,status:"released"}`. Only the owning invocation may release; unknown/already released IDs fail. Releases storage, not accounting or native side effects. |
| `cancel` / `dispose` | `{op,requestId}` → cancelled/disposed. Disposal of a request is **independent** of its finalized result reference: it does not release the result. Cleanup remains available while the binding is valid. Neither operation implies rollback. |

Limits are 128 starts and 8 active native request futures across an invocation
and its descendants, 16 retained requests per invocation, and 1 MiB arguments/
action frames (reject, do not truncate). Reference payloads stay native; retention
and final attachment bounds are separate below. Cancelled/disposed workers retain
native ownership until their execution future stops. Native plugin drivers keep
their existing library lifetime/cleanup ownership through foreign destroy.

Native catalogs allow 4096 entries / 1 MiB with pages of 1..128. The extension
uses the tighter 1024-entry discovery bound. Each invocation uses its effective
native batch snapshot; added names do not authorize tools or refresh the catalog.

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

In the inline variant, `content`, `details` and boolean `terminate` are required; `usage` (nullable)
and `addedToolNames` are optional. The envelope requires version 1 and boolean
`isError`. Only text and image blocks are accepted, with typed text/MIME/base64
fields. The complete envelope is bounded to 1 MiB and inline content to 256 blocks;
name metadata is bounded to 256 names of at most 256 UTF-8 bytes each. Malformed
opted-in payloads fail closed. All non-opted-in Done parsing and ABI Err handling
remain unchanged. The current extension must negotiate **all six exact integer
version-1** list capabilities before guest execution; an older action-30 host might support protocol 1 without
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
unrelated business properties. Ordinary bare numbers are not blanket-redacted. Numeric runtimeHandle is not a
credential: masking handle 3 corrupted the real `32 MiB` diagnostic; that bug
is fixed while actual keys and private request IDs remain guarded.
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

## References, guest results and whole-result selection

The extension starts in `resultMode:"reference"`. At most **16 retained refs per
program**, sharing **64 MiB invocation-tree storage**, are charged as:
`2*serialized_bytes + 4096 + 128*details_nodes + 256*content_blocks`.
Quota implies serialized JSON below 32 MiB; the extension also guards advertised
size at 32 MiB. This is **not a hard RSS cap**: native clone/serialization may
precede admission and Rust JSON/bridge buffers are outside the 32 MiB JS heap.

Nonterminal results ≤1 MiB materialize as mutable guest values, immediately
releasing the native ref. tools.result snapshots current supported content/details
(deleted/undefined details normalize to null), with captured WeakMap native error
authority. Larger results are frozen lazy WeakMap receipts: field access reads
bounded chunks and parses/deep-freezes the **whole** receipt in that same heap,
not field streaming or unlimited memory. Inspection may exhaust the guest heap.

`tools.result(receipt)` forwards one complete native outcome without JS payload
reads, using `{$rpiToolResult:1,isError,resultReference:{version:1,id}}`.
It is mutually exclusive with inline result and contentParts; native error
identity is checked. Both drivers resolve through the captured live owning
invocation before destroy. Unknown/foreign/stale/released/cancelled/mismatched
refs fail closed. Exact selection adds no summary or synthetic error block;
cloned/fabricated identities, repeated selection or emit mixing fail sticky.
Later throw/OOM/deadline/cancellation/lifecycle failure discards whole selection.

Release invalidates large receipt reading/forwarding, including cached access;
small authenticated release is a native no-op. Request dispose is independent.
Terminal children override selection/emits without resuming JS and attach their
original native result. Finish cancels/drains and clears retained references.
Usage/names/termination roll up once from finalized direct children after drain;
attachment cannot duplicate accounting or authorize tools. Native after hooks
retain authority. Helper aliases call/list/emit/result/release use tools.call.

## Ordered native content attachments (third increment)


`tools.emit(value, indices?)` accepts a genuine large WeakMap receipt without
reading content/details into JS or transporting its native payload through FFI.
Captured native operations measure the selected content, then retain ordered
metadata-only parts. Small ordinary values remain mutable and emit inline.
Multiple large receipts and generated inline blocks can interleave; subset,
reorder, repeated indices and empty selection are valid. Repeated emission of
one receipt still represents **one executed direct child**, not extra accounting.
There is no public/private ID-based authority forgery.

```json
{"$rpiToolResult":1,"isError":false,"result":{"details":{"status":"completed"},"terminate":false},"contentParts":[{"content":[{"type":"text","text":"before"}]},{"reference":{"version":1,"id":"host-owned-id"},"indices":[2,1,0,1]}]}
```

Here `result.details` is the bounded **engineSummary**, not child-native details;
native content/base64 and private attachment parts are not duplicated into it.
With `contentParts`, `result.content` is forbidden and whole `resultReference`
is mutually exclusive. Inline parts allow only `content`; reference parts allow
only `reference` and optional `indices`, with exact reference keys `version:1`
and nonempty `id`. Omitted/null indices select all; `[]` selects no blocks.
The parser produces typed inline/reference parts and resolves them in order
through the captured **live ActionBridge before destroy in both drivers**.
Cancelled, expired, foreign, released or otherwise unverifiable bindings/IDs
fail closed. Native owner/range/cancellation checks run again at final resolution.

Unlike `tools.result`, content emission does **not** select native error/details
authority. Emitting a caught native error's content can complete with outer
`isError:false`; native error accounting is still retained once. `tools.result`
continues to select exactly **one** authoritative native content/details/error
outcome, cannot mix with emit, and is discarded on later failure.

### Indices and bounds

Both large-reference and inline guest paths validate raw indices **before
serialization**: undefined/null means all; otherwise an array of at most **128
nonnegative integer primitives** is required. Length and each index are read
once into a new frozen null-prototype snapshot. Captured intrinsics, property
definition and JSON primitive-number encoding avoid Array prototype setters.
Custom toJSON, NaN/Infinity, scalar or boxed objects cannot widen selection to
all. Native selection also validates range. Invalid directives latch failure,
even when JS catches the exception; no subsequent tool start is permitted.

Native attachment limits are **256 parts / 256 blocks** and **32 MiB final
serialized content array**, checking each block's serialized cost **before each
clone**, including duplicates. Measurement excludes array punctuation; final
budgeting includes brackets/commas. The extension is stricter: **128 selected
parts / blocks** and a combined **32 MiB array including 4096 reserved bytes**
for a final diagnostic block (reason bounded to **512 UTF-8 bytes**, including
worst-case JSON escaping in the reserve). Inline-generated output remains
**128 blocks / 1 MiB** and the final metadata envelope separately stays **1 MiB**.
Native payload is not an inline frame. The unchanged 16-reference / shared
64 MiB retention quota and charge above still apply; these are **not RSS caps**.

### Failure, pins and handoff

Explicit emitted receipts are **pinned**: release attempts latch failure before
native free. Eligible explicit parts survive JS throw, OOM or deadline with
outer error and a compact **last** diagnostic, only while their owning native
binding remains live. Cancellation/expiry/unverifiable ownership cannot attach.
A terminal child overrides **all** emits and whole selection, releases unrelated
receipts, and attaches its original native outcome without resuming JS.

An ordinary host budget/action error is not necessarily owner expiry. After
cleanup, an uncertain binding can regain **attachment eligibility only** by
exact read-only `measure_result_content` requests with `indices:[]` for **every
distinct already-emitted receipt** (at most 16). Every reply must have exactly
matching resultId, contentBlocks:0, encodedBytes:0 and bounded frame shape/size.
All probes must pass; sticky failure and the original root error remain.
This does **not** retry the failed operation, start tools, read payloads, replay
effects or grant new authority. Cancellation, whole-result selection, terminal
handoff and known retained-reference cleanup failure never use this recovery.
Native final decoding checks live ownership again.

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

Complete native child identity/order/updates/final events and bounded original
native history/query/replay/recovery with snapshot/live continuity are not
implemented by bounded lifecycle updates and intent/finish audit markers.
Large values now use bounded host-backed references/chunks/attachments;
multiple/mixed large content output is implemented within the limits above.
Multiple whole-result selection remains unsupported. Web/ACP full child
cards/reload are outstanding; the approved `D:/Projects/rpi-package-dev` worktree
on `dev/web-hmr` remains untouched, and no UI changes are part of this increment. Protected self/ancestor exclusion remains.

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

**Historical pre-first-increment evidence only:** local Windows validation passed **25 cases**
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

**Historical FIRST-increment local evidence (not current):** fresh actual Windows host + Code Mode DLL,
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

## Historical third-increment qualification (before codec follow-up)


| Evidence | Proven scope / limits |
| --- | --- |
| Fresh Windows host + Code Mode DLL + fixture + gate | `pi-rust-codemode/target/codemode-content-parts-native-smoke-2/results.json`: **PASSED, 59 cases, all true**. All four provenance hashes below verified against actual local artifacts. |
| 13 new native smoke cases; previous 46 retained | Direct mixed baseline; large text/image emit; two separate large calls interleaved with inline; subset `[2,1,0,1]` (same image twice, one execution); caught large error content with completed outer result; emit then JS throw; pinned release; 32 MiB measurement overflow preserving first PNG; negative indices; NaN scalar sticky failure with no subsequent write; toJSON/prototype resistance; terminal overrides earlier PNG with two child executions. |
| Payload / history proof boundary | Provider validates actual image bytes for **each copy**, including the two-image case, before normalized direct-baseline comparisons. Exact native end ordering/hashes, engineSummary without base64/private attachment parts, intent/finish lifecycle and bounded final snapshots checked. **Not full native history parity** or live-provider qualification. |
| Package checks | **124 passed, 0 failed, 0 ignored**; strict all-target Clippy and fmt passed. Logs: package `target/codemode-content-parts-{tests,clippy}.log`. |
| Broad native regression | Default-parallel SDK/agent/extensions/harness: **1,123 passed, 0 failed, 6 ignored**, twice. Native `target/codemode-content-parts-tests.log` and `target/codemode-content-parts-tests-repeat.log`. Existing both-driver full-reference 22-scenario test and new content-parts scenario test are **test functions**, not separate extra scenario counts. |
| Failed content-parts attempt | `codemode-content-parts-native-smoke-1` **FAILED after 53 cases, before overflow**: masking numeric handle 3 corrupted the real `32 MiB` diagnostic. Implementation fixed; expected needle not weakened. Never cite `-1` as passing. |

Historical third-increment evidence files (not current host qualification):
`pi-rust-codemode/target/codemode-content-parts-native-smoke-2/{results.json,provenance.json}`.

| Windows artifact recorded in historical third-increment provenance | Recorded SHA-256 |
| --- | --- |
| Host `pi-rust-codemode/target/debug/rpi.exe` | `5dba0fa76a699b79f9b286721cd8b2ab1e21b79f3f30536090d086d76d2dc7c7` |
| Code Mode `rpi-package-codemode/target/debug/rpi_codemode.dll` | `5fccaaa062bdebb1fe74294a470d18816cbb59ffe8e79ad43196e220e8ebdd6f` |
| Fixture `pi-rust-codemode/target/codemode-parity-fixture/debug/codemode_parity_fixture.dll` | `27830e8fe1ca4a1f94918c1be51912472d34bba6ed3ace716dd933339eb23bac` |
| Gate `rpi-package-codemode/target/debug/rpi_permission_gate.dll` | `b52ce8ce8f10e032b701eb53cbca01e1abe04d567843aa96e2be9d880542cb34` |

These are recorded local checkpoints, not source-review/commit approval, release,
installed-binary, deployment, CI-green, Linux/macOS or full-plan completion claim.

## Historical second-increment checkpoint (not current)

`target/codemode-reference-native-smoke-3/{results.json,provenance.json}` passed
**46 cases**, the earlier 35 plus 11 reference/inspection/release/throw cases.
Default-parallel native suite passed **1,117 / 0 failed / 6 ignored**, twice in
`target/bridge-owner-all-parallel{,-repeat}.log`; package **92** passed with
strict all-target Clippy/fmt. A cross-session bridge-pinning fix filters weak
owner identity before upgrade, retaining deterministic immediate-drop checks.
The reference smoke `-1` failed an obsolete terminal-summary assertion and is
not passing evidence. First-increment `full-parity-smoke-5` and **1109 / 6 ignored**
above remain historical, not current broad qualification.
