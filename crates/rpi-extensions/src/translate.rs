//! `AgentEvent → StablePluginEvent` translation, plus the [`ExtensionEmitter`]
//! that impls [`AgentEmitter`] by subscribing to the host's
//! `broadcast::Sender<AgentEvent>`, folding each event into a stable event, and
//! fan-out dispatching to every registered handler for the event's tag — all
//! dispatch wrapped in `catch_unwind` (a panicky plugin handler must not unwind
//! across FFI).
//!
//! The 10 already-emitted `AgentEvent` variants fold into their matching
//! `StablePluginEvent` tags now. The remaining `on()` tags (33 total) light up
//! as B3/B4/B5 add the emission points. Tags with no registered handlers are a
//! cheap no-op (empty slice → no dispatch).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use futures::future::BoxFuture;
use rpi_agent::events::{AgentEmitter, AgentEvent};
use rpi_plugin_sdk::{EventTag, StbString, StablePluginEvent};

use crate::host_free_string;
use crate::registry::RegistrySnapshot;

/// Map a native [`AgentEvent`] to its pi `on()` [`EventTag`], or `None` if the
/// host event has no stable-event counterpart (e.g. some internal-only variants
/// — none today, all 10 map). This is the **fold** described in the crate docs:
/// the ten `AgentEvent` variants map onto the 10 matching tags; the rest of the
/// 33-category surface is driven by direct host emission in B3+.
pub fn event_tag_for(event: &AgentEvent) -> Option<EventTag> {
    match event {
        AgentEvent::AgentStart => Some(EventTag::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(EventTag::AgentEnd),
        AgentEvent::TurnStart => Some(EventTag::TurnStart),
        AgentEvent::TurnEnd { .. } => Some(EventTag::TurnEnd),
        AgentEvent::MessageStart { .. } => Some(EventTag::MessageStart),
        AgentEvent::MessageUpdate { .. } => Some(EventTag::MessageUpdate),
        AgentEvent::MessageEnd { .. } => Some(EventTag::MessageEnd),
        AgentEvent::ToolExecutionStart { .. } => Some(EventTag::ToolExecutionStart),
        AgentEvent::ToolExecutionUpdate { .. } => Some(EventTag::ToolExecutionUpdate),
        AgentEvent::ToolExecutionEnd { .. } => Some(EventTag::ToolExecutionEnd),
    }
}

/// Serialize an [`AgentMessage`](rpi_agent::message::AgentMessage) to an owning
/// `StbString` for a message-payload event. The host produced this string, so
/// the **handler** (plugin side) frees it via the host's `free_string`.
fn message_to_stb(message: &rpi_agent::message::AgentMessage) -> StbString {
    // AgentMessage is Serialize (#[serde(tag="kind")]); use serde_json round-trip.
    let text = serde_json::to_string(message).unwrap_or_else(|_| "null".to_string());
    StbString::from_string(text)
}

/// Build the stable event for an `AgentEvent`, or `None` if the event carries
/// no payload that maps onto a stable variant (always maps today, but kept as
/// `Option` for forward-compat with unmapped future variants). Ownership of any
/// `StbString` in the returned event passes to the handler.
pub fn translate(event: &AgentEvent) -> Option<StablePluginEvent> {
    let tag = event_tag_for(event)?;
    match event {
        AgentEvent::MessageStart { message }
        | AgentEvent::MessageUpdate { message, .. }
        | AgentEvent::MessageEnd { message } => {
            let stb = message_to_stb(message);
            Some(StablePluginEvent::message(tag, stb))
        }

        AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args }
        | AgentEvent::ToolExecutionUpdate { tool_call_id, tool_name, args, .. } => {
            Some(StablePluginEvent::tool_call(
                tag,
                StbString::from_string(tool_call_id.clone()),
                StbString::from_string(tool_name.clone()),
                StbString::from_string(serde_json::to_string(args).unwrap_or_else(|_| "null".into())),
            ))
        }

        AgentEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error } => {
            // Serialize the AgentToolResult to JSON for the result payload.
            let result_json = agent_tool_result_to_json(result);
            Some(StablePluginEvent::tool_result(
                tag,
                StbString::from_string(tool_call_id.clone()),
                StbString::from_string(tool_name.clone()),
                StbString::from_string(result_json),
                *is_error,
            ))
        }

        // No-payload events.
        AgentEvent::AgentStart
        | AgentEvent::TurnStart
        | AgentEvent::AgentEnd { .. }
        | AgentEvent::TurnEnd { .. } => Some(StablePluginEvent::empty(tag)),
    }
}

/// Serialize an `AgentToolResult` to the JSON shape handlers expect (same shape
/// the tool adapter uses). Kept here (not reusing tool.rs's `result_to_stb`)
/// because the emitter must not depend on adapter internals, and this is the
/// event-side direction.
fn agent_tool_result_to_json(result: &rpi_agent::types::AgentToolResult) -> String {
    let mut txt = String::new();
    txt.push('{');
    txt.push_str("\"content\":[");
    for (i, c) in result.content.iter().enumerate() {
        if i > 0 {
            txt.push(',');
        }
        match c {
            rpi_agent::types::TextContentOrImage::Text(t) => {
                txt.push_str(
                    &serde_json::to_string(&serde_json::json!({ "type": "text", "text": t.text }))
                        .unwrap_or_else(|_| "\"\"".into()),
                );
            }
            rpi_agent::types::TextContentOrImage::Image(img) => {
                txt.push_str(
                    &serde_json::to_string(&serde_json::json!({
                        "type": "image",
                        "data": img.data,
                        "mimeType": img.mime_type,
                    }))
                    .unwrap_or_else(|_| "\"\"".into()),
                );
            }
        }
    }
    txt.push(']');
    txt.push_str(",\"details\":");
    txt.push_str(&serde_json::to_string(&result.details).unwrap_or_else(|_| "null".into()));
    txt.push_str(",\"terminate\":");
    txt.push_str(if result.terminate { "true" } else { "false" });
    txt.push_str(",\"addedToolNames\":");
    txt.push_str(&serde_json::to_string(&result.added_tool_names).unwrap_or_else(|_| "[]".into()));
    txt.push('}');
    txt
}

/// Free every `StbString` owned by a dispatched [[`StablePluginEvent`]] via the
/// host's `free_string`. Called after dispatch completes (the handler SHOULD
/// have freed them, but the host keeps ownership-of-cleanup so a buggy plugin
/// that retains/leaks can't double-free the host's allocation — the host never
/// trusts the plugin to free, per the SDK's "receiver owns" contract: here the
/// host is the producer → receiver is the plugin; if the plugin failed to free,
/// the host's free is a leak fix, not a double-free, because `StbString` is
/// `Copy` and the host's `host_free_string` reconstructs the `Box<[u8]>` and
/// drops it — a second free of the same bytes would be UB, so this must run
/// EXACTLY ONCE per event. The contract is: the plugin frees what it receives.
/// To honor that, we do NOT free here; the plugin owns the free. This fn is
/// therefore a no-op kept for documentation + future audit).
///
/// **In practice:** the plugin handler is contractually required to free every
/// `StbString` it receives via the host `free_string`. The host does not
/// double-free. If a plugin leaks, that is a plugin bug.
#[allow(dead_code)]
fn free_event_strings(_event: &StablePluginEvent) {
    // intentionally empty — see doc comment.
}

// ===========================================================================
// ExtensionEmitter — AgentEmitter impl fanning out to plugin handlers
// ===========================================================================

/// An [`AgentEmitter`] that fans each [`AgentEvent`] out to every plugin handler
/// registered for the event's tag. Built from a [`RegistrySnapshot`] (so it
/// shares the registry's staleness flag) and installed into
/// `AgentHarnessOptions.agent_emitter` **alongside** the host's
/// [`BroadcastEmitter`] — events flow to BOTH the TUI (which drains the
/// broadcast receiver) and the plugin handlers (which receive translated
/// `StablePluginEvent`s). The host composes the two via
/// [`TeeEmitter`](super::TeeEmitter); this emitter alone only dispatches to
/// plugins.
///
/// Dispatch is `catch_unwind`-wrapped: a panicking plugin handler is logged and
/// skipped (abort-on-unwind policy would be too aggressive for event dispatch
/// where one bad handler shouldn't kill the session; we log + continue, unlike
/// tool-partial dispatch which aborts because it cannot unwind across FFI from
/// a blocking thread). The distinction: `emit` runs on the async runtime thread
/// where `catch_unwind` can recover cleanly; the tool partial cb runs inside
/// `spawn_blocking` cross-FFI where recovery is unsafe.
pub struct ExtensionEmitter {
    snapshot: Arc<RegistrySnapshot>,
    /// Keeps the loaded cdylibs mapped for as long as this emitter (installed
    /// into `AgentHarnessOptions.agent_emitter`) may dispatch to handler fn
    /// pointers that live inside them. Cloned from the load session; the
    /// libraries unload only when every holder (adapter + emitter) drops.
    #[allow(dead_code)]
    keepalive: Arc<crate::PluginKeepalive>,
}

impl ExtensionEmitter {
    /// Build an emitter over a snapshot. The `keepalive` keeps the cdylibs that
    /// own the snapshot's handler fn pointers mapped for the emitter's lifetime.
    pub fn new(snapshot: Arc<RegistrySnapshot>, keepalive: Arc<crate::PluginKeepalive>) -> Self {
        Self { snapshot, keepalive }
    }

    /// Dispatch one stable event to all handlers for its tag. Each handler call
    /// is `catch_unwind`-wrapped; errors (nonzero return) are logged but do not
    /// abort the fan-out (one handler's failure doesn't block the others — mir-
    /// rors pi's per-handler try/catch).
    fn dispatch(&self, event: &StablePluginEvent) {
        // Staleness guard: a stale registry (swapped-out session) does nothing.
        if !crate::registry::assert_active(self.snapshot.active_flag()) {
            return;
        }
        let handlers = self.snapshot.handlers_for(event.tag);
        if handlers.is_empty() {
            // No subscribers — and critically, the event's StbStrings are owned
            // by the host and must still be freed (no handler ran to free them).
            free_dispatched_event(event);
            return;
        }
        for h in handlers {
            // SAFETY: the plugin warrants `handler` + `user_data` are safe to
            // call from this thread. catch_unwind so a panic cannot cross FFI.
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                // The event is Copy (StbString ptr+len); the handler receives a
                // copy and owns freeing its strings. We pass the same `event`
                // copy to each handler — but each handler is contractually
                // responsible for freeing, so only ONE handler may free. v1
                // contract: the LAST handler frees (or, more simply, the host
                // frees after the fan-out and handlers MUST NOT free — see the
                // free_dispatched_event path below). To avoid ambiguity we
                // adopt: **handlers MUST NOT free event strings; the host frees
                // exactly once after dispatch**. This is the safer default and
                // matches "host is producer, host owns cleanup when the plugin
                // doesn't". Documented in the SDK event-handler contract.
                (h.handler)(*event, h.user_data)
            }));
            match outcome {
                Ok(rc) if rc != 0 => {
                    tracing::warn!(tag = ?event.tag, rc, "extension event handler returned nonzero");
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::error!(tag = ?event.tag, "extension event handler panicked — skipped");
                }
            }
        }
        // Host frees the event's strings exactly once after the fan-out.
        free_dispatched_event(event);
    }
}

impl AgentEmitter for ExtensionEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        // Translate + dispatch synchronously (the emitter's emit is awaited by
        // the loop in event order; dispatch is non-blocking fn-pointer calls).
        // If the event maps to no tag, do nothing.
        if let Some(stable) = translate(&event) {
            self.dispatch(&stable);
        }
        Box::pin(async {})
    }

    fn try_emit(&self, event: AgentEvent) {
        if let Some(stable) = translate(&event) {
            self.dispatch(&stable);
        }
    }
}

// ===========================================================================
// TeeEmitter — fan an AgentEvent out to N AgentEmitters (host + plugins)
// ===========================================================================

/// An [`AgentEmitter`] that forwards every event to each of its children, in
/// registration order. The host builds one around `[BroadcastEmitter (→ TUI),
/// ExtensionEmitter (→ plugin handlers)]` so a single `AgentHarnessOptions
/// .agent_emitter` slot feeds both consumers: the TUI keeps rendering from its
/// broadcast receiver, and plugin `on()` handlers receive translated
/// `StablePluginEvent`s.
///
/// `emit` awaits each child in turn (the loop awaits `emit`, so order matches
/// registration); `try_emit` calls each child's `try_emit` (the tool
/// `on_update` path — non-blocking). A child that panics is isolated by the
/// child's own `catch_unwind` where applicable (the `ExtensionEmitter` does);
/// the `BroadcastEmitter` cannot panic (it's a `tx.send`). We do NOT wrap the
/// fan-out itself in `catch_unwind` — each child is responsible for its own
/// soundness, and a generic wrapper would mask a child's contract violation.
pub struct TeeEmitter {
    emitters: Vec<Arc<dyn AgentEmitter>>,
}

impl TeeEmitter {
    /// Build a tee over the given emitters. Order is preserved: `emit`/`try_emit`
    /// visit them front-to-back. A single-child tee is a trivial passthrough
    /// (the host uses that when no extensions loaded, so the code path is
    /// uniform).
    pub fn new(emitters: Vec<Arc<dyn AgentEmitter>>) -> Self {
        Self { emitters }
    }
}

impl AgentEmitter for TeeEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        // We can't hold `&self` across an await boundary into a 'static future
        // cheaply here without cloning the Arcs — so clone them and drive the
        // fan-out inside a pinned async block. Each child's emit returns a
        // no-op future (both BroadcastEmitter and ExtensionEmitter complete
        // synchronously), so this is effectively a synchronous loop in practice.
        let emitters = self.emitters.clone();
        Box::pin(async move {
            for e in &emitters {
                e.emit(event.clone()).await;
            }
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        for e in &self.emitters {
            e.try_emit(event.clone());
        }
    }
}

/// Free every owning `StbString` in a dispatched event exactly once via the
/// host's `free_string`. Called by [`ExtensionEmitter::dispatch`] after the
/// fan-out completes. Handlers MUST NOT free event strings (host owns cleanup).
fn free_dispatched_event(event: &StablePluginEvent) {
    use rpi_plugin_sdk::EventTag as T;
    match event.tag {
        T::MessageStart | T::MessageUpdate | T::MessageEnd => {
            // SAFETY: tag matches the message variant.
            unsafe { host_free_string(event.payload.message.message) };
        }
        T::ToolCall | T::ToolExecutionStart | T::ToolExecutionUpdate => {
            // SAFETY: tag matches the tool_call variant.
            unsafe {
                let tc = &event.payload.tool_call;
                host_free_string(tc.tool_call_id);
                host_free_string(tc.tool_name);
                host_free_string(tc.params);
            }
        }
        T::ToolResult | T::ToolExecutionEnd => {
            // SAFETY: tag matches the tool_result variant.
            unsafe {
                let tr = &event.payload.tool_result;
                host_free_string(tr.tool_call_id);
                host_free_string(tr.tool_name);
                host_free_string(tr.result);
            }
        }
        T::ProjectTrust
        | T::ResourcesDiscover
        | T::SessionStart
        | T::SessionInfoChanged
        | T::SessionBeforeSwitch
        | T::SessionBeforeFork
        | T::SessionBeforeCompact
        | T::SessionCompact
        | T::SessionShutdown
        | T::SessionBeforeTree
        | T::SessionTree
        | T::Context
        | T::BeforeProviderRequest
        | T::BeforeProviderHeaders
        | T::AfterProviderResponse
        | T::BeforeAgentStart
        | T::AgentStart
        | T::AgentEnd
        | T::AgentSettled
        | T::TurnStart
        | T::TurnEnd
        | T::ModelSelect
        | T::ThinkingLevelSelect
        | T::UserBash
        | T::Input => {
            // no payload or data payload (we never construct data payloads from
            // AgentEvent today). If a future emitter builds a data payload, add
            // a free here.
        }
    }
}

// SAFETY note on the `unsafe { host_free_string(...) }` calls above:
// `host_free_string` is itself a safe `extern "C" fn` (it reconstructs a
// `Box<[u8]>` from ptr+len and drops it, idempotent on null/empty). The
// `unsafe` block is required only because reading the union payload is
// `unsafe` (the compiler can't verify tag/variant match) — which we guarantee
// by matching on `event.tag` first. So the union read is sound.

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::message::AgentMessage;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{AssistantMessage, Usage};
    use rpi_plugin_sdk::EventTag;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // The two handler-fan-out tests share the process-global `HANDLER_HITS`
    // counter (the handler is an `extern "C" fn` that can't capture per-test
    // state). Parallel #[test] execution would have one test's `store(0)`
    // wipe the other's in-flight increment. Hold this lock for the ENTIRE
    // body of both tests so their reset/dispatch windows don't overlap. The
    // dispatch is synchronous (fn-pointer calls), so the guard is released
    // before the test returns — no handler outlives the test.
    static HANDLER_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn all_ten_agent_events_map_to_a_tag() {
        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        let events = vec![
            AgentEvent::AgentStart,
            AgentEvent::AgentEnd { messages: vec![] },
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd {
                message: am.clone(),
                tool_results: vec![],
            },
            AgentEvent::MessageStart { message: am.clone() },
            AgentEvent::MessageUpdate {
                message: am.clone(),
                assistant_message_event: rpi_ai::types::AssistantMessageEvent::Start {
                    partial: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
                },
            },
            AgentEvent::MessageEnd { message: am },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
                partial_result: std::sync::Arc::new(AgentToolResult::text("...")),
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                result: AgentToolResult::text("done"),
                is_error: false,
            },
        ];
        for e in &events {
            assert!(event_tag_for(e).is_some(), "event {:?} should map", e.type_tag());
        }
        // Spot-check the fold targets.
        assert_eq!(event_tag_for(&events[0]), Some(EventTag::AgentStart));
        assert_eq!(event_tag_for(&events[2]), Some(EventTag::TurnStart));
        assert_eq!(
            event_tag_for(&events[7]),
            Some(EventTag::ToolExecutionStart)
        );
    }

    #[test]
    fn translate_message_end_produces_message_payload() {
        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        let ev = AgentEvent::MessageEnd { message: am };
        let stable = translate(&ev).expect("maps");
        assert_eq!(stable.tag, EventTag::MessageEnd);
        // free the payload string (host owns cleanup).
        // SAFETY: tag == MessageEnd.
        unsafe { host_free_string(stable.payload.message.message) };
    }

    // --- an emitter fan-out test with an in-process handler -----------------

    static HANDLER_HITS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn counting_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        HANDLER_HITS.fetch_add(1, Ordering::SeqCst);
        0
    }

    #[test]
    fn emitter_dispatches_to_registered_handlers() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let emitter = ExtensionEmitter::new(snap, crate::loader::PluginKeepalive::empty());

        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        // Use try_emit (sync) — no runtime needed.
        emitter.try_emit(AgentEvent::MessageEnd { message: am });
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 1);

        // Stale registry → no dispatch.
        reg.invalidate();
        let am2 = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        emitter.try_emit(AgentEvent::MessageEnd { message: am2 });
        assert_eq!(
            HANDLER_HITS.load(Ordering::SeqCst),
            1,
            "stale registry must not dispatch"
        );
    }

    // --- TeeEmitter: fan-out to both the broadcast + the extension emitter ----

    #[tokio::test]
    async fn tee_emitter_fans_out_to_every_child() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        use rpi_agent::events::{AgentEmitter, CollectorEmitter};

        // Two collector emitters + record how many plugin handler hits land.
        let (collector_a, events_a) = CollectorEmitter::new();
        let (collector_b, events_b) = CollectorEmitter::new();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(EventTag::MessageEnd, counting_handler, std::ptr::null_mut());
        let snap = Arc::new(reg.snapshot());
        let ext = ExtensionEmitter::new(snap, crate::loader::PluginKeepalive::empty());

        let tee = TeeEmitter::new(vec![
            Arc::new(collector_a),
            Arc::new(collector_b),
            Arc::new(ext),
        ]);

        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        // Broadcast path: both collectors receive the event, and the extension
        // emitter dispatches to the one registered handler.
        tee.emit(AgentEvent::MessageEnd { message: am }).await;
        assert_eq!(events_a.lock().unwrap().len(), 1, "collector A got the event");
        assert_eq!(events_b.lock().unwrap().len(), 1, "collector B got the event");
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 1, "plugin handler fired once");
    }
}
