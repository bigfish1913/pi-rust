# Code Organization

This workspace is organized by responsibility. The crate boundaries are the
primary architecture boundary; the files inside a crate are implementation
details and should only be moved when a public module path can be preserved.

## Dependency flow

```text
rpi-telemetry
      |
    rpi-ai
      |
   rpi-agent
      |
   rpi-tools
      |
  rpi-harness
      |
    rpi-cli ---- rpi-tui
      |
  rpi-extensions ---- rpi-plugin-sdk
```

The arrows describe the normal dependency direction. `rpi-plugin-sdk` is a
small ABI-only crate that plugin authors can depend on without pulling in the
host runtime. `rpi-tui` is a rendering library used by the CLI, not a second
agent runtime.

## Functional groups

| Group | Crates and paths | Responsibility |
| --- | --- | --- |
| Model and transport | `crates/pi-ai/` | Provider-neutral messages, schemas, streaming events, model catalog, Anthropic/OpenAI-compatible providers, and retry/wire mapping. |
| Agent runtime | `crates/pi-agent/` | The provider-agnostic loop, tool contract, event stream, hooks, abort handling, and steering/follow-up queues. |
| Built-in tools | `crates/pi-tools/` | `ExecutionEnv`, filesystem/shell adapters, mutation serialization, truncation, image detection, and `read`/`write`/`edit`/`bash`/`grep`/`find`/`ls`. |
| Durable harness | `crates/pi-harness/` | Session tree, memory/JSONL persistence, compaction, context files, skills, prompt templates, system-prompt composition, and the `AgentHarness` facade. |
| CLI and application wiring | `crates/pi-cli/` | Argument parsing, auth/config, provider resolution, session construction, output modes, interactive mode, extension actions, and the `rpi` binary entry point. |
| Terminal UI | `crates/pi-tui/` | Reusable terminal components, editor/input behavior, markdown/diff rendering, selectors, themes, and the interactive screen. |
| Native extensions | `crates/rpi-plugin-sdk/`, `crates/rpi-extensions/` | Stable C ABI for `cdylib` plugins plus host-side discovery, loading, resource registration, and tool/event adapters. |
| Examples and operations | `examples/`, `deploy/`, `website/`, `Taskfile.yml` | Minimal embedding examples, plugin smoke fixture, deployment configuration, documentation site, and repeatable workspace tasks. |

## Important module boundaries

The larger crates already have a useful second-level grouping:

```text
pi-harness/src/
  session/       session state, reducers, memory and JSONL backends
  compaction/    cut points, summaries, token accounting and settings
  skills.rs      skill discovery, metadata validation and formatting
  prompt_templates.rs
  system_prompt.rs
  agent_harness.rs

pi-tools/src/
  tools/         one module per built-in tool plus shared tool context
  env.rs         ExecutionEnv/FileSystem/Shell contracts
  os_env.rs      real filesystem and process implementation
  in_memory.rs   deterministic test implementation
  file_mutation_queue.rs
  truncate.rs, image.rs, path_utils.rs, shell_output.rs

pi-cli/src/
  app.rs, args.rs, modes.rs, interactive_tui.rs
                 application lifecycle and output modes
  config.rs, auth.rs, settings.rs, resource_dirs.rs
                 persistent configuration and project resources
  provider.rs, session.rs
                 model selection and harness construction
  extensions_actions.rs, install.rs
                 native extension lifecycle
  content_tools.rs
                 project-local interactive-content tool adapter
```

`pi-cli` intentionally keeps these modules flat: callers and tests use the
module paths as the CLI's public integration surface. The functional grouping
above is therefore documented rather than implemented as a risky mass rename.

## Project resources and generated output

Project-owned instructions live under `.pi/` so the same checkout can be used
by the native-style CLI and by the Rust `rpi` binary:

```text
.pi/
  SYSTEM.md                 project system prompt
  APPEND_SYSTEM.md         project prompt suffix
  skills/<name>/SKILL.md   project skills discovered by the harness
  sessions/                 local runtime state (ignored)

artifacts/                  interactive-content output (ignored)
target/                     Cargo build output (ignored)
.deploy/                    generated deployment bundle (ignored)
```

The `.pi` prompt and skill files are source inputs and are versioned. Session
logs, build products, DLLs and generated previews are not source and must not
be committed. Native extension binaries are reproduced with the plugin build
workflow and loaded from the runtime extension directory.

## Where to add a feature

1. Add provider protocol or model behavior to `rpi-ai`.
2. Add loop semantics or a tool lifecycle contract to `rpi-agent`.
3. Add an operating-system capability or built-in coding tool to `rpi-tools`.
4. Add persistence, skill/template loading, or prompt composition to
   `rpi-harness`.
5. Add flags, configuration, resource discovery, or user-facing commands to
   `rpi-cli`.
6. Keep terminal presentation in `rpi-tui`; keep plugin ABI changes in
   `rpi-plugin-sdk` and host loading in `rpi-extensions`.

This keeps reusable library crates independent from terminal concerns and
prevents project-specific interactive-content behavior from leaking into the
generic agent loop.

## Verification by group

```powershell
cargo fmt --all -- --check
cargo test -p rpi-ai
cargo test -p rpi-agent
cargo test -p rpi-tools
cargo test -p rpi-harness
cargo check --workspace
```

When changing skill metadata, also test a real `SKILL.md` containing block
sequences (for example `triggers:`). The harness frontmatter parser supports
the common nested mapping/sequence forms without requiring `serde_yaml`; YAML
anchors, aliases, multiline scalars and multi-document files remain out of
scope.
