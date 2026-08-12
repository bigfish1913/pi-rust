//! Mirrors `packages/ai/src/api/anthropic-messages.ts` — the SSE →
//! `AssistantMessageEvent` mapper (`content_block_*` → text/thinking/toolcall
//! start/delta/end, `message_start`/`message_delta` → usage + stop_reason) and
//! `mapStopReason`.
//!
//! The TS `stream` function keeps a single mutable `output: AssistantMessage`
//! plus a `blocks[]` array carrying per-block scratch state (the streaming
//! `partialJson` buffer for tool calls). The Rust port models that as
//! [`MapperState`]: an `AssistantMessage` grown per event, plus a parallel
//! `Vec<BlockScratch>` tracking each content block's Anthropic `index` and, for
//! tool calls, the `partial_json` accumulation.
//!
//! Tool-call arg parsing invariant (plan §5.15): re-parse partial JSON on every
//! `input_json_delta` and do the final authoritative parse on
//! `content_block_stop`. The streaming arm uses [`parse_streaming_json`] so a
//! truncated delta never produces unparseable args — `ToolCallEnd` always
//! carries a real object.

use crate::error::AiError;
use crate::event_stream::AssistantMessageEventStreamProducer;
use crate::providers::anthropic::json_parse::parse_streaming_json;
use crate::providers::anthropic::sse::{AnthropicEvent, SseEventStream};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, DoneReason, ErrorReason, StopReason,
    TextContent, TextContentType, ThinkingContent, ThinkingContentType, ToolCall, ToolCallType,
    Usage, UsageCost,
};
use std::sync::Arc;

/// Per-content-block scratch state, mirroring the TS `Block` shape with its
/// `index` + `partialJson` fields.
#[derive(Debug, Clone)]
struct BlockScratch {
    /// The Anthropic `content_block.index` this scratch tracks. The mapper
    /// grows `output.content` in arrival order but blocks may arrive with
    /// non-sequential indices (e.g. interleaved thinking+text on Opus 4.7); we
    /// look up scratch by `index`, not by `content` position.
    anthropic_index: i64,
    /// The position in `output.content` where this block lives. Set on
    /// `content_block_start`, read on every delta/stop.
    content_index: usize,
    /// The block's kind, so deltas can match without re-inspecting the content.
    kind: BlockKind,
    /// For tool-call blocks: the running `partial_json` buffer, appended on
    /// each `input_json_delta`. Empty for text/thinking.
    partial_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking { redacted: bool },
    ToolCall,
}

/// The mutable state the mapper advances per SSE event. Mirrors the TS `output`
/// + `blocks` pair.
pub struct MapperState {
    pub output: AssistantMessage,
    blocks: Vec<BlockScratch>,
    /// Marker that the mapper has emitted `start` for the stream. The TS
    /// `stream` pushes `start` right before the event loop; the Rust port
    /// defers it to the first event so a pre-stream failure (auth/SSE) doesn't
    /// deliver a partial `start` with no terminal event.
    started: bool,
}

impl MapperState {
    /// Build a fresh state with an empty assistant message ready to grow.
    /// `timestamp` is ms-since-epoch; callers pass it from the provider.
    pub fn new(
        api: crate::types::Api,
        provider: impl Into<String>,
        model: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            output: AssistantMessage::empty(api, provider, model, timestamp),
            blocks: Vec::new(),
            started: false,
        }
    }

    fn ensure_started(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        if !self.started {
            self.started = true;
            prod.push(AssistantMessageEvent::Start {
                partial: Arc::new(self.output.clone()),
            });
        }
    }

    fn find_block_by_anthropic_index(&self, anthropic_index: i64) -> Option<usize> {
        self.blocks
            .iter()
            .position(|b| b.anthropic_index == anthropic_index)
    }

    /// Apply one decoded Anthropic event. Mirrors the per-`event.type` dispatch
    /// in the TS `stream` async IIFE. Returns `Err(AiError)` only on
    /// unrecoverable mapper state (an unexpected block kind for a delta); SSE
    /// parse errors surface earlier, in the decoder. Never panics: a missing
    /// block for an index is a no-op (the TS `findIndex` returns -1 and the
    /// delta is skipped).
    pub fn apply(
        &mut self,
        event: &AnthropicEvent,
        prod: &mut AssistantMessageEventStreamProducer,
    ) -> Result<(), AiError> {
        let AnthropicEvent::Message { event_type, payload } = event else {
            return Ok(()); // Skipped events are a mapper no-op.
        };

        // The TS loop only enters the match arms for event names it recognizes;
        // `iterate_anthropic_events` already filtered to ANTHROPIC_MESSAGE_EVENTS,
        // so we dispatch on the payload `type` (which equals the SSE event name).
        match event_type.as_str() {
            "message_start" => self.apply_message_start(payload, prod),
            "content_block_start" => self.apply_content_block_start(payload, prod),
            "content_block_delta" => self.apply_content_block_delta(payload, prod)?,
            "content_block_stop" => self.apply_content_block_stop(payload, prod),
            "message_delta" => self.apply_message_delta(payload, prod),
            "message_stop" => { /* TS reads message_stop only to flip sawMessageEnd; noop here. */ }
            _ => {}
        }
        Ok(())
    }

    fn apply_message_start(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        self.ensure_started(prod);
        // `event.message.id` — response id.
        if let Some(id) = payload
            .pointer("/message/id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            self.output.response_id = Some(id);
        }

        // Initial usage (input/cacheRead/cacheWrite; output may be 0 here and
        // updated on the final message_delta). Preserves input_tokens from
        // message_start when a proxy omits it in message_delta.
        if let Some(usage) = payload.pointer("/message/usage") {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_write = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // `cache_creation.ephemeral_1h_input_tokens` — the subset of
            // cache_creation written with 1h retention (cost calculation
            // charges these at 2× input).
            let cache_write_1h = usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            self.output.usage.input = input;
            self.output.usage.output = output;
            self.output.usage.cache_read = cache_read;
            self.output.usage.cache_write = cache_write;
            self.output.usage.cache_write_1h = if cache_write_1h > 0 {
                Some(cache_write_1h)
            } else {
                None
            };
            // Anthropic doesn't report total_tokens; compute from components
            // (mirrors the TS `input + output + cacheRead + cacheWrite` line).
            self.output.usage.total_tokens =
                input + output + cache_read + cache_write;
            // Cost is recomputed in the provider after the model is known; the
            // mapper zeroes cost here so the provider's final pass owns it.
            self.output.usage.cost = UsageCost::default();
        }
    }

    fn apply_content_block_start(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        self.ensure_started(prod);
        let anthropic_index = payload
            .get("index")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let Some(block) = payload.get("content_block") else {
            return;
        };
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let content = Content::Text(TextContent {
                    kind: TextContentType,
                    text,
                    text_signature: None,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Text,
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                });
            }
            "thinking" => {
                let thinking = block
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signature = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    thinking,
                    thinking_signature: if signature.is_empty() {
                        None
                    } else {
                        Some(signature)
                    },
                    redacted: false,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Thinking { redacted: false },
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                });
            }
            "redacted_thinking" => {
                let data = block
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    // Mirrors the TS: a fixed marker, the opaque payload lives
                    // in `thinking_signature` so it round-trips back to
                    // `redacted_thinking` on the next turn.
                    thinking: "[Reasoning redacted]".to_string(),
                    thinking_signature: Some(data),
                    redacted: true,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Thinking { redacted: true },
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                });
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // `input` is usually `{}` on block_start (streaming fills it
                // via deltas); keep whatever's present as the initial args.
                let args = block.get("input").cloned().unwrap_or(serde_json::Value::Object(
                    serde_json::Map::new(),
                ));
                let content = Content::tool_call(id, name, args);
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::ToolCall,
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                });
            }
            // image / unknown content blocks are not emitted by the Anthropic
            // Messages streaming API on the assistant side — skip (mirrors TS).
            _ => {}
        }
    }

    fn apply_content_block_delta(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) -> Result<(), AiError> {
        let anthropic_index = payload
            .get("index")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let Some(delta) = payload.get("delta") else {
            return Ok(());
        };
        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");

        let Some(scratch_index) = self.find_block_by_anthropic_index(anthropic_index) else {
            // Delta for an unknown index (block_start missed/truncated) — skip
            // rather than panic, matching the TS `if (block && ...)` guards.
            return Ok(());
        };
        let scratch = &mut self.blocks[scratch_index];
        let content_index = scratch.content_index;

        match (scratch.kind, delta_type) {
            (BlockKind::Text, "text_delta") => {
                let text = delta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Text(slot)) = self.output.content.get_mut(content_index) {
                    slot.text.push_str(&text);
                }
                prod.push(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta: text,
                    partial: Arc::new(self.output.clone()),
                });
            }
            (BlockKind::Thinking { redacted: false }, "thinking_delta") => {
                let thinking = delta
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Thinking(slot)) = self.output.content.get_mut(content_index)
                {
                    slot.thinking.push_str(&thinking);
                }
                prod.push(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: thinking,
                    partial: Arc::new(self.output.clone()),
                });
            }
            (BlockKind::Thinking { redacted: false }, "signature_delta") => {
                // Append to the existing signature (initializing empty to "").
                let sig = delta
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Thinking(slot)) = self.output.content.get_mut(content_index)
                {
                    let current = slot.thinking_signature.take().unwrap_or_default();
                    slot.thinking_signature = Some(format!("{current}{sig}"));
                }
            }
            (BlockKind::ToolCall, "input_json_delta") => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                scratch.partial_json.push_str(&partial);
                // Re-parse the accumulated partial JSON on every delta so the
                // partial assistant message renders args live. Mirrors the TS
                // `block.arguments = parseStreamingJson(block.partialJson)`.
                if let Some(Content::ToolCall(slot)) = self.output.content.get_mut(content_index)
                {
                    slot.arguments = parse_streaming_json(Some(&scratch.partial_json));
                }
                prod.push(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta: partial,
                    partial: Arc::new(self.output.clone()),
                });
            }
            // Redacted-thinking blocks receive no deltas; unknown delta types
            // are ignored (forward-compat for future delta variants).
            _ => {}
        }
        Ok(())
    }

    fn apply_content_block_stop(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        let anthropic_index = payload
            .get("index")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let Some(scratch_index) = self.find_block_by_anthropic_index(anthropic_index) else {
            return;
        };
        let scratch = &mut self.blocks[scratch_index];
        let content_index = scratch.content_index;
        match scratch.kind {
            BlockKind::Text => {
                let content = match &self.output.content[content_index] {
                    Content::Text(t) => t.text.clone(),
                    _ => String::new(),
                };
                prod.push(AssistantMessageEvent::TextEnd {
                    content_index,
                    content,
                    partial: Arc::new(self.output.clone()),
                });
            }
            BlockKind::Thinking { redacted } => {
                let thinking_text = match &self.output.content[content_index] {
                    Content::Thinking(t) => t.thinking.clone(),
                    _ => String::new(),
                };
                // The final `partialJson` parse was already done on each delta;
                // for tool calls the stop event does the authoritative parse.
                // For thinking there's no JSON; just emit the accumulated text.
                let _ = redacted;
                prod.push(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: thinking_text,
                    partial: Arc::new(self.output.clone()),
                });
            }
            BlockKind::ToolCall => {
                // Final authoritative parse. Mirrors the TS
                // `block.arguments = parseStreamingJson(block.partialJson)`.
                let final_args =
                    parse_streaming_json(Some(&scratch.partial_json));
                let tool_call = match &self.output.content[content_index] {
                    Content::ToolCall(tc) => ToolCall {
                        kind: ToolCallType,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: final_args,
                        thought_signature: tc.thought_signature.clone(),
                        namespace: tc.namespace.clone(),
                    },
                    _ => return,
                };
                if let Some(Content::ToolCall(slot)) = self.output.content.get_mut(content_index)
                {
                    slot.arguments = tool_call.arguments.clone();
                }
                prod.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial: Arc::new(self.output.clone()),
                });
            }
        }
    }

    fn apply_message_delta(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        let _ = prod; // message_delta never pushes events; it mutates output only.
        // `delta.stop_reason` → mapStopReason + rawStopReason.
        if let Some(stop_reason) = payload
            .pointer("/delta/stop_reason")
            .and_then(|v| v.as_str())
        {
            self.output.raw_stop_reason = Some(stop_reason.to_string());
            let stop_details = payload.pointer("/delta/stop_details");
            match map_stop_reason(stop_reason, stop_details) {
                Ok(MappedStop { stop_reason, error_message }) => {
                    self.output.stop_reason = stop_reason;
                    if let Some(msg) = error_message {
                        self.output.error_message = Some(msg);
                    }
                }
                Err(e) => {
                    // TS throws on unhandled stop reasons; the Rust port records
                    // it as an error message + Error stop_reason so the stream
                    // still terminates (the provider loop converts the
                    // post-stream Error check).
                    self.output.stop_reason = StopReason::Error;
                    self.output.error_message = Some(e.to_string());
                }
            }
        }

        // Usage update — only fields that are present (not null), mirroring the
        // TS field-by-field null checks. Preserves message_start input_tokens
        // when the proxy omits them here.
        if let Some(usage) = payload.get("usage") {
            if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.input = v;
            }
            if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.output = v;
            }
            if let Some(v) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.cache_read = v;
            }
            if let Some(v) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.cache_write = v;
            }
            // Reasoning tokens — a subset of output_tokens, reported via
            // `output_tokens_details.thinking_tokens` (the SDK type omits the
            // field; the TS reads it through a narrow cast). Mirrors that.
            if let Some(thinking_tokens) = usage
                .pointer("/output_tokens_details/thinking_tokens")
                .and_then(|v| v.as_i64())
            {
                self.output.usage.reasoning = Some(thinking_tokens);
            }
        }
        // Anthropic doesn't provide total_tokens; recompute from components
        // unconditionally (mirrors TS line 741, OUTSIDE the `if (event.usage)`
        // block). When usage is absent the components are unchanged, so this
        // is a no-op — but it stays faithful to the source's control flow.
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
    }
}

/// The result of mapping an Anthropic stop reason. Mirrors the TS return shape
/// `{ stopReason, errorMessage? }`.
pub struct MappedStop {
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

/// Map an Anthropic `stop_reason` to pi's `StopReason`, optionally carrying an
/// error message (`refusal`/`sensitive`). Mirrors TS `mapStopReason`.
///
/// Returns `Err(AiError::Provider)` for an unhandled stop reason so the caller
/// can surface it; the TS source `throw`s, which the Rust port translates to an
/// `AiError` rather than a panic.
pub fn map_stop_reason(
    reason: &str,
    stop_details: Option<&serde_json::Value>,
) -> Result<MappedStop, AiError> {
    match reason {
        "end_turn" => Ok(MappedStop {
            stop_reason: StopReason::Stop,
            error_message: None,
        }),
        "max_tokens" => Ok(MappedStop {
            stop_reason: StopReason::Length,
            error_message: None,
        }),
        "tool_use" => Ok(MappedStop {
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "The model refused to complete the request".to_string());
            Ok(MappedStop {
                stop_reason: StopReason::Error,
                error_message: Some(explanation),
            })
        }
        // pause_turn / stop_sequence both collapse to Stop (mirrors TS).
        "pause_turn" | "stop_sequence" => Ok(MappedStop {
            stop_reason: StopReason::Stop,
            error_message: None,
        }),
        "sensitive" => Ok(MappedStop {
            stop_reason: StopReason::Error,
            error_message: Some("Provider stopped with: sensitive".to_string()),
        }),
        other => Err(AiError::Provider {
            code: "unhandled_stop_reason".to_string(),
            message: format!("Unhandled stop reason: {other}"),
        }),
    }
}

/// Run the mapper over a live SSE event stream, pushing
/// `AssistantMessageEvent`s to `prod` as each Anthropic event arrives, and
/// emit the terminal `Done`/`Error` event when the stream ends.
///
/// Mirrors the TS `stream` async IIFE's event loop + terminal handling. A
/// missing stop reason (stream ended with `Pending`) is converted to an
/// `Error` event; an aborted signal surfaces as `Error { reason: Aborted }`.
/// `cost_fn` is called on the final `output.usage` before the terminal event
/// so the message carries the model-aware cost (the TS calls `calculateCost`
/// inline; the Rust port defers to the provider, which knows the `Model`).
pub async fn run_mapper<F>(
    stream: &mut SseEventStream,
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
) where
    F: Fn(&Usage) -> UsageCost,
{
    loop {
        match stream.next_event().await {
            Ok(None) => break,
            Ok(Some(sse_frame)) => {
                let event = match crate::providers::anthropic::sse::parse_anthropic_event(&sse_frame)
                {
                    Ok(e) => e,
                    Err(err) => {
                        emit_terminal_error(prod, state, err.to_string(), false);
                        return;
                    }
                };
                if let Err(err) = state.apply(&event, prod) {
                    emit_terminal_error(prod, state, err.to_string(), false);
                    return;
                }
            }
            Err(err) => {
                let aborted = matches!(err, AiError::Abort { .. });
                emit_terminal_error(prod, state, err.to_string(), aborted);
                return;
            }
        }
    }

    finalize_mapper(prod, state, cost_fn);
}

/// Emit the terminal `Done`/`Error` event for a normally-ended stream. Mirrors
/// the TS post-loop checks (lines 747-759): `pending` stop reason → error;
/// `aborted`/`error` → error with the carried message; otherwise `done` with
/// the model-aware cost applied.
///
/// Extracted from [`run_mapper`] so tests can drive the mapper over a
/// pre-decoded `Vec<AnthropicEvent>` (no `reqwest::Response` needed) and still
/// exercise the terminal logic.
pub fn finalize_mapper<F>(
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
) where
    F: Fn(&Usage) -> UsageCost,
{
    if state.output.stop_reason == StopReason::Pending {
        emit_terminal_error(
            prod,
            state,
            "Anthropic stream ended without a stop reason".to_string(),
            false,
        );
        return;
    }
    if matches!(
        state.output.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        let aborted = matches!(state.output.stop_reason, StopReason::Aborted);
        let msg = state
            .output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string());
        emit_terminal_error(prod, state, msg, aborted);
        return;
    }

    // Success terminal. Apply the model-aware cost last.
    state.output.usage.cost = cost_fn(&state.output.usage);
    let reason = match state.output.stop_reason {
        StopReason::Stop => DoneReason::Stop,
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        _ => DoneReason::Stop, // unreachable given the checks above.
    };
    prod.push(AssistantMessageEvent::Done {
        reason,
        message: state.output.clone(),
    });
}

/// Emit the terminal `Error` event for the stream, stamping `output.stop_reason`
/// + `error_message` and pushing an `AssistantMessageEvent::Error`. Mirrors the
/// TS `catch` block's `output.stopReason = ...; output.errorMessage = ...;
/// stream.push({ type: "error", ... })`.
///
/// Extracted as a `pub` helper so the provider's pre-stream catch (auth
/// failure / HTTP error / abort) reuses the same terminal shape the mapper's
/// in-stream catch uses, keeping the consumer's `result()` contract uniform.
pub fn emit_terminal_error(
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    message: String,
    aborted: bool,
) {
    state.output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    state.output.error_message = Some(message);
    let reason = if aborted {
        ErrorReason::Aborted
    } else {
        ErrorReason::Error
    };
    prod.push(AssistantMessageEvent::Error {
        reason,
        error: state.output.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;
    use crate::providers::anthropic::sse::{parse_anthropic_event, ServerSentEvent};
    use crate::types::{Api, Content, StopReason};
    use serde_json::json;

    /// Build a `ServerSentEvent` frame from an event name + raw data string.
    fn frame(event: &str, data: &str) -> ServerSentEvent {
        ServerSentEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
            raw: vec![format!("event: {event}\ndata: {data}")],
        }
    }

    /// A JSON object literal as a `&str`, for inline event data.
    fn j(value: &serde_json::Value) -> String {
        serde_json::to_string(value).unwrap()
    }

    struct Run {
        tags: Vec<&'static str>,
        result: AssistantMessage,
    }

    /// Drive a fixture of (event, data) frames through the mapper + finalizer,
    /// returning the emitted event tags and the terminal `AssistantMessage`.
    async fn run_fixture(frames: Vec<ServerSentEvent>) -> Run {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let mut state = MapperState::new(
            Api::AnthropicMessages,
            "anthropic",
            "claude-haiku-4-5",
            0,
        );
        for f in &frames {
            let event = parse_anthropic_event(f).expect("event parses");
            state.apply(&event, &mut prod).expect("apply");
        }
        // finalize_mapper applies the model-aware cost; tests pass a zero cost
        // fn since they assert usage fields, not dollar amounts.
        finalize_mapper(&mut prod, &mut state, |_| UsageCost::default());

        // Hand the producer to a driver task so it can push the buffered events
        // while the consumer drains. The producer's push calls happen above
        // synchronously; dropping the producer into a task that immediately exits
        // lets the consumer observe the terminal event via the result oneshot.
        drop(prod);
        let mut stream = stream;
        let mut tags = Vec::new();
        while let Some(ev) = stream.next().await {
            tags.push(ev.type_tag());
        }
        let result = stream.result().await.expect("terminal result");
        Run { tags, result }
    }

    // Mirrors `anthropic-sse-parsing.test.ts::repairs malformed SSE JSON and
    // malformed streamed tool JSON`. The partial_json delta carries `\H` (an
    // invalid JSON escape) and `\t` (a valid escape). The mapper must repair
    // both layers (outer SSE JSON + inner streaming tool JSON) and end with
    // `stop_reason: ToolUse` + parseable args.
    #[tokio::test]
    async fn repairs_malformed_streamed_tool_json() {
        // TS `String.raw` keeps the backslashes literal: `\H` and `\t` are
        // backslash-H and backslash-t in the raw bytes, NOT an invalid escape
        // pre-decode. The Rust raw string r#"..."# reproduces that exactly.
        let malformed_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1\tcol2\"}"}}"#;

        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_test",
                        "usage": {
                            "input_tokens": 12,
                            "output_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "cache_creation_input_tokens": 0,
                        },
                    },
                })),
            ),
            frame(
                "content_block_start",
                &j(&json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_test", "name": "edit", "input": {} },
                })),
            ),
            frame("content_block_delta", malformed_delta),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 5,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::ToolUse);
        assert!(run.result.error_message.is_none());

        let toolcall = run
            .result
            .content
            .iter()
            .find_map(|c| match c {
                Content::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("a tool call block");
        assert_eq!(toolcall.arguments, json!({ "path": "A\\H", "text": "col1\tcol2" }));
        // Event sequence: start, toolcall_start, toolcall_delta, toolcall_end, done.
        assert_eq!(
            run.tags,
            vec!["start", "toolcall_start", "toolcall_delta", "toolcall_end", "done"]
        );
    }

    // Mirrors `preserves content from content_block_start events` — text +
    // thinking blocks retain their initial content and accumulate deltas;
    // signature_delta appends to the thinking signature.
    #[tokio::test]
    async fn preserves_content_from_content_block_start() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_initial_content",
                        "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                    },
                })),
            ),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "Initial text" } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": " plus delta" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 0 }))),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "thinking", "thinking": "Initial thinking", "signature": "initial signature" } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "thinking_delta", "thinking": " plus delta" } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "signature_delta", "signature": " plus delta" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 1 }))),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("first block is text"),
        };
        assert_eq!(text.text, "Initial text plus delta");
        let thinking = match &run.result.content[1] {
            Content::Thinking(t) => t,
            _ => panic!("second block is thinking"),
        };
        assert_eq!(thinking.thinking, "Initial thinking plus delta");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("initial signature plus delta")
        );
        assert!(!thinking.redacted);
    }

    // Mirrors `preserves refusal stop details from message_delta` — refusal
    // stop_reason maps to Error with the explanation.
    #[tokio::test]
    async fn preserves_refusal_stop_details() {
        let explanation = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy.";
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": { "id": "msg_01XFUDYJgAACzvnptvVoYEL", "usage": { "input_tokens": 412, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } },
                })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber", "explanation": explanation } },
                    "usage": { "input_tokens": 412, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.result.raw_stop_reason.as_deref(), Some("refusal"));
        assert_eq!(run.result.error_message.as_deref(), Some(explanation));
        // Refusal emits an Error terminal, not Done.
        assert_eq!(run.tags.last().copied(), Some("error"));
    }

    // Mirrors `preserves sensitive stop reasons with a descriptive error
    // message`.
    #[tokio::test]
    async fn preserves_sensitive_stop_reasons() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "msg_sensitive", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "sensitive" }, "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.result.raw_stop_reason.as_deref(), Some("sensitive"));
        assert_eq!(
            run.result.error_message.as_deref(),
            Some("Provider stopped with: sensitive")
        );
    }

    // Mirrors `treats message_delta without usage as a no-op for usage
    // accumulation` — usage retains message_start values; total recomputed
    // from components.
    #[tokio::test]
    async fn message_delta_without_usage_noop() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "msg_test", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 0 }))),
            // message_delta with stop_reason but NO usage object.
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        assert!(run.result.error_message.is_none());
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("text block"),
        };
        assert_eq!(text.text, "Hello");
        assert_eq!(run.result.usage.input, 12);
        assert_eq!(run.result.usage.total_tokens, 12);
    }

    // Mirrors `ignores unknown SSE events after message_stop` — `done` and
    // `proxy.stats` frames are skipped (non-ANTHROPIC_MESSAGE_EVENTS), so the
    // message still ends cleanly with Stop.
    #[tokio::test]
    async fn ignores_unknown_events_after_message_stop() {
        let mut frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "msg_test", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 0 }))),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        frames.push(frame("done", "[DONE]"));
        frames.push(frame("proxy.stats", "not json"));

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        assert!(run.result.error_message.is_none());
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("text block"),
        };
        assert_eq!(text.text, "Hello");
    }

    // ---- map_stop_reason table (anthropic-messages.ts:1326-1352) ----

    #[test]
    fn map_stop_reason_table() {
        let stop = |r: &str| map_stop_reason(r, None).unwrap();
        assert_eq!(stop("end_turn").stop_reason, StopReason::Stop);
        assert_eq!(stop("max_tokens").stop_reason, StopReason::Length);
        assert_eq!(stop("tool_use").stop_reason, StopReason::ToolUse);
        assert_eq!(stop("pause_turn").stop_reason, StopReason::Stop);
        assert_eq!(stop("stop_sequence").stop_reason, StopReason::Stop);

        // refusal with explanation.
        let details = json!({ "type": "refusal", "explanation": "blocked" });
        let refusal = map_stop_reason("refusal", Some(&details)).unwrap();
        assert_eq!(refusal.stop_reason, StopReason::Error);
        assert_eq!(refusal.error_message.as_deref(), Some("blocked"));

        // refusal without explanation → default message.
        let refusal_default = map_stop_reason("refusal", None).unwrap();
        assert_eq!(
            refusal_default.error_message.as_deref(),
            Some("The model refused to complete the request")
        );

        // sensitive → fixed message.
        let sensitive = map_stop_reason("sensitive", None).unwrap();
        assert_eq!(sensitive.stop_reason, StopReason::Error);
        assert_eq!(
            sensitive.error_message.as_deref(),
            Some("Provider stopped with: sensitive")
        );

        // unknown → error (TS throws).
        assert!(map_stop_reason("nonsense", None).is_err());
    }

    // `message_start` records the response id + initial usage (including the
    // 1h cache-write subset), mirroring anthropic-messages.ts:574-586.
    #[tokio::test]
    async fn message_start_records_response_id_and_usage() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_abc",
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 2,
                            "cache_read_input_tokens": 3,
                            "cache_creation_input_tokens": 4,
                            "cache_creation": { "ephemeral_1h_input_tokens": 1 },
                        },
                    },
                })),
            ),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.response_id.as_deref(), Some("msg_abc"));
        assert_eq!(run.result.usage.input, 10);
        assert_eq!(run.result.usage.output, 2);
        assert_eq!(run.result.usage.cache_read, 3);
        assert_eq!(run.result.usage.cache_write, 4);
        assert_eq!(run.result.usage.cache_write_1h, Some(1));
        assert_eq!(run.result.usage.total_tokens, 10 + 2 + 3 + 4);
    }

    // `message_delta` usage's `output_tokens_details.thinking_tokens` is
    // recorded as `usage.reasoning` (a subset of output_tokens).
    #[tokio::test]
    async fn message_delta_records_reasoning_tokens() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 50,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "output_tokens_details": { "thinking_tokens": 30 },
                    },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.usage.output, 50);
        assert_eq!(run.result.usage.reasoning, Some(30));
    }

    // `redacted_thinking` content_block_start produces a Thinking block with
    // the fixed marker text, the opaque payload in `thinking_signature`, and
    // `redacted: true`.
    #[tokio::test]
    async fn redacted_thinking_block() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "redacted_thinking", "data": "opaque-base64" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 0 }))),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        let thinking = match &run.result.content[0] {
            Content::Thinking(t) => t,
            _ => panic!("thinking block"),
        };
        assert!(thinking.redacted);
        assert_eq!(thinking.thinking, "[Reasoning redacted]");
        assert_eq!(thinking.thinking_signature.as_deref(), Some("opaque-base64"));
        // Redacted thinking emits a thinking_start/thinking_end pair.
        assert!(run.tags.contains(&"thinking_start"));
        assert!(run.tags.contains(&"thinking_end"));
    }

    // A stream that ends without ever setting a stop_reason (no message_delta)
    // finalizes to an Error terminal, mirroring the TS "Anthropic stream ended
    // without a stop reason" throw.
    #[tokio::test]
    async fn stream_without_stop_reason_is_error() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.tags.last().copied(), Some("error"));
    }

    // `input_json_delta` re-parses the partial JSON on every delta so the
    // partial assistant message renders args live (invariant §5.15), and the
    // final `content_block_stop` does the authoritative parse.
    #[tokio::test]
    async fn tool_call_partial_json_reparse_each_delta() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } })),
            ),
            frame(
                "content_block_start",
                &j(&json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "t1", "name": "write", "input": {} } })),
            ),
            // Two deltas building the args object incrementally.
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"a\"," } })),
            ),
            frame(
                "content_block_delta",
                &j(&json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"text\":\"b\"}" } })),
            ),
            frame("content_block_stop", &j(&json!({ "type": "content_block_stop", "index": 0 }))),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        let tc = match &run.result.content[0] {
            Content::ToolCall(t) => t,
            _ => panic!("toolcall"),
        };
        assert_eq!(tc.arguments, json!({ "path": "a", "text": "b" }));
        // Two input_json_delta frames → two toolcall_delta events + start + end.
        let deltas: Vec<&'static str> = run
            .tags
            .iter()
            .filter(|&&t| t == "toolcall_delta")
            .copied()
            .collect();
        assert_eq!(deltas.len(), 2);
    }

    // A stream that surfaces an `error` SSE event terminates with an Error
    // event carrying the SSE data as the message (parse_anthropic_event
    // rejects `event: error`).
    #[tokio::test]
    async fn sse_error_event_surfaces_error() {
        let (prod, stream) = create_assistant_message_event_stream();
        let mut prod = prod;
        let mut state = MapperState::new(
            Api::AnthropicMessages,
            "anthropic",
            "claude-haiku-4-5",
            0,
        );
        // An error frame — parse_anthropic_event returns Err.
        let err_frame = frame("error", "rate limited");
        match parse_anthropic_event(&err_frame) {
            Err(e) => emit_terminal_error(&mut prod, &mut state, e.to_string(), false),
            Ok(_) => panic!("expected error"),
        }
        let result = stream.result().await.expect("terminal");
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error_message.as_deref().unwrap().contains("rate limited"));
    }
}
