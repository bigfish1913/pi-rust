# rpi — Rust-native coding-agent runtime

[![rpi-cli on crates.io](https://img.shields.io/crates/v/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![rpi-plugin-sdk docs](https://docs.rs/rpi-plugin-sdk/badge.svg)](https://docs.rs/rpi-plugin-sdk)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions)
[![GitHub stars](https://img.shields.io/github/stars/bigfish1913/pi-rust?style=flat)](https://github.com/bigfish1913/pi-rust/stargazers)
[![Latest release](https://img.shields.io/github/v/release/bigfish1913/pi-rust)](https://github.com/bigfish1913/pi-rust/releases/latest)

`rpi` is a Rust-native, library-first coding-agent runtime and terminal CLI.
It provides composable LLM agents with providers, tools, sessions, and plugins —
usable both as embedded SDK crates and as a ready-to-run terminal agent.

Website: [https://rpi.laofu.online/](https://rpi.laofu.online/)

> **Naming.** Published crates use the `rpi-` prefix. On-disk directories remain
> `crates/pi-*` for history; `package.name` in each `Cargo.toml` is `rpi-*`.

## Crates

| Crate (crates.io)  | On-disk dir         | Description                                                                         |
| ------------------ | ------------------- | ----------------------------------------------------------------------------------- |
| `rpi-telemetry`  | `pi-telemetry/`   | Telemetry span/event contracts (noop default).                                    |
| `rpi-ai`         | `pi-ai/`          | Unified multi-provider LLM types + streaming (Anthropic, OpenAI-compatible, faux).|
| `rpi-agent`      | `pi-agent/`       | Agent runtime + loop, `AgentTool` trait, events, hooks, queues.                   |
| `rpi-tools`      | `pi-tools/`       | Coding tools (`read`/`write`/`edit`/`bash`) + `ExecutionEnv`.                    |
| `rpi-harness`    | `pi-harness/`     | `AgentHarness`: session tree, JSONL persistence, compaction, run loop.            |
| `rpi-cli`        | `pi-cli/`         | Terminal coding-agent CLI (`rpi` binary).                                        |
| `rpi-plugin-sdk` | `rpi-plugin-sdk/` | Stable C ABI for Rust-native plugins and extension discovery.                     |
| `rpi-extensions` | `rpi-extensions/` | Dynamic plugin loader and `AgentTool` adapter.                                    |
| `rpi-tui`        | `pi-tui/`         | Terminal UI primitives used by the interactive CLI.                               |

Dependency direction: `rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`.

### Registry links

| Package            | crates.io                                           | docs.rs                                  |
| ------------------ | --------------------------------------------------- | ---------------------------------------- |
| `rpi-cli`        | [crates.io](https://crates.io/crates/rpi-cli)        | [docs.rs](https://docs.rs/rpi-cli)        |
| `rpi-agent`      | [crates.io](https://crates.io/crates/rpi-agent)      | [docs.rs](https://docs.rs/rpi-agent)      |
| `rpi-plugin-sdk` | [crates.io](https://crates.io/crates/rpi-plugin-sdk) | [docs.rs](https://docs.rs/rpi-plugin-sdk) |
| `rpi-extensions` | [crates.io](https://crates.io/crates/rpi-extensions) | [docs.rs](https://docs.rs/rpi-extensions) |

### Extension package repository

Ready-to-install Rust-native extensions are maintained in the companion
[`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package) repository.
Browse its [`packages/`](https://github.com/pi-rust/rpi-package/tree/master/packages)
directory or the [online package catalog](https://rpi.laofu.online/packages.html).

## Quick start — build an agent

```rust
use rpi_agent::{AgentBuilder, AgentEvent};
use rpi_ai::providers::faux::{FauxProvider, FauxScript};

let provider = std::sync::Arc::new(FauxProvider::new(FauxScript::new().with_text("Hello!")));
let model = provider.default_model().clone();
let agent = AgentBuilder::new()
    .model(model)
    .system_prompt("You are a helpful assistant.")
    .tools(vec![/* MyTool */])
    .stream_fn(make_stream_fn(provider))
    .build()
    .unwrap();

let mut events = agent.subscribe();
tokio::spawn(async move {
    while let Some(ev) = events.recv().await {
        match ev { /* AgentEvent::MessageUpdate { .. }, etc. */ }
    }
});

agent.prompt("Hello!").await.unwrap();
```

See [docs/architecture.md](docs/architecture.md) for the full design and
[docs/agent-project.md](docs/agent-project.md) for the recommended project
structure when you build your own agent.

## Plugins

Plugins are Rust `cdylib` libraries loaded through the stable ABI exposed by
`rpi-plugin-sdk`. The repository includes a complete `echo` tool example:

```bash
cargo build -p plugin-stub
rpi --extensions-dir target/debug -p 'echo "hi"'
```

The plugin depends on `rpi-plugin-sdk` only; the host-side loader lives in
`rpi-extensions`. See [`examples/plugin-stub`](examples/plugin-stub) and the
[`rpi-plugin-sdk` API docs](https://docs.rs/rpi-plugin-sdk) for the ABI
contract.

### Install from crates.io

```bash
rpi install rpi-extension-example
rpi install rpi-extension-example --version 0.1.0
```

The command resolves and builds the crate with Cargo in release mode, then
copies its `.dll`, `.so`, or `.dylib` into `~/.rpi/agent/extensions`.
For local development, use `rpi install my-extension --path ../my-rpi-extension --force`.

### Develop with watch mode

From a Rust extension crate (`[lib] crate-type` contains `"cdylib"`):

```bash
rpi dev
```

The command detects the Cargo package, performs an initial build, stages a
versioned library under `.rpi/extensions/.dev`, and watches the crate sources.
Successful source changes trigger a rebuild and live reload.
For a workspace containing multiple extensions, select one explicitly:

```bash
rpi dev --package rpi-todo
rpi dev --release
```

See [`docs/extension-authoring.md`](docs/extension-authoring.md) for complete
Rust extension templates, safety rules, testing, and release checklists.

## Features

- **Providers:** Anthropic Messages and OpenAI-compatible Chat Completions,
  plus a faux provider for tests. Third-party endpoints are configured through
  `~/.rpi/agent/models.json`; Anthropic endpoint overrides also support
  `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`.
- **Auth (in priority order):** `--api-key` → `~/.rpi/auth.json` (set via
  `rpi auth login`) → `~/.rpi/agent/models.json` `apiKey` → provider environment
  variables (`OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`).
- **Tools:** the CLI defaults to Pi's `read`, `write`, `edit`, and `bash`
  tools. The former rpi-only `grep`, `find`, `ls`, `docs`, and `powershell`
  implementations remain library code but are not loaded by default.
- **Extensions:** Rust `cdylib` plugins can be installed with `rpi install` and
  are discovered from project `.rpi/extensions`, legacy `.pi/extensions`,
  global `~/.rpi/agent/extensions`, and `--extensions-dir`.
- **Project resources:** rpi-owned skills, prompts, system instructions, and
  extensions use `.rpi/` first; the original Pi `.pi/` layout remains a
  compatibility fallback. When both contain the same skill or prompt name,
  `.rpi/` wins.
- **Sessions:** JSONL v4 durable backend + in-memory ephemeral; compaction + a
  split-turn two-LLM-call invariant.
- **Remote mode:** `rpi --server` runs the agent headless over TCP, and
  `rpi --connect <host:port> [--token <t>]` attaches a zero-local-resource TUI
  client (token auth; the token may also come from `RPI_SERVER_TOKEN`). See
  [`docs/remote-mode.md`](docs/remote-mode.md).

## Configuration

`rpi` persists credentials under `~/.rpi/` (override the dir with the
`RPI_CODING_AGENT_DIR` env var):

```
~/.rpi/
└── agent/
    ├── auth.json     # set with `rpi auth login` (0o600 on Unix)
    └── models.json   # optional: custom providers/models
```

`models.json` is a hand-edited file for custom Anthropic or OpenAI-compatible gateways:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "authHeader": true,
      "headers": { "x-portkey-key": "…" },
      "models": [
        { "id": "custom-claude", "name": "Custom Claude" }
      ]
    }
  }
}
```

Then `rpi --model gateway/custom-claude -p "hi"` routes to the gateway.
For an OpenAI-compatible endpoint, set `"api": "openai-completions"`.

See [docs/m6-cli-open-questions.md §4–5](docs/m6-cli-open-questions.md) for the
full auth precedence and default-selection rule.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=bigfish1913/pi-rust,pi-rust/rpi-package&type=Date)](https://www.star-history.com/#bigfish1913/pi-rust&pi-rust/rpi-package&Date)

## License

MIT. See [LICENSE](LICENSE).

This is a Rust port of [earendil-works/pi](https://github.com/earendil-works/pi)
(© Mario Zechner, MIT). The `rpi-*` crates are a Rust-native reimplementation of
the original MIT-licensed TypeScript SDK.
