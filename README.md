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
| `rpi-tools`       | `pi-tools/`      | Pi-compatible coding tools (`read`/`write`/`edit`/`bash`) + `ExecutionEnv`. |
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

### Install a crates.io extension

`rpi install` installs Rust-native extensions directly from Cargo. The package
must expose an `rpi-plugin-sdk` compatible `cdylib` target:

```bash
rpi install rpi-extension-example
rpi install rpi-extension-example --version 0.1.0
rpi install rpi-extension-example --force
```

The command resolves and builds the crate with Cargo in release mode, then
copies its `.dll`, `.so`, or `.dylib` into `~/.rpi/agent/extensions` (or the
directory selected by `RPI_CODING_AGENT_DIR`). The extension is loaded on the
next `rpi` start. For local development, use
`rpi install my-extension --path ../my-rpi-extension --force`.

This is intentionally different from plain `cargo install`: `cargo install`
only copies executable targets, while rpi loads dynamic-library extensions.

### Develop an extension with watch mode

From a Rust extension crate (`[lib] crate-type` contains `"cdylib"`), start the
development host with:

```bash
rpi dev
```

The command detects the Cargo package, performs an initial build, stages a
versioned library under `.rpi/extensions/.dev`, and watches the crate sources.
Successful source changes trigger a rebuild and the same live reload used by
the TUI's `/reload` command. A failed build keeps the currently loaded plugin.
For a workspace containing multiple extensions, select one explicitly:

```bash
rpi dev --package rpi-todo
rpi dev --release
rpi dev --no-watch
```

See [`docs/extension-authoring.md`](docs/extension-authoring.md) for complete
Rust extension and Pi JS/TS package templates, safety rules, testing, and
release checklists.

### Load static Pi packages

rpi can load Pi packages, including their static resources and executable
JavaScript/TypeScript extensions. Install a package with:

```bash
rpi install-pi npm:@scope/my-package@1.0.0
rpi install-pi git:github.com/user/my-package@v1
rpi install-pi ./my-pi-package
```

卸载已安装的扩展或 Pi package：

```bash
rpi uninstall rpi-extension-example
rpi uninstall-pi npm:@scope/my-package
# 等价写法：rpi uninstall pi npm:@scope/my-package
```

The installer stores project packages under `.rpi/packages` (use `--global`
for `~/.rpi/agent/packages`), runs `npm install --omit=dev`, and enables the
resolved package in settings. Pi extensions are loaded by a long-lived Node.js
host; `registerTool`, `registerCommand`, and `resources_discover` are supported.
TypeScript uses Node's native type stripping when available, or a package-local
`jiti` dependency. Pi peer/runtime packages are installed and aliased from
nested `node_modules` when npm does not hoist them. Node.js is required, and
extension code has the same filesystem/network permissions as the current user.
Command handlers receive the core Pi context (`mode`, `hasUI`, `capabilities`,
`model`, `modelRegistry`, `ui.notify`, editor text access, and session metadata).
When the active Rust provider is available, `modelRegistry.getProvider(id)`
supports Pi-compatible `streamSimple()`/`complete()` calls; stream events are
delivered as an async-iterable snapshot and `result()` resolves to the final
assistant message. `ctx.ui.custom` fullscreen components are bridged through a
Node Component proxy with terminal input, resize, overlay, and lifecycle
events; extensions still need to stay within the Pi component contract.
The host also exposes the internal `pi.runtimeRequest(action, args)` bridge for
capabilities that are enabled by rpi; extensions should check
`ctx.capabilities` before using it.

For a package that is already present locally, enable it without downloading:

```bash
rpi package add ../my-pi-package
rpi package list
rpi package remove ../my-pi-package
```

The package may provide `skills/`, `prompts/`, `themes/`, `SYSTEM.md`, and
`APPEND_SYSTEM.md`. An optional `rpi` (or legacy `pi`) object in `package.json`
can override those resource paths; `rpi` wins when both are present. Package resources are loaded after project and
global resources, so `.rpi`/`.pi` and `~/.rpi/agent` always win collisions.
`--theme <name-or-path>` selects a package theme in the interactive TUI.

## Status (v1)

- **Providers:** Anthropic Messages and OpenAI-compatible Chat Completions,
  plus a faux provider for tests. Third-party endpoints are configured through
  `~/.rpi/agent/models.json`; Anthropic endpoint overrides also support
  `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`.
- **Auth (in priority order):** `--api-key` → `~/.rpi/auth.json` (set via
  `rpi auth login`) → `~/.rpi/agent/models.json` `apiKey` → provider environment
  variables (`OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`). `rpi auth
  login`/`check`/`logout` manage the stored credential.
- **Tools:** the CLI defaults to Pi's `read`, `write`, `edit`, and `bash`
  tools. The former rpi-only `grep`, `find`, `ls`, `docs`, and `powershell`
  implementations remain library code but are not loaded by default.
- **Extensions:** Rust `cdylib` plugins can be installed with `rpi install` and
  are discovered from project `.rpi/extensions`, legacy `.pi/extensions`,
  global `~/.rpi/agent/extensions`, and `--extensions-dir`.

- **Project resources:** rpi-owned skills, prompts, system instructions, and
  extensions use `.rpi/` first; the original Pi `.pi/` layout remains a
  compatibility fallback. When both contain the same skill or prompt name,
  `.rpi/` wins. Project `.rpi/settings.json` can add `skillDirs`, `promptDirs`,
  `extensionDirs`, and `packages` (with `.pi/settings.json` as fallback).
- **Pi packages:** static package resources are loaded from the package specs in
  `~/.rpi/agent/settings.json` (`packages` array). Skills, prompt templates,
  themes, system prompt fragments, and JavaScript/TypeScript extensions are
  supported through the Node host; Rust `cdylib` extensions remain available
  for native integrations.
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
