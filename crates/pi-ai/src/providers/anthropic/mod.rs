//! Mirrors `packages/ai/src/api/anthropic-messages.ts` + `providers/anthropic.ts`
//! — the Anthropic Messages-API provider: request building, SSE decoding,
//! event mapping, retry, and cost. The crate-level [`Provider`] impl lives here.
//!
//! Only compiled when the `providers` feature is on (reqwest/bytes/eventsource-
//! stream are optional deps). The smoke test behind the `smoke` feature drives
//! a live request against `ANTHROPIC_API_KEY`.
//!
//! v1 scope is API-key auth only; OAuth/Copilot identity headers and the
//! Claude-Code system-prompt stealth prefix are TODO (see plan §5.16). The
//! `is_oauth` flag flows through `build_params` so enabling them later is a
//! localized change, not a re-plumb of the stream path.

pub mod build_params;
pub mod cache_control;
pub mod cost;
pub mod json_parse;
pub mod mapper;
pub mod models;
pub mod retry;
pub mod sse;

use crate::error::AiError;
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::{Model, StreamingProtocolCompat};
use crate::provider::{CacheRetention, Provider, SimpleStreamOptions};
use crate::types::{Context, Usage};
use async_trait::async_trait;
use std::sync::Arc;

use crate::providers::anthropic::build_params::build_params;
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::anthropic::mapper::{emit_terminal_error, run_mapper, MapperState};
use crate::providers::anthropic::models::anthropic_models;
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;

/// The fixed Anthropic Messages-API version header. Mirrors the SDK default
/// (`2023-06-01`); the TS SDK attaches this implicitly, the Rust port sets it
/// explicitly since it speaks raw `reqwest`.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The Anthropic Messages-API provider. Mirrors the `Anthropic`-SDK-backed
/// surface in `api/anthropic-messages.ts` (`stream` + `streamSimple`) +
/// `providers/anthropic.ts`.
///
/// v1 is API-key auth only: the resolved key is sent as `x-api-key`. OAuth
/// (`sk-ant-oat*` bearer tokens, Claude Code identity headers) and Copilot
/// (`github-copilot` provider, bearer + dynamic headers) paths are TODO.
/// Construct via [`AnthropicProvider::new`] or [`AnthropicProvider::from_env`].
pub struct AnthropicProvider {
    /// Default API key, used when `SimpleStreamOptions::api_key` is `None`.
    /// `None` here means "defer to opts + env at call time".
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}

impl AnthropicProvider {
    /// Build a provider with an explicit default API key + custom
    /// `reqwest::Client` (useful for proxies / custom TLS). Serves the
    /// hand-curated catalog from [`models::anthropic_models`].
    pub fn new(api_key: Option<String>, http: reqwest::Client) -> Self {
        Self {
            api_key,
            http,
            models: anthropic_models(),
        }
    }

    /// Build with a custom model catalog (e.g. a generated models.dev snapshot).
    pub fn with_models(api_key: Option<String>, http: reqwest::Client, models: Vec<Model>) -> Self {
        Self {
            api_key,
            http,
            models,
        }
    }

    /// Convenience: build with a default `reqwest::Client`, deferring API-key
    /// resolution to call time (opts.api_key → provider default →
    /// `ANTHROPIC_API_KEY` env). Mirrors how the smoke test constructs a
    /// provider without a pre-resolved key.
    pub fn from_env() -> Self {
        let http = reqwest::Client::new();
        Self::new(None, http)
    }

    /// Default API key carried on the provider (`opts.api_key` still wins).
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // `stream_simple` returns synchronously: create the stream, spawn the
        // producer task, hand back the consumer. Failures surface as `Error`
        // events on the stream, never as panics (plan §5.11). Mirrors the TS
        // `stream` IIFE pattern.
        let (mut prod, stream) = create_assistant_message_event_stream();

        let http = self.http.clone();
        let provider_key = self.api_key.clone();
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        let opts = opts.clone();

        tokio::spawn(async move {
            run_anthropic_stream(&mut prod, http, provider_key, &model, &ctx, &opts).await;
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

// ----------------------------------------------------------------------------
// Producer task — mirrors the TS `stream` async IIFE (anthropic-messages.ts
// :487-774) + `streamSimple` (:801-841).
// ----------------------------------------------------------------------------

/// The producer task body. Mirrors the TS `stream` async IIFE: build the empty
/// `output`, resolve auth (`assertRequestAuth`), build params, POST with retry
/// (`retryProviderRequest`), then drive the SSE mapper to the terminal event.
/// Every failure path pushes an `Error` terminal so the consumer's `result()`
/// always resolves.
///
/// The TS collapses `streamSimple` → `stream` → `buildParams` with an
/// intermediate `AnthropicOptions`; the Rust port folds `streamSimple` into
/// this function and lets `build_params` read `SimpleStreamOptions` directly
/// (the adaptive-vs-budget thinking decision lives there), so there is no
/// intermediate options shape.
async fn run_anthropic_stream(
    prod: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    provider_key: Option<String>,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) {
    let mut state = MapperState::new(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );

    // ---- resolve auth (assertRequestAuth) ----
    let resolved_key = resolve_api_key(&provider_key, opts);
    // TS `assertRequestAuth(model.provider, options?.apiKey, options?.headers)`
    // checks `options.headers` alone — but upstream the model-resolution layer
    // has *already merged* `model.headers` into `options.headers` (models.ts
    // `getApiKeyAndHeaders` → `auth.headers = merge(..., providerOrModel.headers)`).
    // The Rust port keeps auth headers on `model.headers` and merges them later
    // in `assemble_headers`, so here the check must consider BOTH sources to
    // reproduce the TS net effect. This is what lets a Bearer header folded onto
    // `model.headers` (the `ANTHROPIC_AUTH_TOKEN` / `models.json authHeader:true`
    // paths in rpi-cli's `provider::resolve`) authenticate without an `x-api-key`.
    let header_owned_auth = has_header_auth(&opts.headers) || has_header_auth(&model.headers);
    let api_key_for_header = match (resolved_key, header_owned_auth) {
        (Some(k), _) => Some(k),
        (None, true) => None, // headers carry auth; do not send x-api-key.
        (None, false) => {
            // Mirrors `assertRequestAuth`'s `throw new Error("No API key for…")`.
            let msg = format!("No API key for provider: {}", model.provider);
            emit_terminal_error(prod, &mut state, msg, false);
            return;
        }
    };

    // ---- build params (is_oauth = false in v1) ----
    let built = build_params(model, ctx, false, opts);
    let body = match serde_json::to_value(&built.request) {
        Ok(v) => v,
        Err(e) => {
            emit_terminal_error(
                prod,
                &mut state,
                format!("failed to serialize request body: {e}"),
                false,
            );
            return;
        }
    };

    // ---- assemble headers (createClient, API-key arm) ----
    let headers = assemble_headers(
        model,
        opts,
        built.beta_header.as_deref(),
        api_key_for_header.as_deref(),
    );

    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let timeout = opts.timeout;
    let signal = opts.signal.clone();

    // ---- POST with retry (retryProviderRequest) ----
    // The TS hands the SDK `.create({...params, stream:true}, {signal, timeout,
    // maxRetries:0}).asResponse()` to retryProviderRequest; the Rust port
    // reproduces that as a raw `reqwest` POST that returns a `Response` on
    // 2xx and an `AiError::Http { status }` on non-2xx (so the retry predicate
    // can classify 408/409/429/>=500). Cancellation is honored via a `select!`
    // around the send — the in-flight reqwest future is dropped on abort.
    let response_result = retry_provider_request(
        move || {
            let http = http.clone();
            let body = body.clone();
            let url = url.clone();
            let headers = headers.clone();
            let signal = signal.clone();
            async move {
                let mut req = http.post(&url);
                if let Some(t) = timeout {
                    req = req.timeout(t);
                }
                for (k, v) in &headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                let send_fut = req.json(&body).send();
                let resp = tokio::select! {
                    r = send_fut => r.map_err(|e| AiError::Http {
                        status: None,
                        message: format!("http transport error: {e}"),
                    })?,
                    _ = signal.cancelled() => return Err(AiError::Abort {
                        message: "Request aborted".to_string(),
                    }),
                };
                let status = resp.status();
                if !status.is_success() {
                    let code = status.as_u16();
                    // Read the error body for a diagnostic message; the retry
                    // predicate only needs the status code.
                    let text = resp.text().await.unwrap_or_default();
                    return Err(AiError::Http {
                        status: Some(code),
                        message: text,
                    });
                }
                Ok(resp)
            }
        },
        opts.max_retries,
        opts.max_retry_delay,
        &opts.signal,
    )
    .await;

    let response = match response_result {
        Ok(r) => r,
        Err(err) => {
            let aborted = err.is_abort();
            emit_terminal_error(prod, &mut state, err.to_string(), aborted);
            return;
        }
    };

    // ---- push start, drive SSE → mapper → terminal ----
    // The TS pushes `start` here right before the event loop; the Rust mapper
    // defers `start` to the first event (`ensure_started`) so a pre-stream
    // failure never delivers a partial `start` with no terminal. `run_mapper`
    // applies the model-aware `cost_fn` on the final usage before the terminal
    // event (mirrors the inline `calculateCost(model, output.usage)` calls).
    let mut sse = SseEventStream::new(response, opts.signal.clone());
    let cost_model = model.cost.clone();
    let cost_fn = move |usage: &Usage| calculate_cost(&cost_model, usage);
    run_mapper(&mut sse, prod, &mut state, cost_fn).await;
}

// ----------------------------------------------------------------------------
// Header + auth helpers — mirrors createClient (API-key arm) + assertRequestAuth.
// ----------------------------------------------------------------------------

/// Resolve the API key: `opts.api_key` wins, then the provider default, then
/// `ANTHROPIC_API_KEY` from the environment. Mirrors the TS fallback chain
/// (`options?.apiKey` → SDK's `apiKey: null` reads the env var internally).
fn resolve_api_key(provider_key: &Option<String>, opts: &SimpleStreamOptions) -> Option<String> {
    opts.api_key
        .clone()
        .or_else(|| provider_key.clone())
        .or_else(|| {
            std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
        })
}

/// True when `headers` carries an `authorization`, `x-api-key`, or
/// `cf-aig-authorization` header — the auth-owned-header cases
/// `assertRequestAuth` accepts without a resolved API key. Mirrors `hasHeader`
/// over those three names (case-insensitive).
fn has_header_auth(headers: &Option<std::collections::BTreeMap<String, String>>) -> bool {
    let Some(h) = headers else {
        return false;
    };
    const NAMES: &[&str] = &["authorization", "x-api-key", "cf-aig-authorization"];
    h.keys()
        .any(|k| NAMES.contains(&k.to_ascii_lowercase().as_str()))
}

/// Assemble the HTTP header set for the POST. Mirrors the API-key arm of TS
/// `createClient` (anthropic-messages.ts:914-936): `accept`,
/// `anthropic-dangerous-direct-browser-access`, `anthropic-version`,
/// `anthropic-beta` (when present), `x-session-affinity` (when enabled), then
/// `model.headers` and `opts.headers` merged (later wins). `x-api-key` is
/// applied LAST so caller headers cannot clobber the resolved key (the SDK
/// attaches it out-of-band from `defaultHeaders`).
fn assemble_headers(
    model: &Model,
    opts: &SimpleStreamOptions,
    beta_header: Option<&str>,
    api_key: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    headers.push(("accept".into(), "application/json".into()));
    headers.push((
        "anthropic-dangerous-direct-browser-access".into(),
        "true".into(),
    ));
    headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
    if let Some(beta) = beta_header {
        headers.push(("anthropic-beta".into(), beta.into()));
    }

    // Session affinity: TS `cacheSessionId = cacheRetention === "none" ?
    // undefined : options?.sessionId`, then gated on the model's
    // `sendSessionAffinityHeaders` compat flag (default false).
    if !matches!(opts.cache_retention, CacheRetention::None) {
        if let Some(session_id) = &opts.session_id {
            let send = model
                .compat
                .as_ref()
                .and_then(|c| match c {
                    StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
                    _ => None,
                })
                .map(|a| a.send_session_affinity_headers.unwrap_or(false))
                .unwrap_or(false);
            if send {
                headers.push(("x-session-affinity".into(), session_id.clone()));
            }
        }
    }

    // model.headers then opts.headers (later wins). Mirrors `mergeHeaders`.
    if let Some(model_headers) = &model.headers {
        for (k, v) in model_headers {
            merge_header(&mut headers, k.clone(), v.clone());
        }
    }
    if let Some(opt_headers) = &opts.headers {
        for (k, v) in opt_headers {
            merge_header(&mut headers, k.clone(), v.clone());
        }
    }

    // x-api-key applied last so it is not overridable by model/opts headers
    // (matches the SDK attaching it out-of-band). When a key is resolved it is
    // authoritative: drop any `x-api-key` a caller injected via `headers` so
    // only the resolved key is sent. Header-owned-auth callers pass
    // `api_key = None` here and rely on their own `x-api-key` / `authorization`
    // header surviving the merges above.
    if let Some(key) = api_key {
        headers.retain(|(k, _)| k.to_ascii_lowercase() != "x-api-key");
        headers.push(("x-api-key".into(), key.into()));
    }

    headers
}

/// Insert-or-replace a header by lowercase name. Mirrors TS `mergeHeaders` /
/// `Object.assign` last-wins semantics.
fn merge_header(headers: &mut Vec<(String, String)>, name: String, value: String) {
    let lname = name.to_ascii_lowercase();
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.to_ascii_lowercase() == lname)
    {
        slot.1 = value;
    } else {
        headers.push((name, value));
    }
}

/// Wall-clock ms-since-epoch for the assistant message timestamp. Mirrors TS
/// `Date.now()`. The provider is the one `pi-ai` component that touches real
/// time: the message timestamp is persisted into the harness JSONL log, so a
/// monotonic counter (as faux uses) would be wrong here.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::anthropic::models::claude_haiku_4_5;
    use crate::types::{Api, InputModality, ModelCost};

    /// Build a minimal model for header/auth tests (no network).
    fn test_model() -> Model {
        let mut m = Model::new(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        m.input = vec![InputModality::Text];
        m.context_window = 200_000;
        m.max_tokens = 8_192;
        m.cost = ModelCost::default();
        m.compat = Some(crate::model::StreamingProtocolCompat::AnthropicMessages(
            crate::model::AnthropicMessagesCompat::default(),
        ));
        m
    }

    #[test]
    fn assemble_headers_includes_required_defaults() {
        let model = test_model();
        let opts = SimpleStreamOptions::default();
        let headers = assemble_headers(&model, &opts, None, Some("sk-test"));
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"accept"));
        assert!(names.contains(&"anthropic-version"));
        assert!(names.contains(&"anthropic-dangerous-direct-browser-access"));
        assert!(names.contains(&"x-api-key"));
        // No beta header when none requested.
        assert!(!names.contains(&"anthropic-beta"));
        let version = headers
            .iter()
            .find(|(k, _)| k == "anthropic-version")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(version, ANTHROPIC_VERSION);
    }

    #[test]
    fn assemble_headers_applies_beta_when_present() {
        let model = test_model();
        let opts = SimpleStreamOptions::default();
        let headers = assemble_headers(
            &model,
            &opts,
            Some("fine-grained-tool-streaming-2025-05-14"),
            Some("k"),
        );
        let beta = headers
            .iter()
            .find(|(k, _)| k == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(beta, "fine-grained-tool-streaming-2025-05-14");
    }

    #[test]
    fn assemble_headers_x_api_key_not_overridable_by_opts() {
        let model = test_model();
        let mut opts = SimpleStreamOptions::default();
        let mut extra = std::collections::BTreeMap::new();
        // Caller tries to override the resolved key via opts.headers — should
        // be ignored (x-api-key applied last wins), matching SDK semantics.
        extra.insert("x-api-key".to_string(), "SK-ATTACK".to_string());
        opts.headers = Some(extra);
        let headers = assemble_headers(&model, &opts, None, Some("sk-resolved"));
        let key = headers
            .iter()
            .find(|(k, _)| k == "x-api-key")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(key, "sk-resolved");
    }

    #[test]
    fn assemble_headers_session_affinity_gated_on_compat_and_retention() {
        // Haiku 4.5 uses the catalog model which has send_session_affinity off
        // (default false) → no x-session-affinity even with a session id.
        let mut model = claude_haiku_4_5();
        let _ = &mut model; // catalog model, compat default has no affinity flag
        let mut opts = SimpleStreamOptions::default();
        opts.session_id = Some("sess-1".into());
        opts.cache_retention = CacheRetention::Short;
        let headers = assemble_headers(&model, &opts, None, Some("k"));
        assert!(
            !headers.iter().any(|(k, _)| k == "x-session-affinity"),
            "default catalog should not send session affinity"
        );

        // Opt in via compat flag → header emitted.
        let mut model = test_model();
        let mut compat = crate::model::AnthropicMessagesCompat::default();
        compat.send_session_affinity_headers = Some(true);
        model.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
        let headers = assemble_headers(&model, &opts, None, Some("k"));
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-session-affinity")
                .map(|(_, v)| v.as_str()),
            Some("sess-1")
        );

        // CacheRetention::None suppresses it even when compat is on.
        let mut opts2 = opts.clone();
        opts2.cache_retention = CacheRetention::None;
        let headers = assemble_headers(&model, &opts2, None, Some("k"));
        assert!(
            !headers.iter().any(|(k, _)| k == "x-session-affinity"),
            "None retention suppresses session affinity"
        );
    }

    #[test]
    fn assemble_headers_model_and_opts_merge_last_wins() {
        let mut model = test_model();
        let mut mh = std::collections::BTreeMap::new();
        mh.insert("x-custom".to_string(), "from-model".to_string());
        model.headers = Some(mh);
        let mut opts = SimpleStreamOptions::default();
        let mut oh = std::collections::BTreeMap::new();
        oh.insert("x-custom".to_string(), "from-opts".to_string());
        oh.insert("x-extra".to_string(), "extra".to_string());
        opts.headers = Some(oh);
        let headers = assemble_headers(&model, &opts, None, None);
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-custom")
                .map(|(_, v)| v.as_str()),
            Some("from-opts"),
            "opts.headers wins over model.headers"
        );
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-extra")
                .map(|(_, v)| v.as_str()),
            Some("extra")
        );
    }

    #[test]
    fn has_header_auth_detects_owned_auth_headers() {
        let mut h = std::collections::BTreeMap::new();
        h.insert("X-API-Key".to_string(), "k".to_string());
        assert!(has_header_auth(&Some(h.clone())));
        let mut h2 = std::collections::BTreeMap::new();
        h2.insert("authorization".to_string(), "Bearer t".to_string());
        assert!(has_header_auth(&Some(h2)));
        assert!(!has_header_auth(&None));
        let mut h3 = std::collections::BTreeMap::new();
        h3.insert("x-other".to_string(), "v".to_string());
        assert!(!has_header_auth(&Some(h3)));
    }

    #[test]
    fn resolve_api_key_prefers_opts_then_provider_then_env() {
        let opts = SimpleStreamOptions::default().with_api_key("opts-key");
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts),
            Some("opts-key".into())
        );

        let opts = SimpleStreamOptions::default();
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts),
            Some("provider-key".into())
        );
    }

    #[test]
    fn provider_exposes_catalog_models() {
        let p = AnthropicProvider::from_env();
        assert_eq!(p.id(), "anthropic");
        assert!(!p.models().is_empty());
        assert!(p.models().iter().any(|m| m.id == "claude-haiku-4-5"));
        assert!(p.api_key().is_none(), "from_env does not pre-read the key");
    }

    /// Regression for the Bearer-on-`model.headers` path (the
    /// `ANTHROPIC_AUTH_TOKEN` / `models.json authHeader:true` routes in
    /// rpi-cli's `provider::resolve`): when the auth header lives on
    /// `model.headers` and no key is set, the header-owned-auth check must pass
    /// so the request is NOT rejected as "No API key for provider". This mirrors
    /// the net effect of upstream `assertRequestAuth`, which runs *after* the
    /// model-resolution layer has merged `model.headers` into `options.headers`.
    #[test]
    fn header_auth_on_model_headers_counts_as_owned() {
        let mut model = test_model();
        let mut mh = std::collections::BTreeMap::new();
        mh.insert("authorization".to_string(), "Bearer tok".to_string());
        model.headers = Some(mh);

        // No opts.headers, no key — the only auth is on the model. Must satisfy.
        let owned = has_header_auth(&SimpleStreamOptions::default().headers)
            || has_header_auth(&model.headers);
        assert!(
            owned,
            "a Bearer header on model.headers must satisfy header-owned auth"
        );

        // A model carrying only non-auth headers must NOT satisfy it.
        let mut model2 = test_model();
        let mut mh2 = std::collections::BTreeMap::new();
        mh2.insert("x-custom".to_string(), "v".to_string());
        model2.headers = Some(mh2);
        let owned2 = has_header_auth(&SimpleStreamOptions::default().headers)
            || has_header_auth(&model2.headers);
        assert!(
            !owned2,
            "non-auth headers on model.headers must not satisfy header-owned auth"
        );
    }
}
