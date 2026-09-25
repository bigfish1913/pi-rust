# rpi-extensions

[![crates.io](https://img.shields.io/crates/v/rpi-extensions.svg)](https://crates.io/crates/rpi-extensions)
[![docs.rs](https://docs.rs/rpi-extensions/badge.svg)](https://docs.rs/rpi-extensions)

The host side of the plugin system: discovers, loads and drives Rust `cdylib`
plugins, and bridges their tools and events into the runtime's native async
types.

The ABI contract itself lives in
[`rpi-plugin-sdk`](https://crates.io/crates/rpi-plugin-sdk); this crate is the
other half.

## Install

```toml
[dependencies]
rpi-extensions = "0.3"
```

Most users never depend on this directly — the `rpi` CLI uses it, and the
harness consumes its adapters through injection.

## What it does

- **Loading.** `load_one` loads a single library, `load_dir` scans a directory,
  and `load_session` resolves the whole set for a session. Entrypoints are
  resolved unified → v3 → v2, so older plugins keep working.
- **Registry.** `ExtensionRegistry` holds what plugins registered: tools,
  providers, handlers, renderers and resource-discovery callbacks, with a
  deterministic snapshot the runtime can read.
- **Tool bridge.** `PluginToolAdapter` implements the agent's `AgentTool` trait
  on top of a plugin's four-function lifecycle (`execute` → `poll` → `cancel` →
  `destroy`). The blocking `poll` loop runs on `spawn_blocking` inside the
  ambient runtime — the adapter does not own a runtime of its own.
- **Partial results.** Tool progress flows back through an unbounded channel and
  is forwarded to `on_update`, behind a `catch_unwind` trampoline so a panicking
  callback cannot unwind across the FFI boundary.
- **Lifecycle events.** Dispatch wrappers for data, empty and lifecycle events,
  a veto path that lets a handler refuse startup, and a tee emitter so a plugin
  can observe events without stealing them from the TUI.
- **Providers.** `PluggableProvider` and `ExtensionProviderHooks` let a plugin
  serve a model or patch request options.

## Crate dependencies

`rpi-extensions` depends on `rpi-plugin-sdk`, `rpi-ai` and `rpi-agent` only —
never on `rpi-harness`. The harness receives these adapters as trait objects, so
the dependency graph stays acyclic.

## Where plugins are discovered

| Location | Scope |
| -------- | ----- |
| `.rpi/extensions` | Project (preferred) |
| `.pi/extensions` | Project, Pi compatibility fallback |
| `~/.rpi/agent/extensions` | Global |
| `--extensions-dir <path>` | Explicit, added by the CLI |

## Installing and developing

```bash
rpi install my-extension                  # build from crates.io and install
rpi install my-extension --path ../local  # local development
rpi dev                                   # build + watch + live reload
```

## Docs

- Crate docs: <https://docs.rs/rpi-extensions>
- [Extension authoring guide](https://github.com/bigfish1913/pi-rust/blob/main/docs/extension-authoring.md)
- [Plugin ABI contract](https://docs.rs/rpi-plugin-sdk)
- [Ready-to-install extensions](https://github.com/pi-rust/rpi-package)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
