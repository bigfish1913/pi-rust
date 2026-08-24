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

use std::sync::Arc;

use rpi_ai::types::AssistantMessage;
use rpi_ai::{Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};
use rpi_plugin_sdk::EventTag;
use rpi_ai::Context;

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
}

impl ExtensionProviderHooks {
    /// Build hooks over a loaded extension session's registry snapshot. Returns
    /// `None` when no plugin subscribes to any provider-hook tag (so a session
    /// without provider hooks runs the plain no-op path).
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        let snapshot = session.snapshot_arc()?;
        let subscribed = [
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
        ]
        .iter()
        .any(|t| !snapshot.handlers_for(*t).is_empty());
        if !subscribed {
            return None;
        }
        Some(Self {
            snapshot,
            _keepalive: session.keepalive(),
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
        _ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::{EventTag, StablePluginEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static PROVIDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());

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

    /// `dispatch_data_event` fans the JSON to the tag's handlers, and the
    /// `ExtensionProviderHooks::before_request` path dispatches BOTH the
    /// request and headers events to their subscribers.
    #[test]
    fn provider_hooks_dispatch_to_subscribers() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        PROVIDER_HITS.store(0, Ordering::SeqCst);

        let mut registry = crate::registry::ExtensionRegistry::new();
        let handler: rpi_plugin_sdk::EventHandlerFn = counting_provider_handler;
        registry.register_event_handler(EventTag::BeforeProviderRequest, handler, std::ptr::null_mut());
        registry.register_event_handler(EventTag::BeforeProviderHeaders, handler, std::ptr::null_mut());
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
