# rpi v0.1.7

`rpi v0.1.7` adds a first-class install path for Rust-native extensions and
publishes the companion package family.

## Highlights

- Added `rpi install <crate>` for crates.io `cdylib` extensions, with
  `--version`, `--path`, `--locked`, and `--force` options.
- Bumped the core `rpi-*` workspace crates to `0.1.7`.
- Added nine published extensions from `rpi-packages` to the website catalog:
  `rpi-mcp-adapter`, `rpi-web-access`, `rpi-subagents`,
  `rpi-background-tasks`, `rpi-lens`, `rpi-todo`, `rpi-codegraph`,
  `rpi-memory`, and `rpi-token-usage`.
- Updated the static website release timeline and package count.

## Install

```bash
cargo install rpi-cli --version 0.1.7
rpi install rpi-todo
rpi install rpi-mcp-adapter --version 0.1.0
```

The extension installer builds the package as a dynamic library and places it
in `~/.rpi/agent/extensions` (or the directory selected by
`RPI_CODING_AGENT_DIR`).
