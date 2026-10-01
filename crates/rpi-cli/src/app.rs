//! CLI entry orchestrator. The `main(args)` function that:
//!
//! 1. Parses argv ([`crate::args::parse_args`]).
//! 2. Handles `--help`/`--version` + parse errors + startup warnings.
//! 3. Reads piped stdin (non-TTY ⇒ treat as the initial prompt text).
//! 4. Expands `@file` attachments into an initial-message text block
//!    ([`build_initial_message`]).
//! 5. Resolves the provider + model + thinking level ([`crate::provider::resolve`]).
//! 6. Builds the harness ([`crate::session::build`]).
//! 7. Resolves the effective run mode ([`crate::args::resolve_mode`]) and
//!    dispatches to [`crate::modes`] (`print`/`json`/`interactive`), mapping the
//!    outcome to an exit code.
//!
//! `@file` expansion covers text files; images are detected but not attached to
//! the prompt — the harness `prompt_text` accepts images, but this path does not
//! yet wire an image processor. Binary/non-UTF-8 files error.
//!
use std::io::{IsTerminal, Read};
use std::path::Path;

use rpi_ai::types::{ImageContent, ImageContentType};

use crate::args::{parse_args, print_help, print_version, resolve_mode, Args, RunMode};
use crate::provider::{resolve_for_cwd, ResolveError};
use crate::session::{build, BuildError};

/// The exit code for a usage/parse error. (TS `main.ts` uses `process.exit(1)`
/// for most error paths; v1 distinguishes usage errors with the conventional
/// `2` so scripts can tell "bad invocation" from "run failed".)
pub const EXIT_USAGE: i32 = 2;
/// The exit code for a runtime failure (model-resolution, harness-build, or
/// run failure). Mirrors TS `process.exitCode` set from `runPrintMode`.
pub const EXIT_RUNTIME: i32 = 1;
/// The exit code when an extension **vetoes** startup at a lifecycle event
/// (e.g. a `BeforeTuiStart` handler returns [`rpi_plugin_sdk::EVENT_HANDLER_ABORT`]).
/// Distinct from [`EXIT_RUNTIME`] so scripts can tell "an extension refused to
/// start" from "the run itself failed".
pub const EXIT_VETOED: i32 = 3;

/// Read a long-flag value from argv, supporting both `--flag <value>` and
/// `--flag=value`, without disturbing the normal parser. Used for the early
/// `--connect` interception.
fn take_flag_value(argv: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
        if arg == flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Resolve the `--connect` auth token: the `--token` flag wins, then the
/// `RPI_SERVER_TOKEN` environment variable (ignoring empty/whitespace values).
fn resolve_connect_token(argv: &[String]) -> Option<String> {
    take_flag_value(argv, "--token").or_else(|| {
        std::env::var("RPI_SERVER_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// The v1 CLI entry point. Mirrors TS `export async function main(args)`.
///
/// Returns the process exit code (0 = success). The binary wrapper
/// ([`crate::bin`] / `src/bin/rpi.rs`) calls this under a tokio runtime and
/// `std::process::exit`s with the returned code.
pub async fn run() -> i32 {
    // Startup profiling wrapper: RPI_TIMING=1 records namespace timings and
    // flushes them to stderr once the run completes, regardless of exit path.
    crate::timings::reset(crate::timings::TimingNamespace::Main);
    let code = run_inner().await;
    crate::timings::print_timings();
    code
}

async fn run_inner() -> i32 {
    // argv[0] is the program name; skip it (TS `main(args)` receives the same,
    // already sliced by the Node CLI entry).
    let mut argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.first().map(String::as_str) == Some("__rpi_dev_cleanup") {
        return crate::dev_extension::run_cleanup_helper(&argv[1..]);
    }

    // ---- `rpi --connect <addr>` — pure remote client ----
    // Intercepted before any provider / harness / session / extension work so
    // the client holds no local agent resources at all: it only connects to a
    // running `rpi --server` and renders the remote transcript.
    if let Some(addr) = take_flag_value(&argv, "--connect") {
        let token = resolve_connect_token(&argv);
        return crate::remote::tui::run(&addr, token.as_deref()).await;
    }

    // Normalize the CLI flag into RPI_OFFLINE so early-dispatch subcommands and
    // the regular parsed path all observe the same process-wide gate.
    crate::args::normalize_offline_mode(&argv);

    // `rpi dev` wraps the normal CLI: consume only development-specific
    // options, then pass every remaining argument through the regular parser.
    let dev_command = argv.first().map(String::as_str);
    let dev_options = if matches!(dev_command, Some("dev" | "dev-local")) {
        match crate::dev_extension::parse_args(&argv[1..]) {
            Ok(options) if options.help => {
                crate::dev_extension::print_help();
                return 0;
            }
            Ok(mut options) => {
                if dev_command == Some("dev-local") {
                    options.local_only = true;
                }
                argv = options.passthrough.clone();
                Some(options)
            }
            Err(error) => {
                eprintln!("error: {error}");
                crate::dev_extension::print_help();
                return EXIT_USAGE;
            }
        }
    } else {
        None
    };

    // ---- Top-level subcommand dispatch (before flag parsing) ----
    // These are top-level subcommands (mirrors TS `runAuthCommand` / the
    // `handlePackageCommand` / `handleConfigCommand` routing in `main.ts`);
    // dispatching them here avoids them being misparsed as prompts. The set is
    // fixed and each handler has a different shape (sync vs async, extra argv
    // inspection), so a `match` on the first argument — not a trait registry —
    // is the right level of abstraction (and matches the `match mode { .. }`
    // dispatch at the end of this function).
    match argv.first().map(String::as_str) {
        Some("auth") => return crate::auth::run(&argv[1..]).await,
        Some("events") => return crate::events::run(&argv[1..]).await,
        Some("update") => return crate::updates::run_self_update(&argv[1..]),
        Some("install") => return crate::install::run(&argv[1..]),
        Some("uninstall") => return crate::install::uninstall(&argv[1..]),
        // Subcommands that went away with the Pi compatibility layer. Without
        // an explicit arm these fall through to the prompt path, so
        // `rpi package update` silently starts a chat session whose first
        // message is "package update" — reported as a hang, because there is no
        // terminal to read a prompt from. Fail loudly instead.
        Some(
            removed @ ("package" | "pi-package" | "pi-update" | "install-pi" | "uninstall-pi"),
        ) => {
            eprintln!(
                "error: `rpi {removed}` is not available; the Pi package layer was removed.\n\
                 Use `rpi install <crate>` and `rpi uninstall <crate>` for Rust extensions,\n\
                 and `rpi update` to update the CLI."
            );
            return EXIT_USAGE;
        }
        // Never a working invocation; the same fall-through applies.
        Some("self-update") => {
            eprintln!("error: unknown command `self-update`; use `rpi update`");
            return EXIT_USAGE;
        }
        _ => {}
    }

    let mut parsed = parse_args(&argv);
    crate::timings::time("args parsed", crate::timings::TimingNamespace::Main);

    // ---- Parsed startup stages ----
    if let Some(code) = handle_parse_short_circuits(&parsed) {
        return code;
    }

    let cwd = match resolve_cwd() {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };

    // ---- Legacy-layout migration (flat ~/.rpi → ~/.rpi/agent/) ----
    // Best-effort; never blocks startup. Skipped when RPI_CODING_AGENT_DIR is
    // set (an explicit override is its own layout).
    let _ = crate::config::migrate_legacy_layout();
    crate::timings::time("config migrated", crate::timings::TimingNamespace::Main);

    if let Some(code) = export_if_requested(&parsed) {
        return code;
    }

    // `--list-models` is intentionally handled before credentials, session
    // restoration, and harness construction. It is a catalog inspection
    // command, so it must work for a newly installed user who has not
    // authenticated yet.
    if let Some(search) = parsed.list_models.as_deref() {
        return list_models(search).await;
    }

    // Build before provider resolution so compiler errors do not require
    // valid model credentials. The staged directory joins normal discovery.
    let dev_extension = match prepare_dev_extension(dev_options.as_ref(), &cwd, &mut parsed) {
        Ok(extension) => extension,
        Err(code) => return code,
    };

    // `-r/--resume` is an interactive picker, unlike `-c/--continue` which
    // immediately opens the latest session. Resolve the picker result before
    // building the harness so cancelling does not create or modify a session.
    if let Some(code) = resolve_resume(&mut parsed, &cwd).await {
        return code;
    }

    emit_startup_warnings(&parsed);

    // ---- stdin (TS readPipedStdin: non-TTY stdin becomes initial prompt text) ----
    // Do NOT drain stdin when the run is an RPC/server session: stdin is the
    // JSONL command channel there (a spawned `rpi --mode rpc` child reads its
    // commands from stdin), so consuming it up-front would leave the protocol
    // loop nothing to read. `--mode rpc` and `--server` both select RunMode::Rpc.
    let wants_rpc_channel =
        parsed.mode == crate::args::Mode::Rpc || parsed.unknown_flags.contains_key("server");
    let stdin_text = if wants_rpc_channel {
        None
    } else {
        read_piped_stdin()
    };

    // ---- @file attachments → text (TS processFileArguments, text branch only) ----
    let (file_text, file_images) = match process_file_args(&parsed.file_args, &cwd) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("error: {msg}");
            return EXIT_USAGE;
        }
    };

    // ---- initial message + extra messages (TS buildInitialMessage) ----
    let file_text_opt = if file_text.is_empty() {
        None
    } else {
        Some(file_text.as_str())
    };
    let (initial, extra) = build_initial_message(&parsed, stdin_text.as_deref(), file_text_opt);

    // ---- provider + model resolution ----
    let project_trusted = crate::session::resolve_project_trust(&parsed, &cwd);
    let resolved = match resolve_for_cwd(
        parsed.provider.as_deref(),
        parsed.model.as_deref(),
        parsed.thinking,
        parsed.api_key.as_deref(),
        parsed.base_url.as_deref(),
        &cwd,
        project_trusted,
    ) {
        Ok(r) => r,
        Err(e) => {
            print_resolve_error(&e);
            return match e {
                ResolveError::NoApiKey { .. } | ResolveError::Config(_) => EXIT_USAGE,
                _ => EXIT_RUNTIME,
            };
        }
    };

    // The full authenticated catalog (read-only) for the TUI's `/model` selector.
    // v1 does not switch models mid-session, so this is display-only.
    let model_catalog = crate::provider::available_catalog(&resolved);

    // `--models <patterns>`: persist the Ctrl+P cycle scope to settings.json
    // (the same set `/scoped-models` edits). Each pattern matches catalog ids
    // case-insensitively; unmatched patterns are reported so a typo doesn't
    // silently empty the cycle.
    if let Some(patterns) = &parsed.models {
        let mut matched: Vec<String> = Vec::new();
        for p in patterns {
            let hits: Vec<String> = model_catalog
                .iter()
                .filter(|m| m.id.eq_ignore_ascii_case(p))
                .map(|m| m.id.clone())
                .collect();
            if hits.is_empty() {
                eprintln!("warning: --models pattern \"{p}\" matched no model");
            }
            matched.extend(hits);
        }
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.scoped_models = if matched.is_empty() {
            None
        } else {
            Some(matched)
        };
        if let Err(e) = crate::settings::save_settings(&settings) {
            eprintln!("warning: could not save --models scope: {e}");
        }
    }

    // ---- harness build ----
    let (harness, event_rx, mut reload_context) =
        match build(&resolved, &parsed, &cwd, project_trusted).await {
            Ok(triple) => triple,
            Err(e) => {
                print_build_error(&e);
                return EXIT_RUNTIME;
            }
        };
    reload_context.dev_extension = dev_extension;

    // ---- mode dispatch (TS resolveAppMode → runPrintMode / InteractiveMode / runRpcMode) ----
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let mode = resolve_mode(&parsed, stdin_is_tty, stdout_is_tty);

    // TS downgrades interactive → print when piped stdin is present.
    let mode = if matches!(mode, RunMode::Interactive) && stdin_text.is_some() {
        RunMode::Print
    } else {
        mode
    };

    // Debug/testing escape hatch: RPI_FORCE_TUI=1 forces interactive mode
    // (for testing the TUI in non-TTY environments).
    let mode = if std::env::var("RPI_FORCE_TUI")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        RunMode::Interactive
    } else {
        mode
    };

    let dev_cleanup = reload_context.dev_extension.clone();
    let dev_watcher = if matches!(mode, RunMode::Interactive) {
        reload_context
            .dev_extension
            .as_ref()
            .and_then(|extension| extension.start_watcher(reload_context.mailbox.clone()))
    } else {
        None
    };

    let exit_code = match mode {
        RunMode::Print => {
            crate::modes::print(
                &harness,
                &parsed,
                initial.clone(),
                &extra,
                file_images.clone(),
            )
            .await
        }
        RunMode::Json => {
            crate::modes::json(
                &harness,
                &parsed,
                initial.clone(),
                &extra,
                file_images.clone(),
                Some(event_rx),
            )
            .await
        }
        RunMode::Interactive => {
            crate::modes::interactive(
                &harness,
                Some(event_rx),
                &parsed,
                model_catalog,
                initial.clone(),
                &extra,
                file_images.clone(),
                if parsed.no_themes {
                    None
                } else {
                    parsed.theme.as_deref().or(resolved.theme.as_deref())
                },
                parsed.no_themes,
                &reload_context,
            )
            .await
        }
        RunMode::Rpc => {
            // Headless lifecycle (P0): no TUI, but a session still starts/ends —
            // extensions (e.g. rpi-server) auto-start on SessionStart and clean
            // up on SessionShutdown, exactly as in interactive mode. This is why
            // `rpi --server` works headless: the SessionStart hook fires and the
            // extension brings up the TCP server.
            let veto = crate::session::dispatch_session_event_async(
                &reload_context,
                rpi_plugin_sdk::EventTag::BeforeTuiStart,
            )
            .await;
            if let Some(reason) = veto {
                eprintln!("[rpi] startup vetoed by extension: {reason}");
                EXIT_VETOED
            } else {
                if let Some(reason) = crate::session::dispatch_session_event_async(
                    &reload_context,
                    rpi_plugin_sdk::EventTag::SessionStart,
                )
                .await
                {
                    tracing::warn!(
                        "[rpi] extension vetoed SessionStart (headless, advisory): {reason}"
                    );
                }

                let code = if parsed.unknown_flags.contains_key("server") {
                    // Server mode: the rpi-server extension owns the TCP server;
                    // the core just stays alive and tears the session down on
                    // Ctrl+C (firing SessionShutdown so the extension stops it).
                    eprintln!("[rpi] running in headless server mode (no TUI); Ctrl+C to stop");
                    let _ = tokio::signal::ctrl_c().await;
                    0
                } else {
                    // `--mode rpc`: JSONL command loop over stdio — the
                    // server-side agent that rpi-server spawns and that
                    // `rpi --connect` talks to.
                    crate::modes::rpc(&harness, Some(event_rx), model_catalog).await
                };

                if let Some(reason) = crate::session::dispatch_session_event_async(
                    &reload_context,
                    rpi_plugin_sdk::EventTag::SessionShutdown,
                )
                .await
                {
                    tracing::warn!(
                        "[rpi] extension vetoed SessionShutdown (ignored, closing): {reason}"
                    );
                }
                code
            }
        }
    };

    if let Some(dev) = &dev_cleanup {
        dev.stop_watcher();
    }
    if let Some(watcher) = dev_watcher {
        let _ = watcher.join();
    }
    drop(reload_context);
    drop(harness);
    if let Some(dev) = dev_cleanup {
        dev.cleanup();
    }
    exit_code
}

fn handle_parse_short_circuits(parsed: &Args) -> Option<i32> {
    // ---- --help / --version short-circuit (before any heavy work) ----
    if parsed.help {
        print_help();
        return Some(0);
    }
    if parsed.version {
        print_version();
        return Some(0);
    }

    // ---- Parse errors → help + usage exit ----
    if !parsed.errors.is_empty() {
        for err in &parsed.errors {
            eprintln!("error: {err}");
        }
        eprintln!();
        print_help();
        return Some(EXIT_USAGE);
    }

    None
}

fn resolve_cwd() -> Result<std::path::PathBuf, i32> {
    std::env::current_dir().map_err(|error| {
        eprintln!("error: could not determine the current directory: {error}");
        EXIT_USAGE
    })
}

fn export_if_requested(parsed: &Args) -> Option<i32> {
    let input = parsed.export.as_deref()?;
    let output = parsed
        .messages
        .first()
        .map(Path::new)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            let stem = input
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("session");
            Path::new(&format!("rpi-session-{stem}.html")).to_path_buf()
        });

    match crate::export::export_file(input, &output) {
        Ok(()) => {
            println!("Exported to: {}", output.display());
            Some(0)
        }
        Err(error) => {
            eprintln!("error: {error}");
            Some(EXIT_RUNTIME)
        }
    }
}

fn prepare_dev_extension(
    options: Option<&crate::dev_extension::DevOptions>,
    cwd: &Path,
    parsed: &mut Args,
) -> Result<Option<std::sync::Arc<crate::dev_extension::DevExtension>>, i32> {
    let Some(options) = options else {
        return Ok(None);
    };

    match crate::dev_extension::DevExtension::detect(cwd, options) {
        Ok(extension) => {
            if let Err(error) = extension.rebuild() {
                eprintln!("error: initial extension build failed: {error}");
                return Err(EXIT_RUNTIME);
            }
            if let Err(error) = extension.apply_to_args(parsed) {
                eprintln!("error: {error}");
                return Err(EXIT_RUNTIME);
            }
            Ok(Some(extension))
        }
        Err(error) => {
            // No Cargo cdylib found: degrade to skills-only mode.
            eprintln!("dev: {error}");
            eprintln!("dev: no Cargo cdylib found; running in skills-only mode");
            parsed.no_extensions = true;
            parsed.extensions_dir.clear();
            parsed.extension.clear();
            if options.local_only {
                parsed.dev_local_only = true;
            }
            Ok(None)
        }
    }
}

async fn resolve_resume(parsed: &mut Args, cwd: &Path) -> Option<i32> {
    if !parsed.resume {
        return None;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("error: --resume requires an interactive terminal");
        return Some(EXIT_USAGE);
    }

    match crate::resume_picker::select(cwd).await {
        Ok(Some(id)) => {
            parsed.resume = false;
            parsed.session = Some(id);
            None
        }
        Ok(None) => Some(0),
        Err(error) => {
            eprintln!("error: {error}");
            Some(EXIT_RUNTIME)
        }
    }
}

fn emit_startup_warnings(parsed: &Args) {
    // ---- Startup warnings (ignored-but-recognized flags) ----
    if parsed.verbose {
        for warn in &parsed.ignored {
            eprintln!("warning: {warn}");
        }
    }
    if parsed.no_themes && parsed.theme.is_some() {
        eprintln!("warning: --no-themes overrides --theme; using the built-in default theme");
    }
}

/// Print the merged model catalog, optionally filtered by a case-insensitive
/// fuzzy-ish substring over provider, id, and display name.
async fn list_models(search: &str) -> i32 {
    let catalog = match crate::provider::catalog_all() {
        Ok(models) => models,
        Err(error) => {
            eprintln!("warning: could not load models.json: {error}");
            Vec::new()
        }
    };
    let needle = search.trim().to_ascii_lowercase();
    let mut models: Vec<_> = catalog
        .into_iter()
        .filter(|model| {
            needle.is_empty()
                || format!("{} {} {}", model.provider, model.id, model.name)
                    .to_ascii_lowercase()
                    .contains(&needle)
        })
        .collect();
    if models.is_empty() {
        if needle.is_empty() {
            println!("{}", crate::auth_guidance::no_models_available_message());
        } else {
            println!("No models matching \"{search}\"");
        }
        return 0;
    }

    fn format_tokens(value: u64) -> String {
        if value >= 1_000_000 {
            let whole = value % 1_000_000 == 0;
            if whole {
                format!("{}M", value / 1_000_000)
            } else {
                format!("{:.1}M", value as f64 / 1_000_000.0)
            }
        } else if value >= 1_000 {
            let whole = value % 1_000 == 0;
            if whole {
                format!("{}K", value / 1_000)
            } else {
                format!("{:.1}K", value as f64 / 1_000.0)
            }
        } else {
            value.to_string()
        }
    }

    let rows: Vec<_> = models
        .drain(..)
        .map(|model| {
            let images = model
                .input
                .iter()
                .any(|input| matches!(input, rpi_ai::InputModality::Image));
            (
                model.provider,
                model.id,
                format_tokens(model.context_window),
                format_tokens(model.max_tokens),
                if model.reasoning { "yes" } else { "no" }.to_string(),
                if images { "yes" } else { "no" }.to_string(),
            )
        })
        .collect();
    let widths = (
        rows.iter().map(|r| r.0.len()).max().unwrap_or(8).max(8),
        rows.iter().map(|r| r.1.len()).max().unwrap_or(5).max(5),
        rows.iter().map(|r| r.2.len()).max().unwrap_or(7).max(7),
        rows.iter().map(|r| r.3.len()).max().unwrap_or(7).max(7),
        rows.iter().map(|r| r.4.len()).max().unwrap_or(8).max(8),
        rows.iter().map(|r| r.5.len()).max().unwrap_or(6).max(6),
    );
    println!(
        "{:provider$}  {:model$}  {:context$}  {:max_out$}  {:thinking$}  {:images$}",
        "provider",
        "model",
        "context",
        "max-out",
        "thinking",
        "images",
        provider = widths.0,
        model = widths.1,
        context = widths.2,
        max_out = widths.3,
        thinking = widths.4,
        images = widths.5,
    );
    for row in rows {
        println!(
            "{:provider$}  {:model$}  {:context$}  {:max_out$}  {:thinking$}  {:images$}",
            row.0,
            row.1,
            row.2,
            row.3,
            row.4,
            row.5,
            provider = widths.0,
            model = widths.1,
            context = widths.2,
            max_out = widths.3,
            thinking = widths.4,
            images = widths.5,
        );
    }
    0
}

/// Read piped stdin into a string. Mirrors TS `readPipedStdin`: returns `None`
/// when stdin is a TTY (interactive), else the trimmed stdin text (empty ⇒
/// `None`).
///
/// NOTE: if stdin is *not* a TTY but no bytes arrive (e.g. `pi < /dev/null`),
/// this returns `None` (empty), which is what TS does too (`data.trim() || undefined`).
fn read_piped_stdin() -> Option<String> {
    // Debug/testing escape hatch: RPI_SKIP_STDIN=1 skips reading piped stdin
    // (avoids blocking on non-TTY stdin in automated environments).
    if std::env::var("RPI_SKIP_STDIN")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return None;
    }
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buf = String::new();
    match std::io::stdin().read_to_string(&mut buf) {
        Ok(_) => {
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Expand `@file` attachments into prompt text. Mirrors the *text* branch of
/// TS `processFileArguments`: each readable text file is wrapped in
/// `<file name="...">\n<contents>\n</file>\n` and concatenated.
///
/// Paths are resolved relative to `cwd` (the TS uses `resolve(readPath, cwd)`).
fn process_file_args(
    file_args: &[std::path::PathBuf],
    cwd: &Path,
) -> Result<(String, Vec<ImageContent>), String> {
    let mut text = String::new();
    let mut images = Vec::new();
    for rel in file_args {
        let abs = if rel.is_absolute() {
            rel.clone()
        } else {
            cwd.join(rel)
        };
        if !abs.exists() {
            return Err(format!("file not found: {}", abs.display()));
        }
        let bytes = std::fs::read(&abs)
            .map_err(|e| format!("could not read file {}: {e}", abs.display()))?;
        if let Some(image) = image_content_from_bytes(&bytes) {
            images.push(image);
        } else {
            let content = String::from_utf8(bytes).map_err(|_| {
                format!(
                    "file is not valid UTF-8 text or a supported image: {}",
                    abs.display()
                )
            })?;
            text.push_str(&format!(
                "<file name=\"{}\">\n{}\n</file>\n",
                abs.display(),
                content
            ));
        }
    }
    Ok((text, images))
}

pub(crate) fn image_content_from_path(path: &Path) -> Result<Option<ImageContent>, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read file {}: {e}", path.display()))?;
    Ok(image_content_from_bytes(&bytes))
}

fn image_content_from_bytes(bytes: &[u8]) -> Option<ImageContent> {
    let mime_type = rpi_tools::detect_supported_image_mime_type(bytes)?;
    Some(ImageContent {
        kind: ImageContentType,
        data: rpi_tools::encode_base64(bytes),
        mime_type: mime_type.to_string(),
    })
}

/// Build the initial prompt + the remaining extra messages. Mirrors TS
/// `buildInitialMessage`: `[stdinContent, fileText, messages[0]].join("")` is
/// the initial message; `messages[1..]` are the follow-up prompts.
///
/// Returns `(initial: Option<String>, extra: Vec<String>)`.
fn build_initial_message(
    parsed: &Args,
    stdin: Option<&str>,
    file_text: Option<&str>,
) -> (Option<String>, Vec<String>) {
    let mut extra = parsed.messages.clone();
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = stdin {
        parts.push(s.to_string());
    }
    if let Some(t) = file_text {
        parts.push(t.to_string());
    }
    // Pull the first positional message into the initial prompt (TS `.shift()`).
    if !extra.is_empty() {
        parts.push(extra.remove(0));
    }
    let initial = if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    };
    (initial, extra)
}

/// Print a model-resolution error with env-specific guidance. Mirrors the TS
/// auth-guidance / model-resolver error formatting (condensed to stderr lines).
fn print_resolve_error(e: &ResolveError) {
    match e {
        ResolveError::NoApiKey { hint } => {
            eprintln!("error: {e}");
            eprintln!();
            eprintln!("Provide credentials via one of: {hint}.");
        }
        ResolveError::Config(_) => {
            eprintln!("error: {e}");
            eprintln!();
            eprintln!("Check ~/.rpi/auth.json / ~/.rpi/models.json (set RPI_CODING_AGENT_DIR to relocate).");
        }
        _ => eprintln!("error: {e}"),
    }
}

/// Print a harness-build error with flag-specific guidance for restore requests.
fn print_build_error(e: &BuildError) {
    match e {
        BuildError::SessionNotFound { .. } => {
            eprintln!("error: {e}");
            eprintln!();
            eprintln!("List saved sessions with the /session command in interactive mode.");
        }
        _ => eprintln!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;

    #[test]
    fn take_flag_value_supports_both_spellings() {
        let argv = vec![
            "--connect".to_string(),
            "127.0.0.1:9899".to_string(),
            "--token".to_string(),
            "abc".to_string(),
        ];
        assert_eq!(
            take_flag_value(&argv, "--connect").as_deref(),
            Some("127.0.0.1:9899")
        );
        assert_eq!(take_flag_value(&argv, "--token").as_deref(), Some("abc"));
        assert_eq!(take_flag_value(&argv, "--missing"), None);

        let inline = vec!["--connect=1.2.3.4:1".to_string()];
        assert_eq!(
            take_flag_value(&inline, "--connect").as_deref(),
            Some("1.2.3.4:1")
        );
    }

    #[test]
    fn connect_token_prefers_the_flag_over_the_env_var() {
        // The flag wins regardless of any RPI_SERVER_TOKEN in the environment.
        let argv = vec!["--token".to_string(), "flag-token".to_string()];
        assert_eq!(resolve_connect_token(&argv).as_deref(), Some("flag-token"));
    }

    #[test]
    fn build_initial_combines_stdin_file_and_first_message() {
        let mut args = Args::default();
        args.messages = vec!["first".into(), "second".into(), "third".into()];
        let (initial, extra) =
            build_initial_message(&args, Some("stdin-text"), Some("<file>...</file>"));
        assert_eq!(initial.as_deref(), Some("stdin-text<file>...</file>first"));
        assert_eq!(extra, vec!["second".to_string(), "third".to_string()]);
    }

    #[test]
    fn build_initial_with_no_messages_uses_stdin_and_file_only() {
        let args = Args::default();
        let (initial, extra) =
            build_initial_message(&args, Some("only-stdin"), Some("<file>x</file>"));
        assert_eq!(initial.as_deref(), Some("only-stdin<file>x</file>"));
        assert!(extra.is_empty());
    }

    #[test]
    fn build_initial_none_when_all_empty() {
        let args = Args::default();
        let (initial, extra) = build_initial_message(&args, None, None);
        assert!(initial.is_none());
        assert!(extra.is_empty());
    }

    #[test]
    fn build_initial_shifts_only_first_message() {
        let mut args = Args::default();
        args.messages = vec!["a".into(), "b".into()];
        let (initial, extra) = build_initial_message(&args, None, None);
        assert_eq!(initial.as_deref(), Some("a"));
        assert_eq!(extra, vec!["b".to_string()]);
    }

    #[test]
    fn process_file_args_attaches_supported_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0; 13]);
        std::fs::write(&path, png).unwrap();
        let (text, images) = process_file_args(&[path], dir.path()).unwrap();
        assert!(text.is_empty());
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert!(!images[0].data.is_empty());
    }

    #[test]
    fn image_content_from_path_reports_supported_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drop.png");
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0; 13]);
        std::fs::write(&path, png).unwrap();
        let image = image_content_from_path(&path).unwrap().unwrap();
        assert_eq!(image.mime_type, "image/png");
    }
}
