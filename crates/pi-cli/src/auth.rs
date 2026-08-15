//! `rpi auth` subcommand — persistent credential management. Mirrors the
//! Rust-relevant slice of the TS `packages/coding-agent/src/cli/auth-command.ts`
//! + `main.ts:runAuthCommand` + `core/auth-check.ts`.
//!
//! v1 ships three actions:
//!
//! - `rpi auth login`  — prompt (no echo, via `rpassword`) for an Anthropic API
//!   key and persist it to `~/.rpi/auth.json` (atomic write + 0o600 on Unix).
//!   Mirrors the upstream `/login` TUI's `type:"secret"` prompt + `modify`.
//! - `rpi auth check`  — local-only probe of `auth.json` + the `ANTHROPIC_*`
//!   env vars; reports `ready`/`not_ready` (no network call — the upstream
//!   `--no-refresh` equivalent). `--json` emits a structured result.
//! - `rpi auth logout` — drop the `anthropic` entry from `auth.json` (env vars
//!   are left untouched, matching upstream `/logout` semantics).
//!
//! # Not ported (deferred — see `docs/m6-cli-open-questions.md`)
//!
//! OAuth device-code login (Claude Pro/Max subscriptions), the full TUI
//! `--provider` picker, and `auth print-api-key`/`print-bearer-token`. Only
//! the `anthropic` provider id is handled.

use std::collections::BTreeMap;

use crate::config::{
    self, delete_credential, read_auth, upsert_credential, Credential, DEFAULT_PROVIDER_ID,
};
/// Exit code for a missing-credential `auth check` (mirrors upstream's
/// non-zero `auth check` when not ready).
const EXIT_NOT_READY: i32 = 1;
/// Exit code for an operational error (IO failure, bad args).
const EXIT_ERROR: i32 = 2;

/// `rpi auth <sub> [args]` entry. `args` is the slice *after* `auth` (i.e. the
/// subcommand + its flags). Returns the process exit code. Async to match the
/// `app::run` shape, though v1 does no async work here.
pub async fn run(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "login" => run_login(&args[1..]).await,
        "check" => run_check(&args[1..]).await,
        "logout" => run_logout(&args[1..]).await,
        "--help" | "-h" | "help" | "" => {
            print_auth_help();
            0
        }
        other => {
            eprintln!("error: unknown auth subcommand \"{other}\"");
            eprintln!();
            print_auth_help();
            EXIT_ERROR
        }
    }
}

/// `rpi auth login [--provider <id>]` — prompt for a key and persist it.
async fn run_login(args: &[String]) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    if provider != DEFAULT_PROVIDER_ID {
        eprintln!(
            "error: v1 only supports the \"{DEFAULT_PROVIDER_ID}\" provider for login (got \"{provider}\")"
        );
        return EXIT_ERROR;
    }
    eprint!("Enter Anthropic API key: ");
    let key = match rpassword::read_password() {
        Ok(k) => k,
        Err(e) => {
            eprintln!();
            eprintln!("error: could not read the key from the terminal: {e}");
            return EXIT_ERROR;
        }
    };
    eprintln!();
    let key = key.trim();
    if key.is_empty() {
        eprintln!("error: an empty key was entered; nothing saved.");
        return EXIT_ERROR;
    }
    let cred = Credential::ApiKey { key: Some(key.to_string()), env: None };
    if let Err(e) = upsert_credential(provider, cred) {
        eprintln!("error: could not save credentials: {e}");
        return EXIT_ERROR;
    }
    let path = match config::auth_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warn: credentials saved, but could not resolve the config path: {e}");
            return 0;
        }
    };
    println!("Credentials saved to {}", path.display());
    println!("Run `rpi auth check` to verify.");
    0
}

/// `rpi auth check [--provider <id>] [--json]` — local readiness probe.
async fn run_check(args: &[String]) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    let want_json = args.iter().any(|a| a == "--json");

    let source = detect_credential(provider);
    let ready = source.is_present();
    if want_json {
        let json = serde_json::json!({
            "ready": ready,
            "provider": provider,
            "source": source.as_json_str(),
        });
        println!("{json}");
    } else if ready {
        let display = source.as_display().unwrap_or_default();
        println!("ready ({display})");
    } else {
        println!("not_ready — no credentials found.");
        eprintln!();
        eprintln!(
            "Set up credentials with one of:\n  \
             - `rpi auth login`\n  \
             - export ANTHROPIC_API_KEY=<key>\n  \
             - export ANTHROPIC_AUTH_TOKEN=<bearer>\n  \
             - pass --api-key <key>"
        );
    }
    if ready {
        0
    } else {
        EXIT_NOT_READY
    }
}

/// `rpi auth logout [--provider <id>]` — drop the stored credential.
async fn run_logout(args: &[String]) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    match delete_credential(provider) {
        Ok(true) => {
            println!("Removed stored credentials for \"{provider}\".");
            0
        }
        Ok(false) => {
            println!("No stored credential for \"{provider}\" (nothing to do).");
            0
        }
        Err(e) => {
            eprintln!("error: could not remove credentials: {e}");
            EXIT_ERROR
        }
    }
}

/// Pull the `--provider <id>` value from a subcommand's args (defaults to
/// `None`). Mirrors the TS `--provider` handshake before it falls back to the
/// default.
fn parse_provider(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--provider" {
            if let Some(v) = iter.next() {
                return Some(v.as_str());
            }
        } else if let Some(rest) = a.strip_prefix("--provider=") {
            return Some(rest);
        }
    }
    None
}

/// Where a credential was found — used by `auth check` to report its source.
enum CredentialSource {
    StoredFile,
    EnvApiKey,
    EnvAuthTToken,
    CliFlagUnset, // placeholder so the enum stays exhaustive; not used as "present"
}

impl CredentialSource {
    fn is_present(&self) -> bool {
        !matches!(self, CredentialSource::CliFlagUnset)
    }
    fn as_json_str(&self) -> &'static str {
        match self {
            CredentialSource::StoredFile => "auth.json",
            CredentialSource::EnvApiKey => "ANTHROPIC_API_KEY",
            CredentialSource::EnvAuthTToken => "ANTHROPIC_AUTH_TOKEN",
            CredentialSource::CliFlagUnset => "none",
        }
    }
    fn as_display(&self) -> Option<&'static str> {
        match self {
            CredentialSource::StoredFile => Some("key in ~/.rpi/auth.json"),
            CredentialSource::EnvApiKey => Some("ANTHROPIC_API_KEY env var"),
            CredentialSource::EnvAuthTToken => Some("ANTHROPIC_AUTH_TOKEN env var"),
            CredentialSource::CliFlagUnset => None,
        }
    }
}

/// Detect the first credential source available for `provider` (mirrors the
/// `provider::resolve` precedence, minus the `--api-key` flag which lives at
/// the CLI layer; here we probe file + env only).
fn detect_credential(provider: &str) -> CredentialSource {
    if let Ok(store) = read_auth() {
        if matches!(store.get(provider), Some(Credential::ApiKey { key: Some(k), .. }) if !k.is_empty())
            || matches!(store.get(provider), Some(Credential::ApiKey { key: None, env: Some(_env), .. }))
        {
            return CredentialSource::StoredFile;
        }
    }
    if std::env::var(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return CredentialSource::EnvAuthTToken;
    }
    if std::env::var(crate::provider::ANTHROPIC_API_KEY_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return CredentialSource::EnvApiKey;
    }
    CredentialSource::CliFlagUnset
}

/// `rpi auth --help`. Mirrors the TS auth-command usage but scoped to v1.
fn print_auth_help() {
    println!(
        "Usage: {name} auth <subcommand> [options]

Manage persisted Anthropic credentials in ~/.rpi/auth.json.

Subcommands:
  login   Prompt for an API key and save it (input is not echoed).
  check   Report whether credentials are available (no network call).
  logout  Remove the stored credential.

Options:
  --provider <id>   Provider id (v1: anthropic; default: anthropic)
  --json            (check only) Emit a {{ready, provider, source}} JSON object

Environment:
  ANTHROPIC_API_KEY      Fallback API key (x-api-key) when no stored credential.
  ANTHROPIC_AUTH_TOKEN   Fallback bearer token (Authorization: Bearer).

Notes:
  v1 supports only the anthropic provider. OAuth (Claude Pro/Max) is deferred.
",
        name = crate::APP_NAME
    );
}

// Keep BTreeMap imported for future header-bearing credential variants without
// triggering an unused-import in the current v1 shape.
#[allow(dead_code)]
fn _keep_btreemap() -> Option<BTreeMap<String, String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        auth_path, upsert_credential, Credential, DEFAULT_PROVIDER_ID, test_support::env_lock,
    };

    /// Scope `RPI_CODING_AGENT_DIR` + the `ANTHROPIC_*` env vars to a temp dir
    /// for the duration of a test. Holds the shared env lock for its whole
    /// lifetime so it can't race with config/provider env-mutating tests.
    struct TempConfig {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_dir: Option<std::ffi::OsString>,
        prev_key: Option<std::ffi::OsString>,
        prev_tok: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev_dir = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let prev_key = std::env::var_os(crate::provider::ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
            std::env::remove_var(crate::provider::ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            Self {
                _guard: guard,
                _tmp: tmp,
                prev_dir,
                prev_key,
                prev_tok,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            restore(crate::config::CONFIG_DIR_ENV, self.prev_dir.take());
            restore(crate::provider::ANTHROPIC_API_KEY_ENV, self.prev_key.take());
            restore(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
        }
    }
    fn restore(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[tokio::test]
    async fn check_not_ready_with_no_credentials() {
        let _cfg = TempConfig::new();
        let code = run_check(&[]).await;
        assert_eq!(code, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn check_ready_with_stored_credential() {
        let _cfg = TempConfig::new();
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey { key: Some("sk-stored".into()), env: None },
        )
        .unwrap();
        let code = run_check(&[]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_ready_with_env_api_key() {
        let _cfg = TempConfig::new();
        std::env::set_var(crate::provider::ANTHROPIC_API_KEY_ENV, "sk-env");
        let code = run_check(&[]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_json_outputs_object() {
        let _cfg = TempConfig::new();
        // Capture stdout is awkward in unit tests; just assert the exit code +
        // that a stored cred flips `ready`.
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey { key: Some("sk-x".into()), env: None },
        )
        .unwrap();
        let code = run_check(&["--json".to_string()]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn logout_removes_stored_credential() {
        let _cfg = TempConfig::new();
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey { key: Some("sk".into()), env: None },
        )
        .unwrap();
        assert!(auth_path().unwrap().exists());
        let code = run_logout(&[]).await;
        assert_eq!(code, 0);
        // The entry should be gone → check is now not_ready.
        assert_eq!(run_check(&[]).await, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn logout_when_empty_is_noop() {
        let _cfg = TempConfig::new();
        let code = run_logout(&[]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn unknown_auth_subcommand_errors() {
        let code = run(&["bogus".to_string()]).await;
        assert_eq!(code, EXIT_ERROR);
    }

    #[tokio::test]
    async fn auth_help_exits_zero() {
        let code = run(&[]).await;
        assert_eq!(code, 0);
        let code = run(&["--help".to_string()]).await;
        assert_eq!(code, 0);
    }

    #[test]
    fn parse_provider_handles_both_forms() {
        assert_eq!(parse_provider(&[]), None);
        assert_eq!(
            parse_provider(&["--provider".to_string(), "anthropic".to_string()]),
            Some("anthropic")
        );
        assert_eq!(
            parse_provider(&["--provider=custom".to_string()]),
            Some("custom")
        );
        // A trailing flag with no value doesn't panic.
        assert_eq!(parse_provider(&["--provider".to_string()]), None);
    }
}
