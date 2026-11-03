# Test-only Code Mode native parity DLL

Standalone cdylib: SDK ABI only, no host linkage, filesystem/network access,
workers or asynchronous cleanup. Each handle emits exactly one synchronous
native partial callback, then one opted-in version-1 result envelope. Cancel
sets the handle flag; destroy retires it idempotently. Never install this DLL.

From the host repository root:

```sh
cargo build --locked --manifest-path scripts/fixtures/codemode-parity/Cargo.toml --target-dir target/codemode-parity-fixture
```

Windows debug output: `target/codemode-parity-fixture/debug/codemode_parity_fixture.dll`
(Linux: `libcodemode_parity_fixture.so`; macOS: `libcodemode_parity_fixture.dylib`).

Only after building the new host and Code Mode extension, run:

```sh
python -B scripts/codemode-smoke.py --rpi /path/to/new/rpi.exe --extension /path/to/new/rpi_codemode.dll --fixture-extension target/codemode-parity-fixture/debug/codemode_parity_fixture.dll --permission-gate /path/to/rpi_permission_gate.dll --evidence-dir /path/to/NEW/evidence
```

The expanded script passed **35 real Windows cases** with fresh host, Code Mode
DLL, fixture and gate: 20 base, 14 fixture and one denial. Final results and all
four artifact hashes are in `target/codemode-full-parity-smoke-5/results.json`
and `provenance.json`. Earlier full-parity `-1/`, `-2/` and `-4/` runs failed;
`-3/` passed 33 cases before the final missing-details fix. Only `-5/` qualifies
the final increment; the older 25-case run qualifies the previous increment only.

The fixture registers `parity_probe` and helper-name aliases `call`, `list`,
`emit`, `result`; the aliases are invoked through `tools.call(name,args)`.
Business JSON exercises legitimate `runtimeKey`, `runtimeHandle`, `__rpi` and
`__rpiRuntimeKey` names in schemas, arguments and results. `echoBinding:true`
deliberately echoes the actual child invocation metadata/credential in native
text and details to exercise the host's invocation-local public-output guard.
Ordinary bare numbers are preserved; top-level injected `__rpi` remains owned by
the native ABI, not the business data. This test-only echo is not a production
API or a malicious-plugin sandbox test; arbitrary encoding/direct I/O is unguarded.

Ten added cases cover direct success/error baselines, exact success/error
selection with `tools.result`, deleted/undefined selected details normalized to
native null, business schemas/values, binding success/error
echoes, and helper alias collisions. Exact selections compare native
content/details/isError against the direct baselines without injecting a Code
Mode summary or appending synthetic error text. Usage/names/termination remain
host-authoritative. Evidence includes bounded parent progress/end metadata.
Native terminal error is verified on `tool_execution_end` (message history omits `terminate`).
The `oversized` probe parameter deliberately exceeds the opted-in envelope's
1 MiB ABI limit, so it is rejected **before** native child accounting retention.
It is not used to claim oversized Action-30 metadata/accounting coverage.

Selection uses the current guest-mutated content/details snapshot, not an
immutable copy. Missing selected `details` (including deletion or `undefined`)
is normalized to native `null` in the required result-envelope field.
Authoritative native `isError` remains independent of guest mutations.

A read-only review found the missing-details required-field envelope issue;
unit checks and two real cases cover the engine/final-envelope normalization fix.

This fixture targets the first implemented increment, not the full parity
plan. Host-backed values over 1 MiB, complete child progress and original native
history snapshots/recovery, and Web/ACP child cards/reload remain outstanding.
Protected self/ancestor tools are still excluded; no 100% full-toolset claim.
No fixture install, deployment or CI-green claim is made.
