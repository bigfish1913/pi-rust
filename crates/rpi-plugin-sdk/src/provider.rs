//! Provider packages use the same C ABI as tools. This bridge owns a runtime
//! per request by default; opt-in contexts reuse resources until lifecycle shutdown.
//!
//! Available with the optional `provider` feature. Use [`crate::export_provider!`]
//! for single-provider plugins, or [`register`] and [`drive`] when registering
//! multiple providers through one plugin entrypoint.
use crate::{
    PluginApi, ProviderRequestFn, ProviderStreamControl, ProviderStreamEmitFn,
    ProviderStreamRequestFn, StbString, StbStringRef,
};
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Model, Provider, SimpleStreamOptions,
    StopReason,
};
use std::{ffi::c_void, sync::Arc, time::Duration};

mod context;
pub use context::{drive_with_context, register_with_context, ContextFactory, ProviderContext};

pub type Factory = fn(&Model, &SimpleStreamOptions) -> Arc<dyn Provider>;

/// Reuse the maintained OpenAI-compatible protocol without importing any
/// vendor credentials from the host process environment. Client construction
/// errors propagate without bypassing the configured transport.
pub fn completions(
    model: &Model,
    options: &SimpleStreamOptions,
    anonymous: bool,
) -> Result<Arc<dyn Provider>, String> {
    let client = rpi_ai::http::build_client(&model.base_url, None, None)?;
    if anonymous && options.api_key.is_none() {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_auth(model.provider.clone(), client, vec![model.clone()])))
    } else {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key(model.provider.clone(), options.api_key.clone(), client, vec![model.clone()])))
    }
}

/// Declare a catalog and register one implementation with a one-shot fallback.
pub fn register(
    api: &PluginApi,
    id: &str,
    base_url: &str,
    style: &str,
    manifest: serde_json::Value,
    request: ProviderRequestFn,
    stream: ProviderStreamRequestFn,
) -> i32 {
    if let Some(declare) = api.declare {
        let json = serde_json::json!({"providers":[manifest]}).to_string();
        let rc = declare(StbStringRef::from_str(&json));
        if rc != 0 {
            return rc;
        }
    }
    let free = crate::callbacks::free_string;
    if let Some(register) = api.register_streaming_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            stream,
            free,
            std::ptr::null_mut(),
        )
    } else if let Some(register) = api.register_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            request,
            free,
            std::ptr::null_mut(),
        )
    } else {
        1
    }
}

#[derive(serde::Deserialize)]
struct Request {
    model: Model,
    context: Context,
    options: serde_json::Value,
}

fn options(value: &serde_json::Value) -> SimpleStreamOptions {
    let mut options = SimpleStreamOptions::default();
    options.api_key = value["apiKey"].as_str().map(str::to_string);
    options.timeout = value["timeoutMs"].as_u64().map(Duration::from_millis);
    options.stream_idle_timeout = value["streamIdleTimeoutMs"]
        .as_u64()
        .map(Duration::from_millis);
    options.max_retry_delay = value["maxRetryDelayMs"].as_u64().map(Duration::from_millis);
    options.max_retries = value["maxRetries"].as_u64().and_then(|n| n.try_into().ok());
    options.max_tokens = value["maxTokens"].as_u64();
    options.temperature = value["temperature"].as_f64();
    options.session_id = value["sessionId"].as_str().map(str::to_string);
    options.headers = serde_json::from_value(value["headers"].clone()).ok();
    options.metadata = serde_json::from_value(value["metadata"].clone()).ok();
    options.reasoning = value["reasoning"]
        .as_str()
        .and_then(|s| serde_json::from_value(serde_json::json!(s.to_ascii_lowercase())).ok());
    options.thinking_budgets = serde_json::from_value(value["thinkingBudgets"].clone()).ok();
    options.cache_retention = match value["cacheRetention"].as_str() {
        Some("None") => rpi_ai::CacheRetention::None,
        Some("Long") => rpi_ai::CacheRetention::Long,
        _ => rpi_ai::CacheRetention::Short,
    };
    options
}

/// Compare every field without constructing a candidate message or JSON tree.
fn reconstructs_text(
    previous: &AssistantMessage,
    next: &AssistantMessage,
    index: usize,
    delta: &str,
) -> bool {
    let AssistantMessage {
        role,
        content,
        api,
        provider,
        model,
        response_model,
        response_id,
        usage,
        stop_reason,
        deferred,
        error_message,
        raw_stop_reason,
        end_turn,
        timestamp,
    } = previous;
    if role != &next.role
        || api != &next.api
        || provider != &next.provider
        || model != &next.model
        || response_model != &next.response_model
        || response_id != &next.response_id
        || usage != &next.usage
        || stop_reason != &next.stop_reason
        || deferred != &next.deferred
        || error_message != &next.error_message
        || raw_stop_reason != &next.raw_stop_reason
        || end_turn != &next.end_turn
        || timestamp != &next.timestamp
        || content.len() != next.content.len()
    {
        return false;
    }
    let (Some(rpi_ai::Content::Text(before)), Some(rpi_ai::Content::Text(after))) =
        (content.get(index), next.content.get(index))
    else {
        return false;
    };
    let rpi_ai::TextContent {
        kind,
        text,
        text_signature,
    } = before;
    kind == &after.kind
        && text_signature == &after.text_signature
        && after.text.strip_prefix(text) == Some(delta)
        && content
            .iter()
            .zip(&next.content)
            .enumerate()
            .all(|(i, (a, b))| i == index || a == b)
}

fn event_json(
    event: &AssistantMessageEvent,
    previous: &mut Option<(usize, Arc<AssistantMessage>)>,
) -> serde_json::Value {
    let compact = match (event, previous.as_ref()) {
        (
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            },
            Some((index, before)),
        ) => index == content_index && reconstructs_text(before, partial, *index, delta),
        _ => false,
    };
    *previous = match event {
        AssistantMessageEvent::TextDelta {
            content_index,
            partial,
            ..
        } => Some((*content_index, Arc::clone(partial))),
        _ => None,
    };
    if compact {
        let AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        } = event
        else {
            unreachable!()
        };
        return serde_json::json!({"type":"text_delta","contentIndex":content_index,"delta":delta});
    }
    let mut chunk = serde_json::to_value(event).expect("provider event JSON");
    // The SDK uses flat tool-call identity fields.
    let tool = chunk.get("toolCall").cloned().or_else(|| {
        if event.type_tag() != "toolcall_start" {
            return None;
        }
        let index = chunk["contentIndex"].as_u64()? as usize;
        chunk.get("partial")?.get("content")?.get(index).cloned()
    });
    if let Some(tool) = tool {
        for field in ["id", "name", "arguments"] {
            if let Some(value) = tool.get(field) {
                chunk[field] = value.clone();
            }
        }
    }
    chunk
}

async fn drive_stream(
    provider: Arc<dyn Provider>,
    request: &Request,
    options: &SimpleStreamOptions,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: Option<&ProviderStreamControl>,
) -> AssistantMessage {
    let mut stream = provider
        .stream_simple(&request.model, &request.context, options)
        .await;
    let mut partial = Arc::new(AssistantMessage::empty(
        request.model.api.clone(),
        request.model.provider.clone(),
        request.model.id.clone(),
        0,
    ));
    let mut previous_text = None;
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    loop {
        // Check synchronously too: an already-buffered provider can
        // finish before the interval's initial tick becomes ready.
        if options.signal.is_cancelled()
            || control.is_some_and(|control| (control.is_cancelled)(control.user_data, 0) != 0)
        {
            options.signal.cancel();
            let mut partial = (*partial).clone();
            partial.stop_reason = StopReason::Aborted;
            partial.error_message = Some("provider request was cancelled".into());
            break partial;
        }
        tokio::select! {
            biased;
            _ = poll.tick() => {}
            event = stream.next() => {
                let Some(event) = event else {
                    break stream.result().await.unwrap_or_else(|error| AssistantMessage::terminal(
                        request.model.api.clone(), request.model.provider.clone(), request.model.id.clone(), StopReason::Error, error.to_string(), 0));
                };
                match &event {
                    AssistantMessageEvent::Done { message, .. } => break message.clone(),
                    AssistantMessageEvent::Error { error, .. } => break error.clone(),
                    _ => {},
                }
                partial = match &event {
                    AssistantMessageEvent::Start { partial }
                    | AssistantMessageEvent::TextStart { partial, .. }
                    | AssistantMessageEvent::TextDelta { partial, .. }
                    | AssistantMessageEvent::TextEnd { partial, .. }
                    | AssistantMessageEvent::ThinkingStart { partial, .. }
                    | AssistantMessageEvent::ThinkingDelta { partial, .. }
                    | AssistantMessageEvent::ThinkingEnd { partial, .. }
                    | AssistantMessageEvent::ToolCallStart { partial, .. }
                    | AssistantMessageEvent::ToolCallDelta { partial, .. }
                    | AssistantMessageEvent::ToolCallEnd { partial, .. } => Arc::clone(partial),
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => unreachable!(),
                };
                let Some(emit) = emit else { continue; };
                let chunk = event_json(&event, &mut previous_text);
                if emit(StbString::from_string(chunk.to_string()), emit_data) != 0 {
                    options.signal.cancel();
                    let mut partial = (*partial).clone();
                    partial.stop_reason = StopReason::Aborted;
                    break partial;
                }
            }
        }
    }
}

/// Drive a synchronous plugin request without retaining any borrowed ABI data.
///
/// # Safety
/// All arguments must satisfy the SDK request/callback lifetime contract. `out`
/// must be writable; any `control` and callback context must outlive this call.
pub unsafe fn drive(
    factory: Factory,
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: *const ProviderStreamControl,
    out: *mut StbString,
) -> i32 {
    crate::guard_or(1, || {
        if out.is_null() {
            return 2;
        }
        let Ok(request) = serde_json::from_str::<Request>(unsafe { request.as_str() }) else {
            return 2;
        };
        let options = options(&request.options);
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 1;
        };
        let control = unsafe { control.as_ref() };
        let message = runtime.block_on(async {
            let provider = factory(&request.model, &options);
            drive_stream(provider, &request, &options, emit, emit_data, control).await
        });
        // Convert via Value to normalize tagged Content's field representation.
        let json = serde_json::to_value(message)
            .expect("assistant message JSON")
            .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        0
    })
}

/// Export a provider package using an ordinary Rust Provider factory.
///
/// Requires the `provider` feature. For a multi-provider package, use one
/// [`crate::export_plugin!`] entrypoint and call [`register`] for each provider
/// instead of exporting this macro multiple times.
#[macro_export]
macro_rules! export_provider {
    ($id:expr, $base:expr, $api:expr, $manifest:expr, $factory:path) => {
        extern "C" fn request(
            req: $crate::StbStringRef,
            out: *mut $crate::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe {
                $crate::provider::drive(
                    $factory,
                    req,
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    out,
                )
            }
        }
        extern "C" fn stream(
            req: $crate::StbStringRef,
            emit: Option<$crate::ProviderStreamEmitFn>,
            data: *mut std::ffi::c_void,
            control: *const $crate::ProviderStreamControl,
            _: Option<$crate::ProviderStreamCompletionFn>,
            _: *mut std::ffi::c_void,
            out: *mut $crate::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe { $crate::provider::drive($factory, req, emit, data, control, out) }
        }
        $crate::export_plugin!(|api| $crate::provider::register(
            api, $id, $base, $api, $manifest, request, stream
        ));
    };
}

#[cfg(test)]
mod bridge_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
        rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_tool_call("echo", serde_json::json!({"value":"hello"})),
        )
    }
    extern "C" fn collect(raw: StbString, data: *mut c_void) -> i32 {
        let string = unsafe {
            crate::callbacks::OwnedString::from_raw(raw, Some(crate::callbacks::free_string))
        };
        let value = serde_json::from_str(&string.to_string_lossy()).unwrap();
        // The bridge calls emit synchronously on this request thread only.
        unsafe { &mut *data.cast::<Vec<serde_json::Value>>() }.push(value);
        0
    }
    fn request() -> String {
        serde_json::json!({"model":Model::new("m","M",rpi_ai::Api::Faux,"probe","http://localhost"),
            "context":Context::new(vec![]),"options":{}})
        .to_string()
    }
    #[test]
    fn transport_construction_errors_return_terminal_output_with_or_without_emit() {
        fn factory(
            _: &ProviderContext,
            model: &Model,
            options: &SimpleStreamOptions,
        ) -> Result<Arc<dyn Provider>, String> {
            completions(model, options, false)
        }
        let model = Model::new(
            "m",
            "M",
            rpi_ai::Api::OpenaiCompletions,
            "probe",
            ":invalid",
        );
        assert!(completions(&model, &SimpleStreamOptions::default(), false).is_err());
        let request =
            serde_json::json!({"model":model, "context":Context::new(vec![]), "options":{}})
                .to_string();
        for with_emit in [false, true] {
            let mut chunks = Vec::<serde_json::Value>::new();
            let mut out = StbString::empty();
            assert_eq!(
                unsafe {
                    drive_with_context(
                        factory,
                        std::ptr::null_mut(),
                        StbStringRef::from_str(&request),
                        with_emit.then_some(collect as ProviderStreamEmitFn),
                        (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                        std::ptr::null(),
                        &mut out,
                    )
                },
                0
            );
            let output = unsafe {
                crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
            };
            let message: AssistantMessage =
                serde_json::from_str(&output.to_string_lossy()).unwrap();
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("invalid target URL"));
            assert!(chunks.is_empty(), "terminal errors use the output slot");
        }
    }

    #[test]
    fn bridge_emits_tool_identity_and_valid_terminal_json() {
        let mut chunks = Vec::<serde_json::Value>::new();
        let mut out = StbString::empty();
        let rc = unsafe {
            drive(
                factory,
                StbStringRef::from_str(&request()),
                Some(collect),
                (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(rc, 0);
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        for kind in ["toolcall_start", "toolcall_end"] {
            let chunk = chunks
                .iter()
                .find(|chunk| chunk["type"] == kind)
                .expect(kind);
            assert_eq!(chunk["name"], "echo");
            assert!(!chunk["id"].as_str().unwrap().is_empty());
        }
        let end = chunks
            .iter()
            .find(|chunk| chunk["type"] == "toolcall_end")
            .unwrap();
        assert_eq!(end["arguments"]["value"], "hello");
        assert!(!chunks
            .iter()
            .any(|chunk| chunk["type"] == "done" || chunk["type"] == "error"));
    }
    #[test]
    fn bridge_honors_the_host_control_surface() {
        extern "C" fn cancelled(_: *mut c_void, _: u32) -> i32 {
            1
        }
        let control = ProviderStreamControl {
            is_cancelled: cancelled,
            wait_cancelled_ms: cancelled,
            user_data: std::ptr::null_mut(),
        };
        let mut out = StbString::empty();
        assert_eq!(
            unsafe {
                drive(
                    factory,
                    StbStringRef::from_str(&request()),
                    None,
                    std::ptr::null_mut(),
                    &control,
                    &mut out,
                )
            },
            0
        );
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::Aborted);
    }
    #[test]
    fn options_preserve_provider_controls() {
        let options = options(
            &serde_json::json!({"apiKey":"key","timeoutMs":42,"streamIdleTimeoutMs":11,
            "maxRetries":3,"maxRetryDelayMs":17,"reasoning":"Xhigh","maxTokens":80,
            "headers":{"x-custom":"value"},"cacheRetention":"Long","sessionId":"s"}),
        );
        assert_eq!(options.api_key.as_deref(), Some("key"));
        assert_eq!(options.timeout, Some(Duration::from_millis(42)));
        assert_eq!(options.stream_idle_timeout, Some(Duration::from_millis(11)));
        assert_eq!(options.max_retries, Some(3));
        assert_eq!(options.max_retry_delay, Some(Duration::from_millis(17)));
        assert_eq!(options.reasoning, Some(rpi_ai::ThinkingLevel::Xhigh));
        assert_eq!(options.max_tokens, Some(80));
        assert_eq!(options.session_id.as_deref(), Some("s"));
        assert_eq!(options.headers.unwrap()["x-custom"], "value");
    }
}
