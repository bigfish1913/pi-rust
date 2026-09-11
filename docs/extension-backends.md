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

The host now has a bidirectional, multiplexed
`runtime_request`/`runtime_response` channel. Rust keeps a pending-request table
and dispatches replies by id, so a long-running package command does not block
unrelated tool or UI traffic. Rust can install a runtime handler, and a Node
extension can await `pi.runtimeRequest(action, args)`. Capabilities are only
advertised when their handler is actually installed, so unsupported provider/UI
features cannot be mistaken for working APIs.

Rust may send a `cancel_request` host event for an in-flight call. The Node host
maps it to the `AbortSignal` passed to Pi tools and command contexts. Package
code should stop promptly when that signal is aborted.

## Persistent packages and PTC

The multiplexed transport is shared infrastructure for two execution policies:

- Pi-compatible packages use a persistent Node process because registrations,
  event handlers, and package state live for the session.
- A future PTC executor should use an isolated worker or child process per run,
  expose only capability-checked host calls, and terminate the worker on
  completion, timeout, or cancellation.

PTC is therefore an execution policy, not a replacement for the Pi package
adapter. Both should speak the same request-id protocol and use the same Rust
capability handlers.

When adding a capability, prefer a small optional contract over expanding one
large trait. This keeps the native backend complete while allowing Node and
future backends to implement the contract incrementally.
