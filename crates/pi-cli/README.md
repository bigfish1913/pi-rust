# rpi-cli

[![crates.io](https://img.shields.io/crates/v/rpi-cli.svg)](https://crates.io/crates/rpi-cli)
[![docs.rs](https://docs.rs/rpi-cli/badge.svg)](https://docs.rs/rpi-cli)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions)

The terminal coding-agent CLI — the `rpi` binary — built on the `rpi-*` library
crates. It is the ready-to-run surface of the
[rpi](https://github.com/bigfish1913/pi-rust) project: an interactive TUI, a
one-shot `-p` mode, a JSON event mode, durable sessions, and Rust `cdylib`
plugin loading.

## Install

```bash
cargo install rpi-cli
```

Requires Rust 1.78 or newer. Then point it at a provider and start it:

```bash
export ANTHROPIC_API_KEY=...     # or OPENAI_API_KEY, or ~/.rpi/agent/models.json
rpi
```

One-shot, and machine-readable output:

```bash
rpi -p "summarize the README"
rpi --mode json -p "list the crates"
```

## Authentication

Credentials are resolved in priority order:

1. `--api-key` on the command line
2. `~/.rpi/auth.json`, written by `rpi auth login` (mode `0o600` on Unix)
3. the `apiKey` field of a provider in `~/.rpi/agent/models.json`
4. provider environment variables: `OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   `ANTHROPIC_API_KEY`

`~/.rpi/` can be relocated with `RPI_CODING_AGENT_DIR`.

## Custom providers and gateways

Any Anthropic-Messages or OpenAI-compatible endpoint can be declared in
`~/.rpi/agent/models.json`:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "authHeader": true,
      "headers": { "x-portkey-key": "…" },
      "models": [{ "id": "custom-claude", "name": "Custom Claude" }]
    }
  }
}
```

```bash
rpi --model gateway/custom-claude -p "hi"
```

Use `"api": "openai-completions"` for an OpenAI-compatible endpoint. Anthropic
endpoint overrides also honour `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.

## Sessions and resources

- Sessions are JSONL v4 files under the session directory, with branching and
  context compaction.
- Project resources are discovered from `.rpi/` first, with `.pi/` as the Pi
  compatibility fallback. When both define the same skill or prompt name, `.rpi/`
  wins.
- Plugins are loaded from project `.rpi/extensions`, legacy `.pi/extensions`,
  global `~/.rpi/agent/extensions`, and `--extensions-dir`.

## Plugins

```bash
rpi install rpi-extension-example              # from crates.io
rpi install rpi-extension-example --version 0.1.0
rpi install my-extension --path ../my-extension --force   # local
rpi dev                                        # build + watch a local extension
```

`rpi install` builds the crate in release mode with Cargo and copies the
resulting `.dll` / `.so` / `.dylib` into `~/.rpi/agent/extensions`.

## Remote mode

```bash
rpi --server                      # headless agent over TCP, prints a token
rpi --connect host:port --token T # zero-local-resource TUI client
```

The client holds no local provider, tools, extensions or session files; all
agent work happens on the server. See
[docs/remote-mode.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/remote-mode.md).

## Default tools

The CLI loads the Pi-compatible `read`, `write`, `edit` and `bash` tools.
Additional implementations (`grep`, `find`, `ls`, `docs`, `powershell`) remain
available in the [`rpi-tools`](https://crates.io/crates/rpi-tools) library and
through the `default_tools` setting.

## Library usage

The crate also exposes its internals, so the CLI surface can be embedded:

```toml
[dependencies]
rpi-cli = "0.3"
```

For building your own agent, depend on
[`rpi-agent`](https://crates.io/crates/rpi-agent) and
[`rpi-harness`](https://crates.io/crates/rpi-harness) directly instead — see
[docs/agent-project.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent-project.md).

## Documentation

- User guide: <https://rpi.laofu.online/docs.html>
- Architecture: [docs/architecture.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture.md)
- Extension authoring: [docs/extension-authoring.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/extension-authoring.md)
- Changelog: [CHANGELOG.md](https://github.com/bigfish1913/pi-rust/blob/main/CHANGELOG.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

This crate is part of a Rust port of the MIT-licensed TypeScript `pi` SDK
(© Mario Zechner).
