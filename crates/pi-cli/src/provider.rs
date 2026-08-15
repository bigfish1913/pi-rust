//! Provider + model resolution. Mirrors the *Anthropic-only* slice of the TS
//! `packages/coding-agent/src/core/model-resolver.ts` (`resolveCliModel` +
//! the `provider/id[:thinking]` parsing in [`crate::args`]).
//!
//! v1 is Anthropic-only (plan §5.16: "OAuth/Copilot skipped v1; API-key auth
//! only"). The TS `ModelRuntime`/`ModelRegistry` multi-provider machinery is
//! not ported; this module builds a single [`AnthropicProvider`] from an API
//! key and resolves a [`Model`] + [`ThinkingLevel`] against its fixed catalog.
//!
//! # Resolution precedence (mirrors `resolveCliModel`)
//!
//! 1. `--model` may carry `provider/id[:thinking]`. If the prefix before the
//!    first `/` is the provider (`anthropic`), strip it and parse the rest as
//!    `id[:thinking]`.
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

use std::sync::Arc;

use rpi_ai::providers::anthropic::models::anthropic_models;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::{Model, Provider, ThinkingLevel};

use crate::args::parse_thinking_level;

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
    /// The Anthropic provider (carries the API key). Cheap to clone (`Arc`
    /// internally via the `Provider` trait object).
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
    #[error("No API key. Set {env} or pass --api-key.")]
    NoApiKey { env: &'static str },
}

/// Resolve the provider + model + thinking level from the CLI flags + env.
///
/// `cli_provider` is the `--provider` value (optional). `cli_model` is the
/// `--model` value (optional; may be `provider/id[:thinking]` or `id[:thinking]`).
/// `cli_thinking` is the `--thinking` value (optional). `cli_api_key` is the
/// `--api-key` value (optional; overrides `ANTHROPIC_API_KEY`).
pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    // ---- Provider selection (v1: Anthropic only) ----
    if let Some(req) = cli_provider {
        if !req.eq_ignore_ascii_case("anthropic") {
            return Err(ResolveError::UnknownProvider(req.to_string()));
        }
    }

    // ---- API key ----
    let api_key = cli_api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var(ANTHROPIC_API_KEY_ENV).ok().filter(|s| !s.is_empty()));
    // The provider is built without a pinned key when none is supplied; the
    // harness still constructs, but the first turn will fail at the provider.
    // We surface a clear error up-front so the user knows to set the key.
    if api_key.is_none() {
        return Err(ResolveError::NoApiKey { env: ANTHROPIC_API_KEY_ENV });
    }
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::new(api_key, reqwest::Client::new()));

    // ---- Model + thinking pattern parse ----
    let catalog = anthropic_models();
    let available = catalog
        .iter()
        .map(|m| m.id.clone())
        .collect::<Vec<_>>()
        .join(", ");

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

    Ok(ResolvedModel { provider, model, thinking_level })
}

/// Split a `--model` value into `(id_pattern, optional_thinking_level)`.
///
/// Handles `provider/id[:thinking]` (strips a leading `anthropic/`) and
/// `id[:thinking]`. A trailing `:level` is parsed as a thinking level only if
/// it is a valid level string; otherwise the whole tail is kept in the id
/// pattern (some providers/model ids legitimately contain colons — none do in
/// the v1 Anthropic catalog, but the parser stays conservative).
///
/// Mirrors the TS `parseModelPattern` last-colon split + recurse-on-prefix.
fn split_model_pattern(value: &str) -> (String, Option<ThinkingLevel>) {
    // Strip a leading `provider/` when the provider is anthropic.
    let trimmed = value
        .strip_prefix("anthropic/")
        .or_else(|| value.strip_prefix("Anthropic/"))
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
    use std::sync::{Mutex, OnceLock};

    /// Tests in this module mutate the process-global `ANTHROPIC_API_KEY` env
    /// var, so they race under the default parallel test runner. This mutex
    /// serializes every env-touching test to keep the set/restore/clear windows
    /// non-overlapping.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn env_key() -> Option<String> {
        std::env::var(ANTHROPIC_API_KEY_ENV).ok().filter(|s| !s.is_empty())
    }

    // These tests hit the network-free resolution path only (provider/model
    // selection). They set a throwaway API key so `resolve` clears the
    // `NoApiKey` gate, then assert the model + thinking choice — never making
    // a real request.

    fn resolve_with_key(
        provider: Option<&str>,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<ResolvedModel, ResolveError> {
        let _guard = env_lock().lock().unwrap();
        let prev = env_key();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "test-key");
        let r = resolve(provider, model, thinking, None);
        match prev {
            Some(v) => std::env::set_var(ANTHROPIC_API_KEY_ENV, v),
            None => std::env::remove_var(ANTHROPIC_API_KEY_ENV),
        }
        r
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
    fn thinking_suffix_in_model() {
        let r = resolve_with_key(None, Some("claude-sonnet-5:high"), None).unwrap();
        assert_eq!(r.model.id, "claude-sonnet-5");
        assert_eq!(r.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn thinking_flag_overrides_suffix() {
        // `--thinking low` wins over a `:high` suffix.
        let r = resolve_with_key(None, Some("claude-sonnet-5:high"), Some(ThinkingLevel::Low)).unwrap();
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
    fn no_api_key_errors_with_env_name() {
        let _guard = env_lock().lock().unwrap();
        let prev = env_key();
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        let err = resolve(None, None, None, None).unwrap_err();
        match err {
            ResolveError::NoApiKey { env } => assert_eq!(env, ANTHROPIC_API_KEY_ENV),
            other => panic!("expected NoApiKey, got {other:?}"),
        }
        match prev {
            Some(v) => std::env::set_var(ANTHROPIC_API_KEY_ENV, v),
            None => {}
        }
    }
}
