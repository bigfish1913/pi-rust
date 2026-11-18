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
//! FIRE-AND-FORGET: a successful send returns a dispatch ack
//! (`{status:"queued", delivery_id}`). The reply arrives later through the
//! host's own delivery path; the tool never blocks waiting for it.
//!
//! The schema models **two distinct inputs** instead of one shape with a mode
//! flag:
//!
//! - *send*: `target` + `message` are required (optionally `constraints`,
//!   `expecting`, `expects_reply`);
//! - *cancel*: `cancel` (a `delivery_id` from your own ack) is given, and
//!   `target`/`message` must be absent.
//!
//! Mixing both is an error — the tool never guesses which call the model
//! meant. The host receives the caller's session id (resolved from the harness
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
const FREE_TEXT_MAX_CHARS: usize = 4_000;
const DELIVERY_ID_MAX_CHARS: usize = 128;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MessageAgentInput {
    /// Who to message: a roster name or handle. Required for a send; omit when
    /// cancelling. The host validates it against its live roster.
    #[serde(default)]
    pub target: Option<String>,
    /// The message YOU composed for that agent (max 16000 chars). Do not add a
    /// "Message from …" prefix; the host adds it. Required for a send; omit
    /// when cancelling.
    #[serde(default)]
    pub message: Option<String>,
    /// Limits their answer must respect (scope, length, format).
    #[serde(default)]
    pub constraints: Option<String>,
    /// What you need back from them; shown to them as the reply they owe you.
    #[serde(default)]
    pub expecting: Option<String>,
    /// false: notify only — no reply comes back and nothing wakes you.
    #[serde(default = "default_expects_reply")]
    pub expects_reply: bool,
    /// Cancel mode: a `delivery_id` from your own ack. Withdraws that queued
    /// request or stops the turn it started. Cannot be combined with
    /// `target`/`message`.
    #[serde(default)]
    pub cancel: Option<String>,
}

fn default_expects_reply() -> bool {
    true
}

/// A validated dispatch request handed to the host (source id filled in).
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

pub fn create_message_agent_tool(binding: Arc<OnceLock<WeakAgentHarness>>) -> Arc<dyn AgentTool> {
    let params = schemars::schema_for!(MessageAgentInput);
    Arc::new(MessageAgentTool {
        binding,
        schema: Tool {
            name: MESSAGE_AGENT_TOOL_NAME.to_string(),
            description: "Send a message to ANOTHER agent (teammate) on this install. FIRE-AND-FORGET: it validates the target against the live roster, delivers into that agent's Bot Chat with your attribution prefixed, and returns a dispatch ack at once (status queued + delivery_id: a hand-off, not a receipt). It never returns the reply: finish your turn and the reply arrives later through the host. COMPOSE the message yourself (lead with the point and the concrete ask); never paste the user's words verbatim or share private 1:1 content. To withdraw a message you already sent, call with `cancel` set to its delivery_id instead of target/message.".to_string(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(params).unwrap_or_default(),
            ),
            constrained_sampling: None,
        },
    })
}

fn validate_target(target: &str) -> Result<String, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() || trimmed.chars().count() > TARGET_MAX_CHARS {
        return Err("message_agent: target must be a non-empty roster name (≤128 chars)".into());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("message_agent: target contains control characters".into());
    }
    Ok(trimmed.to_owned())
}

fn validate_delivery_id(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("message_agent: cancel requires a delivery_id from your own ack".into());
    }
    if trimmed.chars().count() > DELIVERY_ID_MAX_CHARS {
        return Err("message_agent: delivery_id is too long (≤128 chars)".into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "message_agent: delivery_id may only contain letters, digits, '-' and '_'".into(),
        );
    }
    Ok(trimmed.to_owned())
}

fn validate_free_text(field: &str, raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > FREE_TEXT_MAX_CHARS {
        return Err(format!(
            "message_agent: {field} is too long (≤{FREE_TEXT_MAX_CHARS} chars)"
        ));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!(
            "message_agent: {field} contains control characters"
        ));
    }
    Ok(Some(text.to_owned()))
}

/// The two valid call shapes. Parsing is where "send vs cancel" is decided, so
/// the executor never carries an ambient mode flag.
enum MessageAgentCall {
    Send {
        target: String,
        message: String,
        constraints: Option<String>,
        expecting: Option<String>,
        expects_reply: bool,
    },
    Cancel {
        delivery_id: String,
    },
}

fn parse_call(input: &MessageAgentInput) -> Result<MessageAgentCall, String> {
    if let Some(raw_cancel) = input.cancel.as_deref() {
        let delivery_id = validate_delivery_id(raw_cancel)?;
        let has_target = input
            .target
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| !text.is_empty());
        let has_message = input
            .message
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| !text.is_empty());
        if has_target || has_message {
            return Err(
                "message_agent: `cancel` cannot be combined with `target`/`message`".into(),
            );
        }
        return Ok(MessageAgentCall::Cancel { delivery_id });
    }

    let target = validate_target(input.target.as_deref().unwrap_or(""))?;
    let message = input.message.as_deref().map(str::trim).unwrap_or("");
    if message.is_empty() {
        return Err("message_agent: message must be non-empty".into());
    }
    if message.chars().count() > MESSAGE_MAX_CHARS {
        return Err(format!(
            "message_agent: message is too long (≤{MESSAGE_MAX_CHARS} chars)"
        ));
    }
    Ok(MessageAgentCall::Send {
        target,
        message: message.to_owned(),
        constraints: validate_free_text("constraints", input.constraints.as_deref())?,
        expecting: validate_free_text("expecting", input.expecting.as_deref())?,
        expects_reply: input.expects_reply,
    })
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
        let input: MessageAgentInput = serde_json::from_value(params).map_err(|error| {
            AgentError::Validation(format!("message_agent input invalid: {error}"))
        })?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("message_agent cancelled".into()));
        }
        let caller = self.caller().await?;
        let host = handler().map_err(AgentError::Tool)?;
        execute_input(host, &caller.session_id, input, &signal).await
    }
}

/// Execution body shared by `execute` and the unit tests (which supply a fake
/// door + session id instead of a live harness).
async fn execute_input(
    host: &Arc<dyn MessageAgentHandler>,
    source_session_id: &str,
    input: MessageAgentInput,
    signal: &CancellationToken,
) -> Result<AgentToolResult, AgentError> {
    match parse_call(&input).map_err(AgentError::Validation)? {
        MessageAgentCall::Cancel { delivery_id } => {
            host.cancel(source_session_id, &delivery_id)
                .await
                .map_err(AgentError::Tool)?;
            Ok(AgentToolResult::text(
                serde_json::json!({
                    "status": "cancelled",
                    "delivery_id": delivery_id,
                })
                .to_string(),
            ))
        }
        MessageAgentCall::Send {
            target,
            message,
            constraints,
            expecting,
            expects_reply,
        } => {
            let expects_reply = expects_reply;
            let ack = host
                .dispatch(MessageAgentRequest {
                    source_session_id: source_session_id.to_owned(),
                    target,
                    message,
                    constraints,
                    expecting,
                    expects_reply,
                })
                .await
                .map_err(AgentError::Tool)?;
            if signal.is_cancelled() {
                return Err(AgentError::Tool("message_agent cancelled".into()));
            }
            Ok(AgentToolResult::text(
                serde_json::json!({
                    "status": ack.status,
                    "delivery_id": ack.delivery_id,
                    "reply_delivery": if expects_reply { "notification" } else { "none" },
                })
                .to_string(),
            ))
        }
    }
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
        let metadata = harness.session().get_metadata().await.map_err(|error| {
            AgentError::Tool(format!(
                "message_agent: session metadata unavailable: {error}"
            ))
        })?;
        Ok(ResolvedCaller {
            session_id: metadata.id,
        })
    }
}

struct ResolvedCaller {
    session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingDoor {
        dispatched: Mutex<Vec<MessageAgentRequest>>,
        cancelled: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl MessageAgentHandler for RecordingDoor {
        async fn dispatch(&self, request: MessageAgentRequest) -> Result<MessageAgentAck, String> {
            self.dispatched.lock().unwrap().push(request);
            Ok(MessageAgentAck {
                delivery_id: "delivery-1".into(),
                status: "queued".into(),
            })
        }

        async fn cancel(&self, source: &str, delivery: &str) -> Result<(), String> {
            self.cancelled
                .lock()
                .unwrap()
                .push((source.to_owned(), delivery.to_owned()));
            Ok(())
        }
    }

    fn input(value: serde_json::Value) -> MessageAgentInput {
        serde_json::from_value(value).unwrap()
    }

    async fn run(
        door: &Arc<dyn MessageAgentHandler>,
        value: serde_json::Value,
    ) -> Result<AgentToolResult, AgentError> {
        execute_input(door, "session-1", input(value), &CancellationToken::new()).await
    }

    #[test]
    fn targets_are_trimmed_and_bounded() {
        assert_eq!(validate_target(" 微软 ").unwrap(), "微软");
        assert!(validate_target("   ").is_err());
        assert!(validate_target(&"a".repeat(TARGET_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn delivery_ids_are_validated() {
        assert_eq!(validate_delivery_id(" delivery-1 ").unwrap(), "delivery-1");
        assert!(validate_delivery_id("   ").is_err());
        assert!(validate_delivery_id(&"a".repeat(DELIVERY_ID_MAX_CHARS + 1)).is_err());
        assert!(validate_delivery_id("bad id!").is_err());
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(rpi_agent::types::TextContentOrImage::Text(text)) => text.text.clone(),
            other => panic!("expected a single text block, got {other:?}"),
        }
    }

    /// Send shape reaches the door with the caller's session id and every
    /// field intact; ack is surfaced as the tool's JSON result (reviewer #5:
    /// execution, not just shape).
    #[tokio::test]
    async fn send_dispatches_through_the_door() {
        let door = Arc::new(RecordingDoor::default());
        let door_dyn: Arc<dyn MessageAgentHandler> = door.clone();
        let result = run(
            &door_dyn,
            serde_json::json!({
                "target": "微软",
                "message": "hi",
                "constraints": "short",
                "expecting": "one line",
                "expects_reply": true,
            }),
        )
        .await
        .unwrap();
        let output = text_of(&result);
        assert!(output.contains("\"status\":\"queued\""));
        assert!(output.contains("\"delivery_id\":\"delivery-1\""));
        assert!(output.contains("\"reply_delivery\":\"notification\""));
        let recorded = door.dispatched.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].source_session_id, "session-1");
        assert_eq!(recorded[0].target, "微软");
        assert_eq!(recorded[0].expecting.as_deref(), Some("one line"));
    }

    /// Cancel shape never dispatches; the delivery door gets the ack id.
    #[tokio::test]
    async fn cancel_goes_to_the_cancel_door_only() {
        let door = Arc::new(RecordingDoor::default());
        let door_dyn: Arc<dyn MessageAgentHandler> = door.clone();
        let result = run(&door_dyn, serde_json::json!({ "cancel": "delivery-9" }))
            .await
            .unwrap();
        assert!(text_of(&result).contains("\"status\":\"cancelled\""));
        assert!(door.dispatched.lock().unwrap().is_empty());
        let cancelled = door.cancelled.lock().unwrap();
        assert_eq!(
            cancelled.as_slice(),
            &[("session-1".to_owned(), "delivery-9".to_owned())]
        );
    }

    /// Mixing cancel with send fields is rejected before any door call.
    #[tokio::test]
    async fn cancel_with_send_fields_is_invalid() {
        let door = Arc::new(RecordingDoor::default());
        let door_dyn: Arc<dyn MessageAgentHandler> = door.clone();
        let error = run(
            &door_dyn,
            serde_json::json!({ "cancel": "delivery-9", "target": "微软" }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AgentError::Validation(_)));
        assert!(door.dispatched.lock().unwrap().is_empty());
        assert!(door.cancelled.lock().unwrap().is_empty());
    }

    #[test]
    fn handler_installation_is_process_global() {
        // Runs in its own test binary slice; another test may have set it.
        if !handler_installed() {
            assert!(set_handler(Arc::new(RecordingDoor::default())));
        }
        assert!(handler_installed());
    }
}
