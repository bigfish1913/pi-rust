//! `pi-cli` — a terminal coding-agent CLI built on the pi-rust library crates.
//!
//! This is the post-SDK deliverable per the user's instruction "把sdk复制完后，把cli也写下"
//! (after the SDK port, also write the CLI). It mirrors the *CLI surface* of the
//! TypeScript reference `packages/coding-agent` ([`packages/coding-agent/src/cli.ts`]
//! → [`main.ts`] → [`cli/args.ts`] → [`modes/print-mode.ts`]), ported onto the Rust
//! `AgentHarness` instead of the TS `AgentSession`.
//!
//! # What is ported (v1 scope)
//!
//! - **Argument parsing** ([`args`]) — `parseArgs`/`printHelp` for the flags a
//!   harness-backed CLI actually honors: `--provider`/`--model`/`--api-key`,
//!   `--thinking`, `--print`/`-p`, `--mode {text,json}`, `-c`/`--continue`,
//!   `-r`/`--resume`, `--session`/`--session-dir`/`--no-session`, `--tools`/
//!   `-t`, `--exclude-tools`/`-xt`, `--no-tools`, `--no-builtin-tools`,
//!   `--system-prompt`, `--append-system-prompt`, `--name`/`-n`, `--verbose`,
//!   `--help`/`-h`, `--version`/`-v`, positional `messages`, and `@file`
//!   attachments.
//! - **Provider/model resolution** ([`provider`]) — Anthropic-only (v1), API key
//!   from `--api-key` → `ANTHROPIC_API_KEY`; model pattern `provider/id[:thinking]`
//!   resolved against the provider's catalog.
//! - **Harness construction + run loop** ([`session`]) — `OsExecutionEnv` +
//!   built-in `read`/`write`/`edit`/`bash` tools, durable JSONL session storage,
//!   `AgentHarness`, and the `prompt_text → outcome` run.
//! - **Output modes** ([`modes`]) — `print` (text, single-shot) and `json`
//!   (newline-delimited harness events), plus a simple interactive REPL.
//! - **`app`** ([`app`]) — argument dispatch, model resolution, harness build,
//!   mode dispatch, exit codes.
//!
//! # What is NOT ported (deferred — tracked in `docs/m6-cli-open-questions.md`)
//!
//! The TS `coding-agent` is a large, full-featured product. v1 deliberately
//! scopes to a minimal-but-real CLI exercising the Rust harness end-to-end:
//! no TUI (`modes/interactive`), no extension system, no package manager
//! (`install`/`remove`/`update`/`config`), no OAuth/Copilot auth, no HTML
//! export, no skills/prompt-template/theme discovery, no model cycling
//! (`--models`), no `--fork`/`--export`/`--list-models`. The library APIs for
//! many of these (skills, prompt templates, compaction, JSONL session fork)
//! already exist; wiring them is future work.
//!
//! [`packages/coding-agent/src/cli.ts`]: ../../.reference/pi/packages/coding-agent/src/cli.ts
//! [`main.ts`]: ../../.reference/pi/packages/coding-agent/src/main.ts
//! [`cli/args.ts`]: ../../.reference/pi/packages/coding-agent/src/cli/args.ts
//! [`modes/print-mode.ts`]: ../../.reference/pi/packages/coding-agent/src/modes/print-mode.ts

pub mod app;
pub mod args;
pub mod auth;
pub mod config;
pub mod extensions_actions;
pub mod extras;
pub mod interactive_tui;
pub mod modes;
pub mod provider;
pub mod resource_dirs;
pub mod resume_picker;
pub mod session;
pub mod settings;

/// Crate version, surfaced by `rpi --version`. Mirrors the TS `VERSION` export
/// (sourced from `package.json`; here from `env!("CARGO_PKG_VERSION")`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The application name used in help + version output. Mirrors TS `APP_NAME`
/// (the TS bin is `"pi"`; the Rust crate publishes under the `rpi-` namespace,
/// so the binary + displayed name is `"rpi"` to match).
pub const APP_NAME: &str = "rpi";
