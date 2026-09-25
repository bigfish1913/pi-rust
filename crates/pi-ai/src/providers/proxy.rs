//! Proxy provider — routes LLM calls through a proxy server.
//!
//! Mirrors `packages/agent/src/proxy.ts`. Allows routing LLM API calls through
//! an intermediate server that handles authentication and request forwarding.
//!
//! # Use Cases
//!
//! - **Centralized authentication**: Proxy manages API keys, clients don't need credentials
//! - **Rate limiting**: Proxy enforces usage limits across multiple clients
//! - **Audit logging**: Proxy logs all requests for compliance
//! - **Cost control**: Proxy tracks and limits spending
//!
//! # Architecture
//!
//! ```text
//! Client (rpi) → Proxy Server → LLM Provider (Anthropic/OpenAI)
//!      ↑              ↑
//!   No API key    Manages auth
//! ```
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use rpi_ai::providers::proxy::{ProxyConfig, ProxyProvider};
//!
//! let config = ProxyConfig {
//!     endpoint: "https://proxy.example.com/v1".to_string(),
//!     api_key: Some("proxy-token".to_string()),
//!     timeout: Some(Duration::from_secs(60)),
//!     // `headers`/`api_key` are optional; the proxy holds the real provider key.
//!     headers: None,
//! };
//!
//! let _provider = ProxyProvider::new(config);
//! // Call `stream_simple(&model, &ctx, &opts)` from an async context to use it.
//! ```

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event_stream::AssistantMessageEventStream;
use crate::model::Model;
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::sse::SseEventStream;
use crate::types::{AssistantRole, Content, Context, DoneReason, TextContent, TextContentType, Usage};

/// Configuration for a proxy provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// The proxy server endpoint (e.g., "https://proxy.example.com/v1").
    pub endpoint: String,
    /// Optional API key for the proxy (not the underlying LLM provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Request timeout. Defaults to 600 seconds if not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Additional headers to send with each request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

impl ProxyConfig {
    /// Create a new proxy configuration.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: None,
            timeout: None,
            headers: None,
        }
    }

    /// Set the API key.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Add a header.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let headers = self.headers.get_or_insert_with(Default::default);
        headers.insert(key.into(), value.into());
        self
    }
}

/// A provider that routes requests through a proxy server.
///
/// The proxy server handles authentication with the underlying LLM provider
/// and forwards requests. This allows clients to use the proxy without
/// needing direct API credentials.
pub struct ProxyProvider {
    config: ProxyConfig,
    client: reqwest::Client,
}

impl ProxyProvider {
    /// Create a new proxy provider.
    pub fn new(config: ProxyConfig) -> Self {
        let timeout = config.timeout.unwrap_or(Duration::from_secs(600));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self { config, client }
    }

    /// Get the proxy endpoint.
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Build request headers.
    fn build_headers(&self, opts: &SimpleStreamOptions) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();

        // Add API key if present
        if let Some(api_key) = &self.config.api_key {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .expect("Invalid API key"),
            );
        }

        // Add custom headers from config
        if let Some(config_headers) = &self.config.headers {
            for (key, value) in config_headers {
                if let (Ok(name), Ok(val)) = (
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                    reqwest::header::HeaderValue::from_str(value),
                ) {
                    headers.insert(name, val);
                }
            }
        }

        // Add headers from options
        if let Some(opts_headers) = &opts.headers {
            for (key, value) in opts_headers {
                if let (Ok(name), Ok(val)) = (
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                    reqwest::header::HeaderValue::from_str(value),
                ) {
                    headers.insert(name, val);
                }
            }
        }

        headers
    }
}

#[async_trait]
impl Provider for ProxyProvider {
    fn id(&self) -> &str {
        "proxy"
    }

    fn models(&self) -> &[Model] {
        // Proxy providers don't have a fixed model list; they forward to any model
        // the proxy server supports. Return empty slice; the actual model list
        // would come from the proxy server's /models endpoint.
        &[]
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        use crate::event_stream::create_assistant_message_event_stream;
        use crate::types::{AssistantMessage, AssistantMessageEvent, ErrorReason, StopReason};

        let (mut prod, stream) = create_assistant_message_event_stream();

        // Build the request URL
        let url = format!("{}/chat/completions", self.config.endpoint.trim_end_matches('/'));

        // Build headers
        let headers = self.build_headers(opts);

        // Build the request body
        // The proxy expects OpenAI-compatible format
        let body = serde_json::json!({
            "model": model.id,
            "messages": ctx.messages,
            "stream": true,
        });

        // Clone what we need for the async task
        let client = self.client.clone();
        let model = model.clone();
        let opts = opts.clone();

        tokio::spawn(async move {
            // Make the request
            let response = client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        let status = resp.status();
                        let error_text = resp.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                        let error_msg = AssistantMessage::terminal(
                            model.api.clone(),
                            "proxy".to_string(),
                            model.id.clone(),
                            StopReason::Error,
                            format!("Proxy returned {}: {}", status, error_text),
                            0,
                        );
                        prod.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error: error_msg,
                        });
                        return;
                    }

                    // Parse SSE stream from proxy response
                    let mut events = SseEventStream::new(resp, opts.signal.clone());
                    let mut content = String::new();
                    let mut finish_reason = None;
                    
                    loop {
                        match events.next_event().await {
                            Ok(Some(event)) if event.data.trim() == "[DONE]" => {
                                break;
                            }
                            Ok(Some(event)) => {
                                if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(&event.data) {
                                    if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                                        if let Some(choice) = choices.first() {
                                            if let Some(delta) = choice.get("delta") {
                                                if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                                                    content.push_str(text);
                                                }
                                            }
                                            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                                                finish_reason = Some(reason.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let error_msg = AssistantMessage::terminal(
                                    model.api.clone(),
                                    "proxy".to_string(),
                                    model.id.clone(),
                                    StopReason::Error,
                                    format!("SSE stream error: {}", e),
                                    0,
                                );
                                prod.push(AssistantMessageEvent::Error {
                                    reason: ErrorReason::Error,
                                    error: error_msg,
                                });
                                return;
                            }
                        }
                    }
                    
                    let stop_reason = match finish_reason.as_deref() {
                        Some("stop") => StopReason::Stop,
                        Some("length") => StopReason::Length,
                        Some("tool_calls") => StopReason::ToolUse,
                        _ => StopReason::Stop,
                    };
                    
                    let message = AssistantMessage {
                        role: AssistantRole,
                        content: vec![Content::Text(TextContent {
                            kind: TextContentType,
                            text: content,
                            text_signature: None,
                        })],
                        api: model.api.clone(),
                        provider: "proxy".to_string(),
                        model: model.id.clone(),
                        response_model: None,
                        response_id: None,
                        usage: Usage::zero(),
                        stop_reason,
                        deferred: None,
                        error_message: None,
                        raw_stop_reason: finish_reason,
                        end_turn: None,
                        timestamp: 0,
                    };
                    
                    let done_reason = match stop_reason {
                        StopReason::Stop => DoneReason::Stop,
                        StopReason::Length => DoneReason::Length,
                        StopReason::ToolUse => DoneReason::ToolUse,
                        _ => DoneReason::Stop,
                    };
                    
                    prod.push(AssistantMessageEvent::Done {
                        reason: done_reason,
                        message,
                    });
                }
                Err(e) => {
                    let error_msg = AssistantMessage::terminal(
                        model.api.clone(),
                        "proxy".to_string(),
                        model.id.clone(),
                        StopReason::Error,
                        format!("Proxy request failed: {}", e),
                        0,
                    );
                    prod.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: error_msg,
                    });
                }
            }
        });

        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_builder() {
        let config = ProxyConfig::new("https://proxy.example.com")
            .with_api_key("test-key")
            .with_timeout(Duration::from_secs(30))
            .with_header("X-Custom", "value");

        assert_eq!(config.endpoint, "https://proxy.example.com");
        assert_eq!(config.api_key, Some("test-key".to_string()));
        assert_eq!(config.timeout, Some(Duration::from_secs(30)));
        assert_eq!(
            config.headers.as_ref().unwrap().get("X-Custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn test_proxy_provider_creation() {
        let config = ProxyConfig::new("https://proxy.example.com");
        let provider = ProxyProvider::new(config);
        assert_eq!(provider.endpoint(), "https://proxy.example.com");
        assert_eq!(provider.id(), "proxy");
    }
}
