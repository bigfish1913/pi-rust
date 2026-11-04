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
  Legacy Done supplies large text/image/details and optional terminal success;
  `kind:'error'` returns a real legacy StepErr string, not fabricated structured
  error metadata/accounting. StepErr cannot carry terminate, so **real large
  legacy terminal error is not covered**. Small structured terminal error is
  already tested by parity_probe.

The extension requires all five exact integer caps, including `resultReferences:1`,
and starts requests with `resultMode:'reference'`. Nonterminal results ≤1 MiB
materialize as mutable guest objects and release native storage. Their exact
selection uses current guest content/details; deleted/undefined details become
native null, while isError remains native-authoritative. Results >1 MiB are
frozen lazy WeakMap-authenticated receipts. Inspection reads/validates the whole
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

Release invalidates large receipts, including cached reads/forwarding; release
of a genuine materialized small result is a native no-op. Exactly one full-result
selection is supported, with no tools.emit mixing or second content return.
Inline generated/emitted/small exact selection remains 128 blocks / 1 MiB content
with a separate 1 MiB final-envelope limit. Native attachment is exempt from
inline payload limits, **not** the 16 refs/program and shared 64 MiB tree quota.
Quota charge is `2*serialized_bytes+4096+128*details_nodes+256*content_blocks`;
the engine also guards manifests at 32 MiB. Native clone/serialization before
quota and Rust JSON outside the guest heap mean **no hard RSS ceiling**. Whole-
large emission, mixed/generated large output and multiple full-receipt selection
remain unsupported. No new retry/replay is introduced.

## Current second-increment evidence

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
broad qualification. Current checkpoint: **92 package tests**, strict all-target
Clippy and fmt passed; focused native **33 agent**, **206 extension unit + 5
integration tests (1 ignored)** passed, including one new test with **22 scenarios
across both drivers**. The default-parallel agent/extensions/harness/SDK suite
passed **1,117 tests, 0 failed, 6 ignored**, twice; logs:
`target/bridge-owner-all-parallel{,-repeat}.log`. An earlier parallel attempt
exposed unrelated-session bridge pinning; weak owner filtering before upgrade
fixes it, with a deterministic regression and unchanged immediate-drop assertions.

Overall full parity remains **INCOMPLETE**: complete native child events and
bounded native history/query/replay/recovery; neutral extcore with thin HTTP/ACP
mappings and Web cards/HMR/reload in the approved third UI dev worktree; approved
recursion/self/ancestor policy; multiple/mixed large output; Linux/macOS native
integration, live providers and reload races. Current dev frontend is untouched.
