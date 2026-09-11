# Extension Backends

RPI keeps extension orchestration independent from the extension implementation.
The CLI exposes a small capability contract in `pi-cli::extension_api` and
adapts each backend to it.

## Backends

- `native-rust` loads the existing Rust `cdylib` plugin ABI and exposes the
  complete native registry, event, provider, renderer, and runtime-action
  capabilities.
- `node` runs Pi JavaScript/TypeScript packages in a long-lived Node process.
  The Node host is kept in `crates/pi-cli/src/node_host.mjs` and embedded into
  the binary with `include_str!`, so installed binaries do not depend on a
  neighboring script file.

Every backend reports an API version and explicit capabilities. Unsupported
capabilities should be reported as structured `unsupported_capability` errors;
they must not appear as JavaScript `undefined` failures.

## Compatibility progression

The Node backend currently covers tools, commands, resources, model snapshots,
notifications, editor text access, and session snapshots. Session mutations
and event callbacks remain separate capabilities. Provider calls are available when a Rust provider is
installed: `modelRegistry.getProvider(id)` exposes `streamSimple()` (including
Pi-compatible `for await` events and `result()`) and `complete()`. The current
JSON-lines transport batches provider events before resolving the stream, so
extensions see the same contract without moving HTTP/authentication into Node.
Fullscreen `ui.custom` components use the same bridge: Node retains the
component and Rust owns terminal writes, focus, input, resize, and cleanup.

The host now has a bidirectional `runtime_request`/`runtime_response` channel:
Rust can install a runtime handler, and a Node extension can await
`pi.runtimeRequest(action, args)`. Capabilities are only advertised when their
handler is actually installed, so unsupported provider/UI features cannot be
mistaken for working APIs.

When adding a capability, prefer a small optional contract over expanding one
large trait. This keeps the native backend complete while allowing Node and
future backends to implement the contract incrementally.
