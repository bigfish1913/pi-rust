//! [`PluginToolHandle`] — the plugin's 4-function lifecycle bundle held per
//! registered tool — and [`PluginToolAdapter`], the `AgentTool` impl that drives
//! it across the async/FFI boundary via the corrected spawn_blocking bridge.
//!
//! See the crate-level docs for the load-bearing soundness rationale. The short
//! version: a plugin tool is driven by **four** plugin fns (`execute`→handle,
//! `poll`, `cancel`, `destroy`). The adapter spawns a **blocking** driver that
//! loops `poll`, forwards `Pending` partials through an mpsc, sends the terminal
//! result through a oneshot, and calls `destroy` **exactly once**. Cancel sets
//! an `AtomicBool` the driver observes; the async side keeps awaiting oneshot
//! (never drops the driver). `cancel` ≠ `destroy`.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use rpi_plugin_sdk::{
    FreeStringFn, StbString, StbStringRef, StepHandle, StepResultTag, ToolCancelFn, ToolDestroyFn,
    ToolExecuteFn, ToolPollFn,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::host_free_string;
use crate::loader::PluginKeepalive;

// ---------------------------------------------------------------------------
// PluginToolHandle — the 4-fn bundle + the plugin's free_string
// ---------------------------------------------------------------------------

/// The plugin's per-tool lifecycle bundle the host holds after a successful
/// `register_tool`. All fields are fn pointers (Copy), so the handle is `Copy`:
/// cloning duplicates the pointers, not any allocation. A registered tool is
/// driven by at most one [`PluginToolAdapter`] at a time, but the handle is
/// copied through the registry snapshot path, hence `Copy`.
///
/// `plugin_free_string` is the fn the **plugin** exports to free the
/// [`StbString`]s it *produces* (schema strings, terminal result, pending
/// progress). The host calls it for each such string it receives.
#[derive(Clone, Copy)]
pub struct PluginToolHandle {
    pub(crate) execute_fn: ToolExecuteFn,
    pub(crate) poll_fn: ToolPollFn,
    pub(crate) cancel_fn: ToolCancelFn,
    pub(crate) destroy_fn: ToolDestroyFn,
    pub(crate) plugin_free_string: FreeStringFn,
}

// ---------------------------------------------------------------------------
// JSON ⇄ AgentToolResult helpers (host side)
// ---------------------------------------------------------------------------

/// Serialize an `AgentToolResult` to a JSON string. Used for partials the host
/// hands to the plugin's partial callback (host-produced → plugin frees via
/// host `free_string`) and is also the shape the plugin returns for `Done`.
fn result_to_json(result: &AgentToolResult) -> String {
    let mut txt = String::new();
    txt.push('{');
    txt.push_str("\"content\":[");
    for (i, c) in result.content.iter().enumerate() {
        if i > 0 {
            txt.push(',');
        }
        match c {
            TextContentOrImage::Text(t) => {
                txt.push_str(
                    &serde_json::to_string(&serde_json::json!({ "type": "text", "text": t.text }))
                        .unwrap_or_else(|_| "\"\"".into()),
                );
            }
            TextContentOrImage::Image(img) => {
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
    // usage omitted from the wire shape (Option<Usage> is not in every plugin's
    // contract; the model-visible content/details are what matter). Documented
    // v1 limit: usage does not cross to the plugin.
    txt.push('}');
    txt
}

/// Parse a plugin-produced JSON `AgentToolResult` back into the native struct.
/// Lenient: missing fields default. The caller frees the input `StbString` via
/// the plugin's `free_string` (plugin produced it).
fn stb_to_result(s: &StbString) -> AgentToolResult {
    let text = s.to_string_lossy();
    let val: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let mut result = AgentToolResult::default();
    if let Some(obj) = val.as_object() {
        if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
            for block in content {
                let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match kind {
                    "image" => {
                        let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let mime = block
                            .get("mimeType")
                            .or_else(|| block.get("mime_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("image/png")
                            .to_string();
                        result.content.push(TextContentOrImage::Image(rpi_ai::types::ImageContent {
                            kind: rpi_ai::types::ImageContentType,
                            data,
                            mime_type: mime,
                        }));
                    }
                    _ => {
                        let t = block.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        result.content.push(TextContentOrImage::text(t));
                    }
                }
            }
        }
        if let Some(details) = obj.get("details") {
            result.details = details.clone();
        }
        if let Some(terms) = obj.get("terminate").and_then(|v| v.as_bool()) {
            result.terminate = terms;
        }
        if let Some(arr) = obj
            .get("addedToolNames")
            .or_else(|| obj.get("added_tool_names"))
            .and_then(|v| v.as_array())
        {
            result.added_tool_names = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(usage) = obj.get("usage") {
            if let Ok(u) = serde_json::from_value::<rpi_ai::types::Usage>(usage.clone()) {
                result.usage = Some(u);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// PluginToolAdapter — AgentTool impl driving the 4-fn handle
// ---------------------------------------------------------------------------

/// An [`AgentTool`] backed by a plugin's 4-function handle. One adapter is built
/// per registered tool (`schema` copied from the registration) and inserted into
/// the session's tool set in B2.
///
/// Holds a clone of the session's [`PluginKeepalive`] so the cdylib that owns
/// `handle`'s fn pointers stays mapped for as long as the adapter (and thus any
/// in-flight `execute`) may call them. Without this the `Library` could drop
/// (unload the cdylib) while a fn pointer is still callable → UAF. The keepalive
/// is `Arc`-shared with every other adapter built from the same session, so the
/// last drop — which can only happen once the harness's tool vec drops — unloads.
pub struct PluginToolAdapter {
    schema: Tool,
    label: String,
    handle: PluginToolHandle,
    // Drop order: `keepalive` is declared AFTER `handle` so the cdylib unloads
    // only after the fn-pointer bundle is itself dropped — though since both are
    // fine to drop in any order (fn pointers are Copy, the real call sites are
    // all inside `execute` which holds `&self`, so the adapter is never dropped
    // mid-call), this is belt-and-suspenders.
    #[allow(dead_code)]
    keepalive: Arc<PluginKeepalive>,
}

impl PluginToolAdapter {
    /// Build an adapter from the registered schema + handle + the session's
    /// keepalive. `label` defaults to the tool name. The keepalive clone keeps
    /// the owning cdylib mapped for the adapter's lifetime.
    pub fn new(
        schema: Tool,
        handle: PluginToolHandle,
        keepalive: Arc<PluginKeepalive>,
    ) -> Self {
        let label = schema.name.clone();
        Self { schema, label, handle, keepalive }
    }
}

/// The partial-callback trampoline passed to `poll`. `user_data` is a
/// `*const mpsc::UnboundedSender<AgentToolResult>` valid for the drive (the
/// blocking driver owns the sender in its closure env). The plugin invokes this
/// synchronously inside `poll()`; it parses the partial, frees the plugin-
/// produced `StbString` via the host's `free_string`, and pushes the result
/// through the mpsc. Wrapped in `catch_unwind` so a poisoned sender / panic
/// cannot unwind across FFI (abort-on-unwind).
extern "C" fn partial_cb_trampoline(partial: StbString, user_data: *mut c_void) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Still must free the partial (plugin produced it; host owns
            // cleanup when no handler runs — here "no handler" means no sender).
            host_free_string(partial);
            return;
        }
        // SAFETY: the blocking driver guarantees `user_data` is a live
        // `&mpsc::UnboundedSender<AgentToolResult>` for the duration of poll().
        let sender = unsafe { &*(user_data as *const mpsc::UnboundedSender<AgentToolResult>) };
        let result = stb_to_result(&partial);
        // The partial StbString was plugin-produced inside poll(); the host is
        // the receiver and owns the free (plugin allocated with the global
        // allocator, which host_free_string reclaims — documented v1 contract).
        host_free_string(partial);
        let _ = sender.send(result);
    }));
    if outcome.is_err() {
        tracing::error!("plugin partial callback panicked — aborting (cannot unwind across FFI)");
        std::process::abort();
    }
}

#[async_trait]
impl AgentTool for PluginToolAdapter {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // Plugin tools default to Parallel (the AgentTool default). A plugin
        // could declare Sequential via a future schema field; v1 keeps Parallel.
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        // 1. Acquire the ambient runtime (the adapter only runs inside the agent
        //    loop's runtime). Do NOT own a runtime.
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
            AgentError::State(format!("plugin tool '{}' executed off-runtime: {e}", self.schema.name))
        })?;

        // 2. Bridges: unbounded mpsc for partials, oneshot for terminal.
        let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<AgentToolResult>();
        let (done_tx, done_rx) = oneshot::channel::<Result<AgentToolResult, AgentError>>();

        // The cancel flag the blocking driver observes. Set by `signal.cancelled()`
        // (and by the drop guard, were one needed). SeqCst for cross-thread
        // visibility with the blocking driver.
        let cancel_flag = Arc::new(AtomicBool::new(false));

        // 3. Prepare plugin execute() inputs. `params` is an owning JSON string
        //    the host produced → the plugin frees it via the host's free_string.
        //    `tool_call_id` is borrowed for the call.
        let params_json = serde_json::to_string(&params).unwrap_or_else(|_| "null".to_string());
        let params_stb = StbString::from_string(params_json);
        let id_ref = StbStringRef::from_str(tool_call_id);
        let plugin_free = self.handle.plugin_free_string;

        let execute_fn = self.handle.execute_fn;
        let poll_fn = self.handle.poll_fn;
        let cancel_fn = self.handle.cancel_fn;
        let destroy_fn = self.handle.destroy_fn;

        let schema_name = self.schema.name.clone();
        let schema_name_for_error = schema_name.clone();

        // 4. spawn_blocking driver. It runs to completion regardless of
        //    outer-future drop, so we NEVER drop it; on cancel we set the flag
        //    and keep awaiting done_rx. The sender is captured (Send); the raw
        //    pointer to it is computed INSIDE the closure (not moved across
        //    threads — `*mut c_void` is not `Send`).
        let cancel_flag_drive = Arc::clone(&cancel_flag);
        let sender_for_cb = partial_tx.clone();
        runtime.spawn_blocking(move || {
            // Drive: execute → poll loop → destroy exactly once. Every plugin
            // call is extern "C"; wrap in catch_unwind so a plugin panic cannot
            // unwind across FFI (abort-on-unwind).
            let step_handle: StepHandle = {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    (execute_fn)(id_ref, params_stb, Some(host_free_string))
                }));
                match outcome {
                    Ok(h) if !h.is_null() => h,
                    Ok(_) => {
                        // null handle — allocation failure / refused.
                        let _ = done_tx.send(Err(AgentError::Tool(format!(
                            "plugin execute returned null handle for '{schema_name}'"
                        ))));
                        return;
                    }
                    Err(_) => {
                        tracing::error!("plugin execute panicked — aborting");
                        std::process::abort();
                    }
                }
            };

            // The sender pointer, valid for the drive (sender_for_cb lives in
            // this closure env). Computed here so no raw pointer crosses threads.
            let sender_ptr = &sender_for_cb as *const _ as *mut c_void;

            // poll loop
            let terminal: Result<AgentToolResult, AgentError> = loop {
                if cancel_flag_drive.load(Ordering::SeqCst) {
                    // Observe cancel: tell the plugin, then break with an abort.
                    let _ = catch_unwind(AssertUnwindSafe(|| (cancel_fn)(step_handle)));
                    break Err(AgentError::Tool("plugin tool cancelled".into()));
                }
                let step_result = match catch_unwind(AssertUnwindSafe(|| {
                    (poll_fn)(step_handle, Some(partial_cb_trampoline), sender_ptr)
                })) {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::error!("plugin poll panicked — aborting");
                        std::process::abort();
                    }
                };
                match step_result.tag {
                    StepResultTag::Pending => {
                        // SAFETY: tag == Pending.
                        let progress = unsafe { step_result.pending_payload().progress };
                        if !progress.is_empty() {
                            // The plugin may also have pushed via the partial cb;
                            // both paths land in partial_rx. Forward this one too.
                            let pr = stb_to_result(&progress);
                            // progress was plugin-produced inside poll → free via
                            // the plugin's free_string.
                            (plugin_free)(progress);
                            let _ = partial_tx.send(pr);
                        }
                        continue;
                    }
                    StepResultTag::Done => {
                        // SAFETY: tag == Done.
                        let done = unsafe { step_result.done_payload().result };
                        let result = stb_to_result(&done);
                        (plugin_free)(done);
                        break Ok(result);
                    }
                    StepResultTag::Err => {
                        // SAFETY: tag == Err.
                        let msg = unsafe { step_result.err_payload().message };
                        let message = msg.to_string_lossy();
                        (plugin_free)(msg);
                        break Err(AgentError::Tool(message));
                    }
                }
            };

            // destroy exactly once — idempotent, called by the driver only.
            let _ = catch_unwind(AssertUnwindSafe(|| (destroy_fn)(step_handle)));
            let _ = done_tx.send(terminal);
        });

        // 5. Async side: poll the terminal oneshot and the cancel signal in a
        //    loop. On cancel: set the AtomicBool (driver observes it) and KEEP
        //    polling the same oneshot (never drop the driver — spawn_blocking
        //    runs to completion; dropping is a thread leak). Pinning both
        //    futures lets us re-poll `done_rx` after `cancelled` fired without
        //    moving it (a plain `select!` would consume it on the first fire).
        let schema_name_err = schema_name_for_error.clone();
        tokio::pin!(done_rx);
        let mut cancelled = std::pin::pin!(signal.cancelled());
        let result: Result<AgentToolResult, AgentError> = loop {
            tokio::select! {
                done = &mut done_rx => {
                    while let Ok(p) = partial_rx.try_recv() { on_update(p); }
                    break match done {
                        Ok(Ok(r)) => Ok(r),
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(AgentError::State(format!(
                            "plugin tool '{schema_name_err}' driver dropped done_tx"
                        ))),
                    };
                }
                _ = &mut cancelled => {
                    // Set the flag once; the driver observes it and breaks. Keep
                    // looping so we still poll done_rx to completion.
                    let already = cancel_flag.swap(true, Ordering::SeqCst);
                    if !already {
                        while let Ok(p) = partial_rx.try_recv() { on_update(p); }
                    }
                    // Yield so we don't busy-spin against the driver.
                    tokio::task::yield_now().await;
                }
            }
        };

        // Final drain of any partials that arrived between the select! branch and here.
        while let Ok(p) = partial_rx.try_recv() {
            on_update(p);
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Tests — the ABI bridge against an in-process stub plugin
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::{StepResult, ToolPartialCb};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // These three tests share process-global stub counters (the stub fns are
    // `extern "C"` and cannot capture per-test state). Parallel execution would
    // have one test's `reset_counters` wipe another's in-flight increments. Hold
    // this lock for the ENTIRE test body — the spawn_blocking driver finishes
    // (destroy fires) before the awaited `execute` returns, so releasing the
    // guard after the await means no driver outlives its test's reset window.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    static DESTROY_COUNT: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DriveState {
        cancelled: Arc<AtomicBool>,
        polls: usize,
        done_at: usize,
    }

    extern "C" fn stub_execute(
        _id: StbStringRef,
        _params: StbString,
        _free: Option<FreeStringFn>,
    ) -> StepHandle {
        let state = Box::new(DriveState {
            cancelled: Arc::new(AtomicBool::new(false)),
            polls: 0,
            done_at: 3,
        });
        Box::into_raw(state) as StepHandle
    }

    extern "C" fn stub_poll(
        h: StepHandle,
        _cb: Option<ToolPartialCb>,
        _ud: *mut c_void,
    ) -> StepResult {
        let state = unsafe { &mut *(h as *mut DriveState) };
        state.polls += 1;
        if state.cancelled.load(Ordering::SeqCst) {
            return StepResult::err(StbString::from_string("cancelled".into()));
        }
        if state.polls >= state.done_at {
            let result_json = StbString::from_string(
                r#"{"content":[{"type":"text","text":"echo: hello"}]}"#.to_string(),
            );
            StepResult::done(result_json)
        } else {
            StepResult::pending(StbString::from_string(
                r#"{"content":[{"type":"text","text":"..."}]}"#.to_string(),
            ))
        }
    }

    extern "C" fn stub_cancel(h: StepHandle) {
        CANCEL_COUNT.fetch_add(1, Ordering::SeqCst);
        let state = unsafe { &*(h as *const DriveState) };
        state.cancelled.store(true, Ordering::SeqCst);
    }

    extern "C" fn stub_destroy(h: StepHandle) {
        DESTROY_COUNT.fetch_add(1, Ordering::SeqCst);
        if h.is_null() {
            return;
        }
        unsafe { let _ = Box::from_raw(h as *mut DriveState); }
    }

    extern "C" fn stub_free(s: StbString) {
        if s.is_empty() || s.ptr.is_null() {
            return;
        }
        unsafe {
            let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
            let _ = Box::from_raw(slice as *const [u8] as *mut [u8]);
        }
    }

    fn stub_handle() -> PluginToolHandle {
        PluginToolHandle {
            execute_fn: stub_execute,
            poll_fn: stub_poll,
            cancel_fn: stub_cancel,
            destroy_fn: stub_destroy,
            plugin_free_string: stub_free,
        }
    }

    fn echo_adapter() -> PluginToolAdapter {
        let tool = Tool {
            name: "echo".to_string(),
            description: "echoes".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
            constrained_sampling: None,
        };
        PluginToolAdapter::new(tool, stub_handle(), PluginKeepalive::empty())
    }

    fn reset_counters() {
        DESTROY_COUNT.store(0, Ordering::SeqCst);
        CANCEL_COUNT.store(0, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn adapter_drives_to_done_and_destroys_once() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_counters();
        let adapter = echo_adapter();
        let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
        let signal = CancellationToken::new();
        let result = adapter
            .execute("call_1", serde_json::json!({}), signal, on_update)
            .await
            .expect("drive should succeed");
        assert_eq!(result.content.len(), 1);
        assert_eq!(DESTROY_COUNT.load(Ordering::SeqCst), 1, "destroy exactly once");
        assert_eq!(CANCEL_COUNT.load(Ordering::SeqCst), 0, "no cancel in happy path");
    }

    #[tokio::test]
    async fn adapter_forwards_partials_to_on_update() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_counters();
        let adapter = echo_adapter();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen);
        let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(move |p| {
            if let Some(t) = p.content.first().and_then(|c| match c {
                TextContentOrImage::Text(t) => Some(t.text.clone()),
                _ => None,
            }) {
                seen_clone.lock().unwrap().push(t);
            }
        });
        let signal = CancellationToken::new();
        let _ = adapter
            .execute("call_2", serde_json::json!({}), signal, on_update)
            .await
            .expect("ok");
        let partials = seen.lock().unwrap().clone();
        assert!(partials.iter().any(|t| t == "..."), "got {:?}", partials);
        assert_eq!(DESTROY_COUNT.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_cancel_observed_no_uaf_no_leak() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_counters();
        struct SlowState {
            cancelled: Arc<AtomicBool>,
            polls: usize,
        }
        extern "C" fn slow_execute(_: StbStringRef, _: StbString, _: Option<FreeStringFn>) -> StepHandle {
            Box::into_raw(Box::new(SlowState { cancelled: Arc::new(AtomicBool::new(false)), polls: 0 })) as StepHandle
        }
        extern "C" fn slow_poll(h: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
            let s = unsafe { &mut *(h as *mut SlowState) };
            s.polls += 1;
            if s.cancelled.load(Ordering::SeqCst) {
                return StepResult::err(StbString::from_string("cancelled".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            StepResult::pending(StbString::empty())
        }
        extern "C" fn slow_cancel(h: StepHandle) {
            CANCEL_COUNT.fetch_add(1, Ordering::SeqCst);
            unsafe { (*(h as *mut SlowState)).cancelled.store(true, Ordering::SeqCst); }
        }
        extern "C" fn slow_destroy(h: StepHandle) {
            DESTROY_COUNT.fetch_add(1, Ordering::SeqCst);
            if !h.is_null() {
                unsafe { let _ = Box::from_raw(h as *mut SlowState); }
            }
        }
        let tool = Tool {
            name: "slow".to_string(),
            description: "slow".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
            constrained_sampling: None,
        };
        let handle = PluginToolHandle {
            execute_fn: slow_execute,
            poll_fn: slow_poll,
            cancel_fn: slow_cancel,
            destroy_fn: slow_destroy,
            plugin_free_string: stub_free,
        };
        let adapter = PluginToolAdapter::new(tool, handle, PluginKeepalive::empty());
        let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
        let signal = CancellationToken::new();
        let signal_clone = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            signal_clone.cancel();
        });
        let _ = adapter.execute("call_3", serde_json::json!({}), signal, on_update).await;
        // No hang + destroy exactly once (cancel observed via the AtomicBool).
        assert_eq!(DESTROY_COUNT.load(Ordering::SeqCst), 1, "destroy once on cancel");
    }
}
