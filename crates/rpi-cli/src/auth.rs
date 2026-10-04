//! `rpi auth` subcommand — persistent credential management. Mirrors the
//! Rust-relevant slice of the TS `packages/coding-agent/src/cli/auth-command.ts`
//! + `main.ts:runAuthCommand` + `core/auth-check.ts`.
//!
//! v1 ships three actions:
//!
//! - `rpi auth login`  — legacy API-key prompt (no echo, via `rpassword`) for
//!   the default provider and persist it to `~/.rpi/auth.json`.
//!   Mirrors the upstream `/login` TUI's `type:"secret"` prompt + `modify`.
//! - `rpi auth check`  — local-only probe of `auth.json` + the `ANTHROPIC_*`
//!   env vars; reports `ready`/`not_ready` (no network call — the upstream
//!   `--no-refresh` equivalent). `--json` emits a structured result.
//! - `rpi auth logout` — drop the selected provider entry from `auth.json`
//!   (env vars are left untouched).
//!
//! # Not ported (deferred — see the initial port notes (retired))
//!
//! Provider-specific OAuth implementations belong in extensions. The host only
//! understands the standard OAuth action response and scoped credential format.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::Arc;

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
    run_with_extensions(args, None).await
}

/// Run auth with an optional extension snapshot. The caller must keep the
/// matching extension session alive until this future completes.
pub async fn run_with_extensions(
    args: &[String],
    snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "login" => run_login(&args[1..], snapshot.as_ref()).await,
        "check" | "status" => run_check(&args[1..]).await,
        "refresh" => run_refresh(&args[1..], snapshot.as_ref()).await,
        "logout" => run_logout(&args[1..], snapshot.as_ref()).await,
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
async fn run_login(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    if let Some(snapshot) = snapshot {
        if snapshot.oauth_provider(provider).is_some() {
            return run_oauth_login(provider, snapshot).await;
        }
    }
    // Compatibility API-key login. This path is provider-neutral; OAuth
    // behavior is supplied by the matching extension above.
    eprint!("Enter API key for {provider}: ");
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
    let cred = Credential::ApiKey {
        key: Some(key.to_string()),
        env: None,
    };
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
async fn run_refresh(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    let Some(snapshot) = snapshot else {
        eprintln!("error: no OAuth extensions are loaded");
        return EXIT_ERROR;
    };
    let Some(Credential::Oauth {
        access,
        refresh,
        expires,
    }) = config::read_oauth_token(provider).ok().flatten()
    else {
        eprintln!("error: no stored OAuth credentials for \"{provider}\"");
        return EXIT_NOT_READY;
    };
    let response = match rpi_extensions::request_oauth(
        snapshot,
        provider,
        "refresh",
        serde_json::json!({"credential": {"access": access, "refresh": refresh, "expires_at": expires}}),
    ) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: OAuth refresh failed: {error}");
            return EXIT_ERROR;
        }
    };
    let response = match rpi_extensions::OAuthActionResponse::parse(response) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("error: invalid OAuth refresh response: {error}");
            return EXIT_ERROR;
        }
    };
    let Some(token) = (match response.credential() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("error: invalid OAuth refresh credential: {error}");
            return EXIT_ERROR;
        }
    }) else {
        eprintln!("error: OAuth refresh returned no credential");
        return EXIT_ERROR;
    };
    if let Err(error) =
        config::upsert_oauth_token(provider, token.access, token.refresh, token.expires_at)
    {
        eprintln!("error: could not save refreshed OAuth credentials: {error}");
        return EXIT_ERROR;
    }
    println!("OAuth credentials refreshed for \"{provider}\".");
    0
}

async fn run_logout(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    if let Ok(Some(Credential::Oauth {
        access, refresh, ..
    })) = config::read_oauth_token(provider)
    {
        let Some(snapshot) = snapshot else {
            eprintln!("error: cannot revoke OAuth credentials: no OAuth extensions are loaded");
            return EXIT_ERROR;
        };
        let response = match rpi_extensions::request_oauth(
            snapshot,
            provider,
            "revoke",
            serde_json::json!({"credential": {"access": access, "refresh": refresh}}),
        ) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("error: OAuth revoke failed; local credentials were kept: {error}");
                return EXIT_ERROR;
            }
        };
        match rpi_extensions::OAuthActionResponse::parse(response) {
            Ok(response) if response.kind == rpi_extensions::OAuthResponseKind::Success => {}
            Ok(_) => {
                eprintln!(
                    "error: OAuth revoke did not return success; local credentials were kept"
                );
                return EXIT_ERROR;
            }
            Err(error) => {
                eprintln!(
                    "error: invalid OAuth revoke response; local credentials were kept: {error}"
                );
                return EXIT_ERROR;
            }
        }
        /*
         */
    }
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
async fn run_oauth_login(provider: &str, snapshot: &Arc<rpi_extensions::RegistrySnapshot>) -> i32 {
    let mut input = serde_json::json!({});
    let mut action = "begin";
    loop {
        let raw = match rpi_extensions::request_oauth(snapshot, provider, action, input) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("error: OAuth {action} failed: {error}");
                return EXIT_ERROR;
            }
        };
        let response = match rpi_extensions::OAuthActionResponse::parse(raw) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("error: invalid OAuth response: {error}");
                return EXIT_ERROR;
            }
        };
        match response.kind {
            rpi_extensions::OAuthResponseKind::Credential => {
                let Some(token) = response.credential().ok().flatten() else {
                    eprintln!("error: OAuth response contained no valid credential");
                    return EXIT_ERROR;
                };
                if let Err(error) = config::upsert_oauth_token(
                    provider,
                    token.access,
                    token.refresh,
                    token.expires_at,
                ) {
                    eprintln!("error: could not save OAuth credentials: {error}");
                    return EXIT_ERROR;
                }
                println!("OAuth credentials saved for \"{provider}\".");
                return 0;
            }
            rpi_extensions::OAuthResponseKind::Error => {
                eprintln!("error: OAuth provider rejected the request");
                return EXIT_ERROR;
            }
            rpi_extensions::OAuthResponseKind::Success => {
                eprintln!("error: OAuth login completed without credentials");
                return EXIT_ERROR;
            }
            rpi_extensions::OAuthResponseKind::Interaction => {
                let display = response.payload.get("display").unwrap_or(&response.payload);
                if let Some(title) = display.get("title").and_then(serde_json::Value::as_str) {
                    println!("{title}");
                }
                if let Some(url) = display.get("url").and_then(serde_json::Value::as_str) {
                    println!("{url}");
                }
                if let Some(instructions) = display
                    .get("instructions")
                    .and_then(serde_json::Value::as_array)
                {
                    for instruction in instructions.iter().filter_map(serde_json::Value::as_str) {
                        println!("{instruction}");
                    }
                }
                eprint!("OAuth input: ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                if io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                    eprintln!("error: empty OAuth input; nothing saved.");
                    return EXIT_ERROR;
                }
                input = serde_json::json!({"value": line.trim()});
                action = "exchange";
            }
        }
    }
}

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
    StoredApiKey,
    StoredOauth,
    ModelsJson,
    EnvApiKey,
    EnvAuthTToken,
    EnvOpenAiApiKey,
    CliFlagUnset, // placeholder so the enum stays exhaustive; not used as "present"
}

impl CredentialSource {
    fn is_present(&self) -> bool {
        !matches!(self, CredentialSource::CliFlagUnset)
    }
    fn as_json_str(&self) -> &'static str {
        match self {
            CredentialSource::StoredApiKey => "auth.json",
            CredentialSource::StoredOauth => "oauth",
            CredentialSource::ModelsJson => "models.json",
            CredentialSource::EnvApiKey => "ANTHROPIC_API_KEY",
            CredentialSource::EnvAuthTToken => "ANTHROPIC_AUTH_TOKEN",
            CredentialSource::EnvOpenAiApiKey => "OPENAI_API_KEY",
            CredentialSource::CliFlagUnset => "none",
        }
    }
    fn as_display(&self) -> Option<&'static str> {
        match self {
            CredentialSource::StoredApiKey => Some("key in ~/.rpi/auth.json"),
            CredentialSource::StoredOauth => Some("OAuth token in ~/.rpi/auth.json"),
            CredentialSource::ModelsJson => Some("apiKey in ~/.rpi/agent/models.json"),
            CredentialSource::EnvApiKey => Some("ANTHROPIC_API_KEY env var"),
            CredentialSource::EnvAuthTToken => Some("ANTHROPIC_AUTH_TOKEN env var"),
            CredentialSource::EnvOpenAiApiKey => Some("OPENAI_API_KEY env var"),
            CredentialSource::CliFlagUnset => None,
        }
    }
}

/// Detect the first credential source available for `provider` (mirrors the
/// `provider::resolve` precedence, minus the `--api-key` flag which lives at
/// the CLI layer; here we probe file + env only).
fn detect_credential(provider: &str) -> CredentialSource {
    if let Ok(store) = read_auth() {
        match store.get(provider) {
            Some(Credential::ApiKey { key: Some(k), .. }) if !k.is_empty() => {
                return CredentialSource::StoredApiKey;
            }
            Some(Credential::ApiKey {
                key: None,
                env: Some(_),
                ..
            }) => {
                return CredentialSource::StoredApiKey;
            }
            Some(Credential::Oauth {
                access, expires, ..
            }) if !access.is_empty()
                && (*expires == 0
                    || *expires
                        > std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|duration| duration.as_secs() as i64)
                            .unwrap_or(i64::MAX)) =>
            {
                return CredentialSource::StoredOauth;
            }
            _ => {}
        }
    }
    let configured = config::load_models_config().ok().and_then(|models| {
        models
            .providers
            .into_iter()
            .find(|(id, cfg)| {
                id.eq_ignore_ascii_case(provider)
                    || ((provider.eq_ignore_ascii_case("openai")
                        || provider.eq_ignore_ascii_case("openai-completions"))
                        && config::provider_is_openai_completions(cfg))
            })
            .map(|(_, cfg)| cfg)
    });
    if configured.as_ref().is_some_and(|cfg| {
        cfg.api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .and_then(|key| config::resolve_config_value(key, None))
            .is_some()
    }) {
        return CredentialSource::ModelsJson;
    }
    let is_openai = provider.eq_ignore_ascii_case("openai")
        || provider.eq_ignore_ascii_case("openai-completions")
        || configured
            .as_ref()
            .is_some_and(config::provider_is_openai_completions);
    if is_openai
        && std::env::var(crate::provider::OPENAI_API_KEY_ENV)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        return CredentialSource::EnvOpenAiApiKey;
    }
    if !is_openai
        && std::env::var(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        return CredentialSource::EnvAuthTToken;
    }
    if !is_openai
        && std::env::var(crate::provider::ANTHROPIC_API_KEY_ENV)
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

Manage persisted credentials and inspect models.json provider authentication.

Subcommands:
  login   Prompt for an API key, or run an OAuth provider login.
  check   Report whether credentials are available (no network call).
  status  Alias for check; never prints token material.
  refresh Refresh an OAuth token through its provider extension.
  logout  Revoke OAuth credentials, then remove the stored credential.

Options:
  --provider <id>   Provider id (default: anthropic)
  --json            (check only) Emit a {{ready, provider, source}} JSON object

Environment:
  ANTHROPIC_API_KEY      Fallback API key (x-api-key) when no stored credential.
  ANTHROPIC_AUTH_TOKEN   Fallback bearer token (Authorization: Bearer).
  OPENAI_API_KEY         Fallback bearer token for openai-completions.

Notes:
  `login --provider <id>` uses the registered OAuth extension when one exists;
  otherwise it is a provider-neutral API-key compatibility prompt. The host
  only handles standard interaction and credential responses for OAuth.
  `refresh` and `logout` use extension actions. API-key providers may still
  use ~/.rpi/agent/models.json or environment variables.
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
        auth_path, test_support::env_lock, upsert_credential, Credential, DEFAULT_PROVIDER_ID,
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
        prev_openai_key: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev_dir = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let prev_key = std::env::var_os(crate::provider::ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            let prev_openai_key = std::env::var_os(crate::provider::OPENAI_API_KEY_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
            std::env::remove_var(crate::provider::ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            std::env::remove_var(crate::provider::OPENAI_API_KEY_ENV);
            Self {
                _guard: guard,
                _tmp: tmp,
                prev_dir,
                prev_key,
                prev_tok,
                prev_openai_key,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            restore(crate::config::CONFIG_DIR_ENV, self.prev_dir.take());
            restore(crate::provider::ANTHROPIC_API_KEY_ENV, self.prev_key.take());
            restore(
                crate::provider::ANTHROPIC_AUTH_TOKEN_ENV,
                self.prev_tok.take(),
            );
            restore(
                crate::provider::OPENAI_API_KEY_ENV,
                self.prev_openai_key.take(),
            );
        }
    }
    fn restore(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn standard_oauth_credential_response_is_parsed() {
        let response = rpi_extensions::OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "credential",
            "payload": {
                "access": "access",
                "refresh": "refresh",
                "expires_at": 123
            }
        }))
        .unwrap();
        let credential = response.credential().unwrap().unwrap();
        assert_eq!(credential.access, "access");
        assert_eq!(credential.refresh, "refresh");
        assert_eq!(credential.expires_at, 123);
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
            Credential::ApiKey {
                key: Some("sk-stored".into()),
                env: None,
            },
        )
        .unwrap();
        let code = run_check(&[]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_ready_with_oauth_credential() {
        let _cfg = TempConfig::new();
        crate::config::upsert_oauth_token(
            DEFAULT_PROVIDER_ID,
            "access".into(),
            "refresh".into(),
            0,
        )
        .unwrap();
        assert_eq!(run_check(&[]).await, 0);
    }

    #[tokio::test]
    async fn check_rejects_expired_oauth_credential() {
        let _cfg = TempConfig::new();
        crate::config::upsert_oauth_token(
            DEFAULT_PROVIDER_ID,
            "access".into(),
            "refresh".into(),
            1,
        )
        .unwrap();
        assert_eq!(run_check(&[]).await, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn check_ready_with_env_api_key() {
        let _cfg = TempConfig::new();
        std::env::set_var(crate::provider::ANTHROPIC_API_KEY_ENV, "sk-env");
        let code = run_check(&[]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_ready_with_openai_models_json_key() {
        let _cfg = TempConfig::new();
        std::fs::write(
            crate::config::models_path().unwrap(),
            r#"{"providers":{"gateway":{"api":"openai-completions","apiKey":"key","models":[{"id":"gpt-test"}]}}}"#,
        )
        .unwrap();
        let code = run_check(&["--provider".to_string(), "gateway".to_string()]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_json_outputs_object() {
        let _cfg = TempConfig::new();
        // Capture stdout is awkward in unit tests; just assert the exit code +
        // that a stored cred flips `ready`.
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("sk-x".into()),
                env: None,
            },
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
            Credential::ApiKey {
                key: Some("sk".into()),
                env: None,
            },
        )
        .unwrap();
        assert!(auth_path().unwrap().exists());
        let code = run_logout(&[], None).await;
        assert_eq!(code, 0);
        // The entry should be gone → check is now not_ready.
        assert_eq!(run_check(&[]).await, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn logout_when_empty_is_noop() {
        let _cfg = TempConfig::new();
        let code = run_logout(&[], None).await;
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
