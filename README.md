# rpi — Rust port of the Pi agent SDK

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

Dependency direction: `rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`.

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

## Status (v1)

- **Providers:** Anthropic (API-key auth) + a faux provider for tests. Also
  supports **third-party Anthropic-compatible endpoints** (one-api/new-api/
  claude-code-router/private proxies) via `ANTHROPIC_BASE_URL`/`--base-url` +
  Bearer auth (`ANTHROPIC_AUTH_TOKEN` or a `~/.rpi/models.json` gateway with
  `authHeader: true`). OAuth / Copilot device-code auth is deferred.
- **Auth (in priority order):** `--api-key` → `~/.rpi/auth.json` (set via
  `rpi auth login`) → `~/.rpi/models.json` gateway (`authHeader:true`+`apiKey`) →
  `ANTHROPIC_AUTH_TOKEN` (Bearer) → `ANTHROPIC_API_KEY` (x-api-key). `rpi auth
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
├── auth.json     # set with `rpi auth login` (0o600 on Unix)
└── models.json   # optional: custom Anthropic-compatible providers/models
```

`auth.json` holds the stored API key for `anthropic` (written by
`rpi auth login`, removed by `rpi auth logout`); `auth check` reports whether
any auth source is ready without touching the network.

`models.json` is a hand-edited file for custom Anthropic-compatible gateways:

```jsonc
{
  "providers": {
    "gateway": {
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
`gateway/` prefix is CLI namespacing; v1 routes every `anthropic-messages`
model through its single AnthropicProvider, using the model's `base_url` +
`headers` to reach the endpoint).

**Default model (no `--model`).** A bare `rpi -p "hi"` picks the default the way
native pi does — the first *authenticated* model in the catalog when the
built-in default isn't authenticated. So a `models.json`-only gateway setup
(no Anthropic key) "just works": the gateway model is the only authenticated
one, so `rpi -p "hi"` routes through it — no `--model` needed. With a standard
`ANTHROPIC_API_KEY`/`auth.json`/`--api-key` setup, the built-in
`claude-sonnet-5` remains the default. (A gateway key is folded onto the
gateway models only; an `ANTHROPIC_AUTH_TOKEN` is folded onto every model.)
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
# dep order: telemetry → ai → agent → tools → harness → cli
for c in rpi-telemetry rpi-ai rpi-agent rpi-tools rpi-harness rpi-cli; do
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
