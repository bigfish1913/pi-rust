# rpi — Rust port of the Pi agent SDK

[![rpi-cli on crates.io](https://img.shields.io/crates/v/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![rpi-plugin-sdk docs](https://docs.rs/rpi-plugin-sdk/badge.svg)](https://docs.rs/rpi-plugin-sdk)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions)

A Rust port of [earendil-works/pi](https://github.com/earendil-works/pi)'s SDK
layer — a library-first, multi-crate workspace for building personal LLM coding
agents in Rust, plus an `rpi` CLI built on top.

> **Naming.** The published crates use the `rpi-` prefix (the upstream `pi-*`
> names are owned on crates.io by a parallel port). The on-disk directories stay
> `crates/pi-*` for history; the `package.name` in each `Cargo.toml` is
> `rpi-*`, so `extern crate` / `use` paths are `rpi_ai`, `rpi_agent`, etc.

## Crates (published as `rpi-*`)

| Crate (crates.io) | On-disk dir      | What it is                                                          |
|-------------------|------------------|---------------------------------------------------------------------|
| `rpi-telemetry`   | `pi-telemetry/`  | Telemetry span/event contracts (noop default).                      |
| `rpi-ai`          | `pi-ai/`         | Unified multi-provider LLM types + streaming (Anthropic + faux).    |
| `rpi-agent`       | `pi-agent/`      | Agent runtime + loop, `AgentTool` trait, events, hooks, queues.     |
| `rpi-tools`       | `pi-tools/`      | Built-in tools (`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls`) + `ExecutionEnv`. |
| `rpi-harness`     | `pi-harness/`    | `AgentHarness`: session tree, JSONL persistence, compaction, run loop. |
| `rpi-cli`         | `pi-cli/`        | Terminal coding-agent CLI (`rpi` binary) on top of the library crates. |
| `rpi-plugin-sdk`   | `rpi-plugin-sdk/` | Stable C ABI for Rust-native plugins and extension discovery.        |
| `rpi-extensions`   | `rpi-extensions/` | Dynamic plugin loader and `AgentTool` adapter.                       |
| `rpi-tui`          | `pi-tui/`        | Terminal UI primitives used by the interactive CLI.                 |

Dependency direction: `rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`.

### Rust registry links

| Package | crates.io | docs.rs |
| --- | --- | --- |
| `rpi-cli` | [crates.io](https://crates.io/crates/rpi-cli) | [docs.rs](https://docs.rs/rpi-cli) |
| `rpi-agent` | [crates.io](https://crates.io/crates/rpi-agent) | [docs.rs](https://docs.rs/rpi-agent) |
| `rpi-plugin-sdk` | [crates.io](https://crates.io/crates/rpi-plugin-sdk) | [docs.rs](https://docs.rs/rpi-plugin-sdk) |
| `rpi-extensions` | [crates.io](https://crates.io/crates/rpi-extensions) | [docs.rs](https://docs.rs/rpi-extensions) |

The registry pages are the canonical entry points for installing the CLI or
embedding the SDK. The repository may contain unreleased changes; check the
published version shown on crates.io before depending on a new API.

## Relationship to the TypeScript source

The TypeScript reference is checked out under `.reference/pi/` (read-only). Every
Rust module names the TS file it mirrors in its module-level doc comment. The
crate family is a Rust-native reimplementation, not a thin wrapper — it ports the
SDK surface (`pi-ai`, `pi-agent-core`, the harness tools, the session layer) and
the CLI, keeping the layering and behavior faithful while using idiomatic Rust
(`async`/`await`, `Arc`, `serde`, `tokio`).

## How you build an agent

```rust
use rpi_agent::{Agent, AgentTool, AgentEvent};
use rpi_ai::{providers::faux::faux_provider, Model};

let model = faux_provider().model("echo");
let mut agent = Agent::builder(model)
    .system_prompt("You are a helpful assistant.")
    .tool(MyTool)
    .build();

let mut events = agent.subscribe();
tokio::spawn(async move {
    while let Some(ev) = events.recv().await {
        match ev { /* AgentEvent::MessageUpdate { .. }, etc. */ }
    }
});

agent.prompt("Hello!").await.unwrap();
```

See [docs/architecture.md](docs/architecture.md) for the full design.

## Build a plugin

Plugins are Rust `cdylib` libraries loaded through the stable ABI exposed by
`rpi-plugin-sdk`. The repository includes a complete `echo` tool example that
also exercises event and resource discovery:

```bash
cargo build -p plugin-stub
```

Then point the CLI at the directory containing the generated library (the
extension is named `plugin_stub.dll`, `libplugin_stub.so`, or
`libplugin_stub.dylib` depending on the platform):

```bash
rpi --extensions-dir target/debug -p 'echo "hi"'
```

The plugin depends on `rpi-plugin-sdk` only; the host-side loader lives in
`rpi-extensions`. See [`examples/plugin-stub`](examples/plugin-stub) and the
[`rpi-plugin-sdk` API docs](https://docs.rs/rpi-plugin-sdk) for the ABI
contract.

## Status (v1)

- **Providers:** Anthropic Messages and OpenAI-compatible Chat Completions,
  plus a faux provider for tests. Third-party endpoints are configured through
  `~/.rpi/agent/models.json`; Anthropic endpoint overrides also support
  `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`.
- **Auth (in priority order):** `--api-key` → `~/.rpi/auth.json` (set via
  `rpi auth login`) → `~/.rpi/agent/models.json` `apiKey` → provider environment
  variables (`OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`). `rpi auth
  login`/`check`/`logout` manage the stored credential.
- **Tools:** `read`, `write`, `edit`, `bash` (mutating, run through a
  `MutationQueue`) + `grep`, `find`, `ls` (read-only, in-process via the
  `FileSystem` trait — no `rg`/`fd` shell-out).
- **Sessions:** JSONL v4 durable backend + in-memory ephemeral; compaction + a
  split-turn two-LLM-call invariant.

## Configuration

`rpi` persists credentials under `~/.rpi/` (override the dir with the
`RPI_CODING_AGENT_DIR` env var):

```
~/.rpi/
└── agent/
    ├── auth.json     # set with `rpi auth login` (0o600 on Unix)
    └── models.json   # optional: custom providers/models
```

`auth.json` holds the stored API key for `anthropic` (written by
`rpi auth login`, removed by `rpi auth logout`); `auth check` reports whether
any auth source is ready without touching the network.

`models.json` is a hand-edited file for custom Anthropic or OpenAI-compatible gateways:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "authHeader": true,             // wrap apiKey as Authorization: Bearer
      "headers": { "x-portkey-key": "…" }, // optional extra headers
      "models": [
        { "id": "custom-claude", "name": "Custom Claude" }
      ]
    }
  }
}
```

Then `rpi --model gateway/custom-claude -p "hi"` routes to the gateway (the
`gateway/` prefix is CLI namespacing). For an OpenAI-compatible endpoint, set
`"api": "openai-completions"`; its `apiKey` is sent as a Bearer token and the
provider streams `/v1/chat/completions`.

**Default model (no `--model`).** A bare `rpi -p "hi"` picks the default the way
native pi does — the first *authenticated* model in the catalog when the
built-in default isn't authenticated. So a `models.json`-only Anthropic or
OpenAI gateway setup "just works": the gateway model is the only authenticated
one, so `rpi -p "hi"` routes through it — no `--model` needed. With a standard
`ANTHROPIC_API_KEY`/`auth.json`/`--api-key` setup, the built-in
`claude-sonnet-5` remains the default.
See
[docs/m6-cli-open-questions.md §4–5](docs/m6-cli-open-questions.md) for the
full auth precedence, the default-selection rule, the `~/.rpi`-flat-vs-nested
divergence, and what's deferred (OAuth, `$ENV` credential expansion,
multi-provider registry).

## Releasing

Publish the crate family in dependency order with `cargo publish` (run
`cargo login` once first so `~/.cargo/credentials.toml` exists with a
publish-scoped token; crates.io records are permanent):

```
# dep order: telemetry → ai → agent → tools → harness → plugin-sdk → extensions → tui → cli
for c in rpi-telemetry rpi-ai rpi-agent rpi-tools rpi-harness rpi-plugin-sdk rpi-extensions rpi-tui rpi-cli; do
  cargo publish -p "$c"
done
```

The workspace `Taskfile.yml` (`task dry-run` / `task publish`) used to wrap
`release.ps1`/`release.sh`, but those scripts were removed; publish directly
as above.

## License

MIT. See [LICENSE](LICENSE).

This is a Rust port of [earendil-works/pi](https://github.com/earendil-works/pi)
(© Mario Zechner, MIT). The `rpi-*` crates are a Rust-native reimplementation of
the original MIT-licensed TypeScript SDK; the upstream source is checked out
under `.reference/pi/` (read-only, gitignored).
