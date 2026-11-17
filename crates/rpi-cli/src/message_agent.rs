//! Built-in `message_agent` tool — agent-to-agent messaging for host-integrated
//! bot sessions.
//!
//! ## Why this lives in the engine
//!
//! The tool has to be visible to the model in the session's tool list and it
//! must run in the same turn loop as every other tool. Delivery itself is
//! host-owned (a desktop app knows its roster and its bot sessions; a bare CLI
//! does not), so the engine exposes a process-global [`MessageAgentHandler`]
//! that an embedding host installs once. When no handler is installed the tool
//! is not registered at all — a CLI session never advertises a tool it cannot
//! service.
//!
//! ## Semantics (mirrors hermes `tools/bot_mode_dm.py`)
//!
//! FIRE-AND-FORGET: a successful call returns a dispatch ack
//! (`{status:"queued", delivery_id}`). The reply arrives later through the
//! host's own delivery path; the tool never blocks waiting for it. `target` is
//! resolved and validated by the host against its live roster. `cancel` is
//! forwarded to the handler's optional cancellation door.
//!
//! The host receives the caller's session id (resolved from the harness
//! binding) so it can attribute the message without trusting model input.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use rpi_harness::agent_harness::WeakAgentHarness;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

pub const MESSAGE_AGENT_TOOL_NAME: &str = "message_agent";
pub const MESSAGE_MAX_CHARS: usize = 16_000;
const TARGET_MAX_CHARS: usize = 128;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MessageAgentInput {
    /// Who to message: a roster name or handle. The host validates it against
    /// the live roster.
    pub target: String,
    /// The message YOU composed for that agent (max 16000 chars). Do not add a
    /// "Message from …" prefix; the host adds it.
    pub message: String,
    /// Limits their answer must respect (scope, length, format).
    #[serde(default)]
    pub constraints: Option<String>,
    /// What you need back from them; shown to them as the reply they owe you.
    #[serde(default)]
    pub expecting: Option<String>,
    /// false: notify only — no reply comes back and nothing wakes you.
    #[serde(default = "default_expects_reply")]
    pub expects_reply: bool,
    /// A delivery_id from your own ack: withdraw that queued request or stop
    /// the turn it started.
    #[serde(default)]
    pub cancel: Option<String>,
}

fn default_expects_reply() -> bool {
    true
}

/// A validated dispatch request handed to the host.
#[derive(Debug, Clone)]
pub struct MessageAgentRequest {
    /// Session that called the tool (resolved by the engine, not by the model).
    pub source_session_id: String,
    pub target: String,
    pub message: String,
    pub constraints: Option<String>,
    pub expecting: Option<String>,
    pub expects_reply: bool,
}

#[derive(Debug, Clone)]
pub struct MessageAgentAck {
    pub delivery_id: String,
    pub status: String,
}

/// Host delivery door. Implementations enqueue the message and return at once.
#[async_trait]
pub trait MessageAgentHandler: Send + Sync {
    async fn dispatch(&self, request: MessageAgentRequest) -> Result<MessageAgentAck, String>;

    /// Optional cancellation door. Default = unsupported (the tool reports the
    /// handler's refusal; it never pretends the request was withdrawn).
    async fn cancel(&self, _source_session_id: &str, _delivery_id: &str) -> Result<(), String> {
        Err("message_agent: this host does not support cancelling deliveries".into())
    }
}

static HANDLER: OnceLock<Arc<dyn MessageAgentHandler>> = OnceLock::new();

/// Install the host delivery door (once per process). Returns false when a
/// handler was already installed.
pub fn set_handler(handler: Arc<dyn MessageAgentHandler>) -> bool {
    HANDLER.set(handler).is_ok()
}

pub fn handler_installed() -> bool {
    HANDLER.get().is_some()
}

fn handler() -> Result<&'static Arc<dyn MessageAgentHandler>, String> {
    HANDLER
        .get()
        .ok_or_else(|| "message_agent: no host handler installed".to_owned())
}

pub struct MessageAgentTool {
    schema: Tool,
    binding: Arc<OnceLock<WeakAgentHarness>>,
}

pub fn create_message_agent_tool(
    binding: Arc<OnceLock<WeakAgentHarness>>,
) -> Arc<dyn AgentTool> {
    let params = schemars::schema_for!(MessageAgentInput);
    Arc::new(MessageAgentTool {
        binding,
        schema: Tool {
            name: MESSAGE_AGENT_TOOL_NAME.to_string(),
            description: "Send a message to ANOTHER agent (teammate) on this install. FIRE-AND-FORGET: it validates the target against the live roster, delivers into that agent's Bot Chat with your attribution prefixed, and returns a dispatch ack at once (status queued + delivery_id: a hand-off, not a receipt). It never returns the reply: finish your turn and the reply arrives later through the host. COMPOSE the message yourself (lead with the point and the concrete ask); never paste the user's words verbatim or share private 1:1 content.".to_string(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(params).unwrap_or_default(),
            ),
            constrained_sampling: None,
        },
    })
}

struct ResolvedCaller {
    session_id: String,
}

impl MessageAgentTool {
    async fn caller(&self) -> Result<ResolvedCaller, AgentError> {
        let weak = self
            .binding
            .get()
            .ok_or_else(|| AgentError::Tool("message_agent: session binding not ready".into()))?;
        let harness = weak
            .upgrade()
            .ok_or_else(|| AgentError::Tool("message_agent: session is gone".into()))?;
        let metadata = harness
            .session()
            .get_metadata()
            .await
            .map_err(|error| AgentError::Tool(format!("message_agent: session metadata unavailable: {error}")))?;
        Ok(ResolvedCaller {
            session_id: metadata.id,
        })
    }
}

fn validate_target(target: &str) -> Result<String, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() || trimmed.chars().count() > TARGET_MAX_CHARS {
        return Err("message_agent: target must be a non-empty roster name (≤128 chars)".into());
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("message_agent: target contains control characters".into());
    }
    Ok(trimmed.to_owned())
}

#[async_trait]
impl AgentTool for MessageAgentTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        MESSAGE_AGENT_TOOL_NAME
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Messaging is a cheap enqueue; parallel batches are fine.
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: MessageAgentInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("message_agent input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("message_agent cancelled".into()));
        }
        let caller = self.caller().await?;
        let host = handler().map_err(AgentError::Tool)?;

        if let Some(delivery_id) = input.cancel.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            host.cancel(&caller.session_id, delivery_id)
                .await
                .map_err(AgentError::Tool)?;
            return Ok(AgentToolResult::text(serde_json::json!({
                "status": "cancelled",
                "delivery_id": delivery_id,
            }).to_string()));
        }

        let target = validate_target(&input.target).map_err(AgentError::Validation)?;
        let message = input.message.trim();
        if message.is_empty() || message.chars().count() > MESSAGE_MAX_CHARS {
            return Err(AgentError::Validation(format!(
                "message_agent: message must be non-empty and at most {MESSAGE_MAX_CHARS} chars"
            )));
        }

        let ack = host
            .dispatch(MessageAgentRequest {
                source_session_id: caller.session_id,
                target,
                message: message.to_owned(),
                constraints: input.constraints.clone(),
                expecting: input.expecting.clone(),
                expects_reply: input.expects_reply,
            })
            .await
            .map_err(AgentError::Tool)?;

        Ok(AgentToolResult::text(
            serde_json::json!({
                "status": ack.status,
                "delivery_id": ack.delivery_id,
                "reply_delivery": if input.expects_reply { "notification" } else { "none" },
            })
            .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;
    #[async_trait]
    impl MessageAgentHandler for Stub {
        async fn dispatch(&self, request: MessageAgentRequest) -> Result<MessageAgentAck, String> {
            assert_eq!(request.target, "微软");
            Ok(MessageAgentAck {
                delivery_id: "d-1".into(),
                status: "queued".into(),
            })
        }
    }

    #[test]
    fn targets_are_trimmed_and_bounded() {
        assert_eq!(validate_target(" 微软 ").unwrap(), "微软");
        assert!(validate_target("   ").is_err());
        assert!(validate_target(&"a".repeat(TARGET_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn handler_installation_is_process_global() {
        // Runs in its own test binary slice; another test may have set it.
        if !handler_installed() {
            assert!(set_handler(Arc::new(Stub)));
        }
        assert!(handler_installed());
    }
}
