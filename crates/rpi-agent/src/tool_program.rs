//! Host-owned, invocation-local nested execution. No model turns or transcript events.
use crate::{
    AgentContext, AgentEmitter, AgentEvent, AgentLoopConfig, TextContentOrImage, ToolExecutionMode,
};
use futures::{future::BoxFuture, FutureExt};
use rpi_ai::types::{AssistantMessage, ToolCall, ToolCallType};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);
tokio::task_local! { static CURRENT: Arc<ToolProgram>; static PARENT: NestedParent; }

/// Identity only the native execution driver can install; JSON is never trusted.
#[derive(Clone)]
pub struct NestedParent {
    pub root_id: String,
    pub root_name: String,
    pub parent_id: String,
}
pub fn nested_parent() -> Option<NestedParent> {
    PARENT.try_with(Clone::clone).ok()
}
pub fn current() -> Option<Arc<ToolProgram>> {
    CURRENT.try_with(Clone::clone).ok()
}
pub(crate) async fn scope<F: Future>(capability: Arc<ToolProgram>, future: F) -> F::Output {
    CURRENT.scope(capability, future).await
}
pub(crate) struct InvocationLifetime(pub CancellationToken);
impl Drop for InvocationLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Request {
    signal: CancellationToken,
    result: Option<Value>,
}
#[derive(Default)]
struct State {
    requests: HashMap<String, Request>,
}
/// Captured from the effective executing loop, never from a root registry.
pub struct ToolProgram {
    context: Arc<AgentContext>,
    assistant: Arc<AssistantMessage>,
    config: AgentLoopConfig,
    ancestors: Vec<String>,
    identity: NestedParent,
    child_scope: Option<Arc<crate::tool_execution_scope::ToolExecutionScope>>,
    starts: Arc<AtomicUsize>,
    state: Mutex<State>,
    serial: RwLock<()>,
    active: Arc<Semaphore>,
    tree_active: Arc<Semaphore>,
    ordered: bool,
    terminated: AtomicBool,
    pub(crate) signal: CancellationToken,
}
impl ToolProgram {
    pub(crate) fn capture(
        context: &Arc<AgentContext>,
        assistant: &Arc<AssistantMessage>,
        call: &ToolCall,
        config: &AgentLoopConfig,
        signal: CancellationToken,
    ) -> Arc<Self> {
        let previous = current();
        let mut ancestors = previous
            .as_ref()
            .map(|p| p.ancestors.clone())
            .unwrap_or_default();
        ancestors.push(call.name.clone());
        let identity = previous
            .as_ref()
            .map(|p| NestedParent {
                root_id: p.identity.root_id.clone(),
                root_name: p.identity.root_name.clone(),
                parent_id: call.id.clone(),
            })
            .unwrap_or_else(|| NestedParent {
                root_id: call.id.clone(),
                root_name: call.name.clone(),
                parent_id: call.id.clone(),
            });
        Arc::new(Self {
            context: context.clone(),
            assistant: assistant.clone(),
            config: config.clone(),
            ancestors,
            identity,
            child_scope: crate::tool_execution_scope::current_scope(),
            starts: previous
                .as_ref()
                .map(|p| p.starts.clone())
                .unwrap_or_default(),
            tree_active: previous
                .as_ref()
                .map(|p| p.tree_active.clone())
                .unwrap_or_else(|| Arc::new(Semaphore::new(8))),
            ordered: config.tool_execution == ToolExecutionMode::Sequential
                || context.tools.iter().any(|t| {
                    t.schema().name == call.name
                        && t.execution_mode() == ToolExecutionMode::Sequential
                }),
            state: Mutex::default(),
            serial: RwLock::new(()),
            active: Arc::new(Semaphore::new(8)),
            terminated: AtomicBool::new(false),
            signal,
        })
    }
    /// Effective names, including this tool, for the existing names-only action.
    pub fn active_tool_names(&self) -> Vec<String> {
        self.context
            .tools
            .iter()
            .map(|t| t.schema().name.clone())
            .collect()
    }
    /// Cancellation stops admission. Cleanup/poll remain available to the binding.
    pub fn cancel_all(&self) {
        self.signal.cancel();
    }
    /// Observe driver-local cancellation, without retaining a finished invocation.
    pub fn bind_signal(self: &Arc<Self>, signal: CancellationToken) {
        let weak = Arc::downgrade(self);
        let finished = self.signal.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = signal.cancelled() => {
                    if let Some(program) = weak.upgrade() { program.cancel_all(); }
                }
                _ = finished.cancelled() => {}
            }
        });
    }
    pub(crate) async fn finish(&self) {
        self.cancel_all();
        // An outer Sequential call cannot finish ahead of its descendants.
        let _idle = self.active.acquire_many(8).await;
    }
    pub fn handle(self: &Arc<Self>, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        let this = self.clone();
        Box::pin(async move { this.handle_inner(args).await })
    }
    async fn handle_inner(self: &Arc<Self>, args: Value) -> Result<Value, String> {
        bounded(&args)?;
        let op = args["op"].as_str().ok_or("op is required")?;
        if matches!(op, "list" | "start")
            && (self.signal.is_cancelled() || self.terminated.load(Ordering::SeqCst))
        {
            return Err("parent invocation is no longer active".into());
        }
        match op {
            "list" => {
                let offset = integer(&args, "offset", 0, 0, 4096)? as usize;
                let limit = integer(&args, "limit", 32, 1, 128)? as usize;
                let tools: Vec<_> = self
                    .context
                    .tools
                    .iter()
                    .filter(|t| !self.ancestors.contains(&t.schema().name))
                    .collect();
                if tools.len() > 4096 {
                    return Err("tool catalog exceeds 4096 entries".into());
                }
                // Bound the complete catalog as well as each page.
                let all:Vec<_> = tools.iter().map(|t|json!({"name":t.schema().name,"description":t.schema().description,"parameters":t.schema().parameters})).collect();
                bounded(&json!(all))?;
                let end = offset.saturating_add(limit).min(all.len());
                let page = all.get(offset..end).unwrap_or(&[]);
                Ok(
                    json!({"protocolVersion":1,"tools":page,"nextOffset":if end<all.len(){Some(end)}else{None}}),
                )
            }
            "start" => {
                // The executing parent cannot hold its children's gate. It
                // must instead exclude peers in its own containing batch.
                // Inductively, this also orders independent program groups.
                if !self.ordered {
                    return Err(
                        "ToolProgram start requires a Sequential executing tool or loop".into(),
                    );
                }
                let name = args["name"].as_str().ok_or("name is required")?.to_owned();
                if self.ancestors.contains(&name) || self.ancestors.len() >= 16 {
                    return Err("recursive/ancestor tool invocation is forbidden".into());
                }
                let tool = self
                    .context
                    .tools
                    .iter()
                    .find(|t| t.schema().name == name)
                    .ok_or("tool is not in the effective active subset")?;
                let sequential = self.config.tool_execution == ToolExecutionMode::Sequential
                    || tool.execution_mode() == ToolExecutionMode::Sequential;
                let arguments = args
                    .get("arguments")
                    .filter(|v| v.is_object())
                    .ok_or("arguments must be an object")?
                    .clone();
                let timeout = integer(&args, "timeoutMs", 30000, 1, 120000)?;
                let permit = self
                    .active
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "ToolProgram active request limit exceeded")?;
                let tree_permit = self
                    .tree_active
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "ToolProgram invocation-tree active request limit exceeded")?;
                let signal = self.signal.child_token();
                let id = format!("tp-{}", NEXT.fetch_add(1, Ordering::Relaxed));
                {
                    let mut state = self.state.lock().unwrap();
                    if state.requests.len() >= 16 {
                        return Err("ToolProgram retained/active request limit exceeded".into());
                    }
                    #[allow(deprecated)] // Keep the workspace Rust 1.88 MSRV.
                    if self
                        .starts
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            (n < 128).then_some(n + 1)
                        })
                        .is_err()
                    {
                        return Err("ToolProgram start budget exceeded".into());
                    }
                    if self.signal.is_cancelled() {
                        return Err("parent invocation is no longer active".into());
                    }
                    state.requests.insert(
                        id.clone(),
                        Request {
                            signal: signal.clone(),
                            result: None,
                        },
                    );
                }
                let this = self.clone();
                let request_id = id.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _tree_permit = tree_permit;
                    let work = async {
                        // The parent never takes this lock: Sequential programs cannot self-deadlock.
                        let _write;
                        let _read;
                        if sequential {
                            _write = Some(this.serial.write().await);
                            _read = None;
                        } else {
                            _read = Some(this.serial.read().await);
                            _write = None;
                        }
                        if signal.is_cancelled() {
                            return Err("nested invocation cancelled".to_owned());
                        }
                        let call = ToolCall {
                            kind: ToolCallType,
                            id: request_id.clone(),
                            name,
                            arguments,
                            thought_signature: None,
                            namespace: None,
                            recorded: None,
                        };
                        let mut config = this.config.clone();
                        config.signal = signal.clone();
                        let emit: Arc<dyn AgentEmitter> = Arc::new(SilentEmitter);
                        let execute = PARENT.scope(
                            this.identity.clone(),
                            scope(
                                this.clone(),
                                crate::agent_loop::run_one_tool_call(
                                    &this.context,
                                    &this.assistant,
                                    &call,
                                    &config,
                                    &emit,
                                ),
                            ),
                        );
                        let result = if let Some(scope) = &this.child_scope {
                            scope.run(execute).await
                        } else {
                            execute.await
                        };
                        let finalized = result.map_err(|e| e.to_string())?;
                        let result = &finalized.result;
                        // Finalization (including hooks), not reply encoding,
                        // decides admission. Serialization/bounds may fail.
                        if result.terminate {
                            this.terminated.store(true, Ordering::SeqCst);
                        }
                        let content: Vec<Value> = result
                            .content
                            .iter()
                            .map(|c| match c {
                                TextContentOrImage::Text(t) => serde_json::to_value(t),
                                TextContentOrImage::Image(i) => serde_json::to_value(i),
                            })
                            .collect::<Result<_, _>>()
                            .map_err(|e| e.to_string())?;
                        let reply = json!({"requestId":request_id,"status":"done","isError":finalized.is_error,"result":{"content":content,"details":result.details,"terminate":result.terminate,"addedToolNames":result.added_tool_names,"usage":result.usage}});
                        bounded(&reply)?;
                        Ok::<_, String>(reply)
                    };
                    let work = std::panic::AssertUnwindSafe(work).catch_unwind();
                    tokio::pin!(work);
                    let (result, drain) = tokio::select! {
                        biased;
                        _ = signal.cancelled() => (Err("nested invocation cancelled".into()), true),
                        _ = tokio::time::sleep(Duration::from_millis(timeout)) => (
                            Err("nested invocation timed out; side effects may have occurred".into()), true),
                        result = &mut work => (match result {
                            Ok(reply) => reply,
                            Err(_) => Err("nested invocation panicked".into()),
                        }, false),
                    };
                    signal.cancel();
                    let reply = result.unwrap_or_else(
                        |error| json!({"requestId":request_id,"status":"failed","error":error}),
                    );
                    if let Some(request) = this.state.lock().unwrap().requests.get_mut(&request_id)
                    {
                        request.result = Some(reply);
                    }
                    if drain {
                        // Publish promptly, but never drop native execution.
                        // Its gate and our permits survive foreign cancel/destroy.
                        let _ = work.await;
                    }
                });
                Ok(json!({"requestId":id,"status":"running"}))
            }
            "poll" | "cancel" | "dispose" => {
                let id = args["requestId"].as_str().ok_or("requestId is required")?;
                let mut state = self.state.lock().unwrap();
                if op == "dispose" {
                    if let Some(r) = state.requests.remove(id) {
                        r.signal.cancel();
                    }
                    return Ok(json!({"requestId":id,"status":"disposed"}));
                }
                let request = state
                    .requests
                    .get_mut(id)
                    .ok_or("unknown request for this invocation")?;
                if op == "cancel" {
                    request.signal.cancel();
                    Ok(json!({"requestId":id,"status":"cancelled"}))
                } else {
                    Ok(request
                        .result
                        .clone()
                        .unwrap_or_else(|| json!({"requestId":id,"status":"running"})))
                }
            }
            _ => Err("unknown ToolProgram operation".into()),
        }
    }
}
fn bounded(v: &Value) -> Result<(), String> {
    if serde_json::to_vec(v).map_err(|e| e.to_string())?.len() > MAX_BYTES {
        Err("ToolProgram JSON exceeds 1 MiB".into())
    } else {
        Ok(())
    }
}
fn integer(args: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        Err(format!("{key} must be between {min} and {max}"))
    } else {
        Ok(value)
    }
}
struct SilentEmitter;
impl AgentEmitter for SilentEmitter {
    fn emit(&self, _: AgentEvent) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}
