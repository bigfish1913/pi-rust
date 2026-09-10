# rpi v0.1.8

`rpi v0.1.8` improves session presentation and clarifies how core crates and
runtime extensions are installed.

## Highlights

- Session history, sharing, and exports now request entries in chronological
  order, matching the transcript shown in the terminal.
- Completed assistant messages can render token usage through an installed
  extension renderer such as `rpi-token-usage`.
- The package catalog now distinguishes core Rust libraries from loadable
  `cdylib` extensions:
  - use `cargo add rpi-tools` for a library dependency;
  - use `rpi install rpi-todo` for a runtime extension.
- The website package filters now classify `rpi-plugin-sdk` and
  `rpi-extensions` as core crates rather than installable plugins.

## Install

```bash
cargo install rpi-cli --version 0.1.8
rpi install rpi-token-usage
rpi install rpi-todo
```
