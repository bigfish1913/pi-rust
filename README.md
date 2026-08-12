# pi-rust — Rust port of the Pi agent SDK

A Rust port of [earendil-works/pi](https://github.com/earendil-works/pi)'s SDK layer (`pi-ai` + `pi-agent-core` + built-in tools + session persistence). **No CLI** — the goal is a library that makes it easy to build your own agent in Rust. A CLI crate is reserved for later.

## Relationship to the TypeScript source

The TypeScript reference is checked out under `.reference/pi/` (read-only). Every Rust module names the TS file it mirrors in its module-level doc comment.

| TS package                  | Rust crate        | Status        |
|-----------------------------|-------------------|---------------|
| `pi-ai`                     | `pi-ai`           | core + faux + anthropic; others via trait |
| `pi-agent-core` (core loop) | `pi-agent`        | done          |
| `pi-agent-core` (harness)   | `pi-harness`      | skeleton (phase 2) |
| built-in tools              | `pi-tools`        | read/write/bash + `ExecutionEnv` trait |
| `pi-telemetry`              | `pi-telemetry`    | minimal noop  |
| `pi-coding-agent` (CLI)     | `pi-cli`          | reserved (future) |

## Workspace layout

```
crates/
  pi-telemetry/  → pi-ai       (span/event contracts, noop impl)
  pi-ai/         → pi-agent    (types, models, Provider trait, streaming, providers)
  pi-agent/      → pi-tools    (Agent, agent loop, AgentTool trait, events, hooks, queues)
  pi-tools/      → pi-harness  (ExecutionEnv + read/write/bash)
  pi-harness/    → pi-cli      (AgentHarness: session tree, compaction, JSONL)
  pi-cli/        (future binary)
examples/
  minimal/       — agent with the faux provider, no I/O
  tools/         — agent with read/write/bash over a real FileSystem
```

Dependency direction: `pi-telemetry → pi-ai → pi-agent → pi-tools → pi-harness → pi-cli`.

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
