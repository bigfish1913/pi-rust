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

There are 20 base cases, four optional fixture cases and one optional denial
case. Evidence includes bounded parent progress/end metadata. Native terminal
error is verified on `tool_execution_end` (message history omits `terminate`).
The `oversized` probe parameter deliberately exceeds the opted-in envelope's
1 MiB ABI limit, so it is rejected **before** native child accounting retention.
It is not used to claim oversized Action-30 metadata/accounting coverage.
