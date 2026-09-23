//! DeepSeek provider implementation.
//!
//! DeepSeek is a Chinese AI company providing large language models.
//! API compatible with OpenAI format.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::AiError;
use crate::event_stream::{create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventStreamProducer};
use crate::model::Model;
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::sse::SseEventStream;
use crate::types::{AssistantMessage, AssistantMessageEvent, AssistantRole, Content, Context, DoneReason, ErrorReason, StopReason, TextContent, TextContentType, Usage};

/// DeepSeek API endpoint
const DEEPSEEK_API_BASE: &str = "https://api.deepseek.com/v1";

/// DeepSeek provider implementation
#[derive(Debug, Clone)]
pub struct DeepSeekProvider {
    client: Client,
    api_key: Option<String>,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider
    pub fn new(api_key: Option<String>) -> Self {
        // Route through the shared HTTP builder so HTTP_PROXY/NO_PROXY and the
        // pooled-connection idle timeout apply uniformly.
        let client = crate::http::build_client(DEEPSEEK_API_BASE, None, None)
            .unwrap_or_else(|_| Client::new());
        Self { client, api_key }
    }

    /// Build request headers
    fn build_headers(&self, opts: &SimpleStreamOptions) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
        ];

        // API key from opts or stored
        let api_key = opts.api_key.as_ref().or(self.api_key.as_ref());
        if let Some(key) = api_key {
            headers.push(("Authorization".to_string(), format!("Bearer {}", key)));
        }

        headers
    }

    /// Build request body
    fn build_request_body(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Value {
        let mut body = serde_json::json!({
            "model": model.id,
            "messages": ctx.messages,
            "stream": true,
        });

        // Add optional parameters
        if let Some(temperature) = opts.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }

        if let Some(max_tokens) = opts.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }

        body
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn id(&self) -> &str {
        "deepseek"
    }

    fn models(&self) -> &[Model] {
        // DeepSeek models are configured via models.json
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
        
        let url = format!("{}/chat/completions", DEEPSEEK_API_BASE);
        let headers = self.build_headers(opts);
        let body = self.build_request_body(model, ctx, opts);

        let client = self.client.clone();
        let model_id = model.id.clone();
        let api = model.api.clone();
        let opts = opts.clone();
        
        tokio::spawn(async move {
            let mut request = client.post(&url).json(&body);

            for (key, value) in headers {
                request = request.header(&key, &value);
            }

            let result = request.send().await;
            
            match result {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                        let error_msg = format!("HTTP {}: {}", status.as_u16(), error_text);
                        
                        let error_message = AssistantMessage::terminal(
                            api,
                            "deepseek".to_string(),
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
                                    "deepseek".to_string(),
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
                        provider: "deepseek".to_string(),
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
                        "deepseek".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepseek_provider_creation() {
        let provider = DeepSeekProvider::new(Some("test-key".to_string()));
        assert_eq!(provider.id(), "deepseek");
    }
}
