//! CLI entry orchestrator. Mirrors the v1-relevant slice of the TS
//! `packages/coding-agent/src/main.ts` — the `main(args)` function that:
//!
//! 1. Parses argv ([`crate::args::parse_args`]).
//! 2. Handles `--help`/`--version` + parse errors + startup warnings.
//! 3. Reads piped stdin (non-TTY ⇒ treat as the initial prompt text — TS
//!    `readPipedStdin`).
//! 4. Expands `@file` attachments into an initial-message text block (TS
//!    `processFileArguments` + [`build_initial_message`] — the port of TS
//!    `buildInitialMessage`).
//! 5. Resolves the provider + model + thinking level ([`crate::provider::resolve`]).
//! 6. Builds the harness ([`crate::session::build`]).
//! 7. Resolves the effective run mode ([`crate::args::resolve_mode`]) and
//!    dispatches to [`crate::modes`] (`print`/`json`/`interactive`), mapping the
//!    outcome to an exit code.
//!
//! # v1 scope cuts vs TS `main.ts` (in `docs/m6-cli-open-questions.md`)
//!
//! The TS `main` is enormous: auth-command routing, package-manager commands,
//! HTTP proxy config, project-trust prompts, first-time setup, migrations,
//! settings managers, theme init, extension/resource discovery. **None of that
//! is ported** — v1 is a straight parse → resolve → build → run pipeline. The
//! `@file` expansion ports *only* the text-file branch (images are detected
//! but not attached to the prompt — the harness `prompt_text` accepts images,
//! but v1 does not yet wire an image processor; binary/non-UTF-8 files error).

use std::io::{IsTerminal, Read};
use std::path::Path;

use pi_ai::types::ImageContent;

use crate::args::{parse_args, print_help, print_version, resolve_mode, Args, RunMode};
use crate::provider::{resolve, ResolveError};
use crate::session::{build, BuildError};

/// The exit code for a usage/parse error. (TS `main.ts` uses `process.exit(1)`
/// for most error paths; v1 distinguishes usage errors with the conventional
/// `2` so scripts can tell "bad invocation" from "run failed".)
pub const EXIT_USAGE: i32 = 2;
/// The exit code for a runtime failure (model-resolution, harness-build, or
/// run failure). Mirrors TS `process.exitCode` set from `runPrintMode`.
pub const EXIT_RUNTIME: i32 = 1;

/// The v1 CLI entry point. Mirrors TS `export async function main(args)`.
///
/// Returns the process exit code (0 = success). The binary wrapper
/// ([`crate::bin`] / `src/bin/pi.rs`) calls this under a tokio runtime and
/// `std::process::exit`s with the returned code.
pub async fn run() -> i32 {
    // argv[0] is the program name; skip it (TS `main(args)` receives the same,
    // already sliced by the Node CLI entry).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse_args(&argv);

    // ---- --help / --version short-circuit (before any heavy work) ----
    if parsed.help {
        print_help();
        return 0;
    }
    if parsed.version {
        print_version();
        return 0;
    }

    // ---- Parse errors → help + usage exit ----
    if !parsed.errors.is_empty() {
        for err in &parsed.errors {
            eprintln!("error: {err}");
        }
        eprintln!();
        print_help();
        return EXIT_USAGE;
    }

    // ---- Startup warnings (ignored-but-recognized flags) ----
    if parsed.verbose {
        for warn in &parsed.ignored {
            eprintln!("warning: {warn}");
        }
    }

    // ---- cwd ----
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not determine the current directory: {e}");
            return EXIT_USAGE;
        }
    };

    // ---- stdin (TS readPipedStdin: non-TTY stdin becomes initial prompt text) ----
    let stdin_text = read_piped_stdin();

    // ---- @file attachments → text (TS processFileArguments, text branch only) ----
    let (file_text, _file_images) = match process_file_args(&parsed.file_args, &cwd) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("error: {msg}");
            return EXIT_USAGE;
        }
    };

    // ---- initial message + extra messages (TS buildInitialMessage) ----
    let file_text_opt = if file_text.is_empty() { None } else { Some(file_text.as_str()) };
    let (initial, extra) =
        build_initial_message(&parsed, stdin_text.as_deref(), file_text_opt);

    // ---- provider + model resolution ----
    let resolved = match resolve(
        parsed.provider.as_deref(),
        parsed.model.as_deref(),
        parsed.thinking,
        parsed.api_key.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            print_resolve_error(&e);
            return match e {
                ResolveError::NoApiKey { .. } => EXIT_USAGE,
                _ => EXIT_RUNTIME,
            };
        }
    };

    // ---- harness build ----
    let harness = match build(&resolved, &parsed, &cwd).await {
        Ok(h) => h,
        Err(e) => {
            print_build_error(&e);
            return EXIT_RUNTIME;
        }
    };

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

    match mode {
        RunMode::Print => crate::modes::print(&harness, &parsed, initial.clone(), &extra).await,
        RunMode::Json => crate::modes::json(&harness, &parsed, initial.clone(), &extra).await,
        RunMode::Interactive => {
            crate::modes::interactive(&harness, &parsed, initial.clone(), &extra).await
        }
        RunMode::Rpc => {
            // `--mode rpc` is parsed (so it doesn't hard-error) but not
            // implemented in v1 — the JSON-RPC session protocol the TS
            // `runRpcMode` drives is deferred.
            eprintln!("error: rpc mode is not implemented in v1 (use --mode text or --mode json)");
            EXIT_USAGE
        }
    }
}

/// Read piped stdin into a string. Mirrors TS `readPipedStdin`: returns `None`
/// when stdin is a TTY (interactive), else the trimmed stdin text (empty ⇒
/// `None`).
///
/// NOTE: if stdin is *not* a TTY but no bytes arrive (e.g. `pi < /dev/null`),
/// this returns `None` (empty), which is what TS does too (`data.trim() || undefined`).
fn read_piped_stdin() -> Option<String> {
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
/// v1 divergence: the TS image branch (detect mime → base64 → `ImageContent`)
/// is **not ported** — `pi-tools` ships an image *detector* but no CLI-facing
/// image processor, and the v1 `modes` do not forward images into
/// `prompt_text`. Recognized image extensions are reported as an error rather
/// than silently mis-parsed as text. See `docs/m6-cli-open-questions.md`.
///
/// Paths are resolved relative to `cwd` (the TS uses `resolve(readPath, cwd)`).
fn process_file_args(
    file_args: &[std::path::PathBuf],
    cwd: &Path,
) -> Result<(String, Vec<ImageContent>), String> {
    let mut text = String::new();
    for rel in file_args {
        let abs = if rel.is_absolute() {
            rel.clone()
        } else {
            cwd.join(rel)
        };
        if !abs.exists() {
            return Err(format!("file not found: {}", abs.display()));
        }
        // v1: refuse image files outright (no image-attachment path yet).
        if is_likely_image(&abs) {
            return Err(format!(
                "image attachments are not supported in v1: {}",
                abs.display()
            ));
        }
        match std::fs::read_to_string(&abs) {
            Ok(content) => {
                text.push_str(&format!(
                    "<file name=\"{}\">\n{}\n</file>\n",
                    abs.display(),
                    content
                ));
            }
            Err(e) => {
                return Err(format!(
                    "could not read file {}: {e}",
                    abs.display()
                ));
            }
        }
    }
    Ok((text, Vec::new()))
}

/// True if the path's extension looks like a raster image the TS path would
/// have base64-attached. Used to route `@file` away from the text branch.
fn is_likely_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
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
    let initial = if parts.is_empty() { None } else { Some(parts.join("")) };
    (initial, extra)
}

/// Print a model-resolution error with env-specific guidance. Mirrors the TS
/// auth-guidance / model-resolver error formatting (condensed to stderr lines).
fn print_resolve_error(e: &ResolveError) {
    match e {
        ResolveError::NoApiKey { env } => {
            eprintln!("error: {e}");
            eprintln!();
            eprintln!("Set the {env} environment variable, or pass --api-key <key>.");
        }
        _ => eprintln!("error: {e}"),
    }
}

/// Print a harness-build error with flag-specific guidance for restore requests.
fn print_build_error(e: &BuildError) {
    match e {
        BuildError::RestoreNotImplemented { requested: _, flag } => {
            eprintln!("error: {e}");
            eprintln!();
            eprintln!(
                "To start a fresh session instead, drop {flag} (and any --session argument)."
            );
        }
        _ => eprintln!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;

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
    fn is_likely_image_detects_extensions() {
        assert!(is_likely_image(Path::new("foo.png")));
        assert!(is_likely_image(Path::new("foo.JPG")));
        assert!(!is_likely_image(Path::new("foo.rs")));
        assert!(!is_likely_image(Path::new("foo")));
    }
}
