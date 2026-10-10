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

## Positioned plugin panels

Hosts with declarative panel support accept a `panel` field on the existing
`RuntimeActionId::SetStatus` (18). The ABI and legacy
`{"key":"my-plugin","value":"footer text"}` behavior are unchanged.
No host changes are needed for each new plugin, its text or its position.

```json
{
  "key": "my-plugin.monitor",
  "panel": {
    "version": 1,
    "anchor": "top-right",
    "offsetX": -2,
    "offsetY": 1,
    "width": 44,
    "maxHeight": 9,
    "minScreenWidth": 50,
    "border": true,
    "title": "My monitor",
    "lines": ["1 round · 87 steps · 252 tok/s", "First token: 0.42s"]
  }
}
```

Pass this JSON using `api.runtime_action` and free its output with
`api.free_string`, as for other runtime actions. Update the same key to replace
its panel, or send `{"key":"my-plugin.monitor","panel":null}` to remove it.
Clear panels during `SessionShutdown` as well. Footer statuses and panels have
independent registries; clearing one does not clear the other. Headless runs
store panels without rendering them. Older hosts do not render the new field.

Supported anchors: `top-left`, `top-center`, `top-right`, `left-center`, `center`,
`right-center`, `bottom-left`, `bottom-center`, `bottom-right`. Signed offsets
are terminal columns/rows relative to the anchor; negative offsets move left/up.
In the interactive TUI, positions are relative to the visible transcript area,
so panels stay fixed while chat scrolls and never cover the editor or footer.
Panels are passive text overlays and do not change keyboard focus. Content is
rendered at the allocated width and offsets are clamped inside the available area.
When passive panels overlap, the host moves later panels to the nearest free
vertical position, leaving a one-row gap. A panel that cannot fit is temporarily
hidden and returns when space becomes available. Use `sidebar` to reserve space
beside chat rather than cover transcript content.

Set `"layout":"sidebar"` to reserve a column beside the transcript instead of
covering chat. The default `"layout":"overlay"` preserves existing panels.
Left anchors use the left sidebar; other anchors use the right sidebar. A single
panel follows its vertical anchor and `offsetY`; multiple panels on one side
stack in key order. `offsetX` applies only to overlays. A sidebar adds a two-column
separator to the requested width. It hides when `minScreenWidth` is not met or
less than 48 columns would remain for chat. The editor and footer stay full width.
Removing sidebar panels restores the transcript width. Sidebar mode leaves
keyboard focus and transcript scrolling with the original components.

`version` and `lines` are required. Defaults: anchor `top-right`, width 44,
maxHeight 16, offsets 0, minScreenWidth 0, border true, title empty. Width is
4–240 columns; maxHeight is 1–80 rows including title and borders. Content is
clipped to width and height, with Unicode display widths respected. Terminal
control sequences in content are stripped. At most 32 panels, 64 lines per
panel, 2048 UTF-8 bytes per line and 256 bytes per title are accepted. Invalid
updates return an error and preserve the previous panel.

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

The host loader only resolves `rpi_plugin_register`, exported with
`export_plugin!`. The host passes a `PluginApi` containing registration slots,
`runtime_action` and `declare` (priority and platforms).

The SDK checks `abi_version` against `RPI_PLUGIN_ABI_VERSION_UNIFIED` and
requires `struct_size` to cover the `PluginApi` expected by the plugin before
calling its registration body. A mismatch returns a nonzero status and the
host reports a load failure. Extensions using retired entrypoints must be
updated to `export_plugin!` and rebuilt with the current SDK.

## Versioning

A plugin depends on `rpi-plugin-sdk` only, so its version is decoupled from the
rest of the family. ABI compatibility is checked through `abi_version` and
`struct_size`; each optional capability must also be checked before use.
Rebuild extensions when the SDK contract they require changes.

## Examples and docs

- [`examples/plugin-stub`](https://github.com/bigfish1913/pi-rust/tree/main/examples/plugin-stub) —
  a complete plugin registering an `echo` tool with the full four-function
  lifecycle (`execute` → `poll` → `cancel` → `destroy`), an event handler, and a
  resource-discovery handler.
- [Extension authoring guide](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/authoring.md) —
  templates, safety rules, testing and release checklist.
- Host-side loader: [`rpi-extensions`](https://crates.io/crates/rpi-extensions).
- Ready-to-install extensions:
  [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package).

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
