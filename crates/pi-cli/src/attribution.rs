//! Provider attribution headers.
//!
//! Port of native Pi's `packages/coding-agent/src/core/provider-attribution.ts`.
//! Some providers want to know which app is calling them (for their own
//! dashboards/quotas). Native pi attaches a small set of app-identifying headers
//! for OpenRouter, NVIDIA NIM, and Cloudflare, plus a session header for
//! opencode. This module reproduces that mapping.

use std::collections::BTreeMap;

use rpi_ai::model::Model;

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// Attribution is enabled unless `RPI_DISABLE_ATTRIBUTION=1` (native gates this
/// on install telemetry).
pub fn enabled() -> bool {
    !matches!(
        std::env::var("RPI_DISABLE_ATTRIBUTION").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn host_of(base_url: &str) -> Option<String> {
    let rest = base_url.split("://").nth(1)?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn matches_host(base_url: &str, expected: &str) -> bool {
    host_of(base_url).as_deref() == Some(expected)
}

fn is_openrouter(model: &Model) -> bool {
    model.provider.as_str() == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_nvidia_nim(model: &Model) -> bool {
    model.provider.as_str() == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST)
}

fn is_cloudflare(model: &Model) -> bool {
    matches!(model.provider.as_str(), "cloudflare-workers-ai" | "cloudflare-ai-gateway")
        || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST)
}

fn is_opencode(model: &Model) -> bool {
    matches!(model.provider.as_str(), "opencode" | "opencode-go")
        || matches_host(&model.base_url, OPENCODE_HOST)
}

/// The app-identifying headers for `model` (native `getDefaultAttributionHeaders`).
pub fn default_attribution_headers(model: &Model) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    if !enabled() {
        return headers;
    }
    if is_openrouter(model) {
        headers.insert("HTTP-Referer".into(), "https://pi.dev".into());
        headers.insert("X-OpenRouter-Title".into(), "pi".into());
        headers.insert("X-OpenRouter-Categories".into(), "cli-agent".into());
    }
    if is_nvidia_nim(model) {
        headers.insert("X-BILLING-INVOKE-ORIGIN".into(), "Pi".into());
    }
    if is_cloudflare(model) {
        headers.insert("User-Agent".into(), "rpi-coding-agent".into());
    }
    headers
}

/// opencode session headers (native `getSessionHeaders`).
pub fn session_headers(model: &Model, session_id: Option<&str>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
        return headers;
    };
    if is_opencode(model) {
        headers.insert("x-opencode-session".into(), session_id.to_string());
        headers.insert("x-opencode-client".into(), "rpi".into());
    }
    headers
}

/// Merge session + attribution headers with caller-supplied sources (later
/// wins), matching native `mergeProviderAttributionHeaders`.
pub fn merge_provider_attribution_headers(
    model: &Model,
    session_id: Option<&str>,
    extra: &[&BTreeMap<String, String>],
) -> BTreeMap<String, String> {
    let mut merged = session_headers(model, session_id);
    for (k, v) in default_attribution_headers(model) {
        merged.insert(k, v);
    }
    for source in extra {
        for (k, v) in *source {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{Api, ModelCost, ProviderId};

    fn model(provider: &str, base_url: &str) -> Model {
        let mut m = Model::new("id", "id", Api::AnthropicMessages, provider, base_url);
        m.cost = ModelCost::default();
        m
    }

    #[test]
    fn openrouter_gets_headers() {
        let headers = default_attribution_headers(&model("openrouter", "https://openrouter.ai/api/v1"));
        assert_eq!(headers.get("X-OpenRouter-Title").map(String::as_str), Some("pi"));
    }

    #[test]
    fn openrouter_detected_by_host() {
        let headers = default_attribution_headers(&model("custom", "https://openrouter.ai/api/v1"));
        assert!(headers.contains_key("HTTP-Referer"));
    }

    #[test]
    fn unrelated_provider_has_no_headers() {
        let headers = default_attribution_headers(&model("anthropic", "https://api.anthropic.com"));
        assert!(headers.is_empty());
    }

    #[test]
    fn opencode_session_headers() {
        let headers = session_headers(&model("opencode", "https://opencode.ai/x"), Some("s1"));
        assert_eq!(headers.get("x-opencode-session").map(String::as_str), Some("s1"));
        let none = session_headers(&model("anthropic", "https://api.anthropic.com"), Some("s1"));
        assert!(none.is_empty());
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://user:pw@openrouter.ai:443/api").as_deref(), Some("openrouter.ai"));
        assert_eq!(host_of("not a url"), None);
    }
}
