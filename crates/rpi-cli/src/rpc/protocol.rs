//! Minimal P0 JSONL protocol for the native rpi RPC host.
use serde_json::{json, Value};

pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const PI_BASELINE: &str = "1.2.0";

/// Native ordered user payload; deliberately excludes assistant/tool blocks.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptPart {
    Text { text: String },
    Image { data: String, #[serde(rename = "mimeType")] mime_type: String },
    Audio { data: String, #[serde(rename = "mimeType")] mime_type: String },
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum PromptContent {
    Legacy(String),
    Ordered(Vec<PromptPart>),
}

pub fn ordered_content(request: &Value) -> Result<Option<Vec<rpi_ai::types::Content>>, String> {
    let Some(value) = request.get("content") else { return Ok(None); };
    let content: PromptContent = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let parts = match content {
        PromptContent::Legacy(text) => {
            // Legacy strings retain the existing frame bound, not the tighter
            // ordered-parts budget. The dispatcher still combines legacy media.
            if text.len() > MAX_FRAME_BYTES {
                return Err("prompt content exceeds native frame limit".into());
            }
            return Ok(None);
        }
        PromptContent::Ordered(parts) => parts,
    };
    // There is no implied ordering between the two representations. Check both
    // aliases, including ones hidden by dispatcher precedence. Empty placeholders
    // remain valid; malformed media still goes through the existing decoder.
    if ["message", "prompt"].iter().any(|key| {
        request.get(key).and_then(Value::as_str).is_some_and(|text| !text.is_empty())
    }) || ["images", "audio"].iter().any(|key| {
        request.get(key).and_then(Value::as_array).is_some_and(|parts| !parts.is_empty())
    }) {
        return Err("ordered content cannot be combined with nonempty legacy message, prompt, images or audio".into());
    }
    if parts.len() > 256 || serde_json::to_vec(value).map_err(|e| e.to_string())?.len() > 900_000 {
        return Err("prompt content exceeds native input limits".into());
    }
    use rpi_ai::types::{Content, ImageContent, AudioContent};
    Ok(Some(parts.into_iter().map(|part| match part {
        PromptPart::Text { text } => Content::text(text),
        PromptPart::Image { data, mime_type } => Content::Image(ImageContent { kind: Default::default(), data, mime_type }),
        PromptPart::Audio { data, mime_type } => Content::Audio(AudioContent { kind: Default::default(), data, mime_type }),
    }).collect()))
}


#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub command: Command,
}

/// Payload schema for `Request`. Only deserialization matters: the dispatcher
/// matches on the raw `type` string, so the parsed fields are never read.
#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Prompt {
        #[serde(default, alias = "prompt")]
        message: Option<String>,
        #[serde(default)]
        content: Option<PromptContent>,
    },
    Abort,
    Sample {
        #[serde(rename = "sampleId")]
        sample_id: String,
        request: Value,
    },
    CancelSample {
        #[serde(rename = "sampleId")]
        sample_id: String,
    },
    GetState,
    /// Append (default) to the current pre-skills prompt, or explicitly replace it.
    SetSystemPrompt {
        prompt: String,
        mode: Option<String>,
    },
    /// List extension slash commands (name and description).
    GetCommands,
    /// Invoke an extension handler and return its JSON result as response data.
    /// `command` accepts a registered name with or without a leading slash.
    ExecuteCommand {
        command: String,
        #[serde(default)]
        args: String,
    },
    /// Read-only snapshot of the current branch's message entries.
    GetMessages {
        limit: Option<u64>,
        #[serde(rename = "beforeId")]
        before_id: Option<String>,
    },
    GetHistory {
        limit: Option<u64>,
        #[serde(rename = "afterSeq")]
        after_seq: Option<u64>,
        #[serde(rename = "beforeSeq")]
        before_seq: Option<u64>,
    },
    /// Return every entry on the currently selected native branch.
    GetSessionBranch,
    /// Append a protocol-neutral custom entry to the current branch.
    AppendEntry {
        #[serde(rename = "customType")]
        custom_type: String,
        data: Option<Value>,
    },
    /// Read the context-window usage estimate used by native compaction.
    GetContextUsage,
    /// Toggle native automatic context compaction for subsequent runs.
    SetCompactionEnabled {
        enabled: bool,
    },
    /// Read and change the native session mode strategy.
    GetModes,
    SetMode {
        #[serde(rename = "modeId")]
        mode_id: String,
    },
    GetStats,
    GetQueue,
    QueueMessage {
        message: String,
        #[serde(rename = "deliverAs")]
        deliver_as: Option<String>,
    },
    CancelQueued {
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    ClearQueue,
    Navigate {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
    /// Continue the run that recovery left interrupted.
    ResumePending,
    /// Rewrite the model's view of the branch into a summary.
    Compact {
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    GetAvailableModels,
    GetAuthStatus,
    #[serde(rename = "oauth")]
    OAuth {
        provider: String,
        action: String,
        input: Option<Value>,
    },
    SetApiKey {
        provider: String,
        #[serde(rename = "apiKey")]
        api_key: String,
    },
    SetModel {
        provider: Option<String>,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    GetAvailableThinkingLevels,
    SetThinkingLevel {
        level: rpi_ai::types::ThinkingLevel,
    },
    GetAvailableTools,
    SetActiveTools {
        tools: Vec<String>,
    },
    /// Ask a connected client to perform an operation and wait for its unique
    /// `requestId`. This command is a bridge seam, not a registered agent tool.
    /// Optional `timeoutMs` is validated by the dispatcher (1..=60_000).
    ClientRequest {
        method: String,
        params: Value,
    },
    UiDialogResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        answer: Option<Value>,
        /// Explicit URL action; takes precedence over generic cancellation.
        #[serde(rename = "urlElicitation")]
        url_elicitation: Option<Value>,
        #[serde(default)]
        cancelled: bool,
    },
    ConfigureExternalTools {
        tools: Vec<rpi_tools::tools::external::ExternalToolDefinition>,
    },
    ExternalToolResponse {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    Ping,
    Stop,
}

pub const SUPPORTED_COMMANDS: &[&str] = &[
    "sample",
    "cancel_sample",
    "get_state",
    "set_system_prompt",
    "get_commands",
    "execute_command",
    "get_messages",
    "get_history",
    "get_session_branch",
    "append_entry",
    "get_stats",
    "get_context_usage",
    "set_compaction_enabled",
    "get_modes",
    "set_mode",
    "get_queue",
    "queue_message",
    "cancel_queued",
    "clear_queue",
    "navigate",
    "prompt",
    "resume_pending",
    "compact",
    "abort",
    "get_available_models",
    "get_auth_status",
    "oauth",
    "set_api_key",
    "set_model",
    "get_available_thinking_levels",
    "set_thinking_level",
    "get_available_tools",
    "set_active_tools",
    "configure_external_tools",
    "external_tool_response",
    "client_request",
    "ui_dialog_response",
    "ping",
    "stop",
];

pub fn response(id: Option<Value>, command: &str, result: Result<Option<Value>, String>) -> Value {
    let mut out = json!({
        "type": "response",
        "command": command,
        "success": result.is_ok(),
        // Keep the original remote-client response fields during the RPC
        // protocol transition. New clients use success/data; older clients
        // still require status/result to deserialize the response.
        "status": if result.is_ok() { "ok" } else { "error" },
    });
    if let Some(id) = id {
        out["id"] = id;
    }
    match result {
        Ok(Some(data)) => {
            out["data"] = data.clone();
            out["result"] = data;
        }
        Ok(None) => {}
        Err(error) => out["error"] = json!(error),
    }
    out
}

pub fn ready() -> Value {
    json!({
        "type": "ready",
        "protocol": "rpi",
        "promptContentVersion": 1,
        "piCompatibilityTarget": PI_BASELINE,
        "supportedCommands": SUPPORTED_COMMANDS,
        "supportedEvents": ["session_entry_committed", "compaction_started", "compaction_finished"],
        "semantics": {
            "promptResponse": "accepted",
            "settlementEvent": "agent_settled",
            "agentEndIsTerminal": false
        },
        "limits": {"frameBytes": MAX_FRAME_BYTES, "maxConcurrentPrompts": 1},
        "parityComplete": false
    })
}

/// Only expose capability metadata, never provider credentials or headers.
pub fn model_descriptor(model: &rpi_ai::Model) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "api": model.api,
        "provider": model.provider,
        "reasoning": model.reasoning,
        "input": model.input,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_commands_are_unique() {
        let commands: std::collections::HashSet<_> = SUPPORTED_COMMANDS.iter().collect();
        assert_eq!(commands.len(), SUPPORTED_COMMANDS.len());
    }

    #[test]
    fn extension_commands_are_advertised_and_args_default_to_empty() {
        for name in ["get_commands", "execute_command"] {
            assert!(SUPPORTED_COMMANDS.contains(&name));
        }
        let request: Request = serde_json::from_value(json!({
            "type": "execute_command", "command": "/example"
        }))
        .unwrap();
        assert!(
            matches!(request.command, Command::ExecuteCommand { command, args }
            if command == "/example" && args.is_empty())
        );
        assert!(serde_json::from_value::<Request>(json!({
            "type": "execute_command", "command": "example", "args": 42
        }))
        .is_err());
    }

    #[test]
    fn ready_advertises_the_p0_contract() {
        let value = ready();
        assert_eq!(value["protocol"], "rpi");
        assert_eq!(value["semantics"]["settlementEvent"], "agent_settled");
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "prompt"));
    }

    #[test]
    fn ready_advertises_the_read_only_history_snapshot() {
        let value = ready();
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "get_messages"));
    }

    #[test]
    fn ready_advertises_resuming_an_interrupted_run() {
        let value = ready();
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "resume_pending"));
    }

    #[test]
    fn ready_advertises_compaction() {
        let value = ready();
        assert!(value["supportedCommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "compact"));
    }

    #[test]
    fn compact_payload_carries_instructions() {
        let request: Request = serde_json::from_value(json!({
            "type": "compact", "id": 5, "customInstructions": "keep the decisions"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::Compact {
                custom_instructions: Some(_)
            }
        ));
    }

    #[test]
    fn resume_pending_is_a_bare_command() {
        let request: Request =
            serde_json::from_value(json!({"type": "resume_pending", "id": 4})).unwrap();
        assert!(matches!(request.command, Command::ResumePending));
    }

    #[test]
    fn get_messages_payload_keeps_its_limit_and_cursor() {
        let request: Request = serde_json::from_value(json!({
            "type": "get_messages", "id": 3, "limit": 25, "beforeId": "entry-12"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::GetMessages {
                limit: Some(25),
                before_id: Some(_)
            }
        ));
    }

    #[test]
    fn client_request_payload_is_native_and_correlated() {
        assert!(SUPPORTED_COMMANDS.contains(&"client_request"));
        let request: Request = serde_json::from_value(json!({
            "type": "client_request",
            "id": 9,
            "method": "fs/read_text_file",
            "params": {"path": "C:/workspace/README.md"}
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::ClientRequest { method, params }
                if method == "fs/read_text_file" && params["path"] == "C:/workspace/README.md"
        ));
    }
    #[test]
    fn auth_commands_keep_secret_fields_in_request_only() {
        let request: Request = serde_json::from_value(json!({
            "type": "set_api_key",
            "id": 8,
            "provider": "anthropic",
            "apiKey": "secret-fixture"
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::SetApiKey { provider, api_key }
                if provider == "anthropic" && api_key == "secret-fixture"
        ));
        let status = response(
            Some(json!(8)),
            "set_api_key",
            Ok(Some(json!({
                "provider": "anthropic",
                "ready": true,
                "source": "auth.json",
                "requiresRestart": true
            }))),
        );
        assert!(!status.to_string().contains("secret-fixture"));
    }
    #[test]
    fn prompt_payload_accepts_legacy_content_and_prompt_fields() {
        for field in ["message", "content", "prompt"] {
            let request: Request = serde_json::from_value(json!({
                "type": "prompt",
                field: "legacy text"
            }))
            .unwrap();
            match request.command {
                Command::Prompt { message: Some(message), .. } => assert_eq!(message, "legacy text"),
                Command::Prompt { content: Some(PromptContent::Legacy(text)), .. } => assert_eq!(text, "legacy text"),
                other => panic!("legacy prompt not preserved: {other:?}"),
            }
            assert!(ordered_content(&json!({"type":"prompt", field:"legacy text"})).unwrap().is_none());
        }
    }

    #[test]
    fn ordered_prompt_rejects_every_nonempty_legacy_alias() {
        for (key, value) in [
            ("message", json!("legacy")), ("prompt", json!("legacy")),
            ("images", json!([{"data":"AA==","mimeType":"image/png"}])),
            ("audio", json!([{"data":"AA==","mimeType":"audio/wav"}])),
        ] {
            let mut request = json!({"content":[{"type":"text","text":"ordered"}]});
            request[key] = value;
            assert!(ordered_content(&request).unwrap_err().contains("nonempty legacy"), "{key}");
        }
        let hidden = json!({"message":"", "prompt":"hidden", "content":[]});
        assert!(ordered_content(&hidden).is_err());
        assert!(ordered_content(&json!({"message":"", "prompt":"", "images":[], "audio":[],
            "content":[{"type":"text","text":"ordered"}]})).unwrap().is_some());
        // Wrong-type attachments are left for the dispatcher's strict media
        // decoder, not interpreted as an empty array or silently accepted there.
        assert!(ordered_content(&json!({"images":{}, "content":[]})).unwrap().is_some());
    }

    #[test]
    fn ordered_prompt_parts_are_strict_bounded_and_keep_order() {
        let parts = json!([
            {"type":"text","text":"before"},
            {"type":"image","data":"AA==","mimeType":"image/png"},
            {"type":"text","text":"between"},
            {"type":"audio","data":"AA==","mimeType":"audio/wav"},
            {"type":"text","text":"after"}
        ]);
        let request: Request = serde_json::from_value(json!({"type":"prompt","content":parts})).unwrap();
        assert!(matches!(request.command, Command::Prompt { content: Some(PromptContent::Ordered(parts)), .. } if parts.len() == 5));
        let decoded = ordered_content(&json!({"content":parts})).unwrap().unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), parts);
        for part in [
            json!({"type":"assistant","text":"no"}),
            json!({"type":"tool_result","text":"no"}),
            json!({"type":"text","text":"no","role":"assistant"}),
            json!({"type":"text","text":"no","unknown":true}),
        ] {
            let input = json!({"type":"prompt","content":[part]});
            assert!(serde_json::from_value::<Request>(input.clone()).is_err());
            assert!(ordered_content(&input).is_err());
        }
        assert!(ordered_content(&json!({"content":vec![json!({"type":"text","text":"x"});257]})).is_err());
        assert!(ordered_content(&json!({"content":[{"type":"text","text":"x".repeat(900_000)}]})).is_err());
        assert!(ordered_content(&json!({"content":"x".repeat(900_001)})).unwrap().is_none());
    }

    #[test]
    fn oauth_payload_keeps_provider_action_and_input() {
        let request: Request = serde_json::from_value(json!({
            "type": "oauth",
            "id": 9,
            "provider": "example",
            "action": "begin",
            "input": {"code": "fixture"}
        }))
        .unwrap();
        assert!(matches!(
            request.command,
            Command::OAuth { provider, action, input: Some(input) }
                if provider == "example"
                    && action == "begin"
                    && input["code"] == "fixture"
        ));
    }

    #[test]
    fn response_has_canonical_success_data_shape() {
        let value = response(
            Some(json!(7)),
            "prompt",
            Ok(Some(json!({"accepted": true}))),
        );
        assert_eq!(value["type"], "response");
        assert_eq!(value["success"], true);
        assert_eq!(value["status"], "ok");
        assert_eq!(value["data"]["accepted"], true);
        assert_eq!(value["result"]["accepted"], true);
        let legacy: crate::remote::protocol::RemoteResponse =
            serde_json::from_value(value).unwrap();
        assert!(legacy.is_ok());
        assert_eq!(legacy.result.unwrap()["accepted"], true);
    }
}
