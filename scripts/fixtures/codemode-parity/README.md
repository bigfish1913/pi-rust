# Test-only Code Mode native parity DLL

Standalone cdylib using only the SDK ABI, with no host linkage, workers or
asynchronous cleanup. **Never install this DLL.** Each handle emits one
synchronous native partial callback and one final Done/Err; cancel sets its flag,
and destroy retires it idempotently.

The original `parity_probe` and aliases are **I/O-free**, but the whole fixture
is no longer filesystem-I/O-free: the new legacy `parity_large` probe reads only
`large-native-text-result.json` or `large-native-image-result.json` from the
disposable scratch workspace. It accepts no caller-provided path and bounds its
read at 8 MiB. The smoke harness creates these files before launch. There is no
fixture network access.

## Latest native observation seam (not durable history)

The host now provides an **optional native observation seam**, not a production
child journal or new RPC capability. It observes genuine requested/prepared
arguments, original signed partials and the actual post-hook final result.
Existing private `tp-*` request/progress/audit identity is unchanged; a future
native durable entry ID is separate. Default emitters return no observer, so the
normal CLI still has **no installed child-history writer**. Six negotiated Code
Mode capabilities and the SDK ABI/layout remain unchanged.

Native **1,146 tests passed twice (6 ignored)**; the existing Windows smoke
remains **59/59** with a freshly rebuilt host and unchanged fixture/DLL/gate.
See the [native journal checkpoint](../../../docs/extensions/tool-program.md#latest-installed-native-child-journal-queries-and-ui-still-pending)
for exact scope, lifecycle tests, current artifact evidence, native Clippy blockers
and the still-pending durable history/core/adapters/Web work.

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
See the [latest matrix checkpoint](../../../../rpi-package-codemode/docs/codemode-full-parity-matrix.md#historical-native-result-codec-checkpoint)
for hashes, logs, focused codec checks and native lint blockers. The earlier
third-increment 59-case / 1,123-test evidence and `5dba…` host hash are
historical, not qualification of the rebuilt host.

## Build and run from source

From the host repository root:

```sh
cargo build --locked --manifest-path scripts/fixtures/codemode-parity/Cargo.toml --target-dir target/codemode-parity-fixture
```

Windows debug output: `target/codemode-parity-fixture/debug/codemode_parity_fixture.dll`
(Linux: `libcodemode_parity_fixture.so`; macOS: `libcodemode_parity_fixture.dylib`).
After building the matching host and Code Mode extension:

```sh
python -B scripts/codemode-smoke.py --rpi /path/to/new/rpi.exe --extension /path/to/new/rpi_codemode.dll --fixture-extension target/codemode-parity-fixture/debug/codemode_parity_fixture.dll --permission-gate /path/to/rpi_permission_gate.dll --evidence-dir /path/to/NEW/evidence
```

This is an isolated source-build test, not installation, production restart,
deployment, merge or a CI-green claim.

## Probes and result semantics

- `parity_probe` opts into the version-1 result envelope. It supplies supported
  native text/textSignature, details, usage, duplicate added names, structured
  isError and optional terminate. The `oversized` parameter tests rejection of
  an opted-in envelope over 1 MiB **before** child accounting retention, not
  oversized Action-30 accounting coverage.
- Helper-name aliases are `call`, `list`, `emit`, `result`, **`release`**; invoke
  these native tools through `tools.call(name,args)` to avoid guest helpers.
- Business JSON exercises legitimate `runtimeKey/runtimeHandle/__rpi/
  __rpiRuntimeKey` names. `echoBinding:true` deliberately echoes actual child
  metadata/credentials in text/details to test the invocation-local public-output
  guard. Native ABI injected top-level `__rpi` remains host-owned. Ordinary bare
  numbers survive. This is not a malicious-plugin sandbox test: arbitrary
  encoding/direct I/O are unguarded.
- `parity_large` deliberately **does not** opt into the 1 MiB result envelope.
  Legacy Done supplies large text/image/mixed content/details and optional terminal success;
  `kind:'error'` returns a real legacy StepErr string, not fabricated structured
  error metadata/accounting. StepErr cannot carry terminate, so **real large
  legacy terminal error is not covered**. Small structured terminal error is
  already tested by parity_probe.

The extension requires all **six exact integer v1 caps**, including
`resultReferences:1` and additive sixth `contentAttachments:1`,
and starts requests with `resultMode:'reference'`. Nonterminal results ≤1 MiB
materialize as mutable guest objects and release native storage. Their exact
selection uses current guest content/details; deleted/undefined details become
native null, while isError remains native-authoritative. Results >1 MiB are
frozen lazy WeakMap-authenticated receipts. SDK 0.3.20 pin and ABI/layout are unchanged. Inspection reads/validates the whole
JSON under the 32 MiB guest heap; `tools.result(receipt)` forwards natively without
materialization. Terminal children of either size attach their original native
receipt without resuming JS, not an extension summary.

```js
// Fixture-only: forward a large result without reading it in JS.
return tools.result(await tools.parity_large({kind: 'text'}));
```

```js
// Fixture-only: inspect and release; avoid printing the whole payload.
for (let i = 0; i < 20; i++) {
  const r = await tools.parity_large({kind: 'text'});
  try { print({blocks: r.content.length}); }
  finally { tools.release(r); }
}
return {released: 20};
```

Release invalidates large receipts, including cached reads/forwarding; releasing
a selected or emitted/pinned receipt fails sticky before native free; release
of a genuine materialized small result is a native no-op. Exactly one full-result
selection is supported, with no tools.emit mixing or second content return.
Inline generated/emitted/small exact selection remains 128 blocks / 1 MiB content
with a separate 1 MiB final-envelope limit. Native attachment is exempt from
inline payload limits, **not** the 16 refs/program and shared 64 MiB tree quota.
Quota charge is `2*serialized_bytes+4096+128*details_nodes+256*content_blocks`;
the engine also guards manifests at 32 MiB. Native clone/serialization before
quota and Rust JSON outside the guest heap mean **no hard RSS ceiling**. Ordered large receipt emission and mixed generated/large content are now
supported within bounds; multiple whole-result selection and tools.result/emit
mixing remain unsupported. No new retry/replay is introduced.

## Third-increment mixed content contract

```js
// Fixture-only: two separately executed large results, inline/native in order.
const first = await tools.parity_large({kind:'text'});
tools.emit([{type:'text',text:'before'}]);
tools.emit(first);
tools.emit([{type:'text',text:'between'}]);
const mixed = await tools.parity_large({kind:'mixed'});
tools.emit(mixed, [2,1,0,1]); // image repeated, only one mixed execution
return {processed:2};
```


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

## Historical third-increment evidence (before codec follow-up)


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

## Historical second-increment evidence (not current qualification)

`target/codemode-reference-native-smoke-3/results.json` reports **PASSED, 46 cases,
all true**; its `provenance.json` records four hashes, verified against the actual
binaries:

| Artifact | Verified SHA-256 |
| --- | --- |
| Host | `185f0e7c031fb703c5a59d18047359e8a42e981d91c5658ed907205b9eb01383` |
| Code Mode DLL | `85761d9c99e102a8150ece8f043b4946cdad10edeadc5aff3dbcfc7d2b35f4df` |
| Fixture DLL | `d849ab1773aa0a463b0fc7f61e393d7c3066eba8946e24e20c6e9c632a29a245` |
| Permission gate | `b52ce8ce8f10e032b701eb53cbca01e1abe04d567843aa96e2be9d880542cb34` |

The 46 cases are the previous 35 plus **11 new cases**: four direct baselines
(2 MiB text, actual >1 MiB PNG, legacy plain error, terminal success), four
composed forwards, lazy frozen inspection/mutation rejection, 20 inspect/release
loops with once-only usage, and selection followed by JS throw/discard.
Canonical native end payload hashes compare against actual direct baselines;
image payload may be normalized, so it is not compared to the original scratch
PNG bytes. Actual requests to the local fixture provider validate delivered image
content before evidence is summarized. Large probes use isolated sessions,
stream native JSONL markers and obtain only bounded final snapshots; this is
**not full get_messages large-history parity** or live-provider qualification.
Native terminal flags are checked on tool_execution_end (message history omits terminate).

Harness bounds: provider requests at most **16 MiB** each / **128 requests**,
retained provider summaries **8 MiB**, RPC/transcript lines **16 MiB**, process
logs **64 MiB**. These are transport/evidence bounds, **not process RSS limits**.

`codemode-reference-native-smoke-1` failed an obsolete terminal-summary assertion;
never cite it as passing. Historical `codemode-full-parity-smoke-5` (**35 cases**)
and native **1109 tests / 6 ignored** belong to the first increment, not current
broad qualification. Historical second-increment checkpoint: **92 package tests**, strict all-target
Clippy and fmt passed; focused native **33 agent**, **206 extension unit + 5
integration tests (1 ignored)** passed, including one new test with **22 scenarios
across both drivers**. The default-parallel agent/extensions/harness/SDK suite
passed **1,117 tests, 0 failed, 6 ignored**, twice; logs:
`target/bridge-owner-all-parallel{,-repeat}.log`. An earlier parallel attempt
exposed unrelated-session bridge pinning; weak owner filtering before upgrade
fixes it, with a deterministic regression and unchanged immediate-drop assertions.

Overall full parity remains **INCOMPLETE**: complete native child events and
bounded native history/query/replay/recovery; neutral extcore with thin HTTP/ACP
mappings and Web cards/HMR/reload in the approved `D:/Projects/rpi-package-dev` worktree on `dev/web-hmr`; approved
recursion/self/ancestor policy; Linux/macOS native
integration, live providers and reload races. Current dev frontend is untouched.
