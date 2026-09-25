//! Provider + model resolution. Mirrors the provider/model selection and
//! request-auth portions of native Pi's model runtime.
//!
//! The built-in lane uses Anthropic's protocol, while `models.json` may add
//! named Anthropic-compatible, OpenAI Completions, and OpenAI Responses
//! providers. OAuth/Copilot remains deferred. This module resolves one provider
//! identity and its isolated model catalog for each run.
//!
//! # Auth resolution and isolation
//!
//! Credentials are resolved independently for every provider id:
//!
//! 1. `--api-key` overrides only the finally selected provider.
//! 2. `auth.json[provider-id]` wins over that provider's configured key.
//! 3. `models.json.providers[provider-id].apiKey` (plus `authHeader`) applies
//!    only to models owned by that provider.
//! 4. Provider-specific environment variables are fallbacks. The global
//!    `ANTHROPIC_*` and `OPENAI_API_KEY` variables belong only to the built-in
//!    `anthropic` and `openai` identities respectively.
//!
//! A credential never makes another provider's model available and is never
//! passed to another provider's runtime, even when endpoints or model ids are
//! identical.
//!
//! # Endpoint + catalog
//!
//! - `--base-url` overrides the selected `model.base_url` at resolve time;
//!   `ANTHROPIC_BASE_URL` is the fallback for the built-in `anthropic` identity
//!   only (the request URL is built from it per-request in rpi-ai).
//! - `~/.rpi/models.json` (if present) merges/overrides the built-in catalog:
//!   each `anthropic-messages` provider contributes its models, with
//!   provider-level `base_url`/`headers`/`authHeader` folded in. The models.json
//!   provider id (e.g. `gateway`) remains the routing and credential identity,
//!   so providers may safely share a protocol, endpoint, or model id.
//!
//! # Model pattern precedence (mirrors `resolveCliModel`)
//!
//! 1. `--model` may carry `provider/id[:thinking]`. The prefix is interpreted
//!    as a provider only when it names a known built-in or `models.json`
//!    provider. Otherwise the slash remains part of the raw model id (for
//!    example `meta-llama/llama-*`). If both interpretations exist, the known
//!    provider wins while authenticated; an unauthenticated inferred provider
//!    yields to one uniquely authenticated raw-id match, as in native Pi.
//! 2. Otherwise treat `--model` as `id[:thinking]`: if a trailing `:level` is a
//!    valid thinking level, strip it and apply it (overriding `--thinking`);
//!    else the whole string is the id.
//! 3. `--provider` must name a built-in provider or a configured models.json
//!    provider and restricts selection to that identity.
//! 4. The model id is matched **exactly, case-insensitively** against the
//!    catalog. The TS resolver additionally does fuzzy/partial matching; v1
//!    keeps it exact to avoid surprising model picks (partial match is a common
//!    source of "got the wrong model" bugs — documented as a divergence in
//!    `docs/m6-cli-open-questions.md`).
//! 5. No `--model` ⇒ [`pick_default_model`]:
//!    (a) scan native Pi's `defaultModelPerProvider` entries in their declared
//!    order and take the first authenticated match; otherwise (b) take the
//!    **first authenticated model** in the catalog — mirroring the TS
//!    `findInitialModel` fallback over `availableModels`. This lets a
//!    `models.json`-only gateway config "just work": the built-in Anthropic
//!    models carry no auth, so the gateway model (the only authenticated one)
//!    is picked. The all-builtin/no-custom-code default (`ANTHROPIC_API_KEY`
//!    path) selects `claude-opus-4-8`. Last resort falls back to
//!    [`DEFAULT_MODEL_ID`] (or the catalog head) — unreachable in practice
//!    because the auth gate refuses an unauthed catalog earlier.
//!
//! [`AnthropicProvider`]: rpi_ai::providers::anthropic::AnthropicProvider

use std::collections::BTreeMap;
use std::sync::Arc;

use rpi_ai::providers::anthropic::models::anthropic_models;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_ai::providers::openai_responses::openai_responses_models;
use rpi_ai::providers::openai_responses::OpenAiResponsesProvider;
use rpi_ai::{
    AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions, ThinkingLevel,
};

use crate::args::parse_thinking_level;
use crate::config::{self, Credential, DEFAULT_PROVIDER_ID};
use crate::settings;

/// The default Anthropic model when `--model` is absent. Kept in sync with the
/// current native Pi `defaultModelPerProvider.anthropic` entry.
pub const DEFAULT_MODEL_ID: &str = "claude-opus-4-8";

/// Native Pi checks these provider defaults in declaration order before it
/// falls back to `availableModels[0]`. Configured providers using one of rpi's
/// supported wire protocols participate too, even when they are not built in.
const DEFAULT_MODELS_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", DEFAULT_MODEL_ID),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-5.5"),
    ("radius", "auto"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.6"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
    ("together", "moonshotai/Kimi-K2.6"),
    ("baseten", "zai-org/GLM-5.2"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k2.6"),
    ("kimi-coding", "kimi-for-coding"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    (
        "cloudflare-ai-gateway",
        "workers-ai/@cf/moonshotai/kimi-k2.6",
    ),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

/// The default thinking level when neither `--thinking` nor a `:level` suffix
/// is present. Mirrors the TS `DEFAULT_THINKING_LEVEL` (`"medium"`, clamped to
/// model capabilities by the harness's provider build_params).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// The resolved run configuration: the provider handle, the chosen model, and
/// the effective thinking level (after `--thinking` / `:level` / model-clamp).
#[derive(Clone)]
pub struct ResolvedModel {
    /// The selected provider. Cheap to clone through the trait-object `Arc`.
    pub provider: Arc<dyn Provider>,
    /// The chosen model from the catalog.
    pub model: Model,
    /// Effective thinking level (the requested level, before model-clamp — the
    /// harness/provider clamps to the model's supported set).
    pub thinking_level: ThinkingLevel,
    /// Whether the selected runtime carries a provider-default key. When
    /// false, authentication is owned by the selected model's headers.
    ///
    /// Kept so [`available_catalog`] can reproduce the auth-filtered snapshot
    /// (pi `getAvailableSnapshot`: `available = all.filter(m =>
    /// configuredProviders.has(m.provider))`) and surface only models that
    /// won't fail at request time with "No API key for provider".
    pub has_provider_key: bool,
    /// Saved theme name from `~/.rpi/agent/settings.json`, if any. Best-effort:
    /// the TUI applies it at startup when it matches a known preset
    /// (dark/light/monochrome); otherwise ignored.
    pub theme: Option<String>,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.provider.id())
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("has_provider_key", &self.has_provider_key)
            .field("theme", &self.theme)
            .finish()
    }
}

/// Bind the Anthropic wire implementation to the provider id declared in
/// models.json. Native Pi keeps providers isolated by id even when they share
/// the same protocol and model ids; the wrapper preserves that routing identity
/// without duplicating the protocol implementation.
struct NamedAnthropicProvider {
    id: String,
    inner: AnthropicProvider,
}

#[async_trait::async_trait]
impl Provider for NamedAnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[Model] {
        self.inner.models()
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_simple(model, ctx, opts).await
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

/// Standard OpenAI API-key environment variable used by the
/// `openai-completions` provider.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Hint text surfaced when no credential source is available. Lists every
/// accepted source so the user can pick the one that fits their setup.
pub const NO_API_KEY_HINT: &str =
    "models.json apiKey, OPENAI_API_KEY / ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN env, --api-key, or `rpi auth login`";

/// A resolution error. The TS resolver returns `{ error, warning }`; v1 folds
/// both into a single enum since the CLI treats them the same (print + non-zero
/// exit) except `NoApiKey`, which prints guidance then exits.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Unknown provider \"{0}\". Supported: anthropic, openai-completions, openai-responses, or a models.json provider id")]
    UnknownProvider(String),
    #[error(
        "Provider \"{requested}\" is ambiguous: {matches}. Use the exact provider id to select one."
    )]
    AmbiguousProvider { requested: String, matches: String },
    #[error("No model matches \"{pattern}\". Available: {available}")]
    NoMatch { pattern: String, available: String },
    #[error(
        "Model \"{pattern}\" is ambiguous across providers: {matches}. {auth_hint} Use --provider or provider/model."
    )]
    AmbiguousModel {
        pattern: String,
        matches: String,
        auth_hint: &'static str,
    },
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
/// Build the provider HTTP client honoring system proxy settings and the
/// `httpIdleTimeout` setting. Uses the model's own `base_url` when present (so
/// `NO_PROXY` matches the real host), else `default_base_url`.
fn provider_http_client(
    models: &[Model],
    default_base_url: &str,
    settings: &settings::Settings,
) -> reqwest::Client {
    let base_url = models
        .iter()
        .map(|m| m.base_url.as_str())
        .find(|u| !u.is_empty())
        .unwrap_or(default_base_url);
    rpi_ai::http::build_client(base_url, settings.http_idle_timeout_ms(), None)
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_settings().unwrap_or_default();
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

/// Resolve using native Pi's global -> trusted-project settings precedence.
pub fn resolve_for_cwd(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    cwd: &std::path::Path,
    project_trusted: bool,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, project_trusted)?;
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

fn resolve_with_settings(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    settings: settings::Settings,
) -> Result<ResolvedModel, ResolveError> {
    // Load models/auth once, then resolve Anthropic-wire credentials per
    // provider id. A key belonging to `anthropic` must never make an unrelated
    // gateway available or become that gateway's provider default.
    let models_cfg = config::load_models_config()?;
    let canonical_cli_provider = cli_provider
        .map(|requested| canonicalize_cli_provider(requested, &models_cfg))
        .transpose()?;
    let cli_provider = canonical_cli_provider.as_deref();
    let auth_store = config::read_auth()?;
    let anthropic_credentials = resolve_anthropic_credentials(&models_cfg, &auth_store);
    let openai_credentials = resolve_openai_credentials(&models_cfg, &auth_store);
    // The CLI override is intentionally not inserted into either provider map:
    // it is applied only after the final model/provider identity is selected.
    let cli_api_key = cli_api_key
        .filter(|key| !key.is_empty())
        .map(str::to_string);

    // ---- Endpoint override (--base-url → ANTHROPIC_BASE_URL) ----
    let cli_base_url_override = cli_base_url.map(str::to_string);
    let anthropic_base_url_override = std::env::var(ANTHROPIC_BASE_URL_ENV)
        .ok()
        .filter(|value| !value.is_empty());

    // ---- Catalog: built-in + ~/.rpi/models.json (merged, reusing the
    // already-loaded config) ----
    let mut catalog = anthropic_models();
    catalog.extend(openai_responses_models());
    merge_user_catalog(&mut catalog, &models_cfg);

    // Apply an explicit CLI endpoint override to the selected catalog lane.
    // The ambient Anthropic override belongs only to the built-in `anthropic`
    // identity; applying it to custom Anthropic-wire providers would redirect
    // their provider-specific credentials to an unrelated endpoint.
    for model in &mut catalog {
        if let Some(base) = &cli_base_url_override {
            model.base_url = base.clone();
        } else if matches!(model.api, rpi_ai::Api::AnthropicMessages)
            && model.provider == DEFAULT_PROVIDER_ID
        {
            if let Some(base) = &anthropic_base_url_override {
                model.base_url = base.clone();
            }
        }
    }

    if let Some(requested) = cli_provider {
        catalog.retain(|model| provider_matches(model, requested, &models_cfg));
    }

    let available = catalog
        .iter()
        .map(|m| m.id.clone())
        .collect::<Vec<_>>()
        .join(", ");
    if catalog.is_empty() {
        return Err(ResolveError::NoMatch {
            pattern: cli_provider.unwrap_or("default").to_string(),
            available,
        });
    }

    // ---- Model selection ----
    // With `--model`: parse the pattern (`provider/id[:thinking]`), match it
    // exactly against the catalog (TS fuzzy/partial match is a deliberate v1
    // omission — see module docs §5). Without `--model`: pi `findInitialModel`
    // precedence — (3) the saved default from settings (when present + authed),
    // then (4) `pick_default_model` (built-in default if authed, else first
    // authed). The saved default mirrors `findInitialModel` step 3 and lets a
    // copied pi `settings.json`'s `defaultModel` come alive on launch.
    let (mut model, thinking_level) = match cli_model {
        Some(raw) => {
            let parsed_pattern = split_model_pattern(raw, cli_provider, &catalog, &models_cfg)?;
            if let Some(provider) = parsed_pattern.provider.as_deref() {
                if !provider_is_known(provider, &models_cfg) {
                    return Err(ResolveError::UnknownProvider(provider.to_string()));
                }
            }
            let mut pattern_thinking = parsed_pattern.thinking;
            let mut selected = find_cli_model(
                &parsed_pattern.model_id,
                parsed_pattern.provider.as_deref(),
                &catalog,
                &models_cfg,
                &anthropic_credentials,
                &openai_credentials,
            )?;

            if parsed_pattern.inferred_provider {
                if let Some(inferred) = selected.as_ref() {
                    if !model_is_authed_for_resolution(
                        inferred,
                        &anthropic_credentials,
                        &openai_credentials,
                        false,
                    ) {
                        let authenticated_raw_matches = |candidate: &str| {
                            catalog
                                .iter()
                                .filter(|model| {
                                    model.id.eq_ignore_ascii_case(candidate)
                                        && (model.provider != inferred.provider
                                            || model.id != inferred.id)
                                        && model_is_authed_for_resolution(
                                            model,
                                            &anthropic_credentials,
                                            &openai_credentials,
                                            false,
                                        )
                                })
                                .collect::<Vec<_>>()
                        };
                        let mut raw_matches =
                            authenticated_raw_matches(&parsed_pattern.raw_model_id);
                        let mut matched_complete_raw_id = true;
                        if raw_matches.is_empty() && parsed_pattern.thinking.is_some() {
                            if let Some((raw_without_thinking, _)) =
                                parsed_pattern.raw_model_id.rsplit_once(':')
                            {
                                raw_matches = authenticated_raw_matches(raw_without_thinking);
                                matched_complete_raw_id = false;
                            }
                        }
                        if let [raw_match] = raw_matches.as_slice() {
                            selected = Some((*raw_match).clone());
                            if matched_complete_raw_id {
                                pattern_thinking = None;
                            }
                        }
                    }
                } else {
                    selected = find_cli_model(
                        &parsed_pattern.raw_model_id,
                        None,
                        &catalog,
                        &models_cfg,
                        &anthropic_credentials,
                        &openai_credentials,
                    )?;
                    if selected.is_some() {
                        pattern_thinking = None;
                    } else if parsed_pattern.thinking.is_some() {
                        if let Some((raw_without_thinking, _)) =
                            parsed_pattern.raw_model_id.rsplit_once(':')
                        {
                            selected = find_cli_model(
                                raw_without_thinking,
                                None,
                                &catalog,
                                &models_cfg,
                                &anthropic_credentials,
                                &openai_credentials,
                            )?;
                        }
                    }
                }
            }

            let model = match selected {
                Some(m) => m,
                None => {
                    return Err(ResolveError::NoMatch {
                        pattern: parsed_pattern.model_id,
                        available,
                    });
                }
            };
            // `--thinking` wins over a parsed `:level` suffix; an exact model
            // id containing that suffix does not implicitly set thinking.
            let thinking_level = cli_thinking
                .or(pattern_thinking)
                .unwrap_or(DEFAULT_THINKING_LEVEL);
            (model, thinking_level)
        }
        None => {
            // `--thinking` > settings `defaultThinkingLevel` > built-in default.
            // The settings level is honored only when its model is also the
            // saved default (matches pi, which applies `defaultThinkingLevel`
            // inside the step-3 branch). For the fallback default, keep
            // `DEFAULT_THINKING_LEVEL`.
            let settings_thinking = settings
                .default_thinking_level
                .as_deref()
                .and_then(parse_thinking_level);

            // (3) Saved default from settings, when the provider is anthropic
            // (or absent — v1 is anthropic-only) OR names a configured
            // models.json gateway (config-namespacing: the saved
            // `defaultProvider` id matches a `~/.rpi/models.json` provider
            // key), and the saved model is authed. Without the gateway arm a
            // copied pi settings.json (`defaultProvider:
            // "cc-switch-deep-seek-copy-2"`) is ignored and the default falls
            // to first-authed — which, once a second gateway is enabled, may
            // NOT be the user's saved choice (BTreeMap provider order).
            let saved_provider = settings
                .default_provider
                .as_deref()
                .filter(|provider| provider_is_known(provider, &models_cfg));
            let saved = settings.default_model.as_deref().and_then(|id| {
                if settings.default_provider.is_some() && saved_provider.is_none() {
                    return None;
                }
                find_model(id, saved_provider, &catalog, &models_cfg).filter(|m| {
                    model_is_authed_for_resolution(
                        m,
                        &anthropic_credentials,
                        &openai_credentials,
                        cli_api_key.is_some(),
                    )
                })
            });
            if let Some(model) = saved {
                let thinking_level = cli_thinking
                    .or(settings_thinking)
                    .unwrap_or(DEFAULT_THINKING_LEVEL);
                (model, thinking_level)
            } else {
                // (4) Fallback: built-in default if authed, else first authed.
                let thinking_level = cli_thinking.unwrap_or(DEFAULT_THINKING_LEVEL);
                let model = pick_default_model(
                    &catalog,
                    &models_cfg,
                    &anthropic_credentials,
                    &openai_credentials,
                    cli_api_key.is_some(),
                );
                (model, thinking_level)
            }
        }
    };

    // ---- Provider build ----
    let selected_api = model.api.clone();
    let selected_provider = model.provider.clone();
    let selected_anthropic_credential = if matches!(selected_api, rpi_ai::Api::AnthropicMessages) {
        let auth_header = models_cfg
            .providers
            .get(&selected_provider)
            .and_then(|provider| provider.auth_header)
            .unwrap_or(false);
        credential_for_selected_anthropic_provider(
            cli_api_key.as_deref(),
            &anthropic_credentials,
            &selected_provider,
            auth_header,
        )
    } else {
        None
    };
    let selected_openai_key = if matches!(
        selected_api,
        rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses
    ) {
        cli_api_key
            .clone()
            .or_else(|| openai_credential_for(&openai_credentials, &selected_provider).cloned())
    } else {
        None
    };
    let selected_is_authed = model_has_header_auth(&model)
        || match selected_api {
            rpi_ai::Api::AnthropicMessages => selected_anthropic_credential.is_some(),
            rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => {
                selected_openai_key.is_some()
            }
            _ => false,
        };
    if !selected_is_authed {
        return Err(ResolveError::NoApiKey {
            hint: NO_API_KEY_HINT,
        });
    }

    if let Some(AnthropicCredential::Headers(headers)) = &selected_anthropic_credential {
        merge_auth_headers(&mut model, headers);
    }
    if let Some(key) = &selected_openai_key {
        merge_auth_headers(
            &mut model,
            &BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]),
        );
    }
    let mut provider_models: Vec<Model> = catalog
        .into_iter()
        .filter(|candidate| {
            candidate.api == selected_api && candidate.provider == selected_provider
        })
        .collect();
    if let Some(AnthropicCredential::Headers(headers)) = &selected_anthropic_credential {
        for candidate in &mut provider_models {
            merge_auth_headers(candidate, headers);
        }
    }
    if let Some(key) = &selected_openai_key {
        let headers = BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]);
        for candidate in &mut provider_models {
            merge_auth_headers(candidate, &headers);
        }
    }
    // Provider attribution headers (native `mergeProviderAttributionHeaders`):
    // app-identifying defaults that never clobber user/model headers.
    for candidate in &mut provider_models {
        let attribution = crate::attribution::default_attribution_headers(candidate);
        if attribution.is_empty() {
            continue;
        }
        let mut headers = candidate.headers.clone().unwrap_or_default();
        for (key, value) in attribution {
            headers.entry(key).or_insert(value);
        }
        candidate.headers = Some(headers);
    }
    let (provider, has_provider_key): (Arc<dyn Provider>, bool) = match selected_api {
        rpi_ai::Api::AnthropicMessages => {
            let provider_key = selected_anthropic_credential
                .as_ref()
                .and_then(AnthropicCredential::provider_key)
                .map(str::to_string);
            let has_key = provider_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.anthropic.com", &settings);
            let inner = if selected_provider == DEFAULT_PROVIDER_ID
                && !matches!(
                    selected_anthropic_credential,
                    Some(AnthropicCredential::Headers(_))
                ) {
                AnthropicProvider::with_models(provider_key, http, provider_models)
            } else {
                AnthropicProvider::with_models_without_env_api_key(
                    provider_key,
                    http,
                    provider_models,
                )
            };
            (
                Arc::new(NamedAnthropicProvider {
                    id: selected_provider,
                    inner,
                }),
                has_key,
            )
        }
        rpi_ai::Api::OpenaiCompletions => {
            let has_key = selected_openai_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.openai.com/v1", &settings);
            let inner = if selected_provider == "openai" {
                OpenAiCompletionsProvider::with_models(
                    selected_provider,
                    selected_openai_key,
                    http,
                    provider_models,
                )
            } else {
                OpenAiCompletionsProvider::with_models_without_env_api_key(
                    selected_provider,
                    selected_openai_key,
                    http,
                    provider_models,
                )
            };
            (Arc::new(inner), has_key)
        }
        rpi_ai::Api::OpenaiResponses => {
            let has_key = selected_openai_key.is_some();
            let http =
                provider_http_client(&provider_models, "https://api.openai.com/v1", &settings);
            let inner = if selected_provider == "openai" {
                OpenAiResponsesProvider::with_models(
                    selected_provider,
                    selected_openai_key,
                    http,
                    provider_models,
                )
            } else {
                OpenAiResponsesProvider::with_models_without_env_api_key(
                    selected_provider,
                    selected_openai_key,
                    http,
                    provider_models,
                )
            };
            (Arc::new(inner), has_key)
        }
        _ => unreachable!("unsupported APIs are filtered while loading models.json"),
    };

    Ok(ResolvedModel {
        provider,
        model,
        thinking_level,
        has_provider_key,
        theme: settings.theme.clone(),
    })
}

/// The catalog the TUI's `/model` selector displays (read-only). Re-derives the
/// **auth-filtered** snapshot the provider was built from so the selector shows
/// exactly the models that can actually run (mirrors pi `getAvailableSnapshot`:
/// `available = all.filter(m => configuredProviders.has(m.provider))` — v1's
/// single-provider equivalent of "configured" is [`model_is_authed`]).
///
/// The runtime provider already owns one provider-id-specific catalog. The
/// additional auth filter keeps header-only and provider-key paths consistent
/// if a future runtime exposes a broader snapshot.
///
/// On any config read error it falls back to the built-in Anthropic catalog —
/// the selector is non-critical and must never block the TUI from starting.
pub fn available_catalog(resolved: &ResolvedModel) -> Vec<Model> {
    let selected_api = &resolved.model.api;
    let selected_provider = &resolved.model.provider;
    let mut seen = std::collections::HashSet::new();
    resolved
        .provider
        .models()
        .iter()
        // A provider snapshot is expected to be homogeneous, but extension
        // and gateway providers can expose a broader catalog. The selector
        // must only offer models the current lane can actually route to.
        .filter(|m| m.api == *selected_api && m.provider == *selected_provider)
        .filter(|m| model_is_authed(m, resolved.has_provider_key))
        .filter(|m| seen.insert((m.api.clone(), m.provider.clone(), m.id.to_ascii_lowercase())))
        .cloned()
        .collect()
}

/// Return the merged built-in + `models.json` catalog without requiring a
/// credential or selecting a runnable model. This is used by CLI commands
/// such as `--list-models`, which must remain useful before authentication.
pub fn catalog_all() -> Result<Vec<Model>, config::ConfigError> {
    let cfg = config::load_models_config()?;
    let mut catalog = anthropic_models();
    catalog.extend(openai_responses_models());
    merge_user_catalog(&mut catalog, &cfg);
    catalog.sort_by(|a, b| {
        a.provider
            .to_ascii_lowercase()
            .cmp(&b.provider.to_ascii_lowercase())
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });
    Ok(catalog)
}

/// Merge `~/.rpi/models.json` providers into the built-in catalog. Models from
/// the same runtime provider and API replace entries with the same id; models
/// with the same id under different OpenAI-compatible providers remain
/// distinct so `provider/id` can select the intended endpoint.
fn merge_user_catalog(catalog: &mut Vec<Model>, cfg: &config::ModelsConfig) {
    for (provider_id, provider_cfg) in &cfg.providers {
        let Some(models) = config::provider_to_models(provider_id, provider_cfg) else {
            // Unsupported protocol.
            continue;
        };
        for m in models {
            if let Some(existing) = catalog.iter_mut().find(|candidate| {
                candidate.api == m.api
                    && candidate.provider == m.provider
                    && candidate.id.eq_ignore_ascii_case(&m.id)
            }) {
                *existing = m;
            } else {
                catalog.push(m);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AnthropicCredential {
    /// Passed as the selected provider's default `x-api-key`.
    ProviderKey(String),
    /// Folded only onto models owned by the selected provider.
    Headers(BTreeMap<String, String>),
}

impl AnthropicCredential {
    fn provider_key(&self) -> Option<&str> {
        match self {
            Self::ProviderKey(key) => Some(key),
            Self::Headers(_) => None,
        }
    }
}

type AnthropicCredentials = BTreeMap<String, AnthropicCredential>;
type OpenAiCredentials = BTreeMap<String, String>;

/// Resolve one independent Anthropic-wire credential for each provider id.
/// Stored credentials win over `models.json`; only the built-in `anthropic`
/// identity may fall back to the `ANTHROPIC_*` environment variables.
fn resolve_anthropic_credentials(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
) -> AnthropicCredentials {
    let mut provider_ids = vec![DEFAULT_PROVIDER_ID.to_string()];
    for (provider_id, provider_cfg) in &cfg.providers {
        if config::provider_is_anthropic_compatible(provider_cfg)
            && !provider_ids.iter().any(|known| known == provider_id)
        {
            provider_ids.push(provider_id.clone());
        }
    }

    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let provider_cfg = cfg.providers.get(&provider_id);
        let auth_header = provider_cfg
            .and_then(|provider| provider.auth_header)
            .unwrap_or(false);
        let stored_key = stored_api_key(auth_store, &provider_id);
        let configured_key = provider_cfg.and_then(|provider| {
            provider
                .api_key
                .as_deref()
                .filter(|raw| !raw.is_empty())
                .and_then(|raw| config::resolve_config_value(raw, None))
                .filter(|key| !key.is_empty())
        });

        let credential = stored_key
            .or(configured_key)
            .map(|key| anthropic_credential_from_key(key, auth_header))
            .or_else(|| {
                if provider_id != DEFAULT_PROVIDER_ID {
                    return None;
                }
                std::env::var(ANTHROPIC_AUTH_TOKEN_ENV)
                    .ok()
                    .filter(|token| !token.is_empty())
                    .map(|token| {
                        AnthropicCredential::Headers(BTreeMap::from([(
                            "authorization".to_string(),
                            format!("Bearer {token}"),
                        )]))
                    })
                    .or_else(|| {
                        std::env::var(ANTHROPIC_API_KEY_ENV)
                            .ok()
                            .filter(|key| !key.is_empty())
                            .map(|key| anthropic_credential_from_key(key, auth_header))
                    })
            });
        if let Some(credential) = credential {
            credentials.insert(provider_id, credential);
        }
    }
    credentials
}

fn stored_api_key(auth_store: &config::AuthStore, provider_id: &str) -> Option<String> {
    auth_store
        .get(provider_id)
        .and_then(|credential| match credential {
            Credential::ApiKey {
                key: Some(key),
                env,
            } => config::resolve_config_value(key, env.as_ref()).filter(|key| !key.is_empty()),
            _ => None,
        })
}

fn anthropic_credential_from_key(key: String, auth_header: bool) -> AnthropicCredential {
    if auth_header {
        AnthropicCredential::Headers(BTreeMap::from([(
            "authorization".to_string(),
            format!("Bearer {key}"),
        )]))
    } else {
        AnthropicCredential::ProviderKey(key)
    }
}

fn anthropic_credential_for<'a>(
    credentials: &'a AnthropicCredentials,
    provider_id: &str,
) -> Option<&'a AnthropicCredential> {
    credentials.get(provider_id)
}

fn credential_for_selected_anthropic_provider(
    cli_api_key: Option<&str>,
    credentials: &AnthropicCredentials,
    provider_id: &str,
    auth_header: bool,
) -> Option<AnthropicCredential> {
    cli_api_key
        .filter(|key| !key.is_empty())
        .map(|key| anthropic_credential_from_key(key.to_string(), auth_header))
        .or_else(|| anthropic_credential_for(credentials, provider_id).cloned())
}

fn resolve_openai_credentials(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
) -> OpenAiCredentials {
    let mut provider_ids = vec!["openai".to_string()];
    for (provider_id, provider_cfg) in &cfg.providers {
        if (config::provider_is_openai_completions(provider_cfg)
            || config::provider_is_openai_responses(provider_cfg))
            && !provider_ids.iter().any(|known| known == provider_id)
        {
            provider_ids.push(provider_id.clone());
        }
    }

    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let provider_cfg = cfg.providers.get(&provider_id);
        let key = stored_api_key(auth_store, &provider_id)
            .or_else(|| configured_openai_api_key(&provider_id, provider_cfg));
        if let Some(key) = key {
            credentials.insert(provider_id, key);
        }
    }
    credentials
}

fn configured_openai_api_key(
    provider_id: &str,
    provider_cfg: Option<&config::ProviderConfig>,
) -> Option<String> {
    if let Some(provider_cfg) = provider_cfg {
        return config::openai_provider_api_key(provider_id, provider_cfg);
    }

    (provider_id == "openai")
        .then(|| std::env::var(OPENAI_API_KEY_ENV).ok())
        .flatten()
        .filter(|value| !value.is_empty())
}

fn openai_credential_for<'a>(
    credentials: &'a OpenAiCredentials,
    provider_id: &str,
) -> Option<&'a String> {
    credentials.get(provider_id)
}

fn merge_auth_headers(model: &mut Model, auth_headers: &BTreeMap<String, String>) {
    let headers = model.headers.get_or_insert_with(BTreeMap::new);
    for (name, value) in auth_headers {
        headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        headers.insert(name.clone(), value.clone());
    }
}

struct SplitModelPattern {
    provider: Option<String>,
    model_id: String,
    raw_model_id: String,
    thinking: Option<ThinkingLevel>,
    inferred_provider: bool,
}

/// Split a `--model` value into its provider, id, and thinking components.
///
/// A slash prefix is provider syntax only when it names a known provider (or
/// matches an explicit `--provider`). Unknown prefixes stay in the raw model
/// id, which is required for OpenRouter-style ids such as
/// `meta-llama/llama-3.3`.
///
/// Mirrors the TS `parseModelPattern` last-colon split + recurse-on-prefix.
fn split_model_pattern(
    value: &str,
    cli_provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
) -> Result<SplitModelPattern, ResolveError> {
    let mut provider = cli_provider.map(str::to_string);
    let mut model_id = value.to_string();
    let mut inferred_provider = false;

    if let Some((prefix, remainder)) = value.split_once('/') {
        if !prefix.is_empty() && !remainder.is_empty() {
            let matches_explicit = cli_provider
                .and_then(|requested| {
                    canonicalize_cli_provider(prefix, cfg)
                        .ok()
                        .map(|canonical| canonical == requested)
                })
                .unwrap_or(false);
            if matches_explicit {
                model_id = remainder.to_string();
            } else if cli_provider.is_none() {
                match canonicalize_cli_provider(prefix, cfg) {
                    Ok(canonical) => {
                        provider = Some(canonical);
                        model_id = remainder.to_string();
                        inferred_provider = true;
                    }
                    Err(ResolveError::UnknownProvider(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    // Native Pi attempts the complete id first. Only peel a valid thinking
    // suffix when that complete id does not exist in the selected scope.
    let has_full_exact_match = catalog.iter().any(|model| {
        model.id.eq_ignore_ascii_case(&model_id)
            && provider
                .as_deref()
                .map_or(true, |requested| provider_matches(model, requested, cfg))
    });
    let thinking = if has_full_exact_match {
        None
    } else if let Some((head, suffix)) = model_id.rsplit_once(':') {
        if let Some(level) = parse_thinking_level(suffix) {
            model_id = head.to_string();
            Some(level)
        } else {
            None
        }
    } else {
        None
    };

    Ok(SplitModelPattern {
        provider,
        model_id,
        raw_model_id: value.to_string(),
        thinking,
        inferred_provider,
    })
}

/// Case-insensitive exact id match, optionally scoped to a provider.
fn find_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
) -> Option<Model> {
    catalog
        .iter()
        .find(|model| {
            model.id.eq_ignore_ascii_case(pattern)
                && provider.map_or(true, |requested| provider_matches(model, requested, cfg))
        })
        .cloned()
}

/// Resolve an exact CLI model reference without silently choosing the first
/// provider when a bare model id is duplicated. This mirrors native Pi's
/// `resolveCliModel`: a sole configured-auth match wins; zero or multiple
/// configured-auth matches require an explicit provider.
fn find_cli_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
) -> Result<Option<Model>, ResolveError> {
    let exact_matches: Vec<&Model> = catalog
        .iter()
        .filter(|model| {
            model.id.eq_ignore_ascii_case(pattern)
                && provider.map_or(true, |requested| provider_matches(model, requested, cfg))
        })
        .collect();

    match exact_matches.as_slice() {
        [] => Ok(None),
        [model] => Ok(Some((*model).clone())),
        _ => {
            let authenticated: Vec<&Model> = exact_matches
                .iter()
                .copied()
                .filter(|model| {
                    model_is_authed_for_resolution(
                        model,
                        anthropic_credentials,
                        openai_credentials,
                        false,
                    )
                })
                .collect();
            if let [model] = authenticated.as_slice() {
                return Ok(Some((*model).clone()));
            }

            let mut matches = exact_matches
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>();
            matches.sort();
            let auth_hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            Err(ResolveError::AmbiguousModel {
                pattern: pattern.to_string(),
                matches: matches.join(", "),
                auth_hint,
            })
        }
    }
}

fn provider_is_known(requested: &str, cfg: &config::ModelsConfig) -> bool {
    cfg.providers.contains_key(requested)
        || requested.eq_ignore_ascii_case("anthropic")
        || requested.eq_ignore_ascii_case("openai")
        || requested.eq_ignore_ascii_case("openai-completions")
        || requested.eq_ignore_ascii_case("openai-responses")
}

/// Canonicalize user-facing `--provider` input without weakening provider
/// identity. Exact configured ids win. A case-insensitive custom match is
/// accepted only when unique; otherwise the user must provide exact casing so
/// credentials and endpoints can never cross between distinct ids.
fn canonicalize_cli_provider(
    requested: &str,
    cfg: &config::ModelsConfig,
) -> Result<String, ResolveError> {
    if cfg.providers.contains_key(requested) {
        return Ok(requested.to_string());
    }

    const BUILTIN_PROVIDER_IDS: &[&str] = &[
        "anthropic",
        "openai",
        "openai-completions",
        "openai-responses",
    ];
    if BUILTIN_PROVIDER_IDS.contains(&requested) {
        return Ok(requested.to_string());
    }

    let mut matches = cfg
        .providers
        .keys()
        .filter(|provider| provider.eq_ignore_ascii_case(requested))
        .cloned()
        .collect::<Vec<_>>();
    matches.extend(
        BUILTIN_PROVIDER_IDS
            .iter()
            .filter(|provider| provider.eq_ignore_ascii_case(requested))
            .map(|provider| (*provider).to_string()),
    );
    matches.sort();
    matches.dedup();

    match matches.as_slice() {
        [provider] => return Ok(provider.clone()),
        [] => {}
        _ => {
            return Err(ResolveError::AmbiguousProvider {
                requested: requested.to_string(),
                matches: matches.join(", "),
            });
        }
    }

    Err(ResolveError::UnknownProvider(requested.to_string()))
}

fn provider_matches(model: &Model, requested: &str, cfg: &config::ModelsConfig) -> bool {
    // Configured provider ids are exact identities in native Pi. An exact
    // config match takes precedence over the case-insensitive built-in/protocol
    // aliases below, so a custom `Anthropic` remains distinct from `anthropic`.
    if model.provider == requested {
        return true;
    }
    if cfg.providers.contains_key(requested) {
        return false;
    }
    if requested.eq_ignore_ascii_case("anthropic") {
        return model.provider == DEFAULT_PROVIDER_ID;
    }
    if requested.eq_ignore_ascii_case("openai") {
        return model.provider == "openai";
    }
    if requested.eq_ignore_ascii_case("openai-completions") {
        return matches!(model.api, rpi_ai::Api::OpenaiCompletions);
    }
    if requested.eq_ignore_ascii_case("openai-responses") {
        return matches!(model.api, rpi_ai::Api::OpenaiResponses);
    }
    false
}

/// Provider identity matching for native Pi's default-model table. Unlike the
/// CLI matcher, this intentionally does not treat `openai` as a protocol alias:
/// a default belonging to OpenAI must not select the same model id from an
/// unrelated OpenAI-compatible gateway.
fn model_belongs_to_default_provider(
    model: &Model,
    requested: &str,
    _cfg: &config::ModelsConfig,
) -> bool {
    model.provider == requested
}

/// Whether a catalog model is "configured-auth" — i.e. the request built for it
/// would pass `assertRequestAuth` and not return "No API key". Mirrors the TS
/// `hasConfiguredAuth(providerId)` filter that `getAvailableSnapshot()` applies
/// (`available = all.filter(m => configuredProviders.has(m.provider))`).
///
/// The selected provider snapshot is identity-homogeneous. A model is runnable
/// when its own headers carry auth or that selected provider has a default key.
fn model_is_authed(m: &Model, has_provider_key: bool) -> bool {
    model_has_header_auth(m) || has_provider_key
}

fn model_is_authed_for_resolution(
    model: &Model,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
) -> bool {
    model_has_header_auth(model)
        || has_cli_key
        || match model.api {
            rpi_ai::Api::AnthropicMessages => {
                anthropic_credential_for(anthropic_credentials, &model.provider).is_some()
            }
            rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => {
                openai_credential_for(openai_credentials, &model.provider).is_some()
            }
            _ => false,
        }
}

/// Same three-name check as rpi-ai's `has_header_auth`, but called from the
/// CLI layer (rpi-ai's `has_header_auth` is private to the provider module, so
/// we mirror it here over the model's `headers` map).
fn model_has_header_auth(m: &Model) -> bool {
    let Some(h) = &m.headers else { return false };
    const NAMES: &[&str] = &["authorization", "x-api-key", "cf-aig-authorization"];
    h.keys()
        .any(|k| NAMES.contains(&k.to_ascii_lowercase().as_str()))
}

/// Choose the default model when `--model` is absent. Mirrors upstream
/// `findInitialModel` [`packages/coding-agent/src/core/model-resolver.ts`]:
/// known-provider defaults are checked in native declaration order, followed by
/// the first authenticated model in the catalog (`availableModels[0]`). This
/// also keeps a models.json-only gateway from accidentally selecting an
/// unauthenticated built-in model.
///
/// Credential maps are keyed by provider id, so only a credential belonging to
/// the candidate model can make it participate in default selection.
fn pick_default_model(
    catalog: &[Model],
    models_cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
) -> Model {
    // 1. Native Pi's known-provider defaults, in its declared priority order.
    for (provider, model_id) in DEFAULT_MODELS_PER_PROVIDER {
        if let Some(model) = catalog.iter().find(|model| {
            model.id.eq_ignore_ascii_case(model_id)
                && model_belongs_to_default_provider(model, provider, models_cfg)
                && model_is_authed_for_resolution(
                    model,
                    anthropic_credentials,
                    openai_credentials,
                    has_cli_key,
                )
        }) {
            return model.clone();
        }
    }

    // 2. First authed model (TS `availableModels[0]`). Provider and model array
    //    declaration order is preserved while loading models.json.
    //    this is the gateway model (Bearer folded onto it, base_url = gateway).
    if let Some(m) = catalog.iter().find(|m| {
        model_is_authed_for_resolution(m, anthropic_credentials, openai_credentials, has_cli_key)
    }) {
        return m.clone();
    }
    // 3. Last resort: the built-in Anthropic default, authed or not. The auth gate above
    //    already errored when no source resolved, so reaching here means *some*
    //    auth exists but none folded/attached to a model we can see — keep the
    //    historical default to avoid a NoMatch surprise.
    catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(DEFAULT_MODEL_ID))
        .or_else(|| catalog.first())
        .expect("catalog is never empty (built-in anthropic_models)")
        .clone()
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
        prev_openai_key: Option<std::ffi::OsString>,
        prev_dir: Option<std::ffi::OsString>,
        _tmp: tempfile::TempDir,
    }
    impl TestEnv {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
            let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
            let prev_openai_key = std::env::var_os(OPENAI_API_KEY_ENV);
            let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
            std::env::remove_var(ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
            std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
            std::env::remove_var(OPENAI_API_KEY_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                prev_key,
                prev_tok,
                prev_base,
                prev_openai_key,
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
            restore(OPENAI_API_KEY_ENV, self.prev_openai_key.take());
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
    fn default_model_matches_native_anthropic_default() {
        let r = resolve_with_key(None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        assert_eq!(r.thinking_level, DEFAULT_THINKING_LEVEL);
        assert_eq!(r.provider.id(), "anthropic");
    }

    #[test]
    fn settings_default_model_wins_when_authed() {
        // A copied pi `settings.json` carrying `defaultModel` (step 3 of pi's
        // `findInitialModel`) overrides the built-in Anthropic default
        // when that model is in the catalog and authed. Mirrors the on-disk-
        // parity goal: drop a `.pi/agent/` dir at `~/.rpi/agent/` and the saved
        // default comes alive on launch (no `--model` needed).
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let path = config::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"defaultProvider":"anthropic","defaultModel":"claude-haiku-4-5","defaultThinkingLevel":"high"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "claude-haiku-4-5");
        assert_eq!(r.thinking_level, ThinkingLevel::High);
        // An unauthed saved default (unknown id) falls through to the built-in.
        std::fs::write(&path, r#"{"defaultModel":"claude-does-not-exist"}"#).unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
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
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "official-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{ "providers": { "gateway": { "baseUrl": "https://gw", "apiKey": "gateway-key", "models": [{"id":"custom-claude"}] } } }"#,
        )
        .unwrap();
        let r = resolve(None, Some("gateway/custom-claude"), None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
    }

    #[test]
    fn unknown_slash_prefix_remains_part_of_the_raw_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [{"id":"meta-llama/llama-3.3"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("meta-llama/llama-3.3:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "meta-llama/llama-3.3");
        assert_eq!(resolved.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn complete_model_id_wins_before_parsing_a_thinking_suffix() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [
        {"id":"vendor/model"},
        {"id":"vendor/model:high"}
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("gateway/vendor/model:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "vendor/model:high");
        assert_eq!(resolved.thinking_level, DEFAULT_THINKING_LEVEL);
    }

    #[test]
    fn authenticated_inferred_provider_beats_a_matching_raw_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "alpha");
        assert_eq!(resolved.model.id, "target");
        assert_eq!(resolved.model.base_url, "https://alpha.example.com");
    }

    #[test]
    fn authenticated_raw_model_beats_an_unauthenticated_inferred_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "alpha/target");
        assert_eq!(resolved.model.base_url, "https://gateway.example.com");
    }

    #[test]
    fn thinking_suffix_raw_model_beats_an_unauthenticated_inferred_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("alpha/target:high"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "gateway");
        assert_eq!(resolved.model.id, "alpha/target");
        assert_eq!(resolved.model.base_url, "https://gateway.example.com");
        assert_eq!(resolved.thinking_level, ThinkingLevel::High);
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

        let r = resolve_with_key(Some("ANTHROPIC"), Some("claude-sonnet-5"), None).unwrap();
        assert_eq!(r.provider.id(), DEFAULT_PROVIDER_ID);
    }

    #[test]
    fn unknown_provider_rejected() {
        let err = resolve_with_key(Some("unsupported-provider"), None, None).unwrap_err();
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
            Credential::ApiKey {
                key: Some("stored-key".into()),
                env: None,
            },
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
        // x-api-key path: no Bearer header folded onto the model (auth rides on
        // the provider's default key, surfaced to the provider at build time).
        assert!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .is_none(),
            "x-api-key path should not synthesize a Bearer header"
        );
    }

    #[test]
    fn gateway_models_key_wins_over_unrelated_anthropic_auth() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("official-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("gateway/gateway-model"), None, None, None).unwrap();
        assert_eq!(resolved.provider.id(), "gateway");
        assert_eq!(resolved.model.provider, "gateway");
        assert!(resolved.has_provider_key);

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, DEFAULT_PROVIDER_ID),
            Some(&AnthropicCredential::ProviderKey("official-key".into()))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::ProviderKey("gateway-key".into()))
        );
    }

    #[test]
    fn gateway_auth_json_credential_is_usable() {
        let _env = TestEnv::new();
        config::upsert_credential(
            "gateway",
            Credential::ApiKey {
                key: Some("stored-gateway-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(Some("gateway"), None, None, None, None).unwrap();
        assert_eq!(resolved.model.id, "gateway-model");
        assert_eq!(resolved.provider.id(), "gateway");
        assert!(resolved.has_provider_key);
    }

    #[test]
    fn anthropic_auth_does_not_authenticate_an_unkeyed_gateway() {
        let _env = TestEnv::new();
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("official-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, ResolveError::NoApiKey { .. }));

        let default = resolve(None, None, None, None, None).unwrap();
        assert_eq!(default.provider.id(), DEFAULT_PROVIDER_ID);
        assert_eq!(default.model.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn two_anthropic_gateways_keep_credentials_isolated_by_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "beta-key",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, "alpha"),
            Some(&AnthropicCredential::ProviderKey("alpha-key".into()))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "beta"),
            Some(&AnthropicCredential::ProviderKey("beta-key".into()))
        );
        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(beta.provider.id(), "beta");
        assert!(alpha.has_provider_key && beta.has_provider_key);
    }

    #[test]
    fn case_distinct_provider_ids_keep_endpoints_and_credentials_isolated() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://lower.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    },
    "ALPHA": {
      "api": "anthropic-messages",
      "baseUrl": "https://upper.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();
        config::upsert_credential(
            "alpha",
            Credential::ApiKey {
                key: Some("lower-key".into()),
                env: None,
            },
        )
        .unwrap();
        config::upsert_credential(
            "ALPHA",
            Credential::ApiKey {
                key: Some("upper-key".into()),
                env: None,
            },
        )
        .unwrap();

        let lower = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(lower.provider.id(), "alpha");
        assert_eq!(lower.model.base_url, "https://lower.example.com");
        assert_eq!(
            lower
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer lower-key")
        );

        let upper = resolve(None, Some("ALPHA/shared-model"), None, None, None).unwrap();
        assert_eq!(upper.provider.id(), "ALPHA");
        assert_eq!(upper.model.base_url, "https://upper.example.com");
        assert_eq!(
            upper
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer upper-key")
        );

        let error = resolve(Some("Alpha"), None, None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "Alpha" && matches == "ALPHA, alpha"
        ));

        let prefix_error = resolve(None, Some("Alpha/shared-model"), None, None, None).unwrap_err();
        assert!(matches!(
            prefix_error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "Alpha" && matches == "ALPHA, alpha"
        ));
    }

    #[test]
    fn cli_provider_case_insensitively_selects_one_canonical_custom_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://routeryo-copy.example.com",
      "apiKey": "copy-key",
      "models": [{"id":"copy-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved =
            resolve(Some("ROUTERYO-COPY"), Some("copy-model"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "routeryo-copy");
        assert_eq!(resolved.provider.id(), "routeryo-copy");
        assert_eq!(resolved.model.base_url, "https://routeryo-copy.example.com");

        let prefixed = resolve(None, Some("ROUTERYO-COPY/copy-model"), None, None, None).unwrap();
        assert_eq!(prefixed.model.provider, "routeryo-copy");
        assert_eq!(prefixed.model.id, "copy-model");
    }

    #[test]
    fn cli_provider_canonicalization_keeps_builtin_and_custom_case_ids_distinct() {
        let _env = TestEnv::new();
        std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "OpenAI": {
      "api": "openai-completions",
      "baseUrl": "https://custom-openai.example.com",
      "apiKey": "custom-openai-key",
      "models": [{"id":"custom-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let custom = resolve(Some("OpenAI"), Some("custom-model"), None, None, None).unwrap();
        assert_eq!(custom.model.provider, "OpenAI");
        assert_eq!(custom.model.base_url, "https://custom-openai.example.com");

        let builtin = resolve(Some("openai"), Some("gpt-6-astra"), None, None, None).unwrap();
        assert_eq!(builtin.model.provider, "openai");

        let error = resolve(Some("OPENAI"), None, None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousProvider {
                requested,
                matches
            } if requested == "OPENAI" && matches == "OpenAI, openai"
        ));
    }

    #[test]
    fn cli_api_key_overrides_only_the_selected_anthropic_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "authHeader": true,
      "apiKey": "configured-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            credential_for_selected_anthropic_provider(
                Some("cli-key"),
                &credentials,
                "gateway",
                true,
            ),
            Some(AnthropicCredential::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer cli-key".into(),
            )])))
        );
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer configured-key".into(),
            )])))
        );

        let resolved = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            Some("cli-key"),
            None,
        )
        .unwrap();
        assert!(!resolved.has_provider_key);
        assert_eq!(
            resolved
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer cli-key")
        );
    }

    #[test]
    fn auth_token_routes_via_bearer_header() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_AUTH_TOKEN_ENV, "tok-123");
        let r = resolve(None, None, None, None, None).unwrap();
        // No provider key carries auth — it lives on the model header.
        let headers = r.model.headers.as_ref().expect("bearer header on model");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer tok-123")
        );
        // ANTHROPIC_AUTH_TOKEN is a *global* credential (not endpoint-specific
        // like a models.json gateway key): the default claude-sonnet-5 is picked
        // (it carries the env Bearer) — NOT a gateway model.
        assert_eq!(r.model.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn api_key_flag_beats_env_and_stored() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "env-key");
        config::upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("stored-key".into()),
                env: None,
            },
        )
        .unwrap();
        // `--api-key flag-key` wins; resolve succeeds + takes the x-api-key path
        // (no Bearer header on the model).
        let r = resolve(None, None, None, Some("flag-key"), None).unwrap();
        assert!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .is_none(),
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
    fn anthropic_base_url_env_does_not_redirect_a_custom_provider() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://ambient-proxy.example.com");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-secret",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let r = resolve(
            Some("gateway"),
            Some("gateway/gateway-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.model.base_url, "https://gateway.example.com");
        assert!(r.has_provider_key);
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
        assert_eq!(r.model.provider, "gateway");
        assert_eq!(r.provider.id(), "gateway");
        // Provider-level authHeader folded in.
        let headers = r.model.headers.as_ref().expect("headers merged");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    #[test]
    fn openai_completions_models_json_is_a_complete_provider_config() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo": {
      "baseUrl": "https://api.routeryo.com",
      "api": "openai-completions",
      "apiKey": "router-secret",
      "models": [
        {
          "id": "gpt-5.6-sol",
          "name": "GPT 5.6",
          "reasoning": true,
          "contextWindow": 200000,
          "maxTokens": 32768
        }
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.id, "gpt-5.6-sol");
        assert_eq!(resolved.model.api, rpi_ai::Api::OpenaiCompletions);
        assert_eq!(resolved.model.provider, "routeryo");
        assert_eq!(resolved.provider.id(), "routeryo");
        assert!(resolved.has_provider_key);
        assert_eq!(
            resolved
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer router-secret")
        );

        let explicit = resolve(
            Some("routeryo"),
            Some("routeryo/gpt-5.6-sol"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(explicit.provider.id(), "routeryo");
        assert_eq!(explicit.model.id, "gpt-5.6-sol");
    }

    #[test]
    fn openai_completions_provider_alias_excludes_responses_models() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(Some("openai-completions"), None, None, None, None).unwrap();
        assert_eq!(resolved.model.api, rpi_ai::Api::OpenaiCompletions);
        assert_eq!(resolved.model.provider, "chat-gateway");
        assert_eq!(resolved.model.id, "chat-model");
    }

    #[test]
    fn openai_model_prefix_disambiguates_providers_with_the_same_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(alpha.model.base_url, "https://alpha.example.com");
        assert_eq!(
            alpha
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer alpha-secret")
        );

        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(beta.provider.id(), "beta");
        assert_eq!(beta.model.base_url, "https://beta.example.com");
        assert_eq!(
            beta.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer beta-secret")
        );
    }

    #[test]
    fn bare_duplicate_model_id_prefers_the_only_authenticated_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, Some("shared-model"), None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "beta");
        assert_eq!(resolved.model.base_url, "https://beta.example.com");
    }

    #[test]
    fn bare_duplicate_model_id_rejects_multiple_authenticated_providers() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
        match error {
            ResolveError::AmbiguousModel {
                pattern,
                matches,
                auth_hint,
            } => {
                assert_eq!(pattern, "shared-model");
                assert_eq!(matches, "alpha/shared-model, beta/shared-model");
                assert_eq!(
                    auth_hint,
                    "More than one matching provider is authenticated."
                );
            }
            other => panic!("expected AmbiguousModel, got {other:?}"),
        }
    }

    #[test]
    fn bare_duplicate_model_id_rejects_when_no_provider_is_authenticated() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::AmbiguousModel {
                auth_hint: "No matching provider is authenticated.",
                ..
            }
        ));
    }

    #[test]
    fn openai_auth_json_overrides_config_for_both_protocols() {
        let _env = TestEnv::new();
        config::upsert_credential(
            "chat-gateway",
            Credential::ApiKey {
                key: Some("stored-chat-key".into()),
                env: None,
            },
        )
        .unwrap();
        config::upsert_credential(
            "responses-gateway",
            Credential::ApiKey {
                key: Some("stored-responses-key".into()),
                env: None,
            },
        )
        .unwrap();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let chat = resolve(
            Some("chat-gateway"),
            Some("chat-gateway/chat-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(chat.has_provider_key);
        assert_eq!(
            chat.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer stored-chat-key")
        );

        let responses = resolve(
            Some("responses-gateway"),
            Some("responses-gateway/responses-model"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(responses.has_provider_key);
        assert_eq!(
            responses
                .model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer stored-responses-key")
        );
    }

    #[test]
    fn openai_env_does_not_authenticate_custom_providers() {
        let _env = TestEnv::new();
        std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "models": [{"id":"responses-model"}]
    },
    "openai-completions": {
      "api": "openai-completions",
      "baseUrl": "https://alias-chat.example.com/v1",
      "models": [{"id":"alias-chat-model"}]
    },
    "openai-responses": {
      "api": "openai-responses",
      "baseUrl": "https://alias-responses.example.com/v1",
      "models": [{"id":"alias-responses-model"}]
    },
    "OpenAI": {
      "api": "openai-responses",
      "baseUrl": "https://case-distinct.example.com/v1",
      "models": [{"id":"case-distinct-model"}]
    }
  }
}"#,
        )
        .unwrap();

        for (provider, model) in [
            ("chat-gateway", "chat-gateway/chat-model"),
            ("responses-gateway", "responses-gateway/responses-model"),
            ("openai-completions", "openai-completions/alias-chat-model"),
            ("openai-responses", "openai-responses/alias-responses-model"),
            ("OpenAI", "OpenAI/case-distinct-model"),
        ] {
            let error = resolve(Some(provider), Some(model), None, None, None).unwrap_err();
            assert!(matches!(error, ResolveError::NoApiKey { .. }));
        }

        let official = resolve(None, None, None, None, None).unwrap();
        assert_eq!(official.provider.id(), "openai");
        assert_eq!(official.model.id, "gpt-6-astra");
        assert!(official.has_provider_key);
    }

    #[test]
    fn cli_api_key_overrides_selected_openai_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
        )
        .unwrap();

        for (provider, model) in [
            ("chat-gateway", "chat-gateway/chat-model"),
            ("responses-gateway", "responses-gateway/responses-model"),
        ] {
            let resolved =
                resolve(Some(provider), Some(model), None, Some("cli-key"), None).unwrap();
            assert_eq!(resolved.provider.id(), provider);
            assert!(resolved.has_provider_key);
            assert_eq!(
                resolved
                    .model
                    .headers
                    .as_ref()
                    .and_then(|headers| headers.get("authorization"))
                    .map(String::as_str),
                Some("Bearer cli-key")
            );
        }
    }

    #[test]
    fn anthropic_model_prefix_disambiguates_providers_with_the_same_model_id() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://beta.example.com",
      "authHeader": true,
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
        )
        .unwrap();

        let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
        assert_eq!(alpha.provider.id(), "alpha");
        assert_eq!(alpha.model.provider, "alpha");
        assert_eq!(alpha.model.base_url, "https://alpha.example.com");
        assert!(alpha.has_provider_key);
        assert!(alpha.model.headers.as_ref().map_or(true, |headers| {
            headers
                .keys()
                .all(|name| !name.eq_ignore_ascii_case("x-api-key"))
        }));

        let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
        assert_eq!(beta.provider.id(), "beta");
        assert_eq!(beta.model.provider, "beta");
        assert_eq!(beta.model.base_url, "https://beta.example.com");
        assert_eq!(
            beta.model
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer beta-secret")
        );
    }

    #[test]
    fn trusted_project_defaults_override_global_defaults_for_resolution() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "global": {
      "api": "anthropic-messages",
      "apiKey": "global-secret",
      "models": [{"id":"global-model"}]
    },
    "project": {
      "api": "anthropic-messages",
      "apiKey": "project-secret",
      "models": [{"id":"project-model"}]
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"global","defaultModel":"global-model"}"#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".rpi")).unwrap();
        std::fs::write(
            project.path().join(".rpi/settings.json"),
            r#"{"defaultProvider":"project","defaultModel":"project-model"}"#,
        )
        .unwrap();

        let trusted = resolve_for_cwd(None, None, None, None, None, project.path(), true).unwrap();
        assert_eq!(trusted.provider.id(), "project");
        assert_eq!(trusted.model.id, "project-model");

        let untrusted =
            resolve_for_cwd(None, None, None, None, None, project.path(), false).unwrap();
        assert_eq!(untrusted.provider.id(), "global");
        assert_eq!(untrusted.model.id, "global-model");
    }

    #[test]
    fn unknown_model_prefix_without_a_raw_match_returns_no_match() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo": {
      "api": "openai-completions",
      "apiKey": "secret",
      "models": [{"id":"gpt-test"}]
    }
  }
}"#,
        )
        .unwrap();

        let error = resolve(None, Some("misspelled/gpt-test"), None, None, None).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::NoMatch { pattern, .. } if pattern == "misspelled/gpt-test"
        ));
    }

    /// A models.json gateway with `authHeader:true` + `apiKey` is itself an auth
    /// source — it satisfies the `resolve` auth gate WITHOUT any env var, stored
    /// cred, or `--api-key`. This is the "models.json file alone sets up a
    /// third-party endpoint" path. The Bearer folds onto the gateway model only
    /// (built-in claude-* stays Bearer-less), and — with no `--model` — the
    /// default selector picks that gateway model (the only authed one).
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
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    /// The `--api-key` flag wins over a models.json `authHeader:true` gateway
    /// key while preserving the provider's Bearer transport contract.
    /// A `models.json`-only gateway config (no `--model`, no env, no auth.json)
    /// should pick the gateway model by default — mirroring the TS
    /// `findInitialModel` step-4 fallback `availableModels[0]` over the
    /// auth-filtered snapshot. The built-in Anthropic models carry no auth in a
    /// gateway-only setup, so the gateway model is the first (and only)
    /// authenticated model. This is the `rpi -p hi` (no `--model`) case.
    #[test]
    fn default_prefers_gateway_when_only_gateway_configured() {
        // TestEnv already holds the shared env_lock for its whole lifetime —
        // don't take it again here (would self-deadlock and poison the mutex).
        let _env = TestEnv::new();
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
        // No --model (None): the default selector must pick the gateway model,
        // NOT the built-in claude-sonnet-5 (which would carry a foreign Bearer
        // to api.anthropic.com → 401, the bug this fixes).
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        // Gateway model carries the folded Bearer.
        let headers = r.model.headers.as_ref().expect("bearer on gateway model");
        assert_eq!(
            headers.get("authorization").map(|s| s.as_str()),
            Some("Bearer gw-secret")
        );
    }

    #[test]
    fn api_key_flag_honors_models_json_auth_header() {
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
        assert_eq!(
            r.model
                .headers
                .as_ref()
                .and_then(|h| h.get("authorization"))
                .map(String::as_str),
            Some("Bearer flag-key")
        );
        assert!(!r.has_provider_key);
    }

    /// A models.json gateway with a **bare** `apiKey` (no `authHeader`) is the
    /// `composeApiKeyAuth` arm — it satisfies the `resolve` auth gate WITHOUT
    /// any env var, stored cred, or `--api-key`, routing the resolved key as
    /// the selected provider's default `x-api-key`. It is not copied into model
    /// headers, so a different provider can never inherit it. This is the
    /// default copied-pi `models.json` shape.
    #[test]
    fn models_json_bare_apikey_satisfies_auth_without_env() {
        let _env = TestEnv::new();
        // No ANTHROPIC_* env, no auth.json — only the bare-apiKey models.json gateway.
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
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
        assert!(r.has_provider_key);
        assert!(r.model.headers.as_ref().map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));
    }

    /// The bare-`apiKey` x-api-key fold is endpoint-specific: with no `--model`,
    /// the default selector must pick the gateway model (the only authed one),
    // NOT the built-in claude-sonnet-5 — which would carry a gateway x-api-key to
    // api.anthropic.com → 401, the same misrouting the Bearer fold guards
    // against. This is the `rpi -p hi` (no `--model`) case for a bare-apiKey
    /// gateway.
    #[test]
    fn default_prefers_gateway_when_only_bare_apikey_configured() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        // No --model (None): the default selector must pick the gateway model.
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "custom-claude");
        assert_eq!(r.model.base_url, "https://gw.example.com");
        assert!(r.has_provider_key);
    }

    /// A bare `apiKey` that references an unset env var resolves to `None` and
    /// is skipped (mirrors pi `resolveConfigValue` semantics) — the auth gate
    /// falls through to the env/`rpi auth login` sources rather than partially
    /// authenticating with an empty key.
    #[test]
    fn models_json_bare_apikey_env_template_resolves() {
        let _env = TestEnv::new();
        // Prime the env var the apiKey references.
        std::env::set_var("RPI_TEST_GATEWAY_KEY", "env-resolved-secret");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "$RPI_TEST_GATEWAY_KEY",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
        assert!(r.has_provider_key);
        let cfg = config::load_models_config().unwrap();
        let credentials = resolve_anthropic_credentials(&cfg, &config::read_auth().unwrap());
        assert_eq!(
            anthropic_credential_for(&credentials, "gateway"),
            Some(&AnthropicCredential::ProviderKey(
                "env-resolved-secret".into()
            ))
        );
        std::env::remove_var("RPI_TEST_GATEWAY_KEY");
    }

    /// `authHeader: true` takes precedence over a bare `apiKey` on the SAME or a
    /// later provider: the Bearer step (3a) runs before the bare-apiKey step
    /// A models.json with BOTH auth shapes — `authHeader:true` and bare
    /// `apiKey` — routes each provider's credential onto ITS OWN models
    /// (per-provider fold, mirroring upstream `composeApiKeyAuth`): the
    /// authHeader provider's key becomes `Authorization: Bearer` on its model,
    /// the bare-apiKey provider's key becomes `x-api-key` on its model. A
    /// copied pi models.json mixing both shapes works end-to-end — no model
    /// ends up unauthenticated because another provider "won" the gate.
    #[test]
    fn auth_header_provider_and_bare_apikey_provider_each_fold_their_own() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "bearer-gw": {
      "baseUrl": "https://bearer.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "bearer-secret",
      "models": [ { "id": "bearer-model" } ]
    },
    "xkey-gw": {
      "baseUrl": "https://xkey.example.com",
      "api": "anthropic-messages",
      "apiKey": "xkey-secret",
      "models": [ { "id": "xkey-model" } ]
    }
  }
}"#,
        )
        .unwrap();
        // Both providers satisfy the auth gate together (no env / stored cred
        // needed); the default selector picks the first authed model.
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "bearer-model");

        // bearer-gw's key folds as Bearer onto bearer-model only.
        let r = resolve(None, Some("bearer-model"), None, None, None).unwrap();
        let h = r.model.headers.as_ref().expect("bearer folded");
        assert_eq!(
            h.get("authorization").map(|s| s.as_str()),
            Some("Bearer bearer-secret")
        );
        assert!(
            h.get("x-api-key").is_none(),
            "authHeader path must not synthesize x-api-key"
        );

        // xkey-gw's bare apiKey becomes only that provider's default key.
        let r2 = resolve(None, Some("xkey-model"), None, None, None).unwrap();
        assert!(r2.has_provider_key);
        assert!(r2.model.headers.as_ref().map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));

        // The active provider exposes only its own catalog. Selecting the
        // other provider explicitly creates a separately keyed provider.
        let catalog = available_catalog(&r);
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["bearer-model"]);
        let other_catalog = available_catalog(&r2);
        let other_ids: Vec<&str> = other_catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(other_ids, vec!["xkey-model"]);
    }

    /// A copied pi settings.json whose `defaultProvider` names a **models.json
    /// gateway** (not "anthropic") must still honor the saved `defaultModel` —
    /// pi's `findInitialModel` step-3 applies `defaultModelPerProvider`
    /// regardless of provider id. Without this, enabling a second gateway
    /// flips the no-`--model` default to the FIRST authed model in catalog
    /// order, not the user's saved choice.
    #[test]
    fn settings_default_model_honored_for_models_json_provider() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "beta-gw": {
      "baseUrl": "https://beta.example.com",
      "api": "anthropic-messages",
      "apiKey": "beta-secret",
      "models": [ { "id": "beta-model" } ]
    },
    "alpha-gw": {
      "baseUrl": "https://alpha.example.com",
      "api": "anthropic-messages",
      "apiKey": "alpha-secret",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
        )
        .unwrap();
        // Saved default points at the ALPHA gateway's model even though
        // "beta-gw" is declared first and would win first-authed without the
        // settings arm, matching native Pi's Object.entries order.
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"alpha-gw","defaultModel":"alpha-model"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "alpha-model");
        // An unknown provider id falls through to first-authed (beta-gw).
        std::fs::write(
            config::settings_path().unwrap(),
            r#"{"defaultProvider":"not-a-provider","defaultModel":"beta-model"}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        assert_eq!(r.model.id, "beta-model");
    }

    #[test]
    fn models_json_fallback_preserves_provider_and_model_declaration_order() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://router.example.com/v1",
      "apiKey": "router-key",
      "models": [
        { "id": "gpt-5.6-sol" },
        { "id": "gpt-5.6-terra" }
      ]
    },
    "alpha-gw": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com/v1",
      "apiKey": "alpha-key",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "routeryo-copy");
        assert_eq!(resolved.model.id, "gpt-5.6-sol");
    }

    #[test]
    fn native_known_provider_default_beats_first_model_in_array() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "deepseek": {
      "api": "openai-completions",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "deepseek-key",
      "models": [
        { "id": "deepseek-chat" },
        { "id": "deepseek-v4-pro" }
      ]
    }
  }
}"#,
        )
        .unwrap();

        let resolved = resolve(None, None, None, None, None).unwrap();
        assert_eq!(resolved.model.provider, "deepseek");
        assert_eq!(resolved.model.id, "deepseek-v4-pro");
    }

    /// The `/model` selector catalog (`available_catalog`) is auth-filtered —
    /// it must NOT offer built-in claude-* models that carry no auth headers in
    /// a gateway-only setup (selecting one would fail at request time with
    /// "No API key for provider: anthropic"). Mirrors pi's
    /// `getAvailableSnapshot` filter (`available = all.filter(m =>
    /// configuredProviders.has(m.provider))`): only the gateway model is
    /// loadable, so only it appears in the selector / Ctrl+M cycle.
    #[test]
    fn available_catalog_filters_to_authed_models_in_gateway_only_setup() {
        let _env = TestEnv::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
        )
        .unwrap();
        let r = resolve(None, None, None, None, None).unwrap();
        // A bare models.json key is carried by this provider, not model headers.
        assert!(r.has_provider_key);
        let catalog = available_catalog(&r);
        // Exactly one loadable model: the gateway one. The 7 built-in Anthropic
        // models are filtered out.
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["custom-claude"],
            "selector must only list authed models"
        );
        // The runtime provider and selector share the same provider-isolated
        // catalog, so models from another identity cannot be routed through it.
        assert_eq!(r.provider.models().len(), catalog.len());
    }

    /// On the x-api-key path (`--api-key`/auth.json/`ANTHROPIC_API_KEY`), the
    /// provider's default key attaches to EVERY model out-of-band — so the
    /// catalog filter keeps the full list (all models are loadable).
    #[test]
    fn available_catalog_keeps_all_models_on_provider_key_path() {
        let _env = TestEnv::new();
        std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
        let r = resolve(None, None, None, None, None).unwrap();
        assert!(r.has_provider_key);
        let catalog = available_catalog(&r);
        assert_eq!(catalog.len(), r.provider.models().len());
        assert!(catalog.iter().any(|m| m.id == DEFAULT_MODEL_ID));
    }
}
