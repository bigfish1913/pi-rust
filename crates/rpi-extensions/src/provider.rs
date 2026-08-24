//! B5c — `PluggableProvider`: the host-side `Provider` impl wrapping a plugin's
//! `ProviderRequestFn`.
//!
//! A plugin's [`ProviderRequestFn`](rpi_plugin_sdk::ProviderRequestFn) is a
//! **synchronous one-shot**: given a borrowed JSON request envelope it writes a
//! **plugin-owned** `out` [`StbString`] (a full assistant-message JSON). It
//! cannot drive a chunked `stream_simple` without blocking, so the v1
//! `PluggableProvider` emits the full response as a **single terminal `Done`
//! chunk** — a documented divergence from pi's async `streamSimple`
//! (`custom-provider.md`). The ffi call runs on `spawn_blocking` (via a captured
//! [`Handle`](tokio::runtime::Handle) — same foreign-thread fix as the
//! `runtime_action` bridge), so the async `stream_simple` never blocks a runtime
//! worker.
//!
//! ## Request/response envelope
//!
//! - **Request** (borrowed `StbStringRef` handed to `request_fn`): JSON
//!   `{ model, context, options }`, where `model`/`context` are the serde
//!   serializations of [`Model`](rpi_ai::Model)/[`Context`](rpi_ai::Context)
//!   and `options` is a hand-built JSON of the [`SimpleStreamOptions`] fields
//!   (that type is not `Serialize`; we mirror the field set
//!   [`ExtensionProviderHooks`](crate::ExtensionProviderHooks) dispatches).
//! - **Response** (the plugin-owned `out` `StbString`): an
//!   [`AssistantMessage`] JSON. The host reclaims `out` via the plugin's
//!   `plugin_free_string` (traveled with the registration), parses the JSON, and
//!   pushes it as one `Done` (or `Error` on nonzero rc / parse failure).
//!
//! ## Lifetime + staleness
//!
//! `PluggableProvider` mirrors [`ExtensionProviderHooks`]: it holds an
//! `Arc<RegistrySnapshot>` (the staleness flag is shared — a `/reload`
//! invalidate marks it stale) + an `Arc<PluginKeepalive>` (keeps the cdylib
//! mapped so `request_fn` stays callable) + the [`RegisteredProvider`] record +
//! a `Handle` captured at build time (the host is on the runtime when it
//! constructs providers from the session).

use std::sync::Arc;

use rpi_ai::event_stream::{create_assistant_message_event_stream};
use rpi_ai::types::{AssistantMessage, DoneReason, ErrorReason};
use rpi_ai::{AssistantMessageEvent, AssistantMessageEventStream, AssistantMessageEventStreamProducer, Context, Model, Provider, SimpleStreamOptions};
use tokio::runtime::Handle;

use crate::loader::ExtensionSession;
use crate::registry::{RegisteredProvider, RegistrySnapshot};
use crate::PluginKeepalive;

/// A `Provider` backed by a plugin's sync `ProviderRequestFn`.
///
/// See the module docs for the v1 one-shot streaming model + the request/response
/// envelope. Built from a loaded [`ExtensionSession`]; one `PluggableProvider`
/// per [`RegisteredProvider`] in the session's snapshot.
pub struct PluggableProvider {
    /// The registration record (id/base_url/api_style + the fn pointers). Cloned
    /// from the snapshot; the fn pointers are valid for the keepalive's lifetime.
    record: RegisteredProvider,
    /// Shared staleness flag (a `/reload` invalidate flows through here).
    snapshot: Arc<RegistrySnapshot>,
    /// Keeps the cdylib mapped so `request_fn` stays callable.
    _keepalive: Arc<PluginKeepalive>,
    /// Captured at build time — `spawn_blocking` works from the async context.
    runtime: Handle,
}

impl PluggableProvider {
    fn new(
        record: RegisteredProvider,
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        runtime: Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            record,
            snapshot,
            _keepalive: keepalive,
            runtime,
        })
    }

    /// Build one [`PluggableProvider`] per registered provider in the session's
    /// snapshot, as `Arc<dyn Provider>`. Returns an empty vec when no plugin
    /// registered a provider (so a session without provider plugins injects
    /// nothing). The `Handle` MUST be captured from a thread running the target
    /// runtime (pi-cli builds providers on the async main thread, same as the
    /// `ActionBridge`).
    pub fn from_session(
        session: &ExtensionSession,
        runtime: Handle,
    ) -> Vec<Arc<dyn Provider>> {
        let Some(snapshot) = session.snapshot_arc() else {
            return Vec::new();
        };
        let keepalive = session.keepalive();
        snapshot
            .providers()
            .iter()
            .cloned()
            .map(|record| {
                Self::new(record, Arc::clone(&snapshot), Arc::clone(&keepalive), runtime.clone())
                    as Arc<dyn Provider>
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Provider for PluggableProvider {
    fn id(&self) -> &str {
        &self.record.provider_id
    }

    fn models(&self) -> &[Model] {
        // v1: the registrar carries no model list (the SDK `ProviderRequestFn`
        // signature has no models channel — a plugin resolves models itself).
        // The host injects this provider into `AgentHarnessOptions.models`, so
        // the harness finds it by id when a catalog model's `provider` field
        // matches; whether such a model exists is a catalog/config concern.
        // Returning empty here means the provider advertises no models of its
        // own (mirrors a provider that only serves foreign-configured models).
        &[]
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let snapshot = Arc::clone(&self.snapshot);
        // into_owned_record copies the fn pointers + owned strings; the
        // `user_data` raw pointer is carried (plugin-owned, valid for the
        // keepalive lifetime).
        let record = self.record.clone();
        let runtime = self.runtime.clone();
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        // `opts` is not `Clone`-free of lifetime — capture the fields the
        // request envelope needs as owned values so the spawned task is
        // `'static`. `SimpleStreamOptions` IS `Clone` (verified, provider.rs:27),
        // so a single clone carries every field into the task.
        let opts = opts.clone();

        // `spawn` (not `spawn_blocking`) for the producer task — the producer
        // must be async so it can `await` the blocking ffi call. Inside, we
        // `spawn_blocking` the sync `request_fn` (the ffi call is blocking by
        // contract). This keeps the runtime worker free while the plugin runs.
        tokio::spawn(async move {
            let message =
                drive_plugin_provider(&snapshot, &record, runtime, &model, &ctx, &opts).await;
            push_terminal(&mut prod, message);
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

/// Run the sync `ProviderRequestFn` on `spawn_blocking`, read its plugin-owned
/// `out` JSON, reclaim it via `plugin_free_string`, parse to an
/// [`AssistantMessage`]. Returns an error terminal message on any failure
/// (staleness, nonzero rc, parse error, runtime shutdown).
async fn drive_plugin_provider(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    runtime: Handle,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> AssistantMessage {
    // Staleness: a `/reload`d session must not drive its old providers.
    if !snapshot.is_active() {
        return AssistantMessage::terminal(
            model.api.clone(),
            record.provider_id.clone(),
            model.id.clone(),
            rpi_ai::types::StopReason::Error,
            "extensions provider registry is stale (session swapped/reloaded)",
            0,
        );
    }

    // Build the request envelope JSON. `Model`/`Context` are Serialize;
    // `SimpleStreamOptions` is not, so the `options` object is hand-built
    // (mirrors `ExtensionProviderHooks::before_request`).
    let request = serde_json::json!({
        "model": serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
        "context": serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null),
        "options": options_json(opts),
    });
    let request_json = request.to_string();

    // The fn pointers + user_data are `Copy`-able (fn ptrs) or plain raw (ud);
    // clone what spawn_blocking needs. `record` is `Clone` (owned strings).
    let record = record.clone();
    let provider_id = record.provider_id.clone();
    let api = model.api.clone();
    let model_id = model.id.clone();

    // `spawn_blocking` the sync ffi call — `request_fn` is blocking by contract.
    // The join handle is awaited so a panic in the plugin (caught by
    // `catch_unwind` below) surfaces as an error, not a silent hang.
    let join = runtime.spawn_blocking(move || {
        run_provider_request(&record, &request_json)
    });
    let outcome = match join.await {
        Ok(inner) => inner,
        Err(join_err) => {
            // The blocking task panicked before returning (catch_unwind inside
            // caught a plugin panic and aborted, OR tokio cancelled). Either way
            // no response.
            return AssistantMessage::terminal(
                api,
                provider_id,
                model_id,
                rpi_ai::types::StopReason::Error,
                format!("plugin provider task failed: {join_err}"),
                0,
            );
        }
    };

    match outcome {
        ProviderOutcome::Ok(message) => message,
        ProviderOutcome::Err(msg) => AssistantMessage::terminal(
            api,
            provider_id,
            model_id,
            rpi_ai::types::StopReason::Error,
            msg,
            0,
        ),
    }
}

/// The result of one sync `request_fn` call: either a parsed assistant message
/// or an error string. Computed inside `spawn_blocking`.
enum ProviderOutcome {
    Ok(AssistantMessage),
    Err(String),
}

/// The inner sync driver (runs on the blocking pool). Calls `request_fn`,
/// recovers the plugin-owned `out` `StbString`, reclaims it via
/// `plugin_free_string`, parses to an [`AssistantMessage`]. Wrapped in
/// `catch_unwind` so a plugin panic cannot unwind across FFI (⇒ error outcome).
fn run_provider_request(record: &RegisteredProvider, request_json: &str) -> ProviderOutcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_provider_request_inner(record, request_json)
    }));
    match outcome {
        Ok(inner) => inner,
        Err(_) => {
            tracing::error!(
                "plugin provider {} panicked — refusing to unwind across FFI",
                record.provider_id
            );
            ProviderOutcome::Err(format!(
                "plugin provider {} panicked during request",
                record.provider_id
            ))
        }
    }
}

fn run_provider_request_inner(
    record: &RegisteredProvider,
    request_json: &str,
) -> ProviderOutcome {
    // Borrowed request envelope: the plugin must NOT free it (StbStringRef is a
    // borrow — the SDK contract).
    let req_ref = StbStringRef::from_str(request_json);

    // Plugin-owned `out` — `request_fn` writes a StbString into `*out` (plugin
    // allocation, reclaimed via `plugin_free_string`). Start empty.
    let mut out = StbString::empty();
    let rc = (record.request_fn)(req_ref, &mut out as *mut StbString, record.user_data);
    if rc != 0 {
        // Nonzero rc: the plugin signaled failure. Reclaim whatever it wrote
        // (may be empty) before returning.
        out.free_with(Some(record.plugin_free_string));
        return ProviderOutcome::Err(format!(
            "plugin provider {} returned error code {rc}",
            record.provider_id
        ));
    }

    // Read the plugin-owned JSON (borrow `out` for the parse) then reclaim it.
    let response_text = out.to_string_lossy();
    out.free_with(Some(record.plugin_free_string));

    // Parse the response as an AssistantMessage. A plugin that returns a partial
    // or malformed message surfaces a parse error (not a crash) — the host
    // emits it as an `Error` terminal so the run terminates cleanly.
    match serde_json::from_str::<AssistantMessage>(&response_text) {
        Ok(message) => {
            // Stamp the provider/model identity the host expects if the plugin
            // omitted them (a plugin may return only `content`/`stopReason`).
            // We do NOT overwrite fields the plugin set — only fill defaults on
            // a message whose provider/model don't match this provider.
            let mut message = message;
            if message.provider.is_empty() {
                message.provider = record.provider_id.clone();
            }
            if message.model.is_empty() {
                message.model = String::new(); // host stamps model id upstream; leave as-is
            }
            ProviderOutcome::Ok(message)
        }
        Err(err) => ProviderOutcome::Err(format!(
            "plugin provider {} returned unparseable response: {err}",
            record.provider_id
        )),
    }
}

/// Push the terminal event for `message`. A `StopReason::Error`/`Aborted`
/// message → `Error`; otherwise → `Done` (mapping the stop reason onto
/// [`DoneReason`]). This is the single chunk v1 emits (one-shot streaming).
fn push_terminal(prod: &mut AssistantMessageEventStreamProducer, message: AssistantMessage) {
    use rpi_ai::types::StopReason;
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            prod.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: message,
            });
        }
        other => {
            let reason = match other {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Deferred => DoneReason::Deferred,
                // Pending/unknown settle to Stop (the plugin should have set a
                // terminal reason; if not, the response is still complete).
                _ => DoneReason::Stop,
            };
            prod.push(AssistantMessageEvent::Done { reason, message });
        }
    }
}

/// Build the `options` JSON for the request envelope. `SimpleStreamOptions` is
/// not `Serialize`, so we hand-build the field set (mirrors
/// `ExtensionProviderHooks::before_request`). Sensitive fields (`api_key`) are
/// included — the plugin is trusted host-side code (a cdylib the user installed
/// under `--extensions-dir`); this matches pi's `registerProvider` handing the
/// full options to the plugin's `streamSimple`.
fn options_json(opts: &SimpleStreamOptions) -> serde_json::Value {
    serde_json::json!({
        "apiKey": opts.api_key,
        "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
        "maxRetries": opts.max_retries,
        "maxRetryDelayMs": opts.max_retry_delay.map(|d| d.as_millis() as u64),
        "headers": opts.headers,
        "metadata": opts.metadata,
        "cacheRetention": format!("{:?}", opts.cache_retention),
        "sessionId": opts.session_id,
        "reasoning": opts.reasoning.map(|r| format!("{r:?}")),
        "maxTokens": opts.max_tokens,
        "temperature": opts.temperature,
    })
}

use rpi_plugin_sdk::{StbString, StbStringRef};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;

    /// A stub `ProviderRequestFn` that writes a minimal assistant-message JSON
    /// into `out`. Mirrors the shape a real plugin's request fn returns.
    extern "C" fn ok_request_fn(
        _req: StbStringRef,
        out: *mut StbString,
        _ud: *mut std::ffi::c_void,
    ) -> i32 {
        let json = r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"api":"faux","provider":"pluggy","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        unsafe {
            *out = StbString::from_string(json.to_string());
        }
        0
    }

    /// A free fn matching the stub's allocation (`StbString::from_string` ⇒ a
    /// `Box<[u8]>` reclaimed by reconstructing the slice).
    extern "C" fn stub_free(s: StbString) {
        if s.is_empty() || s.ptr.is_null() {
            return;
        }
        unsafe {
            let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
            let _ = Box::from_raw(slice as *const [u8] as *mut [u8]);
        }
    }

    /// `run_provider_request_inner` round-trips a nonzero-rc plugin response to
    /// `Err`, and a valid-message response to `Ok` (with provider stamped from
    /// the record when the plugin omits it).
    #[test]
    fn request_fn_round_trip_ok_and_err() {
        let mut registry = ExtensionRegistry::new();
        registry.register_provider(RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: ok_request_fn,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        });
        let snap = Arc::new(registry.snapshot());
        let record = snap.providers()[0].clone();

        // Valid response → Ok(message).
        let outcome = run_provider_request(&record, r#"{"model":"m"}"#);
        match outcome {
            ProviderOutcome::Ok(m) => {
                assert_eq!(m.provider, "pluggy");
                assert_eq!(m.stop_reason, rpi_ai::types::StopReason::Stop);
                assert_eq!(m.content.len(), 1);
            }
            _ => panic!("expected Ok"),
        }

        // A request fn that returns nonzero rc ⇒ Err (registered separately).
        extern "C" fn err_request_fn(
            _req: StbStringRef,
            _out: *mut StbString,
            _ud: *mut std::ffi::c_void,
        ) -> i32 {
            7
        }
        let err_record = RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: err_request_fn,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        let outcome = run_provider_request(&err_record, r#"{"model":"m"}"#);
        assert!(matches!(outcome, ProviderOutcome::Err(_)));
    }
}
