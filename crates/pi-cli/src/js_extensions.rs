//! Minimal Pi JS/TS extension host.
//!
//! A single Node child process owns loaded extensions for the session. Rust
//! exchanges JSON-lines requests with it, keeping extension code isolated from
//! the agent process while still exposing Pi's tool and resource contracts.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
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

const NODE_HOST: &str = include_str!("node_host.mjs");

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
    tool: Tool,
}

pub type RuntimeHandler =
    Arc<dyn Fn(&str, serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

struct HostIo {
    _child: Child,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    rpc: Mutex<()>,
    runtime_handlers: Mutex<Vec<RuntimeHandler>>,
}

impl Drop for HostIo {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

#[derive(Clone)]
pub struct JsExtensionSession {
    io: Arc<HostIo>,
    tools: Arc<Vec<JsToolDefinition>>,
    pub resources: JsResources,
    pub commands: Vec<String>,
    capabilities: Arc<Mutex<Vec<String>>>,
    custom_active: Arc<Mutex<Option<String>>>,
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
        let node = which_node()?;
        let mut child = Command::new(node)
            .args(["--input-type=module", "-e", NODE_HOST])
            .env(
                "RPI_JS_EXTENSION_PATHS",
                serde_json::to_string(&paths).unwrap_or_else(|_| "[]".into()),
            )
            .env(
                "RPI_JS_EXTENSION_CONTEXT",
                serde_json::to_string(&context).unwrap_or_else(|_| "{}".into()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Extension diagnostics belong on stderr; keeping it inherited
            // prevents an undrained pipe from blocking a noisy extension.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start Node JS extension host: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or("Node extension host stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Node extension host stdout unavailable")?;
        let io = Arc::new(HostIo {
            _child: child,
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            rpc: Mutex::new(()),
            runtime_handlers: Mutex::new(Vec::new()),
        });
        let init = read_response(&io)?;
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
            definitions.push(JsToolDefinition {
                name: name.clone(),
                label,
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
        Ok(Some(Self {
            io,
            tools: Arc::new(definitions),
            resources,
            commands,
            capabilities: Arc::new(Mutex::new(capabilities)),
            custom_active: Arc::new(Mutex::new(None)),
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
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _rpc_guard = self
            .io
            .rpc
            .lock()
            .map_err(|_| "Node host RPC lock poisoned")?;
        let mut request = serde_json::json!({"id": id, "method": method});
        if let (Some(object), Some(values)) = (request.as_object_mut(), payload.as_object()) {
            object.extend(values.clone());
        }
        {
            let mut stdin = self
                .io
                .stdin
                .lock()
                .map_err(|_| "Node host stdin lock poisoned")?;
            writeln!(stdin, "{}", request)
                .map_err(|error| format!("could not write to Node extension host: {error}"))?;
            stdin
                .flush()
                .map_err(|error| format!("could not flush Node extension host: {error}"))?;
        }
        loop {
            let response = read_response(&self.io)?;
            if response.get("type").and_then(serde_json::Value::as_str) == Some("runtime_request") {
                let request_id = response
                    .get("requestId")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("Node runtime request has no requestId")?;
                let action = response
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("Node runtime request has no action")?;
                let args = response.get("args").cloned().unwrap_or_default();
                let handlers = self
                    .io
                    .runtime_handlers
                    .lock()
                    .map_err(|_| "Node runtime handler lock poisoned")?
                    .clone();
                let mut result = Err(format!("unsupported capability: {action}"));
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
                let mut stdin = self
                    .io
                    .stdin
                    .lock()
                    .map_err(|_| "Node host stdin lock poisoned")?;
                writeln!(stdin, "{response}")
                    .map_err(|error| format!("could not write Node runtime response: {error}"))?;
                stdin
                    .flush()
                    .map_err(|error| format!("could not flush Node runtime response: {error}"))?;
                continue;
            }
            if response.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                return Err("Node extension host returned an out-of-order response".into());
            }
            if response
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(response.get("result").cloned().unwrap_or_default());
            }
            return Err(response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("JS extension failed")
                .to_string());
        }
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
        self.invoke(
            "invoke_command",
            serde_json::json!({"command": command, "args": args, "context": context}),
        )
    }

    pub fn set_runtime_context(&self, context: serde_json::Value) -> Result<(), String> {
        self.invoke(
            "set_runtime_context",
            serde_json::json!({"context": context}),
        )
        .map(|_| ())
    }

    pub fn set_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        *self
            .io
            .runtime_handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")? = vec![handler];
        Ok(())
    }

    pub fn add_runtime_handler(&self, handler: RuntimeHandler) -> Result<(), String> {
        self.io
            .runtime_handlers
            .lock()
            .map_err(|_| "Node runtime handler lock poisoned")?
            .push(handler);
        Ok(())
    }

    pub fn custom_active(&self) -> bool {
        self.custom_active
            .lock()
            .map(|value| value.is_some())
            .unwrap_or(false)
    }

    pub fn send_custom_input(&self, data: &str) -> Result<(), String> {
        let id = self
            .custom_active
            .lock()
            .map_err(|_| "Node custom state lock poisoned")?
            .clone()
            .ok_or("no active Node custom UI")?;
        let event = serde_json::json!({
            "type": "host_event",
            "event": "custom_input",
            "customId": id,
            "data": data,
        });
        let mut stdin = self
            .io
            .stdin
            .lock()
            .map_err(|_| "Node host stdin lock poisoned")?;
        writeln!(stdin, "{event}")
            .map_err(|error| format!("could not write Node custom input: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("could not flush Node custom input: {error}"))?;
        Ok(())
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
        let mut stdin = self
            .io
            .stdin
            .lock()
            .map_err(|_| "Node host stdin lock poisoned")?;
        writeln!(stdin, "{event}")
            .map_err(|error| format!("could not write Node custom resize: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("could not flush Node custom resize: {error}"))?;
        Ok(())
    }

    pub fn install_ui_runtime(&self, tui: Arc<rpi_tui::TuiAltScreen>) -> Result<(), String> {
        let active = self.custom_active.clone();
        self.add_runtime_handler(Arc::new(move |action, args| match action {
            "ui.custom.open" => {
                let id = args
                    .get("customId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.custom.open missing customId")?
                    .to_string();
                *active
                    .lock()
                    .map_err(|_| "Node custom state lock poisoned")? = Some(id.clone());
                tui.set_render_suspended(true);
                tui.terminal().clear_screen();
                Ok(serde_json::json!(true))
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
            "ui.custom.close" => {
                *active
                    .lock()
                    .map_err(|_| "Node custom state lock poisoned")? = args
                    .get("restoreCustomId")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                tui.set_render_suspended(false);
                Ok(serde_json::json!(true))
            }
            "ui.custom.resize" => Ok(serde_json::json!(true)),
            _ => Err(format!("unsupported capability: {action}")),
        }))?;
        self.enable_capability("ui.custom")?;
        self.set_runtime_context(serde_json::json!({"capabilities":["ui.custom"]}))
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
            let context: Context = serde_json::from_value(
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
        ToolExecutionMode::Parallel
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let session = self.session.clone();
        let name = self.definition.name.clone();
        let tool_call_id = tool_call_id.to_string();
        let result = tokio::task::spawn_blocking(move || {
            session.invoke(
                "invoke_tool",
                serde_json::json!({"tool": name, "toolCallId": tool_call_id, "args": params}),
            )
        })
        .await
        .map_err(|error| AgentError::tool(error.to_string()))?
        .map_err(AgentError::tool)?;
        parse_tool_result(result).map_err(AgentError::tool)
    }
}

fn which_node() -> Result<String, String> {
    for candidate in ["node", "nodejs"] {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Ok(candidate.to_string());
        }
    }
    Err("Pi JS/TS extensions require Node.js (node --version was not found)".into())
}

fn read_response(io: &HostIo) -> Result<serde_json::Value, String> {
    let mut line = String::new();
    io.stdout
        .lock()
        .map_err(|_| "Node host stdout lock poisoned")?
        .read_line(&mut line)
        .map_err(|error| format!("could not read Node extension host: {error}"))?;
    if line.trim().is_empty() {
        return Err("Node extension host exited without a response".into());
    }
    serde_json::from_str(&line).map_err(|error| format!("invalid Node extension response: {error}"))
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
    result.terminate = value
        .get("terminate")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(result)
}

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
    fn loads_javascript_tool_and_invokes_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("extension.js");
        std::fs::write(
            &path,
            "export default (pi) => pi.registerTool({name: 'hello', description: 'test', parameters: {type: 'object'}, async execute() { return {content: [{type: 'text', text: 'ok'}], details: {}}; }});",
        )
        .unwrap();
        let session = JsExtensionSession::load(&[path], true).unwrap().unwrap();
        let tool = session.tools().next().unwrap();
        assert_eq!(tool.schema().name, "hello");
        let value = session
            .invoke(
                "invoke_tool",
                serde_json::json!({"tool":"hello", "args":{}}),
            )
            .unwrap();
        assert_eq!(value["content"][0]["text"], "ok");
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
        session
            .set_runtime_context(serde_json::json!({
                "session":{"id":"session-1","leafId":"leaf-1","entries":[{"id":"entry-1"}]}
            }))
            .unwrap();
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
                        assert!(args["data"]
                            .as_str()
                            .unwrap_or_default()
                            .starts_with("frame"));
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
}
