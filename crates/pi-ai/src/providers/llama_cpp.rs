//! LLaMA.cpp provider integration.
//!
//! Provides support for local LLM inference using llama.cpp server.
//! This allows users to run models locally without API keys.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::event_stream::{create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventStreamProducer};
use crate::model::Model;
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::sse::SseEventStream;
use crate::types::{AssistantMessage, AssistantMessageEvent, AssistantRole, Content, Context, DoneReason, ErrorReason, StopReason, TextContent, TextContentType, Usage};

/// LLaMA.cpp server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlamaCppConfig {
    /// Server URL (default: http://localhost:8080)
    #[serde(default = "default_server_url")]
    pub server_url: String,
    
    /// Request timeout
    #[serde(default = "default_timeout")]
    pub timeout: Duration,
    
    /// API key (if required)
    pub api_key: Option<String>,
}

fn default_server_url() -> String {
    "http://localhost:8080".to_string()
}

fn default_timeout() -> Duration {
    Duration::from_secs(300)
}

impl Default for LlamaCppConfig {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            timeout: default_timeout(),
            api_key: None,
        }
    }
}

/// LLaMA.cpp provider
pub struct LlamaCppProvider {
    config: LlamaCppConfig,
    client: Client,
}

impl LlamaCppProvider {
    /// Create a new LLaMA.cpp provider
    pub fn new(config: LlamaCppConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("Failed to build HTTP client");
        
        Self { config, client }
    }
    
    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(LlamaCppConfig::default())
    }
    
    /// Check if the server is available
    pub async fn health_check(&self) -> Result<bool, AiError> {
        let url = format!("{}/health", self.config.server_url);
        
        let mut request = self.client.get(&url);
        
        if let Some(api_key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Request failed: {}", e),
        })?;
        
        Ok(response.status().is_success())
    }
    
    /// Get model information from the server
    pub async fn get_model_info(&self) -> Result<serde_json::Value, AiError> {
        let url = format!("{}/v1/models", self.config.server_url);
        
        let mut request = self.client.get(&url);
        
        if let Some(api_key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Request failed: {}", e),
        })?;
        
        if !response.status().is_success() {
            return Err(AiError::Http {
                status: Some(response.status().as_u16()),
                message: format!("Failed to get model info: {}", response.status()),
            });
        }
        
        let info: serde_json::Value = response.json().await.map_err(|e| AiError::Http {
            status: None,
            message: format!("Failed to parse response: {}", e),
        })?;
        Ok(info)
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn id(&self) -> &str {
        "llama-cpp"
    }
    
    fn models(&self) -> &[Model] {
        // LLaMA.cpp models are dynamically loaded from the server
        &[]
    }
    
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        use crate::event_stream::create_assistant_message_event_stream;
        use crate::types::{ErrorReason, StopReason};
        
        let (mut producer, stream) = create_assistant_message_event_stream();
        
        let url = format!("{}/v1/chat/completions", self.config.server_url);
        
        let mut request_body = serde_json::json!({
            "model": model.id,
            "messages": ctx.messages,
            "stream": true,
        });
        
        // Add optional parameters
        if let Some(temperature) = opts.temperature {
            request_body["temperature"] = serde_json::json!(temperature);
        }
        
        if let Some(max_tokens) = opts.max_tokens {
            request_body["max_tokens"] = serde_json::json!(max_tokens);
        }
        
        let mut request = self.client.post(&url).json(&request_body);
        
        if let Some(api_key) = &self.config.api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        if let Some(headers) = &opts.headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }
        
        let client = self.client.clone();
        let model_id = model.id.clone();
        let api = model.api.clone();
        let opts = opts.clone();
        
        tokio::spawn(async move {
            let result = request.send().await;
            
            match result {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                        let error_msg = format!("HTTP {}: {}", status.as_u16(), error_text);
                        
                        let error_message = AssistantMessage::terminal(
                            api,
                            "llama-cpp".to_string(),
                            model_id,
                            StopReason::Error,
                            error_msg,
                            0,
                        );
                        
                        producer.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error: error_message,
                        });
                        return;
                    }

                    // Parse SSE stream
                    let mut events = SseEventStream::new(response, opts.signal.clone());
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
                                let error_message = AssistantMessage::terminal(
                                    api,
                                    "llama-cpp".to_string(),
                                    model_id,
                                    StopReason::Error,
                                    format!("SSE stream error: {}", e),
                                    0,
                                );
                                producer.push(AssistantMessageEvent::Error {
                                    reason: ErrorReason::Error,
                                    error: error_message,
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
                        api,
                        provider: "llama-cpp".to_string(),
                        model: model_id,
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
                    
                    producer.push(AssistantMessageEvent::Done {
                        reason: done_reason,
                        message,
                    });
                }
                Err(e) => {
                    let error_message = AssistantMessage::terminal(
                        api,
                        "llama-cpp".to_string(),
                        model_id,
                        StopReason::Error,
                        format!("Request failed: {}", e),
                        0,
                    );
                    
                    producer.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: error_message,
                    });
                }
            }
        });

        stream
    }
}

/// LLaMA.cpp model manager
pub struct LlamaCppModelManager {
    provider: LlamaCppProvider,
}

impl LlamaCppModelManager {
    /// Create a new model manager
    pub fn new(config: LlamaCppConfig) -> Self {
        Self {
            provider: LlamaCppProvider::new(config),
        }
    }
    
    /// List available models
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let info = self.provider.get_model_info().await?;
        
        let models = info["data"]
            .as_array()
            .ok_or_else(|| AiError::Provider {
                code: "INVALID_RESPONSE".to_string(),
                message: "Invalid model list format".to_string(),
            })?
            .iter()
            .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
            .collect();
        
        Ok(models)
    }
    
    /// Check if server is healthy
    pub async fn is_healthy(&self) -> Result<bool, AiError> {
        self.provider.health_check().await
    }
    
    /// Get the provider
    pub fn provider(&self) -> &LlamaCppProvider {
        &self.provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_config_default() {
        let config = LlamaCppConfig::default();
        assert_eq!(config.server_url, "http://localhost:8080");
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert!(config.api_key.is_none());
    }
    
    #[test]
    fn test_provider_creation() {
        let provider = LlamaCppProvider::with_defaults();
        assert_eq!(provider.id(), "llama-cpp");
    }
}
