//! `AgentEvent → StablePluginEvent` translation, plus the [`ExtensionEmitter`]
//! that impls [`AgentEmitter`] by subscribing to the host's
//! `broadcast::Sender<AgentEvent>`, folding each event into a stable event, and
//! fan-out dispatching to every registered handler for the event's tag — all
//! dispatch wrapped in `catch_unwind` (a panicky plugin handler must not unwind
//! across FFI).
//!
//! The 10 stable lifecycle `AgentEvent` variants fold into their matching
//! `StablePluginEvent` tags. Internal host events such as retry scheduling are
//! intentionally not projected onto the fixed 33-tag plugin ABI.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use futures::future::BoxFuture;
use rpi_agent::events::{AgentEmitter, AgentEvent};
use rpi_plugin_sdk::{EventTag, StablePluginEvent, StbString};

use crate::host_free_string;
use crate::registry::RegistrySnapshot;

/// Map a native [`AgentEvent`] to its pi `on()` [`EventTag`], or `None` if the
/// host event has no stable-event counterpart. This is the **fold** described
/// in the crate docs: the ten stable lifecycle variants map onto matching tags;
/// internal retry scheduling stays host-only, and the rest of the 33-category
/// surface is driven by direct host emission in B3+.
pub fn event_tag_for(event: &AgentEvent) -> Option<EventTag> {
    match event {
        AgentEvent::AgentStart => Some(EventTag::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(EventTag::AgentEnd),
        AgentEvent::RetryScheduled { .. } => None,
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
        AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
            let stb = message_to_stb(message);
            Some(StablePluginEvent::message(tag, stb))
        }
        AgentEvent::MessageUpdate { message, .. } => {
            // The partial arrives as a shared `Arc<AssistantMessage>`; serialize it
            // in the same `AgentMessage` shape the other two arms use, without
            // materializing a copy of the growing message per delta.
            let stb =
                StbString::from_string(rpi_agent::message::assistant_json(message).to_string());
            Some(StablePluginEvent::message(tag, stb))
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        }
        | AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            ..
        } => Some(StablePluginEvent::tool_call(
            tag,
            StbString::from_string(tool_call_id.clone()),
            StbString::from_string(tool_name.clone()),
            StbString::from_string(serde_json::to_string(args).unwrap_or_else(|_| "null".into())),
        )),

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
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

        AgentEvent::RetryScheduled { .. } => None,
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
        Self {
            snapshot,
            keepalive,
        }
    }

    /// Dispatch one stable event to all handlers for its tag. Each handler call
    /// is `catch_unwind`-wrapped; errors (nonzero return) are logged but do not
    /// abort the fan-out (one handler's failure doesn't block the others — mir-
    /// rors pi's per-handler try/catch).
    fn dispatch(&self, event: &StablePluginEvent) {
        dispatch_to_handlers(&self.snapshot, event);
    }
}

/// Fan an already-built event out to the tag's handlers (or free it when no
/// handler subscribes). Shared by [`ExtensionEmitter`] and the provider-hook
/// dispatcher ([`crate::provider_hooks`]). Handlers MUST NOT free the event
/// strings; the host frees exactly once after the fan-out.
pub fn dispatch_to_handlers(snapshot: &RegistrySnapshot, event: &StablePluginEvent) {
    // Staleness guard: a stale registry (swapped-out session) does nothing.
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return;
    }
    let handlers = snapshot.handlers_for(event.tag);
    if handlers.is_empty() {
        // No subscribers — and critically, the event's StbStrings are owned
        // by the host and must still be freed (no handler ran to free them).
        free_dispatched_event(event);
        return;
    }
    for h in handlers {
        // P2: skip a handler whose plugin declared platforms excluding this host.
        if !crate::registry::platform_allows(&h.platforms) {
            crate::event_log::log_handler_invocation(
                event.tag,
                &h.plugin,
                "PlatformSkip",
                0,
                Some(format!("host={}", crate::registry::current_platform())),
            );
            continue;
        }
        // SAFETY: the plugin warrants `handler` + `user_data` are safe to
        // call from this thread. catch_unwind so a panic cannot cross FFI.
        let started = std::time::Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| (h.handler)(*event, h.user_data)));
        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(rc) if rc != 0 => {
                tracing::warn!(tag = ?event.tag, rc, "extension event handler returned nonzero");
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Error",
                    duration_ms,
                    Some(format!("rc={rc}")),
                );
            }
            Ok(_) => {
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Continue",
                    duration_ms,
                    None,
                );
            }
            Err(_) => {
                tracing::error!(tag = ?event.tag, "extension event handler panicked — skipped");
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Panic",
                    duration_ms,
                    None,
                );
            }
        }
    }
    // Host frees the event's strings exactly once after the fan-out.
    free_dispatched_event(event);
}

/// Build + dispatch a generic `data` event (JSON payload) to the tag's
/// handlers. Returns whether any handler was invoked. Used by the B4
/// provider-hook observer path ([`crate::provider_hooks`]) which has no
/// matching `AgentEvent` to translate.
pub fn dispatch_data_event(snapshot: &RegistrySnapshot, tag: EventTag, data: &str) -> bool {
    let handlers = snapshot.handlers_for(tag);
    if handlers.is_empty() {
        return false;
    }
    let event = StablePluginEvent::data(tag, StbString::from_string(data.to_string()));
    dispatch_to_handlers(snapshot, &event);
    true
}

/// Build + dispatch a **no-payload** event (e.g. `session_start`, `session_shutdown`,
/// the rpi-specific `before_tui_start`) to the tag's handlers. Returns whether any
/// handler was invoked. Mirrors [`dispatch_data_event`] for events whose payload
/// is `EventEmpty`; a handler that subscribes to a no-payload tag receives the
/// event with an empty payload.
pub fn dispatch_empty_event(snapshot: &RegistrySnapshot, tag: EventTag) -> bool {
    let handlers = snapshot.handlers_for(tag);
    if handlers.is_empty() {
        return false;
    }
    let event = StablePluginEvent::empty(tag);
    dispatch_to_handlers(snapshot, &event);
    true
}

/// Per-lifecycle-event handler timeout budget (mirrors lifescope §6.1):
/// startup 5s, session shutdown 15s, everything else 10s. A handler that
/// exceeds its budget is logged + skipped (it keeps running on its blocking
/// thread, but no longer gates startup/shutdown).
pub fn lifecycle_timeout_for(tag: EventTag) -> std::time::Duration {
    use rpi_plugin_sdk::EventTag as T;
    match tag {
        T::BeforeTuiStart => std::time::Duration::from_secs(5),
        T::SessionShutdown => std::time::Duration::from_secs(15),
        _ => std::time::Duration::from_secs(10),
    }
}

/// Invoke one no-payload lifecycle handler, catching unwinds at the FFI boundary.
///
/// Takes [`RegisteredHandler`](crate::registry::RegisteredHandler) **by value**
/// so the caller's `spawn_blocking` closure captures the whole struct (which has
/// `unsafe impl Send`) rather than its disjoint fields: `user_data` is a raw
/// `*mut c_void` and is NOT `Send` on its own (Rust 2021 disjoint closure
/// capture would otherwise move the raw pointer directly into the closure env
/// and fail the `Send` bound).
fn call_lifecycle_handler(
    h: crate::registry::RegisteredHandler,
    tag: EventTag,
) -> std::thread::Result<i32> {
    let event = StablePluginEvent::empty(tag);
    // SAFETY: the plugin warrants `h.handler` + `h.user_data` are safe to call
    // from any thread; catch_unwind so a panic cannot cross FFI.
    catch_unwind(AssertUnwindSafe(|| (h.handler)(event, h.user_data)))
}

/// Veto-aware, timeout-bounded dispatch for a **no-payload lifecycle event**
/// (`BeforeTuiStart` / `SessionStart` / `SessionShutdown`).
///
/// Unlike [`dispatch_empty_event`] (fire-and-forget observe fan-out), this:
///
/// 1. Runs each handler on a blocking thread with a per-event timeout
///    ([`lifecycle_timeout_for`]) — a hung handler is logged + skipped, it
///    does not stall startup or shutdown.
/// 2. Honors the SDK's [`EVENT_HANDLER_ABORT`] veto code: the first handler to
///    return it stops the fan-out and this returns `Some(reason)`.
///
/// Returns `None` when no handler vetoed (continue the lifecycle); `Some` when
/// a handler aborted. A stale registry (swapped-out session) returns `None`.
/// The **caller** decides what a veto means for the phase — e.g. a
/// `BeforeTuiStart` veto aborts startup before the TUI initializes.
pub async fn dispatch_lifecycle_event(
    snapshot: &RegistrySnapshot,
    tag: EventTag,
) -> Option<String> {
    // Staleness guard: a stale registry (swapped-out session) does nothing.
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return None;
    }
    let handlers = snapshot.handlers_for(tag).to_vec();
    if handlers.is_empty() {
        return None;
    }
    // Lifecycle tags are no-payload: the event is `EventEmpty`. We build it
    // inside the helper (via `spawn_blocking`) so the closure env only carries
    // Send values.
    let timeout = lifecycle_timeout_for(tag);
    for h in handlers {
        // P2: skip a handler whose plugin declared platforms excluding this host.
        if !crate::registry::platform_allows(&h.platforms) {
            crate::event_log::log_handler_invocation(
                tag,
                &h.plugin,
                "PlatformSkip",
                0,
                Some(format!("host={}", crate::registry::current_platform())),
            );
            continue;
        }
        // Capture the owning extension's display name for the veto message
        // before the handler is moved into the blocking closure.
        let plugin_name = h.plugin.clone();
        let h_task = h;
        let tag_task = tag;
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || call_lifecycle_handler(h_task, tag_task)),
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(Ok(rc))) if rc == rpi_plugin_sdk::EVENT_HANDLER_ABORT => {
                tracing::warn!(
                    tag = ?tag,
                    plugin = %plugin_name,
                    "extension handler vetoed lifecycle event"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Abort",
                    duration_ms,
                    None,
                );
                return Some(format!("extension `{plugin_name}` vetoed {tag:?}"));
            }
            Ok(Ok(Ok(rc))) => {
                let result = if rc == 0 { "Continue" } else { "Error" };
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    result,
                    duration_ms,
                    if rc == 0 {
                        None
                    } else {
                        Some(format!("rc={rc}"))
                    },
                );
            }
            Ok(Ok(Err(_))) => {
                tracing::error!(tag = ?tag, "extension handler panicked — skipped");
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Panic",
                    duration_ms,
                    None,
                );
            }
            Ok(Err(join_err)) => {
                tracing::error!(
                    tag = ?tag,
                    error = %join_err,
                    "extension handler task join failed — skipped"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "JoinFailed",
                    duration_ms,
                    Some(join_err.to_string()),
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    tag = ?tag,
                    timeout_ms = timeout.as_millis(),
                    "extension handler timed out — skipped (still running on its blocking thread)"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Timeout",
                    duration_ms,
                    Some(format!("budget_ms={}", timeout.as_millis())),
                );
            }
        }
    }
    None
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
        | T::BeforeAgentStart
        | T::AgentStart
        | T::AgentEnd
        | T::AgentSettled
        | T::TurnStart
        | T::TurnEnd
        | T::ModelSelect
        | T::ThinkingLevelSelect
        | T::UserBash
        | T::Input
        | T::BeforeTuiStart
        | T::UiPromptStart
        | T::UiPromptEnd => {
            // no payload today.
        }
        // The B4 provider-hook observer events carry a generic data payload
        // (built by `dispatch_data_event`); free the single StbString.
        T::BeforeProviderRequest | T::BeforeProviderHeaders | T::AfterProviderResponse => {
            // SAFETY: these tags are only ever constructed as data payloads.
            unsafe { host_free_string(event.payload.data.data) };
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
            AgentEvent::MessageStart {
                message: am.clone(),
            },
            AgentEvent::MessageUpdate {
                // `MessageUpdate` carries the shared partial snapshot, so build
                // the same `Arc` the provider would have handed over.
                message: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
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
            assert!(
                event_tag_for(e).is_some(),
                "event {:?} should map",
                e.type_tag()
            );
        }
        // Spot-check the fold targets.
        assert_eq!(event_tag_for(&events[0]), Some(EventTag::AgentStart));
        assert_eq!(event_tag_for(&events[2]), Some(EventTag::TurnStart));
        assert_eq!(
            event_tag_for(&events[7]),
            Some(EventTag::ToolExecutionStart)
        );
        assert!(event_tag_for(&AgentEvent::RetryScheduled {
            attempt: 1,
            max_retries: 10,
            delay_ms: 2_000,
            error: "503".into(),
        })
        .is_none());
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
            "test-plugin".to_string(),
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
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
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
        assert_eq!(
            events_a.lock().unwrap().len(),
            1,
            "collector A got the event"
        );
        assert_eq!(
            events_b.lock().unwrap().len(),
            1,
            "collector B got the event"
        );
        assert_eq!(
            HANDLER_HITS.load(Ordering::SeqCst),
            1,
            "plugin handler fired once"
        );
    }

    // --- dispatch_empty_event: no-payload lifecycle events (P0 session events)

    /// Verifies `dispatch_empty_event` fans a no-payload event (e.g. the
    /// rpi-specific `BeforeTuiStart` / session lifecycle tags) to every
    /// subscriber and reports whether any handler was invoked.
    #[test]
    fn dispatch_empty_event_fans_out_to_subscribers() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::SessionStart,
            counting_handler,
            std::ptr::null_mut(),
        );
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeTuiStart,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());

        // No subscriber for SessionShutdown → no handler invoked.
        assert!(!dispatch_empty_event(&snap, EventTag::SessionShutdown));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 0);

        // SessionStart + BeforeTuiStart both have one subscriber each.
        assert!(dispatch_empty_event(&snap, EventTag::SessionStart));
        assert!(dispatch_empty_event(&snap, EventTag::BeforeTuiStart));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 2);

        // Stale registry (session swapped): dispatch is a silent no-op inside
        // `dispatch_to_handlers` — no handler is invoked — though the fn still
        // reports subscribers existed (matches `dispatch_data_event` semantics).
        reg.invalidate();
        assert!(dispatch_empty_event(&snap, EventTag::SessionStart));
        assert!(dispatch_empty_event(&snap, EventTag::BeforeTuiStart));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 2);
    }

    // --- dispatch_lifecycle_event: veto + timeout (P1) ---

    /// A lifecycle handler that vetoes (returns the SDK abort code).
    extern "C" fn aborting_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_ABORT
    }

    /// A lifecycle handler that returns success (`0`) — no veto.
    extern "C" fn continue_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_CONTINUE
    }

    /// A lifecycle handler that returns a generic handled error — no veto.
    extern "C" fn error_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_ERROR
    }

    #[test]
    fn lifecycle_timeout_budget_matches_spec() {
        assert_eq!(
            lifecycle_timeout_for(EventTag::BeforeTuiStart),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            lifecycle_timeout_for(EventTag::SessionShutdown),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            lifecycle_timeout_for(EventTag::SessionStart),
            std::time::Duration::from_secs(10)
        );
    }

    /// A handler returning `EVENT_HANDLER_ABORT` vetoes the phase; the returned
    /// reason names the owning extension.
    #[tokio::test]
    async fn dispatch_lifecycle_event_honors_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "veto-ext".to_string(),
            EventTag::BeforeTuiStart,
            aborting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let result = dispatch_lifecycle_event(&snap, EventTag::BeforeTuiStart).await;
        assert_eq!(
            result.as_deref(),
            Some("extension `veto-ext` vetoed BeforeTuiStart")
        );
    }

    /// A `0` (continue) or generic nonzero (handled error) return does NOT veto;
    /// the fan-out still visits every handler and returns `None`.
    #[tokio::test]
    async fn dispatch_lifecycle_event_continue_and_error_do_not_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "ok-ext".to_string(),
            EventTag::SessionStart,
            continue_handler,
            std::ptr::null_mut(),
        );
        reg.register_event_handler(
            "err-ext".to_string(),
            EventTag::SessionStart,
            error_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        assert!(dispatch_lifecycle_event(&snap, EventTag::SessionStart)
            .await
            .is_none());
    }

    /// No subscribers → no veto, no work.
    #[tokio::test]
    async fn dispatch_lifecycle_event_no_subscribers_is_none() {
        let reg = crate::registry::ExtensionRegistry::new();
        let snap = Arc::new(reg.snapshot());
        assert!(dispatch_lifecycle_event(&snap, EventTag::BeforeTuiStart)
            .await
            .is_none());
    }

    /// A stale registry (swapped-out session) is a no-op — a veto from a stale
    /// handler must NOT abort the new session's startup.
    #[tokio::test]
    async fn dispatch_lifecycle_event_stale_is_none() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "veto-ext".to_string(),
            EventTag::BeforeTuiStart,
            aborting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        reg.invalidate();
        assert!(dispatch_lifecycle_event(&snap, EventTag::BeforeTuiStart)
            .await
            .is_none());
    }

    /// The observe fan-out records one JSONL line per handler when the global
    /// event logger is enabled.
    #[test]
    fn dispatch_records_handler_invocation_to_event_log() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        crate::event_log::set_event_logger_for_test(crate::event_log::EventLogger::open(
            path.clone(),
        ));

        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "log-ext".to_string(),
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let event = StablePluginEvent::message(
            EventTag::MessageEnd,
            StbString::from_string("m".to_string()),
        );
        dispatch_to_handlers(&snap, &event);

        // Restore the disabled default so later tests don't write into the
        // (soon-deleted) temp file.
        crate::event_log::set_event_logger_for_test(crate::event_log::EventLogger::disabled());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""event":"MessageEnd""#), "log: {text}");
        assert!(text.contains(r#""plugin":"log-ext""#), "log: {text}");
        assert!(text.contains(r#""result":"Continue""#), "log: {text}");
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 1);
    }
}
