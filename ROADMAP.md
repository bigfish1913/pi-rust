# Roadmap

This is a direction document, not a commitment. It records what the project is
working on next and why, so that issues and PRs can be judged against it. For
what has already shipped, see [CHANGELOG.md](CHANGELOG.md).

Current release: **0.3.0**. Install with `cargo install rpi-cli`.

## Where the project is now

`rpi` is a Rust-native, library-first coding-agent runtime: nine published
crates (`rpi-telemetry` → `rpi-ai` → `rpi-agent` → `rpi-tools` →
`rpi-harness` → `rpi-cli`, plus `rpi-tui`, `rpi-plugin-sdk` and
`rpi-extensions`) and a terminal CLI built on them.

What is solid today:

- Async, streaming agent loop with tool calls, hooks, queues, cancellation and
  a faux provider for offline tests.
- Anthropic Messages and OpenAI-compatible providers, plus custom gateways via
  `models.json`.
- Durable JSONL sessions with branching, compaction and crash recovery.
- A stable `#[repr(C)]` plugin ABI with a dynamic loader, lifecycle events and
  veto support.
- Remote mode: a headless `--server` and a zero-local-resource `--connect`
  client.
- Pi compatibility: `.pi/` resource layout, `settings.json` keys, Pi-compatible
  managed package stores.

## Near term

### 1. Distribution without a Rust toolchain

Today the only install path is `cargo install rpi-cli`, which requires rustup, a
C toolchain and a full release build. Prebuilt binaries for
`x86_64`/`aarch64` on Linux, macOS and Windows, plus an install script, are the
highest-leverage change for adoption. Package-manager taps (Homebrew, Scoop,
winget) follow from the same release artifacts.

### 2. Documentation that stands on its own

- Per-crate README and `lib.rs` landing docs, so each crates.io/docs.rs page is
  usable without the workspace README.
- A task-oriented tutorial that goes from `cargo install rpi-cli` to a working
  custom tool, in one page.
- Keep `docs/native-pi-missing-features.md` current: it is the honest answer to
  "is this really at parity?" and it is more useful than a compatibility claim.

### 3. Provider breadth

The provider boundary is in place; the gaps are adapters. Shipped today:
Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, plus
OpenAI-compatible gateways including OpenRouter, DeepSeek and llama.cpp.
Candidates next, in rough order of demand: Google Gemini, AWS Bedrock, Azure
OpenAI, and a first-class Ollama preset.

### 4. Extension ecosystem

The ABI and loader work; the ecosystem is thin. Concretely:

- Keep [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package) building
  green against the current ABI and publish a compatibility table per ABI
  version.
- Ship an extension template (`cargo generate`-style) so a new extension is one
  command, not a copied example.
- Settle the Node/TypeScript bridge question: either finish it as a supported
  path or mark it explicitly experimental and stop advertising it.

### 5. Measurable claims

The Rust rewrite's advantages (startup time, resident memory, single binary)
are currently unquantified. A reproducible benchmark against the TypeScript
original and against peer CLIs — startup, idle memory, tool-loop overhead —
belongs in the README.

## Later

- **Sandboxing.** Plugin code and `bash` currently run with the user's full
  privileges by design. A documented sandbox profile (seccomp/AppContainer or
  a container runner) would make the agent safer to point at untrusted
  repositories. This is a design problem, not a patch.
- **Multi-agent orchestration in-tree.** Sub-agents exist as an out-of-tree
  extension. The question is whether the runtime should own delegation,
  hand-off and budget accounting.
- **Editor/IDE integrations** built on the remote-mode protocol, so the agent
  is usable outside the terminal.
- **Windows parity** for anything currently POSIX-shaped in the tool layer.
- **Structured output and evaluation tooling** for teams that want to run the
  agent in CI.

## Not planned

- A hosted service. `rpi` is a library and a local tool; there is no plan to
  run inference or store user sessions on project infrastructure.
- A visual/GUI application. The TUI and the JSONL remote protocol are the
  supported interfaces.
- Bundling a model. Providers are BYO-key or BYO-endpoint.

## How to influence this

Open an issue describing the problem you hit, not just the feature you want.
Features that unblock a real workflow move up; features that only add surface
area move down. If you want to work on something listed here, comment on the
issue first so we can agree on the shape before you write the patch. See
[CONTRIBUTING.md](CONTRIBUTING.md).
