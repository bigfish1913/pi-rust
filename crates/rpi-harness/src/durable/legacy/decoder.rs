//! Shared, protocol-neutral durable state primitives.
//!
//! This crate deliberately owns no session, storage, worker, or network code.
//! It owns only the canonical entry discriminator, the bounded projection model,
//! and replay of `rpi-durable` journal records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ENTRY_TYPE: &str = "durable-state";
pub const JOURNAL_FORMAT: &str = "rpi-durable-journal";
pub const JOURNAL_VERSION: u64 = 1;
pub const MAX_QUEUE_ITEMS: usize = 32;
pub const MAX_TOOL_ENTRIES: usize = 64;
pub const MAX_SUMMARY_CHARS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableMutation {
    StartRun,
    SettleRun,
    RecoverRun,
    Enqueue {
        text: String,
    },
    Claim {
        id: String,
    },
    Finish {
        id: String,
        status: QueueStatus,
        note: String,
    },
    Cancel {
        id: String,
        note: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub name: String,
    pub status: WorkflowStatus,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Running,
    Paused,
    Completed,
    Failed,
}

impl WorkflowStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: String,
    pub title: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

impl StepStatus {
    pub fn marker(&self) -> &'static str {
        match self {
            Self::Pending => " ",
            Self::InProgress => "•",
            Self::Completed => "✓",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub next_run_id: u64,
    #[serde(default)]
    pub next_queue_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Workflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunCheckpoint>,
    #[serde(default)]
    pub queue: Vec<QueueItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Settled,
    Failed,
    Interrupted,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Settled => "settled",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Started,
    Completed,
    Failed,
    Interrupted,
    Unknown,
}

impl ToolStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCheckpoint {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_key: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub params: String,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCheckpoint {
    pub id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub started_revision: u64,
    #[serde(default)]
    pub updated_revision: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recovery_of: String,
    #[serde(default)]
    pub tools: Vec<ToolCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Queued,
    Claimed,
    Completed,
    Failed,
    Cancelled,
}

impl QueueStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub status: QueueStatus,
    #[serde(default)]
    pub created_revision: u64,
    #[serde(default)]
    pub updated_revision: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl DurableState {
    /// Decode a filtered canonical journal, failing closed on malformed envelopes and suffixes.
    pub fn try_from_branch(entries: &[Value]) -> Result<Self, String> {
        replay_entries(entries)?
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| format!("decode durable state: {error}"))
            })
            .transpose()
            .map(|state| state.unwrap_or_default())
    }
}

/// Replay entries from a native branch query filtered to [`ENTRY_TYPE`].
/// Native envelopes are always validated; only obsolete payload errors before
/// the newest valid, self-contained snapshot may be ignored.
pub fn replay_entries(entries: &[Value]) -> Result<Option<Value>, String> {
    let mut records = Vec::new();
    let mut invalid = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if !is_canonical_entry(entry) {
            return Err("filtered durable branch contains an unexpected entry type".into());
        }
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("filtered durable branch entry has no valid native id")?;
        let data = entry
            .get("data")
            .ok_or("durable custom entry has no data")?;
        let parsed = (|| {
            let record: JournalRecord = serde_json::from_value(data.clone())
                .map_err(|error| format!("invalid durable journal record: {error}"))?;
            if record.format != JOURNAL_FORMAT || record.version != JOURNAL_VERSION {
                return Err("unsupported durable journal format or version".into());
            }
            if record.revision < record.base_revision {
                return Err("durable journal revision moves backwards".into());
            }
            match record.kind.as_str() {
                "snapshot" => {
                    if record.changes.is_some() {
                        return Err("durable snapshot must contain only a complete state".into());
                    }
                    let state = record
                        .state
                        .as_ref()
                        .ok_or("durable snapshot has no state")?;
                    if state.revision != record.revision {
                        return Err("durable snapshot state has the wrong revision".into());
                    }
                    validate_state(state)?;
                }
                "delta" => {
                    if record.state.is_some() || record.changes.as_ref().map_or(true, Vec::is_empty)
                    {
                        return Err("durable delta must contain non-empty changes only".into());
                    }
                }
                kind => return Err(format!("unsupported durable journal kind: {kind}")),
            }
            Ok((record, id.to_owned()))
        })();
        match parsed {
            Ok((record, id)) => records.push((index, record, id)),
            Err(error) => invalid.push((index, error)),
        }
    }
    // A validated complete snapshot is self-contained. Obsolete malformed
    // prefixes can be discarded, but never a malformed record in its suffix.
    let snapshot = records
        .iter()
        .rposition(|(_, record, _)| record.kind == "snapshot");
    let Some(snapshot) = snapshot else {
        if let Some((_, error)) = invalid.first() {
            return Err(error.clone());
        }
        return if records.is_empty() {
            Ok(None)
        } else {
            Err("durable journal has no self-contained snapshot base".into())
        };
    };
    let (index, record, id) = &records[snapshot];
    if let Some((_, error)) = invalid
        .iter()
        .find(|(invalid_index, _)| invalid_index > index)
    {
        return Err(error.clone());
    }
    let mut state = record
        .state
        .clone()
        .ok_or("durable snapshot has no state")?;
    let mut anchor = id;
    for (_, record, id) in records.iter().skip(snapshot + 1) {
        if record.base_revision != state.revision || record.base_entry_id.as_ref() != Some(anchor) {
            return Err("durable journal delta has a gap or wrong branch anchor".into());
        }
        if record.revision
            != state
                .revision
                .checked_add(1)
                .ok_or("durable revision overflow")?
        {
            return Err("durable journal delta has a non-contiguous revision".into());
        }
        for change in record
            .changes
            .as_ref()
            .ok_or("durable delta has no changes")?
        {
            apply_change(&mut state, change)?;
        }
        state.revision = record.revision;
        validate_state(&state)?;
        anchor = id;
    }
    serde_json::to_value(state)
        .map(Some)
        .map_err(|error| format!("encode durable state: {error}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JournalRecord {
    format: String,
    version: u64,
    kind: String,
    base_revision: u64,
    revision: u64,
    #[serde(default)]
    base_entry_id: Option<String>,
    #[serde(default)]
    state: Option<DurableState>,
    #[serde(default)]
    changes: Option<Vec<JournalChange>>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum JournalChange {
    SetWorkflow {
        workflow: Option<Workflow>,
    },
    UpdateStep {
        step_id: String,
        status: StepStatus,
        note: Option<String>,
    },
    SetWorkflowMeta {
        name: String,
        status: WorkflowStatus,
        reason: Option<String>,
    },
    SetCounters {
        next_run_id: u64,
        next_queue_id: u64,
    },
    SetRun {
        run: Option<RunCheckpoint>,
    },
    SetRunMeta {
        id: String,
        status: RunStatus,
        started_revision: u64,
        updated_revision: u64,
        outcome: String,
        recovery_of: String,
    },
    AppendTool {
        tool: ToolCheckpoint,
    },
    UpdateTool {
        call_id: String,
        #[serde(default)]
        call_key: Option<String>,
        status: ToolStatus,
        result: String,
    },
    SetTools {
        tools: Vec<ToolCheckpoint>,
    },
    AppendQueue {
        item: QueueItem,
    },
    UpdateQueue {
        item: QueueItem,
    },
    SetQueue {
        queue: Vec<QueueItem>,
    },
}

fn apply_change(state: &mut DurableState, change: &JournalChange) -> Result<(), String> {
    match change {
        JournalChange::SetWorkflow { workflow } => state.workflow = workflow.clone(),
        JournalChange::SetWorkflowMeta {
            name,
            status,
            reason,
        } => {
            let workflow = state
                .workflow
                .as_mut()
                .ok_or("durable journal workflow metadata path has no workflow")?;
            workflow.name = name.clone();
            workflow.status = status.clone();
            workflow.reason = reason.clone();
        }
        JournalChange::UpdateStep {
            step_id,
            status,
            note,
        } => {
            let workflow = state
                .workflow
                .as_mut()
                .ok_or("durable journal step path has no workflow")?;
            let step = workflow
                .steps
                .iter_mut()
                .find(|step| step.id == *step_id)
                .ok_or_else(|| format!("durable journal step path not found: {step_id}"))?;
            step.status = status.clone();
            step.note = note.clone();
        }
        JournalChange::SetCounters {
            next_run_id,
            next_queue_id,
        } => {
            state.next_run_id = *next_run_id;
            state.next_queue_id = *next_queue_id;
        }
        JournalChange::SetRun { run } => state.run = run.clone(),
        JournalChange::SetRunMeta {
            id,
            status,
            started_revision,
            updated_revision,
            outcome,
            recovery_of,
        } => {
            let run = state
                .run
                .as_mut()
                .ok_or("durable journal run metadata path has no run")?;
            run.id = id.clone();
            run.status = status.clone();
            run.started_revision = *started_revision;
            run.updated_revision = *updated_revision;
            run.outcome = outcome.clone();
            run.recovery_of = recovery_of.clone();
        }
        JournalChange::AppendTool { tool } => {
            let run = state
                .run
                .as_mut()
                .ok_or("durable journal tool path has no run")?;
            run.tools.push(tool.clone());
            if run.tools.len() > MAX_TOOL_ENTRIES {
                run.tools.drain(..run.tools.len() - MAX_TOOL_ENTRIES);
            }
        }
        JournalChange::UpdateTool {
            call_id,
            call_key,
            status,
            result,
        } => {
            let run = state
                .run
                .as_mut()
                .ok_or("durable journal tool path has no run")?;
            let mut count = 0;
            for tool in run.tools.iter_mut().filter(|tool| {
                tool.call_key == *call_key && (call_key.is_some() || tool.call_id == *call_id)
            }) {
                tool.status = status.clone();
                tool.result = result.clone();
                count += 1;
            }
            if count != 1 {
                return Err(format!(
                    "durable journal tool path matched {count} entries for {call_id}"
                ));
            }
        }
        JournalChange::SetTools { tools } => {
            if tools.len() > MAX_TOOL_ENTRIES {
                return Err("durable journal tool replacement exceeds its bound".into());
            }
            state
                .run
                .as_mut()
                .ok_or("durable journal tool path has no run")?
                .tools = tools.clone();
        }
        JournalChange::AppendQueue { item } => {
            if state.queue.len() >= MAX_QUEUE_ITEMS
                || state.queue.iter().any(|existing| existing.id == item.id)
            {
                return Err("durable journal queue append is invalid".into());
            }
            state.queue.push(item.clone());
        }
        JournalChange::UpdateQueue { item } => {
            let existing = state
                .queue
                .iter_mut()
                .find(|existing| existing.id == item.id)
                .ok_or_else(|| format!("durable journal queue path not found: {}", item.id))?;
            *existing = item.clone();
        }
        JournalChange::SetQueue { queue } => {
            if queue.len() > MAX_QUEUE_ITEMS {
                return Err("durable journal queue replacement exceeds its bound".into());
            }
            state.queue = queue.clone();
        }
    }
    validate_state(state)
}

fn validate_state(state: &DurableState) -> Result<(), String> {
    if let Some(workflow) = &state.workflow {
        if workflow.name.trim().is_empty()
            || workflow.name.chars().count() > 500
            || workflow.steps.is_empty()
            || workflow.steps.len() > 128
        {
            return Err("durable journal workflow exceeds its bounds".into());
        }
        for (index, step) in workflow.steps.iter().enumerate() {
            if step.id.is_empty()
                || step.id.chars().count() > 128
                || step.title.chars().count() > 1_000
                || step
                    .note
                    .as_ref()
                    .is_some_and(|note| note.chars().count() > 4_000)
                || workflow.steps[..index]
                    .iter()
                    .any(|other| other.id == step.id)
            {
                return Err(
                    "durable journal contains an invalid or duplicate workflow step".into(),
                );
            }
        }
    }
    if let Some(run) = &state.run {
        if run.id.trim().is_empty()
            || run.id.chars().count() > 128
            || run.tools.len() > MAX_TOOL_ENTRIES
        {
            return Err("durable journal contains an invalid run or too many tool entries".into());
        }
        for tool in &run.tools {
            if tool.call_id.chars().count() > MAX_SUMMARY_CHARS + 1
                || tool.call_key.as_ref().is_some_and(|key| {
                    key.is_empty() || key.chars().count() > MAX_SUMMARY_CHARS + 8
                })
                || tool.name.chars().count() > MAX_SUMMARY_CHARS + 1
                || tool.params.chars().count() > MAX_SUMMARY_CHARS + 1
                || tool.result.chars().count() > MAX_SUMMARY_CHARS + 1
            {
                return Err("durable journal contains an oversized tool summary".into());
            }
        }
    }
    if state.queue.len() > MAX_QUEUE_ITEMS {
        return Err("durable journal contains an invalid queue".into());
    }
    for (index, item) in state.queue.iter().enumerate() {
        if item.id.trim().is_empty()
            || item.id.chars().count() > 128
            || item.text.chars().count() > 4_000
            || item.note.chars().count() > 4_000
            || state.queue[..index].iter().any(|other| other.id == item.id)
        {
            return Err("durable journal contains an invalid or duplicate queue item".into());
        }
    }
    Ok(())
}

pub fn is_canonical_entry(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("custom")
        && entry.get("customType").and_then(Value::as_str) == Some(ENTRY_TYPE)
}
