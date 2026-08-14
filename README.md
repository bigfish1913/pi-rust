# pi-rust — Rust port of the Pi agent SDK

A Rust port of [earendil-works/pi](https://github.com/earendil-works/pi)'s SDK
layer — a library-first, multi-crate workspace for building personal LLM coding
agents in Rust, plus a `pi` CLI built on top.

## Crates

| Crate          | What it is                                                          |
|----------------|---------------------------------------------------------------------|
| `pi-telemetry` | Telemetry span/event contracts (noop default).                      |
| `pi-ai`        | Unified multi-provider LLM types + streaming (Anthropic + faux).    |
| `pi-agent`     | Agent runtime + loop, `AgentTool` trait, events, hooks, queues.     |
| `pi-tools`     | Built-in tools (`read`/`write`/`edit`/`bash`/`grep`/`find`/`ls`) + `ExecutionEnv`. |
| `pi-harness`   | `AgentHarness`: session tree, JSONL persistence, compaction, run loop. |
| `pi-cli`       | Terminal coding-agent CLI (`pi` binary) on top of the library crates. |

Dependency direction: `pi-telemetry → pi-ai → pi-agent → pi-tools → pi-harness → pi-cli`.

## Relationship to the TypeScript source

The TypeScript reference is checked out under `.reference/pi/` (read-only). Every
Rust module names the TS file it mirrors in its module-level doc comment. The
crate family is a Rust-native reimplementation, not a thin wrapper — it ports the
SDK surface (`pi-ai`, `pi-agent-core`, the harness tools, the session layer) and
the CLI, keeping the layering and behavior faithful while using idiomatic Rust
(`async`/`await`, `Arc`, `serde`, `tokio`).

## How you build an agent

```rust
use pi_agent::{Agent, AgentTool, AgentEvent};
use pi_ai::{providers::faux::faux_provider, Model};

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

- **Providers:** Anthropic (API-key auth) + a faux provider for tests. OAuth /
  Copilot auth is deferred — bring an `ANTHROPIC_API_KEY`.
- **Tools:** `read`, `write`, `edit`, `bash` (mutating, run through a
  `MutationQueue`) + `grep`, `find`, `ls` (read-only, in-process via the
  `FileSystem` trait — no `rg`/`fd` shell-out).
- **Sessions:** JSONL v4 durable backend + in-memory ephemeral; compaction + a
  split-turn two-LLM-call invariant.

## License

MIT. See [LICENSE](LICENSE).
