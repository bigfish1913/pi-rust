//! Multiplexed JSON-lines transport for long-lived Node extension runtimes.
//!
//! Requests may complete out of order. A dedicated reader dispatches each
//! response by id, while Node-to-Rust runtime requests use the same stdin pipe
//! for their replies. This keeps transport concerns out of the Pi adapter and
//! gives persistent packages and future one-shot PTC runtimes one protocol.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde_json::Value;

pub type RuntimeHandler = Arc<dyn Fn(&str, Value) -> Result<Value, String> + Send + Sync>;

type Response = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Response>>>>;
pub type ToolUpdateHandler = Arc<dyn Fn(Value) + Send + Sync>;

const NODE_STOPPED: &str = "Node extension host stopped";

struct Inner {
    /// Shared with the reader thread so an EOF or malformed response can
    /// terminate a broken host even while other transport clones remain alive.
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    tool_update_handlers: Arc<Mutex<HashMap<String, ToolUpdateHandler>>>,
    runtime_handlers: Arc<Mutex<Vec<RuntimeHandler>>>,
    next_id: AtomicU64,
    /// Shared with the reader thread so a natural EOF or malformed response
    /// makes every existing transport clone observe the terminal state.
    shutdown: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    cleanup_path: Option<PathBuf>,
}

impl Inner {
    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    fn mark_failed(&self, error: &str) {
        self.shutdown.store(true, Ordering::Release);
        fail_pending(&self.pending, error);
        terminate_child(&self.child);
    }

    /// Stop the child and wake every request that is waiting on its response.
    ///
    /// The operation is deliberately idempotent. A session can be shut down
    /// while detached command/tool work still owns a transport clone, and the
    /// final `Drop` must be able to run the same cleanup without racing a
    /// second kill/wait.
    fn shutdown(&self) {
        let first_shutdown = !self.shutdown.swap(true, Ordering::AcqRel);
        // Wake callers before waiting for the process. `Child::kill` is
        // synchronous on some platforms and must not keep a pending command
        // blocked behind process cleanup.
        if first_shutdown {
            fail_pending(&self.pending, NODE_STOPPED);
        }
        terminate_child(&self.child);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
        if let Ok(reader) = self.reader.get_mut() {
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
        }
        if let Some(path) = self.cleanup_path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Clone)]
pub struct NodeTransport {
    inner: Arc<Inner>,
}

pub struct PendingRequest {
    id: u64,
    receiver: mpsc::Receiver<Response>,
}

/// A spawned Node host whose initialization response has not necessarily
/// arrived yet.  Keeping the transport available separately from the init
/// receiver lets callers publish it to their shutdown path before extension
/// factories or lifecycle hooks can block.
pub struct NodeTransportStartup {
    transport: NodeTransport,
    init_receiver: mpsc::Receiver<Response>,
}

impl NodeTransportStartup {
    /// Return a clone of the live transport.  The clone is intentionally cheap
    /// and can be stored in a concurrent shutdown slot while `wait` blocks.
    pub fn transport(&self) -> NodeTransport {
        self.transport.clone()
    }

    /// Wait for the host's `id:0` initialization envelope.  Any failed or
    /// closed initialization path terminates the child before returning.
    pub fn wait(self) -> Result<(NodeTransport, Value), String> {
        let result = match self.init_receiver.recv() {
            Ok(Ok(init)) => Ok(init),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("Node extension host exited before initialization".into()),
        };
        match result {
            Ok(init) => Ok((self.transport, init)),
            Err(error) => {
                self.transport.shutdown();
                Err(error)
            }
        }
    }
}

impl PendingRequest {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn wait(self) -> Response {
        self.receiver
            .recv()
            .unwrap_or_else(|_| Err("Node extension response channel closed".into()))
    }
}

impl NodeTransport {
    pub fn start(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> Result<(Self, Value), String> {
        Self::start_with_cleanup_and_handlers(child, stdin, stdout, None, Vec::new())
    }

    /// Start a transport and remove an optional host script when the child is
    /// dropped. Windows cannot carry the embedded Node source in `node -e`
    /// once the host grows beyond the CreateProcess command-line limit, so the
    /// caller may launch a temporary `.mjs` file and hand its path to us.
    pub fn start_with_cleanup(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        cleanup_path: Option<PathBuf>,
    ) -> Result<(Self, Value), String> {
        Self::start_with_cleanup_and_handlers(child, stdin, stdout, cleanup_path, Vec::new())
    }

    /// Start a transport with runtime handlers that are available while the
    /// Node host is loading extension factories and lifecycle hooks. The host
    /// may issue `runtime_request` messages before it publishes its `id:0`
    /// initialization response, so the reader must be running before waiting
    /// for that response.
    pub fn start_with_cleanup_and_handlers(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        cleanup_path: Option<PathBuf>,
        initial_handlers: Vec<RuntimeHandler>,
    ) -> Result<(Self, Value), String> {
        Self::start_pending_with_cleanup_and_handlers(
            child,
            stdin,
            stdout,
            cleanup_path,
            initial_handlers,
        )?
        .wait()
    }

    /// Spawn the transport and start its reader, returning before the Node
    /// host's initialization response arrives.  This is the primitive used by
    /// the lazy runtime so shutdown can kill a host whose extension factory is
    /// stuck before it emits `id:0`.
    pub fn start_pending_with_cleanup_and_handlers(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        cleanup_path: Option<PathBuf>,
        initial_handlers: Vec<RuntimeHandler>,
    ) -> Result<NodeTransportStartup, String> {
        let stdout = BufReader::new(stdout);
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let tool_update_handlers = Arc::new(Mutex::new(HashMap::new()));
        let runtime_handlers = Arc::new(Mutex::new(initial_handlers));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (init_sender, init_receiver) = mpsc::channel();
        let child = Arc::new(Mutex::new(child));
        let inner = Arc::new(Inner {
            child: child.clone(),
            stdin: stdin.clone(),
            pending: pending.clone(),
            tool_update_handlers: tool_update_handlers.clone(),
            runtime_handlers: runtime_handlers.clone(),
            next_id: AtomicU64::new(1),
            shutdown: shutdown.clone(),
            reader: Mutex::new(None),
            cleanup_path,
        });
        let reader = std::thread::Builder::new()
            .name("rpi-node-transport".into())
            .spawn(move || {
                read_loop(
                    stdout,
                    stdin,
                    child,
                    pending,
                    tool_update_handlers,
                    runtime_handlers,
                    Some(init_sender),
                    shutdown,
                )
            })
            .map_err(|error| {
                terminate_child(&inner.child);
                if let Some(path) = inner.cleanup_path.as_ref() {
                    let _ = std::fs::remove_file(path);
                }
                format!("could not start Node response reader: {error}")
            })?;
        if let Err(error) = inner.reader.lock().map(|mut slot| *slot = Some(reader)) {
            terminate_child(&inner.child);
            if let Some(path) = inner.cleanup_path.as_ref() {
                let _ = std::fs::remove_file(path);
            }
            return Err(format!("Node reader lock poisoned: {error}"));
        }

        // Initialization is delivered by the same reader that handles all
        // subsequent responses. This prevents a startup runtime request from
        // being mistaken for the first response or blocking the response
        // reader until after the lifecycle hook completes.
        Ok(NodeTransportStartup {
            transport: Self { inner },
            init_receiver,
        })
    }

    /// Stop the Node host and resolve all currently pending requests with an
    /// error. Safe to call repeatedly and while other transport clones exist.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    /// Whether the reader or an explicit shutdown has made this transport
    /// unusable. Lazy owners use this to discard a stale slot and retry on the
    /// next request.
    pub fn is_shutdown(&self) -> bool {
        self.inner.is_shutdown()
    }

    /// Whether two handles refer to the same child process.  Lazy startup can
    /// briefly expose one handle through both its `starting` and `transport`
    /// slots; callers updating shared runtime handlers must avoid appending the
    /// same callback twice in that window.
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn begin_request(&self, method: &str, payload: Value) -> Result<PendingRequest, String> {
        if self.inner.is_shutdown() {
            return Err(NODE_STOPPED.into());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| "Node pending-request lock poisoned")?;
        if self.inner.is_shutdown() {
            return Err(NODE_STOPPED.into());
        }
        pending.insert(id, sender);
        // Close the small race where shutdown sets the flag after the check
        // above but before this request is inserted. Holding the pending lock
        // makes shutdown either drain this entry or observe this branch.
        if self.inner.is_shutdown() {
            let sender = pending.remove(&id);
            drop(pending);
            if let Some(sender) = sender {
                let _ = sender.send(Err(NODE_STOPPED.into()));
            }
            return Err(NODE_STOPPED.into());
        }
        drop(pending);

        let mut request = serde_json::json!({"id": id, "method": method});
        if let (Some(object), Some(values)) = (request.as_object_mut(), payload.as_object()) {
            object.extend(values.clone());
        }
        if let Err(error) = write_value(&self.inner.stdin, &request) {
            if let Ok(mut pending) = self.inner.pending.lock() {
                pending.remove(&id);
            }
            self.inner.mark_failed(&error);
            return Err(error);
        }
        Ok(PendingRequest { id, receiver })
    }

    pub fn request(&self, method: &str, payload: Value) -> Response {
        self.begin_request(method, payload)?.wait()
    }

    pub fn send_event(&self, event: Value) -> Result<(), String> {
        if self.inner.is_shutdown() {
            return Err(NODE_STOPPED.into());
        }
        if let Err(error) = write_value(&self.inner.stdin, &event) {
            self.inner.mark_failed(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Send a host event and wait for a normal id-correlated response. This
    /// is used for terminal input listeners, whose `consume` result must be
    /// known before the outer Rust TUI dispatches the same key.
    pub fn request_event(&self, mut event: Value) -> Response {
        if self.inner.is_shutdown() {
            return Err(NODE_STOPPED.into());
        }
        let supported = event
            .get("event")
            .and_then(Value::as_str)
            .is_some_and(|name| matches!(name, "custom_input" | "custom_resize"));
        if !supported {
            return Err("host event does not support request/response mode".into());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| "Node pending-request lock poisoned")?;
        if self.inner.is_shutdown() {
            return Err(NODE_STOPPED.into());
        }
        pending.insert(id, sender);
        if self.inner.is_shutdown() {
            let sender = pending.remove(&id);
            drop(pending);
            if let Some(sender) = sender {
                let _ = sender.send(Err(NODE_STOPPED.into()));
            }
            return Err(NODE_STOPPED.into());
        }
        drop(pending);
        let Some(object) = event.as_object_mut() else {
            if let Ok(mut pending) = self.inner.pending.lock() {
                pending.remove(&id);
            }
            return Err("Node host event must be a JSON object".into());
        };
        object.insert("id".into(), Value::from(id));
        if let Err(error) = write_value(&self.inner.stdin, &event) {
            if let Ok(mut pending) = self.inner.pending.lock() {
                pending.remove(&id);
            }
            self.inner.mark_failed(&error);
            return Err(error);
        }
        receiver
            .recv()
            .unwrap_or_else(|_| Err("Node extension response channel closed".into()))
    }

    pub fn cancel(&self, id: u64) -> Result<(), String> {
        self.send_event(serde_json::json!({
            "type": "host_event",
            "event": "cancel_request",
            "id": id,
        }))
    }

    /// Register a callback for partial updates emitted by one JS tool call.
    ///
    /// The callback is intentionally keyed by the Pi tool-call id rather than
    /// the transport request id: the former is what Node receives and what
    /// remains stable when a request is cancelled or retried.
    pub fn register_tool_update_handler(
        &self,
        tool_call_id: impl Into<String>,
        handler: ToolUpdateHandler,
    ) -> Result<(), String> {
        self.inner
            .tool_update_handlers
            .lock()
            .map_err(|_| "Node tool-update lock poisoned")?
            .insert(tool_call_id.into(), handler);
        Ok(())
    }

    /// Stop routing partial updates after a tool call settles. This also
    /// suppresses updates sent by a JS tool after its execute promise resolves,
    /// matching the agent loop's late-update gate.
    pub fn unregister_tool_update_handler(&self, tool_call_id: &str) {
        if let Ok(mut handlers) = self.inner.tool_update_handlers.lock() {
            handlers.remove(tool_call_id);
        }
    }

    pub fn replace_runtime_handlers(&self, handler: RuntimeHandler) -> Result<(), String> {
        *self
            .inner
            .runtime_handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")? = vec![handler];
        Ok(())
    }

    pub fn add_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.inner
            .runtime_handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")?
            .push(handler);
        Ok(())
    }
}

fn read_loop(
    mut stdout: BufReader<ChildStdout>,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    pending: Pending,
    tool_update_handlers: Arc<Mutex<HashMap<String, ToolUpdateHandler>>>,
    handlers: Arc<Mutex<Vec<RuntimeHandler>>>,
    init_sender: Option<mpsc::Sender<Response>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut init_sender = init_sender;
    loop {
        let message = match read_value(&mut stdout) {
            Ok(message) => message,
            Err(error) => {
                // A reader failure is terminal even when the child process
                // itself is still around (for example after malformed JSON).
                // Publish that state before waking callers so lazy owners do
                // not keep reusing a broken stdin/stdout pair.
                shutdown.store(true, Ordering::Release);
                fail_pending(&pending, &error);
                terminate_child(&child);
                if let Some(sender) = init_sender.take() {
                    let _ = sender.send(Err(error));
                }
                return;
            }
        };
        // The host reserves id 0 for its initialization envelope. Route it
        // through a dedicated channel while leaving the reader alive for
        // runtime requests and normal request/response traffic.
        if init_sender.is_some() && message.get("id").and_then(Value::as_u64) == Some(0) {
            if let Some(sender) = init_sender.take() {
                // Preserve the full initialization envelope for callers. The
                // regular request path unwraps `result`, but startup callers
                // inspect the `ok` and `result` fields themselves.
                let _ = sender.send(Ok(message));
            }
            continue;
        }
        if message.get("type").and_then(Value::as_str) == Some("runtime_request") {
            let background = message
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(runtime_action_may_block);
            if background {
                let stdin = stdin.clone();
                let handlers = handlers.clone();
                let pending = pending.clone();
                let child = child.clone();
                let shutdown = shutdown.clone();
                std::thread::spawn(move || {
                    handle_runtime_request(message, &stdin, &handlers, &pending, &child, &shutdown)
                });
            } else {
                handle_runtime_request(message, &stdin, &handlers, &pending, &child, &shutdown);
            }
            continue;
        }
        if message.get("type").and_then(Value::as_str) == Some("host_event")
            && message.get("event").and_then(Value::as_str) == Some("tool_update")
        {
            let Some(tool_call_id) = message.get("toolCallId").and_then(Value::as_str) else {
                continue;
            };
            let handler = tool_update_handlers
                .lock()
                .ok()
                .and_then(|handlers| handlers.get(tool_call_id).cloned());
            if let Some(handler) = handler {
                handler(message.get("partialResult").cloned().unwrap_or_default());
            }
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let sender = pending
            .lock()
            .ok()
            .and_then(|mut values| values.remove(&id));
        let Some(sender) = sender else {
            continue;
        };
        let response = response_from_message(&message);
        let _ = sender.send(response);
    }
}

/// Kill and reap the host after a terminal transport failure. This is kept
/// separate from `Inner::shutdown` because the reader thread owns no `Inner`
/// handle and must never try to join itself.
fn terminate_child(child: &Arc<Mutex<Child>>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn response_from_message(message: &Value) -> Response {
    if message.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(message.get("result").cloned().unwrap_or_default())
    } else {
        Err(message
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("JS extension failed")
            .to_string())
    }
}

fn runtime_action_may_block(action: &str) -> bool {
    action.starts_with("provider.")
        || action.starts_with("tools.")
        || action.starts_with("session.")
        // A dialog waits for a key press in the TUI. Run it away from the
        // response reader so a concurrent `ui.dialog.cancel` can still be
        // dispatched when the command is aborted or times out.
        || action == "ui.dialog"
}

fn handle_runtime_request(
    message: Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    handlers: &Arc<Mutex<Vec<RuntimeHandler>>>,
    pending: &Pending,
    child: &Arc<Mutex<Child>>,
    shutdown: &Arc<AtomicBool>,
) {
    let Some(request_id) = message.get("requestId").and_then(Value::as_u64) else {
        return;
    };
    let Some(action) = message.get("action").and_then(Value::as_str) else {
        return;
    };
    let args = message.get("args").cloned().unwrap_or_default();
    let handlers = handlers.lock().map(|values| values.clone());
    let mut result = Err(format!("unsupported capability: {action}"));
    match handlers {
        Ok(handlers) => {
            for handler in handlers {
                match handler(action, args.clone()) {
                    Ok(value) => {
                        result = Ok(value);
                        break;
                    }
                    Err(error) if error.starts_with("unsupported capability:") => {}
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
        }
        Err(_) => result = Err("Node runtime handler lock poisoned".into()),
    }
    let response = match result {
        Ok(result) => serde_json::json!({
            "type": "runtime_response",
            "requestId": request_id,
            "ok": true,
            "result": result,
        }),
        Err(error) => serde_json::json!({
            "type": "runtime_response",
            "requestId": request_id,
            "ok": false,
            "error": error,
        }),
    };
    if let Err(error) = write_value(stdin, &response) {
        // A runtime response is part of the host protocol. If Rust cannot
        // deliver it, the Node side may be blocked forever waiting for this
        // request. Tear down the broken transport and wake every Rust caller
        // instead of silently leaving both sides hanging.
        shutdown.store(true, Ordering::Release);
        fail_pending(pending, &error);
        terminate_child(child);
    }
}

fn write_value(stdin: &Arc<Mutex<ChildStdin>>, value: &Value) -> Result<(), String> {
    let mut stdin = stdin.lock().map_err(|_| "Node host stdin lock poisoned")?;
    writeln!(stdin, "{value}")
        .map_err(|error| format!("could not write to Node extension host: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("could not flush Node extension host: {error}"))
}

fn read_value(reader: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("could not read Node extension host: {error}"))?;
    if line.trim().is_empty() {
        return Err("Node extension host exited without a response".into());
    }
    serde_json::from_str(&line).map_err(|error| format!("invalid Node extension response: {error}"))
}

fn fail_pending(pending: &Pending, error: &str) {
    let values = pending
        .lock()
        .map(|mut values| values.drain().map(|(_, sender)| sender).collect::<Vec<_>>())
        .unwrap_or_default();
    for sender in values {
        let _ = sender.send(Err(error.to_string()));
    }
}
