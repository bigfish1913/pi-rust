//! Host-declared tools bridged to a `--mode rpc` client.
//!
//! A host that drives `rpi --mode rpc` (a desktop shell, an editor plugin)
//! often wants tools only it can implement: "reveal this file in the UI", "ask
//! the user to pick a target". The model may call them like any built-in tool,
//! but the engine forwards the call to the host and waits for its answer:
//!
//! - declaration: `--host-tools <file>`, a JSON array of
//!   `{name, description, parameters, promptSnippet?, promptGuidelines?}`
//!   entries (`parameters` is a JSON Schema object, the prompt metadata means
//!   the same as an extension tool's);
//! - call: the RPC loop writes
//!   `{"type":"host_tool_call","requestId",…,"toolCallId",…,"toolName",…,"arguments":{…}}`;
//! - result: the host writes
//!   `{"type":"host_tool_result","requestId",…,"content":[…],"isError"?,"details"?}`;
//! - cancel: an aborted run answers the call with an abort error and the RPC
//!   loop writes `{"type":"host_tool_cancelled","requestId"}` — a later result
//!   for that id is unknown.
//!
//! The host tools are ordinary tools in every other respect: `--tools`/
//! `--exclude-tools`/`--no-tools` filter them ([`crate::session::tool_name_allowed`]),
//! they keep the default `Never` replay policy, their declared prompt metadata
//! feeds the system prompt, and both the startup build and `/reload` register
//! them.
//!
//! The bridge is deliberately process-local state owned by whoever builds the
//! harness: tests and embedding hosts construct independent instances, and only
//! the `--mode rpc` loop relay that same instance to the client.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use rpi_harness::types::HarnessTool;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::args::{Args, Mode};
use crate::session::tool_name_allowed;

/// Longest accepted tool name (`^[A-Za-z0-9_-]{1,64}$`).
const NAME_MAX_LEN: usize = 64;

/// One entry of the `--host-tools` file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments the model must supply.
    pub parameters: serde_json::Value,
    /// Prompt contributions, identical in meaning to an extension tool's.
    #[serde(default)]
    pub prompt_snippet: Option<String>,
    #[serde(default)]
    pub prompt_guidelines: Vec<String>,
}

impl HostToolDefinition {
    fn prompt_metadata(&self) -> rpi_extensions::ToolPromptMetadata {
        rpi_extensions::ToolPromptMetadata {
            prompt_snippet: self.prompt_snippet.clone(),
            prompt_guidelines: self.prompt_guidelines.clone(),
        }
    }
}

/// One content block of a `host_tool_result` command.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToolContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl HostToolContentBlock {
    fn into_content(self) -> TextContentOrImage {
        match self {
            HostToolContentBlock::Text { text } => {
                TextContentOrImage::Text(rpi_ai::types::TextContent {
                    kind: rpi_ai::types::TextContentType,
                    text,
                    text_signature: None,
                })
            }
            HostToolContentBlock::Image { data, mime_type } => {
                TextContentOrImage::Image(rpi_ai::types::ImageContent {
                    kind: rpi_ai::types::ImageContentType,
                    data,
                    mime_type,
                })
            }
        }
    }
}

/// The `host_tool_result` command payload, as sent by the host.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolResultCommand {
    pub request_id: String,
    #[serde(default)]
    pub content: Vec<HostToolContentBlock>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub details: Option<serde_json::Value>,
}

impl HostToolResultCommand {
    pub(crate) fn into_outcome(self) -> HostToolOutcome {
        HostToolOutcome {
            content: self.content.into_iter().map(|b| b.into_content()).collect(),
            details: self.details.unwrap_or(serde_json::Value::Null),
            is_error: self.is_error,
        }
    }
}

/// What the host answered for one call.
#[derive(Debug, Clone, Default)]
pub struct HostToolOutcome {
    pub content: Vec<TextContentOrImage>,
    pub details: serde_json::Value,
    pub is_error: bool,
}

impl HostToolOutcome {
    /// A `isError: true` answer becomes a failed tool call, so the model sees
    /// the host's text as an error result instead of a success.
    fn into_tool_result(self, name: &str) -> Result<AgentToolResult, AgentError> {
        if self.is_error {
            let text = self
                .content
                .iter()
                .filter_map(|block| match block {
                    TextContentOrImage::Text(text) => Some(text.text.as_str()),
                    TextContentOrImage::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = if text.trim().is_empty() {
                format!("host tool `{name}` reported an error")
            } else {
                text
            };
            return Err(AgentError::Tool(text));
        }
        Ok(AgentToolResult {
            content: self.content,
            details: self.details,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        })
    }
}

/// What the RPC loop must write to the host: a call, or the cancellation of one.
#[derive(Debug, Clone)]
pub(crate) enum BridgeEvent {
    Call {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    Cancelled {
        request_id: String,
    },
}

struct BridgeState {
    definitions: Vec<HostToolDefinition>,
    /// Declared tools that survived the tool policy, i.e. what `ready`
    /// advertises and what the model can actually call.
    enabled: Mutex<Vec<String>>,
    /// Calls awaiting a `host_tool_result`, keyed by request id.
    pending: Mutex<HashMap<String, oneshot::Sender<Result<HostToolOutcome, String>>>>,
    events: mpsc::UnboundedSender<BridgeEvent>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<BridgeEvent>>>,
    /// False once the RPC loop is gone (stdin EOF / `stop`): no new call can be
    /// answered, so the tool fails immediately instead of waiting forever.
    connected: AtomicBool,
}

/// Connects the host-declared tools to the `--mode rpc` command loop.
///
/// Cheap to clone; every clone shares the same definitions and in-flight calls.
#[derive(Clone)]
pub struct HostToolBridge {
    state: Arc<BridgeState>,
}

impl std::fmt::Debug for HostToolBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostToolBridge")
            .field("tools", &self.state.definitions.len())
            .finish()
    }
}

impl Default for HostToolBridge {
    fn default() -> Self {
        Self::empty()
    }
}

impl HostToolBridge {
    /// A bridge with no declared tools (the common case).
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn new(definitions: Vec<HostToolDefinition>) -> Self {
        let (events, receiver) = mpsc::unbounded_channel();
        Self {
            state: Arc::new(BridgeState {
                definitions,
                enabled: Mutex::new(Vec::new()),
                pending: Mutex::new(HashMap::new()),
                events,
                receiver: Mutex::new(Some(receiver)),
                connected: AtomicBool::new(true),
            }),
        }
    }

    /// The declared tools the current tool policy enables, in file order.
    pub fn enabled_names(&self) -> Vec<String> {
        self.state
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Register the host tools that survive `--tools`/`--exclude-tools`/
    /// `--no-tools` into an existing tool list, and record that set for `ready`.
    /// Called on startup and on every reload, so the recorded set always matches
    /// the live tool list. A reloaded plugin may have registered a same-named
    /// tool in the meantime; first-registration-wins keeps that one (an
    /// extension tool is then not silently replaced mid-session).
    pub fn merge_into(&self, tools: &mut Vec<HarnessTool>, args: &Args) -> Vec<String> {
        let mut added = Vec::new();
        for definition in &self.state.definitions {
            if !tool_name_allowed(&definition.name, args) {
                continue;
            }
            if tools
                .iter()
                .any(|tool| tool.tool.schema().name == definition.name)
            {
                continue;
            }
            added.push(definition.name.clone());
            tools.push(HarnessTool::new(Arc::new(HostTool::new(
                definition.clone(),
                self.clone(),
            ))));
        }
        *self
            .state
            .enabled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = added.clone();
        added
    }

    /// Prompt contributions keyed by tool name. Tools without `promptSnippet`/
    /// `promptGuidelines` contribute the empty metadata, exactly like an
    /// extension tool that registered none; inactive names are ignored by the
    /// prompt renderer.
    pub fn prompt_metadata(&self) -> BTreeMap<String, rpi_extensions::ToolPromptMetadata> {
        self.state
            .definitions
            .iter()
            .map(|definition| (definition.name.clone(), definition.prompt_metadata()))
            .collect()
    }

    /// Hand the call stream to the single RPC loop relay. Returns `None` once
    /// it has been taken (or when no relay will ever read it).
    pub(crate) fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<BridgeEvent>> {
        self.state
            .receiver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Answer the call with that request id. An unknown or already settled id
    /// (cancelled, answered, or never issued) is an error for the host.
    pub fn respond(&self, request_id: &str, outcome: HostToolOutcome) -> Result<(), String> {
        let sender = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(request_id);
        match sender {
            Some(sender) => {
                let _ = sender.send(Ok(outcome));
                Ok(())
            }
            None => Err(format!("unknown host tool request `{request_id}`")),
        }
    }

    /// End the session: no further call can be answered, so every in-flight
    /// call fails now instead of waiting for a host that is gone.
    pub fn disconnect(&self) {
        self.state.connected.store(false, Ordering::Release);
        let pending: Vec<_> = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for sender in pending {
            let _ = sender.send(Err("host disconnected".to_string()));
        }
    }

    /// Register the call before it is announced, so a result can never arrive
    /// for an unknown id.
    fn begin(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<(String, oneshot::Receiver<Result<HostToolOutcome, String>>), AgentError> {
        if !self.state.connected.load(Ordering::Acquire) {
            return Err(AgentError::Tool("host disconnected".to_string()));
        }
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let (sender, receiver) = oneshot::channel();
        self.state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(request_id.clone(), sender);
        let sent = self.state.events.send(BridgeEvent::Call {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments,
        });
        // Re-check after announcing: `disconnect` drains whatever was pending at
        // that instant, so a call announced during the shutdown must not be left
        // waiting for an answer nobody can send.
        if sent.is_err() || !self.state.connected.load(Ordering::Acquire) {
            self.state
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&request_id);
            return Err(AgentError::Tool("host disconnected".to_string()));
        }
        Ok((request_id, receiver))
    }

    /// Retire the request and tell the host the run no longer waits for it.
    fn cancel(&self, request_id: &str) {
        let removed = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(request_id);
        if removed.is_some() {
            let _ = self.state.events.send(BridgeEvent::Cancelled {
                request_id: request_id.to_string(),
            });
        }
    }
}

/// Load and validate `--host-tools`. A missing flag is the empty bridge.
///
/// Every rejection is fatal at startup: the model would otherwise be offered
/// tools the host cannot answer, or a name collision would silently shadow a
/// built-in/extension tool. `extension_tool_names` comes from the extension
/// session loaded just before this call.
pub fn load(args: &Args, extension_tool_names: &[String]) -> Result<HostToolBridge, String> {
    let Some(path) = args.host_tools.as_deref() else {
        return Ok(HostToolBridge::empty());
    };
    if args.mode != Mode::Rpc {
        return Err("--host-tools requires --mode rpc".to_string());
    }
    let raw = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "could not read --host-tools file {}: {error}",
            path.display()
        )
    })?;
    let definitions = parse_definitions(&raw)?;
    for definition in &definitions {
        if extension_tool_names
            .iter()
            .any(|name| name == &definition.name)
        {
            return Err(format!(
                "host tool \"{}\" collides with an extension tool of the same name",
                definition.name
            ));
        }
    }
    Ok(HostToolBridge::new(definitions))
}

/// Parse and validate the declaration file: array of tools, valid unique names,
/// JSON-Schema-object parameters, and no built-in collision.
fn parse_definitions(raw: &str) -> Result<Vec<HostToolDefinition>, String> {
    let definitions: Vec<HostToolDefinition> =
        serde_json::from_str(raw).map_err(|error| format!("invalid --host-tools file: {error}"))?;
    let mut seen = std::collections::HashSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        if !valid_name(&definition.name) {
            return Err(format!(
                "--host-tools entry {index} has an invalid name \"{}\": names must match ^[A-Za-z0-9_-]{{1,{NAME_MAX_LEN}}}$",
                definition.name
            ));
        }
        if reserved(&definition.name) {
            return Err(format!(
                "host tool \"{}\" collides with a built-in tool of the same name",
                definition.name
            ));
        }
        if !seen.insert(definition.name.as_str()) {
            return Err(format!(
                "duplicate host tool name \"{}\" in the --host-tools file",
                definition.name
            ));
        }
        if !definition.parameters.is_object() {
            return Err(format!(
                "host tool \"{}\" has non-object `parameters`; expected a JSON Schema object",
                definition.name
            ));
        }
    }
    Ok(definitions)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Whether a host tool may not take `name`: every tool the CLI itself can
/// register, `powershell` included on every platform so a declaration file
/// means the same everywhere.
fn reserved(name: &str) -> bool {
    crate::session::BUILTIN_TOOL_NAMES.contains(&name) || name == "powershell"
}

/// One declared tool, executed by forwarding the call to the host.
struct HostTool {
    bridge: HostToolBridge,
    schema: Tool,
}

impl HostTool {
    fn new(definition: HostToolDefinition, bridge: HostToolBridge) -> Self {
        Self {
            bridge,
            schema: Tool {
                name: definition.name,
                description: definition.description,
                parameters: rpi_ai::types::Schema::new(definition.parameters),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for HostTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let (request_id, answer) = self.bridge.begin(tool_call_id, &self.schema.name, params)?;
        tokio::select! {
            biased;
            _ = signal.cancelled() => {
                self.bridge.cancel(&request_id);
                Err(AgentError::Tool(format!(
                    "host tool `{}` cancelled",
                    self.schema.name
                )))
            }
            answer = answer => match answer {
                Ok(Ok(outcome)) => outcome.into_tool_result(&self.schema.name),
                Ok(Err(error)) => Err(AgentError::Tool(error)),
                Err(_) => Err(AgentError::Tool("host disconnected".to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc_args(path: &std::path::Path) -> Args {
        Args {
            mode: Mode::Rpc,
            host_tools: Some(path.to_path_buf()),
            ..Default::default()
        }
    }

    fn write_declarations(content: &serde_json::Value) -> (tempfile::TempDir, Args) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host-tools.json");
        std::fs::write(&path, content.to_string()).unwrap();
        let args = rpc_args(&path);
        (directory, args)
    }

    #[test]
    fn rejects_invalid_names() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open file", "description": "d", "parameters": {"type": "object"}}
        ]));
        let error = load(&args, &[]).unwrap_err();
        assert!(error.contains("invalid name"), "{error}");
        for name in ["", "a".repeat(65).as_str(), "révéler"] {
            let (_directory, args) = write_declarations(&serde_json::json!([
                {"name": name, "description": "d", "parameters": {"type": "object"}}
            ]));
            assert!(load(&args, &[]).is_err(), "accepted name {name:?}");
        }
    }

    #[test]
    fn rejects_duplicate_and_builtin_names() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}},
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}}
        ]));
        assert!(load(&args, &[]).unwrap_err().contains("duplicate"));
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "read", "description": "d", "parameters": {"type": "object"}}
        ]));
        assert!(load(&args, &[]).unwrap_err().contains("built-in"));
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}}
        ]));
        let error = load(&args, &["open_file".to_string()]).unwrap_err();
        assert!(error.contains("extension tool"), "{error}");
    }

    #[test]
    fn rejects_the_flag_outside_rpc_mode() {
        let (_directory, mut args) = write_declarations(&serde_json::json!([]));
        args.mode = Mode::Text;
        assert!(load(&args, &[]).unwrap_err().contains("--mode rpc"));
        args.mode = Mode::Json;
        assert!(load(&args, &[]).is_err());
        args.mode = Mode::Rpc;
        assert!(load(&args, &[]).is_ok());
    }

    #[test]
    fn rejects_unreadable_files() {
        let args = rpc_args(std::path::Path::new("does/not/exist.json"));
        assert!(load(&args, &[]).unwrap_err().contains("could not read"));
    }

    #[test]
    fn rejects_non_object_parameters() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": []}
        ]));
        assert!(load(&args, &[]).unwrap_err().contains("non-object"));
    }

    #[test]
    fn rejects_malformed_declaration_files() {
        let (_directory, args) = write_declarations(&serde_json::json!({"name": "open_file"}));
        assert!(load(&args, &[])
            .unwrap_err()
            .contains("invalid --host-tools"));
        // `parameters` is required; a missing schema is not an empty object.
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d"}
        ]));
        assert!(load(&args, &[]).is_err());
    }

    #[test]
    fn tool_policy_filters_declared_tools() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}},
            {"name": "reveal", "description": "d", "parameters": {"type": "object"}}
        ]));
        let bridge = load(&args, &[]).unwrap();
        let mut tools = Vec::new();
        bridge.merge_into(&mut tools, &Args::default());
        assert_eq!(tools.len(), 2);
        assert_eq!(bridge.enabled_names(), ["open_file", "reveal"]);
        let mut tools = Vec::new();
        bridge.merge_into(
            &mut tools,
            &Args {
                tools: Some(vec!["reveal".to_string()]),
                ..Default::default()
            },
        );
        assert_eq!(tools.len(), 1);
        assert_eq!(bridge.enabled_names(), ["reveal"]);
        let mut tools = Vec::new();
        bridge.merge_into(
            &mut tools,
            &Args {
                no_tools: true,
                ..Default::default()
            },
        );
        assert!(tools.is_empty());
        assert!(bridge.enabled_names().is_empty());
    }

    #[tokio::test]
    async fn answered_call_returns_the_host_content() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}}
        ]));
        let bridge = load(&args, &[]).unwrap();
        let mut tools = Vec::new();
        bridge.merge_into(&mut tools, &Args::default());
        let tool = tools.remove(0).tool;
        let mut receiver = bridge.take_receiver().unwrap();
        let call = tokio::spawn(async move {
            tool.execute(
                "call-1",
                serde_json::json!({"path": "a.txt"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
        });
        let Some(BridgeEvent::Call {
            request_id,
            tool_call_id,
            tool_name,
            arguments,
        }) = receiver.recv().await
        else {
            panic!("expected a call event");
        };
        assert_eq!(tool_call_id, "call-1");
        assert_eq!(tool_name, "open_file");
        assert_eq!(arguments["path"], "a.txt");
        bridge
            .respond(
                &request_id,
                HostToolOutcome {
                    content: vec![TextContentOrImage::text("opened")],
                    details: serde_json::json!({"path": "a.txt"}),
                    is_error: false,
                },
            )
            .unwrap();
        let result = call.await.unwrap().unwrap();
        assert_eq!(result.content, vec![TextContentOrImage::text("opened")]);
        assert_eq!(result.details["path"], "a.txt");
    }

    #[tokio::test]
    async fn cancelled_call_notifies_the_host_and_rejects_a_late_result() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}}
        ]));
        let bridge = load(&args, &[]).unwrap();
        let mut tools = Vec::new();
        bridge.merge_into(&mut tools, &Args::default());
        let tool = tools.remove(0).tool;
        let mut receiver = bridge.take_receiver().unwrap();
        let signal = CancellationToken::new();
        let call = tokio::spawn({
            let signal = signal.clone();
            async move {
                tool.execute("call-1", serde_json::json!({}), signal, Arc::new(|_| {}))
                    .await
            }
        });
        let Some(BridgeEvent::Call { request_id, .. }) = receiver.recv().await else {
            panic!("expected a call event");
        };
        signal.cancel();
        assert!(call.await.unwrap().is_err());
        assert!(matches!(
            receiver.recv().await,
            Some(BridgeEvent::Cancelled { request_id: cancelled }) if cancelled == request_id
        ));
        assert!(bridge
            .respond(&request_id, HostToolOutcome::default())
            .is_err());
    }

    #[tokio::test]
    async fn disconnect_fails_every_pending_call() {
        let (_directory, args) = write_declarations(&serde_json::json!([
            {"name": "open_file", "description": "d", "parameters": {"type": "object"}}
        ]));
        let bridge = load(&args, &[]).unwrap();
        let mut tools = Vec::new();
        bridge.merge_into(&mut tools, &Args::default());
        let tool = tools.remove(0).tool;
        let mut receiver = bridge.take_receiver().unwrap();
        let call = tokio::spawn(async move {
            tool.execute(
                "call-1",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
        });
        assert!(receiver.recv().await.is_some());
        bridge.disconnect();
        let error = call.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("host disconnected"), "{error}");
    }
}
