//! Mirrors `packages/agent/src/types.ts` — the public type contract of the
//! agent layer: tool results, hook payloads, agent state, events.
//!
//! The TS source declares `AgentMessage = Message | Custom` via declaration
//! merging; the Rust port models it as an open [`AgentMessage`] enum (see
//! [`crate::message`]). Everything else here is a straight port of the TS
//! interfaces, adapted to Rust ownership/async idioms.

use rpi_ai::types::{AssistantMessage, ImageContent, TextContent, ToolResultMessage, Usage};
use std::collections::HashSet;
use std::sync::Arc;

use crate::message::AgentMessage;

/// How a batch of tool calls from one assistant message are executed.
///
/// - `Sequential`: each call is prepared, executed, finalized before the next starts.
/// - `Parallel`: calls are prepared sequentially, then allowed tools execute
///   concurrently. `tool_execution_end` fires in completion order; tool-result
///   `MessageEnd` fires later in assistant source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// Controls how many queued user messages are injected at a drain point.
/// `All` drains every queued message; `OneAtATime` drains only the oldest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Result returned from a tool `execute`. Mirrors TS `AgentToolResult<T>`:
/// `content` goes back to the model, `details` are structured log/UI payload,
/// `usage` is optionally reported, and `terminate` hints the batch should stop.
#[derive(Debug, Clone, Default)]
pub struct AgentToolResult {
    pub content: Vec<TextContentOrImage>,
    pub details: serde_json::Value,
    pub usage: Option<Usage>,
    pub added_tool_names: Vec<String>,
    pub terminate: bool,
}
/// an enum so tool authors stay within the provider-content language without
/// pulling in the full `Content` union (which adds Thinking/ToolCall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextContentOrImage {
    Text(TextContent),
    Image(ImageContent),
}

impl TextContentOrImage {
    pub fn text<S: Into<String>>(s: S) -> Self {
        TextContentOrImage::Text(TextContent {
            kind: rpi_ai::types::TextContentType,
            text: s.into(),
            text_signature: None,
        })
    }
}

impl AgentToolResult {
    /// Convenience: a single text block, empty details.
    pub fn text(message: impl Into<String>) -> Self {
        Self {
            content: vec![TextContentOrImage::text(message)],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        }
    }

    /// Convenience: an error text block. `is_error` is carried on the
    /// `ToolResultMessage`, not the result; this just builds the content.
    pub fn error_text(message: impl Into<String>) -> Self {
        Self::text(message)
    }

    pub fn into_content(self) -> Vec<rpi_ai::types::Content> {
        self.content
            .into_iter()
            .map(|c| match c {
                TextContentOrImage::Text(t) => rpi_ai::types::Content::Text(t),
                TextContentOrImage::Image(i) => rpi_ai::types::Content::Image(i),
            })
            .collect()
    }
}

impl From<AgentToolResult> for Result<AgentToolResult, crate::AgentError> {
    fn from(r: AgentToolResult) -> Self {
        Ok(r)
    }
}

/// Partial result pushed by a tool's `on_update` callback during execution.
/// Mirrors TS `AgentToolUpdateCallback<T>` payload.
pub type ToolResultPartial = AgentToolResult;

/// Result of a `before_tool_call` hook. `block` prevents execution; the loop
/// emits an error tool result with `reason` (or a default) instead. `terminate`
/// participates in the batch early-termination rule (all results must set it).
///
/// `args` is the Rust equivalent of TS `beforeToolCall` mutating the validated
/// args object in place: JS callbacks receive `args` by reference and write to
/// it; Rust hands the hook an immutable `&serde_json::Value`, so to rewrite the
/// args the hook returns them here. Replacement args are applied **without
/// re-validation** — mirroring TS, where the mutation happens after
/// `validateToolArguments` and is never re-checked. `None` keeps the validated
/// args.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
    pub args: Option<serde_json::Value>,
}

/// Partial override returned from `after_tool_call`. Field-by-field merge:
/// provided values replace the executed result's fields; omitted fields keep
/// the original. No deep merge.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<TextContentOrImage>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// Context passed to `before_tool_call`. Mirrors TS `BeforeToolCallContext`.
pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a rpi_ai::types::ToolCall,
    pub args: &'a serde_json::Value,
    pub context: &'a AgentContext,
}

/// Context passed to `after_tool_call`. Mirrors TS `AfterToolCallContext`.
pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a rpi_ai::types::ToolCall,
    pub args: &'a serde_json::Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
    pub context: &'a AgentContext,
}

/// Context passed to `should_stop_after_turn` / `prepare_next_turn`.
pub struct ShouldStopAfterTurnContext<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    pub context: &'a AgentContext,
    pub new_messages: &'a [AgentMessage],
}

/// A context snapshot handed to the low-level loop. Mirrors TS `AgentContext`.
#[derive(Clone, Default)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn crate::agent_tool::AgentTool>>,
}

impl std::fmt::Debug for AgentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentContext")
            .field("system_prompt", &self.system_prompt)
            .field("messages", &self.messages)
            .field("tools", &self.tools.iter().map(|t| t.schema().name.as_str()).collect::<Vec<_>>())
            .finish()
    }
}

impl AgentContext {
    pub fn new(messages: Vec<AgentMessage>) -> Self {
        Self {
            system_prompt: String::new(),
            messages,
            tools: Vec::new(),
        }
    }
}

/// Replacement runtime state returned by `prepare_next_turn`. `None` on any
/// field means "keep current".
#[derive(Debug, Clone, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<rpi_ai::model::Model>,
    pub thinking_level: Option<rpi_ai::types::ThinkingLevel>,
}

/// Public agent state snapshot. Mirrors TS `AgentState` (the readable subset).
/// `tools`/`messages` clone on read so callers can't mutate internal state.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: rpi_ai::model::Model,
    pub thinking_level: rpi_ai::types::ThinkingLevel,
    pub tools: Vec<Arc<dyn crate::agent_tool::AgentTool>>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: HashSet<String>,
    pub error_message: Option<String>,
}

impl std::fmt::Debug for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentState")
            .field("system_prompt", &self.system_prompt)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("tools", &self.tools.iter().map(|t| t.schema().name.as_str()).collect::<Vec<_>>())
            .field("messages", &self.messages)
            .field("is_streaming", &self.is_streaming)
            .field("streaming_message", &self.streaming_message)
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish()
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: default_model(),
            thinking_level: rpi_ai::types::ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        }
    }
}

/// The placeholder model used when none is configured. Mirrors TS `DEFAULT_MODEL`.
pub(crate) fn default_model() -> rpi_ai::model::Model {
    rpi_ai::model::Model::new("unknown", "unknown", rpi_ai::types::Api::Other("unknown".into()), "unknown", "")
}
