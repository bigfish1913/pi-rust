//! Minimal P0 JSONL protocol for the native rpi RPC host.
use serde_json::{json, Value};

pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const PI_BASELINE: &str = "1.2.0";

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
        message: String,
    },
    Abort,
    GetState,
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
    UiDialogResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        answer: Option<Value>,
        #[serde(default)]
        cancelled: bool,
    },
    Ping,
    Stop,
}

pub const SUPPORTED_COMMANDS: &[&str] = &[
    "get_state",
    "get_messages",
    "get_history",
    "get_stats",
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
    "ui_dialog_response",
    "ping",
    "stop",
];

pub fn response(id: Option<Value>, command: &str, result: Result<Option<Value>, String>) -> Value {
    let mut out = json!({
        "type": "response",
        "command": command,
        "success": result.is_ok(),
    });
    if let Some(id) = id {
        out["id"] = id;
    }
    match result {
        Ok(Some(data)) => out["data"] = data,
        Ok(None) => {}
        Err(error) => out["error"] = json!(error),
    }
    out
}

pub fn ready() -> Value {
    json!({
        "type": "ready",
        "protocol": "rpi",
        "piCompatibilityTarget": PI_BASELINE,
        "supportedCommands": SUPPORTED_COMMANDS,
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
    fn response_has_canonical_success_data_shape() {
        let value = response(
            Some(json!(7)),
            "prompt",
            Ok(Some(json!({"accepted": true}))),
        );
        assert_eq!(value["type"], "response");
        assert_eq!(value["success"], true);
        assert_eq!(value["data"]["accepted"], true);
    }
}
