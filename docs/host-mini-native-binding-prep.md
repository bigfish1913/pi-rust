# Session-free native resource assembly prerequisite

## Scope and outcome

This is **not** the production mini Agent. It removes one concrete blocker on the raw-Agent path: frozen Host resource configuration and the existing owned DLL loader previously operated only by mutating `AgentHarnessOptions`, whose `Default` constructs an in-memory `Session`.

The same frozen resources can now configure prompt/provenance independently, and the existing DLL assembly can yield the already supported Agent-loop callbacks without manufacturing Harness options. Ordinary persistent assembly continues through its previous Session inventory/persistence and owned startup path.

## Changes

- `crates/rpi-cli/src/host/instruction_resources.rs`: expose Session-independent prompt/provenance configuration. The existing Session inventory and strict sync functions remain available for persistent agents; raw prompt configuration does not touch either.
- `crates/rpi-cli/src/host/binding_resources.rs`: expose `configure_parts` and an owned `assemble_callbacks` result. Instruction-only selection yields no hooks; explicit Hooks selection delegates to the same Unix staging/loader.
- `crates/rpi-cli/src/host/native_hooks.rs`: retain the original staging, exact SDK inventory validation, `NativeExtensionCustody::load_owned_checked`, weak callback owner and actual after-tool/Provider hooks. Return the resulting loop callbacks independently from Harness options. No second loader or registry was added.
- `crates/rpi-cli/src/host/bindings.rs` and `ordinary/instruction_resource_tests.rs`: persistent binding configures the same prompt and resource provenance through the new primitive; Session persistence code remains unchanged.
- Added one test to the existing instruction-resource group asserting frozen `{{...}}` prompt content and context-file provenance can be composed directly without constructing a Harness or Session.

No profile accepts Mini yet. No new Host owner/coordinator, lifecycle entrypoint, IPC/wire capability, result receipt, or default CLI/TUI path was added. The DLL callback factory already derives `sessionId` from `AgentSpec.session`, so a future Mini spec with `None` will produce explicit null; that behavior still needs an actual raw-Agent/DLL integration test before being claimed as Mini acceptance. Native DLLs remain trusted code, not a sandbox.

## Validation

Using the pre-existing offline Cargo environment and isolated logs:

| Check | Result |
|---|---:|
| Persistent instruction resource fault/sync tests plus Session-free prompt composition | 3 passed, 0 failed |
| Complete `host_control` binary integration suite | 39 passed, 0 failed |
| `rpi-cli --lib --bins --test host_control --no-deps -- -D warnings` Clippy | exit 0 |
| `rustfmt --edition 2021` on changed Rust files | applied; final check recorded separately |

The binary suite includes the existing real C-DLL + HTTP Provider ordinary profile tests, confirming the extracted callback/loader path has not broken the persistent production path. It is **not** a Mini binary test: these tests still use ordinary persistent Host profiles.

## Boundary and remaining work

1. Add an explicit, backward-compatible mode/session profile schema; default remains Persistent, while Mini requires `session` absent and cannot resolve to any ordinary or in-memory Harness Session.
2. Build the raw `rpi_agent::Agent` from the closed runtime/model/tool/resource binding. Provider request options must use the real resolved Provider factory; terminal observers must enter the same loop only once. Keep `session_id=None` and preserve literal template braces.
3. Integrate Mini as an owner variant in the **one existing Host runtime/service owner table**, not a parallel registry. Reuse real owner custody for busy/Running/cancel/close, delayed callbacks and DLL leases; mini result/admission capabilities must be explicitly volatile/epoch-scoped and never returned as durable JSONL receipts.
4. Keep restart classification at Lost with no auto-restore or prompt/tool replay. Prove no Session, JSONL, companion, writer lease, or filesystem side effect using a real binary and the actual approved management DLL + HTTP Provider chain.
5. Only after those contracts close may #65 be completed. Approval workflows, timers, daemon configuration/recovery, default thin CLI/TUI and whole-process cleanup remain unimplemented.

All edits are in the existing core worktree. The extension worktree, four mirrored wire contracts, workspace manifests/lock, old reports and benchmark evidence are unchanged. No service was installed/launched or deployed.
