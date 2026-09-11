//! Multiplexed JSON-lines transport for long-lived Node extension runtimes.
//!
//! Requests may complete out of order. A dedicated reader dispatches each
//! response by id, while Node-to-Rust runtime requests use the same stdin pipe
//! for their replies. This keeps transport concerns out of the Pi adapter and
//! gives persistent packages and future one-shot PTC runtimes one protocol.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde_json::Value;

pub type RuntimeHandler = Arc<dyn Fn(&str, Value) -> Result<Value, String> + Send + Sync>;

type Response = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Response>>>>;

struct Inner {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    runtime_handlers: Arc<Mutex<Vec<RuntimeHandler>>>,
    next_id: AtomicU64,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        fail_pending(&self.pending, "Node extension host stopped");
        if let Ok(reader) = self.reader.get_mut() {
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
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
        let mut stdout = BufReader::new(stdout);
        let init = read_value(&mut stdout)?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let runtime_handlers = Arc::new(Mutex::new(Vec::new()));
        let inner = Arc::new(Inner {
            child: Mutex::new(child),
            stdin: stdin.clone(),
            pending: pending.clone(),
            runtime_handlers: runtime_handlers.clone(),
            next_id: AtomicU64::new(1),
            reader: Mutex::new(None),
        });
        let reader = std::thread::Builder::new()
            .name("rpi-node-transport".into())
            .spawn(move || read_loop(stdout, stdin, pending, runtime_handlers))
            .map_err(|error| format!("could not start Node response reader: {error}"))?;
        *inner
            .reader
            .lock()
            .map_err(|_| "Node reader lock poisoned")? = Some(reader);
        Ok((Self { inner }, init))
    }

    pub fn begin_request(&self, method: &str, payload: Value) -> Result<PendingRequest, String> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .map_err(|_| "Node pending-request lock poisoned")?
            .insert(id, sender);

        let mut request = serde_json::json!({"id": id, "method": method});
        if let (Some(object), Some(values)) = (request.as_object_mut(), payload.as_object()) {
            object.extend(values.clone());
        }
        if let Err(error) = write_value(&self.inner.stdin, &request) {
            if let Ok(mut pending) = self.inner.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }
        Ok(PendingRequest { id, receiver })
    }

    pub fn request(&self, method: &str, payload: Value) -> Response {
        self.begin_request(method, payload)?.wait()
    }

    pub fn send_event(&self, event: Value) -> Result<(), String> {
        write_value(&self.inner.stdin, &event)
    }

    pub fn cancel(&self, id: u64) -> Result<(), String> {
        self.send_event(serde_json::json!({
            "type": "host_event",
            "event": "cancel_request",
            "id": id,
        }))
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
    pending: Pending,
    handlers: Arc<Mutex<Vec<RuntimeHandler>>>,
) {
    loop {
        let message = match read_value(&mut stdout) {
            Ok(message) => message,
            Err(error) => {
                fail_pending(&pending, &error);
                return;
            }
        };
        if message.get("type").and_then(Value::as_str) == Some("runtime_request") {
            let background = message
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(runtime_action_may_block);
            if background {
                let stdin = stdin.clone();
                let handlers = handlers.clone();
                std::thread::spawn(move || handle_runtime_request(message, &stdin, &handlers));
            } else {
                handle_runtime_request(message, &stdin, &handlers);
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
        let response = if message.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(message.get("result").cloned().unwrap_or_default())
        } else {
            Err(message
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("JS extension failed")
                .to_string())
        };
        let _ = sender.send(response);
    }
}

fn runtime_action_may_block(action: &str) -> bool {
    action.starts_with("provider.")
        || action.starts_with("tools.")
        || action.starts_with("session.")
}

fn handle_runtime_request(
    message: Value,
    stdin: &Arc<Mutex<ChildStdin>>,
    handlers: &Arc<Mutex<Vec<RuntimeHandler>>>,
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
    let _ = write_value(stdin, &response);
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
