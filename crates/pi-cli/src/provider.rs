//! Provider + model resolution. Mirrors the *Anthropic-protocol* slice of the
//! TS `packages/coding-agent/src/core/model-resolver.ts` (`resolveCliModel` +
//! the `provider/id[:thinking]` parsing in [`crate::args`]).
//!
//! v1 is Anthropic-protocol only (plan §5.16: "OAuth/Copilot skipped v1;
//! API-key auth only" — now extended to include third-party Anthropic-compatible
//! endpoints via `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` and a
//! `~/.rpi/models.json` catalog; OAuth is still deferred). The TS
//! `ModelRuntime`/`ModelRegistry` multi-provider machinery is not ported; this
//! module builds a single [`AnthropicProvider`] from a resolved credential and
//! resolves a [`Model`] + [`ThinkingLevel`] against the catalog.
//!
//! # Auth resolution precedence (mirrors upstream `anthropic.ts:resolve`)
//!
//! 1. `--api-key` → provider default key (sent as `x-api-key`).
//! 2. `~/.rpi/auth.json` `anthropic.api_key.key` — the persistent `rpi auth
//!    login` credential (sent as `x-api-key`). This is the "logged-in" path.
//! 3. `~/.rpi/models.json` provider with `authHeader: true` + `apiKey` →
//!    `Authorization: Bearer <key>` (a static gateway credential — the models.json
//!    file alone is a complete third-party-endpoint setup, no env var needed).
//! 4. `ANTHROPIC_AUTH_TOKEN` env → `Authorization: Bearer <token>` (folded into
//!    each model's `headers`; the provider's `has_header_auth` recognizes it and
//!    skips `x-api-key`, so a token-only setup does not error on a missing key).
//! 5. `ANTHROPIC_API_KEY` env → provider default key (`x-api-key`).
//! 6. None of the above ⇒ [`ResolveError::NoApiKey`].
//!
//! When a Bearer source (item 3 or 4) wins, the provider is built with
//! `api_key = None` — the header on each model carries the auth. When a key
//! source wins (1, 2, or 5), the provider carries the key as `x-api-key`.
//!
//! # Endpoint + catalog
//!
//! - `--base-url` / `ANTHROPIC_BASE_URL` overrides `model.base_url` at resolve
//!   time (the request URL is built from it per-request in rpi-ai).
//! - `~/.rpi/models.json` (if present) merges/overrides the built-in catalog:
//!   each `anthropic-messages` provider contributes its models, with
//!   provider-level `base_url`/`headers`/`authHeader` folded in. The models.json
//!   provider id (e.g. `gateway`) is **config-namespacing only** in v1: every
//!   models.json model is stamped `provider = "anthropic"` so it routes through
//!   the single `AnthropicProvider` (the per-model `base_url` + `headers` carry
//!   the endpoint/auth differentiation). A `--model gateway/custom-claude` just
//!   strips the `gateway/` prefix and matches the `custom-claude` id.
//!
//! # Model pattern precedence (mirrors `resolveCliModel`)
//!
//! 1. `--model` may carry `provider/id[:thinking]`. A leading `anthropic/`
//!    (case-insensitive) is stripped; any other `foo/` prefix is also stripped
//!    so a `models.json` provider id (e.g. `gateway/…`) addresses its model.
//! 2. Otherwise treat `--model` as `id[:thinking]`: if a trailing `:level` is a
//!    valid thinking level, strip it and apply it (overriding `--thinking`);
//!    else the whole string is the id.
//! 3. A `--provider` that isn't `anthropic` is a hard error (v1 has no other
//!    provider). `--provider anthropic` is accepted and just confirms the
//!    default.
//! 4. The model id is matched **exactly, case-insensitively** against the
//!    catalog. The TS resolver additionally does fuzzy/partial matching; v1
//!    keeps it exact to avoid surprising model picks (partial match is a common
//!    source of "got the wrong model" bugs — documented as a divergence in
//!    `docs/m6-cli-open-questions.md`).
//! 5. No `--model` ⇒ the default ([`DEFAULT_MODEL_ID`] = `claude-sonnet-5`),
//!    mirroring the TS per-provider default.
//!
//! [`AnthropicProvider`]: rpi_ai::providers::anthropic::AnthropicProvider

use std::collections::BTreeMap;
use std::sync::Arc;

use rpi_ai::providers::anthropic::models::anthropic_models;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::{Model, Provider, ThinkingLevel};

use crate::args::parse_thinking_level;
use crate::config::{self, Credential, DEFAULT_PROVIDER_ID};

/// The v1-default model id when `--model` is absent. Mirrors the TS
/// `defaultModelPerProvider["anthropic"]` (the first current-generation
/// reasoning model in the catalog).
pub const DEFAULT_MODEL_ID: &str = "claude-sonnet-5";

/// The default thinking level when neither `--thinking` nor a `:level` suffix
/// is present. Mirrors the TS `DEFAULT_THINKING_LEVEL` (`"medium"`, clamped to
/// model capabilities by the harness's provider build_params).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// The resolved run configuration: the provider handle, the chosen model, and
/// the effective thinking level (after `--thinking` / `:level` / model-clamp).
#[derive(Clone)]
pub struct ResolvedModel {
    /// The Anthropic provider (carries the API key, or `None` when Bearer
    /// headers carry the auth). Cheap to clone (`Arc` internally via the
    /// `Provider` trait object).
    pub provider: Arc<dyn Provider>,
    /// The chosen model from the catalog.
    pub model: Model,
    /// Effective thinking level (the requested level, before model-clamp — the
    /// harness/provider clamps to the model's supported set).
    pub thinking_level: ThinkingLevel,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.provider.id())
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .finish()
    }
}

/// The env var consulted for the API key. Mirrors TS `ANTHROPIC_API_KEY`.
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The env var consulted for a bearer token (routed as
/// `Authorization: Bearer`). Mirrors TS `ANTHROPIC_AUTH_TOKEN` — used by
/// third-party Anthropic-compatible gateways (one-api/new-api/claude-code-router
/// and private reverse proxies) that authenticate via `Authorization` rather
/// than `x-api-key`.
pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The env var that overrides the Anthropic endpoint base URL. Mirrors TS
/// `ANTHROPIC_BASE_URL` — point this at a gateway/proxy that speaks the
/// `/v1/messages` protocol.
pub const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Hint text surfaced when no credential source is available. Lists every
/// accepted source so the user can pick the one that fits their setup.
pub const NO_API_KEY_HINT: &str =
    "ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN env, --api-key, or `rpi auth login` (writes ~/.rpi/auth.json)";

/// A resolution error. The TS resolver returns `{ error, warning }`; v1 folds
/// both into a single enum since the CLI treats them the same (print + non-zero
/// exit) except `NoApiKey`, which prints guidance then exits.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Unknown provider \"{0}\". v1 supports: anthropic")]
    UnknownProvider(String),
    #[error("No model matches \"{pattern}\". Available: {available}")]
    NoMatch { pattern: String, available: String },
    #[error("Invalid thinking level \"{0}\" in model pattern. Valid: {1}")]
    InvalidThinkingLevel(String, String),
    #[error("No API key. Set one of: {hint}")]
    NoApiKey { hint: &'static str },
    #[error("Could not read config: {0}")]
    Config(#[from] config::ConfigError),
}

/// Resolve the provider + model + thinking level from the CLI flags + env +
/// `~/.rpi/` config.
///
/// `cli_provider` is the `--provider` value (optional). `cli_model` is the
/// `--model` value (optional; may be `provider/id[:thinking]` or `id[:thinking]`).
/// `cli_thinking` is the `--thinking` value (optional). `cli_api_key` is the
/// `--api-key` value (optional; highest-priority `x-api-key` source).
/// `cli_base_url` is the `--base-url` value (optional; overrides
/// `ANTHROPIC_BASE_URL` + each model's `base_url`).
pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    // ---- Provider selection (v1: Anthropic protocol only) ----
    if let Some(req) = cli_provider {
        if !req.eq_ignore_ascii_case("anthropic") {
            return Err(ResolveError::UnknownProvider(req.to_string()));
        }
    }

    // ---- Auth resolution: provider_key (x-api-key) OR auth_headers (Bearer) ----
    let mut provider_key: Option<String> = None;
    let mut auth_headers: BTreeMap<String, String> = BTreeMap::new();

    // Load the models.json config ONCE — it is consulted both as an auth source
    // (a provider with `authHeader: true` + `apiKey` supplies a Bearer token,
    // mirroring upstream `provider-composer.ts` `withConfiguredAuth`) and as the
    // model catalog merge source (below). Loading here (before the auth gate)
    // means a static `~/.rpi/models.json` gateway credential can satisfy auth
    // without any env var or `rpi auth login` — the models.json file alone is a
    // complete third-party-endpoint setup.
    let models_cfg = config::load_models_config()?;

    // 1. --api-key (highest-priority x-api-key source).
    if let Some(k) = cli_api_key.filter(|s| !s.is_empty()) {
        provider_key = Some(k.to_string());
    }
    // 2. ~/.rpi/auth.json anthropic.api_key.key (persistent login).
    if provider_key.is_none() {
        if let Ok(store) = config::read_auth() {
            if let Some(Credential::ApiKey { key: Some(k), .. }) = store.get(DEFAULT_PROVIDER_ID) {
                if !k.is_empty() {
                    provider_key = Some(k.clone());
                }
            }
        }
    }
    // 3. ~/.rpi/models.json provider with authHeader:true + apiKey → Bearer.
    //    The first anthropic-compatible provider that declares a static gateway
    //    key supplies the Bearer token (v1 routes through one provider, so the
    //    first match is authoritative). Mirrors upstream's `authHeader` handling
    //    where the resolved apiKey is wrapped as `Authorization: Bearer`.
    if provider_key.is_none() && auth_headers.is_empty() {
        if let Some(tok) = models_json_bearer_token(&models_cfg) {
            auth_headers.insert("authorization".to_string(), format!("Bearer {tok}"));
        }
    }
    // 4. ANTHROPIC_AUTH_TOKEN → Authorization: Bearer (third-party gateways).
    if provider_key.is_none() && auth_headers.is_empty() {
        if let Ok(tok) = std::env::var(ANTHROPIC_AUTH_TOKEN_ENV) {
            if !tok.is_empty() {
                auth_headers.insert("authorization".to_string(), format!("Bearer {tok}"));
            }
        }
    }
    // 5. ANTHROPIC_API_KEY → x-api-key (fallback).
    if provider_key.is_none() && auth_headers.is_empty() {
        if let Ok(k) = std::env::var(ANTHROPIC_API_KEY_ENV) {
            if !k.is_empty() {
                provider_key = Some(k);
            }
        }
    }
    // 6. Nothing → clear error listing every accepted source.
    if provider_key.is_none() && auth_headers.is_empty() {
        return Err(ResolveError::NoApiKey { hint: NO_API_KEY_HINT });
    }

    // ---- Endpoint override (--base-url → ANTHROPIC_BASE_URL) ----
    let base_url_override = cli_base_url
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var(ANTHROPIC_BASE_URL_ENV)
                .ok()
                .filter(|s| !s.is_empty())
        });

    // ---- Catalog: built-in + ~/.rpi/models.json (merged, reusing the
    // already-loaded config) ----
    let mut catalog = anthropic_models();
    merge_user_catalog(&mut catalog, &models_cfg);

    // Apply the endpoint override to every model (the request URL is built from
    // `model.base_url` per-request in rpi-ai).
    if let Some(base) = &base_url_override {
        for m in catalog.iter_mut() {
            m.base_url = base.clone();
        }
    }

    // Fold the Bearer header (if any) into every model so `has_header_auth`
    // recognizes it and the provider skips `x-api-key`. Provider-level headers
    // from models.json are preserved; an env-derived Bearer is additive
    // (inserted via `entry` so a models.json bearer isn't clobbered when the
    // env token is absent — but when both exist, the env token is the
    // interactive-session override and wins).
    if !auth_headers.is_empty() {
        for m in catalog.iter_mut() {
            let headers = m.headers.get_or_insert_with(BTreeMap::new);
            for (k, v) in &auth_headers {
                headers.insert(k.clone(), v.clone());
            }
        }
    }

    let available = catalog
        .iter()
        .map(|m| m.id.clone())
        .collect::<Vec<_>>()
        .join(", ");

    // ---- Model + thinking pattern parse ----
    let (pattern, pattern_thinking) = split_model_pattern(cli_model.unwrap_or(DEFAULT_MODEL_ID));

    // Effective thinking: `--thinking` wins over a `:level` suffix; else default.
    let thinking_level = cli_thinking
        .or(pattern_thinking)
        .unwrap_or(DEFAULT_THINKING_LEVEL);

    // Match the pattern against the catalog.
    let model = match find_model(&pattern, &catalog) {
        Some(m) => m,
        None => {
            return Err(ResolveError::NoMatch {
                pattern: pattern.clone(),
                available,
            });
        }
    };

    // ---- Provider build ----
    // Bearer path: `provider_key = None` — the model headers carry the auth
    // (`has_header_auth` skips x-api-key). x-api-key path: pass the key.
    let provider: Arc<dyn Provider> = Arc::new(AnthropicProvider::with_models(
        provider_key,
        reqwest::Client::new(),
        catalog,
    ));

    Ok(ResolvedModel { provider, model, thinking_level })
}

/// Merge `~/.rpi/models.json` providers into the built-in catalog. Models from
/// the user file replace any built-in entry with the same id (custom
/// definitions win); brand-new ids are appended. Non-`anthropic-messages`
/// providers are skipped (ignored in v1, documented). Takes the already-loaded
/// config so the file is read once per `resolve`.
fn merge_user_catalog(catalog: &mut Vec<Model>, cfg: &config::ModelsConfig) {
    for (provider_id, provider_cfg) in &cfg.providers {
        let Some(models) = config::provider_to_models(provider_id, provider_cfg) else {
            // Non-anthropic protocol — ignored in v1 (documented).
            continue;
        };
        for m in models {
            if let Some(existing) = catalog.iter_mut().find(|c| c.id.eq_ignore_ascii_case(&m.id)) {
                *existing = m;
            } else {
                catalog.push(m);
            }
        }
    }
}

/// Extract a static gateway Bearer token from the first anthropic-compatible
/// models.json provider that declares `authHeader: true` + a non-empty
/// `apiKey`. Mirrors upstream's `withConfiguredAuth` (`authHeader` wraps the
/// resolved key as `Authorization: Bearer`). Returns `None` when no such
/// provider exists (the env/stored-cred/cli-flag sources still apply).
fn models_json_bearer_token(cfg: &config::ModelsConfig) -> Option<String> {
    for (_provider_id, provider_cfg) in &cfg.providers {
        if !config::provider_is_anthropic_compatible(provider_cfg) {
            continue;
        }
        if provider_cfg.auth_header.unwrap_or(false) {
            if let Some(key) = provider_cfg.api_key.as_deref().filter(|s| !s.is_empty()) {
                return Some(key.to_string());
            }
        }
    }
    None
}

/// Split a `--model` value into `(id_pattern, optional_thinking_level)`.
///
/// Handles `provider/id[:thinking]` (strips a leading `anthropic/` or any other
/// `foo/` prefix so a `models.json` provider id addresses its model) and
/// `id[:thinking]`. A trailing `:level` is parsed as a thinking level only if
/// it is a valid level string; otherwise the whole tail is kept in the id
/// pattern (some model ids legitimately contain colons — none do in the v1
/// Anthropic catalog, but the parser stays conservative).
///
/// Mirrors the TS `parseModelPattern` last-colon split + recurse-on-prefix.
fn split_model_pattern(value: &str) -> (String, Option<ThinkingLevel>) {
    // Strip a leading `provider/` prefix. `anthropic/` is the common case; any
    // other `foo/` prefix is also stripped so a `models.json` provider id (e.g.
    // `gateway/custom-claude`) resolves to the `custom-claude` catalog entry.
    let trimmed = value
        .strip_prefix("anthropic/")
        .or_else(|| value.strip_prefix("Anthropic/"))
        .or_else(|| {
            if let Some(idx) = value.find('/') {
                Some(&value[idx + 1..])
            } else {
                None
            }
        })
        .unwrap_or(value);

    // Last-colon split: if the suffix is a valid thinking level, peel it.
    if let Some(idx) = trimmed.rfind(':') {
        let (head, tail) = trimmed.split_at(idx);
        let suffix = &tail[1..]; // drop the ':'
        if let Some(level) = parse_thinking_level(suffix) {
            return (head.to_string(), Some(level));
        }
    }
    (trimmed.to_string(), None)
}

/// Case-insensitive exact id match against the catalog. The TS resolver also
/// does partial/fuzzy match; v1 keeps it exact (see module docs).
fn find_model(pattern: &str, catalog: &[Model]) -> Option<Model> {
    catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(pattern))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{parse_thinking_level, VALID_THINKING_LEVELS};
    use crate::config::test_support::env_lock;

    /// Scope a test to a throwaway config dir + clear the `ANTHROPIC_*` env
    /// vars, restoring both on drop. Holds the shared env lock for its whole
    /// lifetime so parallel env-mutating tests across config/provider/auth all
    /// serialize on one mutex.
    struct TestEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev_key: Option<std::ffi::OsString>,
        prev_tok: Option<std::ffi::OsString>,
        prev_base: Option<std::ffi::OsString>,
        prev_dir: Option<std::ffi::OsString>,
        _tmp: tempfile::TempDir,
    }
    impl TestEnv {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
            let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
            let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
            std::env::remove_var(ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
            std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                prev_key,
                prev_tok,
                prev_base,
                prev_dir,
                _tmp: tmp,
            }
        }
    }
    impl Drop for TestEnv {
        fn drop(&mut self) {
            restore(ANTHROPIC_API_KEY_ENV, self.prev_key.take());
            restore(ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
            restore(ANTHROPIC_BASE_URL_ENV, self.prev_base.take());
            restore(config::CONFIG_DIR_ENV, self.prev_dir.take());
        }
    }
    fn restore(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    // These tests hit the network-free resolution path only (provider/model
    // selection). They set a throwaway credential so `resolve` clears the
    // `NoApiKey` gate, then assert the model + thinking choice — never making
    // a real request.

    fn resolve_with_key(
        provider: Option<&str>,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<ResolvedModel, ResolveError> {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "test-key");
        resolve(provider, model, thinking, None, None)
    }

    #[test]
    fn default_model_is_sonnet_5() {
        let r = resolve_with_key(None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        assert_eq!(r.thinking_level, DEFAULT_THINKING_LEVEL);
        assert_eq!(r.provider.id(), "anthropic");
    }

    #[test]
    fn explicit_id_match() {
        let r = resolve_with_key(None, Some("claude-haiku-4-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-haiku-4-5");
    }

    #[test]
    fn case_insensitive_id() {
        let r = resolve_with_key(None, Some("CLAUDE-OPUS-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-opus-5");
    }

    #[test]
    fn provider_prefix_stripped() {
        let r = resolve_with_key(None, Some("anthropic/claude-sonnet-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
    }

    #[test]
    fn custom_provider_prefix_stripped() {
        // `gateway/custom-claude` resolves to the catalog id `custom-claude`
        // after the `foo/` prefix is stripped.
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{ "providers": { "gateway": { "baseUrl": "https://gw", "models": [{"id":"custom-claude"}] } } }"#,
        )
        .unwrap();
        let r = resolve(None, Some("gateway/custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
    }

    #[test]
    fn thinking_suffix_in_model() {
        let r = resolve_with_key(None, Some("claude-sonnet-5:high"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
        assert_eq!(r.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn thinking_flag_overrides_suffix() {
        // `--thinking low` wins over a `:high` suffix.
        let r =
            resolve_with_key(None, Some("claude-sonnet-5:high"), Some(ThinkingLevel::Low)).unwrap();
        assert_eq!(r.thinking_level, ThinkingLevel::Low);
    }

    #[test]
    fn explicit_provider_anthropic_ok() {
        let r = resolve_with_key(Some("anthropic"), Some("claude-sonnet-5"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
    }

    #[test]
    fn unknown_provider_rejected() {
        let err = resolve_with_key(Some("openai"), None, None).unwrap_err();
        assert!(matches!(err, ResolveError::UnknownProvider(_)));
    }

    #[test]
    fn no_match_lists_available() {
        let err = resolve_with_key(None, Some("claude-does-not-exist"), None).unwrap_err();
        match err {
            ResolveError::NoMatch { pattern, available } => {
                assert_eq!(pattern, "claude-does-not-exist");
                assert!(available.contains("claude-sonnet-5"));
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn colon_not_a_thinking_level_kept_in_id() {
        // A trailing `:foo` that isn't a thinking level stays part of the id
        // pattern → no match (no model id contains `:foo`).
        let err = resolve_with_key(None, Some("claude-sonnet-5:foo"), None).unwrap_err();
        assert!(matches!(err, ResolveError::NoMatch { .. }));
    }

    #[test]
    fn parse_thinking_level_roundtrip() {
        assert_eq!(parse_thinking_level("xhigh"), Some(ThinkingLevel::Xhigh));
        assert_eq!(parse_thinking_level("bogus"), None);
        // Sanity: the valid set matches what help advertises.
        for lvl in VALID_THINKING_LEVELS {
            assert!(parse_thinking_level(lvl).is_some(), "{lvl} should parse");
        }
    }

    #[test]
    fn no_api_key_errors_with_hint() {
        let _env = TestEnv::new();
        let err = resolve(None, None, None, None, None).unwrap_err();
        match err {
            ResolveError::NoApiKey { hint } => {
                assert!(hint.contains("ANTHROPIC_API_KEY"));
                assert!(hint.contains("auth login"));
            }
            other => panic!("expected NoApiKey, got {other:?}"),
        }
    }

    #[test]
    fn stored_credential_satisfies_auth() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey { key: Some("stored-key".into()), env: None },
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        // x-api-key path: no Bearer header folded onto the model (auth rides on
        // the provider's default key, surfaced to the provider at build time).
        assert!(
            r.model.headers.as_ref().and_then(|h| h.get("authorization")).is_none(),
            "x-api-key path should not synthesize a Bearer header"
        );
    }

    #[test]
    fn auth_token_routes_via_bearer_header() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_AUTH_TOKEN_ENV, "tok-123");
        let r = resolve(None, None, None, None, None).unwrap();
        // No provider key carries auth — it lives on the model header.
        let headers = r.model.headers.as_ref().expect("bearer header on model");
        assert_eq!(headers.get("authorization").map(|s| s.as_str()), Some("Bearer tok-123"));
    }

    #[test]
    fn api_key_flag_beats_env_and_stored() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "env-key");
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey { key: Some("stored-key".into()), env: None },
        )
        .unwrap();
        // `--api-key flag-key` wins; resolve succeeds + takes the x-api-key path
        // (no Bearer header on the model).
        let r = resolve(None, None, None, Some("flag-key"), None).unwrap();
        assert!(
            r.model.headers.as_ref().and_then(|h| h.get("authorization")).is_none(),
            "--api-key should take the x-api-key path, not Bearer"
        );
    }

    #[test]
    fn base_url_override_applies_to_model() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let r = resolve(None, None, None, None, Some("https://gw.example.com")).unwrap();
        assert_eq!(r.model.base_url, "https://gw.example.com");
    }

    #[test]
    fn base_url_env_is_fallback_for_flag() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://env-gw.example.com");
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.base_url, "https://env-gw.example.com");
    }

    #[test]
    fn models_json_adds_custom_model() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "name": "Custom" }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        // The model is routed through the single AnthropicProvider (provider
        // stamped "anthropic" by config::provider_to_models).
        assert_eq!(r.model.provider, DEFAULT_PROVIDER_ID);
        // Provider-level authHeader folded in.
        let headers = r.model.headers.as_ref().expect("headers merged");
        assert_eq!(headers.get("authorization").map(|s| s.as_str()), Some("Bearer gw-secret"));
    }

    /// A models.json gateway with `authHeader:true` + `apiKey` is itself an auth
    /// source — it satisfies the `resolve` auth gate WITHOUT any env var, stored
    /// cred, or `--api-key`. This is the "models.json file alone sets up a
    /// third-party endpoint" path. The Bearer token folds onto every model and
    /// `resolve` succeeds.
    #[test]
    fn models_json_auth_header_satisfies_auth_without_env() {
        let _env = TestEnv::new();
        // No ANTHROPIC_* env, no auth.json — only the models.json gateway.
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        let headers = r.model.headers.as_ref().expect("bearer folded onto model");
        assert_eq!(headers.get("authorization").map(|s| s.as_str()), Some("Bearer gw-secret"));
    }

    /// The `--api-key` flag wins over a models.json `authHeader:true` gateway
    /// key (the flag is the highest-priority x-api-key source; the gateway
    /// Bearer is only consulted when no key path is taken).
    #[test]
    fn api_key_flag_beats_models_json_bearer() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [ { "id": "custom-claude" } ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, Some("flag-key"), None).unwrap();
        // --api-key path: no Bearer folded on (the gateway bearer is skipped).
        assert!(
            r.model.headers.as_ref().and_then(|h| h.get("authorization")).is_none(),
            "--api-key should win over the models.json gateway bearer"
        );
    }
}
