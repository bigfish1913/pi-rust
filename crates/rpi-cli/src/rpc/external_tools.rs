//! Native-owned external tool execution; no backend protocol lives here.
use super::{
    client_requests::ClientRequestBroker,
    writer::{RpcWriter, write_line},
};
use async_trait::async_trait;
use rpi_agent::{
    error::AgentError,
    types::{AgentToolResult, TextContentOrImage},
};
use rpi_ai::types::Content;
use rpi_tools::tools::external::ExternalToolBackend;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) struct ExternalTools {
    pub broker: ClientRequestBroker,
    writer: std::sync::Mutex<Option<RpcWriter>>,
    stop: CancellationToken,
}
impl ExternalTools {
    pub fn new(writer: RpcWriter) -> Arc<Self> {
        Arc::new(Self {
            broker: ClientRequestBroker::default(),
            writer: std::sync::Mutex::new(Some(writer)),
            stop: CancellationToken::new(),
        })
    }
    pub fn abort(&self) {
        self.broker.cancel_all();
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
        self.abort();
        // Registry entries can outlive RPC. They must not keep its writer open.
        self.writer.lock().unwrap().take();
    }
    pub fn respond(&self, message: &Value) -> Result<bool, String> {
        let id = message
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or("external_tool_response requires requestId")?;
        let forms = usize::from(message.get("result").is_some())
            + usize::from(message.get("error").is_some())
            + usize::from(message.get("cancelled") == Some(&Value::Bool(true)));
        if forms != 1 {
            return Err(
                "external_tool_response requires exactly one result, error, or cancelled:true"
                    .into(),
            );
        }
        let reply = if message.get("cancelled") == Some(&Value::Bool(true)) {
            Err("external tool cancelled".into())
        } else if let Some(error) = message.get("error") {
            Err(error.to_string())
        } else {
            let result = message["result"].clone();
            validate_result(&result).map_err(|e| e.to_string())?;
            Ok(result)
        };
        Ok(self.broker.resolve(id, reply))
    }
}

// Preserve the complete unmodified result alongside its model-facing native
// content. Unknown backend metadata is never truncated or converted to text.
fn validate_result(raw: &Value) -> Result<(), AgentError> {
    if raw.get("isError").is_some_and(|value| !value.is_boolean()) {
        return Err(AgentError::Validation(
            "external result isError must be boolean".into(),
        ));
    }
    if raw
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
    {
        return Err(AgentError::Validation(
            "external result structuredContent must be an object".into(),
        ));
    }
    decode_content(raw).map(|_| ())
}

fn decode_content(raw: &Value) -> Result<Vec<TextContentOrImage>, AgentError> {
    let blocks = raw
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Validation("external result requires a content array".into()))?;
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        let parsed: Content = serde_json::from_value(block.clone())
            .map_err(|e| AgentError::Validation(e.to_string()))?;
        content.push(match parsed {
            Content::Text(t) => TextContentOrImage::Text(t),
            Content::Image(i) => TextContentOrImage::Image(i),
            _ => {
                return Err(AgentError::Validation(
                    "external tool content must be native text or image blocks".into(),
                ));
            }
        });
    }
    Ok(content)
}

fn decode_result(raw: Value) -> Result<AgentToolResult, AgentError> {
    validate_result(&raw)?;
    let mut content = decode_content(&raw)?;
    if raw.get("isError") == Some(&Value::Bool(true)) {
        // AgentToolResult has no error flag: the native execution boundary
        // carries errors through AgentError, never through a successful result.
        return Err(AgentError::Tool(raw.to_string()));
    }
    if let Some(structured) = raw.get("structuredContent") {
        // Details do not reach providers. Project only the explicit structured
        // result, never backend metadata, and avoid repeating equivalent JSON.
        let represented = content.iter().any(|block| match block {
            TextContentOrImage::Text(text) => {
                serde_json::from_str::<Value>(&text.text).is_ok_and(|value| value == *structured)
            }
            _ => false,
        });
        if !represented {
            content.push(TextContentOrImage::text(structured.to_string()));
        }
    }
    Ok(AgentToolResult {
        content,
        details: json!({"externalToolResult":raw}),
        ..Default::default()
    })
}

#[async_trait]
impl ExternalToolBackend for ExternalTools {
    async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentError> {
        if cancel.is_cancelled() || self.stop.is_cancelled() {
            return Err(AgentError::Abort);
        }
        // Child requests have no trusted session routing in the external bridge.
        // Host task-local provenance, never caller arguments, controls this gate.
        // Fail before allocating a request or emitting any external event.
        if rpi_agent::tool_execution_scope::is_child_execution() {
            return Err(AgentError::Tool(
                "external tools are unsupported in child agents: the external bridge does not support child-session routing".into(),
            ));
        }
        let writer = self
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or(AgentError::Abort)?;
        let request = self.broker.register().map_err(AgentError::Tool)?;
        let _close = CloseRequest {
            writer: writer.clone(),
            id: request.id.clone(),
        };
        let event = json!({"type":"external_tool_request", "requestId":request.id, "toolName":name, "arguments":arguments});
        if event.to_string().len() > super::protocol::MAX_FRAME_BYTES {
            return Err(AgentError::Validation(
                "external arguments exceed frame limit".into(),
            ));
        }
        let operation = async {
            write_line(&writer, &event).await;
            request
                .wait(Duration::from_secs(60), &cancel)
                .await
                .map_err(AgentError::Tool)
        };
        let raw = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(AgentError::Abort),
            _ = cancel.cancelled() => return Err(AgentError::Abort),
            result = tokio::time::timeout(Duration::from_secs(60), operation) => result.map_err(|_| AgentError::Tool("external tool timed out".into()))??,
        };
        decode_result(raw)
    }
}
struct CloseRequest {
    writer: RpcWriter,
    id: String,
}
impl Drop for CloseRequest {
    fn drop(&mut self) {
        // Bounded best-effort closure notification; slot retirement itself is
        // synchronous in ClientRequest::drop, independent of writer progress.
        let event = json!({"type":"external_tool_request_closed", "requestId":self.id});
        self.writer.try_write_line(&event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_result_is_preserved_and_unsupported_content_rejected() {
        let raw = json!({"content":[{"type":"text","text":"a","annotations":{"x":1}},{"type":"image","data":"AA==","mimeType":"image/png"}],"structuredContent":{"x":2}});
        let result = decode_result(raw.clone()).unwrap();
        assert_eq!(result.content.len(), 3);
        assert_eq!(result.content[2], TextContentOrImage::text(r#"{"x":2}"#));
        assert_eq!(result.details["externalToolResult"], raw);
        assert!(decode_result(json!({"content":[{"type":"resource"}]})).is_err());
    }

    include!("external_tools_result_tests.rs");
    include!("external_tools_child_tests.rs");
}
