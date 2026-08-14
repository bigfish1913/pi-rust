//! CLI argument parsing + help text. Mirrors the TS
//! `packages/coding-agent/src/cli/args.ts` (`parseArgs` + `printHelp`), scoped
//! to the flags the v1 Rust CLI honors.
//!
//! The TS parser is a hand-rolled positional/flag loop (no `yargs`/`commander`
//! dep) that collects `messages`, `@file` attachments, known flags, and a map
//! of *unknown* `--flags` (for extensions to claim later). This port keeps the
//! same shape so the help text and flag semantics line up 1:1 with the
//! reference. Unknown flags are *not* stored (there is no extension system in
//! v1); they produce a warning diagnostic instead.
//!
//! Divergences from the TS parser (all deliberate v1 scope cuts, documented in
//! `docs/m6-cli-open-questions.md`):
//! - `--mode rpc`, `--tui-mode`, `--export`, `--list-models`, `--models`,
//!   `--fork`, `--offline`, `--approve`/`-na`, the package-manager subcommands,
//!   `--extension`/`-e`, `--skill`, `--prompt-template`, `--theme`, and their
//!   `--no-*` discovery toggles are **recognized but ignored** (parsed so users
//!   don't get a hard error for muscle-memory flags, with a warning). They are
//!   not in v1's surface.
//! - `--thinking` is typed via [`ThinkingLevel`] from `pi_ai` (the TS parser
//!   validates against the same string set).
//! - `--print`/`-p` may consume a following positional as its prompt (the TS
//!   parser's `next !== undefined && !startsWith('@')` heuristic) — preserved.

use std::path::PathBuf;

use pi_ai::ThinkingLevel;

/// Output mode. Mirrors TS `Mode = "text" | "json" | "rpc"`. `rpc` is parsed
/// (so `--mode rpc` doesn't error) but v1 does not implement it; `main`
/// reports an error if selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Text,
    Json,
    Rpc,
}

/// The parsed argument set. Mirrors TS `Args`. Fields absent in v1
/// (`unknownFlags`, extension/resource discovery) are omitted; everything here
/// is either honored or explicitly ignored-with-warning.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ThinkingLevel>,

    pub print: bool,
    pub mode: Mode,

    pub continue_session: bool,
    pub resume: bool,
    pub session: Option<String>,
    pub session_dir: Option<PathBuf>,
    pub no_session: bool,
    pub name: Option<String>,

    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,

    pub verbose: bool,
    pub help: bool,
    pub version: bool,

    /// Positional prompt text (one or more messages). Mirrors TS `messages`.
    pub messages: Vec<String>,
    /// `@file` attachments (prefix stripped), as raw paths for the caller to
    /// expand. Mirrors TS `fileArgs`.
    pub file_args: Vec<PathBuf>,

    /// Warnings about recognized-but-ignored flags (v1 scope cuts). Surfaced
    /// to the user on startup when `--verbose`.
    pub ignored: Vec<String>,
    /// Hard parse errors (unknown short flags, missing values). Non-empty ⇒
    /// `main` prints them + help and exits non-zero.
    pub errors: Vec<String>,
}

/// The canonical valid `--thinking` level strings, in level order. Mirrors TS
/// `VALID_THINKING_LEVELS`.
pub const VALID_THINKING_LEVELS: &[&str] =
    &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Parse a thinking-level string. Mirrors TS `isValidThinkingLevel`.
pub fn parse_thinking_level(s: &str) -> Option<ThinkingLevel> {
    Some(match s {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => return None,
    })
}

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
    let mut result = Args::default();
    let mut i = 0;
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
                            result
                                .errors
                                .push(format!("Invalid --mode \"{other}\". Valid: text, json, rpc"));
                            Mode::Text
                        }
                    };
                }
            }
            "--continue" | "-c" => result.continue_session = true,
            "--resume" | "-r" => result.resume = true,
            "--no-session" => result.no_session = true,
            "--no-tools" | "-nt" => result.no_tools = true,
            "--no-builtin-tools" | "-nbt" => result.no_builtin_tools = true,
            "--verbose" => result.verbose = true,
            "--provider" => result.provider = take_value(&mut result, "--provider"),
            "--model" => result.model = take_value(&mut result, "--model"),
            "--api-key" => result.api_key = take_value(&mut result, "--api-key"),
            "--system-prompt" => result.system_prompt = take_value(&mut result, "--system-prompt"),
            "--append-system-prompt" => {
                if let Some(v) = take_value(&mut result, "--append-system-prompt") {
                    result.append_system_prompt.push(v);
                }
            }
            "--name" | "-n" => result.name = take_value(&mut result, "--name"),
            "--session" => result.session = take_value(&mut result, "--session"),
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
            // ---- Recognized-but-ignored v1 scope cuts (warn, don't error) ----
            // `flag_key` has already had any `=value` peeled, so these match the
            // bare flag name even when the user wrote `--offline=1`.
            other
                if matches!(
                    other,
                    "--models"
                        | "--offline"
                        | "--export"
                        | "--tui-mode"
                        | "--approve" | "-a"
                        | "--no-approve" | "-na"
                        | "--no-extensions" | "-ne"
                        | "--no-skills" | "-ns"
                        | "--no-prompt-templates" | "-np"
                        | "--no-themes"
                        | "--no-context-files" | "-nc"
                ) =>
            {
                // Consume a value if the next token isn't a flag (so
                // `--models sonnet` doesn't swallow `sonnet` as a message).
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                }
                result.ignored.push(format!("{other} is not supported in v1 (ignored)"));
            }
            flag @ ("--extension" | "-e" | "--skill" | "--prompt-template" | "--theme") => {
                // These take a value (or an inline `=`); consume the next token
                // when there's no inline value so the path isn't read as a
                // message, then warn.
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                }
                result.ignored.push(format!("{flag} is not supported in v1 (ignored)"));
            }
            "--list-models" => {
                // Optionally consumes a search term.
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                }
                result.ignored.push("--list-models is not supported in v1 (ignored)".to_string());
            }
            "--fork" => {
                result.ignored.push("--fork is not supported in v1 (ignored)".to_string());
                if inline.is_none() && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                }
            }
            // Unknown long flag (with or without `=`). `flag_key` already holds
            // the bare name, so both `--frobnicate` and `--frobnicate=x` land
            // here; consume a value if the next token isn't a flag/file.
            other if other.starts_with("--") => {
                let name = &flag_key;
                if inline.is_none()
                    && i + 1 < args.len()
                    && !args[i + 1].starts_with('-')
                    && !args[i + 1].starts_with('@')
                {
                    i += 1;
                }
                result.ignored.push(format!("{name} is not a recognized flag (ignored)"));
            }
            // Unknown short flag → hard error (mirrors TS).
            other if other.starts_with('-') && other.len() > 1 => {
                result
                    .errors
                    .push(format!("Unknown option: {other}"));
            }
            // `@file` attachment.
            other if let Some(path) = file_arg(other) => {
                result.file_args.push(path);
            }
            // Bare positional → prompt message.
            other => {
                result.messages.push(other.to_string());
            }
        }
        i += 1;
    }

    // `--print` + `--mode json`: `--print` implies non-interactive, but
    // `--mode json` selects the JSON event stream. The TS `resolveAppMode`
    // treats `mode === "json"` as its own non-interactive mode; we follow that.
    result
}

/// Split a comma-separated list (mirrors the TS `.split(',').map(trim)`).
fn split_csv(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Resolve the effective output [`Mode`]. Mirrors TS `resolveAppMode`:
/// `rpc`→rpc, `json`→json, `print` or piped-stdin/redirected-stdout→print,
/// else interactive. Here `stdin_is_tty`/`stdout_is_tty` come from
/// `std::io::IsTerminal`.
pub fn resolve_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> RunMode {
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
    let builtin = "read, bash, edit, write, grep, find, ls";
    println!(
        "{name} - AI coding assistant with read, bash, edit, write, grep, find, ls tools

{u}Usage:{r}
  {name} [options] [@files...] [messages...]

{u}Options:{r}
  --provider <name>              Provider name (v1: anthropic)
  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")
  --api-key <key>                API key (defaults to ANTHROPIC_API_KEY)
  --system-prompt <text>         Replace the default system prompt
  --append-system-prompt <text>  Append text to the system prompt (repeatable)
  --thinking <level>             off, minimal, low, medium, high, xhigh, max
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive: process prompt(s) and exit
  --continue, -c                 Continue the most recent session
  --resume, -r                   Browse and select a session to resume
  --session <id|path>            Use a specific session (partial UUID or file)
  --session-dir <dir>            Directory for session storage
  --no-session                   Ephemeral mode (do not persist the session)
  --name, -n <name>              Set the session display name
  --tools, -t <list>             Comma-separated allowlist of tool names to enable
  --exclude-tools, -xt <list>    Comma-separated denylist of tool names to disable
  --no-tools, -nt                Disable all tools
  --no-builtin-tools, -nbt       Disable the built-in tools (read, bash, edit, write, grep, find, ls)
  --verbose                      Show startup warnings (e.g. ignored flags)
  --help, -h                     Show this help
  --version, -v                  Show version

{u}Built-in Tools:{r}
  {builtin}  (enabled by default; grep/find/ls are read-only)

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
  ANTHROPIC_API_KEY              Anthropic API key (required for real runs)

{u}Notes:{r}
  v1 is Anthropic-only (API key). TUI, extensions, skills, prompt templates,
  themes, model cycling, package manager, HTML export, --fork, --list-models,
  --export, and OAuth are recognized but not implemented yet.
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
    use super::*;

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
        assert_eq!(a.tools.as_deref(), Some(&["read".to_string(), "bash".to_string(), "write".to_string()][..]));
    }

    #[test]
    fn unknown_short_flag_errors() {
        let a = parse_args(&s(&["-Z"]));
        assert!(!a.errors.is_empty());
    }

    #[test]
    fn unknown_long_flag_warns_not_errors() {
        let a = parse_args(&s(&["--frobnicate", "value"]));
        assert!(a.errors.is_empty());
        assert!(!a.ignored.is_empty());
    }

    #[test]
    fn ignored_scope_cuts_warn() {
        let a = parse_args(&s(&["--models", "sonnet"]));
        assert!(a.errors.is_empty());
        assert!(!a.ignored.is_empty());
        // The value is consumed, not read as a message:
        assert!(a.messages.is_empty());
    }

    #[test]
    fn file_args_stripped() {
        let a = parse_args(&s(&["@a.txt", "@b.md", "hi"]));
        assert_eq!(a.file_args, vec![PathBuf::from("a.txt"), PathBuf::from("b.md")]);
        assert_eq!(a.messages, vec!["hi".to_string()]);
    }

    #[test]
    fn equals_form_supported() {
        let a = parse_args(&s(&["--model=claude-sonnet-5", "--thinking=low"]));
        assert_eq!(a.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(a.thinking, Some(ThinkingLevel::Low));
    }

    #[test]
    fn resolve_mode_interactive_when_tty() {
        let a = Args { print: true, ..Args::default() };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Print);
        let a = Args::default();
        assert_eq!(resolve_mode(&a, true, true), RunMode::Interactive);
        let a = Args { mode: Mode::Json, ..Args::default() };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Json);
        let a = Args { mode: Mode::Rpc, ..Args::default() };
        assert_eq!(resolve_mode(&a, true, true), RunMode::Rpc);
    }

    #[test]
    fn piped_stdout_forces_print() {
        let a = Args::default();
        // stdout not a TTY ⇒ print even without -p (mirrors TS).
        assert_eq!(resolve_mode(&a, true, false), RunMode::Print);
    }
}
