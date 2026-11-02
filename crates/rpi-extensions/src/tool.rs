//! [`PluginToolHandle`] — the plugin's 4-function lifecycle bundle held per
//! registered tool — and [`PluginToolAdapter`], the `AgentTool` impl that drives
//! it across the async/FFI boundary via the corrected spawn_blocking bridge.
//!
//! Wake-capable registrations instead use an owned async driver, serializing
//! short `spawn_blocking` calls and awaiting retained notifications on Pending.
//! Both drivers retain the library and invocation context through cleanup.
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
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use rpi_plugin_sdk::callbacks::{take_step_result_with, OwnedString, Step};
use rpi_plugin_sdk::{
    FreeStringFn, StbString, StbStringRef, StepHandle, ToolCancelFn, ToolDestroyFn, ToolExecuteFn,
    ToolPollFn,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod wake;

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
    pub(crate) set_waker_fn: Option<rpi_plugin_sdk::ToolSetWakerFn>,
    pub(crate) poll_fn: ToolPollFn,
    pub(crate) cancel_fn: ToolCancelFn,
    pub(crate) destroy_fn: ToolDestroyFn,
    pub(crate) plugin_free_string: FreeStringFn,
    pub(crate) runtime_handle: Option<usize>,
}

// ---------------------------------------------------------------------------
// JSON ⇄ AgentToolResult helpers (host side)
// ---------------------------------------------------------------------------

/// Serialize an `AgentToolResult` to a JSON string. Used for partials the host
/// hands to the plugin's partial callback (host-produced → plugin frees via
/// host `free_string`) and is also the shape the plugin returns for `Done`.
#[allow(dead_code)]
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
/// Lenient: missing fields default. ABI ownership is handled before parsing.
fn json_to_result(text: &str) -> AgentToolResult {
    let val: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    let mut result = AgentToolResult::default();
    if let Some(obj) = val.as_object() {
        if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
            for block in content {
                let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match kind {
                    "image" => {
                        let data = block
                            .get("data")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let mime = block
                            .get("mimeType")
                            .or_else(|| block.get("mime_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("image/png")
                            .to_string();
                        result.content.push(TextContentOrImage::Image(
                            rpi_ai::types::ImageContent {
                                kind: rpi_ai::types::ImageContentType,
                                data: data.into(),
                                mime_type: mime,
                            },
                        ));
                    }
                    _ => {
                        let t = block
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
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
            result.added_tool_names = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
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
// ToolCallContext — the per-session context a plugin tool is handed
// ---------------------------------------------------------------------------

/// Per-session context the host injects into plugin tool arguments under
/// [`rpi_plugin_sdk::HOST_CONTEXT_KEY`].
///
/// A built-in tool gets an `ExecutionToolContext` from the harness and can
/// resolve the working directory from it; a plugin tool has no such channel.
/// Without this, a plugin that needs project (or session) identity has to ask
/// the model for it — and the model does not know the session id, while any
/// `cwd` it guesses is a silent way to split one store into two.
///
/// It is filled in two steps because the CLI assembles the tool set *before* the
/// session it belongs to exists: [`ToolCallContext::new`] carries the `cwd`, and
/// [`ToolCallContext::set_session_id`] completes it once the session does.
/// Every adapter for one session shares one instance (`Arc` inside), so the late
/// write is visible to all of them — including across an extension reload, which
/// rebuilds the adapters from the same context.
///
/// Unknown fields are absent, not empty strings: a consumer should prefer a
/// project-scoped fallback when [`ToolCallContext::session_id`] is `None`.
#[derive(Clone, Debug, Default)]
pub struct ToolCallContext {
    inner: Arc<RwLock<ToolCallContextState>>,
}

#[derive(Debug, Default)]
struct ToolCallContextState {
    cwd: Option<String>,
    session_id: Option<String>,
}

impl ToolCallContext {
    /// A context that knows the project directory but not yet the session id.
    /// An empty `cwd` is treated as unknown.
    pub fn new(cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        Self {
            inner: Arc::new(RwLock::new(ToolCallContextState {
                cwd: (!cwd.is_empty()).then_some(cwd),
                session_id: None,
            })),
        }
    }

    /// Record the session id once the session exists. Safe to call again (a
    /// reload re-derives it); a poisoned lock is ignored rather than panicking
    /// inside a tool call.
    pub fn set_session_id(&self, session_id: impl Into<String>) {
        if let Ok(mut state) = self.inner.write() {
            state.session_id = Some(session_id.into());
        }
    }

    /// The project directory the session was launched in, when known.
    pub fn cwd(&self) -> Option<String> {
        self.inner.read().ok().and_then(|s| s.cwd.clone())
    }

    /// The session id, when the session has one and the host has recorded it.
    pub fn session_id(&self) -> Option<String> {
        self.inner.read().ok().and_then(|s| s.session_id.clone())
    }

    /// The object to inject, or `None` when nothing is known yet — in which
    /// case arguments are passed through untouched rather than gaining an empty
    /// `{}` that a plugin would have to special-case.
    pub fn to_json(&self) -> Option<Value> {
        let state = self.inner.read().ok()?;
        if state.cwd.is_none() && state.session_id.is_none() {
            return None;
        }
        let mut object = serde_json::Map::new();
        if let Some(cwd) = &state.cwd {
            object.insert("cwd".to_string(), Value::String(cwd.clone()));
        }
        if let Some(session_id) = &state.session_id {
            object.insert("sessionId".to_string(), Value::String(session_id.clone()));
        }
        Some(Value::Object(object))
    }
}

/// Add the host context to one plugin tool call's arguments.
///
/// Injected here — inside the adapter, on a copy — rather than in the agent loop
/// or in `AgentTool::prepare_arguments`, so the field reaches the plugin and
/// nothing else: not the model, not `ToolExecutionStart`/`Update` events, not
/// the session log. It also means a model cannot forge it.
///
/// Only object-shaped arguments can carry the key. A plugin whose schema takes
/// an array or a scalar gets its arguments untouched; `null` (the shape a
/// no-parameter tool call arrives in) is promoted to an object so such a tool
/// still receives the context.
fn inject_tool_context(params: Value, context: &ToolCallContext) -> Value {
    let Some(context_json) = context.to_json() else {
        return params;
    };
    match params {
        Value::Object(mut object) => {
            object.insert(rpi_plugin_sdk::HOST_CONTEXT_KEY.to_string(), context_json);
            Value::Object(object)
        }
        Value::Null => {
            let mut object = serde_json::Map::new();
            object.insert(rpi_plugin_sdk::HOST_CONTEXT_KEY.to_string(), context_json);
            Value::Object(object)
        }
        other => other,
    }
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
    /// Injected into every call's arguments under
    /// [`rpi_plugin_sdk::HOST_CONTEXT_KEY`]. `Default` (nothing known) until the
    /// CLI attaches the session's context, so an adapter built without one — as
    /// the tests and the SDK smoke test do — behaves exactly as before.
    context: ToolCallContext,
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
    pub fn new(schema: Tool, handle: PluginToolHandle, keepalive: Arc<PluginKeepalive>) -> Self {
        let label = schema.name.clone();
        Self {
            schema,
            label,
            handle,
            context: ToolCallContext::default(),
            keepalive,
        }
    }

    /// Attach the session's context, so each call carries `cwd` + `sessionId`.
    /// Builder-style so the three-argument [`PluginToolAdapter::new`] keeps
    /// working for callers that have no session (tests, the SDK smoke test).
    pub fn with_context(mut self, context: ToolCallContext) -> Self {
        self.context = context;
        self
    }
}

struct PartialCallbackContext {
    sender: mpsc::UnboundedSender<AgentToolResult>,
    plugin_free: FreeStringFn,
}

/// The blocking driver owns the callback context until poll returns. The
/// plugin invokes this synchronously and transfers a partial to its registered
/// deallocator. OwnedString also reclaims it if host-side parsing unwinds.
extern "C" fn partial_cb_trampoline(partial: StbString, user_data: *mut c_void) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Invalid ABI invocation: no producer is available to free safely.
            // A normal driver always supplies the context, even if the receiver
            // has gone away. Never guess that host and plugin allocators match.
            tracing::error!("plugin partial callback missing its context");
            return;
        }
        // SAFETY: the driver keeps this context live for the synchronous poll.
        let context = unsafe { &*user_data.cast::<PartialCallbackContext>() };
        // SAFETY: the plugin transfers a live partial and its registered free fn.
        let owned = unsafe { OwnedString::from_raw(partial, Some(context.plugin_free)) };
        let result = json_to_result(&owned.to_string_lossy());
        let _ = context.sender.send(result);
    }));
    if outcome.is_err() {
        // A panic inside host-side copying or parsing was caught above. Log it
        // and continue; the adopted partial is reclaimed by Drop.
        tracing::error!("plugin partial callback panicked — dropped (host continues)");
    }
}

struct CancelOnDrop(Arc<AtomicBool>, CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        self.1.cancel();
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
            AgentError::State(format!(
                "plugin tool '{}' executed off-runtime: {e}",
                self.schema.name
            ))
        })?;

        if self.handle.set_waker_fn.is_some() {
            return self
                .execute_wake(tool_call_id, params, signal, on_update)
                .await;
        }

        // 2. Bridges: unbounded mpsc for partials, oneshot for terminal.
        let (partial_tx, mut partial_rx) = mpsc::unbounded_channel::<AgentToolResult>();
        let (done_tx, done_rx) = oneshot::channel::<Result<AgentToolResult, AgentError>>();

        // The cancel flag the blocking driver observes. Set by `signal.cancelled()`
        // (and by the drop guard, were one needed). SeqCst for cross-thread
        // visibility with the blocking driver.
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let invocation_signal = signal.child_token();
        let _cancel_on_drop = CancelOnDrop(cancel_flag.clone(), invocation_signal.clone());
        let background = rpi_agent::tool_execution_scope::track_background_tool();
        let invocation = self
            .handle
            .runtime_handle
            .and_then(crate::ActionBridge::resolve_handle)
            .map(|bridge| {
                bridge.for_invocation(
                    rpi_agent::tool_execution_scope::is_child_execution(),
                    invocation_signal,
                )
            });

        // 3. Prepare plugin execute() inputs. `params` is an owning JSON string
        //    the host produced → the plugin frees it via the host's free_string.
        //    `tool_call_id` is borrowed for the call. The host context is merged
        //    into the arguments here — after the model's arguments were parsed,
        //    before the plugin sees them, and never into the events or the log.
        let mut params = inject_tool_context(params, &self.context);
        if invocation.is_some() && params.is_null() {
            params = serde_json::json!({});
        }
        if let (Some(invocation), Some(object)) = (&invocation, params.as_object_mut()) {
            let context = object
                .entry(rpi_plugin_sdk::HOST_CONTEXT_KEY)
                .or_insert_with(|| serde_json::json!({}));
            // Always replace any model-supplied value with host-owned identity.
            if !context.is_object() {
                *context = serde_json::json!({});
            }
            context["runtimeHandle"] = serde_json::json!(invocation.user_data() as usize);
            context["runtimeKey"] = serde_json::json!(invocation.invocation_key());
        }
        let params_json = serde_json::to_string(&params).unwrap_or_else(|_| "null".to_string());
        let params_stb = StbString::from_string(params_json);
        let tool_call_id = tool_call_id.to_owned();
        let plugin_free = self.handle.plugin_free_string;

        let execute_fn = self.handle.execute_fn;
        let poll_fn = self.handle.poll_fn;
        let cancel_fn = self.handle.cancel_fn;
        let destroy_fn = self.handle.destroy_fn;

        let schema_name = self.schema.name.clone();
        let schema_name_for_error = schema_name.clone();

        // 4. spawn_blocking driver. It runs to completion regardless of
        //    outer-future drop, so we NEVER drop it; on cancel we set the flag
        //    and keep awaiting done_rx. The callback context is captured (Send);
        //    its raw pointer is computed inside the closure, never moved across
        //    threads.
        let cancel_flag_drive = Arc::clone(&cancel_flag);
        let callback_context = PartialCallbackContext {
            sender: partial_tx.clone(),
            plugin_free,
        };
        let keepalive = self.keepalive.clone();
        runtime.spawn_blocking(move || {
            #[cfg(test)]
            let _blocking = wake::BlockingCallGuard::enter();
            // These owners must live through destroy AND terminal string frees,
            // independently of the async caller's cancellation/drop.
            let _keepalive = keepalive;
            let _background = background;
            let _invocation_guard = invocation.as_ref().map(|bridge| bridge.enter_invocation());
            let id_ref = StbStringRef::from_str(&tool_call_id);
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
                        // Reached only when the panic unwound into the host
                        // (plugin `extern "C"` panics abort at the plugin frame
                        // before we get here). Report a tool error instead of
                        // aborting the process.
                        tracing::error!("plugin execute panicked — reporting tool error");
                        let _ = done_tx.send(Err(AgentError::Tool(format!(
                            "plugin tool '{schema_name}' panicked during execute"
                        ))));
                        return;
                    }
                }
            };

            // Compute the opaque context pointer inside the blocking closure;
            // its sender and producer-side deallocator outlive every poll.
            let context_ptr = &callback_context as *const _ as *mut c_void;

            // poll loop
            let terminal: Result<AgentToolResult, AgentError> = loop {
                if cancel_flag_drive.load(Ordering::SeqCst) {
                    // Observe cancel: tell the plugin, then break with an abort.
                    let _ = catch_unwind(AssertUnwindSafe(|| (cancel_fn)(step_handle)));
                    break Err(AgentError::Tool("plugin tool cancelled".into()));
                }
                let step_result = match catch_unwind(AssertUnwindSafe(|| {
                    (poll_fn)(step_handle, Some(partial_cb_trampoline), context_ptr)
                })) {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::error!("plugin poll panicked — reporting tool error");
                        break Err(AgentError::Tool(format!(
                            "plugin tool '{schema_name}' panicked during poll"
                        )));
                    }
                };
                // SAFETY: poll transfers its live result to the registered
                // producer deallocator. The SDK selects the arm and frees once.
                let step = unsafe { take_step_result_with(step_result, plugin_free) };
                if cancel_flag_drive.load(Ordering::SeqCst) {
                    // poll may return success after cancellation was requested.
                    let _ = catch_unwind(AssertUnwindSafe(|| (cancel_fn)(step_handle)));
                    break Err(AgentError::Tool("plugin tool cancelled".into()));
                }
                match step {
                    Step::Pending(progress) => {
                        if !progress.is_empty() {
                            // The plugin may also have pushed via the partial cb;
                            // both paths land in partial_rx. Forward this one too.
                            let pr = json_to_result(&progress);
                            let _ = partial_tx.send(pr);
                        }
                        // poll is a nonblocking ABI. Empty Pending must not spin
                        // a dedicated CPU core while the plugin worker is idle.
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    Step::Done(done) => {
                        let result = json_to_result(&done);
                        break Ok(result);
                    }
                    Step::Err(message) => {
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
        let mut cancel_observed = false;
        let result: Result<AgentToolResult, AgentError> = loop {
            tokio::select! {
                Some(partial) = partial_rx.recv() => on_update(partial),
                done = &mut done_rx => {
                    while let Ok(p) = partial_rx.try_recv() { on_update(p); }
                    break match done {
                        Ok(Ok(_)) if signal.is_cancelled() => Err(AgentError::Tool("plugin tool cancelled".into())),
                        Ok(Ok(r)) => Ok(r),
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(AgentError::State(format!(
                            "plugin tool '{schema_name_err}' driver dropped done_tx"
                        ))),
                    };
                }
                _ = &mut cancelled, if !cancel_observed => {
                    // Consume cancellation once, then sleep on partials/done.
                    // A permanently-ready cancellation future would busy-spin
                    // while an uncooperative foreign poll/destroy is finishing.
                    cancel_observed = true;
                    cancel_flag.store(true, Ordering::SeqCst);
                    while let Ok(p) = partial_rx.try_recv() { on_update(p); }
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
    use serde_json::json;
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
    static PRODUCER_FREES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counted_plugin_free(value: StbString) {
        if !value.is_empty() {
            PRODUCER_FREES.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: the test plugin allocated the transferred payload with SDK.
        unsafe { rpi_plugin_sdk::callbacks::free_string(value) };
    }

    extern "C" fn poll_with_callback(
        handle: StepHandle,
        callback: Option<ToolPartialCb>,
        data: *mut c_void,
    ) -> StepResult {
        let result = stub_poll(handle, None, std::ptr::null_mut());
        if result.tag == rpi_plugin_sdk::StepResultTag::Pending {
            if let Some(callback) = callback {
                callback(
                    StbString::from_string(
                        r#"{"content":[{"type":"text","text":"callback"}]}"#.into(),
                    ),
                    data,
                );
            }
        }
        result
    }

    #[tokio::test]
    async fn callback_and_step_payloads_use_the_plugin_deallocator() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_counters();
        PRODUCER_FREES.store(0, Ordering::SeqCst);
        let mut handle = stub_handle();
        handle.poll_fn = poll_with_callback;
        handle.plugin_free_string = counted_plugin_free;
        let adapter = PluginToolAdapter::new(
            Tool {
                name: "producer-check".into(),
                description: "test".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
                constrained_sampling: None,
            },
            handle,
            PluginKeepalive::empty(),
        );
        let seen = Arc::new(AtomicUsize::new(0));
        let updates = seen.clone();
        adapter
            .execute(
                "call",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(move |_| {
                    updates.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await
            .unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 4); // Two callbacks and two pending results.
        assert_eq!(PRODUCER_FREES.load(Ordering::SeqCst), 5); // Plus the terminal result.
        assert_eq!(DESTROY_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closed_partial_receiver_still_reclaims_the_plugin_payload() {
        let _guard = TEST_LOCK.lock().unwrap();
        PRODUCER_FREES.store(0, Ordering::SeqCst);
        let (sender, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        let context = PartialCallbackContext {
            sender,
            plugin_free: counted_plugin_free,
        };
        partial_cb_trampoline(
            StbString::from_string("{}".into()),
            &context as *const _ as *mut c_void,
        );
        assert_eq!(PRODUCER_FREES.load(Ordering::SeqCst), 1);
    }

    /// The raw arguments string the most recent `stub_execute` was handed. The
    /// stub used to ignore its `params` entirely, which made it impossible to
    /// assert that the host context actually crosses the FFI boundary — the one
    /// thing about the injection that unit-testing the merge function cannot
    /// prove.
    static LAST_PARAMS: Mutex<Option<String>> = Mutex::new(None);

    fn take_last_params() -> Value {
        let raw = LAST_PARAMS
            .lock()
            .unwrap()
            .take()
            .expect("stub_execute should have recorded params");
        serde_json::from_str(&raw).expect("params should be JSON")
    }

    struct DriveState {
        polls: usize,
        done_at: usize,
    }

    static TEST_DRIVES: rpi_plugin_sdk::handles::ToolHandles<DriveState> =
        rpi_plugin_sdk::handles::ToolHandles::new();

    extern "C" fn stub_execute(
        _id: StbStringRef,
        params: StbString,
        free: Option<FreeStringFn>,
    ) -> StepHandle {
        // SAFETY: execute transfers live parameters with their producer.
        let owned = unsafe { OwnedString::from_raw(params, free) };
        *LAST_PARAMS.lock().unwrap() = Some(owned.to_string_lossy());
        TEST_DRIVES.insert(DriveState {
            polls: 0,
            done_at: 3,
        })
    }

    extern "C" fn stub_poll(
        h: StepHandle,
        _cb: Option<ToolPartialCb>,
        _ud: *mut c_void,
    ) -> StepResult {
        let Some(entry) = TEST_DRIVES.get(h) else {
            return StepResult::err(StbString::from_string("invalid handle".into()));
        };
        if entry.cancelled.load(Ordering::SeqCst) {
            return StepResult::err(StbString::from_string("cancelled".into()));
        }
        let mut state = entry.lock();
        state.polls += 1;
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
        TEST_DRIVES.cancel(h);
    }

    extern "C" fn stub_destroy(h: StepHandle) {
        DESTROY_COUNT.fetch_add(1, Ordering::SeqCst);
        drop(TEST_DRIVES.remove(h));
    }

    use rpi_plugin_sdk::callbacks::free_string as stub_free;

    fn stub_handle() -> PluginToolHandle {
        PluginToolHandle {
            execute_fn: stub_execute,
            set_waker_fn: None,
            poll_fn: stub_poll,
            cancel_fn: stub_cancel,
            destroy_fn: stub_destroy,
            plugin_free_string: stub_free,
            runtime_handle: None,
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
        assert_eq!(
            DESTROY_COUNT.load(Ordering::SeqCst),
            1,
            "destroy exactly once"
        );
        assert_eq!(
            CANCEL_COUNT.load(Ordering::SeqCst),
            0,
            "no cancel in happy path"
        );
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
        static SLOW_DRIVES: rpi_plugin_sdk::handles::ToolHandles<()> =
            rpi_plugin_sdk::handles::ToolHandles::new();
        extern "C" fn slow_execute(
            _: StbStringRef,
            params: StbString,
            free: Option<FreeStringFn>,
        ) -> StepHandle {
            // SAFETY: execute transfers parameters and their producer.
            drop(unsafe { OwnedString::from_raw(params, free) });
            SLOW_DRIVES.insert(())
        }
        extern "C" fn slow_poll(
            h: StepHandle,
            _: Option<ToolPartialCb>,
            _: *mut c_void,
        ) -> StepResult {
            let Some(entry) = SLOW_DRIVES.get(h) else {
                return StepResult::err(StbString::from_string("invalid handle".into()));
            };
            if entry.cancelled.load(Ordering::SeqCst) {
                return StepResult::err(StbString::from_string("cancelled".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            StepResult::pending(StbString::empty())
        }
        extern "C" fn slow_cancel(h: StepHandle) {
            CANCEL_COUNT.fetch_add(1, Ordering::SeqCst);
            SLOW_DRIVES.cancel(h);
        }
        extern "C" fn slow_destroy(h: StepHandle) {
            DESTROY_COUNT.fetch_add(1, Ordering::SeqCst);
            drop(SLOW_DRIVES.remove(h));
        }
        let tool = Tool {
            name: "slow".to_string(),
            description: "slow".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
            constrained_sampling: None,
        };
        let handle = PluginToolHandle {
            execute_fn: slow_execute,
            set_waker_fn: None,
            poll_fn: slow_poll,
            cancel_fn: slow_cancel,
            destroy_fn: slow_destroy,
            plugin_free_string: stub_free,
            runtime_handle: None,
        };
        let adapter = PluginToolAdapter::new(tool, handle, PluginKeepalive::empty());
        let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
        let signal = CancellationToken::new();
        let signal_clone = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            signal_clone.cancel();
        });
        let _ = adapter
            .execute("call_3", serde_json::json!({}), signal, on_update)
            .await;
        // No hang + destroy exactly once (cancel observed via the AtomicBool).
        assert_eq!(
            DESTROY_COUNT.load(Ordering::SeqCst),
            1,
            "destroy once on cancel"
        );
    }

    // ---- host context injection ----

    #[test]
    fn injected_context_carries_cwd_and_session_only_when_known() {
        // Nothing known yet → no key at all, rather than an empty object a
        // plugin would have to special-case.
        assert!(ToolCallContext::default().to_json().is_none());
        assert!(ToolCallContext::new("").to_json().is_none());

        let context = ToolCallContext::new("D:\\proj");
        let json = context.to_json().expect("cwd alone is enough to inject");
        assert_eq!(json["cwd"], "D:\\proj");
        assert!(json.get("sessionId").is_none(), "not yet recorded");

        context.set_session_id("sess-1");
        let json = context.to_json().unwrap();
        assert_eq!(json["cwd"], "D:\\proj");
        assert_eq!(json["sessionId"], "sess-1");
    }

    #[test]
    fn injection_targets_objects_and_leaves_other_shapes_alone() {
        let context = ToolCallContext::new("/proj");
        context.set_session_id("s");

        let merged = inject_tool_context(json!({"action": "list"}), &context);
        assert_eq!(merged["action"], "list", "model arguments survive");
        assert_eq!(merged["__rpi"]["sessionId"], "s");

        // A no-argument call arrives as `null`; it should still receive the
        // context instead of being left unusable as an object.
        let merged = inject_tool_context(Value::Null, &context);
        assert_eq!(merged["__rpi"]["cwd"], "/proj");

        // Array/scalar parameters cannot carry a key — pass through untouched.
        let array = json!([1, 2]);
        assert_eq!(inject_tool_context(array.clone(), &context), array);
        assert_eq!(inject_tool_context(json!("raw"), &context), json!("raw"));
    }

    #[test]
    fn host_context_overrides_a_forged_one() {
        let context = ToolCallContext::new("/real");
        context.set_session_id("real-session");
        let merged = inject_tool_context(
            json!({"action": "list", "__rpi": {"cwd": "/forged", "sessionId": "forged"}}),
            &context,
        );
        assert_eq!(merged["__rpi"]["cwd"], "/real");
        assert_eq!(merged["__rpi"]["sessionId"], "real-session");
    }

    #[test]
    fn injection_is_a_no_op_without_a_context() {
        let params = json!({"action": "list"});
        let merged = inject_tool_context(params.clone(), &ToolCallContext::default());
        assert_eq!(merged, params, "an unattached adapter behaves as before");
    }

    #[tokio::test]
    async fn plugin_receives_the_host_context_across_the_ffi_boundary() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_counters();
        *LAST_PARAMS.lock().unwrap() = None;

        let context = ToolCallContext::new("D:\\proj");
        context.set_session_id("sess-42");
        let adapter = echo_adapter().with_context(context);
        let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
        adapter
            .execute(
                "call_ctx",
                json!({"action": "list"}),
                CancellationToken::new(),
                on_update,
            )
            .await
            .expect("drive should succeed");

        let seen = take_last_params();
        assert_eq!(seen["action"], "list");
        assert_eq!(seen["__rpi"]["cwd"], "D:\\proj");
        assert_eq!(seen["__rpi"]["sessionId"], "sess-42");
    }
}
