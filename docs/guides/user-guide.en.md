# rpi User Guide

This guide describes the behavior implemented by the current `pi-rust` workspace. rpi is a native Rust coding-agent toolkit: the reusable runtime is published as crates, while `rpi-cli` provides a ready-to-run terminal application. The examples below use the current command names, configuration paths, and extension model; they are not a generic guide for the upstream Pi project.

## 1. Install and run

### Install the CLI

Install the published binary with Rust and Cargo:

```bash
cargo install rpi-cli
rpi --version
rpi --help
```

To build the development version from source:

```bash
git clone https://github.com/bigfish1913/pi-rust.git
cd pi-rust
cargo install --path crates/rpi-cli --force
```

`cargo install` installs the `rpi` executable. It is different from `rpi install`, which installs Rust dynamic-library extensions built against `rpi-plugin-sdk`.

### Build from the workspace

rpi is a Cargo workspace, not a Node/npm application:

```bash
cargo run -p rpi-cli
cargo run -p rpi-cli -- -p "Summarize README.md"
cargo run -p rpi-cli -- --mode json -p "List the crates in this project"
cargo build -p rpi-cli --release
```

The release binary is `target/release/rpi.exe` on Windows and normally `target/release/rpi` on Linux/macOS. The offline minimal example uses the deterministic faux provider and needs neither an API key nor network access:

```bash
cargo run -p minimal
```

### First prompt

Interactive mode:

```bash
rpi
rpi "Explain the structure of this project"
```

One-shot mode:

```bash
rpi -p "Summarize README.md"
rpi --mode json -p "List the risks in this project"
```

Prefix a file with `@` to include it in the first user message:

```bash
rpi @README.md "Suggest improvements for this project"
```

## 2. Models and credentials

### Credential precedence

When several sources are present, rpi checks them in this order:

1. `--api-key`
2. `~/.rpi/auth.json`, written by `rpi auth login`
3. The provider's `apiKey` in `~/.rpi/agent/models.json`
4. Environment variables

The built-in Anthropic provider accepts `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`; built-in OpenAI uses `OPENAI_API_KEY`. DeepSeek is an optional package declaring `DEEPSEEK_API_KEY`. Provider packages declare their own `apiKeyEnv`; custom providers can explicitly reference a variable in `models.json`. Using an OpenAI-compatible protocol does not inherit `OPENAI_API_KEY`. See the [provider integration specification](../extensions/provider-integration.md). Credential checks do not make a network request:

```bash
rpi auth check
rpi auth check --provider gateway --json
rpi auth login
rpi auth logout
```

Never commit API keys to the repository, `.rpi` resources, or documentation.

### Configure a custom provider

The default model catalog is `~/.rpi/agent/models.json`. Set `RPI_CODING_AGENT_DIR` to relocate the whole agent configuration directory:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "models": [{ "id": "custom-claude", "name": "Custom Claude" }]
    },
    "openai-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://api.example.com/v1",
      "apiKey": "sk-openai-secret",
      "models": [{ "id": "custom-model", "name": "Custom Model" }]
    }
  }
}
```

Select a model with either a provider/model pattern or separate flags:

```bash
rpi --model gateway/custom-claude -p "hello"
rpi --model openai-gateway/custom-model -p "hello"
rpi --provider gateway --model custom-claude -p "hello"
```

Without `--model`, rpi chooses an authenticated model according to its provider catalog. A compatible Anthropic gateway can also be selected with `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.

## 3. CLI modes and commands

Frequently used global options:

```text
--provider <name>       provider name
--model <pattern>       model id; supports provider/model and thinking suffixes
--api-key <key>         per-run credential override
--base-url <url>        endpoint override
--timeout <seconds>     LLM request timeout, default 600
--thinking <level>      off/minimal/low/medium/high/xhigh/max
--print, -p             run once and exit
--mode text|json|rpc    output mode
--continue, -c         continue the most recent session
--resume, -r           choose a saved session
--session <id|path>    select a session
--session-dir <dir>    session directory
--tools <list>         allow only these tools
--exclude-tools <list> disable selected tools
--no-tools             disable every tool
--no-builtin-tools     disable read/bash/edit/write/docs
--no-skills            skip skill discovery
--no-prompt-templates  skip prompt-template discovery
--no-context-files     skip AGENTS.md/CLAUDE.md discovery
--no-extensions        disable extension loading
--extensions-dir <dir> scan an extra extension directory
--package-only         load only project package/resources
--offline              disable startup network checks
--export <file>        export a JSONL session as HTML
--server               start the headless JSONL server
--connect <host:port>  connect a remote TUI client
```

Subcommands include:

```text
rpi auth login|check|logout
rpi events path|tail
rpi install <crate>
rpi uninstall <crate>
rpi update                 # download a verified GitHub prebuilt CLI
rpi update --build         # explicitly compile with Cargo
rpi dev [options]
rpi dev-local [options]
rpi package link|list|unlink
```

The startup `Context` summary lists the effective `SYSTEM.md`, `APPEND_SYSTEM.md`, `AGENTS.md`, and `CLAUDE.md` resources. `/context` shows the full list and `/reload` refreshes it. `/changelog` displays the release notes embedded in the current binary; `/changelog all` displays the complete history.

### Headless server and remote client

Run the agent without a local TUI and connect from another process:

```bash
rpi --server --port 9899
rpi --connect 127.0.0.1:9899 --token <token>

# Alternatively provide the token through the environment
export RPI_SERVER_TOKEN=<token>
rpi --connect 127.0.0.1:9899
```

The server prints a token at startup and enables token authentication by default. The remote client is a display and input client: it does not load local providers, tools, sessions, or extensions. Session files and execution remain on the server. Remote mode supports runtime operations such as `/state`, `/model`, `/thinking`, `/tools`, and `/abort`; operations that require local harness state are intentionally limited. See `docs/remote/user-guide.md` for the protocol and constraints.

## 4. Built-in tools and safety boundaries

The default tool set contains Pi-compatible `read`, `bash`, `edit`, and `write`, plus rpi's read-only `docs` lookup tool. Windows also registers `powershell` by default. The library includes `grep`, `find`, and `ls` implementations, but the CLI does not register them by default.

Restrict the default tool set in `.rpi/settings.json`:

```json
{
  "defaultTools": ["read", "bash", "edit", "write", "docs"]
}
```

You can also choose tools per run:

```bash
rpi --tools read,bash,edit,write,docs -p "Inspect and update the project"
rpi --no-tools -p "Explain this error without touching files"
```

The CLI's tool selection is not a complete operating-system sandbox. Treat `bash`, `powershell`, file writes, and native extensions as trusted capabilities. Add confirmation and filesystem isolation at the product boundary when untrusted users or repositories are involved.

## 5. Project resources and precedence

Project resources live under `.rpi`:

```text
project/
└── .rpi/
    ├── settings.json
    ├── SYSTEM.md
    ├── APPEND_SYSTEM.md
    ├── skills/
    ├── prompts/
    ├── themes/
    └── extensions/
```

Global resources normally live under `~/.rpi/agent/`. Project resources take precedence over global resources with the same name. Additional directories can be declared in `.rpi/settings.json`:

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"]
}
```

The shorthand keys `skills`, `prompts`, and `extensions` are also supported. Paths are relative to the project root; global settings resolve relative paths against the agent directory. Use `--package-only` or `--project-only` to load only the current project's package and resources. This is different from `--local-only`, which limits extensions to explicitly supplied local paths.

Set `RPI_CODING_AGENT_DIR=/work/rpi-agent` to relocate configuration and default sessions. Use `--no-session` when a temporary, non-persistent run is required.

## 6. Rust extensions

Native extensions are Rust `cdylib` libraries loaded by `rpi-extensions` and linked to the stable `rpi-plugin-sdk` ABI:

```bash
cargo build -p plugin-stub
rpi --extensions-dir target/debug -p 'echo "hi"'
```

Install a published extension or a local development package:

```bash
rpi install rpi-extension-example
rpi install rpi-extension-example --version 0.1.0
rpi install my-extension --path ../my-rpi-extension --force
```

`rpi install <name>` upgrades newer versions directly and skips an already installed version. Use `--force` to reinstall the same version or allow a downgrade. This also applies to Cargo builds and local installs; local changes without a version bump require `--force`.

A prebuilt archive may also carry files that the extension needs beside its library, such as a Node worker. The archive's `runtime-assets.json` index, written by the package's release builder, lists each file's path, size and SHA-256. The installer verifies every file before it installs anything, installs them beside the library in one transaction, and removes them on uninstall. Archives without an index install only the library. An `rpi` build without this support also installs only the library and does not reject the archive, so upgrade `rpi` before publishing a package that declares assets.

For iterative development:

```bash
cd ../my-rpi-extension
rpi dev
rpi dev --package my-extension
rpi dev-local
```

`rpi dev` detects Cargo `cdylib` targets, builds the extension, loads versioned artifacts from `.rpi/extensions/.dev`, and hot-reloads after source changes. If a rebuild fails, the last successfully loaded version remains active. `rpi dev-local` isolates the current extension from global extensions.

Link a development package to one project:

```bash
rpi package link --path ../my-extension --package my-extension
rpi package list
rpi package unlink my-extension
```

Project packages are stored in `.rpi/packages/<name>/`. They are trusted local Rust libraries, not npm packages. Installed libraries normally live under `~/.rpi/agent/extensions` or the directory selected by `RPI_CODING_AGENT_DIR`.

An extension may register tools, providers, event handlers, and resource handlers through the SDK. Keep data crossing the ABI inside SDK-defined serializable types and do not depend on private `rpi-cli` modules.

## 7. SDK integration

Compose only the layers an embedded application needs:

- `rpi-ai`: model types, providers, messages, and streaming events.
- `rpi-agent`: the Agent loop, tools, hooks, cancellation, and events.
- `rpi-tools`: built-in tools and the replaceable `ExecutionEnv` boundary.
- `rpi-harness`: sessions, JSONL persistence, compaction, skills, and context files.
- `rpi-cli`: the terminal application and configuration surface.

A minimal offline Agent looks like this:

```rust
use rpi_agent::{Agent, AgentEvent};
use rpi_ai::providers::faux::faux_provider;

let model = faux_provider().model("echo");
let mut agent = Agent::builder(model)
    .system_prompt("You are a helpful assistant.")
    .build();

let mut events = agent.subscribe();
agent.prompt("Hello from rpi").await?;
while let Some(event) = events.recv().await {
    if matches!(event, AgentEvent::AgentEnd { .. }) {
        break;
    }
}
```

Implement `rpi-agent::AgentTool` for a custom tool and `rpi-ai::Provider` for a model adapter. Combine the faux provider with `InMemoryExecutionEnv` to test message ordering, tool calls, failures, and completion without an API key, network, or real filesystem.

## 8. Troubleshooting

### Model or endpoint errors

Run `rpi auth check`, verify the provider name, model id, `models.json`, environment variables, and `baseUrl`. Use an explicit `provider/model` pattern to avoid ambiguity. Check authentication headers and endpoint compatibility before changing Agent code.

### Extension load failures

Confirm that the library is a platform-compatible `cdylib`, that its ABI version matches the host, and that `--extensions-dir` points to the directory containing the built artifact. Keep the complete host error output; it usually distinguishes discovery, loading, ABI, and registration failures.

### Event handler diagnostics

```bash
export RPI_EVENT_LOG=1
rpi
rpi events tail
```

The default log is `~/.rpi/logs/events.jsonl`; `RPI_EVENT_LOG_PATH` overrides it. A `BeforeTuiStart` veto returns exit code `3`. Inspect whether a handler returned `Continue`, `Error`, `Abort`, `Panic`, or `Timeout`.

### Resources missing from the startup context

Place files under the project `.rpi` directory or global `~/.rpi/agent`. Run `rpi --debug-system-prompt` to inspect the resolved system prompt, skills, and resource counts. Check project-over-global precedence and whether `--package-only`, `--no-skills`, or `--no-context-files` was supplied.

### Remote connection failures

Confirm that the server is listening on the expected address and port, and pass the exact startup token or `RPI_SERVER_TOKEN`. Remember that the client loads no local agent resources; provider credentials, extensions, and sessions must be available on the server.

## 9. Development, tests, and releases

Run the workspace checks before changing public APIs:

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
```

The normal dependency publication order is:

```text
rpi-telemetry -> rpi-ai -> rpi-agent -> rpi-tools -> rpi-harness
rpi-plugin-sdk -> rpi-extensions -> rpi-tui -> rpi-cli
```

When a release changes user-visible behavior, update the release notes, README, website `data/docs.json`, and both language versions of this guide. The website is a static site; updating the Markdown source does not publish it automatically—run the repository's deployment task after building.

## 10. Documentation map

- Online documentation: https://rpi.laofu.online/docs.html
- Repository: https://github.com/bigfish1913/pi-rust
- Rust API: https://docs.rs/rpi-agent and https://docs.rs/rpi-plugin-sdk
- Extension authoring: https://rpi.laofu.online/extension-authoring.md
- Remote mode: `docs/remote/user-guide.md`
- Architecture: `docs/architecture/overview.md`

When the online site and an installed binary differ, use the documentation shipped with the matching Git tag and release. The CLI's `docs` tool can query the local documentation snapshot without depending on the website.

### CLI self-update distribution

`rpi update` resolves the released version through crates.io and downloads the matching GitHub `rpi-release.json` distribution. It verifies the exact target, archive size, archive and executable SHA-256, and the executable’s version before transactional replacement. Windows applies the staged update after the running process exits. Missing manifests and failed downloads/checks do not silently invoke Cargo; use `--build` explicitly. `--force` reinstalls the resolved version and `--offline` only reports cached information. See [the CLI distribution protocol](../distribution/prebuilt-cli.md).
