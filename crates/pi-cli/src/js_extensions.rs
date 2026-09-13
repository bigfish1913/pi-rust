//! Minimal Pi JS/TS extension host.
//!
//! A single Node child process owns loaded extensions for the session. Rust
//! exchanges JSON-lines requests with it, keeping extension code isolated from
//! the agent process while still exposing Pi's tool and resource contracts.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::{
    AgentError, AgentTool, AgentToolResult, TextContentOrImage, ToolExecutionMode,
    ToolResultPartial,
};
use rpi_ai::types::{Context, Schema, Tool};
use rpi_ai::{CacheRetention, Model, Provider, SimpleStreamOptions};
use rpi_tui::TUI;
use tokio_util::sync::CancellationToken;

use crate::node_transport::{NodeTransport, NodeTransportStartup, RuntimeHandler};

const NODE_HOST: &str = include_str!("node_host.mjs");
static NEXT_NODE_HOST_SCRIPT: AtomicU64 = AtomicU64::new(1);
const NODE_HOST_STOPPED: &str = "Node extension host stopped";
const NODE_PREPARATION_CANCELLED: &str = "Node extension preparation cancelled";

#[derive(Debug, Clone, Default)]
pub struct JsResources {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct JsToolDefinition {
    name: String,
    label: String,
    execution_mode: ToolExecutionMode,
    tool: Tool,
}

#[derive(Clone)]
pub struct JsExtensionSession {
    transport: LazyNodeTransport,
    tools: Arc<Vec<JsToolDefinition>>,
    pub resources: JsResources,
    pub commands: Vec<String>,
    capabilities: Arc<Mutex<Vec<String>>>,
    custom_active: Arc<Mutex<Option<String>>>,
    custom_visible: Arc<Mutex<bool>>,
    custom_input_waiters: Arc<Mutex<std::collections::HashMap<u64, std_mpsc::Sender<bool>>>>,
    next_custom_input_id: Arc<AtomicU64>,
    api_version: u32,
}

impl JsExtensionSession {
    pub fn load(paths: &[PathBuf], _verbose: bool) -> Result<Option<Self>, String> {
        Self::load_with_context(paths, _verbose, serde_json::json!({}))
    }

    pub fn load_with_context(
        paths: &[PathBuf],
        _verbose: bool,
        context: serde_json::Value,
    ) -> Result<Option<Self>, String> {
        let paths: Vec<PathBuf> = paths
            .iter()
            .filter(|path| path.exists())
            .map(normalize_host_path)
            .collect();
        if paths.is_empty() {
            return Ok(None);
        }
        let (_, init) = start_node_transport(&paths, &context, true, &[])?.wait()?;
        if !init
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(init
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Node extension load failed")
                .to_string());
        }
        let result = init.get("result").cloned().unwrap_or_default();
        let mut definitions = Vec::new();
        for item in result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let label = item
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&name)
                .to_string();
            let description = item
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let parameters = item
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}}));
            let execution_mode = match item
                .get("executionMode")
                .and_then(serde_json::Value::as_str)
            {
                Some("sequential") => ToolExecutionMode::Sequential,
                _ => ToolExecutionMode::Parallel,
            };
            definitions.push(JsToolDefinition {
                name: name.clone(),
                label,
                execution_mode,
                tool: Tool {
                    name,
                    description,
                    parameters: Schema::new(parameters),
                    constrained_sampling: None,
                },
            });
        }
        let resources = parse_resources(result.get("resources"));
        let api_version = result
            .get("apiVersion")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(crate::extension_api::EXTENSION_API_VERSION as u64)
            as u32;
        let commands = result
            .get("commands")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let capabilities = result
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // The discovery host already ran the startup lifecycle hook to derive
        // the initial JS active-tool projection. Carry that projection into
        // the later persistent host so it can skip replaying the same hook
        // during module initialization. A second lifecycle invocation would
        // duplicate user-visible side effects for extensions that subscribe to
        // `before_agent_start`.
        let mut persistent_context = context;
        if let Some(active_tools) = result.get("activeTools") {
            persistent_context["activeTools"] = active_tools.clone();
        }
        let transport = LazyNodeTransport::new(paths, persistent_context);
        transport.record_active_tools(result.get("activeTools"));
        Ok(Some(Self {
            transport,
            tools: Arc::new(definitions),
            resources,
            commands,
            capabilities: Arc::new(Mutex::new(capabilities)),
            custom_active: Arc::new(Mutex::new(None)),
            custom_visible: Arc::new(Mutex::new(false)),
            custom_input_waiters: Arc::new(Mutex::new(std::collections::HashMap::new())),
            next_custom_input_id: Arc::new(AtomicU64::new(1)),
            api_version,
        }))
    }

    pub fn capabilities(&self) -> Vec<String> {
        self.capabilities
            .lock()
            .map(|values| values.clone())
            .unwrap_or_default()
    }

    pub fn tools(&self) -> impl Iterator<Item = JsToolAdapter> + '_ {
        self.tools.iter().cloned().map(|definition| JsToolAdapter {
            session: self.clone(),
            definition,
        })
    }

    fn invoke(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.transport.request(method, payload)
    }

    pub fn invoke_command(&self, command: &str, args: &str) -> Result<serde_json::Value, String> {
        self.invoke_command_with_context(command, args, serde_json::json!({}))
    }

    pub fn invoke_command_with_context(
        &self,
        command: &str,
        args: &str,
        context: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let result = self.invoke(
            "invoke_command",
            serde_json::json!({"command": command, "args": args, "context": context}),
        )?;
        self.transport
            .record_active_tools(result.get("activeTools"));
        Ok(result)
    }

    pub fn set_runtime_context(&self, context: serde_json::Value) -> Result<(), String> {
        self.transport.set_runtime_context(context)
    }

    pub fn set_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.transport.replace_runtime_handlers(handler)
    }

    pub fn add_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.transport.add_runtime_handler(handler)
    }

    /// Stop the persistent Node host even when detached command/tool work still
    /// holds a clone of this session. The transport wakes those callers, and
    /// repeated calls are harmless.
    pub fn shutdown(&self) {
        self.clear_custom_state();
        self.transport.shutdown();
    }

    fn clear_custom_state(&self) {
        if let Ok(mut active) = self.custom_active.lock() {
            *active = None;
        }
        if let Ok(mut visible) = self.custom_visible.lock() {
            *visible = false;
        }
        let waiters = self
            .custom_input_waiters
            .lock()
            .map(|mut waiters| {
                waiters
                    .drain()
                    .map(|(_, sender)| sender)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for sender in waiters {
            let _ = sender.send(false);
        }
    }

    pub fn custom_active(&self) -> bool {
        self.custom_active
            .lock()
            .map(|value| value.is_some())
            .unwrap_or(false)
    }

    /// Names of JS tools currently selected by the extension runtime. The
    /// returned list intentionally contains only the Node-side view; callers
    /// merge it with Rust built-ins before updating the harness lane.
    pub fn active_tools(&self) -> Option<Vec<String>> {
        self.transport.active_tools()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.name.clone()).collect()
    }

    /// Whether the active custom component should receive terminal input.
    /// Hidden overlay handles keep their promise alive but release keyboard
    /// focus back to the outer TUI.
    pub fn custom_accepts_input(&self) -> bool {
        self.custom_active()
            && self
                .custom_visible
                .lock()
                .map(|value| *value)
                .unwrap_or(false)
    }

    pub fn send_custom_input(&self, data: &str) -> Result<(), String> {
        let id = self
            .custom_active
            .lock()
            .map_err(|_| "Node custom state lock poisoned")?
            .clone()
            .ok_or("no active Node custom UI")?;
        // Visible custom components own the key stream. Their component
        // handler can be asynchronous, so do not wait for the hidden-overlay
        // consumption acknowledgement on every keystroke.
        self.transport.send_event(serde_json::json!({
            "type": "host_event",
            "event": "custom_input",
            "customId": id,
            "data": data,
        }))
    }

    /// Send raw input to the Node custom component and return whether a
    /// `ctx.ui.onTerminalInput` listener consumed it. Hidden overlays still
    /// need to see the event for their reopen shortcut, while unconsumed input
    /// must fall through to the outer editor.
    pub fn send_custom_input_with_consumed(&self, data: &str) -> Result<bool, String> {
        let id = self
            .custom_active
            .lock()
            .map_err(|_| "Node custom state lock poisoned")?
            .clone()
            .ok_or("no active Node custom UI")?;
        let hidden = !self.custom_accepts_input();
        let input_id = self.next_custom_input_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = std_mpsc::channel();
        self.custom_input_waiters
            .lock()
            .map_err(|_| "Node custom input lock poisoned")?
            .insert(input_id, sender);
        let event = serde_json::json!({
            "type": "host_event",
            "event": "custom_input",
            "customId": id,
            "data": data,
            "inputId": input_id,
            "hidden": hidden,
        });
        if let Err(error) = self.transport.send_event(event) {
            if let Ok(mut waiters) = self.custom_input_waiters.lock() {
                waiters.remove(&input_id);
            }
            return Err(error);
        }
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(consumed) => Ok(consumed),
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut waiters) = self.custom_input_waiters.lock() {
                    waiters.remove(&input_id);
                }
                // A crashed or older host must not permanently lock the outer
                // editor. Treat a missing acknowledgement as unconsumed.
                Ok(false)
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                if let Ok(mut waiters) = self.custom_input_waiters.lock() {
                    waiters.remove(&input_id);
                }
                Ok(false)
            }
        }
    }

    pub fn send_custom_resize(&self, width: usize, height: usize) -> Result<(), String> {
        let id = self
            .custom_active
            .lock()
            .map_err(|_| "Node custom state lock poisoned")?
            .clone()
            .ok_or("no active Node custom UI")?;
        let event = serde_json::json!({
            "type": "host_event",
            "event": "custom_resize",
            "customId": id,
            "width": width,
            "height": height,
        });
        self.transport.send_event(event)
    }

    pub fn install_ui_runtime(&self, tui: Arc<rpi_tui::TuiAltScreen>) -> Result<(), String> {
        let active = self.custom_active.clone();
        let visible = self.custom_visible.clone();
        let input_waiters = self.custom_input_waiters.clone();
        self.add_runtime_handler(Arc::new(move |action, args| match action {
            "ui.custom.open" => {
                let id = args
                    .get("customId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.custom.open missing customId")?
                    .to_string();
                *visible
                    .lock()
                    .map_err(|_| "Node custom visibility lock poisoned")? = true;
                *active
                    .lock()
                    .map_err(|_| "Node custom state lock poisoned")? = Some(id.clone());
                tui.set_render_suspended(true);
                tui.terminal().clear_screen();
                let info = tui.terminal().info();
                Ok(serde_json::json!({
                    "columns": info.columns,
                    "rows": info.rows,
                }))
            }
            "ui.custom.write" => {
                let data = args
                    .get("data")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.custom.write missing data")?;
                tui.terminal().write(data);
                Ok(serde_json::json!(true))
            }
            "ui.custom.invalidate" => {
                tui.request_render(false);
                Ok(serde_json::json!(true))
            }
            "ui.custom.input" => {
                let input_id = args
                    .get("inputId")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("ui.custom.input missing inputId")?;
                let consumed = args
                    .get("consumed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if let Ok(mut waiters) = input_waiters.lock() {
                    if let Some(sender) = waiters.remove(&input_id) {
                        let _ = sender.send(consumed);
                    }
                }
                Ok(serde_json::json!(consumed))
            }
            "ui.custom.handle" => {
                let operation = args
                    .get("operation")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.custom.handle missing operation")?;
                let id = args
                    .get("customId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.custom.handle missing customId")?;
                let is_active = active
                    .lock()
                    .map_err(|_| "Node custom state lock poisoned")?
                    .as_deref()
                    == Some(id);
                if !is_active {
                    return Ok(serde_json::json!(false));
                }
                match operation {
                    "setHidden" | "hide" => {
                        let hidden = args
                            .get("hidden")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(operation == "hide");
                        tui.terminal().clear_screen();
                        // A hidden custom overlay gives the outer TUI its
                        // terminal back; showing it suspends the outer repaint
                        // again so Node owns the screen.
                        tui.set_render_suspended(!hidden);
                        *visible
                            .lock()
                            .map_err(|_| "Node custom visibility lock poisoned")? = !hidden;
                        if !hidden {
                            tui.terminal().clear_screen();
                        }
                    }
                    // Focus state is maintained by the Node-side handle. The
                    // Rust host only needs to repaint when visibility changes.
                    "focus" | "unfocus" => {}
                    _ => return Err(format!("unsupported custom handle operation: {operation}")),
                }
                Ok(serde_json::json!(true))
            }
            "ui.custom.close" => {
                let restore_custom_id = args
                    .get("restoreCustomId")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                let restore_hidden = args
                    .get("restoreHidden")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                *visible
                    .lock()
                    .map_err(|_| "Node custom visibility lock poisoned")? =
                    restore_custom_id.is_some() && !restore_hidden;
                *active
                    .lock()
                    .map_err(|_| "Node custom state lock poisoned")? = restore_custom_id.clone();
                // Each custom open clears the terminal. Clear again before a
                // parent is restored so the child's frame cannot remain
                // behind the parent's first render.
                tui.terminal().clear_screen();
                // A nested custom UI closes back into its parent custom UI.
                // Keep the outer TUI suspended until the whole custom stack is
                // gone; otherwise the editor renderer races the restored
                // component and overwrites its terminal output.
                if restore_custom_id.is_none() || restore_hidden {
                    tui.set_render_suspended(false);
                }
                Ok(serde_json::json!(true))
            }
            "ui.custom.resize" => Ok(serde_json::json!(true)),
            _ => Err(format!("unsupported capability: {action}")),
        }))?;
        self.enable_capability("ui.custom")?;
        self.set_runtime_context(serde_json::json!({
            "mode": "tui",
            "hasUI": true,
            "capabilities":["ui.custom"]
        }))
    }

    /// Start the lazy Node host after all interactive runtime handlers are
    /// installed and the TUI input worker is running. Startup executes the
    /// `before_agent_start` lifecycle hook, which may wait on a native dialog;
    /// callers must therefore invoke this only once the dialog bridge can
    /// service pending requests.
    pub fn ensure_runtime(&self) -> Result<(), String> {
        self.transport.ensure().map(|_| ())
    }

    /// Prepare the Node runtime for one agent prompt. Starting a fresh lazy
    /// host already runs `before_agent_start` during its context handoff;
    /// prompts that reuse a live host send one context snapshot so the same
    /// lifecycle runs exactly once again for that prompt.
    pub fn prepare_for_prompt(&self) -> Result<(), String> {
        self.prepare_for_prompt_with_cancellation(&CancellationToken::new())
    }

    pub(crate) fn prepare_for_prompt_with_cancellation(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        if cancellation.is_cancelled() {
            return Err(NODE_PREPARATION_CANCELLED.into());
        }
        let already_live = self.transport.live_transport()?.is_some();
        self.transport
            .ensure_with_cancellation(Some(cancellation))?;
        if cancellation.is_cancelled() {
            return Err(NODE_PREPARATION_CANCELLED.into());
        }
        if already_live {
            self.transport.set_runtime_context(serde_json::json!({}))?;
        }
        if cancellation.is_cancelled() {
            return Err(NODE_PREPARATION_CANCELLED.into());
        }
        Ok(())
    }

    /// Interrupt only the host involved in prompt preparation. Unlike
    /// [`Self::shutdown`], this leaves the lazy session reusable so a later
    /// prompt can start a fresh host after the user aborts one stuck hook.
    pub(crate) fn cancel_prompt_preparation(&self) {
        self.clear_custom_state();
        self.transport.stop_current();
    }

    /// Install the host side of the small dialog protocol used by
    /// `ctx.ui.select/confirm/input/editor` in the Node runtime.
    ///
    /// The handler is deliberately supplied by the interactive TUI: opening a
    /// dialog needs to swap/focus a native component and then wait for a key
    /// press, while this module only owns the Node transport. It receives both
    /// `ui.dialog` and `ui.dialog.cancel` actions and must return one of the
    /// JSON result shapes documented in `node_host.mjs` (`{value}`,
    /// `{confirmed}`, or `{cancelled:true}`).
    pub fn install_ui_dialog_runtime(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.add_runtime_handler(handler)?;
        for capability in ["ui.select", "ui.confirm", "ui.input", "ui.editor_dialog"] {
            self.enable_capability(capability)?;
        }
        self.set_runtime_context(serde_json::json!({
            "mode": "tui",
            "hasUI": true,
            "capabilities": ["ui.select", "ui.confirm", "ui.input", "ui.editor_dialog"],
        }))
    }

    pub fn enable_capability(&self, capability: &str) -> Result<(), String> {
        let mut capabilities = self
            .capabilities
            .lock()
            .map_err(|_| "Node capability lock poisoned")?;
        if !capabilities.iter().any(|value| value == capability) {
            capabilities.push(capability.to_string());
        }
        Ok(())
    }

    /// Install the Rust provider as the Node Pi model registry's runtime
    /// implementation. Node receives a complete AssistantMessage; Rust remains
    /// the only owner of HTTP, authentication, retries, and provider details.
    pub fn enable_provider_runtime(
        &self,
        provider: Arc<dyn Provider>,
        runtime: tokio::runtime::Handle,
    ) -> Result<(), String> {
        let provider_id = provider.id().to_string();
        self.set_runtime_handler(Arc::new(move |action, args| {
            if !matches!(action, "provider.complete" | "provider.stream") {
                return Err(format!("unsupported capability: {action}"));
            }
            let model: Model = serde_json::from_value(
                args.get("model")
                    .cloned()
                    .ok_or("provider.complete missing model")?,
            )
            .map_err(|error| format!("invalid provider model: {error}"))?;
            if model.provider != provider_id {
                return Err(format!(
                    "no provider registered for model provider '{}'",
                    model.provider
                ));
            }
            let context = normalize_provider_context(
                args.get("context")
                    .cloned()
                    .ok_or("provider.complete missing context")?,
            )
            .map_err(|error| format!("invalid provider context: {error}"))?;
            let options = parse_simple_stream_options(args.get("options"))?;
            let stream = runtime.block_on(provider.stream_simple(&model, &context, &options));
            if action == "provider.complete" {
                let message = runtime
                    .block_on(stream.result())
                    .map_err(|error| format!("provider stream failed: {error}"))?;
                return serde_json::to_value(message)
                    .map_err(|error| format!("provider result encode failed: {error}"));
            }

            // The JSON-lines transport is request/response based, so the Node
            // side receives a complete event snapshot. It still exposes the
            // same async-iterator + result() shape as Pi's EventStream, while
            // Rust remains the owner of provider execution and wire details.
            let (mut events_rx, result_rx) = stream.split();
            let (events, message) = runtime.block_on(async move {
                let mut events = Vec::new();
                while let Some(event) = events_rx.recv().await {
                    events.push(
                        serde_json::to_value(event)
                            .map_err(|error| format!("provider event encode failed: {error}"))?,
                    );
                }
                let message = result_rx
                    .await
                    .map_err(|error| format!("provider stream failed: {error}"))?;
                Ok::<_, String>((events, message))
            })?;
            Ok(serde_json::json!({
                "events": events,
                "result": serde_json::to_value(message)
                    .map_err(|error| format!("provider result encode failed: {error}"))?,
            }))
        }))?;
        self.enable_capability("provider_calls")?;
        self.set_runtime_context(serde_json::json!({
            "capabilities": ["provider_calls"],
        }))
    }
}

/// Holds registrations discovered at startup while deferring the persistent
/// Node runtime until the first request needs it.
struct LazyNodeTransport {
    /// A separate shared owner count lets the final clone perform cleanup
    /// without shutting down the host when a short-lived tool/command clone is
    /// dropped. This is explicit rather than derived from `Arc::strong_count`:
    /// two clones can be dropped concurrently, and both would otherwise see
    /// a count greater than one before either `Drop` releases its Arc.
    owner: Arc<AtomicUsize>,
    paths: Arc<Vec<PathBuf>>,
    context: Arc<serde_json::Value>,
    transport: Arc<Mutex<Option<NodeTransport>>>,
    handlers: Arc<Mutex<Vec<RuntimeHandler>>>,
    pending_context: Arc<Mutex<Option<serde_json::Value>>>,
    context_revision: Arc<AtomicU64>,
    active_tools: Arc<Mutex<Option<Vec<String>>>>,
    shutdown: Arc<AtomicBool>,
    // `ensure` serializes startup with this lock while keeping the transport
    // slot free for shutdown to take a live instance out concurrently.
    start_lock: Arc<Mutex<()>>,
    // A startup transport is published here before any potentially blocking
    // initialization request (`set_runtime_context`) is sent.
    starting: Arc<Mutex<Option<NodeTransport>>>,
    // Serializes context merging with the final startup handoff. It is never
    // held while waiting for a Node response.
    context_lock: Arc<Mutex<()>>,
}

impl Clone for LazyNodeTransport {
    fn clone(&self) -> Self {
        self.owner.fetch_add(1, Ordering::Relaxed);
        Self {
            owner: self.owner.clone(),
            paths: self.paths.clone(),
            context: self.context.clone(),
            transport: self.transport.clone(),
            handlers: self.handlers.clone(),
            pending_context: self.pending_context.clone(),
            context_revision: self.context_revision.clone(),
            active_tools: self.active_tools.clone(),
            shutdown: self.shutdown.clone(),
            start_lock: self.start_lock.clone(),
            starting: self.starting.clone(),
            context_lock: self.context_lock.clone(),
        }
    }
}

impl Drop for LazyNodeTransport {
    fn drop(&mut self) {
        // `LazyNodeTransport` is cloned into tool adapters and detached
        // command workers. Keep the host alive while any of those owners
        // remain, then stop it when the final lazy owner disappears.
        if self.owner.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shutdown();
        }
    }
}

impl LazyNodeTransport {
    fn new(paths: Vec<PathBuf>, context: serde_json::Value) -> Self {
        Self {
            owner: Arc::new(AtomicUsize::new(1)),
            paths: Arc::new(paths),
            context: Arc::new(context),
            transport: Arc::new(Mutex::new(None)),
            handlers: Arc::new(Mutex::new(Vec::new())),
            pending_context: Arc::new(Mutex::new(None)),
            context_revision: Arc::new(AtomicU64::new(0)),
            active_tools: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(AtomicBool::new(false)),
            start_lock: Arc::new(Mutex::new(())),
            starting: Arc::new(Mutex::new(None)),
            context_lock: Arc::new(Mutex::new(())),
        }
    }

    fn live_transport(&self) -> Result<Option<NodeTransport>, String> {
        let mut slot = self
            .transport
            .lock()
            .map_err(|_| "Node transport lock poisoned")?;
        match slot.as_ref() {
            Some(transport) if !transport.is_shutdown() => Ok(Some(transport.clone())),
            Some(_) => {
                // The reader has terminated (or a write failed). Drop the
                // stale slot so the next request can create a fresh host.
                slot.take();
                Ok(None)
            }
            None => Ok(None),
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.stop_current();
    }

    /// Stop the currently active or starting child without closing the lazy
    /// session. Prompt preparation uses this for user cancellation so the
    /// next prompt can retry with a fresh process.
    fn stop_current(&self) {
        // Take the transport out before calling into it so no lazy-state lock
        // is held while the child is being terminated.
        let active = self.transport.lock().ok().and_then(|mut slot| slot.take());
        let starting = self.starting.lock().ok().and_then(|mut slot| slot.take());
        if let Some(active) = active.as_ref() {
            active.shutdown();
        }
        if let Some(starting) = starting {
            if active
                .as_ref()
                .map_or(true, |active| !active.same_instance(&starting))
            {
                starting.shutdown();
            }
        }
    }

    fn active_tools(&self) -> Option<Vec<String>> {
        self.active_tools
            .lock()
            .ok()
            .and_then(|value| value.clone())
    }

    fn record_active_tools(&self, value: Option<&serde_json::Value>) {
        let Some(values) = value.and_then(serde_json::Value::as_array) else {
            return;
        };
        let names = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        if let Ok(mut active) = self.active_tools.lock() {
            *active = Some(names);
        }
    }

    /// Put a context snapshot back into the pre-start queue after a failed
    /// startup. `ensure` temporarily removes the snapshot so the Node host can
    /// receive it in its environment; if that host exits before becoming the
    /// active transport, the next attempt still needs the same state. Updates
    /// that arrived concurrently are merged afterwards, preserving their
    /// newer scalar values and additive capabilities.
    fn restore_pending_context(&self, restored: Option<serde_json::Value>) {
        let Some(mut restored) = restored else {
            return;
        };
        let Ok(_context_guard) = self.context_lock.lock() else {
            return;
        };
        let Ok(mut pending) = self.pending_context.lock() else {
            return;
        };
        if let Some(current) = pending.take() {
            merge_runtime_context(&mut restored, &current);
        }
        *pending = Some(restored);
    }

    fn ensure(&self) -> Result<NodeTransport, String> {
        self.ensure_with_cancellation(None)
    }

    fn ensure_with_cancellation(
        &self,
        cancellation: Option<&CancellationToken>,
    ) -> Result<NodeTransport, String> {
        let cancelled = || cancellation.is_some_and(CancellationToken::is_cancelled);
        if cancelled() {
            return Err(NODE_PREPARATION_CANCELLED.into());
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(NODE_HOST_STOPPED.into());
        }
        if let Some(transport) = self.live_transport()? {
            return Ok(transport);
        }

        // Keep only startup serialization under this lock. The transport slot
        // itself is intentionally not held while Node starts or handles an
        // initialization callback, so shutdown can terminate `starting`.
        let _start_guard = self
            .start_lock
            .lock()
            .map_err(|_| "Node startup lock poisoned")?;
        if cancelled() {
            return Err(NODE_PREPARATION_CANCELLED.into());
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(NODE_HOST_STOPPED.into());
        }
        if let Some(transport) = self.live_transport()? {
            return Ok(transport);
        }
        let initial_handlers = self
            .handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")?
            .clone();
        // Include all context updates queued before the first request in the
        // host's initial environment. This makes the first lifecycle hook see
        // the real TUI/session capabilities instead of a stale headless base
        // context; updates that arrive while startup is in flight are drained
        // below after initialization.
        let (initial_context, initial_pending, initial_revision) = {
            let _context_guard = self
                .context_lock
                .lock()
                .map_err(|_| "Node runtime context lock poisoned")?;
            let mut context = (*self.context).clone();
            let mut pending = self
                .pending_context
                .lock()
                .map_err(|_| "Node runtime context lock poisoned")?;
            let initial_pending = pending.take();
            let initial_revision = self.context_revision.load(Ordering::Acquire);
            if let Some(next) = initial_pending.as_ref() {
                merge_runtime_context(&mut context, next);
            }
            (context, initial_pending, initial_revision)
        };

        // Spawn first and publish the transport before waiting for `id:0`.
        // Extension factories and `before_agent_start` are user code and may
        // block indefinitely; exposing this handle makes concurrent shutdown
        // able to terminate the child during that phase.
        let startup =
            match start_node_transport(&self.paths, &initial_context, false, &initial_handlers) {
                Ok(startup) => startup,
                Err(error) => {
                    self.restore_pending_context(initial_pending.clone());
                    return Err(error);
                }
            };
        let transport = startup.transport();

        // Cancellation can arrive after the worker entered `ensure` but
        // before the child was available through either shared slot.
        if cancelled() {
            transport.shutdown();
            self.restore_pending_context(initial_pending.clone());
            return Err(NODE_PREPARATION_CANCELLED.into());
        }

        {
            let mut starting = self
                .starting
                .lock()
                .map_err(|_| "Node startup state lock poisoned")?;
            if self.shutdown.load(Ordering::Acquire) || cancelled() {
                drop(starting);
                transport.shutdown();
                self.restore_pending_context(initial_pending.clone());
                return Err(if cancelled() {
                    NODE_PREPARATION_CANCELLED.into()
                } else {
                    NODE_HOST_STOPPED.into()
                });
            }
            *starting = Some(transport.clone());
            // Close the publication race with `stop_current`: a cancellation
            // that happened while this lock was held must still tear down the
            // child before we wait for its initialization envelope.
            if cancelled() {
                let stopped = starting.take();
                drop(starting);
                if let Some(stopped) = stopped {
                    stopped.shutdown();
                }
                self.restore_pending_context(initial_pending.clone());
                return Err(NODE_PREPARATION_CANCELLED.into());
            }
        }

        let init = match startup.wait() {
            Ok((_, init)) => init,
            Err(error) => {
                if let Ok(mut starting) = self.starting.lock() {
                    starting.take();
                }
                self.restore_pending_context(initial_pending.clone());
                return Err(error);
            }
        };

        let startup_result = (|| -> Result<NodeTransport, String> {
            if cancelled() {
                return Err(NODE_PREPARATION_CANCELLED.into());
            }
            if !init
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return Err(init
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Node extension load failed")
                    .to_string());
            }
            self.record_active_tools(
                init.get("result")
                    .and_then(|value| value.get("activeTools")),
            );
            let handlers = self
                .handlers
                .lock()
                .map_err(|_| "Node runtime handler lock poisoned")?
                .clone();
            if let Some(first) = handlers.first() {
                transport.replace_runtime_handlers(first.clone())?;
                for handler in handlers.iter().skip(1).cloned() {
                    transport.add_runtime_handler(handler)?;
                }
            }
            // Keep a replayable snapshot of every context delta sent during
            // startup. If a request fails, the host is discarded and the next
            // attempt must receive all of these updates again.
            let mut startup_context = initial_pending
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            // The initial context is also placed in the host environment so
            // factories can inspect it before the init envelope. Keep one
            // replay of the queued delta for the post-init handoff: the
            // persistent host skips its duplicate startup lifecycle hook, so
            // `set_runtime_context` must still be delivered once to run the
            // hook against the real runtime/UI context.
            // The persistent host deliberately skips its module-init lifecycle
            // pass (the discovery host already ran it). Always replay one
            // context envelope, even when no caller queued a delta, so the
            // persistent process still executes `before_agent_start` once for
            // its own extension state. An empty object is sufficient here:
            // the full initial context was supplied through the environment.
            let mut startup_pending = Some((
                initial_pending
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
                initial_revision,
            ));
            // Drain context updates that arrived while Node was starting. The
            // final empty check and active-slot handoff share `context_lock`,
            // so a concurrent setter cannot enqueue a value behind the check.
            loop {
                if cancelled() {
                    return Err(NODE_PREPARATION_CANCELLED.into());
                }
                let next = if let Some(next) = startup_pending.take() {
                    Some(next)
                } else {
                    let _context_guard = self
                        .context_lock
                        .lock()
                        .map_err(|_| "Node runtime context lock poisoned")?;
                    let mut pending = self
                        .pending_context
                        .lock()
                        .map_err(|_| "Node runtime context lock poisoned")?;
                    if let Some(context) = pending.take() {
                        let revision = self.context_revision.load(Ordering::Acquire);
                        Some((context, revision))
                    } else {
                        // Retain the latest merged delta after a successful
                        // handoff. A live host can exit later without another
                        // context setter, and its replacement must still see
                        // the session/UI state applied during this startup.
                        *pending = Some(startup_context.clone());
                        let mut slot = self
                            .transport
                            .lock()
                            .map_err(|_| "Node transport lock poisoned")?;
                        if self.shutdown.load(Ordering::Acquire) || cancelled() {
                            return Err(if cancelled() {
                                NODE_PREPARATION_CANCELLED.into()
                            } else {
                                NODE_HOST_STOPPED.into()
                            });
                        }
                        if let Some(existing) = slot.as_ref() {
                            return Ok(existing.clone());
                        }
                        *slot = Some(transport.clone());
                        // `shutdown()` can set the flag and drain the slots
                        // concurrently. Re-check after publishing so a
                        // shutdown that won the race cannot leave a stopped
                        // transport installed for the next caller.
                        if self.shutdown.load(Ordering::Acquire) || cancelled() {
                            let stopped = slot.take();
                            drop(slot);
                            if let Some(stopped) = stopped {
                                stopped.shutdown();
                            }
                            return Err(if cancelled() {
                                NODE_PREPARATION_CANCELLED.into()
                            } else {
                                NODE_HOST_STOPPED.into()
                            });
                        }
                        return Ok(transport.clone());
                    }
                };
                let Some((context, revision)) = next else {
                    continue;
                };
                if cancelled() {
                    return Err(NODE_PREPARATION_CANCELLED.into());
                }
                merge_runtime_context(&mut startup_context, &context);
                let result = match transport.request(
                    "set_runtime_context",
                    serde_json::json!({"context": context, "revision": revision}),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        // The `?` form would discard the context deltas that
                        // were already sent successfully in this startup.
                        // Requeue the complete replay snapshot before letting
                        // the outer error path tear down the host.
                        self.restore_pending_context(Some(startup_context));
                        return Err(error);
                    }
                };
                self.record_active_tools(result.get("activeTools"));
            }
        })();

        // Only one ensure holds start_lock, so this slot can contain at most
        // the transport created above. Shutdown may already have taken it.
        if let Ok(mut starting) = self.starting.lock() {
            starting.take();
        }
        match startup_result {
            Ok(transport) => Ok(transport),
            Err(error) => {
                transport.shutdown();
                self.restore_pending_context(initial_pending);
                Err(error)
            }
        }
    }

    fn request(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.ensure()?.request(method, payload)
    }

    fn begin_request(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<crate::node_transport::PendingRequest, String> {
        self.ensure()?.begin_request(method, payload)
    }

    fn send_event(&self, event: serde_json::Value) -> Result<(), String> {
        self.ensure()?.send_event(event)
    }

    fn cancel(&self, id: u64) -> Result<(), String> {
        self.ensure()?.cancel(id)
    }

    fn register_tool_update_handler(
        &self,
        tool_call_id: impl Into<String>,
        handler: crate::node_transport::ToolUpdateHandler,
    ) -> Result<(), String> {
        self.ensure()?
            .register_tool_update_handler(tool_call_id, handler)
    }

    fn unregister_tool_update_handler(&self, tool_call_id: &str) {
        if let Ok(slot) = self.transport.lock() {
            if let Some(transport) = slot.as_ref() {
                transport.unregister_tool_update_handler(tool_call_id);
            }
        }
    }

    /// Return every currently spawned host that can receive runtime requests.
    /// During lazy startup the same child is exposed through `starting` until
    /// the initialization/context handoff completes, so deduplicate handles by
    /// child identity before mutating its handler list.
    fn handler_update_transports(&self) -> Result<Vec<NodeTransport>, String> {
        let active = self.live_transport()?;
        let starting = self
            .starting
            .lock()
            .map_err(|_| "Node startup state lock poisoned")?
            .as_ref()
            .filter(|transport| !transport.is_shutdown())
            .cloned();
        let mut transports = Vec::with_capacity(2);
        if let Some(active) = active {
            transports.push(active);
        }
        if let Some(starting) = starting {
            if !transports
                .iter()
                .any(|transport| transport.same_instance(&starting))
            {
                transports.push(starting);
            }
        }
        Ok(transports)
    }

    fn replace_runtime_handlers(&self, handler: RuntimeHandler) -> Result<(), String> {
        *self
            .handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")? = vec![handler.clone()];
        for transport in self.handler_update_transports()? {
            transport.replace_runtime_handlers(handler.clone())?;
        }
        Ok(())
    }

    fn add_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")?
            .push(handler.clone());
        for transport in self.handler_update_transports()? {
            transport.add_runtime_handler(handler.clone())?;
        }
        Ok(())
    }

    fn set_runtime_context(&self, context: serde_json::Value) -> Result<(), String> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(NODE_HOST_STOPPED.into());
        }
        // Serialize only the merge/snapshot step.  The Node request can invoke
        // a Rust runtime handler (and therefore re-enter this method), so
        // keeping `context_lock` across the blocking request would deadlock.
        let (merged, revision, transport) = {
            let _context_guard = self
                .context_lock
                .lock()
                .map_err(|_| "Node runtime context lock poisoned")?;
            let mut pending = self
                .pending_context
                .lock()
                .map_err(|_| "Node runtime context lock poisoned")?;
            let mut merged = pending.take().unwrap_or_else(|| serde_json::json!({}));
            merge_runtime_context(&mut merged, &context);
            *pending = Some(merged.clone());
            let revision = self.context_revision.fetch_add(1, Ordering::AcqRel) + 1;
            // Do not reuse a transport whose reader already reached EOF or
            // whose stdin write failed. Keep the merged context queued so the
            // next real request can start a fresh host and apply it during the
            // startup handoff.
            let transport = self.live_transport()?;
            (merged, revision, transport)
        };
        if let Some(transport) = transport {
            let result = transport.request(
                "set_runtime_context",
                serde_json::json!({"context": merged, "revision": revision}),
            )?;
            self.record_active_tools(result.get("activeTools"));
        }
        Ok(())
    }
}

/// Merge runtime context updates that are queued before the lazy Node process
/// starts. Capability updates are additive; ordinary context fields use the
/// newest value. Without this, installing two independent bridges before the
/// first command (for example `ui.custom` and `ui.dialog`) drops the first
/// bridge's capability from the initial Node context.
fn merge_runtime_context(base: &mut serde_json::Value, next: &serde_json::Value) {
    let (Some(base_object), Some(next_object)) = (base.as_object_mut(), next.as_object()) else {
        *base = next.clone();
        return;
    };
    for (key, value) in next_object {
        if key == "capabilities" {
            let Some(next_values) = value.as_array() else {
                base_object.insert(key.clone(), value.clone());
                continue;
            };
            let entry = base_object
                .entry(key.clone())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            let Some(base_values) = entry.as_array_mut() else {
                *entry = serde_json::Value::Array(Vec::new());
                let Some(base_values) = entry.as_array_mut() else {
                    continue;
                };
                for item in next_values {
                    if !base_values.contains(item) {
                        base_values.push(item.clone());
                    }
                }
                continue;
            };
            for item in next_values {
                if !base_values.contains(item) {
                    base_values.push(item.clone());
                }
            }
        } else {
            base_object.insert(key.clone(), value.clone());
        }
    }
}

/// Accept Pi-compatible response objects that omit the discriminating `role`
/// field when a side thread feeds a previous response back into `Context`.
/// Only infer roles from unambiguous shape markers; malformed/ambiguous
/// messages still fail typed deserialization with the original error.
fn normalize_provider_context(mut value: serde_json::Value) -> Result<Context, String> {
    let Some(messages) = value
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return serde_json::from_value(value).map_err(|error| error.to_string());
    };
    for message in messages {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if object.contains_key("role") {
            continue;
        }
        let inferred = if object.contains_key("toolCallId")
            || object.contains_key("toolName")
            || object.contains_key("isError")
        {
            Some("toolResult")
        } else if object.contains_key("stopReason")
            && object.contains_key("content")
            && (object.contains_key("provider") || object.contains_key("model"))
        {
            Some("assistant")
        } else {
            None
        };
        if let Some(role) = inferred {
            object.insert(
                "role".to_string(),
                serde_json::Value::String(role.to_string()),
            );
        }
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn parse_simple_stream_options(
    value: Option<&serde_json::Value>,
) -> Result<SimpleStreamOptions, String> {
    let value = value.cloned().unwrap_or_default();
    let mut options = SimpleStreamOptions::default();
    options.api_key = value
        .get("apiKey")
        .and_then(|v| v.as_str())
        .map(String::from);
    options.headers = value
        .get("headers")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid headers: {e}"))?;
    options.metadata = value
        .get("metadata")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid metadata: {e}"))?;
    options.timeout = value
        .get("timeout")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    options.max_retries = value
        .get("maxRetries")
        .and_then(|v| v.as_u64())
        .map(|value| value as u32);
    options.max_retry_delay = value
        .get("maxRetryDelay")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    options.session_id = value
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(String::from);
    options.max_tokens = value.get("maxTokens").and_then(|v| v.as_u64());
    options.temperature = value.get("temperature").and_then(|v| v.as_f64());
    options.reasoning = value
        .get("reasoning")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid reasoning: {e}"))?;
    options.thinking_budgets = value
        .get("thinkingBudgets")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid thinkingBudgets: {e}"))?;
    options.cache_retention = match value.get("cacheRetention").and_then(|v| v.as_str()) {
        Some("long") => CacheRetention::Long,
        Some("none") => CacheRetention::None,
        _ => CacheRetention::Short,
    };
    Ok(options)
}

impl crate::extension_api::ExtensionBackend for JsExtensionSession {
    fn backend_info(&self) -> crate::extension_api::ExtensionBackendInfo {
        use crate::extension_api::{ExtensionBackendInfo, ExtensionCapability};

        let mut info = ExtensionBackendInfo::new("node");
        info.api_version = self.api_version;
        for capability in self
            .capabilities()
            .iter()
            .filter_map(|name| match name.as_str() {
                "tools" => Some(ExtensionCapability::Tools),
                "commands" => Some(ExtensionCapability::Commands),
                "resources" => Some(ExtensionCapability::Resources),
                "ui.notify" => Some(ExtensionCapability::UiNotify),
                "ui.editor" => Some(ExtensionCapability::UiEditor),
                "ui.custom" => Some(ExtensionCapability::UiCustom),
                "ui.select" => Some(ExtensionCapability::UiSelect),
                "ui.confirm" => Some(ExtensionCapability::UiConfirm),
                "ui.input" => Some(ExtensionCapability::UiInput),
                "ui.editor_dialog" => Some(ExtensionCapability::UiEditorDialog),
                "session" => Some(ExtensionCapability::Session),
                "models" => Some(ExtensionCapability::Models),
                "events" => Some(ExtensionCapability::Events),
                "providers" => Some(ExtensionCapability::Providers),
                "provider_calls" => Some(ExtensionCapability::ProviderCalls),
                "renderers" => Some(ExtensionCapability::Renderers),
                "runtime_actions" => Some(ExtensionCapability::RuntimeActions),
                _ => None,
            })
        {
            info.capabilities.insert(capability);
        }
        info
    }
}

#[derive(Clone)]
pub struct JsToolAdapter {
    session: JsExtensionSession,
    definition: JsToolDefinition,
}

#[async_trait]
impl AgentTool for JsToolAdapter {
    fn schema(&self) -> &Tool {
        &self.definition.tool
    }
    fn label(&self) -> &str {
        &self.definition.label
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.definition.execution_mode
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let name = self.definition.name.clone();
        let tool_call_id = tool_call_id.to_string();
        let transport = self.session.transport.clone();
        let update_handler: Arc<dyn Fn(serde_json::Value) + Send + Sync> = {
            let on_update = on_update.clone();
            Arc::new(move |value| {
                // Partial updates are best effort, just like the native Pi
                // callback. A malformed update must not fail the main tool
                // invocation or poison the transport reader thread.
                if let Ok(partial) = parse_tool_result(value) {
                    on_update(partial);
                }
            })
        };
        transport
            .register_tool_update_handler(tool_call_id.clone(), update_handler)
            .map_err(AgentError::tool)?;
        let pending = match transport.begin_request(
            "invoke_tool",
            serde_json::json!({"tool": name, "toolCallId": tool_call_id.clone(), "args": params}),
        ) {
            Ok(pending) => pending,
            Err(error) => {
                transport.unregister_tool_update_handler(&tool_call_id);
                return Err(AgentError::tool(error));
            }
        };
        let request_id = pending.id();
        let cancel_transport = transport.clone();
        let wait = tokio::task::spawn_blocking(move || pending.wait());
        let result = tokio::select! {
            result = wait => result
                .map_err(|error| AgentError::tool(error.to_string()))
                .and_then(|response| response.map_err(AgentError::tool)),
            _ = signal.cancelled() => {
                let _ = cancel_transport.cancel(request_id);
                Err(AgentError::Abort)
            }
        };
        transport.unregister_tool_update_handler(&tool_call_id);
        let result = result?;
        parse_tool_result(result).map_err(AgentError::tool)
    }
}

fn start_node_transport(
    paths: &[PathBuf],
    context: &serde_json::Value,
    discovery_only: bool,
    initial_handlers: &[RuntimeHandler],
) -> Result<NodeTransportStartup, String> {
    let node = which_node()?;
    // `node -e <embedded host>` exceeds the Windows CreateProcess command
    // line limit once this host grows past roughly 8 KiB. Keep the eval path
    // on Unix (where it preserves the existing import.meta resolution), and
    // launch a short-lived temp module on Windows.
    let host_script = if cfg!(windows) {
        Some(write_node_host_script()?)
    } else {
        None
    };
    let mut command = Command::new(node);
    if let Some(path) = host_script.as_ref() {
        command.arg(path);
    } else {
        command.args(["--input-type=module", "-e", NODE_HOST]);
    }
    command
        .env(
            "RPI_JS_EXTENSION_PATHS",
            serde_json::to_string(paths).unwrap_or_else(|_| "[]".into()),
        )
        .env(
            "RPI_JS_EXTENSION_CONTEXT",
            serde_json::to_string(context).unwrap_or_else(|_| "{}".into()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Extension diagnostics belong on stderr; keeping it inherited
        // prevents an undrained pipe from blocking a noisy extension.
        .stderr(Stdio::inherit());
    if discovery_only {
        command.env("RPI_JS_EXTENSION_ONESHOT", "1");
    } else {
        command
            .env("RPI_JS_EXTENSION_SKIP_INITIAL_HOOK", "1")
            .env("RPI_JS_EXTENSION_SKIP_INITIAL_DISCOVERY", "1");
    }
    let mut child: Child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            remove_node_host_script(host_script.as_ref());
            return Err(format!("could not start Node JS extension host: {error}"));
        }
    };
    let stdin: ChildStdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            remove_node_host_script(host_script.as_ref());
            return Err("Node extension host stdin unavailable".into());
        }
    };
    let stdout: ChildStdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            remove_node_host_script(host_script.as_ref());
            return Err("Node extension host stdout unavailable".into());
        }
    };
    let result = NodeTransport::start_pending_with_cleanup_and_handlers(
        child,
        stdin,
        stdout,
        host_script.clone(),
        initial_handlers.to_vec(),
    );
    if result.is_err() {
        remove_node_host_script(host_script.as_ref());
    }
    result
}

fn write_node_host_script() -> Result<PathBuf, String> {
    let id = NEXT_NODE_HOST_SCRIPT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rpi-node-host-{}-{id}.mjs", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("could not create Node host script: {error}"))?;
    if let Err(error) = file.write_all(NODE_HOST.as_bytes()) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("could not write Node host script: {error}"));
    }
    Ok(path)
}

fn remove_node_host_script(path: Option<&PathBuf>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

fn which_node() -> Result<String, String> {
    for candidate in ["node", "nodejs"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok(candidate.to_string());
        }
    }
    Err("Pi JS/TS extensions require Node.js (node --version was not found)".into())
}

fn parse_resources(value: Option<&serde_json::Value>) -> JsResources {
    let list = |key: &str| {
        value
            .and_then(|v| v.get(key))
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    JsResources {
        skill_paths: list("skillPaths"),
        prompt_paths: list("promptPaths"),
        theme_paths: list("themePaths"),
    }
}

fn parse_tool_result(value: serde_json::Value) -> Result<AgentToolResult, String> {
    let mut result = AgentToolResult::default();
    if let Some(content) = value.get("content").and_then(serde_json::Value::as_array) {
        for block in content {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("image") {
                result
                    .content
                    .push(TextContentOrImage::Image(rpi_ai::types::ImageContent {
                        kind: rpi_ai::types::ImageContentType,
                        data: block
                            .get("data")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        mime_type: block
                            .get("mimeType")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("image/png")
                            .into(),
                    }));
            } else {
                result.content.push(TextContentOrImage::text(
                    block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                ));
            }
        }
    } else if let Some(text) = value.as_str() {
        result.content.push(TextContentOrImage::text(text));
    }
    result.details = value
        .get("details")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    result.usage = value
        .get("usage")
        .cloned()
        .filter(|value| !value.is_null())
        // Usage is optional metadata. Keep a well-formed native Usage value,
        // but preserve the tool result when an extension sends a newer or
        // incomplete shape that this Rust version does not understand.
        .and_then(|value| serde_json::from_value(value).ok());
    result.added_tool_names = value
        .get("addedToolNames")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    result.terminate = value
        .get("terminate")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(result)
}

fn normalize_host_path(path: &PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix("\\\\?\\") {
            return PathBuf::from(stripped);
        }
    }
    path.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_api::ExtensionBackend;

    #[test]
    fn lazy_runtime_context_merges_capabilities_before_start() {
        let transport = LazyNodeTransport::new(Vec::new(), serde_json::json!({}));
        transport
            .set_runtime_context(serde_json::json!({
                "capabilities": ["ui.custom"],
                "session": {"id": "first"}
            }))
            .unwrap();
        transport
            .set_runtime_context(serde_json::json!({
                "capabilities": ["ui.dialog"],
                "session": {"id": "second"}
            }))
            .unwrap();
        let pending = transport.pending_context.lock().unwrap().clone().unwrap();
        assert_eq!(
            pending["capabilities"],
            serde_json::json!(["ui.custom", "ui.dialog"])
        );
        assert_eq!(pending["session"]["id"], "second");
    }

    #[test]
    fn lazy_transport_stops_host_when_final_owner_drops() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("drop.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerCommand('ping', async () => ({ text: 'pong' }));",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let live = session.transport.ensure().unwrap();
        assert!(!live.is_shutdown());

        // A detached adapter/session clone keeps the lazy owner alive. The
        // transport itself is retained so the test can observe the shared
        // shutdown flag after the final lazy owner is released.
        let retained_owner = session.transport.clone();
        drop(session);
        assert!(!live.is_shutdown());
        drop(retained_owner);

        assert!(
            live.is_shutdown(),
            "the final LazyNodeTransport owner must stop the Node host"
        );
        assert!(live
            .request(
                "invoke_command",
                serde_json::json!({"command": "ping", "args": ""})
            )
            .is_err());
    }

    #[test]
    fn parse_tool_result_preserves_supported_metadata_and_ignores_unknown_usage() {
        let result = parse_tool_result(serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
            "details": {"source": "js"},
            "usage": {"totalTokens": "later-version"},
            "addedToolNames": ["next", 7],
        }))
        .unwrap();
        assert!(result.usage.is_none());
        assert_eq!(result.added_tool_names, ["next"]);
        assert_eq!(result.details["source"], "js");
    }

    #[test]
    fn loads_javascript_tool_and_invokes_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("extension.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerTool({name: 'hello', description: 'test', executionMode: 'sequential', parameters: {type: 'object'}, async execute() { return {content: [{type: 'text', text: 'ok'}], details: {}}; }});",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], true).unwrap().unwrap();
        let tool = session.tools().next().unwrap();
        assert_eq!(tool.schema().name, "hello");
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
        let value = session
            .invoke(
                "invoke_tool",
                serde_json::json!({"tool":"hello", "args":{}}),
            )
            .unwrap();
        assert_eq!(value["content"][0]["text"], "ok");
    }

    #[test]
    fn failed_extension_rolls_back_registrations_and_keeps_loading() {
        let temp = tempfile::tempdir().unwrap();
        let broken = temp.path().join("a-broken.js");
        let healthy = temp.path().join("b-healthy.js");
        std::fs::write(
            &broken,
            r#"export default (pi) => {
                pi.registerCommand('leaked-command', async () => ({ text: 'leaked' }));
                pi.registerTool({
                    name: 'leaked-tool',
                    description: 'must be rolled back',
                    parameters: { type: 'object', properties: {} },
                    async execute() { return { text: 'leaked' }; }
                });
                throw new Error('broken factory');
            };"#,
        )
        .unwrap();
        std::fs::write(
            &healthy,
            "export default (pi) => pi.registerCommand('healthy', async () => ({ text: 'pong' }));",
        )
        .unwrap();

        let session = JsExtensionSession::load(&[broken, healthy], false)
            .unwrap()
            .unwrap();
        assert_eq!(session.commands, ["healthy"]);
        assert!(!session
            .tool_names()
            .iter()
            .any(|name| name == "leaked-tool"));
        let value = session.invoke_command("healthy", "").unwrap();
        assert_eq!(value["result"]["text"], "pong");
        assert!(session.invoke_command("leaked-command", "").is_err());
    }

    #[test]
    fn failed_extension_rollback_preserves_existing_event_unsubscribe() {
        let temp = tempfile::tempdir().unwrap();
        let healthy = temp.path().join("a-healthy.js");
        let broken = temp.path().join("b-broken.js");
        std::fs::write(
            &healthy,
            r#"export default (pi) => {
                let calls = 0;
                const off = pi.events.on('probe', () => { calls += 1; });
                pi.registerCommand('unsubscribe', async () => {
                    off();
                    await pi.events.emit('probe');
                    return { text: String(calls) };
                });
            };"#,
        )
        .unwrap();
        std::fs::write(
            &broken,
            r#"export default (pi) => {
                pi.events.on('probe', () => {});
                throw new Error('broken factory');
            };"#,
        )
        .unwrap();

        let session = JsExtensionSession::load(&[healthy, broken], false)
            .unwrap()
            .unwrap();
        let value = session.invoke_command("unsubscribe", "").unwrap();
        assert_eq!(value["result"]["text"], "0");
    }

    #[test]
    fn directory_discovery_does_not_recurse_beyond_one_child_level() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("extensions");
        let deep = root.join("child/deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            root.join("direct.js"),
            "export default (pi) => pi.registerCommand('direct', async () => ({ text: 'ok' }));",
        )
        .unwrap();
        std::fs::write(
            deep.join("index.js"),
            "export default (pi) => pi.registerCommand('too-deep', async () => ({ text: 'bad' }));",
        )
        .unwrap();

        let session = JsExtensionSession::load(&[root], false).unwrap().unwrap();
        assert_eq!(session.commands, ["direct"]);
    }

    #[test]
    fn directory_manifest_prefers_rpi_extension_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("package");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"pi":{"extensions":["pi.js"]},"rpi":{"extensions":["rpi.js"]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("pi.js"),
            "export default (pi) => pi.registerCommand('pi-entry', async () => ({ text: 'pi' }));",
        )
        .unwrap();
        std::fs::write(
            root.join("rpi.js"),
            "export default (pi) => pi.registerCommand('rpi-entry', async () => ({ text: 'rpi' }));",
        )
        .unwrap();

        let session = JsExtensionSession::load(&[root], false).unwrap().unwrap();
        assert_eq!(session.commands, ["rpi-entry"]);
    }

    #[tokio::test]
    async fn javascript_tool_forwards_on_update_partials() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("progress.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerTool({
                name: 'progress',
                description: 'emit progress',
                parameters: { type: 'object', properties: {} },
                async execute(_id, _args, _signal, onUpdate) {
                    onUpdate({ content: [{ type: 'text', text: 'first' }], details: { step: 1 } });
                    await new Promise(resolve => setTimeout(resolve, 20));
                    onUpdate({ content: [{ type: 'text', text: 'second' }], details: { step: 2 } });
                    return {
                        content: [{ type: 'text', text: 'done' }],
                        details: {},
                        usage: {
                            input: 1, output: 2, cacheRead: 3, cacheWrite: 4,
                            totalTokens: 10,
                            cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.3 }
                        },
                        addedToolNames: ['follow_up']
                    };
                }
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let updates = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = updates.clone();
        let tool = session.tools().next().expect("registered JS tool");
        let result = tool
            .execute(
                "progress-call",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(move |partial| {
                    let text = partial
                        .content
                        .first()
                        .and_then(|content| match content {
                            TextContentOrImage::Text(text) => Some(text.text.clone()),
                            TextContentOrImage::Image(_) => None,
                        })
                        .unwrap_or_default();
                    observed.lock().unwrap().push(text);
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            result.content.first().and_then(|content| match content {
                TextContentOrImage::Text(text) => Some(text.text.as_str()),
                TextContentOrImage::Image(_) => None,
            }),
            Some("done")
        );
        assert_eq!(
            result.usage.as_ref().map(|usage| usage.total_tokens),
            Some(10)
        );
        assert_eq!(result.added_tool_names, ["follow_up"]);
        assert_eq!(updates.lock().unwrap().as_slice(), ["first", "second"]);
    }

    #[tokio::test]
    async fn javascript_tool_receives_ui_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ui-tool.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerTool({
                name: 'ui_probe',
                description: 'verify the Pi tool context',
                parameters: { type: 'object', properties: {} },
                async execute(_toolCallId, _args, _signal, _onUpdate, ctx) {
                    if (ctx?.hasUI !== true) throw new Error('tool context missing hasUI');
                    if (typeof ctx?.ui?.custom !== 'function') throw new Error('tool context missing ui.custom');
                    if (!ctx.capabilities?.has('ui.custom')) throw new Error('tool context missing ui.custom capability');
                    return { content: [{ type: 'text', text: 'ui-context-ok' }], details: {} };
                }
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        // Simulate the interactive host installing the custom UI bridge before
        // the lazy Node process starts. The tool must observe the same runtime
        // context as a command handler.
        session
            .set_runtime_context(serde_json::json!({
                "mode": "tui",
                "hasUI": true,
                "capabilities": ["ui.custom"],
            }))
            .unwrap();

        let tool = session.tools().next().expect("registered JS tool");
        let result = tool
            .execute(
                "tool-call-1",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .expect("tool should receive a UI context");
        match result.content.first() {
            Some(TextContentOrImage::Text(text)) => assert_eq!(text.text, "ui-context-ok"),
            other => panic!("unexpected tool content: {other:?}"),
        }
    }

    #[test]
    fn loads_typescript_when_node_supports_type_stripping() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("extension.ts");
        std::fs::write(
            &path,
            "export default (pi: any) => pi.registerTool({name: 'typed', description: 'test', parameters: {type: 'object'}, async execute() { return {content: [{type: 'text', text: 'typed'}]}; }});",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        assert_eq!(session.tools().next().unwrap().schema().name, "typed");
    }

    #[test]
    fn command_context_roundtrips_editor_and_capabilities() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("command.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerCommand('edit', async (_args, ctx) => { if (!ctx.capabilities.has('ui.editor')) throw new Error('missing capability'); if (ctx.model?.id !== 'model') throw new Error('missing model'); if (ctx.modelRegistry.getAll().length !== 1) throw new Error('missing catalog'); if (ctx.sessionManager.getSessionId() !== 'session-1' || ctx.sessionManager.getLeafId() !== 'leaf-1' || !ctx.sessionManager.getEntry('entry-1')) throw new Error('missing session'); ctx.ui.setEditorText(ctx.ui.getEditorText() + '!'); ctx.ui.notify('updated', 'info'); return {text: 'done'}; });",
        )
        .unwrap();
        let session = JsExtensionSession::load_with_context(
            &[path],
            false,
            serde_json::json!({
                "currentModel":{"id":"model","provider":"anthropic"},
                "models":[{"id":"model","provider":"anthropic"}]
            }),
        )
        .unwrap()
        .unwrap();
        assert!(session.transport.transport.lock().unwrap().is_none());
        session
            .set_runtime_context(serde_json::json!({
                "session":{"id":"session-1","leafId":"leaf-1","entries":[{"id":"entry-1"}]}
            }))
            .unwrap();
        assert!(session.transport.transport.lock().unwrap().is_none());
        let value = session
            .invoke_command_with_context("edit", "", serde_json::json!({"editorText":"draft"}))
            .unwrap();
        assert_eq!(value["result"]["text"], "done");
        assert_eq!(value["editorText"], "draft!");
        assert_eq!(value["notifications"][0]["message"], "updated");
        assert!(session
            .backend_info()
            .supports(crate::extension_api::ExtensionCapability::UiEditor));
    }

    #[test]
    fn command_context_is_non_ui_until_a_tui_bridge_is_installed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ui-mode.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('mode', async (_args, ctx) => ({
                hasUI: ctx.hasUI,
                mode: ctx.mode,
                hasCustom: typeof ctx.ui.custom === 'function'
            }));"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let plain = session.invoke_command("mode", "").unwrap();
        assert_eq!(plain["result"]["hasUI"], false);
        assert_eq!(plain["result"]["mode"], "print");
        assert_eq!(plain["result"]["hasCustom"], true);

        session
            .install_ui_dialog_runtime(Arc::new(|action, _args| {
                Err(format!("unsupported capability: {action}"))
            }))
            .unwrap();
        let tui = session.invoke_command("mode", "").unwrap();
        assert_eq!(tui["result"]["hasUI"], true);
        assert_eq!(tui["result"]["mode"], "tui");
    }

    #[test]
    fn before_agent_start_reconciles_js_active_tools_when_ui_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("active-tools.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.registerTool({
                    name: 'ask_user_question',
                    description: 'probe',
                    parameters: { type: 'object', properties: {} },
                    async execute() { return { content: [{ type: 'text', text: 'ok' }] }; }
                });
                pi.on('before_agent_start', (_event, ctx) => {
                    const active = pi.getActiveTools();
                    if (!ctx.hasUI) {
                        pi.setActiveTools(active.filter((name) => name !== 'ask_user_question'));
                    } else if (!active.includes('ask_user_question')) {
                        pi.setActiveTools([...active, 'ask_user_question']);
                    }
                });
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        assert!(!session
            .active_tools()
            .unwrap()
            .iter()
            .any(|name| name == "ask_user_question"));

        let tui = Arc::new(rpi_tui::TuiAltScreen::new(
            Box::new(rpi_tui::ProcessTerminal::new()),
            true,
            None,
        ));
        session.install_ui_runtime(tui).unwrap();
        assert!(session.transport.transport.lock().unwrap().is_none());
        session.ensure_runtime().unwrap();
        assert!(session
            .active_tools()
            .unwrap()
            .iter()
            .any(|name| name == "ask_user_question"));
    }

    #[test]
    fn persistent_host_runs_before_agent_start_without_pending_context() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("before-agent-start-count");
        let path = temp.path().join("before-agent-start.js");
        let marker_literal = serde_json::to_string(&marker.to_string_lossy()).unwrap();
        std::fs::write(&marker, "0").unwrap();
        std::fs::write(
            &path,
            format!(
                r#"import fs from 'node:fs';
                export default (pi) => {{
                    pi.registerCommand('ping', async () => ({{ text: 'pong' }}));
                    pi.on('before_agent_start', () => {{
                        const file = {marker};
                        const count = Number(fs.readFileSync(file, 'utf8') || '0') + 1;
                        fs.writeFileSync(file, String(count));
                    }});
                }};"#,
                marker = marker_literal,
            ),
        )
        .unwrap();

        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "1",
            "discovery host should run the lifecycle hook once"
        );

        // No runtime context is queued here. The first command still starts a
        // new persistent host and must run its own lifecycle hook.
        session.invoke_command("ping", "").unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "2",
            "persistent host skipped before_agent_start when no context was queued"
        );
    }

    #[test]
    fn prepare_for_prompt_runs_lifecycle_once_per_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("prompt-lifecycle-count");
        let path = temp.path().join("prompt-lifecycle.js");
        let marker_literal = serde_json::to_string(&marker.to_string_lossy()).unwrap();
        std::fs::write(&marker, "0").unwrap();
        std::fs::write(
            &path,
            format!(
                r#"import fs from 'node:fs';
                export default (pi) => {{
                    pi.on('before_agent_start', () => {{
                        const file = {marker};
                        const count = Number(fs.readFileSync(file, 'utf8') || '0') + 1;
                        fs.writeFileSync(file, String(count));
                    }});
                }};"#,
                marker = marker_literal,
            ),
        )
        .unwrap();

        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "1");

        session.prepare_for_prompt().unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "2");

        session.prepare_for_prompt().unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "3");
    }

    #[test]
    fn concurrent_runtime_context_hooks_apply_latest_active_tools() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("context-order.log");
        let marker_literal = serde_json::to_string(&marker.to_string_lossy()).unwrap();
        let path = temp.path().join("context-order.js");
        let source = r#"import fs from 'node:fs';
            export default (pi) => {
                pi.registerTool({ name: 'v1', description: 'v1', parameters: { type: 'object', properties: {} }, async execute() { return { text: 'v1' }; } });
                pi.registerTool({ name: 'v2', description: 'v2', parameters: { type: 'object', properties: {} }, async execute() { return { text: 'v2' }; } });
                pi.on('before_agent_start', async (_event, ctx) => {
                    const version = ctx.sessionManager.getSessionId();
                    fs.writeFileSync(__MARKER__, version);
                    if (version === 'v1') await new Promise(resolve => setTimeout(resolve, 200));
                    pi.setActiveTools([version]);
                });
            };"#
            .replace("__MARKER__", &marker_literal);
        std::fs::write(&path, source).unwrap();

        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        session.ensure_runtime().unwrap();

        let first = session.clone();
        let first_worker = std::thread::spawn(move || {
            first.set_runtime_context(serde_json::json!({ "session": { "id": "v1" } }))
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::fs::read_to_string(&marker).ok().as_deref() != Some("v1")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "v1");

        let second = session.clone();
        let second_worker = std::thread::spawn(move || {
            second.set_runtime_context(serde_json::json!({ "session": { "id": "v2" } }))
        });
        first_worker.join().unwrap().unwrap();
        second_worker.join().unwrap().unwrap();

        assert_eq!(session.active_tools().unwrap(), ["v2"]);
    }

    #[test]
    fn stale_runtime_context_revision_cannot_overwrite_newer_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("context-revision.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.on('before_agent_start', (_event, ctx) => {
                    pi.setActiveTools([ctx.sessionManager.getSessionId()]);
                });
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let transport = session.transport.ensure().unwrap();

        let newer = transport
            .request(
                "set_runtime_context",
                serde_json::json!({
                    "revision": 2,
                    "context": {"session": {"id": "newer"}}
                }),
            )
            .unwrap();
        assert_eq!(newer["activeTools"], serde_json::json!(["newer"]));

        let stale = transport
            .request(
                "set_runtime_context",
                serde_json::json!({
                    "revision": 1,
                    "context": {"session": {"id": "stale"}}
                }),
            )
            .unwrap();
        assert_eq!(stale["activeTools"], serde_json::json!(["newer"]));
    }

    #[test]
    fn restarted_host_replays_context_queued_before_first_start() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("context-restart.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.registerCommand('session-id', async (_args, ctx) => ({
                    text: ctx.sessionManager.getSessionId()
                }));
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        session
            .set_runtime_context(serde_json::json!({"session": {"id": "latest"}}))
            .unwrap();

        let first = session.invoke_command("session-id", "").unwrap();
        assert_eq!(first["result"]["text"], "latest");
        let first_transport = session.transport.live_transport().unwrap().unwrap();
        first_transport.shutdown();

        let restarted = session.invoke_command("session-id", "").unwrap();
        assert_eq!(restarted["result"]["text"], "latest");
    }

    #[test]
    fn failed_persistent_startup_requeues_initial_runtime_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup-failure.js");
        std::fs::write(
            &path,
            r#"export default async (pi) => {
                // Discovery is deliberately healthy, while the persistent
                // process exits before publishing its initialization envelope.
                // A thrown factory error is isolated per extension by the host
                // and therefore is not a transport-startup failure.
                if (process.env.RPI_JS_EXTENSION_ONESHOT !== '1') {
                    process.exit(17);
                }
                pi.registerCommand('ping', async () => ({ text: 'pong' }));
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let context = serde_json::json!({
            "mode": "tui",
            "hasUI": true,
            "capabilities": ["ui.custom"],
            "session": {"id": "startup-session"},
        });
        session.set_runtime_context(context.clone()).unwrap();

        let error = session.invoke_command("ping", "").unwrap_err();
        assert!(
            error.contains("exited before initialization")
                || error.contains("exited without a response")
                || error.contains("persistent host startup failure"),
            "unexpected startup error: {error}"
        );
        let pending = session
            .transport
            .pending_context
            .lock()
            .unwrap()
            .clone()
            .expect("failed startup must preserve runtime context");
        assert_eq!(pending["mode"], "tui");
        assert_eq!(pending["session"]["id"], "startup-session");
        assert_eq!(pending["capabilities"], serde_json::json!(["ui.custom"]));
    }

    #[test]
    fn dialog_ui_roundtrips_through_runtime_handler() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dialog.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.registerCommand('select', async (_args, ctx) => ({ text: await ctx.ui.select('Pick one', ['red', 'blue']) }));
                pi.registerCommand('confirm', async (_args, ctx) => ({ text: String(await ctx.ui.confirm('Continue?', 'Do it?')) }));
                pi.registerCommand('input', async (_args, ctx) => ({ text: await ctx.ui.input('Name', 'placeholder') }));
                pi.registerCommand('editor', async (_args, ctx) => ({ text: await ctx.ui.editor('Notes', 'prefilled') }));
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let actions = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let observed = actions.clone();
        session
            .install_ui_dialog_runtime(Arc::new(move |action, args| {
                observed
                    .lock()
                    .unwrap()
                    .push((action.to_string(), args.clone()));
                match action {
                    "ui.dialog" => match args.get("method").and_then(serde_json::Value::as_str) {
                        Some("select") => Ok(serde_json::json!({"value":"blue"})),
                        Some("confirm") => Ok(serde_json::json!({"confirmed":true})),
                        Some("input") => Ok(serde_json::json!({"value":"Ada"})),
                        Some("editor") => Ok(serde_json::json!({"value":"edited"})),
                        _ => Err("unknown dialog method".into()),
                    },
                    _ => Err(format!("unsupported capability: {action}")),
                }
            }))
            .unwrap();

        let backend = session.backend_info();
        assert!(backend.supports(crate::extension_api::ExtensionCapability::UiSelect));
        assert!(backend.supports(crate::extension_api::ExtensionCapability::UiConfirm));
        assert!(backend.supports(crate::extension_api::ExtensionCapability::UiInput));
        assert!(backend.supports(crate::extension_api::ExtensionCapability::UiEditorDialog));

        assert_eq!(
            session.invoke_command("select", "").unwrap()["result"]["text"],
            "blue"
        );
        assert_eq!(
            session.invoke_command("confirm", "").unwrap()["result"]["text"],
            "true"
        );
        assert_eq!(
            session.invoke_command("input", "").unwrap()["result"]["text"],
            "Ada"
        );
        assert_eq!(
            session.invoke_command("editor", "").unwrap()["result"]["text"],
            "edited"
        );

        let actions = actions.lock().unwrap();
        assert_eq!(actions.len(), 4);
        assert!(actions.iter().all(|(action, _)| action == "ui.dialog"));
        assert_eq!(actions[0].1["method"], "select");
        assert_eq!(actions[0].1["options"], serde_json::json!(["red", "blue"]));
        assert_eq!(actions[2].1["placeholder"], "placeholder");
        assert_eq!(actions[3].1["prefill"], "prefilled");
    }

    #[test]
    fn dialog_timeout_sends_cancel_request() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dialog-timeout.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('confirm', async (_args, ctx) => ({ text: String(await ctx.ui.confirm('Continue?', 'wait', { timeout: 20 })) }));"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let actions = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = actions.clone();
        session
            .install_ui_dialog_runtime(Arc::new(move |action, _args| {
                observed.lock().unwrap().push(action.to_string());
                match action {
                    "ui.dialog" => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        Ok(serde_json::json!({"confirmed":true}))
                    }
                    "ui.dialog.cancel" => Ok(serde_json::json!(true)),
                    _ => Err(format!("unsupported capability: {action}")),
                }
            }))
            .unwrap();
        let value = session.invoke_command("confirm", "").unwrap();
        assert_eq!(value["result"]["text"], "false");
        let actions = actions.lock().unwrap();
        assert!(actions.iter().any(|action| action == "ui.dialog"));
        assert!(actions.iter().any(|action| action == "ui.dialog.cancel"));
    }

    #[test]
    fn runtime_request_roundtrips_through_rust_handler() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerCommand('runtime', async () => ({text: await pi.runtimeRequest('echo', {value: 7})}));",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        session
            .set_runtime_handler(Arc::new(|action, args| {
                if action == "echo" {
                    Ok(args.get("value").cloned().unwrap_or_default())
                } else {
                    Err(format!("unsupported capability: {action}"))
                }
            }))
            .unwrap();
        let value = session.invoke_command("runtime", "").unwrap();
        assert_eq!(value["result"]["text"], 7);
    }

    #[test]
    fn startup_runtime_request_is_not_mistaken_for_init_and_uses_initial_handler() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup-runtime.js");
        std::fs::write(
            &path,
            r#"export default async (pi) => {
                const value = await pi.runtimeRequest('startup.echo', { value: 'persistent' })
                    .catch(() => 'discovery');
                pi.registerCommand('startup', async () => ({ text: String(value) }));
            };"#,
        )
        .unwrap();

        // Discovery runs without runtime handlers. The request must receive a
        // normal unsupported-capability response so the host can still publish
        // its id:0 initialization envelope.
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        session
            .set_runtime_handler(Arc::new(|action, args| {
                if action == "startup.echo" {
                    Ok(args.get("value").cloned().unwrap_or_default())
                } else {
                    Err(format!("unsupported capability: {action}"))
                }
            }))
            .unwrap();

        // Lazy startup passes the registered handler before the factory runs,
        // so the persistent host observes the handler during initialization.
        let value = session.invoke_command("startup", "").unwrap();
        assert_eq!(value["result"]["text"], "persistent");
    }

    #[test]
    fn provider_runtime_adapts_node_model_registry_to_rust_provider() {
        use rpi_ai::providers::faux::{FauxProvider, FauxScript};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("provider.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('complete', async (_args, ctx) => {
                const model = ctx.model;
                const provider = ctx.modelRegistry.getProvider(model.provider);
                if (!provider) throw new Error('provider missing');
                const stream = provider.streamSimple(model, { messages: [] }, {});
                let eventCount = 0;
                for await (const event of stream) eventCount += 1;
                const message = await stream.result();
                return { text: message.content?.[0]?.text || '', stopReason: message.stopReason, eventCount };
            });"#,
        )
        .unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let provider = FauxProvider::new(FauxScript::new().with_text("from rust"));
        let model = provider.default_model().clone();
        let session = JsExtensionSession::load_with_context(
            &[path],
            false,
            serde_json::json!({
                "currentModel": model,
                "models": [model],
            }),
        )
        .unwrap()
        .unwrap();
        session
            .enable_provider_runtime(provider.clone(), runtime.handle().clone())
            .unwrap();

        // `invoke_command` is synchronous by design. Keep it outside the
        // runtime's enter context so the handler can block on provider work.
        let value = session.invoke_command("complete", "").unwrap();
        assert_eq!(value["result"]["text"], "from rust");
        assert_eq!(value["result"]["stopReason"], "stop");
        assert!(value["result"]["eventCount"].as_u64().unwrap() >= 2);
        assert_eq!(
            provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        drop(session);
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    }

    #[test]
    fn node_transport_multiplexes_out_of_order_command_responses() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("concurrent.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.registerCommand('slow', async () => {
                    await new Promise(resolve => setTimeout(resolve, 300));
                    return { text: 'slow' };
                });
                pi.registerCommand('fast', async () => ({ text: 'fast' }));
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        assert!(session.transport.transport.lock().unwrap().is_none());
        let (sender, receiver) = std::sync::mpsc::channel();

        let slow = session.clone();
        let slow_sender = sender.clone();
        std::thread::spawn(move || {
            let value = slow.invoke_command("slow", "").unwrap();
            slow_sender.send(value["result"]["text"].clone()).unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let fast = session.clone();
        std::thread::spawn(move || {
            let value = fast.invoke_command("fast", "").unwrap();
            sender.send(value["result"]["text"].clone()).unwrap();
        });

        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("fast request should not wait for the slow request");
        assert_eq!(first, serde_json::json!("fast"));
        assert_eq!(receiver.recv().unwrap(), serde_json::json!("slow"));
    }

    #[test]
    fn shutting_down_session_wakes_detached_command_and_blocks_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("shutdown.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('wait', async () => {
                await new Promise(resolve => setTimeout(resolve, 10000));
                return { text: 'late' };
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let worker_session = session.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(worker_session.invoke_command("wait", ""))
                .unwrap();
        });

        // Wait until the lazy transport has started and the worker has had a
        // chance to enqueue its request before exercising shutdown.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while session.transport.transport.lock().unwrap().is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.transport.transport.lock().unwrap().is_some(),
            "lazy Node transport did not start"
        );

        session.shutdown();
        session.shutdown();
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown should wake the detached command");
        assert!(result.is_err(), "stopped command unexpectedly succeeded");
        worker.join().unwrap();
        assert!(
            session.transport.ensure().is_err(),
            "shutdown must prevent lazy Node restart"
        );
    }

    #[test]
    fn shutting_down_during_lazy_initialization_does_not_wait_for_slot_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("shutdown-init.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.on('before_agent_start', async (_event, ctx) => {
                    if (ctx.hasUI) {
                        await new Promise(resolve => setTimeout(resolve, 5000));
                    }
                });
                pi.registerCommand('wait', async () => ({ text: 'late' }));
            };"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        // Queue a context update so `ensure` has to wait for the corresponding
        // lifecycle response after the Node transport is started.
        session
            .set_runtime_context(serde_json::json!({
                "mode": "tui",
                "hasUI": true,
                "capabilities": ["ui.custom"],
            }))
            .unwrap();
        let worker_session = session.clone();
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let command_worker = std::thread::spawn(move || {
            command_sender
                .send(worker_session.invoke_command("wait", ""))
                .unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while session.transport.starting.lock().unwrap().is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.transport.starting.lock().unwrap().is_some(),
            "lazy startup did not publish its in-flight transport"
        );

        let shutdown_session = session.clone();
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        let shutdown_worker = std::thread::spawn(move || {
            shutdown_session.shutdown();
            shutdown_sender.send(()).unwrap();
        });
        shutdown_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown should not wait for a blocked initialization request");
        let result = command_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown should wake lazy initialization");
        assert!(
            result.is_err(),
            "stopped initialization unexpectedly succeeded"
        );
        command_worker.join().unwrap();
        shutdown_worker.join().unwrap();
    }

    #[test]
    fn shutting_down_during_node_initialization_kills_host_before_init_response() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("shutdown-before-init.js");
        std::fs::write(
            &path,
            r#"export default (pi) => {
                pi.on('before_agent_start', async (_event, ctx) => {
                    // Discovery runs in the one-shot host. Only the persistent
                    // host deliberately blocks before publishing id:0.
                    if (ctx.hasUI && process.env.RPI_JS_EXTENSION_ONESHOT !== '1') {
                        await new Promise(resolve => setTimeout(resolve, 30000));
                    }
                });
                pi.registerCommand('wait', async () => ({ text: 'late' }));
            };"#,
        )
        .unwrap();

        let session = JsExtensionSession::load_with_context(
            &[path],
            false,
            serde_json::json!({"mode":"tui", "hasUI":true}),
        )
        .unwrap()
        .unwrap();
        let worker_session = session.clone();
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let command_worker = std::thread::spawn(move || {
            command_sender
                .send(worker_session.invoke_command("wait", ""))
                .unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while session.transport.starting.lock().unwrap().is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.transport.starting.lock().unwrap().is_some(),
            "lazy startup did not publish its transport before init"
        );

        let started = std::time::Instant::now();
        session.shutdown();
        let result = command_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown should wake a command blocked in Node init");
        assert!(
            result.is_err(),
            "stopped initialization unexpectedly succeeded"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "shutdown waited for the blocked init hook"
        );
        command_worker.join().unwrap();
    }

    #[test]
    fn cancelling_prompt_preparation_kills_stuck_hook_and_allows_restart() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("blocked-once");
        let marker_literal = serde_json::to_string(&marker.to_string_lossy()).unwrap();
        let path = temp.path().join("cancel-prompt-preparation.js");
        std::fs::write(
            &path,
            format!(
                r#"import fs from 'node:fs';
                export default (pi) => {{
                    pi.registerCommand('ping', async () => ({{ text: 'pong' }}));
                    pi.on('before_agent_start', async () => {{
                        if (process.env.RPI_JS_EXTENSION_ONESHOT !== '1'
                            && !fs.existsSync({marker})) {{
                            fs.writeFileSync({marker}, 'blocked');
                            await new Promise(() => {{}});
                        }}
                    }});
                }};"#,
                marker = marker_literal,
            ),
        )
        .unwrap();

        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let cancellation = CancellationToken::new();
        let worker_session = session.clone();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(worker_session.prepare_for_prompt_with_cancellation(&worker_cancellation))
                .unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.is_file(), "persistent lifecycle hook did not block");

        cancellation.cancel();
        session.cancel_prompt_preparation();
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cancellation should wake a stuck prompt preparation");
        assert!(
            result.is_err(),
            "cancelled preparation unexpectedly succeeded"
        );
        worker.join().unwrap();
        assert!(session.transport.transport.lock().unwrap().is_none());
        assert!(session.transport.starting.lock().unwrap().is_none());

        // Cancelling one preparation is not a session shutdown. The marker
        // makes the replacement host's lifecycle return immediately.
        session.prepare_for_prompt().unwrap();
        let value = session.invoke_command("ping", "").unwrap();
        assert_eq!(value["result"]["text"], "pong");
    }

    #[test]
    fn node_transport_cancellation_reaches_tool_abort_signal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cancel.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerTool({
                name: 'wait',
                description: 'wait for cancellation',
                parameters: { type: 'object', properties: {} },
                execute: (_id, _args, signal) => new Promise(resolve => {
                    signal.addEventListener('abort', () => resolve({
                        content: [{ type: 'text', text: 'cancelled' }],
                        details: {}
                    }), { once: true });
                })
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let pending = session
            .transport
            .begin_request(
                "invoke_tool",
                serde_json::json!({"tool": "wait", "toolCallId": "test", "args": {}}),
            )
            .unwrap();
        let request_id = pending.id();
        std::thread::sleep(std::time::Duration::from_millis(30));
        session.transport.cancel(request_id).unwrap();
        let result = pending.wait().unwrap();
        assert_eq!(result["content"][0]["text"], "cancelled");
    }

    #[test]
    fn unsupported_ui_capabilities_fail_with_explicit_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ui.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerCommand('ui', async (_args, ctx) => { await ctx.ui.custom(() => null); });",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let error = session.invoke_command("ui", "").unwrap_err();
        assert!(error.contains("unsupported capability: ui.custom"));
    }

    #[test]
    fn custom_ui_roundtrips_open_render_and_done() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                return await ctx.ui.custom((parent, theme, keybindings, done) => {
                    parent.terminal.write(theme.fg('muted', 'frame'));
                    done({ ok: true });
                    return { render() { return ['frame']; }, handleInput() { return true; } };
                }, { overlay: true });
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let actions = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = actions.clone();
        session
            .set_runtime_handler(Arc::new(move |action, args| {
                observed.lock().unwrap().push(action.to_string());
                match action {
                    "ui.custom.open" | "ui.custom.close" => Ok(serde_json::json!(true)),
                    "ui.custom.write" => {
                        let data = args["data"].as_str().unwrap_or_default();
                        assert!(data.ends_with("frame\r\n") || data.starts_with("frame"));
                        Ok(serde_json::json!(true))
                    }
                    _ => Err(format!("unsupported capability: {action}")),
                }
            }))
            .unwrap();
        let value = session.invoke_command("custom", "").unwrap();
        assert_eq!(value["result"]["ok"], true);
        let actions = actions.lock().unwrap();
        assert!(actions.iter().any(|action| action == "ui.custom.open"));
        assert!(actions.iter().any(|action| action == "ui.custom.write"));
        assert!(actions.iter().any(|action| action == "ui.custom.close"));
    }

    #[test]
    fn custom_ui_forwards_overlay_options_handle_and_terminal_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom-overlay.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                let finish;
                const result = ctx.ui.custom((parent, _theme, _keys, done) => {
                    if (parent.terminal.columns !== 90 || parent.terminal.rows !== 20) throw new Error('wrong terminal size');
                    finish = done;
                    return { render() { return ['overlay-frame']; }, invalidate() {}, dispose() {} };
                }, {
                    overlay: true,
                    overlayOptions: { anchor: 'bottom-center', width: '100%', maxHeight: '100%', margin: { left: 0, right: 0, bottom: 0 } },
                    onHandle(handle) {
                        if (!handle.isFocused() || handle.isHidden()) throw new Error('invalid overlay handle');
                        handle.setHidden(true);
                        if (!handle.isHidden()) throw new Error('setHidden(true) was ignored');
                        handle.setHidden(false);
                        if (handle.isHidden()) throw new Error('setHidden(false) was ignored');
                        finish({ ok: true });
                    }
                });
                return await result;
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let actions = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let observed = actions.clone();
        session
            .set_runtime_handler(Arc::new(move |action, args| {
                observed
                    .lock()
                    .unwrap()
                    .push((action.to_string(), args.clone()));
                match action {
                    "ui.custom.open" => Ok(serde_json::json!({ "columns": 90, "rows": 20 })),
                    "ui.custom.handle" | "ui.custom.close" | "ui.custom.invalidate" => {
                        Ok(serde_json::json!(true))
                    }
                    "ui.custom.write" => Ok(serde_json::json!(true)),
                    _ => Err(format!("unsupported capability: {action}")),
                }
            }))
            .unwrap();
        let value = session.invoke_command("custom", "").unwrap();
        assert_eq!(value["result"]["ok"], true);
        let actions = actions.lock().unwrap();
        let open = actions
            .iter()
            .find(|(action, _)| action == "ui.custom.open")
            .expect("custom open action");
        assert_eq!(open.1["options"]["overlay"], true);
        assert_eq!(
            open.1["options"]["overlayOptions"]["anchor"],
            "bottom-center"
        );
        assert!(actions
            .iter()
            .any(|(action, _)| action == "ui.custom.handle"));
        assert!(actions
            .iter()
            .any(|(action, args)| { action == "ui.custom.handle" && args["hidden"] == true }));
        assert!(actions
            .iter()
            .any(|(action, args)| { action == "ui.custom.handle" && args["hidden"] == false }));
    }

    #[test]
    fn custom_ui_terminal_input_listener_can_consume_before_component() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom-input.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                let finish;
                ctx.ui.onTerminalInput((data) => {
                    if (data !== 'x') return;
                    finish({ consumed: true });
                    return { consume: true };
                });
                return await ctx.ui.custom((_parent, _theme, _keys, done) => {
                    finish = done;
                    return { handleInput() { throw new Error('component received consumed input'); } };
                });
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let tui = Arc::new(rpi_tui::TuiAltScreen::new(
            Box::new(rpi_tui::ProcessTerminal::new()),
            true,
            None,
        ));
        session.install_ui_runtime(tui).unwrap();

        let worker_session = session.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(worker_session.invoke_command("custom", ""))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !session.custom_active() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.custom_active(), "custom UI did not open");
        session.send_custom_input("x").unwrap();
        let value = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(value["result"]["consumed"], true);
    }

    #[test]
    fn custom_ui_terminal_input_listener_can_swallow_with_empty_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom-empty-input.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                let finish;
                let componentCalls = 0;
                ctx.ui.onTerminalInput((data) => {
                    if (data !== 'empty') return;
                    // Let the current input dispatch finish before resolving
                    // the command. Without the host's empty-data short-circuit
                    // the component is called before this timer and the count
                    // becomes 1.
                    setTimeout(() => finish({ componentCalls }), 0);
                    return { data: '' };
                });
                return await ctx.ui.custom((_parent, _theme, _keys, done) => {
                    finish = done;
                    return { handleInput() { componentCalls += 1; } };
                });
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let tui = Arc::new(rpi_tui::TuiAltScreen::new(
            Box::new(rpi_tui::ProcessTerminal::new()),
            true,
            None,
        ));
        session.install_ui_runtime(tui).unwrap();

        let worker_session = session.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(worker_session.invoke_command("custom", ""))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !session.custom_active() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.custom_active(), "custom UI did not open");

        session.send_custom_input("empty").unwrap();
        let value = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(value["result"]["componentCalls"], 0);
    }

    #[test]
    fn hidden_custom_ui_releases_unconsumed_input_but_keeps_listener() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom-hidden-input.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                let finish;
                let reopen;
                let componentCalls = 0;
                ctx.ui.onTerminalInput((data) => {
                    if (data === 'reopen') {
                        reopen?.setHidden(false);
                        return { consume: true };
                    }
                    if (data !== 'finish') return;
                    finish({ componentCalls });
                    return { consume: true };
                });
                return await ctx.ui.custom((_parent, _theme, _keys, done) => {
                    finish = done;
                    return { handleInput() { componentCalls += 1; } };
                 }, { onHandle(handle) { reopen = handle; handle.setHidden(true); } });
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let tui = Arc::new(rpi_tui::TuiAltScreen::new(
            Box::new(rpi_tui::ProcessTerminal::new()),
            true,
            None,
        ));
        session.install_ui_runtime(tui).unwrap();

        let worker_session = session.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(worker_session.invoke_command("custom", ""))
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while (session.custom_accepts_input() || !session.custom_active())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.custom_active(), "custom UI did not open");
        assert!(
            !session.custom_accepts_input(),
            "hidden UI kept keyboard focus"
        );

        // An ordinary key is acknowledged as unconsumed and must not reach the
        // hidden component. The outer TUI can use the same key for editing.
        assert!(!session.send_custom_input_with_consumed("ordinary").unwrap());
        assert!(session.send_custom_input_with_consumed("reopen").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !session.custom_accepts_input() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            session.custom_accepts_input(),
            "reopen shortcut did not restore focus"
        );
        // The raw listener still receives input while hidden and can reopen or
        // finish the overlay by consuming its shortcut.
        assert!(session.send_custom_input_with_consumed("finish").unwrap());
        let value = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(value["result"]["componentCalls"], 0);
    }

    #[test]
    fn custom_ui_closes_when_command_is_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom-cancel.js");
        std::fs::write(
            &path,
            r#"export default (pi) => pi.registerCommand('custom', async (_args, ctx) => {
                return await ctx.ui.custom((_parent, _theme, _keys, _done) => ({
                    handleInput() {},
                    dispose() {},
                }));
            });"#,
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let tui = Arc::new(rpi_tui::TuiAltScreen::new(
            Box::new(rpi_tui::ProcessTerminal::new()),
            true,
            None,
        ));
        session.install_ui_runtime(tui).unwrap();

        let pending = session
            .transport
            .begin_request(
                "invoke_command",
                serde_json::json!({"command":"custom", "args":"", "context":{}}),
            )
            .unwrap();
        let request_id = pending.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !session.custom_active() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(session.custom_active(), "custom UI did not open");

        session.transport.cancel(request_id).unwrap();
        let value = pending
            .wait()
            .expect("cancelled custom command should resolve");
        assert!(value.get("result").is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while session.custom_active() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !session.custom_active(),
            "cancelled custom UI remained active"
        );
    }

    #[test]
    fn runtime_handler_added_during_startup_reaches_starting_host() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("persistent-factory-ready");
        let release = temp.path().join("release-persistent-factory");
        let path = temp.path().join("startup-handler-race.js");
        let ready_literal = serde_json::to_string(&ready.to_string_lossy()).unwrap();
        let release_literal = serde_json::to_string(&release.to_string_lossy()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"import fs from 'node:fs';
                const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
                export default async (pi) => {{
                    if (process.env.RPI_JS_EXTENSION_ONESHOT === '1') {{
                        pi.registerCommand('late', async () => ({{ text: 'discovery' }}));
                        return;
                    }}
                    fs.writeFileSync({ready}, 'ready');
                    while (!fs.existsSync({release})) await sleep(5);
                    const value = await pi.runtimeRequest('late.echo', {{ value: 'handled' }});
                    pi.registerCommand('late', async () => ({{ text: String(value) }}));
                }};"#,
                ready = ready_literal,
                release = release_literal,
            ),
        )
        .unwrap();

        let session = JsExtensionSession::load(&[path], false).unwrap().unwrap();
        let worker_session = session.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(worker_session.invoke_command("late", ""))
                .unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !ready.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready.is_file(), "persistent factory did not reach startup");
        while session.transport.starting.lock().unwrap().is_none()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            session.transport.starting.lock().unwrap().is_some(),
            "startup transport was not published"
        );

        session
            .set_runtime_handler(Arc::new(|action, args| {
                if action == "late.echo" {
                    Ok(args.get("value").cloned().unwrap_or_default())
                } else {
                    Err(format!("unsupported capability: {action}"))
                }
            }))
            .unwrap();
        std::fs::write(&release, "release").unwrap();

        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("startup command did not finish");
        assert_eq!(result.unwrap()["result"]["text"], "handled");
    }
}
