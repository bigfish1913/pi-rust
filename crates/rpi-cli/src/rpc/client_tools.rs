//! Session-owned, explicitly activated client text and terminal operations.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rpi_tools::tools::client::ClientToolBackend;
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio_util::task::TaskTracker;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::client_requests::{ClientRequestBroker, MAX_TIMEOUT_MS};
use super::writer::{write_line, RpcWriter};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ClientToolsConfig {
    pub version: u32,
    #[serde(default)]
    pub read_text: bool,
    #[serde(default)]
    pub write_text: bool,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub workspace_roots: Vec<std::path::PathBuf>,
    #[serde(default = "default_timeout")]
    pub request_timeout_ms: u64,
}
fn default_timeout() -> u64 { MAX_TIMEOUT_MS }

impl ClientToolsConfig {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let config: Self = serde_json::from_str(raw).map_err(|e| format!("RPI_CLIENT_TOOLS: {e}"))?;
        if config.version != 1 || !(1..=MAX_TIMEOUT_MS).contains(&config.request_timeout_ms) {
            return Err("RPI_CLIENT_TOOLS requires version 1 and requestTimeoutMs in 1..=60000".into());
        }
        for root in &config.workspace_roots {
            if !root.is_absolute() || !root.is_dir() {
                return Err("RPI_CLIENT_TOOLS workspaceRoots must be absolute existing directories".into());
            }
        }
        Ok(config)
    }
}

#[derive(Clone)]
struct Binding { writer: RpcWriter, session_id: String }

pub(crate) struct ClientTools {
    pub config: ClientToolsConfig,
    env: Arc<OsExecutionEnv>,
    pub broker: ClientRequestBroker,
    binding: Mutex<Option<Binding>>,
    stop: CancellationToken,
    // Admission and shutdown are serialized; never hold this across an await.
    admission: Mutex<()>,
    tasks: TaskTracker,
}

impl ClientTools {
    pub(crate) fn new(config: ClientToolsConfig, env: Arc<OsExecutionEnv>) -> Arc<Self> {
        Arc::new(Self { config, env, broker: ClientRequestBroker::default(), binding: Mutex::new(None),
            stop: CancellationToken::new(), admission: Mutex::new(()), tasks: TaskTracker::new() })
    }
    pub(crate) fn attach(&self, writer: RpcWriter, session_id: String) {
        *self.binding.lock().unwrap() = Some(Binding { writer, session_id });
    }
    fn spawn_owner(&self, task: impl std::future::Future<Output = ()> + Send + 'static) -> Result<(), String> {
        let _admission = self.admission.lock().unwrap();
        if self.stop.is_cancelled() {
            return Err("client transport closed".into());
        }
        self.tasks.spawn(task);
        Ok(())
    }
    pub(crate) async fn shutdown(&self) {
        {
            let _admission = self.admission.lock().unwrap();
            self.stop.cancel();
            self.tasks.close();
        }
        // Owners can still add cleanup children while tracked. Do not clear
        // the broker here: that would race kill/release responses.
        self.tasks.wait().await;
        self.broker.cancel_all();
        self.binding.lock().unwrap().take();
    }
    async fn request(self: &Arc<Self>, method: &str, params: Value, cancel: &CancellationToken,
        timeout: Duration, cleanup: bool) -> Result<Value, String> {
        // Resolve once on the originating task. Never rediscover a root binding
        // in a detached worker or in terminal cleanup after scope exit.
        let mut binding = self.binding.lock().unwrap().clone().ok_or("client tools require an attached RPC transport")?;
        if rpi_agent::tool_execution_scope::is_child_execution() {
            binding.session_id = rpi_agent::tool_execution_scope::execution_session_id()
                .filter(|id| !id.is_empty()).ok_or("child execution lacks trusted session identity")?;
        }
        let service = self.clone();
        let method = method.to_owned();
        let owned_cancel = cancel.child_token();
        let _cancel_on_drop = owned_cancel.clone().drop_guard();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let worker = async move {
            let result = service.request_inner(binding, &method, params, &owned_cancel, timeout, cleanup).await;
            let _ = tx.send(result);
        };
        if cleanup {
            // Only tracked terminal owners issue creation/cleanup, so the tracker
            // cannot become empty before this child is registered.
            self.tasks.spawn(worker);
        } else {
            self.spawn_owner(worker)?;
        }
        rx.await.unwrap_or_else(|_| Err("client request owner failed".into()))
    }
    async fn request_inner(self: &Arc<Self>, binding: Binding, method: &str, params: Value, cancel: &CancellationToken,
        timeout: Duration, cleanup: bool) -> Result<Value, String> {
        if !cleanup && (cancel.is_cancelled() || self.stop.is_cancelled()) {
            return Err("client request cancelled".into());
        }
        let request = self.broker.register()?;
        let event = json!({"type":"client_request", "requestId":request.id,
            "sessionId":binding.session_id, "method":method, "params":params});
        let _close = CloseRequest { service: self.clone(), writer: binding.writer.clone(), id: request.id.clone(), session_id: binding.session_id.clone() };
        // Queue admission is inside the timeout too; a stalled writer cannot
        // strand a tool or its broker slot.
        let operation = async {
            write_line(&binding.writer, &event).await;
            request.wait(timeout, cancel).await
        };
        if cleanup {
            tokio::time::timeout(timeout, operation).await.map_err(|_| "client cleanup timed out".to_string())?
        } else {
            tokio::select! {
                biased;
                _ = self.stop.cancelled() => Err("client transport closed".into()),
                _ = cancel.cancelled() => Err("client request cancelled".into()),
                result = tokio::time::timeout(timeout, operation) => result.map_err(|_| "client request timed out".to_string())?,
            }
        }
    }
    fn timeout(&self) -> Duration { Duration::from_millis(self.config.request_timeout_ms) }
    async fn path(&self, path: &str, cancel: &CancellationToken) -> Result<String, String> {
        if path.is_empty() || path.contains('\0') || path.starts_with("file:") || path.starts_with('~') {
            return Err("client path must be a nonempty filesystem path, not a URL or home alias".into());
        }
        #[cfg(windows)]
        {
            // Reject drive-relative and root-relative paths; Windows resolves
            // these using process/drive state rather than the session cwd.
            let p = std::path::Path::new(path);
            if !p.is_absolute() && (p.has_root() || path.contains(':')) {
                return Err("client path must be absolute or relative to the session cwd".into());
            }
        }
        self.env.absolute_path(path, Some(cancel)).await.map(|p| p.to_string_lossy().into_owned()).map_err(|e| e.to_string())
    }
    async fn terminal(self: Arc<Self>, command: String, timeout: Duration, cancel: CancellationToken) -> Result<rpi_tools::env::ShellOutput, String> {
        // Do not cancel creation on caller cancellation or shutdown: the owner
        // must learn the resource ID before killing/releasing it. The bounded
        // request deadline still retires an unresponsive create. On timeout,
        // client_request_closed delegates late resource cleanup to the client.
        let create = self.request("terminal/create", json!({"command":"bash", "args":["-c",command],
            "cwd": self.env.cwd(), "outputByteLimit":51200}), &CancellationToken::new(), self.timeout(), true).await?;
        let id = create.get("terminalId").and_then(Value::as_str).filter(|s| !s.is_empty())
            .ok_or("terminal/create response requires terminalId")?.to_owned();
        let result = async {
            let exited = self.request("terminal/wait_for_exit", json!({"terminalId":id}), &cancel, timeout, false).await?;
            let output = self.request("terminal/output", json!({"terminalId":id}), &cancel, self.timeout(), false).await?;
            let text = output.get("output").and_then(Value::as_str).ok_or("terminal/output response requires output")?;
            let code = exited.get("exitCode").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok())
                .ok_or("terminal/wait_for_exit response requires integer exitCode (signal exits are errors)")?;
            let suffix = if output.get("truncated").and_then(Value::as_bool) == Some(true) { "\n[Client terminal output truncated]" } else { "" };
            Ok(rpi_tools::env::ShellOutput { stdout: format!("{text}{suffix}"), stderr: String::new(), exit_code:code })
        }.await;
        // Cleanup has independent cancellation and a short bounded deadline.
        // Always release, even when kill fails or the caller future was dropped.
        let cleanup_cancel = CancellationToken::new();
        let cleanup_timeout = self.timeout().min(Duration::from_secs(1));
        if result.is_err() || cancel.is_cancelled() || self.stop.is_cancelled() {
            let _ = self.request("terminal/kill", json!({"terminalId":id}), &cleanup_cancel, cleanup_timeout, true).await;
        }
        let released = self.request("terminal/release", json!({"terminalId":id}), &cleanup_cancel, cleanup_timeout, true).await;
        match (result, released) {
            (Ok(_), Err(error)) => Err(format!("terminal release failed: {error}")),
            (result, _) => result,
        }
    }
}

struct CloseRequest { service: Arc<ClientTools>, writer: RpcWriter, id: String, session_id: String }
impl Drop for CloseRequest {
    fn drop(&mut self) {
        let writer = self.writer.clone();
        let event = json!({"type":"client_request_closed", "requestId":self.id, "sessionId":self.session_id});
        self.service.tasks.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(1), write_line(&writer, &event)).await;
        });
    }
}

/// Newtype keeps the backend trait independent of native RPC lifecycle types.
#[derive(Clone)]
pub(crate) struct ClientBackend(pub Arc<ClientTools>);
impl std::ops::Deref for ClientBackend {
    type Target = Arc<ClientTools>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

#[async_trait]
impl ClientToolBackend for ClientBackend {
    async fn read_text(&self, path: &str, cancel: CancellationToken) -> Result<String, String> {
        if !self.config.read_text { return Err("client text read capability disabled".into()); }
        let path = self.path(path, &cancel).await?;
        let reply = self.request("fs/read_text_file", json!({"path":path}), &cancel, self.timeout(), false).await?;
        reply.get("content").and_then(Value::as_str).map(str::to_owned).ok_or("fs/read_text_file response requires string content".into())
    }
    async fn write_text(&self, path: &str, content: &str, cancel: CancellationToken) -> Result<(), String> {
        if !self.config.write_text { return Err("client text write capability disabled".into()); }
        let path = self.path(path, &cancel).await?;
        self.request("fs/write_text_file", json!({"path":path,"content":content}), &cancel, self.timeout(), false).await?;
        Ok(())
    }
    async fn exec(&self, command: &str, timeout: Option<f64>, cancel: CancellationToken) -> Result<rpi_tools::env::ShellOutput, String> {
        if !self.config.terminal { return Err("client terminal capability disabled".into()); }
        self.env.validate_shell_command(command).map_err(|e| e.to_string())?;
        let seconds = timeout.unwrap_or(120.0);
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 600.0 { return Err("terminal timeout must be in (0,600] seconds".into()); }
        if cancel.is_cancelled() || self.stop.is_cancelled() { return Err("client terminal cancelled".into()); }
        let owner_cancel = cancel.child_token();
        let _cancel_on_drop = owner_cancel.clone().drop_guard();
        let service = self.0.clone();
        let command = command.to_owned();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let scope = rpi_agent::tool_execution_scope::ToolExecutionScope::capture();
        if rpi_agent::tool_execution_scope::is_child_execution()
            && rpi_agent::tool_execution_scope::execution_session_id().is_none()
        {
            return Err("child execution lacks trusted session identity".into());
        }
        self.spawn_owner(async move {
            let result = rpi_agent::tool_execution_scope::run_captured(scope,
                service.terminal(command, Duration::from_secs_f64(seconds), owner_cancel)).await;
            let _ = tx.send(result);
        })?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("client terminal cancelled".into()),
            result = rx => result.unwrap_or_else(|_| Err("terminal owner failed".into())),
        }
    }
}

#[cfg(test)]
mod tests;
