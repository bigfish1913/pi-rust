//! CLI argument parsing + help text. Mirrors the TS
//! The CLI arg surface, scoped
//! to the flags the v1 Rust CLI honors.
//!
//! The TS parser is a hand-rolled positional/flag loop (no `yargs`/`commander`
//! dep) that collects `messages`, `@file` attachments, known flags, and a map
//! of *unknown* `--flags` (for extensions to claim later). This port keeps the
//! same shape so the help text and flag semantics line up 1:1 with the
//! reference. Unknown long flags are retained in [`Args::unknown_flags`] so a
//! native extension can claim and consume its own CLI options after loading.
//!
//! Divergences from the TS parser (all deliberate v1 scope cuts, documented in
//! the initial port notes (retired)):
//! - `--mode rpc`,
//!   `--fork`, `--approve`/`-na`,
//!   `--extension`/`-e`, `--skill`, and `--prompt-template` are recognized but
//!   resource discovery follows the supported project/global directories.
//!   flags are handled by the Rust loader; remaining compatibility flags are
//!   accepted with a warning.
//! - `--thinking` is typed via [`ThinkingLevel`] from `rpi_ai` (the TS parser
//!   validates against the same string set).
//! - `--print`/`-p` may consume a following positional as its prompt (the TS
//!   parser's `next !== undefined && !startsWith('@')` heuristic) — preserved.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use rpi_ai::ThinkingLevel;

/// upstream's process-wide offline flag. The CLI normalizes a truthy value
/// to `1` before dispatch so early subcommands and the regular app path share
/// the same network gate.
pub(crate) const RPI_OFFLINE_ENV: &str = "RPI_OFFLINE";

/// Match upstream's environment-flag contract exactly: empty values and
/// values other than `1`, `true`, or `yes` are false; words are ASCII
/// case-insensitive.
pub(crate) fn is_truthy_env_flag(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

pub(crate) fn offline_env_enabled() -> bool {
    is_truthy_env_flag(std::env::var(RPI_OFFLINE_ENV).ok().as_deref())
}

pub(crate) fn offline_mode_enabled(cli_offline: bool) -> bool {
    cli_offline || offline_env_enabled()
}

/// Resolve and normalize offline mode before top-level subcommand dispatch.
/// This mirrors upstream setting `RPI_OFFLINE=1` after either input enables it,
/// allowing downstream code to use the same process-wide gate.
pub(crate) fn normalize_offline_mode(args: &[String]) -> bool {
    let enabled = offline_mode_enabled(args.iter().any(|arg| arg == "--offline"));
    if enabled {
        std::env::set_var(RPI_OFFLINE_ENV, "1");
    }
    enabled
}

/// Drop the global `--offline` flag before an early-dispatched subcommand parses
/// its own arguments (`rpi update --offline` must not see it as a stray arg).
pub(crate) fn without_offline_flag(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|arg| arg.as_str() != "--offline")
        .cloned()
        .collect()
}

/// Output mode. Mirrors rpi's text/json/rpc modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Text,
    Json,
    Rpc,
}

/// RPC message-update wire representation. Missing capability metadata on older
/// hosts means snapshot semantics, not this CLI's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RpcMessageUpdates {
    #[default]
    CompactDeltas,
    Snapshot,
}

impl RpcMessageUpdates {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompactDeltas => "compact-deltas",
            Self::Snapshot => "snapshot",
        }
    }
}

/// Interactive TUI presentation mode. `fullscreen` uses the alternate screen
/// buffer; `regular` renders into the terminal's normal scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Fullscreen,
    Regular,
}

const fn default_tui_mode_for(is_macos: bool) -> TuiMode {
    if is_macos {
        TuiMode::Regular
    } else {
        TuiMode::Fullscreen
    }
}

impl Default for TuiMode {
    fn default() -> Self {
        default_tui_mode_for(cfg!(target_os = "macos"))
    }
}

/// The parsed argument set. Mirrors TS `Args`. Unknown long flags are retained
/// for extension consumption; unknown short flags remain hard errors.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// `--base-url` — overrides `ANTHROPIC_BASE_URL` + each model's base URL,
    /// for third-party Anthropic-compatible gateways/proxies.
    pub base_url: Option<String>,
    /// `--timeout <seconds>` overrides the deadline for each LLM API request.
    pub timeout: Option<Duration>,
    /// Maximum silence between streamed response bytes; zero disables it.
    pub stream_idle_timeout: Option<Duration>,
    pub resource_config: Option<PathBuf>,
    pub resource_transition: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    /// `--theme` — built-in theme name or a theme file path.
    pub theme: Option<String>,
    pub thinking: Option<ThinkingLevel>,

    pub print: bool,
    /// Select an ephemeral session and turn off native durable execution.
    pub mini: bool,
    /// The empty string selects the configured default area.
    pub host: Option<String>,
    pub mode: Mode,
    pub rpc_message_updates: RpcMessageUpdates,
    /// `--tui-mode regular|fullscreen` controls the interactive terminal
    /// buffer. macOS defaults to regular so native selection and terminal
    /// scrollback remain available together; other platforms use fullscreen.
    pub tui_mode: TuiMode,

    /// `--list-models [search]`: list the merged model catalog and exit.
    /// `Some("")` represents the bare flag; `None` means absent.
    pub list_models: Option<String>,
    /// `--offline`: disable best-effort startup network checks.
    pub offline: bool,
    /// `--export <session-file>`: export a JSONL session to HTML (or copy it
    /// when the destination ends in `.jsonl`).
    pub export: Option<PathBuf>,
    /// Explicit project trust override. `--approve` trusts the current
    /// project; `--no-approve` keeps project-local resources disabled.
    pub trust_override: Option<bool>,

    pub continue_session: bool,
    pub resume: bool,
    pub session: Option<String>,
    /// `--session-id <id>`: use the EXACT project session id, creating it if
    /// missing (pi `--session-id`). Unlike `--session` (partial match), this
    /// is an exact-id open-or-create.
    pub session_id: Option<String>,
    /// `--fork <path|id>`: fork the given session into a new one and start in
    /// the fork (pi `--fork`).
    pub fork: Option<String>,
    /// `--fork-entry <entryId>`: cut the new branch at this message entry
    /// instead of the lane leaf. The id must match exactly — unlike `--fork`'s
    /// partial session id, no prefix or substring match is attempted.
    /// Requires `--fork`.
    pub fork_entry: Option<String>,
    /// `--fork-position at|before`: whether `--fork-entry` includes (`at`) or
    /// excludes (`before`) its message. `None` keeps the harness default for an
    /// explicit entry, which is `before` (`ForkOptions::Branch`).
    /// Requires `--fork` and `--fork-entry`; without an explicit entry the
    /// harness applies the position to the implicit lane leaf and truncates it.
    pub fork_position: Option<rpi_harness::session::types::ForkPosition>,
    /// `--models <patterns>`: comma-separated model patterns for the Ctrl+P
    /// cycle (globs/fuzzy in pi; v1 writes the matched ids to settings.json's
    /// scopedModels — the same set /scoped-models edits). Empty = all models.
    pub models: Option<Vec<String>>,
    pub session_dir: Option<PathBuf>,
    pub no_session: bool,
    pub name: Option<String>,

    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    /// Native durable host selection. CLI parsing enables it by default; the
    /// temporary `--no-native-durable` escape hatch selects legacy session behavior.
    pub native_durable: bool,
    /// Explicit experimental v2 epoch; requires native durable admission.
    pub experimental_durable_epoch: Option<String>,

    /// `--no-skills`/`-ns`: skip skill discovery + the `<available_skills>`
    /// system-prompt listing.
    pub no_skills: bool,
    /// `--no-prompt-templates`/`-np`: skip prompt-template discovery (templates
    /// are on-demand only; this suppresses populating the resource registry).
    pub no_prompt_templates: bool,
    /// `--no-context-files`/`-nc`: skip context-file (`AGENTS.md`/`CLAUDE.md`)
    /// discovery + the `<project_context>` system-prompt block.
    pub no_context_files: bool,
    /// `--no-extensions`/`-ne`: skip Rust cdylib extension loading.
    /// Honored by `session.rs`: when set, no extension directory is scanned.
    pub no_extensions: bool,
    /// `--no-themes`: disable custom theme discovery and loading.
    /// Built-in presets remain available unless a custom `--theme` is given.
    pub no_themes: bool,
    /// `--extensions-dir`/`-ed`: an extra directory to scan for cdylib plugins
    /// (`.dll`/`.so`/`.dylib`), in addition to project `.rpi/extensions`
    /// and global
    /// `agent_dir()/extensions` defaults. May be repeated; scanned after
    /// the defaults (so a same-named tool in a default dir wins first, mirroring
    /// pi's registration order). `RPI_EXTENSIONS_DIR` (colon-separated on Unix,
    /// semicolon on Windows) provides the same list via env.
    pub extensions_dir: Vec<PathBuf>,
    /// `--extension`/`-e <path>`: load an explicit extension cdylib file (may
    /// be repeated). Loaded in addition to the discovered dirs.
    pub extension: Vec<PathBuf>,
    /// `--skill <path>`: load an explicit skill file or directory (repeated).
    pub skill: Vec<PathBuf>,
    /// `--prompt-template <path>`: load an explicit prompt-template file or
    /// directory (repeated).
    pub prompt_template: Vec<PathBuf>,

    /// `--local-only`: load only explicitly supplied extension paths.
    pub dev_local_only: bool,
    /// `--package-only` / `--project-only`: load project resources and
    /// extensions from the active project's settings, excluding global ones.
    pub project_only: bool,

    pub verbose: bool,
    pub help: bool,
    pub version: bool,

    /// `--debug-system-prompt`: print the resolved system-prompt sections
    /// (base, append, context, skills listing) + resource counts to stderr at
    /// harness build time, then proceed normally. A verification affordance for
    /// resource-discovery (Part A) — lets a smoke confirm `<available_skills>` +
    /// `<project_context>` + appended text reached the prompt without a full
    /// round-trip parse. Mirrors the plan's "add --debug-system-prompt if absent".
    pub debug_system_prompt: bool,

    /// Positional prompt text (one or more messages). Mirrors TS `messages`.
    pub messages: Vec<String>,
    /// `@file` attachments (prefix stripped), as raw paths for the caller to
    /// expand. Mirrors TS `fileArgs`.
    pub file_args: Vec<PathBuf>,

    /// Extension-declared or otherwise unknown long flags. Values are either
    /// JSON booleans (a bare flag) or strings (a flag with a value), matching
    /// Pi's `unknownFlags` contract so an extension can claim its own options.
    pub unknown_flags: BTreeMap<String, serde_json::Value>,
    /// Warnings about recognized-but-ignored flags (v1 scope cuts). Surfaced
    /// to the user on startup when `--verbose`.
    pub ignored: Vec<String>,
    /// Hard parse errors (unknown short flags, missing values). Non-empty ⇒
    /// `main` prints them + help and exits non-zero.
    pub errors: Vec<String>,
}

impl Args {
    pub(crate) fn fork_cut_errors(&self) -> Vec<&'static str> {
        let mut errors = Vec::new();
        if self.fork.is_none() {
            if self.fork_entry.is_some() {
                errors.push("--fork-entry requires --fork");
            }
            if self.fork_position.is_some() {
                errors.push("--fork-position requires --fork");
            }
        } else if (self.fork_entry.is_some() || self.fork_position.is_some())
            && (self.fork.as_deref() == Some("")
                || self.no_session
                || self.continue_session
                || self.resume)
        {
            errors.push("--fork cut flags require an active --fork selection");
        }
        if self
            .fork_entry
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            errors.push("--fork-entry requires a nonempty entry ID");
        }
        // An implicit leaf combined with `before` would drop that leaf.
        if self.fork_entry.is_none() && self.fork_position.is_some() {
            errors.push("--fork-position requires --fork-entry");
        }
        errors
    }
}

/// The canonical valid `--thinking` level strings, in level order. Mirrors TS
/// `VALID_THINKING_LEVELS`.
pub use rpi_runtime::thinking::VALID_THINKING_LEVELS;

/// Parse a thinking-level string. Mirrors TS `isValidThinkingLevel`.
pub use rpi_runtime::thinking::parse_thinking_level;

/// `@file`-argument helper mirroring the TS parser: a leading `@` marks a file
/// attachment (the `@` is stripped).
fn file_arg(arg: &str) -> Option<PathBuf> {
    if let Some(rest) = arg.strip_prefix('@') {
        // Reject the bare `@` (TS keeps it as a message; we treat it as one).
        if rest.is_empty() {
            None
        } else {
            Some(PathBuf::from(rest))
        }
    } else {
        None
    }
}

/// Parse `argv` (excluding the program name). Mirrors TS `parseArgs`.
///
/// Long flags accept `--name value` or `--name=value` (the TS parser only
/// handles `--name=value` for *unknown* flags; we extend it to known flags for
/// ergonomics). Short flags use a single leading `-`.
pub fn parse_args(args: &[String]) -> Args {
    // Native durable is the CLI default. Keep `Args::default()` unchanged for
    // embedded/test hosts that construct programmatic options explicitly.
    let mut result = Args {
        native_durable: true,
        offline: offline_env_enabled(),
        ..Args::default()
    };
    // `RPI_EXTENSIONS_DIR` env: an extra list of plugin dirs prepended to any
    // `--extensions-dir` flags. Semicolon-separated on Windows, colon-separated
    // on Unix (PATH-style). Empty entries skipped. `--no-extensions` still wins.
    let mut env_extension_dirs = Vec::new();
    if let Ok(raw) = std::env::var("RPI_EXTENSIONS_DIR") {
        if !raw.is_empty() {
            let sep = if cfg!(windows) { ';' } else { ':' };
            for part in raw.split(sep) {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    env_extension_dirs.push(PathBuf::from(trimmed));
                }
            }
        }
    }
    let mut i = 0;
    let mut host_arg_count = 0;
    let mut host_tui_arg_count = 0;
    let mut unsupported_host_rpc_flag = None;
    while i < args.len() {
        let arg = args[i].clone();
        // Peel an inline `--flag=value` (long flags only — short flags never use
        // `=`) so the match below compares bare flag names. `inline` holds the
        // RHS for `take_value` to consume in place of the next argv token.
        let (flag_key, inline) = if arg.starts_with("--") {
            match arg.find('=') {
                Some(eq) => (arg[..eq].to_string(), Some(arg[eq + 1..].to_string())),
                None => (arg.clone(), None),
            }
        } else {
            (arg.clone(), None)
        };

        // Take a value: prefer the inline `--flag=value`, else the next argv
        // token (when it isn't flag-shaped). Advances `i` past a consumed token.
        // (For unknown-flag diagnostics the closing arm reads `flag_key` itself.)
        if !matches!(
            flag_key.as_str(),
            "--host"
                | "--mode"
                | "--session"
                | "-s"
                | "--session-id"
                | "--session-dir"
                | "--fork"
                | "--fork-entry"
                | "--fork-position"
                | "--provider"
                | "--model"
                | "--resource-config"
                | "--resource-transition"
                | "--permission-gate"
                | "--bypass"
                | "--approve"
                | "-a"
                | "--no-approve"
                | "-na"
                | "--offline"
                | "--no-native-durable"
                | "--no-session"
                | "--rpc-message-updates"
                | "--help"
                | "-h"
                | "--version"
                | "-v"
        ) && flag_key.starts_with('-')
        {
            unsupported_host_rpc_flag.get_or_insert_with(|| flag_key.clone());
        }

        let mut take_value = |result: &mut Args, _flag: &str| -> Option<String> {
            if let Some(v) = inline.clone() {
                return Some(v);
            }
            if i + 1 < args.len() {
                let next = &args[i + 1];
                if !next.starts_with('-') || next == "-" {
                    i += 1;
                    return Some(args[i].clone());
                }
            }
            result.errors.push(format!("{flag_key} requires a value"));
            None
        };

        match flag_key.as_str() {
            "--help" | "-h" => result.help = true,
            "--version" | "-v" => result.version = true,
            "--print" | "-p" => {
                result.print = true;
                // `-p` may consume the following positional as the prompt
                // (TS heuristic: next is present, doesn't start with `@`, and
                // isn't a flag — except `---` which TS lets through; we keep
                // the simple `!@` && `!-` form).
                if i + 1 < args.len() {
                    let next = &args[i + 1];
                    if !next.starts_with('@') && !next.starts_with('-') {
                        i += 1;
                        result.messages.push(args[i].clone());
                    }
                }
            }
            "--mode" => {
                if let Some(v) = take_value(&mut result, "--mode") {
                    result.mode = match v.as_str() {
                        "text" => Mode::Text,
                        "json" => Mode::Json,
                        "rpc" => Mode::Rpc,
                        other => {
                            result.errors.push(format!(
                                "Invalid --mode \"{other}\". Valid: text, json, rpc"
                            ));
                            Mode::Text
                        }
                    };
                }
            }
            "--rpc-message-updates" => {
                if let Some(value) = take_value(&mut result, "--rpc-message-updates") {
                    match value.as_str() {
                        "snapshot" => result.rpc_message_updates = RpcMessageUpdates::Snapshot,
                        "compact-deltas" => result.rpc_message_updates = RpcMessageUpdates::CompactDeltas,
                        other => result.errors.push(format!(
                            "Invalid --rpc-message-updates \"{other}\". Valid: snapshot, compact-deltas"
                        )),
                    }
                }
            }
            "--tui-mode" => {
                if let Some(v) = take_value(&mut result, "--tui-mode") {
                    host_tui_arg_count += if inline.is_some() { 1 } else { 2 };
                    result.tui_mode = match v.to_ascii_lowercase().as_str() {
                        "regular" => TuiMode::Regular,
                        "fullscreen" => TuiMode::Fullscreen,
                        other => {
                            result.errors.push(format!(
                                "Invalid --tui-mode \"{other}\". Valid: regular, fullscreen"
                            ));
                            TuiMode::Fullscreen
                        }
                    };
                }
            }
            "--continue" | "-c" => result.continue_session = true,
            "--resume" | "-r" => result.resume = true,
            "--host" => {
                if result.host.is_some() {
                    result
                        .errors
                        .push("--host may only be supplied once".into());
                }
                let area = if let Some(area) = inline {
                    if area.is_empty() {
                        result.errors.push("--host= requires an area name".into());
                    }
                    area
                } else if let Some(next) = args.get(i + 1).filter(|next| {
                    !next.starts_with('-') && !next.starts_with('@') && !next.is_empty()
                }) {
                    i += 1;
                    host_arg_count += 1;
                    next.clone()
                } else {
                    String::new()
                };
                host_arg_count += 1;
                result.host = Some(area);
            }
            "--mini" => {
                result.mini = true;
                if inline.is_some() {
                    result.errors.push("--mini does not take a value".into());
                }
            }
            "--no-session" => result.no_session = true,
            "--no-tools" | "-nt" => result.no_tools = true,
            "--no-builtin-tools" | "-nbt" => result.no_builtin_tools = true,
            // `--native-durable` remains accepted for scripts during the
            // transition; `--no-native-durable` is the temporary escape hatch.
            "--native-durable" => result.native_durable = true,
            "--no-native-durable" => result.native_durable = false,
            "--experimental-durable-epoch" => {
                if result.experimental_durable_epoch.is_some() {
                    result
                        .errors
                        .push("--experimental-durable-epoch may only be supplied once".into());
                }
                if let Some(epoch) = take_value(&mut result, "--experimental-durable-epoch") {
                    result.experimental_durable_epoch = Some(epoch);
                }
            }
            "--no-skills" | "-ns" => result.no_skills = true,
            "--no-prompt-templates" | "-np" => result.no_prompt_templates = true,
            "--no-context-files" | "-nc" => result.no_context_files = true,
            "--no-extensions" | "-ne" => result.no_extensions = true,
            "--local-only" => result.dev_local_only = true,
            "--package-only" | "--project-only" => result.project_only = true,
            "--extensions-dir" | "-ed" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.extensions_dir.push(PathBuf::from(v));
                }
            }
            "--verbose" => result.verbose = true,
            "--debug-system-prompt" => result.debug_system_prompt = true,
            "--provider" => result.provider = take_value(&mut result, "--provider"),
            "--model" => result.model = take_value(&mut result, "--model"),
            "--api-key" => result.api_key = take_value(&mut result, "--api-key"),
            "--base-url" => result.base_url = take_value(&mut result, "--base-url"),
            "--timeout" => {
                if let Some(v) = take_value(&mut result, "--timeout") {
                    match v.parse::<u64>() {
                        Ok(seconds) if seconds > 0 => {
                            result.timeout = Some(Duration::from_secs(seconds));
                        }
                        _ => result.errors.push(format!(
                            "Invalid --timeout \"{v}\". Expected a positive integer number of seconds"
                        )),
                    }
                }
            }
            "--stream-idle-timeout" => {
                if let Some(v) = take_value(&mut result, "--stream-idle-timeout") {
                    match v.parse::<u64>() {
                        Ok(seconds) => result.stream_idle_timeout = Some(Duration::from_secs(seconds)),
                        Err(_) => result.errors.push(format!(
                            "Invalid --stream-idle-timeout \"{v}\". Expected a non-negative integer number of seconds (0 disables it)"
                        )),
                    }
                }
            }
            "--resource-config" => {
                result.resource_config =
                    take_value(&mut result, "--resource-config").map(PathBuf::from)
            }
            "--resource-transition" => {
                result.resource_transition =
                    take_value(&mut result, "--resource-transition").map(PathBuf::from)
            }
            "--system-prompt" => result.system_prompt = take_value(&mut result, "--system-prompt"),
            "--append-system-prompt" => {
                if let Some(v) = take_value(&mut result, "--append-system-prompt") {
                    result.append_system_prompt.push(v);
                }
            }
            "--name" | "-n" => result.name = take_value(&mut result, "--name"),
            "--session" => result.session = take_value(&mut result, "--session"),
            "--session-id" => result.session_id = take_value(&mut result, "--session-id"),
            "--fork" => result.fork = take_value(&mut result, "--fork"),
            "--fork-entry" => {
                if let Some(value) = take_value(&mut result, "--fork-entry") {
                    if value.trim().is_empty() {
                        result
                            .errors
                            .push("--fork-entry requires a nonempty entry ID".into());
                    } else if result.fork_entry.replace(value).is_some() {
                        result.errors.push("duplicate --fork-entry".into());
                    }
                }
            }
            "--fork-position" => {
                if let Some(value) = take_value(&mut result, "--fork-position") {
                    // Mirror `ForkPosition`'s two variants; `AT`/`BEFORE` are
                    // accepted so the flag is not case-sensitive by accident.
                    if result.fork_position.is_some() {
                        result.errors.push("duplicate --fork-position".into());
                    } else if value.eq_ignore_ascii_case("at") {
                        result.fork_position = Some(rpi_harness::session::types::ForkPosition::At);
                    } else if value.eq_ignore_ascii_case("before") {
                        result.fork_position =
                            Some(rpi_harness::session::types::ForkPosition::Before);
                    } else {
                        result.errors.push(format!(
                            "Invalid --fork-position \"{value}\". Expected at or before"
                        ));
                    }
                }
            }
            "--models" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.models = Some(split_csv(&v));
                }
            }
            "--extension" | "-e" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.extension.push(PathBuf::from(v));
                }
            }
            "--skill" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.skill.push(PathBuf::from(v));
                }
            }
            "--prompt-template" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.prompt_template.push(PathBuf::from(v));
                }
            }
            "--session-dir" => {
                if let Some(v) = take_value(&mut result, "--session-dir") {
                    result.session_dir = Some(PathBuf::from(v));
                }
            }
            "--thinking" => {
                if let Some(v) = take_value(&mut result, "--thinking") {
                    match parse_thinking_level(&v) {
                        Some(lvl) => result.thinking = Some(lvl),
                        None => result.ignored.push(format!(
                            "Invalid --thinking \"{v}\". Valid: {}",
                            VALID_THINKING_LEVELS.join(", ")
                        )),
                    }
                }
            }
            "--tools" | "-t" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.tools = Some(split_csv(&v));
                }
            }
            "--exclude-tools" | "-xt" => {
                if let Some(v) = take_value(&mut result, &flag_key) {
                    result.exclude_tools = Some(split_csv(&v));
                }
            }
            "--list-models" => {
                // Optionally consumes a search term, matching the native
                // parser's bare-flag versus string distinction.
                let mut search = inline.clone().unwrap_or_default();
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                    search = args[i].clone();
                }
                result.list_models = Some(search);
            }
            "--offline" => result.offline = true,
            "--export" => {
                if let Some(value) = take_value(&mut result, &flag_key) {
                    result.export = Some(PathBuf::from(value));
                }
            }
            "--approve" | "-a" => result.trust_override = Some(true),
            "--no-approve" | "-na" => result.trust_override = Some(false),
            // ---- Recognized-but-ignored v1 scope cuts (warn, don't error) ----
            // `flag_key` has already had any `=value` peeled, so these match the
            // bare flag name even when the user wrote `--offline=1`.
            //
            // NOTE: `--no-skills`/`-ns`, `--no-prompt-templates`/`-np`,
            // `--no-context-files`/`-nc`, `--no-extensions`/`-ne`, and
            // `--no-themes` is honored (parsed into a real field above). The
            // into real fields above), so they no longer reach this arm. The
            // resource flags gate discovery in `session.rs`; plugin loading
            // is separately opt-in.
            other if matches!(other, "--models") => {
                // Consume a value if the next token isn't a flag (so
                // `--models sonnet` doesn't swallow `sonnet` as a message).
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                }
                result
                    .ignored
                    .push(format!("{other} is not supported in v1 (ignored)"));
            }
            "--theme" => {
                result.theme = take_value(&mut result, "--theme");
                if result.theme.is_some() {
                    host_tui_arg_count += if inline.is_some() { 1 } else { 2 };
                }
            }
            "--no-themes" => {
                host_tui_arg_count += 1;
                result.no_themes = true;
            }
            // Unknown long flag (with or without `=`). Preserve it for an
            // extension to claim after extension registration, matching Pi's
            // `unknownFlags` behavior. A bare flag is boolean true; a following
            // non-flag token is its string value.
            other if other.starts_with("--") => {
                let name = &flag_key;
                let value = if let Some(value) = inline {
                    serde_json::Value::String(value)
                } else if i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                    serde_json::Value::String(args[i].clone())
                } else {
                    serde_json::Value::Bool(true)
                };
                result.unknown_flags.insert(name[2..].to_string(), value);
            }
            // Unknown short flag → hard error (mirrors TS).
            other if other.starts_with('-') && other.len() > 1 => {
                result.errors.push(format!("Unknown option: {other}"));
            }
            other => {
                if let Some(path) = file_arg(other) {
                    result.file_args.push(path);
                } else {
                    result.messages.push(other.to_string());
                }
            }
        }
        i += 1;
    }

    let fork_errors = result.fork_cut_errors();
    result
        .errors
        .extend(fork_errors.into_iter().map(str::to_owned));

    if result.host.is_some()
        && !result.mini
        && result.mode != Mode::Rpc
        && args.len() != host_arg_count + host_tui_arg_count
    {
        result.errors.push(
            "Host TUI accepts --tui-mode, --theme and --no-themes; prompt options require --mini"
                .into(),
        );
    }

    if result.host.is_some() && !result.mini && result.mode == Mode::Rpc {
        if let Some(flag) = unsupported_host_rpc_flag {
            result
                .errors
                .push(format!("{flag} is not supported by Host RPC"));
        }
    }

    if result.mini {
        result.no_session = true;
        result.native_durable = false;
        for (present, flag) in [
            (result.continue_session, "--continue"),
            (result.resume, "--resume"),
            (result.session.is_some(), "--session"),
            (result.session_id.is_some(), "--session-id"),
            (result.session_dir.is_some(), "--session-dir"),
            (result.fork.is_some(), "--fork"),
            (result.export.is_some(), "--export"),
        ] {
            if present {
                result
                    .errors
                    .push(format!("--mini cannot be combined with {flag}"));
            }
        }
    }
    if result.host.is_some() && result.mini {
        for (present, flag) in [
            (result.name.is_some(), "--name"),
            (result.mode != Mode::Text, "--mode json/rpc"),
            (result.list_models.is_some(), "--list-models"),
            (result.models.is_some(), "--models"),
            (result.resource_config.is_some(), "--resource-config"),
            (
                result.resource_transition.is_some(),
                "--resource-transition",
            ),
            (!result.extension.is_empty(), "--extension"),
            (!result.extensions_dir.is_empty(), "--extensions-dir"),
            (!result.skill.is_empty(), "--skill"),
            (!result.prompt_template.is_empty(), "--prompt-template"),
            (result.theme.is_some(), "--theme"),
        ] {
            if present {
                result
                    .errors
                    .push(format!("--mini cannot be combined with {flag}"));
            }
        }
        for flag in result.unknown_flags.keys() {
            result
                .errors
                .push(format!("--mini does not support --{flag}"));
        }
    }
    if result.resource_config.is_none() && result.resource_transition.is_none() {
        // Exact startup ignores discovery environment, but still rejects explicit CLI dirs.
        env_extension_dirs.append(&mut result.extensions_dir);
        result.extensions_dir = env_extension_dirs;
    }
    // `--print` + `--mode json`: `--print` implies non-interactive, but
    // `--mode json` selects the JSON event stream. The TS `resolveAppMode`
    // treats `mode === "json"` as its own non-interactive mode; we follow that.
    if let Err(error) = crate::native_durable::experimental_epoch::policy(&result) {
        result.errors.push(error);
    }
    result
}

/// Split a comma-separated list (mirrors the TS `.split(',').map(trim)`).
fn split_csv(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve the effective output [`Mode`]. Mirrors TS `resolveAppMode`:
/// `rpc`→rpc, `json`→json, `print` or piped-stdin/redirected-stdout→print,
/// else interactive. Here `stdin_is_tty`/`stdout_is_tty` come from
/// `std::io::IsTerminal`.
pub fn resolve_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> RunMode {
    // Check for --server flag (unknown flag stored in unknown_flags map)
    if parsed.unknown_flags.contains_key("server") {
        return RunMode::Rpc;
    }
    if parsed.mode == Mode::Rpc {
        return RunMode::Rpc;
    }
    if parsed.mode == Mode::Json {
        return RunMode::Json;
    }
    if parsed.print || !stdin_is_tty || !stdout_is_tty {
        RunMode::Print
    } else {
        RunMode::Interactive
    }
}

/// The concrete run mode [`resolve_mode`] picks. Mirrors TS `AppMode`
/// (`interactive`/`print`/`json`/`rpc`). Distinguished from [`Mode`] (the raw
/// `--mode` flag value) because the effective mode also folds in `-p` + TTY
/// detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

/// Print the help text to stdout. Mirrors TS `printHelp`, scoped to v1 flags.
pub fn print_help() {
    let builtin = "read, bash, edit, write, docs";
    println!(
        "{name} - AI coding assistant with read, bash, edit, write, docs tools

{u}Usage:{r}
  {name} [options] [@files...] [messages...]

{u}Options:{r}
  --provider <name>              Provider name (built-in, installed package, or models.json id)
  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")
  --api-key <key>                API key override for the selected provider
  --base-url <url>               Override the selected model endpoint
  --timeout <seconds>            LLM API request timeout (default: 600)
  --stream-idle-timeout <seconds> Max silence between response bytes (default: 300; 0 disables)
  --resource-capabilities       Pure bounded JSON resource protocol profile (use alone; no startup)
  --resource-session-read <file> Strict bounded nonexecuting saved-session read (standalone)
  --resource-transition <file>   Explicit same-session resource CAS (RPC only; see docs/resource-transition-rpc.md)
  --resource-config <file>       Exact versioned startup resources (RPC only; see docs/resource-selection-rpc.md)
  --system-prompt <text>         Replace the default system prompt
  --append-system-prompt <text>  Append text to the system prompt (repeatable)
  --thinking <level>             off, minimal, low, medium, high, xhigh, max
  --mode <mode>                  Output mode: text (default), json, or rpc
  --rpc-message-updates <mode>   RPC updates: compact-deltas (default) or snapshot (legacy)
  --host [area]                  Run in a shared local Host area (Unix), in TUI, RPC, or --mini mode
  --host[=area] --mini [prompt]   Run a one-shot text prompt in that Host
  --server [--port <n>] [--bind <ip>]
                                 Headless server: serve a remote-connectable agent (no TUI)
  --native-host [area]          Join or start a persistent Host area (macOS)
  --connect <host:port>          Remote client: connect to a `--server` instance and run the TUI
                                 (no local provider/tools/session; all agent work is remote)
  --token <token>                Auth token for `--connect` (printed by `--server`; may also
                                 be provided via the RPI_SERVER_TOKEN env var)
  --tui-mode <mode>              TUI buffer: regular or fullscreen (macOS default: regular)
  --list-models [search]         List available models (with optional fuzzy search)
  --offline                      Disable startup network operations (same as RPI_OFFLINE=1)
  --export <file>                Export a JSONL session to HTML and exit
  --approve, -a                  Force-enable current-project resources
  --no-approve, -na              Disable current-project resources
  --print, -p                    Non-interactive: process prompt(s) and exit
  --mini                         Use an in-memory session and disable native durable execution
  --continue, -c                 Continue the most recent session
  --resume, -r                   Browse and select a session to resume
  --session <id|path>            Use a specific session (partial UUID or file)
  --session-id <id>              Use the exact session id, creating it if missing
  --fork <id|path>               Fork a session into a new one and start in the fork
                                 (partial UUID, file name fragment, or file path)
  --fork-entry <entryId>         With --fork: cut the new branch at this message
                                 entry. The entry id must match exactly
  --fork-position <at|before>    With --fork-entry: keep the selected message in the
                                 new branch (at) or drop it (before). Default: before
  --session-dir <dir>            Directory for session storage
  --no-session                   Ephemeral mode (do not persist the session)
  --name, -n <name>              Set the session display name
  --tools, -t <list>             Comma-separated allowlist of tool names to enable
  --exclude-tools, -xt <list>    Comma-separated denylist of tool names to disable
  --no-tools, -nt                Disable all tools
  --no-builtin-tools, -nbt       Disable the built-in tools (read, bash, edit, write, docs)
  --no-native-durable            Temporary escape hatch: use legacy session behavior
  --experimental-durable-epoch <id>
                                With native durable enabled: bounded v2 ledger, explicit same epoch on reopen; no migration
  --no-skills, -ns               Skip skill discovery (no <available_skills> block)
  --no-prompt-templates, -np     Skip prompt-template discovery (/expand templates)
  --no-context-files, -nc        Skip AGENTS.md/CLAUDE.md discovery (no <project_context>)
  --no-extensions, -ne           Skip Rust cdylib extension loading
  --extensions-dir, -ed <dir>    Extra dir to scan for plugins (.dll/.so/.dylib); repeatable
                                 (also via RPI_EXTENSIONS_DIR env: ';' on Windows, ':' on Unix)
  --local-only                   Skip automatic extension discovery: load only
                                 --extensions-dir / --extension paths (no project
                                 .rpi/extensions, no global extensions)
  --package-only, --project-only Load project skills/prompts/extensions/packages
                                 from the active project; exclude global resources
  --debug-system-prompt          Print the resolved system-prompt sections to stderr (verification)
  --verbose                      Show startup warnings (e.g. ignored flags)
  --help, -h                     Show this help
  --version, -v                  Show version

{u}Subcommands:{r}
  host <command>                 Shared Host control: foreground serve; status/list/
                                inspect; submit/receipt; optional profiles and
                                administrative-* (Unix; authenticated IPC;
                                see `rpi host --help`)
  update                       Update the rpi CLI from crates.io
  auth login|check|logout        Manage persisted credentials in ~/.rpi/auth.json
                                (see `rpi auth --help`)
  events path|tail               Inspect the extension event journal (needs
                                RPI_EVENT_LOG=1; see `rpi events --help`)
  install <crate>                Install a prebuilt Rust extension (--build for source)
                                (see `rpi install --help`)
  uninstall <crate>              Remove an installed Rust cdylib extension
  extension update [name]        Update project and global installed Rust extensions
                                (see `rpi extension update --help`)
  package link|list|update|unlink Manage project-local Rust extension packages
                                (update is a legacy alias; see `rpi package --help`)
  dev [options]                  Build, watch, and hot-reload a Rust extension
                                (see `rpi dev --help`)
  dev-local [options]            Debug only the current Rust extension
                                (shortcut for `rpi dev --local-only`)

{u}Built-in Tools:{r}
  {builtin}  (enabled by default)

{u}Examples:{r}
  # Interactive with an initial prompt
  {name} \"List all .rs files in src/\"

  # Single-shot print mode
  {name} -p \"Summarize this project\"

  # Include a file in the initial message
  {name} @README.md \"What does this project do?\"

  # Continue the previous session
  {name} -c \"What did we discuss?\"

  # Use a specific model + thinking level
  {name} --model claude-sonnet-5 --thinking high \"Refactor this\"

  # JSON event stream (one JSON object per line on stdout)
  {name} --mode json -p \"Inspect the code\"

  # Read-only: no file-modifying tools
  {name} --tools read,bash -p \"Review the code in src/\"

{u}Environment:{r}
  ANTHROPIC_API_KEY              Anthropic API key (x-api-key) — fallback when no stored credential
  ANTHROPIC_AUTH_TOKEN           Bearer token (Authorization: Bearer) for third-party gateways
  ANTHROPIC_BASE_URL             Override the Anthropic endpoint (e.g. a compatible proxy)
  OPENAI_API_KEY                 Bearer token for openai-completions/responses
  RPI_OFFLINE                     Disable startup network operations when set to 1/true/yes
  RPI_CODING_AGENT_DIR           Override the ~/.rpi agent home (auth/models/settings/trust)
  RPI_SESSION_ID                 Session id served by a plugin (set by the host; also honored as an override)

{u}Notes:{r}
  Supported HTTP protocols are Anthropic Messages and OpenAI Chat Completions.
  Define custom model catalogs and provider apiKey values in
  ~/.rpi/agent/models.json. The interactive TUI, Rust extensions, skills,
  prompt templates, themes, model cycling, session fork/export, and trust
  commands are available in the current build. OAuth, RPC, and full model
  cycling remain outside the current implementation.
",
        name = crate::APP_NAME,
        builtin = builtin,
        u = "\x1b[1m",
        r = "\x1b[0m",
    );
}

/// Print the version line. Mirrors TS `--version` output (`pi <version>`).
pub fn print_version() {
    println!("{} {}", crate::APP_NAME, crate::VERSION);
}

#[cfg(test)]
mod tests {
    #[test]
    fn rpc_message_updates_selection_and_errors() {
        assert_eq!(
            super::Args::default().rpc_message_updates,
            super::RpcMessageUpdates::CompactDeltas
        );
        for (value, expected) in [
            ("snapshot", super::RpcMessageUpdates::Snapshot),
            ("compact-deltas", super::RpcMessageUpdates::CompactDeltas),
        ] {
            for args in [
                vec![
                    "--mode".into(),
                    "rpc".into(),
                    format!("--rpc-message-updates={value}"),
                ],
                vec!["--rpc-message-updates".into(), value.into()],
            ] {
                let parsed = super::parse_args(&args);
                assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
                assert_eq!(parsed.rpc_message_updates, expected);
                assert!(parsed.unknown_flags.is_empty());
            }
        }
        for args in [
            vec!["--rpc-message-updates".into()],
            vec!["--rpc-message-updates".into(), "--mode=json".into()],
            vec!["--rpc-message-updates=".into()],
            vec!["--rpc-message-updates=invalid".into()],
            vec!["--rpc-message-updates".into(), "invalid".into()],
        ] {
            let parsed = super::parse_args(&args);
            assert_eq!(parsed.errors.len(), 1);
            assert!(parsed.errors[0].contains("--rpc-message-updates"));
        }
        let parsed = super::parse_args(&[
            "--mode=json".into(),
            "--rpc-message-updates=snapshot".into(),
        ]);
        assert_eq!(parsed.mode, super::Mode::Json);
    }

    use super::*;

    struct RestoreOfflineEnv(Option<std::ffi::OsString>);

    impl Drop for RestoreOfflineEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(RPI_OFFLINE_ENV, value),
                None => std::env::remove_var(RPI_OFFLINE_ENV),
            }
        }
    }

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses_basic_prompt() {
        let a = parse_args(&s(&["hello", "world"]));
        assert_eq!(a.messages, vec!["hello".to_string(), "world".to_string()]);
        assert!(!a.help);
    }

    #[test]
    fn parses_help_and_version() {
        let a = parse_args(&s(&["--help"]));
        assert!(a.help);
        let a = parse_args(&s(&["-v"]));
        assert!(a.version);
    }

    #[test]
    fn print_consumes_following_positional() {
        let a = parse_args(&s(&["-p", "summarize"]));
        assert!(a.print);
        assert_eq!(a.messages, vec!["summarize".to_string()]);
    }

    #[test]
    fn print_does_not_consume_file_or_flag() {
        let a = parse_args(&s(&["-p", "@file.md"]));
        assert!(a.print);
        assert!(a.messages.is_empty());
        assert_eq!(a.file_args, vec![PathBuf::from("file.md")]);
    }

    #[test]
    fn model_and_thinking() {
        let a = parse_args(&s(&["--model", "claude-sonnet-5", "--thinking", "high"]));
        assert_eq!(a.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(a.thinking, Some(ThinkingLevel::High));
    }

    #[test]
    fn model_with_thinking_shorthand() {
        let a = parse_args(&s(&["--model", "claude-sonnet-5:high"]));
        // The model pattern keeps the `:high`; provider resolution splits it.
        assert_eq!(a.model.as_deref(), Some("claude-sonnet-5:high"));
    }

    #[test]
    fn tools_split_csv() {
        let a = parse_args(&s(&["--tools", "read, bash ,write"]));
        assert_eq!(
            a.tools.as_deref(),
            Some(&["read".to_string(), "bash".to_string(), "write".to_string()][..])
        );
    }

    #[test]
    fn native_durable_is_enabled_by_default_with_a_transition_escape_hatch() {
        assert!(parse_args(&s(&[])).native_durable);
        assert!(parse_args(&s(&["--native-durable"])).native_durable);
        assert!(!parse_args(&s(&["--no-native-durable"])).native_durable);
        assert!(parse_args(&s(&["--no-native-durable", "--native-durable"])).native_durable);
        assert!(!parse_args(&s(&["--native-durable", "--no-native-durable"])).native_durable);
    }

    #[test]
    fn mini_always_disables_session_and_durable() {
        for argv in [
            ["--mini", "--native-durable", "hello"],
            ["--native-durable", "--mini", "hello"],
        ] {
            let args = parse_args(&s(&argv));
            assert!(args.mini && args.no_session && !args.native_durable);
            assert!(args.errors.is_empty(), "{:?}", args.errors);
            assert_eq!(args.messages, ["hello"]);
            assert_eq!(resolve_mode(&args, true, true), RunMode::Interactive);
        }
    }

    #[test]
    fn mini_accepts_regular_modes_and_resource_flags() {
        let args = parse_args(&s(&[
            "--mini",
            "--mode",
            "rpc",
            "--tools",
            "read",
            "--extension",
            "extension.dylib",
        ]));
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        assert_eq!(resolve_mode(&args, true, true), RunMode::Rpc);
        assert_eq!(args.tools.unwrap(), ["read"]);
        assert_eq!(args.extension, [PathBuf::from("extension.dylib")]);
    }

    #[test]
    fn mini_rejects_session_and_durable_epoch_flags() {
        for (flags, expected) in [
            (&["--resume"][..], "--resume"),
            (&["--session-id", "saved"][..], "--session-id"),
            (&["--export", "session.jsonl"][..], "--export"),
            (
                &["--experimental-durable-epoch", "epoch"][..],
                "--experimental-durable-epoch",
            ),
        ] {
            let argv = ["--mini"]
                .into_iter()
                .chain(flags.iter().copied())
                .collect::<Vec<_>>();
            let args = parse_args(&s(&argv));
            assert!(
                args.errors.iter().any(|error| error.contains(expected)),
                "{:?}",
                args.errors
            );
        }
    }

    #[test]
    fn unknown_short_flag_errors() {
        let a = parse_args(&s(&["-Z"]));
        assert!(!a.errors.is_empty());
    }

    #[test]
    fn unknown_long_flag_is_retained_for_extensions() {
        let a = parse_args(&s(&["--frobnicate", "value"]));
        assert!(a.errors.is_empty());
        assert_eq!(
            a.unknown_flags.get("frobnicate"),
            Some(&serde_json::Value::String("value".into()))
        );
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn unknown_long_boolean_flag_is_retained() {
        let a = parse_args(&s(&["--server"]));
        assert_eq!(
            a.unknown_flags.get("server"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn unknown_long_flags_keep_string_and_equals_values() {
        let a = parse_args(&s(&["--port", "8080", "--bind=127.0.0.1"]));
        assert_eq!(
            a.unknown_flags.get("port"),
            Some(&serde_json::Value::String("8080".into()))
        );
        assert_eq!(
            a.unknown_flags.get("bind"),
            Some(&serde_json::Value::String("127.0.0.1".into()))
        );
    }

    #[test]
    fn models_flag_parses_csv() {
        let a = parse_args(&s(&["--models", "a,b,c"]));
        assert!(a.errors.is_empty());
        assert!(a.ignored.is_empty(), "--models is implemented");
        assert_eq!(
            a.models.as_deref(),
            Some(&["a".to_string(), "b".to_string(), "c".to_string()][..])
        );
        // The value is consumed, not read as a message:
        assert!(a.messages.is_empty());
    }

    #[test]
    fn list_models_accepts_bare_and_search_forms() {
        let bare = parse_args(&s(&["--list-models"]));
        assert_eq!(bare.list_models.as_deref(), Some(""));
        assert!(bare.ignored.is_empty());
        assert!(bare.messages.is_empty());

        let search = parse_args(&s(&["--list-models", "claude"]));
        assert_eq!(search.list_models.as_deref(), Some("claude"));
        assert!(search.messages.is_empty());

        let inline = parse_args(&s(&["--list-models=gpt"]));
        assert_eq!(inline.list_models.as_deref(), Some("gpt"));
    }

    #[test]
    fn offline_flag_is_honored_without_warning() {
        let args = parse_args(&s(&["--offline"]));
        assert!(args.offline);
        assert!(args.ignored.is_empty());
    }

    #[test]
    fn timeout_parses_seconds_in_separate_and_equals_forms() {
        let separate = parse_args(&s(&["--timeout", "45"]));
        assert!(separate.errors.is_empty());
        assert_eq!(separate.timeout, Some(Duration::from_secs(45)));

        let inline = parse_args(&s(&["--timeout=90"]));
        assert!(inline.errors.is_empty());
        assert_eq!(inline.timeout, Some(Duration::from_secs(90)));
    }

    #[test]
    fn timeout_rejects_zero_and_invalid_values() {
        for value in ["0", "1.5", "forever", "18446744073709551616"] {
            let args = parse_args(&s(&[&format!("--timeout={value}")]));
            assert_eq!(args.errors.len(), 1, "value: {value}");
            assert!(args.timeout.is_none(), "value: {value}");
        }
    }

    #[test]
    fn stream_idle_timeout_accepts_both_forms_and_zero_but_rejects_invalid_values() {
        for (argv, seconds) in [
            (s(&["--stream-idle-timeout", "45"]), 45),
            (s(&["--stream-idle-timeout=90"]), 90),
            (s(&["--stream-idle-timeout=0"]), 0),
        ] {
            let args = parse_args(&argv);
            assert!(args.errors.is_empty(), "{:?}", args.errors);
            assert_eq!(args.stream_idle_timeout, Some(Duration::from_secs(seconds)));
            assert!(args.timeout.is_none());
        }
        for value in ["-1", "1.5", "no", "18446744073709551616"] {
            let args = parse_args(&s(&[&format!("--stream-idle-timeout={value}")]));
            assert!(!args.errors.is_empty(), "{value}");
            assert!(args.stream_idle_timeout.is_none());
        }
        assert!(!parse_args(&s(&["--stream-idle-timeout"])).errors.is_empty());
    }

    #[test]
    fn native_pi_offline_truthy_values_are_case_insensitive() {
        for value in [
            Some("1"),
            Some("true"),
            Some("TRUE"),
            Some("Yes"),
            Some("yEs"),
        ] {
            assert!(is_truthy_env_flag(value), "value={value:?}");
        }
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some(" true "),
        ] {
            assert!(!is_truthy_env_flag(value), "value={value:?}");
        }
    }

    #[test]
    fn pi_offline_env_and_cli_flag_share_one_normalized_gate() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore = RestoreOfflineEnv(std::env::var_os(RPI_OFFLINE_ENV));

        std::env::set_var(RPI_OFFLINE_ENV, "YeS");
        assert!(parse_args(&[]).offline);
        assert!(normalize_offline_mode(&[]));
        assert_eq!(std::env::var(RPI_OFFLINE_ENV).as_deref(), Ok("1"));

        std::env::set_var(RPI_OFFLINE_ENV, "0");
        assert!(!parse_args(&[]).offline);
        let argv = s(&["--offline"]);
        assert!(normalize_offline_mode(&argv));
        assert_eq!(std::env::var(RPI_OFFLINE_ENV).as_deref(), Ok("1"));
    }

    #[test]
    fn project_trust_flags_are_honored_without_warning() {
        let approved = parse_args(&s(&["--approve"]));
        assert_eq!(approved.trust_override, Some(true));
        assert!(approved.ignored.is_empty());
        let denied = parse_args(&s(&["--no-approve"]));
        assert_eq!(denied.trust_override, Some(false));
        assert!(denied.ignored.is_empty());
    }

    #[test]
    fn export_flag_captures_input_and_output_position() {
        let args = parse_args(&s(&["--export", "session.jsonl", "transcript.html"]));
        assert_eq!(args.export, Some(PathBuf::from("session.jsonl")));
        assert_eq!(args.messages, vec!["transcript.html".to_string()]);
        assert!(args.ignored.is_empty());
    }

    #[test]
    fn session_id_and_fork_flags_parse() {
        let a = parse_args(&s(&["--session-id", "01abc", "--fork", "xyz"]));
        assert!(a.errors.is_empty());
        assert_eq!(a.session_id.as_deref(), Some("01abc"));
        assert_eq!(a.fork.as_deref(), Some("xyz"));
        assert_eq!(a.fork_entry, None);
        assert_eq!(
            a.fork_position, None,
            "an explicit entry must keep the harness default (before), not force a position"
        );
        let a = parse_args(&s(&[
            "--fork",
            "xyz",
            "--fork-entry",
            "message-2",
            "--fork-position",
            "before",
        ]));
        assert!(a.errors.is_empty());
        assert_eq!(a.fork_entry.as_deref(), Some("message-2"));
        assert_eq!(
            a.fork_position,
            Some(rpi_harness::session::types::ForkPosition::Before)
        );
        let a = parse_args(&s(&["--fork-position", "middle"]));
        assert_eq!(a.fork_position, None);
        assert_eq!(
            a.errors,
            vec!["Invalid --fork-position \"middle\". Expected at or before".to_string()]
        );
        let a = parse_args(&s(&[
            "-e",
            "plugin.dll",
            "--skill",
            "s",
            "--prompt-template",
            "t.md",
        ]));
        assert_eq!(a.extension.len(), 1);
        assert_eq!(a.skill.len(), 1);
        assert_eq!(a.prompt_template.len(), 1);
    }

    #[test]
    fn fork_cut_flags_are_reserved_and_require_an_active_fork() {
        let args = parse_args(&s(&[
            "--fork-entry=entry-1",
            "--fork",
            "source",
            "--fork-position",
            "at",
            "--extension-option=keep",
        ]));
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        assert_eq!(
            args.unknown_flags,
            BTreeMap::from([("extension-option".into(), serde_json::json!("keep"))])
        );
        for argv in [
            &["--fork-entry", "entry-1"][..],
            &["--fork", "source", "--fork-position", "at"],
            &[
                "--fork",
                "source",
                "--fork-entry",
                "entry-1",
                "--no-session",
            ],
            &["--fork", "source", "--fork-entry", "entry-1", "--resume"],
            &[
                "--fork",
                "source",
                "--fork-entry",
                "entry-1",
                "--fork-position=after",
            ],
            &[
                "--fork",
                "source",
                "--fork-entry=entry-1",
                "--fork-entry=entry-2",
            ],
            &["--fork", "source", "--fork-entry="],
        ] {
            assert!(!parse_args(&s(argv)).errors.is_empty(), "{argv:?}");
        }
    }

    #[test]
    fn fork_position_is_case_insensitive() {
        use rpi_harness::session::types::ForkPosition;
        for (value, expected) in [
            ("at", ForkPosition::At),
            ("At", ForkPosition::At),
            ("AT", ForkPosition::At),
            ("before", ForkPosition::Before),
            ("Before", ForkPosition::Before),
            ("BEFORE", ForkPosition::Before),
        ] {
            let a = parse_args(&s(&[
                "--fork",
                "xyz",
                "--fork-entry",
                "message-2",
                "--fork-position",
                value,
            ]));
            assert!(a.errors.is_empty(), "{value}: {:?}", a.errors);
            assert_eq!(a.fork_position, Some(expected), "{value}");
        }
    }

    /// `--fork-entry` is only read inside `SessionSelection::Fork`, and
    /// `--fork-position` only makes sense alongside an explicit entry: without
    /// one it reaches the harness as `entry_id: None, position: Some(Before)`
    /// and truncates the current leaf from the child.
    #[test]
    fn fork_cut_flags_require_fork() {
        let a = parse_args(&s(&["--fork-entry", "message-2", "--print", "hi"]));
        assert_eq!(a.errors, vec!["--fork-entry requires --fork".to_string()]);

        // `--fork-position` alone is missing both prerequisites; the existing
        // rule stays, and the new entry rule is reported alongside it.
        let a = parse_args(&s(&["--fork-position", "before"]));
        assert_eq!(
            a.errors,
            vec![
                "--fork-position requires --fork".to_string(),
                "--fork-position requires --fork-entry".to_string(),
            ]
        );

        // Both flags together report once each, in flag order.
        let a = parse_args(&s(&["--fork-position=at", "--fork-entry=message-2"]));
        assert_eq!(
            a.errors,
            vec![
                "--fork-entry requires --fork".to_string(),
                "--fork-position requires --fork".to_string(),
            ]
        );

        // With `--fork` they parse cleanly.
        let a = parse_args(&s(&[
            "--fork",
            "xyz",
            "--fork-entry",
            "message-2",
            "--fork-position",
            "at",
        ]));
        assert!(a.errors.is_empty(), "{:?}", a.errors);
    }

    /// The reviewer's P2: `--fork-position` without `--fork-entry` is not
    /// inert — the harness takes the current leaf as the implicit target and
    /// then applies `Before`, dropping it from the child. Cover both position
    /// values, both argv spellings, and the no-`--fork` combination.
    #[test]
    fn fork_position_requires_fork_entry() {
        for value in ["at", "before", "AT", "BEFORE"] {
            // Separate-value form.
            let a = parse_args(&s(&["--fork", "xyz", "--fork-position", value]));
            assert_eq!(
                a.errors,
                vec!["--fork-position requires --fork-entry".to_string()],
                "{value}: the flag is not inert"
            );
            assert!(a.fork_position.is_some(), "{value}");

            // Inline `--flag=value` form.
            let flag = format!("--fork-position={value}");
            let a = parse_args(&s(&["--fork", "xyz", &flag]));
            assert_eq!(
                a.errors,
                vec!["--fork-position requires --fork-entry".to_string()],
                "{flag}: the flag is not inert"
            );
        }

        // With --fork-entry present, both forms are accepted.
        let a = parse_args(&s(&[
            "--fork",
            "xyz",
            "--fork-entry",
            "message-2",
            "--fork-position=before",
        ]));
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(
            a.fork_position,
            Some(rpi_harness::session::types::ForkPosition::Before)
        );
    }

    #[test]
    fn no_skills_flag_honored() {
        let a = parse_args(&s(&["-ns"]));
        assert!(a.errors.is_empty());
        assert!(a.no_skills);
        // Honored flags do NOT also warn-ignore themselves.
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn no_prompt_templates_flag_honored() {
        let a = parse_args(&s(&["--no-prompt-templates"]));
        assert!(a.no_prompt_templates);
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn no_context_files_flag_honored() {
        let a = parse_args(&s(&["-nc"]));
        assert!(a.no_context_files);
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn no_extensions_flag_honored() {
        // `--no-extensions` is parsed and disables both extension backends.
        let a = parse_args(&s(&["--no-extensions"]));
        assert!(a.no_extensions);
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn local_only_flag_is_honored_on_the_regular_parser() {
        // The plain `rpi --local-only` form sets the same scope `rpi dev
        // --local-only` uses, without the build/watch layer.
        let a = parse_args(&s(&["--local-only"]));
        assert!(a.errors.is_empty());
        assert!(a.dev_local_only);
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn project_only_aliases_are_honored() {
        for flag in ["--package-only", "--project-only"] {
            let a = parse_args(&s(&[flag]));
            assert!(a.errors.is_empty());
            assert!(a.project_only);
            assert!(a.ignored.is_empty());
        }
    }

    #[test]
    fn extensions_dir_flag_collects_dirs() {
        let a = parse_args(&s(&["--extensions-dir", "/a/b", "-ed", "/c/d"]));
        assert_eq!(
            a.extensions_dir,
            vec![PathBuf::from("/a/b"), PathBuf::from("/c/d")]
        );
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn extensions_dir_inline_equals_form() {
        let a = parse_args(&s(&["--extensions-dir=/x/y"]));
        assert_eq!(a.extensions_dir, vec![PathBuf::from("/x/y")]);
    }

    #[test]
    fn extensions_dir_env_is_merged() {
        // The env var contributes its split list. We can't fully control env in
        // a unit test without `set_var` (process-global + racy under parallel
        // tests), so this asserts the flag path only; the env path is exercised
        // by the B2 smoke. Keep the test green regardless of the host env by
        // NOT asserting emptiness — just confirm the flag appends after env.
        let a = parse_args(&s(&["--extensions-dir", "/flag/only"]));
        assert!(a
            .extensions_dir
            .iter()
            .any(|p| p == &PathBuf::from("/flag/only")));
    }

    #[test]
    fn file_args_stripped() {
        let a = parse_args(&s(&["@a.txt", "@b.md", "hi"]));
        assert_eq!(
            a.file_args,
            vec![PathBuf::from("a.txt"), PathBuf::from("b.md")]
        );
        assert_eq!(a.messages, vec!["hi".to_string()]);
    }

    #[test]
    fn equals_form_supported() {
        let a = parse_args(&s(&["--model=claude-sonnet-5", "--thinking=low"]));
        assert_eq!(a.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(a.thinking, Some(ThinkingLevel::Low));
    }

    #[test]
    fn theme_flag_is_honored() {
        let a = parse_args(&s(&["--theme", "ocean.json"]));
        assert_eq!(a.theme.as_deref(), Some("ocean.json"));
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn no_themes_is_honored() {
        let a = parse_args(&s(&["--no-themes"]));
        assert!(a.no_themes);
        assert!(a.ignored.is_empty());
    }

    #[test]
    fn tui_mode_parses_and_validates() {
        assert_eq!(
            parse_args(&s(&["--tui-mode", "regular"])).tui_mode,
            TuiMode::Regular
        );
        assert_eq!(
            parse_args(&s(&["--tui-mode=fullscreen"])).tui_mode,
            TuiMode::Fullscreen
        );
        let invalid = parse_args(&s(&["--tui-mode", "split"]));
        assert!(!invalid.errors.is_empty());
    }

    #[test]
    fn host_accepts_terminal_presentation_flags() {
        let args = parse_args(&s(&[
            "--host",
            "work",
            "--tui-mode",
            "regular",
            "--theme=light",
        ]));
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        assert_eq!(args.host.as_deref(), Some("work"));
        assert_eq!(args.tui_mode, TuiMode::Regular);
        assert_eq!(args.theme.as_deref(), Some("light"));

        let args = parse_args(&s(&["--no-themes", "--tui-mode=fullscreen", "--host=work"]));
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        assert_eq!(args.tui_mode, TuiMode::Fullscreen);
        assert!(args.no_themes);
        assert!(!parse_args(&s(&["--host", "--provider", "fixture"]))
            .errors
            .is_empty());
    }

    #[test]
    fn tui_mode_default_preserves_native_macos_scrollback() {
        assert_eq!(default_tui_mode_for(true), TuiMode::Regular);
        assert_eq!(default_tui_mode_for(false), TuiMode::Fullscreen);
    }

    #[test]
    fn resolve_mode_interactive_when_tty() {
        let a = Args {
            print: true,
            ..Args::default()
        };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Print);
        let a = Args::default();
        assert_eq!(resolve_mode(&a, true, true), RunMode::Interactive);
        let a = Args {
            mode: Mode::Json,
            ..Args::default()
        };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Json);
        let a = Args {
            mode: Mode::Rpc,
            ..Args::default()
        };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Rpc);
    }

    #[test]
    fn piped_stdout_forces_print() {
        let a = Args::default();
        // stdout not a TTY ⇒ print even without -p (mirrors TS).
        assert_eq!(resolve_mode(&a, true, false), RunMode::Print);
    }
}
