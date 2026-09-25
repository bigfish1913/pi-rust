# rpi-plugin-sdk

[![crates.io](https://img.shields.io/crates/v/rpi-plugin-sdk.svg)](https://crates.io/crates/rpi-plugin-sdk)
[![docs.rs](https://docs.rs/rpi-plugin-sdk/badge.svg)](https://docs.rs/rpi-plugin-sdk)

The stable `#[repr(C)]` ABI contract for rpi plugins. Depend on this crate, and
nothing else, to build a Rust `cdylib` that extends the `rpi` agent with tools,
providers, slash commands, event handlers, renderers and discovered resources.

This crate has **no dependency on the rpi runtime** on purpose. A plugin must
not link the host — the host links the plugin. Both sides share only the types
defined here.

## Install

```toml
[package]
name = "my-rpi-extension"

[lib]
crate-type = ["cdylib"]

[dependencies]
rpi-plugin-sdk = "0.3"
serde_json = "1"
```

## Minimal plugin

```rust
use rpi_plugin_sdk::{export_plugin, StbStringRef};

export_plugin!(|api| {
    // Optional: declare a load priority and the platforms this plugin supports.
    if let Some(declare) = api.declare {
        declare(StbStringRef::from_str(r#"{"priority":60,"platforms":["linux"]}"#));
    }

    // ... register tools, providers, commands and handlers through `api` ...

    0 // status code: 0 = success
});
```

The macro emits the `rpi_plugin_register` symbol. The version lives *inside* the
`PluginApi` struct (`abi_version`, `struct_size`) rather than in the symbol name,
so the entrypoint signature does not change when the struct grows.

## Why a hand-written C ABI

Both sides are Rust, but they are compiled separately and may use different
compiler versions and different versions of this crate. That rules out passing
any Rust type that is not `#[repr(C)]` or that has a `Drop` impl. The design
rules the SDK enforces:

1. **Every crossing type is `#[repr(C)]`**, and enums in unions carry an explicit
   `#[repr(u32)]` so the discriminant width is pinned.
2. **No `Drop` type crosses.** No `Vec`, `String`, `serde_json::Value`, `Option`
   of those, or `Result`. Owned strings cross as `StbString` (pointer + length)
   together with an explicit `free_string` function the producer exports, so each
   allocation is freed exactly once, by the side that received it.
3. **Structured data crosses as JSON** in a `StbString`. `serde_json` must be
   configured consistently on host and plugin (`preserve_order` +
   `arbitrary_precision`), or integers beyond `u64`/`i64` lose precision and
   object key order can change.
4. **Unions hold `Copy` payloads only**, so a variant read is `unsafe` and the
   caller discriminates on the tag.
5. **Unwinding never crosses the boundary.** Every call is `extern "C"`, and both
   sides contain panics with `catch_unwind`; a panic is converted into a status
   code rather than aborting the process.

These are not stylistic preferences — violating any of them is undefined
behaviour at the boundary.

## Compatibility

The host loader resolves entrypoints in this order:

| Symbol | ABI | Note |
| ------ | --- | ---- |
| `rpi_plugin_register` | `RPI_PLUGIN_ABI_VERSION_UNIFIED` | Current. Preferred. |
| `rpi_plugin_register_v3` | `RPI_PLUGIN_ABI_VERSION_V3` | Adds `declare` (priority, platforms). |
| `rpi_plugin_register_v2` | `RPI_PLUGIN_ABI_VERSION` | Legacy. |

A plugin built against an older ABI keeps loading, so the migration path for
extension authors is "rebuild when convenient", not "rebuild or break".

## Versioning

A plugin depends on `rpi-plugin-sdk` only, so its version is decoupled from the
rest of the family. When the ABI struct changes, an existing plugin continues to
work through its older entrypoint; a new field is only reachable after the
plugin checks `struct_size`.

## Examples and docs

- [`examples/plugin-stub`](https://github.com/bigfish1913/pi-rust/tree/main/examples/plugin-stub) —
  a complete plugin registering an `echo` tool with the full four-function
  lifecycle (`execute` → `poll` → `cancel` → `destroy`), an event handler, and a
  resource-discovery handler.
- [Extension authoring guide](https://github.com/bigfish1913/pi-rust/blob/main/docs/extension-authoring.md) —
  templates, safety rules, testing and release checklist.
- Host-side loader: [`rpi-extensions`](https://crates.io/crates/rpi-extensions).
- Ready-to-install extensions:
  [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package).

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
