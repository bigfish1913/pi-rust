# rpi v0.1.10

`rpi v0.1.10` makes Node/TypeScript extensions safer to run alongside the
interactive agent and adds the development workflow needed to iterate on
Rust extensions quickly.

## Highlights

- Multiplexed Node extension transport allows concurrent requests and accepts
  responses in any order.
- Tool and command cancellation now propagates through `AbortSignal` to JS/TS
  extensions.
- Runtime requests remain isolated behind the Node transport, including
  provider calls and custom UI events.
- `rpi dev` builds and watches a local Rust extension, while `/reload` forces a
  fresh build without replacing a working extension when compilation fails.
- Added `rpi uninstall` and `rpi uninstall-pi` for removing native and Pi
  packages.
- Provider context compatibility keeps Pi responses that omit an explicit
  `role` field usable in side threads.

## Install

```bash
cargo install rpi-cli --version 0.1.10
```

For a local checkout:

```bash
cargo install --path crates/pi-cli --force
```
