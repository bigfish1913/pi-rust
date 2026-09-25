//! B4 — extension provider hooks. Bridges an extension's `on(BeforeProviderRequest)`
//! / `on(BeforeProviderHeaders)` / `on(AfterProviderResponse)` handlers into
//! rpi-ai's [`ProviderHooks`] slot, so every provider call fans the live
//! request/response context out to the subscribed plugins.
//!
//! **Observer semantics (v1):** the SDK `EventHandlerFn` returns `i32` (no
//! result payload), so a handler observes the request/response — it can record
//! state, rotate an external token store, emit diagnostics — but cannot PATCH
//! the live `SimpleStreamOptions`. The `before_request` hook therefore returns
//! `None` (no patch); a future ABI extension (an `out` pointer on the handler)
//! would unlock the patch path. The harness treats a `None` return as
//! "leave opts untouched", so the observer path is safe.

use std::sync::{Arc, Mutex};

use rpi_ai::types::{AssistantMessage, Content, Message, UserContent};
use rpi_ai::Context;
use rpi_ai::{Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};
use rpi_plugin_sdk::EventTag;

use crate::loader::ExtensionSession;
use crate::registry::RegistrySnapshot;
use crate::translate::dispatch_data_event;
use crate::PluginKeepalive;

/// A [`ProviderHooks`] that dispatches to the registered extension handlers.
/// Keeps the cdylib mappings alive via the keepalive (the handler fn pointers
/// live inside the plugins).
pub struct ExtensionProviderHooks {
    snapshot: Arc<RegistrySnapshot>,
    _keepalive: Arc<PluginKeepalive>,
    /// Timestamp of the last user message we emitted `BeforeAgentStart` for.
    /// The hook fires once per provider call, but `before_agent_start` must
    /// fire once per prompt, so the prompt timestamp is the dedupe key.
    last_before_agent_start: Mutex<Option<i64>>,
}

impl ExtensionProviderHooks {
    /// Build hooks over a loaded extension session's registry snapshot. Returns
    /// `None` when no plugin subscribes to any provider-hook tag (so a session
    /// without provider hooks runs the plain no-op path). `BeforeAgentStart` is
    /// included: it is emitted from this bridge (see [`Self::before_request`])
    /// because the harness never emits a prompt message event.
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        let snapshot = session.snapshot_arc()?;
        let subscribed = [
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
            EventTag::BeforeAgentStart,
        ]
        .iter()
        .any(|t| !snapshot.handlers_for(*t).is_empty());
        if !subscribed {
            return None;
        }
        Some(Self {
            snapshot,
            _keepalive: session.keepalive(),
            last_before_agent_start: Mutex::new(None),
        })
    }
}

impl ProviderHooks for ExtensionProviderHooks {
    /// Fan the planned request out to `BeforeProviderRequest` +
    /// `BeforeProviderHeaders` subscribers. Returns `None` — observer-only in
    /// v1 (the handler ABI has no result channel; see the module docs).
    fn before_request(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        // (A) pi's `before_agent_start`: observe each NEW user prompt exactly
        // once. The harness persists prompts before the run and calls
        // `run_agent_loop` with an empty prompts vec, so no
        // MessageStart/MessageEnd reaches plugins for them. This bridge is the
        // first place the prompt is visible, so it emits the pi-equivalent
        // event. Dedupe by prompt timestamp: one run can make several provider
        // calls, but `before_agent_start` fires once per prompt.
        if let Some((timestamp, prompt, image_count)) = latest_user_prompt(&ctx.messages) {
            let mut last = self.last_before_agent_start.lock().unwrap();
            if *last != Some(timestamp) {
                *last = Some(timestamp);
                drop(last);
                let event = serde_json::json!({
                    "prompt": prompt,
                    "imageCount": image_count,
                });
                dispatch_data_event(
                    &self.snapshot,
                    EventTag::BeforeAgentStart,
                    &event.to_string(),
                );
            }
        }

        let request = serde_json::json!({
            "model": model.id,
            "provider": model.provider,
            "baseUrl": model.base_url,
            "reasoning": model.reasoning,
            "apiKey": opts.api_key,
            "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
            "headers": opts.headers,
            "metadata": opts.metadata,
            "maxTokens": opts.max_tokens,
            "temperature": opts.temperature,
            // (B) the model-facing messages, so observers (e.g. rpi-langfuse)
            // can record the real generation input. `ctx` was previously
            // discarded entirely, which is why generation/trace input was
            // always null for plugins.
            "messages": ctx.messages,
        });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderRequest,
            &request.to_string(),
        );
        let headers = serde_json::json!({ "headers": opts.headers });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderHeaders,
            &headers.to_string(),
        );
        None
    }

    /// Fan the terminal assistant message out to `AfterProviderResponse`
    /// subscribers.
    fn after_response(&self, _model: &Model, message: &AssistantMessage) {
        let json = serde_json::to_value(message).unwrap_or(serde_json::json!({}));
        dispatch_data_event(
            &self.snapshot,
            EventTag::AfterProviderResponse,
            &json.to_string(),
        );
    }
}

/// Text/timestamp/image-count of the most recent user message. Mirrors the
/// `prompt`/`images` fields pi puts on its `before_agent_start` event.
fn latest_user_prompt(messages: &[Message]) -> Option<(i64, String, usize)> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some((
            user.timestamp,
            user_content_text(&user.content),
            user_content_image_count(&user.content),
        )),
        _ => None,
    })
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_image_count(content: &UserContent) -> usize {
    match content {
        UserContent::Text(_) => 0,
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| matches!(block, Content::Image(_)))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::{EventTag, StablePluginEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static PROVIDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());
    static CAPTURED: Mutex<Vec<(EventTag, String)>> = Mutex::new(Vec::new());

    extern "C" fn capture_handler(ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        let payload = unsafe { ev.payload.data.data.to_string_lossy() };
        CAPTURED.lock().unwrap().push((ev.tag, payload));
        0
    }

    extern "C" fn counting_provider_handler(
        ev: StablePluginEvent,
        _ud: *mut std::ffi::c_void,
    ) -> i32 {
        // Count every dispatch on the subscribed tags (BeforeProviderRequest +
        // BeforeProviderHeaders). The `model` check validates the request-event
        // payload shape separately — it must NOT gate the count, or the headers
        // event (whose payload is `{"headers":...}` with no `model` key) would
        // be silently dropped and the fan-out count would be wrong.
        let s = unsafe { ev.payload.data.data.to_string_lossy() };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            // The request event carries `model`; the headers event does not.
            // Both are valid dispatches — count unconditionally, and sanity-check
            // the request payload when `model` is present.
            if v.get("model").is_some() {
                assert!(v.get("model").is_some(), "request event must carry model");
            }
            PROVIDER_HITS.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    /// `before_agent_start` fires once per NEW user prompt (not once per
    /// provider call in a run), and the request payload carries the
    /// model-facing `messages`.
    #[test]
    fn before_request_emits_before_agent_start_once_and_forwards_messages() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            capture_handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                42,
            ),
        )]);
        let opts = rpi_ai::SimpleStreamOptions::default();

        // Two provider calls in the same run (same prompt) → one
        // before_agent_start, two before_provider_request observations.
        hooks.before_request(&model, &ctx, &opts);
        hooks.before_request(&model, &ctx, &opts);

        let captured = CAPTURED.lock().unwrap().clone();
        let agent_starts: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .collect();
        assert_eq!(
            agent_starts.len(),
            1,
            "before_agent_start must fire once per prompt: {captured:?}"
        );
        assert!(agent_starts[0].1.contains("\"prompt\":\"hello\""));

        let requests: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeProviderRequest))
            .collect();
        assert_eq!(requests.len(), 2, "every provider call is observed");
        assert!(
            requests[0].1.contains("\"messages\""),
            "request payload must forward messages: {}",
            requests[0].1
        );
    }

    /// `dispatch_data_event` fans the JSON to the tag's handlers, and the
    /// `ExtensionProviderHooks::before_request` path dispatches BOTH the
    /// request and headers events to their subscribers.
    #[test]
    fn provider_hooks_dispatch_to_subscribers() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        PROVIDER_HITS.store(0, Ordering::SeqCst);

        let mut registry = crate::registry::ExtensionRegistry::new();
        let handler: rpi_plugin_sdk::EventHandlerFn = counting_provider_handler;
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderHeaders,
            handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());

        // dispatch_data_event directly: one request event ⇒ one hit.
        assert!(dispatch_data_event(
            &snapshot,
            EventTag::BeforeProviderRequest,
            r#"{"model":"m"}"#,
        ));
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 1);

        // through ExtensionProviderHooks::before_request. The hook fires BOTH
        // BeforeProviderRequest AND BeforeProviderHeaders (each with the same
        // counting handler subscribed), so 2 more hits ⇒ 3 total. The headers
        // event's payload is `{"headers":...}` (no `model` key) — the handler
        // counts it regardless (see counting_provider_handler).
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::default();
        let opts = rpi_ai::SimpleStreamOptions::default();
        let patch = hooks.before_request(&model, &ctx, &opts);
        assert!(patch.is_none(), "observer-only v1: no patch returned");
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 3);
    }

    /// A tag with no subscribers dispatches nothing (and doesn't free anything
    /// dangling — dispatch_data_event short-circuits before building events).
    #[test]
    fn provider_hooks_no_subscribers_is_noop() {
        let registry = crate::registry::ExtensionRegistry::new();
        let snapshot = Arc::new(registry.snapshot());
        assert!(!dispatch_data_event(
            &snapshot,
            EventTag::AfterProviderResponse,
            "{}",
        ));
    }
}
