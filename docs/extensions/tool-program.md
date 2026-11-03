# Native Code Mode: ToolProgram (runtime action 30)

`RuntimeActionId::ToolProgram` implements protocol version 1 on the existing
extension runtime-action ABI. No MCP connection, subprocess model, or second
assistant turn is involved. Older hosts reject action 30 explicitly.

A tool receives opaque `__rpi.runtimeHandle` and `__rpi.runtimeKey` from its native
driver. Forward the handle as runtime-action user_data and the key as the reserved
`__rpiRuntimeKey` argument. Keep both out of guest JavaScript, model-visible
results, and logs. A registration pointer, guessed key, or retired bridge is not
a tool invocation. Synchronous callbacks inherit their actual driver identity;
foreign workers authenticate the captured invocation, including child subsets.

Operations:

- `list`: `{op:"list", offset?:0, limit?:32}` returns
  `{protocolVersion:1, tools:[{name,description,parameters}], nextOffset}`.
  Pages allow 1..128 entries; catalog maximum is 4096 entries and 1 MiB.
  The executing tool and ancestor names are excluded.
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
tool execution. A silent emitter prevents synthetic nested ToolExecutionStart or
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
Noncooperative tools can consequently hold the gate indefinitely (fail closed).

The harness requires an already checkpointed **Never-replay** root parent.
Replay-Safe parents fail closed before the nested effect. The ordinary parent
ToolStarted record remains the only assistant-associated tool-start record.
Before each authorized nested effect, the harness acknowledges a bounded custom
`rpi.tool_program.intent.v1` entry naming run/root/parent/request/tool/effective
arguments. Successful execution adds `rpi.tool_program.finished.v1` audit metadata.
These custom entries are not fabricated assistant/tool-result messages and do
not alter provider history. Interrupted parent programs remain unknown outcomes
and are never automatically replayed. No automatic retry is provided.

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

## Real native smoke

Build this host and the companion `rpi-package` checkout's `rpi-codemode` and
`rpi-permission-gate` libraries, then run with explicit local artifact paths:

```sh
python scripts/codemode-smoke.py --rpi /path/to/rpi \
  --extension /path/to/rpi_codemode.dll \
  --permission-gate /path/to/rpi_permission_gate.dll \
  --evidence-dir /new/empty/evidence-directory
```

Use `.so` / `.dylib` on Linux/macOS. The smoke starts only an isolated native
RPC process and a bounded loopback provider fixture (no real model credentials).
It creates fresh agent settings/workspace/transcripts, rejects the nested write
approval explicitly via RPC, and retains binary SHA-256, commands, RPC logs,
provider requests, messages and `results.json`. Existing evidence directories
are rejected rather than overwritten. The approval case is explicitly skipped
if no gate DLL is supplied.

Local Windows validation passed eight cases: catalog discovery, dependent and
parallel read/write composition, invalid arguments, partial effect followed by
JS error, timeout, explicit abort, reuse after abort, and rejected nested write
approval. Checks also confirm durable nested markers, no orphan provider tool
results and no persisted invocation capabilities. Evidence is in the local
`target/codemode-native-smoke-3/` directory; it is not a release asset. This is a
local source-build qualification, not installation/deployment, a performance
benchmark, live-provider validation, or proof that Linux/macOS/native reload
races have passed. Cross-platform workflows are authored but not run locally.
