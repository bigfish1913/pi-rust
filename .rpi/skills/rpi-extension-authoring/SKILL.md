---
name: rpi-extension-authoring
description: Create or update rpi Rust cdylib extensions and Pi-compatible JS/TS packages using the repository's supported SDK, manifests, safety boundaries, development workflow, tests, and release checks.
---

# rpi Extension Authoring

Before editing an rpi extension or Pi-compatible package, call the built-in `docs` tool with `{"topic":"authoring"}`. Use focused queries for `JS/TS`, `ABI`, `rpi dev`, `resources`, or `release` when the full document is unnecessary.

Choose one backend deliberately:

- Pi ecosystem compatibility and commands/UI: JS/TS package.
- Native Rust, system integration, performance, and stable host boundary: `cdylib` using `rpi-plugin-sdk`.

For Rust extensions, depend only on `rpi-plugin-sdk` at the ABI boundary and start from `examples/plugin-stub`. Keep domain logic outside FFI functions, honor execute/poll/cancel/destroy ownership, and test both ordinary Rust behavior and host loading. Run `rpi dev --no-watch` for a real load smoke; use `rpi dev` during interactive development.

For JS/TS packages, define resource and extension paths in `package.json`, prefer compiled JavaScript in published packages, validate tool schemas strictly, check capabilities before using UI/runtime features, and test through `rpi install-pi` plus `/reload`.

Never weaken path/network/command boundaries to make an extension demo pass. Document permissions, persistence, external access, supported platforms, and compatibility fallbacks in the package README.
