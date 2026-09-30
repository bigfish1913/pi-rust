//! Mirrors `packages/agent/src/harness/messages.ts` — the harness's custom-role
//! `AgentMessage` constructors and the harness-level `convert_to_llm`.
//!
//! TS extends the agent's `AgentMessage` via declaration merging on
//! `CustomAgentMessages` with four roles: `bashExecution`, `custom`,
//! `branchSummary`, `compactionSummary`. Rust has no declaration merging, so
//! each role is a `rpi_agent::message::AgentMessage::Custom(CustomMessage)`
//! whose `role` field carries the TS role string and whose `data: Value` carries
//! the role-specific structured payload (mirrors the TS role interface fields).
//!
//! `convert_to_llm` is the *harness* converter (distinct from
//! `rpi_agent::default_convert_to_llm`, which drops custom roles): it projects
//! each custom role into a provider-facing `rpi_ai::Message` (a `User` message
//! with role-rendered text), so the LLM sees bash output / branch + compaction
//! summaries as user text. Unregistered custom roles are dropped (mirrors the
//! TS `default → undefined` branch).
//!
//! Pulled forward from M5e because M5d compaction depends on it
//! (`createCompactionSummaryMessage`, `createBranchSummaryMessage`,
//! `convertToLlm`).

use rpi_agent::message::{AgentMessage, CustomMessage};
use rpi_ai::types::{Content, Message, UserContent, UserMessage};
use serde::{Deserialize, Serialize};

/// Wraps `summary` in the compaction-summary text envelope. Mirrors
/// `COMPACTION_SUMMARY_PREFIX` / `SUFFIX`.
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Wraps `summary` in the branch-summary text envelope. Mirrors
/// `BRANCH_SUMMARY_PREFIX` / `SUFFIX`.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

// ---------------------------------------------------------------------------
// Role tag constants
// ---------------------------------------------------------------------------

pub const BASH_EXECUTION_ROLE: &str = "bashExecution";
pub const CUSTOM_ROLE: &str = "custom";
pub const BRANCH_SUMMARY_ROLE: &str = "branchSummary";
pub const COMPACTION_SUMMARY_ROLE: &str = "compactionSummary";

// ---------------------------------------------------------------------------
// Per-role structured payloads (stored on `CustomMessage::data`)
// ---------------------------------------------------------------------------

/// `BashExecutionMessage` payload. Mirrors the TS interface fields (minus
/// `role`/`timestamp`, which live on `CustomMessage` itself).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionData {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// `BranchSummaryMessage` payload. Mirrors `BranchSummaryMessage`
/// (`{summary, fromId}`, the role/timestamp live on `CustomMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryData {
    pub summary: String,
    pub from_id: String,
}

/// `CompactionSummaryMessage` payload. Mirrors `CompactionSummaryMessage`
/// (`{summary, tokensBefore}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryData {
    pub summary: String,
    pub tokens_before: i64,
}

/// `CustomMessage<T>` payload. Mirrors `CustomMessage` (`{customType, content,
/// display, details}`) for the generic `custom` role. `content` lives on
/// `CustomMessage::content`, so the data payload carries the metadata fields
/// only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomData {
    pub custom_type: String,
    #[serde(default = "default_true")]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Constructors — mirror createBashExecutionMessage / createBranchSummaryMessage
// / createCompactionSummaryMessage / createCustomMessage
// ---------------------------------------------------------------------------

/// Build a `bashExecution` custom message. Mirrors the TS constructor; `content`
/// is empty (rendered from `data` at convert time, matches TS where
/// `convertToLlm` calls `bashExecutionToText`).
pub fn bash_execution_message(
    command: impl Into<String>,
    output: impl Into<String>,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    exclude_from_context: Option<bool>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BashExecutionData {
        command: command.into(),
        output: output.into(),
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        exclude_from_context,
    })
    .expect("BashExecutionData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BASH_EXECUTION_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createBranchSummaryMessage(summary, fromId, timestamp)`. Mirrors the TS
/// constructor; `content` is empty (rendered from `data.summary` at convert
/// time).
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BranchSummaryData {
        summary: summary.into(),
        from_id: from_id.into(),
    })
    .expect("BranchSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BRANCH_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCompactionSummaryMessage(summary, tokensBefore, timestamp)`. Mirrors
/// the TS constructor; `content` is empty (rendered from `data.summary` at
/// convert time).
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: i64,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(CompactionSummaryData {
        summary: summary.into(),
        tokens_before,
    })
    .expect("CompactionSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: COMPACTION_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCustomMessage(customType, content, display, details, timestamp)`.
/// Mirrors the TS constructor; `content` (string or text/image blocks) is
/// normalized to `Vec<Content>` on the stored `CustomMessage`.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: UserContent,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: i64,
) -> AgentMessage {
    let blocks = match content {
        UserContent::Text(s) => vec![Content::text(s)],
        UserContent::Blocks(b) => b,
    };
    let data = serde_json::to_value(CustomData {
        custom_type: custom_type.into(),
        display,
        details,
    })
    .expect("CustomData serializes");
    AgentMessage::Custom(CustomMessage {
        role: CUSTOM_ROLE.into(),
        content: blocks,
        data,
        timestamp,
    })
}

// ---------------------------------------------------------------------------
// Role-data accessors (used by compaction's estimate_tokens / convert_to_llm)
// ---------------------------------------------------------------------------

/// If `msg` is a `compactionSummary` custom message, deserialize its `data`.
pub fn compaction_summary_data(msg: &AgentMessage) -> Option<CompactionSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == COMPACTION_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `branchSummary` custom message, deserialize its `data`.
pub fn branch_summary_data(msg: &AgentMessage) -> Option<BranchSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BRANCH_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `bashExecution` custom message, deserialize its `data`.
pub fn bash_execution_data(msg: &AgentMessage) -> Option<BashExecutionData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BASH_EXECUTION_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a generic `custom` role message, deserialize its `data`.
pub fn custom_data(msg: &AgentMessage) -> Option<CustomData> {
    match msg {
        AgentMessage::Custom(c) if c.role == CUSTOM_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// bashExecutionToText — mirrors the TS renderer
// ---------------------------------------------------------------------------

/// Render a `bashExecution` message to its user-visible text. Mirrors
/// `bashExecutionToText`.
pub fn bash_execution_to_text(data: &BashExecutionData) -> String {
    let mut text = format!("Ran `{}`\n", data.command);
    if !data.output.is_empty() {
        text += &format!("```\n{}\n```", data.output);
    } else {
        text += "(no output)";
    }
    if data.cancelled {
        text += "\n\n(command cancelled)";
    } else if let Some(code) = data.exit_code {
        if code != 0 {
            text += &format!("\n\nCommand exited with code {code}");
        }
    }
    if data.truncated {
        if let Some(full) = &data.full_output_path {
            text += &format!("\n\n[Output truncated. Full output: {full}]");
        }
    }
    text
}

// ---------------------------------------------------------------------------
// convertToLlm — the harness-level AgentMessage[] -> Message[] converter
// ---------------------------------------------------------------------------

/// The harness converter. Mirrors TS `convertToLlm`: drops `bashExecution` when
/// `excludeFromContext`; renders `bashExecution`/`branchSummary`/`compactionSummary`
/// as `User` text messages; passes through `user`/`assistant`/`toolResult`;
/// drops unregistered custom roles.
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::User(u) => Some(Message::User(u)),
            AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
            AgentMessage::ToolResult(t) => Some(Message::ToolResult(t)),
            AgentMessage::Custom(c) => convert_custom_to_llm(c),
        })
        .collect()
}

fn convert_custom_to_llm(c: CustomMessage) -> Option<Message> {
    let timestamp = c.timestamp;
    match c.role.as_str() {
        BASH_EXECUTION_ROLE => {
            let data: BashExecutionData = serde_json::from_value(c.data).ok()?;
            if data.exclude_from_context.unwrap_or(false) {
                return None;
            }
            let text = bash_execution_to_text(&data);
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        CUSTOM_ROLE => {
            // Generic custom: content is the stored text/image blocks.
            Some(Message::User(UserMessage::new(
                UserContent::Blocks(c.content),
                timestamp,
            )))
        }
        BRANCH_SUMMARY_ROLE => {
            let data: BranchSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        COMPACTION_SUMMARY_ROLE => {
            let data: CompactionSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        // Unregistered custom role → drop (mirrors TS `default → undefined`).
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_summary_round_trips_data() {
        let m = create_compaction_summary_message("sum", 42, 100);
        let d = compaction_summary_data(&m).unwrap();
        assert_eq!(d.summary, "sum");
        assert_eq!(d.tokens_before, 42);
    }

    #[test]
    fn branch_summary_round_trips_data() {
        let m = create_branch_summary_message("s", "from-1", 7);
        let d = branch_summary_data(&m).unwrap();
        assert_eq!(d.summary, "s");
        assert_eq!(d.from_id, "from-1");
    }

    #[test]
    fn convert_to_llm_renders_compaction_summary_as_user_text() {
        let m = create_compaction_summary_message("hello", 10, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("hello"));
                    assert!(s.contains("<summary>"));
                }
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn convert_to_llm_drops_bash_when_excluded() {
        let m = bash_execution_message("ls", "out", None, false, false, None, Some(true), 1);
        assert!(convert_to_llm(vec![m]).is_empty());
    }

    #[test]
    fn convert_to_llm_renders_bash_execution_to_text() {
        let m = bash_execution_message("echo hi", "hi", Some(0), false, false, None, None, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("Ran `echo hi`"));
                    assert!(s.contains("hi"));
                }
                _ => panic!("expected text"),
            },
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn convert_to_llm_passes_through_base_roles() {
        let u = AgentMessage::User(UserMessage::new("hi", 1));
        let out = convert_to_llm(vec![u]);
        assert!(matches!(out[0], Message::User(_)));
    }

    #[test]
    fn convert_to_llm_drops_unregistered_custom_role() {
        let c = CustomMessage::new("weird".to_string(), Vec::new(), serde_json::Value::Null, 1);
        let out = convert_to_llm(vec![AgentMessage::Custom(c)]);
        assert!(out.is_empty());
    }
}
