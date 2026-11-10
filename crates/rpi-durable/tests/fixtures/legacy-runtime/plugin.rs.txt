#![forbid(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! Durable, session-scoped workflow checkpoints for rpi.
//!
//! The native session branch is the source of truth. This extension only keeps
//! a decoded copy in memory and appends a compact custom entry after each
//! mutation; it does not create a second transcript or project database.

use rpi_durable_core::{
    DurableState, QueueItem, QueueStatus, RunCheckpoint, RunStatus, Step, StepStatus,
    ToolCheckpoint, ToolStatus, Workflow, WorkflowStatus, MAX_QUEUE_ITEMS, MAX_SUMMARY_CHARS,
};
use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    EventHandlerFn, EventTag, FreeStringFn, PluginApi, RuntimeActionId, StableToolSchema,
    StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

const TOOL: &str = "durable";
const ENTRY_TYPE: &str = rpi_durable_core::ENTRY_TYPE;
const MAX_STEPS: usize = 128;
const MAX_NAME_CHARS: usize = 500;
const MAX_STEP_ID_CHARS: usize = 128;
const MAX_STEP_TITLE_CHARS: usize = 1_000;
const MAX_NOTE_CHARS: usize = 4_000;
const MAX_JOURNAL_ENTRIES: usize = 64;
const TOOL_ID_HASH_HEX_CHARS: usize = 64;
const MAX_QUEUE_TEXT_CHARS: usize = 4_000;
const JOURNAL_FORMAT: &str = rpi_durable_core::JOURNAL_FORMAT;
const JOURNAL_VERSION: u64 = rpi_durable_core::JOURNAL_VERSION;
const SNAPSHOT_INTERVAL: u64 = 32;

type Runtime = PluginApi;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static STATE: OnceLock<Mutex<Store>> = OnceLock::new();
static RESTORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn restore_lock() -> &'static Mutex<()> {
    RESTORE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct JournalMeta {
    anchor_entry_id: Option<String>,
    commits_since_snapshot: u64,
    force_snapshot: bool,
}

impl Default for JournalMeta {
    fn default() -> Self {
        Self {
            anchor_entry_id: None,
            commits_since_snapshot: 0,
            force_snapshot: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Store {
    state: DurableState,
    meta: JournalMeta,
    initialized: bool,
    restore_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JournalRecord {
    format: String,
    version: u64,
    kind: JournalKind,
    base_revision: u64,
    revision: u64,
    #[serde(default)]
    base_entry_id: Option<String>,
    #[serde(default)]
    state: Option<DurableState>,
    #[serde(default)]
    changes: Option<Vec<JournalChange>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JournalKind {
    Snapshot,
    Delta,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
        run: Option<RunState>,
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
        tool: ToolExecution,
    },
    UpdateTool {
        call_id: String,
        #[serde(default)]
        call_key: Option<String>,
        status: ToolExecutionStatus,
        result: String,
    },
    SetTools {
        tools: Vec<ToolExecution>,
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

fn state() -> &'static Mutex<Store> {
    STATE.get_or_init(|| Mutex::new(Store::default()))
}

type RunState = RunCheckpoint;
type ToolExecution = ToolCheckpoint;
type ToolExecutionStatus = ToolStatus;
type QueueItemStatus = QueueStatus;

fn call_runtime(id: RuntimeActionId, args: Value) -> Result<Value, String> {
    let runtime = *RUNTIME.get().ok_or("durable runtime is unavailable")?;
    let input = args.to_string();
    let mut output = StbString::empty();
    let rc = (runtime.runtime_action)(
        id as u32,
        StbStringRef::from_str(&input),
        &mut output,
        runtime.user_data,
    );
    let text = output.to_string_lossy();
    (runtime.free_string)(output);
    if rc != 0 {
        return Err(if text.is_empty() {
            format!("runtime action {id:?} failed")
        } else {
            text
        });
    }
    if text.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(&text).map_err(|error| format!("invalid runtime response: {error}"))
    }
}

fn append_entry_identity(response: &Value) -> Option<String> {
    response.get("entryId").and_then(|value| match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

fn branch_entry_identity(entry: &Value) -> Option<String> {
    entry.get("id").and_then(|value| match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

fn append_entry(payload: Value) -> Result<String, String> {
    let response = call_runtime(RuntimeActionId::AppendEntry, payload).map_err(|error| {
        // A successful host call with an unreadable acknowledgement may have
        // committed already. Reconcile before using the previous anchor again.
        if error.starts_with("invalid runtime response:") {
            format!("durable append acknowledgement is unknown: {error}")
        } else {
            error
        }
    })?;
    append_entry_identity(&response).ok_or_else(|| {
        "durable append acknowledgement is unknown: AppendEntry returned no usable entryId; durable journal is unusable until the branch is explicitly reloaded".into()
    })
}

fn validate_state(value: &DurableState) -> Result<(), String> {
    if let Some(workflow) = &value.workflow {
        if workflow.name.trim().is_empty()
            || workflow.name.chars().count() > MAX_NAME_CHARS
            || workflow.steps.is_empty()
            || workflow.steps.len() > MAX_STEPS
        {
            return Err("durable journal workflow exceeds its bounds".into());
        }
        for step in &workflow.steps {
            if step.id.is_empty()
                || step.id.chars().count() > MAX_STEP_ID_CHARS
                || step.title.chars().count() > MAX_STEP_TITLE_CHARS
                || step
                    .note
                    .as_ref()
                    .is_some_and(|note| note.chars().count() > MAX_NOTE_CHARS)
            {
                return Err("durable journal contains an invalid workflow step".into());
            }
        }
        if workflow
            .steps
            .windows(2)
            .any(|steps| steps[0].id == steps[1].id)
            || workflow.steps.iter().enumerate().any(|(index, step)| {
                workflow.steps[..index]
                    .iter()
                    .any(|other| other.id == step.id)
            })
        {
            return Err("durable journal contains duplicate workflow step IDs".into());
        }
    }
    if let Some(run) = &value.run {
        if run.id.trim().is_empty()
            || run.id.chars().count() > MAX_STEP_ID_CHARS
            || run.tools.len() > MAX_JOURNAL_ENTRIES
        {
            return Err("durable journal contains too many tool entries".into());
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
    if value.queue.len() > MAX_QUEUE_ITEMS
        || value.queue.iter().any(|item| {
            item.id.trim().is_empty()
                || item.id.chars().count() > MAX_STEP_ID_CHARS
                || item.text.chars().count() > MAX_QUEUE_TEXT_CHARS
                || item.note.chars().count() > MAX_NOTE_CHARS
        })
    {
        return Err("durable journal contains an invalid queue".into());
    }
    if value
        .queue
        .iter()
        .enumerate()
        .any(|(index, item)| value.queue[..index].iter().any(|other| other.id == item.id))
    {
        return Err("durable journal contains duplicate queue item IDs".into());
    }
    Ok(())
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
            if run.tools.len() > MAX_JOURNAL_ENTRIES {
                run.tools.drain(..run.tools.len() - MAX_JOURNAL_ENTRIES);
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
            let matches = run.tools.iter_mut().filter(|tool| {
                tool.call_key == *call_key && (call_key.is_some() || tool.call_id == *call_id)
            });
            let mut count = 0;
            for tool in matches {
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
            if tools.len() > MAX_JOURNAL_ENTRIES {
                return Err("durable journal tool replacement exceeds its bound".into());
            }
            let run = state
                .run
                .as_mut()
                .ok_or("durable journal tool path has no run")?;
            run.tools = tools.clone();
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

fn apply_record(base: &DurableState, record: &JournalRecord) -> Result<DurableState, String> {
    if record.format != JOURNAL_FORMAT || record.version != JOURNAL_VERSION {
        return Err("unsupported durable journal format or version".into());
    }
    let next = match record.kind {
        JournalKind::Snapshot => {
            if record.changes.is_some() {
                return Err("durable snapshot contains delta changes".into());
            }
            record
                .state
                .clone()
                .ok_or("durable snapshot has no complete state")?
        }
        JournalKind::Delta => {
            if record.state.is_some() {
                return Err("durable delta contains a snapshot state".into());
            }
            let changes = record
                .changes
                .as_ref()
                .ok_or("durable delta has no changes")?;
            if changes.is_empty() {
                return Err("durable delta must contain a change".into());
            }
            let mut next = base.clone();
            for change in changes {
                apply_change(&mut next, change)?;
            }
            next.revision = record.revision;
            next
        }
    };
    if next.revision != record.revision {
        return Err("durable journal resulting revision does not match its state".into());
    }
    validate_state(&next)?;
    Ok(next)
}

#[derive(Clone, Debug)]
struct BranchRecord {
    record: JournalRecord,
    entry_id: String,
}

fn decode_branch(entries: &[Value]) -> Result<(DurableState, JournalMeta), String> {
    let mut records = Vec::new();
    let mut invalid_records = Vec::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        if entry.get("type").and_then(Value::as_str) != Some("custom")
            || entry.get("customType").and_then(Value::as_str) != Some(ENTRY_TYPE)
        {
            return Err("filtered durable branch contains an unexpected entry type".into());
        }
        let entry_id = branch_entry_identity(entry)
            .ok_or("filtered durable branch entry has no valid native id")?;
        let data = entry
            .get("data")
            .ok_or("durable custom entry has no data")?;
        let parsed = (|| -> Result<BranchRecord, String> {
            let record: JournalRecord = serde_json::from_value(data.clone())
                .map_err(|error| format!("invalid durable journal record: {error}"))?;
            if record.format != JOURNAL_FORMAT || record.version != JOURNAL_VERSION {
                return Err("unsupported durable journal format or version".into());
            }
            match record.kind {
                JournalKind::Snapshot => {
                    if record.state.is_none() || record.changes.is_some() {
                        return Err("durable snapshot must contain only a complete state".into());
                    }
                }
                JournalKind::Delta => {
                    if record.state.is_some() || record.changes.as_ref().map_or(true, Vec::is_empty)
                    {
                        return Err("durable delta must contain non-empty changes only".into());
                    }
                }
            }
            if record.revision < record.base_revision {
                return Err("durable journal revision moves backwards".into());
            }
            if matches!(record.kind, JournalKind::Snapshot) {
                let snapshot = record
                    .state
                    .as_ref()
                    .ok_or("durable snapshot has no complete state")?;
                if snapshot.revision != record.revision {
                    return Err("durable snapshot state has the wrong revision".into());
                }
                validate_state(snapshot)?;
            }
            Ok(BranchRecord { record, entry_id })
        })();
        match parsed {
            Ok(record) => records.push((entry_index, record)),
            Err(error) => invalid_records.push((entry_index, error)),
        }
    }

    // A complete snapshot is self-contained. Ignore malformed durable entries
    // before the newest valid snapshot, but fail closed for anything after it.
    // This keeps obsolete journal prefixes from making a later recovery base
    // unusable while preserving strict suffix validation.
    let latest_snapshot = records.iter().rposition(|(_, record)| {
        matches!(
            record,
            BranchRecord {
                record: JournalRecord {
                    kind: JournalKind::Snapshot,
                    ..
                },
                ..
            }
        )
    });
    let records = if let Some(snapshot) = latest_snapshot {
        let snapshot_entry = records[snapshot].0;
        if let Some((_, error)) = invalid_records
            .iter()
            .find(|(entry_index, _)| *entry_index > snapshot_entry)
        {
            return Err(error.clone());
        }
        records.into_iter().skip(snapshot).collect()
    } else {
        if let Some((_, error)) = invalid_records.first() {
            return Err(error.clone());
        }
        records
    };
    let records = records
        .into_iter()
        .map(|(_, record)| record)
        .collect::<Vec<_>>();
    let (mut restored, mut anchor, force_snapshot) = match records.first() {
        None => (DurableState::default(), None, true),
        Some(BranchRecord { record, entry_id }) => {
            if !matches!(record.kind, JournalKind::Snapshot) {
                return Err("durable journal has no self-contained snapshot base".into());
            }
            // A complete snapshot is deliberately independent of records before it. Its
            // own state/revision have already been validated above; its native id becomes
            // the anchor for the suffix that follows it.
            let restored = apply_record(&DurableState::default(), record)?;
            (restored, Some(entry_id.clone()), false)
        }
    };
    validate_state(&restored)?;
    let mut commits_since_snapshot: u64 = 0;

    for BranchRecord { record, entry_id } in records.iter().skip(1) {
        if matches!(record.kind, JournalKind::Snapshot) {
            return Err("unexpected durable snapshot after the selected snapshot anchor".into());
        }
        if record.base_revision != restored.revision || record.base_entry_id != anchor {
            return Err("durable journal delta has a gap or wrong branch anchor".into());
        }
        if record.revision
            != restored
                .revision
                .checked_add(1)
                .ok_or("durable revision overflow")?
        {
            return Err("durable journal delta has a non-contiguous revision".into());
        }
        restored = apply_record(&restored, record)?;
        anchor = Some(entry_id.clone());
        commits_since_snapshot = commits_since_snapshot.saturating_add(1);
    }
    Ok((
        restored,
        JournalMeta {
            anchor_entry_id: anchor,
            commits_since_snapshot,
            force_snapshot,
        },
    ))
}

fn run_meta(run: &RunState) -> JournalChange {
    JournalChange::SetRunMeta {
        id: run.id.clone(),
        status: run.status.clone(),
        started_revision: run.started_revision,
        updated_revision: run.updated_revision,
        outcome: run.outcome.clone(),
        recovery_of: run.recovery_of.clone(),
    }
}

fn tool_changes(current: &[ToolExecution], next: &[ToolExecution]) -> Option<Vec<JournalChange>> {
    if next.len() == current.len() + 1 && next[..current.len()] == *current {
        return Some(vec![JournalChange::AppendTool {
            tool: next.last()?.clone(),
        }]);
    }
    if current.len() == MAX_JOURNAL_ENTRIES
        && next.len() == MAX_JOURNAL_ENTRIES
        && next[..MAX_JOURNAL_ENTRIES - 1] == current[1..]
    {
        return Some(vec![JournalChange::AppendTool {
            tool: next.last()?.clone(),
        }]);
    }
    if current.len() == next.len() {
        let mut changes = Vec::new();
        for (before, after) in current.iter().zip(next) {
            if before != after {
                let same_identity =
                    before.call_key == after.call_key && before.call_id == after.call_id;
                let unique = current
                    .iter()
                    .filter(|tool| {
                        if let Some(key) = &before.call_key {
                            tool.call_key.as_ref() == Some(key)
                        } else {
                            tool.call_key.is_none() && tool.call_id == before.call_id
                        }
                    })
                    .count()
                    == 1;
                if !same_identity
                    || before.name != after.name
                    || before.params != after.params
                    || !unique
                {
                    return None;
                }
                changes.push(JournalChange::UpdateTool {
                    call_id: after.call_id.clone(),
                    call_key: after.call_key.clone(),
                    status: after.status.clone(),
                    result: after.result.clone(),
                });
            }
        }
        if !changes.is_empty() {
            return Some(changes);
        }
    }
    None
}

fn workflow_changes(current: &Option<Workflow>, next: &Option<Workflow>) -> Vec<JournalChange> {
    if let (Some(before), Some(after)) = (current, next) {
        if before.steps.len() == after.steps.len()
            && before
                .steps
                .iter()
                .zip(&after.steps)
                .all(|(old, new)| old.id == new.id && old.title == new.title)
        {
            let mut changes = Vec::new();
            if before.name != after.name
                || before.status != after.status
                || before.reason != after.reason
            {
                changes.push(JournalChange::SetWorkflowMeta {
                    name: after.name.clone(),
                    status: after.status.clone(),
                    reason: after.reason.clone(),
                });
            }
            for (before, after) in before.steps.iter().zip(&after.steps) {
                if before.status != after.status || before.note != after.note {
                    changes.push(JournalChange::UpdateStep {
                        step_id: after.id.clone(),
                        status: after.status.clone(),
                        note: after.note.clone(),
                    });
                }
            }
            if !changes.is_empty() {
                return changes;
            }
        }
    }
    vec![JournalChange::SetWorkflow {
        workflow: next.clone(),
    }]
}

fn journal_changes(current: &DurableState, next: &DurableState) -> Vec<JournalChange> {
    let mut changes = Vec::new();
    if current.workflow != next.workflow {
        changes.extend(workflow_changes(&current.workflow, &next.workflow));
    }
    if current.next_run_id != next.next_run_id || current.next_queue_id != next.next_queue_id {
        changes.push(JournalChange::SetCounters {
            next_run_id: next.next_run_id,
            next_queue_id: next.next_queue_id,
        });
    }
    match (&current.run, &next.run) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => changes.push(JournalChange::SetRun {
            run: next.run.clone(),
        }),
        (Some(before), Some(after)) if before.id != after.id => {
            changes.push(JournalChange::SetRun {
                run: next.run.clone(),
            })
        }
        (Some(before), Some(after)) => {
            if before.id != after.id
                || before.status != after.status
                || before.started_revision != after.started_revision
                || before.updated_revision != after.updated_revision
                || before.outcome != after.outcome
                || before.recovery_of != after.recovery_of
            {
                changes.push(run_meta(after));
            }
            if let Some(tool_changes) = tool_changes(&before.tools, &after.tools) {
                changes.extend(tool_changes);
            } else if before.tools != after.tools {
                changes.push(JournalChange::SetTools {
                    tools: after.tools.clone(),
                });
            }
        }
    }
    if current.queue != next.queue {
        if next.queue.len() == current.queue.len() + 1
            && next.queue[..current.queue.len()] == *current.queue
        {
            changes.push(JournalChange::AppendQueue {
                item: next.queue.last().expect("queue length checked").clone(),
            });
        } else if current.queue.len() == next.queue.len() {
            let differing = current
                .queue
                .iter()
                .zip(&next.queue)
                .filter(|(before, after)| before != after)
                .count();
            if differing == 1 {
                let index = current
                    .queue
                    .iter()
                    .zip(&next.queue)
                    .position(|(before, after)| before != after)
                    .expect("queue difference counted");
                changes.push(JournalChange::UpdateQueue {
                    item: next.queue[index].clone(),
                });
            } else {
                changes.push(JournalChange::SetQueue {
                    queue: next.queue.clone(),
                });
            }
        } else {
            changes.push(JournalChange::SetQueue {
                queue: next.queue.clone(),
            });
        }
    }
    changes
}

fn journal_record(
    current: &DurableState,
    next: &DurableState,
    meta: &JournalMeta,
) -> JournalRecord {
    let changes = journal_changes(current, next);
    JournalRecord {
        format: JOURNAL_FORMAT.into(),
        version: JOURNAL_VERSION,
        kind: JournalKind::Delta,
        base_revision: current.revision,
        revision: next.revision,
        base_entry_id: meta.anchor_entry_id.clone(),
        state: None,
        changes: Some(changes),
    }
}

fn commit_candidate<F>(
    current: &DurableState,
    meta: &JournalMeta,
    mut next: DurableState,
    append: F,
) -> Result<(DurableState, JournalMeta), String>
where
    F: FnOnce(Value) -> Result<String, String>,
{
    if next == *current {
        return Ok((current.clone(), meta.clone()));
    }
    next.revision = current
        .revision
        .checked_add(1)
        .ok_or("durable revision exhausted")?;
    if let Some(run) = next.run.as_mut() {
        run.updated_revision = next.revision;
    }
    let delta = journal_record(current, &next, meta);
    if delta.changes.as_ref().map_or(true, Vec::is_empty) {
        return Err("durable state changed without a journal change".into());
    }
    let snapshot = JournalRecord {
        format: JOURNAL_FORMAT.into(),
        version: JOURNAL_VERSION,
        kind: JournalKind::Snapshot,
        base_revision: current.revision,
        revision: next.revision,
        base_entry_id: meta.anchor_entry_id.clone(),
        state: Some(next.clone()),
        changes: None,
    };
    let delta_size = serde_json::to_vec(&delta)
        .map_err(|error| format!("serialize durable delta: {error}"))?
        .len();
    let snapshot_size = serde_json::to_vec(&snapshot)
        .map_err(|error| format!("serialize durable snapshot: {error}"))?
        .len();
    let use_snapshot = meta.force_snapshot
        || meta.anchor_entry_id.is_none()
        || meta.commits_since_snapshot + 1 >= SNAPSHOT_INTERVAL
        || delta_size >= snapshot_size;
    let record = if use_snapshot { snapshot } else { delta };
    let entry_id = append(json!({
        "customType": ENTRY_TYPE,
        "data": record,
    }))?;
    Ok((
        next,
        JournalMeta {
            anchor_entry_id: Some(entry_id),
            commits_since_snapshot: if use_snapshot {
                0
            } else {
                meta.commits_since_snapshot + 1
            },
            force_snapshot: false,
        },
    ))
}

fn restore_state_inner() -> Result<(), String> {
    // Filtering is a required host contract, not a hint. Never retry with an
    // unfiltered branch on hosts that do not implement it.
    let value = call_runtime(
        RuntimeActionId::GetSessionBranch,
        json!({"customTypes": [ENTRY_TYPE]}),
    )?;
    if value.get("filtered").and_then(Value::as_bool) != Some(true) {
        return Err("GetSessionBranch must confirm filtered: true; upgrade the native host".into());
    }
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("GetSessionBranch returned no valid entries array")?;
    let (restored, meta) = decode_branch(entries)?;
    let restored = mark_unresolvable_legacy_tools(restored);
    let interrupted = mark_interrupted(restored.clone());
    let (restored, meta) = if interrupted != restored {
        commit_candidate(&restored, &meta, interrupted, |payload| {
            append_entry(payload)
        })?
    } else {
        (restored, meta)
    };
    *state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Store {
        state: restored,
        meta,
        initialized: true,
        restore_error: None,
    };
    Ok(())
}

fn restore_state_locked() -> Result<(), String> {
    match restore_state_inner() {
        Ok(()) => Ok(()),
        Err(error) => {
            // Keep the failure state under RESTORE_LOCK. Otherwise a mutation
            // can pass the restore_error check between this function returning
            // and the caller publishing the failure.
            fail_restore(error.clone());
            Err(error)
        }
    }
}

fn restore_state() -> Result<(), String> {
    let _restore_guard = restore_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    restore_state_locked()
}

fn fail_restore(error: String) {
    *state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Store {
        state: DurableState::default(),
        meta: JournalMeta::default(),
        initialized: true,
        restore_error: Some(error),
    };
}

fn ensure_initialized() -> Result<(), String> {
    {
        let status = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(error) = &status.restore_error {
            return Err(format!("durable branch restore failed: {error}"));
        }
        if status.initialized {
            return Ok(());
        }
    }

    let _restore_guard = restore_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Another caller may have completed the restore while this caller waited.
    // Re-check under the restore gate before touching the native branch.
    {
        let status = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(error) = &status.restore_error {
            return Err(format!("durable branch restore failed: {error}"));
        }
        if status.initialized {
            return Ok(());
        }
    }

    match restore_state_locked() {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("durable branch restore failed: {error}")),
    }
}

fn snapshot_state() -> Result<DurableState, String> {
    ensure_initialized()?;
    let _restore_guard = restore_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let store = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(error) = &store.restore_error {
        return Err(format!("durable branch restore failed: {error}"));
    }
    Ok(store.state.clone())
}

fn tool_call_id(value: &str) -> String {
    if value.chars().count() <= MAX_SUMMARY_CHARS {
        return value.to_string();
    }
    // Keep the persisted/displayed ID bounded while making long IDs stable
    // with a cryptographic digest. This avoids adding a second public field
    // to the durable state schema.
    let digest = Sha256::digest(value.as_bytes());
    let digest_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let suffix = format!("…{digest_hex}");
    let prefix_len = MAX_SUMMARY_CHARS - suffix.chars().count();
    let prefix: String = value.chars().take(prefix_len).collect();
    format!("{prefix}{suffix}")
}

fn legacy_hashed_tool_id(value: &str) -> Option<String> {
    if value.chars().count() <= MAX_SUMMARY_CHARS {
        return None;
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let suffix = format!("…{hash:016x}");
    let prefix_len = MAX_SUMMARY_CHARS - suffix.chars().count();
    let prefix: String = value.chars().take(prefix_len).collect();
    Some(format!("{prefix}{suffix}"))
}

fn legacy_tool_call_id(value: &str) -> bool {
    value.chars().count() == MAX_SUMMARY_CHARS + 1 && value.ends_with('…')
}

fn mark_unresolvable_legacy_tools(mut next: DurableState) -> DurableState {
    if let Some(run) = next.run.as_mut() {
        for tool in &mut run.tools {
            if legacy_tool_call_id(&tool.call_id) && tool.status == ToolExecutionStatus::Started {
                tool.status = ToolExecutionStatus::Unknown;
                if tool.result.is_empty() {
                    tool.result =
                        "tool identity was truncated before durable identity migration".into();
                }
            }
        }
    }
    next
}

fn tool_call_key(value: &str) -> String {
    if value.chars().count() <= MAX_SUMMARY_CHARS {
        format!("raw:{value}")
    } else {
        format!("hash:{}", tool_call_id(value))
    }
}

fn looks_like_generated_tool_id(value: &str) -> bool {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() != MAX_SUMMARY_CHARS {
        return false;
    }
    [TOOL_ID_HASH_HEX_CHARS, 16].into_iter().any(|hash_chars| {
        chars[MAX_SUMMARY_CHARS - (hash_chars + 1)] == '…'
            && chars[MAX_SUMMARY_CHARS - hash_chars..]
                .iter()
                .all(|character| character.is_ascii_hexdigit())
    })
}

fn legacy_tool_matches(tool: &ToolExecution, call_id: &str, stable_id: &str) -> bool {
    if tool.call_key.is_some() {
        return false;
    }
    if call_id.chars().count() <= MAX_SUMMARY_CHARS {
        tool.call_id == call_id && !looks_like_generated_tool_id(&tool.call_id)
    } else {
        tool.call_id == stable_id
            || legacy_hashed_tool_id(call_id).as_deref() == Some(tool.call_id.as_str())
    }
}

fn tool_matches(tool: &ToolExecution, call_id: &str, stable_id: &str, key: &str) -> bool {
    tool.call_key.as_deref() == Some(key) || legacy_tool_matches(tool, call_id, stable_id)
}

fn summary(value: &str) -> String {
    let mut result: String = value.chars().take(MAX_SUMMARY_CHARS).collect();
    if value.chars().count() > MAX_SUMMARY_CHARS {
        result.push('…');
    }
    result
}

fn commit_state(current: &DurableState, next: DurableState) -> Result<DurableState, String> {
    // SessionStart/SessionTree restore uses the same gate. Hold it through
    // the native append so a mutation cannot commit against the branch that
    // is being replaced underneath it.
    let _restore_guard = restore_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(error) = &guard.restore_error {
        return Err(format!("durable branch restore failed: {error}"));
    }
    if guard.state != *current {
        return Err("durable state changed concurrently; retry the operation".into());
    }
    let committed = commit_candidate(current, &guard.meta, next, append_entry);
    match committed {
        Ok((state, meta)) => {
            guard.state = state.clone();
            guard.meta = meta;
            Ok(state)
        }
        Err(error) => {
            if error.starts_with("durable append acknowledgement is unknown:") {
                // The native append may have committed even though its response was
                // unusable. Do not retry against the old anchor; require an explicit
                // branch reload to reconcile the chain.
                guard.initialized = true;
                guard.restore_error = Some(error.clone());
            }
            Err(error)
        }
    }
}

fn mutate_state<F>(mutator: F) -> Result<(), String>
where
    F: FnOnce(&mut DurableState),
{
    ensure_initialized()?;
    let current = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .state
        .clone();
    let mut next = current.clone();
    mutator(&mut next);
    commit_state(&current, next).map(|_| ())
}

fn mark_interrupted(mut next: DurableState) -> DurableState {
    if let Some(run) = next.run.as_mut() {
        if run.status == RunStatus::Running {
            run.status = RunStatus::Interrupted;
            run.outcome = "interrupted".into();
            for tool in &mut run.tools {
                if tool.status == ToolExecutionStatus::Started {
                    tool.status = ToolExecutionStatus::Interrupted;
                    if tool.result.is_empty() {
                        tool.result = "execution did not reach a durable result".into();
                    }
                }
            }
        }
    }
    next
}

fn text(value: Option<&Value>, field: &str, max_chars: usize) -> Result<String, String> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} is required"))?
        .trim();
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(format!("{field} exceeds {max_chars} characters"));
    }
    Ok(value.to_string())
}

fn optional_text(
    value: Option<&Value>,
    field: &str,
    max_chars: usize,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    text(Some(value), field, max_chars).map(Some)
}

fn workflow_mut(state: &mut DurableState) -> Result<&mut Workflow, String> {
    state
        .workflow
        .as_mut()
        .ok_or_else(|| "no durable workflow is active; call durable with action start".into())
}

fn parse_steps(value: Option<&Value>) -> Result<Vec<Step>, String> {
    let values = value
        .and_then(Value::as_array)
        .ok_or("steps is required and must be an array")?;
    if values.is_empty() {
        return Err("steps must contain at least one item".into());
    }
    if values.len() > MAX_STEPS {
        return Err(format!("steps cannot contain more than {MAX_STEPS} items"));
    }

    let mut steps = Vec::with_capacity(values.len());
    for value in values {
        let id = text(value.get("id"), "step id", MAX_STEP_ID_CHARS)?;
        let title = text(value.get("title"), "step title", MAX_STEP_TITLE_CHARS)?;
        if steps.iter().any(|step: &Step| step.id == id) {
            return Err(format!("duplicate step id: {id}"));
        }
        steps.push(Step {
            id,
            title,
            status: StepStatus::Pending,
            note: None,
        });
    }
    Ok(steps)
}

fn require_running(workflow: &Workflow) -> Result<(), String> {
    if workflow.status != WorkflowStatus::Running {
        return Err(format!(
            "workflow is {}; only a running workflow can be checkpointed",
            workflow.status.as_str()
        ));
    }
    Ok(())
}

fn allocate_run(next: &mut DurableState, status: RunStatus, recovery_of: String) -> RunState {
    next.next_run_id = next.next_run_id.saturating_add(1);
    RunState {
        id: format!("run-{}", next.next_run_id),
        status,
        started_revision: next.revision.saturating_add(1),
        updated_revision: next.revision.saturating_add(1),
        outcome: String::new(),
        recovery_of,
        tools: Vec::new(),
    }
}

fn start_run(next: &mut DurableState) {
    let recovery_of = next
        .run
        .as_ref()
        .filter(|run| matches!(run.status, RunStatus::Queued | RunStatus::Running))
        .map(|run| run.id.clone())
        .unwrap_or_default();
    next.run = Some(allocate_run(next, RunStatus::Running, recovery_of));
}

fn recover_run(next: &mut DurableState) -> Result<(), String> {
    let previous = next
        .run
        .as_ref()
        .ok_or_else(|| "no durable run is available to recover".to_string())?;
    if previous.status != RunStatus::Interrupted {
        return Err(format!(
            "run is {}; only an interrupted run can be recovered",
            previous.status.as_str()
        ));
    }
    let previous_id = previous.id.clone();
    let tools = previous.tools.clone();
    let mut run = allocate_run(next, RunStatus::Queued, previous_id.clone());
    run.tools = tools;
    next.run = Some(run);
    Ok(())
}

fn forget_run(next: &mut DurableState) -> Result<(), String> {
    if next.run.is_none() {
        return Err("no durable run is recorded".into());
    }
    next.run = None;
    Ok(())
}

fn queue_item_id(params: &Value) -> Result<String, String> {
    text(params.get("item"), "item", MAX_STEP_ID_CHARS)
}

fn enqueue_item(next: &mut DurableState, params: &Value) -> Result<(), String> {
    if next.queue.len() >= MAX_QUEUE_ITEMS {
        return Err(format!(
            "queue cannot contain more than {MAX_QUEUE_ITEMS} items"
        ));
    }
    let item_text = text(params.get("text"), "text", MAX_QUEUE_TEXT_CHARS)?;
    next.next_queue_id = next.next_queue_id.saturating_add(1);
    let revision = next.revision.saturating_add(1);
    next.queue.push(QueueItem {
        id: format!("item-{}", next.next_queue_id),
        text: item_text,
        status: QueueItemStatus::Queued,
        created_revision: revision,
        updated_revision: revision,
        note: String::new(),
    });
    Ok(())
}

fn claim_item(next: &mut DurableState, params: &Value) -> Result<(), String> {
    let id = queue_item_id(params)?;
    let item = next
        .queue
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("queue item not found: {id}"))?;
    if item.status != QueueItemStatus::Queued {
        return Err(format!(
            "queue item {id} is {}; only a queued item can be claimed",
            item.status.as_str()
        ));
    }
    item.status = QueueItemStatus::Claimed;
    item.updated_revision = next.revision.saturating_add(1);
    Ok(())
}

fn finish_item(next: &mut DurableState, params: &Value) -> Result<(), String> {
    let id = queue_item_id(params)?;
    let status = match params.get("status").and_then(Value::as_str) {
        Some("completed") => QueueItemStatus::Completed,
        Some("failed") => QueueItemStatus::Failed,
        _ => return Err("status must be completed or failed for queue finish".into()),
    };
    let note = optional_text(params.get("note"), "note", MAX_NOTE_CHARS)?.unwrap_or_default();
    let revision = next.revision.saturating_add(1);
    let item = next
        .queue
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("queue item not found: {id}"))?;
    if item.status != QueueItemStatus::Claimed {
        return Err(format!(
            "queue item {id} is {}; only a claimed item can be finished",
            item.status.as_str()
        ));
    }
    item.status = status;
    item.updated_revision = revision;
    item.note = note;
    Ok(())
}

fn cancel_item(next: &mut DurableState, params: &Value) -> Result<(), String> {
    let id = queue_item_id(params)?;
    let note = optional_text(params.get("note"), "note", MAX_NOTE_CHARS)?
        .unwrap_or_else(|| "cancelled".into());
    let revision = next.revision.saturating_add(1);
    let item = next
        .queue
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("queue item not found: {id}"))?;
    if !matches!(
        item.status,
        QueueItemStatus::Queued | QueueItemStatus::Claimed
    ) {
        return Err(format!(
            "queue item {id} is {}; only a queued or claimed item can be cancelled",
            item.status.as_str()
        ));
    }
    item.status = QueueItemStatus::Cancelled;
    item.updated_revision = revision;
    item.note = note;
    Ok(())
}

fn record_tool_start(next: &mut DurableState, call_id: &str, name: &str, params: &str) {
    let Some(run) = next.run.as_mut() else {
        return;
    };
    let stable_id = tool_call_id(call_id);
    let key = tool_call_key(call_id);
    if run.tools.iter().any(|tool| {
        tool_matches(tool, call_id, &stable_id, &key) && tool.status != ToolExecutionStatus::Unknown
    }) {
        return;
    }
    run.tools.push(ToolExecution {
        call_id: stable_id,
        call_key: Some(key),
        name: summary(name),
        params: summary(params),
        status: ToolExecutionStatus::Started,
        result: String::new(),
    });
    if run.tools.len() > MAX_JOURNAL_ENTRIES {
        let overflow = run.tools.len() - MAX_JOURNAL_ENTRIES;
        run.tools.drain(..overflow);
    }
}

fn record_tool_end(
    next: &mut DurableState,
    call_id: &str,
    name: &str,
    result: &str,
    is_error: bool,
) {
    let Some(run) = next.run.as_mut() else {
        return;
    };
    let stable_id = tool_call_id(call_id);
    let key = tool_call_key(call_id);
    let matching = run
        .tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| tool_matches(tool, call_id, &stable_id, &key))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matching.len() == 1 {
        let tool = &mut run.tools[matching[0]];
        if matches!(
            tool.status,
            ToolExecutionStatus::Completed
                | ToolExecutionStatus::Failed
                | ToolExecutionStatus::Interrupted
                | ToolExecutionStatus::Unknown
        ) {
            return;
        }
        tool.status = if is_error {
            ToolExecutionStatus::Failed
        } else {
            ToolExecutionStatus::Completed
        };
        tool.result = summary(result);
        return;
    }
    // A legacy journal may contain colliding truncated IDs. Preserve this
    // result as an explicitly ambiguous entry instead of attaching it to the
    // first matching tool.
    run.tools.push(ToolExecution {
        call_id: stable_id,
        call_key: Some(key),
        name: summary(name),
        params: String::new(),
        status: ToolExecutionStatus::Unknown,
        result: summary(result),
    });
    if run.tools.len() > MAX_JOURNAL_ENTRIES {
        let overflow = run.tools.len() - MAX_JOURNAL_ENTRIES;
        run.tools.drain(..overflow);
    }
}

/// Apply one action without touching the host. Keeping this state machine pure
/// makes transition and validation tests independent of the native ABI.
fn apply_action(current: &DurableState, params: &Value) -> Result<DurableState, String> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    let mut next = current.clone();

    match action {
        "status" => {}
        "start_run" => start_run(&mut next),
        "settle_run" => {
            if let Some(run) = next.run.as_mut() {
                if run.status == RunStatus::Running {
                    run.status = RunStatus::Settled;
                    run.outcome = "settled".into();
                    for tool in &mut run.tools {
                        if tool.status == ToolExecutionStatus::Started {
                            tool.status = ToolExecutionStatus::Interrupted;
                            tool.result = "run settled without a durable tool result".into();
                        }
                    }
                }
            }
        }
        "recover_run" => recover_run(&mut next)?,
        "start" => {
            let name = text(params.get("name"), "name", MAX_NAME_CHARS)?;
            next.workflow = Some(Workflow {
                name,
                status: WorkflowStatus::Running,
                steps: parse_steps(params.get("steps"))?,
                reason: None,
            });
        }
        "checkpoint" => {
            let step_id = text(params.get("step"), "step", MAX_STEP_ID_CHARS)?;
            let status = match params.get("status").and_then(Value::as_str).unwrap_or("in_progress") {
                "pending" => StepStatus::Pending,
                "in_progress" => StepStatus::InProgress,
                "completed" => StepStatus::Completed,
                _ => return Err("status must be pending, in_progress, or completed".into()),
            };
            let note = optional_text(params.get("note"), "note", MAX_NOTE_CHARS)?;
            let workflow = workflow_mut(&mut next)?;
            require_running(workflow)?;
            let step = workflow
                .steps
                .iter_mut()
                .find(|step| step.id == step_id)
                .ok_or_else(|| format!("step not found: {step_id}"))?;
            step.status = status;
            if note.is_some() {
                step.note = note;
            }
        }
        "pause" => {
            let reason = optional_text(params.get("reason"), "reason", MAX_NOTE_CHARS)?;
            let workflow = workflow_mut(&mut next)?;
            if workflow.status != WorkflowStatus::Running {
                return Err(format!(
                    "workflow is {}; only a running workflow can be paused",
                    workflow.status.as_str()
                ));
            }
            workflow.status = WorkflowStatus::Paused;
            workflow.reason = reason;
        }
        "resume" => {
            let workflow = workflow_mut(&mut next)?;
            if workflow.status != WorkflowStatus::Paused {
                return Err(format!(
                    "workflow is {}; only a paused workflow can be resumed",
                    workflow.status.as_str()
                ));
            }
            workflow.status = WorkflowStatus::Running;
            workflow.reason = None;
        }
        "complete" => {
            let workflow = workflow_mut(&mut next)?;
            require_running(workflow)?;
            if workflow
                .steps
                .iter()
                .any(|step| step.status != StepStatus::Completed)
            {
                return Err("all steps must be completed before completing the workflow".into());
            }
            workflow.status = WorkflowStatus::Completed;
            workflow.reason = None;
        }
        "fail" => {
            let reason = optional_text(params.get("reason"), "reason", MAX_NOTE_CHARS)?
                .or_else(|| Some("workflow marked as failed".into()));
            let workflow = workflow_mut(&mut next)?;
            if !matches!(workflow.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
                return Err(format!(
                    "workflow is {}; only a running or paused workflow can fail",
                    workflow.status.as_str()
                ));
            }
            workflow.status = WorkflowStatus::Failed;
            workflow.reason = reason;
        }
        "reset" => next.workflow = None,
        "recover" => recover_run(&mut next)?,
        "forget_run" => forget_run(&mut next)?,
        "enqueue" => enqueue_item(&mut next, params)?,
        "claim" => claim_item(&mut next, params)?,
        "finish" => finish_item(&mut next, params)?,
        "cancel" => cancel_item(&mut next, params)?,
        "queue" => {}
        _ => return Err("action must be one of: status, start, checkpoint, pause, resume, complete, fail, reset, recover, forget_run, enqueue, queue, claim, finish, cancel".into()),
    }
    Ok(next)
}

fn render(state: &DurableState) -> String {
    let mut lines = Vec::new();
    if let Some(workflow) = &state.workflow {
        let completed = workflow
            .steps
            .iter()
            .filter(|step| step.status == StepStatus::Completed)
            .count();
        lines.push(format!(
            "{} [{}] — {completed}/{} completed",
            workflow.name,
            workflow.status.as_str(),
            workflow.steps.len()
        ));
        if let Some(reason) = &workflow.reason {
            lines.push(format!("Reason: {reason}"));
        }
        for step in &workflow.steps {
            let marker = step.status.marker();
            lines.push(format!("[{marker}] {} — {}", step.id, step.title));
        }
    } else {
        lines.push("No durable workflow is active".into());
    }
    if let Some(run) = &state.run {
        let tool_summary = run
            .tools
            .iter()
            .rev()
            .take(3)
            .map(|tool| format!("{}:{}", tool.name, tool.status.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "Run {} [{}]{}",
            run.id,
            run.status.as_str(),
            if tool_summary.is_empty() {
                String::new()
            } else {
                format!(" — tools: {tool_summary}")
            }
        ));
        if run.status == RunStatus::Interrupted {
            lines
                .push("Recovery required: inspect the journal before retrying side effects".into());
        }
    }
    if !state.queue.is_empty() {
        let queued = state
            .queue
            .iter()
            .filter(|item| item.status == QueueItemStatus::Queued)
            .count();
        lines.push(format!(
            "Queue: {queued}/{} items queued",
            state.queue.len()
        ));
        for item in state.queue.iter().rev().take(3) {
            lines.push(format!(
                "[{}] {} — {}",
                item.status.as_str(),
                item.id,
                item.text
            ));
        }
    }
    lines.join("\n")
}

fn result(action: &str, state: &DurableState, error: Option<&str>) -> String {
    let mut details = serde_json::to_value(state).unwrap_or_else(|_| json!({"workflow": null}));
    details["action"] = json!(action);
    if let Some(error) = error {
        details["error"] = json!(error);
    }
    json!({
        "content": [{"type": "text", "text": if let Some(error) = error { format!("Error: {error}") } else { render(state) }}],
        "details": details
    })
    .to_string()
}

fn error_result(action: &str, error: &str) -> String {
    json!({
        "content": [{"type": "text", "text": format!("Error: {error}")}],
        "details": {"action": action, "error": error}
    })
    .to_string()
}

fn durable(params: &Value) -> Result<String, String> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    let current = match snapshot_state() {
        Ok(state) => state,
        Err(error) => return Ok(error_result(action, &error)),
    };
    let next = match apply_action(&current, params) {
        Ok(next) => next,
        Err(error) => return Ok(result(action, &current, Some(&error))),
    };

    let committed = commit_state(&current, next)?;
    Ok(result(action, &committed, None))
}

static HANDLES: ToolHandles<Drive> = ToolHandles::new();

struct Drive {
    params: Value,
    done: bool,
}

extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let input = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        params: serde_json::from_str(&input).unwrap_or(Value::Null),
        done: false,
    })
}

extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if handle.is_null() {
        return StepResult::err(StbString::from_string("null durable handle".into()));
    }
    let Some(entry) = HANDLES.get(handle) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed durable tool handle".into(),
        ));
    };
    let mut drive = entry.lock();
    if entry.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        return StepResult::err(StbString::from_string("durable cancelled".into()));
    }
    if drive.done {
        return StepResult::err(StbString::from_string(
            "durable polled after completion".into(),
        ));
    }
    drive.done = true;
    match durable(&drive.params) {
        Ok(value) => StepResult::done(StbString::from_string(value)),
        Err(error) => StepResult::err(StbString::from_string(error)),
    }
}

extern "C" fn cancel(handle: StepHandle) {
    HANDLES.cancel(handle);
}

extern "C" fn destroy(handle: StepHandle) {
    drop(HANDLES.remove(handle));
}

use rpi_plugin_sdk::callbacks::free_string;

rpi_plugin_sdk::string_handler! {
fn command(args, out, _user_data) {
    if !out.is_available() {
        return 1;
    }
    let request: Value = match serde_json::from_str(args) {
        Ok(request) => request,
        Err(error) => {
            out.write(json!({"error": format!("invalid command request: {error}")}).to_string());
            return 0;
        }
    };
    let raw = request.get("args").and_then(Value::as_str).unwrap_or("").trim();
    if raw.is_empty() {
        let snapshot = match snapshot_state() {
            Ok(state) => state,
            Err(error) => {
                out.write(json!({"kind":"message","text":format!("Error: {error}")}).to_string());
                return 0;
            }
        };
        out.write(json!({"kind":"message","text":render(&snapshot)}).to_string());
        return 0;
    }
    let params: Value = match serde_json::from_str::<Value>(raw) {
        Ok(params) if params.is_object() => params,
        Ok(_) => {
            out.write(json!({"error":"durable command args must be a JSON object"}).to_string());
            return 0;
        }
        Err(error) => {
            out.write(json!({"error": format!("invalid durable command args: {error}")}).to_string());
            return 0;
        }
    };
    match durable(&params) {
        Ok(result) => out.write(result),
        Err(error) => out.write(json!({"error": error}).to_string()),
    };
    0
}
}

rpi_plugin_sdk::event_handler! {
fn handle_event(event, _user_data) {
    match event.tag() {
        EventTag::SessionStart | EventTag::SessionTree => {
            if restore_state().is_err() {
                return 1;
            }
        }
        EventTag::AgentStart => {
            if mutate_state(start_run).is_err() {
                return 1;
            }
        }
        EventTag::ToolExecutionStart => {
            let Some(call) = event.tool_call() else {
                return 1;
            };
            let result = mutate_state(|next| {
                if next.run.is_none() {
                    start_run(next);
                }
                if next
                    .run
                    .as_ref()
                    .is_some_and(|run| run.status == RunStatus::Running)
                {
                    record_tool_start(next, &call.tool_call_id, &call.tool_name, &call.params);
                }
            });
            if result.is_err() {
                return 1;
            }
        }
        EventTag::ToolExecutionEnd | EventTag::ToolResult => {
            let Some(result_event) = event.tool_result() else {
                return 1;
            };
            let result = mutate_state(|next| {
                if next.run.is_none() {
                    start_run(next);
                }
                record_tool_end(
                    next,
                    &result_event.tool_call_id,
                    &result_event.tool_name,
                    &result_event.result,
                    result_event.is_error,
                );
            });
            if result.is_err() {
                return 1;
            }
        }
        EventTag::AgentSettled => {
            let result = mutate_state(|next| {
                if let Some(run) = next.run.as_mut() {
                    if run.status == RunStatus::Running {
                        run.status = RunStatus::Settled;
                        run.outcome = "settled".into();
                        for tool in &mut run.tools {
                            if tool.status == ToolExecutionStatus::Started {
                                tool.status = ToolExecutionStatus::Interrupted;
                                tool.result =
                                    "run settled without a durable tool result".into();
                            }
                        }
                    }
                }
            });
            if result.is_err() {
                return 1;
            }
        }
        EventTag::SessionShutdown => {
            let result = mutate_state(|next| {
                let recovered = mark_interrupted(next.clone());
                *next = recovered;
            });
            if result.is_err() {
                return 1;
            }
        }
        _ => {}
    }
    0
}
}

rpi_plugin_sdk::export_plugin!(|api| {
    let _ = RUNTIME.set(*api);
    if let Some(register_event) = api.register_event_handler {
        for tag in [
            EventTag::SessionStart,
            EventTag::SessionTree,
            EventTag::AgentStart,
            EventTag::ToolExecutionStart,
            EventTag::ToolExecutionEnd,
            EventTag::ToolResult,
            EventTag::AgentSettled,
            EventTag::SessionShutdown,
        ] {
            if register_event(tag, handle_event as EventHandlerFn, std::ptr::null_mut()) != 0 {
                return 1;
            }
        }
    }
    if let Some(register_command) = api.register_command {
        let _ = register_command(
            StbStringRef::from_str("durable"),
            StbStringRef::from_str(
                "Show the current durable workflow, recovery journal, and work queue",
            ),
            command,
        );
    }
    let Some(register) = api.register_tool else {
        return 1;
    };
    let schema = Box::new(tool_schema());
    let rc = register(&*schema, execute, poll, cancel, destroy, free_string);
    drop(schema);
    rc
});

// Shared by registration and contract tests; no changes to argument execution.
fn tool_schema() -> StableToolSchema {
    let nonempty = |description: &str| {
        json!({
            "type": "string", "pattern": "\\S", "description": description
        })
    };
    let parameters = json!({
        "type": "object",
        "description": "Arguments apply to the indicated actions.",
        "additionalProperties": true,
        "properties": {
            "action": {"type": "string", "default": "status", "enum": ["status", "start", "checkpoint", "pause", "resume", "complete", "fail", "reset", "recover", "forget_run", "enqueue", "queue", "claim", "finish", "cancel"]},
            "name": nonempty("Required for start; trimmed workflow name, at most 500 Unicode scalar values. Replaces the existing workflow."),
            "steps": {
                "type": "array", "minItems": 1, "maxItems": MAX_STEPS,
                "description": "Required for start. Stable IDs must be unique after trimming; each step starts pending.",
                "items": {
                    "type": "object", "additionalProperties": true,
                    "properties": {
                        "id": nonempty("Trimmed stable step ID, at most 128 Unicode scalar values; unique within this workflow."),
                        "title": nonempty("Trimmed step title, at most 1000 Unicode scalar values.")
                    },
                    "required": ["id", "title"]
                }
            },
            "step": nonempty("Required for checkpoint; existing step ID, at most 128 Unicode scalar values after trimming."),
            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "failed"], "description": "checkpoint: pending/in_progress/completed (default in_progress); finish: completed/failed (required)."},
            "note": {"type": ["string", "null"], "pattern": "\\S", "description": "Optional for checkpoint/finish/cancel; null or omitted means absent, otherwise nonblank after trimming, at most 4000 Unicode scalar values. Cancel defaults to 'cancelled'."},
            "reason": {"type": ["string", "null"], "pattern": "\\S", "description": "Optional for pause/fail; null or omitted means absent, otherwise nonblank after trimming, at most 4000 Unicode scalar values. Fail defaults to 'workflow marked as failed'."},
            "text": nonempty("Required for enqueue; trimmed work text, at most 4000 Unicode scalar values. Queue holds at most 32 items including terminal history."),
            "item": nonempty("Required for claim/finish/cancel; exact returned queue ID (not an index), at most 128 Unicode scalar values after trimming.")
        },
        "allOf": [
            {"if": {"properties": {"action": {"const": "start"}}, "required": ["action"]}, "then": {"required": ["name", "steps"]}},
            {"if": {"properties": {"action": {"const": "checkpoint"}}, "required": ["action"]}, "then": {"required": ["step"], "properties": {"status": {"enum": ["pending", "in_progress", "completed"], "default": "in_progress"}}}},
            {"if": {"properties": {"action": {"const": "enqueue"}}, "required": ["action"]}, "then": {"required": ["text"]}},
            {"if": {"properties": {"action": {"enum": ["claim", "finish", "cancel"]}}, "required": ["action"]}, "then": {"required": ["item"]}},
            {"if": {"properties": {"action": {"const": "finish"}}, "required": ["action"]}, "then": {"required": ["status"], "properties": {"status": {"enum": ["completed", "failed"]}}}}
        ]
    });
    StableToolSchema {
        name: StbString::from_string(TOOL.into()),
        description: StbString::from_string("Persist workflow checkpoints, a recovery journal, and a manual work queue in the current native session branch, not a shared workspace/project queue. Independent native sessions do not share claims. A host-managed child created by delegate_task shares the parent workflow, queue, and claims. Do not start/reset it without explicit authorization. Use goal for one context-injected objective; use durable for explicit steps or recovery. Start replaces the workflow; checkpoint requires a running workflow; complete requires all steps completed. Inspect status before recover or retrying interrupted side effects: recover only creates a queued recovery point and never replays tools. Claim queued items before manual execution, then finish claimed items. No workers, delegation, scheduling, or exactly-once guarantee. reset clears only the workflow; forget_run clears only the journal.".into()),
        parameters: StbString::from_string(parameters.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_schema_is_valid_json_with_nested_action_contracts() {
        let schema = tool_schema();
        assert_eq!(schema.name.to_string_lossy(), "durable");
        let parameters: Value = serde_json::from_str(&schema.parameters.to_string_lossy()).unwrap();
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["additionalProperties"], true); // includes __rpi
        assert!(parameters.get("required").is_none()); // action defaults to status
        let props = &parameters["properties"];
        assert_eq!(props["action"]["default"], "status");
        assert_eq!(props["action"]["enum"].as_array().unwrap().len(), 15);
        assert_eq!(props["steps"]["minItems"], 1);
        assert_eq!(props["steps"]["maxItems"], MAX_STEPS);
        assert_eq!(props["steps"]["items"]["type"], "object");
        assert_eq!(props["steps"]["items"]["required"], json!(["id", "title"]));
        assert_eq!(
            props["steps"]["items"]["properties"]["id"]["pattern"],
            "\\S"
        );
        assert_eq!(props["note"]["type"], json!(["string", "null"]));
        let rules = parameters["allOf"].as_array().unwrap();
        assert_eq!(rules.len(), 5);
        for rule in rules {
            assert_eq!(rule["if"]["required"], json!(["action"]));
        }
        assert_eq!(rules[0]["then"]["required"], json!(["name", "steps"]));
        assert_eq!(rules[1]["then"]["required"], json!(["step"]));
        assert_eq!(
            rules[1]["then"]["properties"]["status"]["default"],
            "in_progress"
        );
        assert_eq!(
            rules[1]["then"]["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
        assert_eq!(rules[2]["then"]["required"], json!(["text"]));
        assert_eq!(
            rules[3]["if"]["properties"]["action"]["enum"],
            json!(["claim", "finish", "cancel"])
        );
        assert_eq!(rules[3]["then"]["required"], json!(["item"]));
        assert_eq!(rules[4]["then"]["required"], json!(["status"]));
        assert_eq!(
            rules[4]["then"]["properties"]["status"]["enum"],
            json!(["completed", "failed"])
        );
    }

    #[test]
    fn registered_guidance_warns_about_shared_delegate_task_state() {
        let schema = tool_schema();
        let description = schema.description.to_string_lossy();
        assert!(description.contains("delegate_task"));
        assert!(description.contains("shares the parent workflow, queue, and claims"));
        assert!(description.contains("Do not start/reset it without explicit authorization"));
        schema.name.free_with(Some(free_string));
        schema.description.free_with(Some(free_string));
        schema.parameters.free_with(Some(free_string));
    }

    #[test]
    fn advertised_defaults_metadata_and_key_rules_match_parser() {
        let state = started();
        assert_eq!(
            apply_action(&state, &json!({"__rpi": {"trace": [1, {"id": "x"}]}})).unwrap(),
            state
        );
        let next = apply_action(
            &state,
            &json!({"action": "checkpoint", "step": "build", "note": null}),
        )
        .unwrap();
        assert_eq!(
            next.workflow.unwrap().steps[0].status,
            StepStatus::InProgress
        );
        assert!(apply_action(
            &state,
            &json!({"action": "checkpoint", "step": "build", "status": "failed"})
        )
        .is_err());
        assert!(apply_action(&state, &json!({"action": "start", "name": "x"})).is_err());
        assert!(parse_steps(Some(&json!([{"id": "x"}]))).is_err());
        assert!(parse_steps(Some(
            &json!([{"id": "x", "title": "X"}, {"id": " x ", "title": "Y"}])
        ))
        .is_err());
        assert!(apply_action(&state, &json!({"action": "enqueue"})).is_err());
        let queued = apply_action(&state, &json!({"action": "enqueue", "text": "work"})).unwrap();
        let claimed = apply_action(&queued, &json!({"action": "claim", "item": "item-1"})).unwrap();
        for status in [None, Some("pending"), Some("in_progress")] {
            let mut params = json!({"action": "finish", "item": "item-1"});
            if let Some(status) = status {
                params["status"] = json!(status);
            }
            assert!(apply_action(&claimed, &params).is_err());
        }
        for status in ["completed", "failed"] {
            assert!(apply_action(
                &claimed,
                &json!({"action": "finish", "item": "item-1", "status": status, "__rpi": {}})
            )
            .is_ok());
        }
        assert!(optional_text(Some(&json!("  ")), "note", MAX_NOTE_CHARS).is_err());
        assert_eq!(
            optional_text(Some(&Value::Null), "note", MAX_NOTE_CHARS).unwrap(),
            None
        );
    }

    fn started() -> DurableState {
        apply_action(
            &DurableState::default(),
            &json!({
                "action":"start",
                "name":"Release",
                "steps":[{"id":"build","title":"Build"},{"id":"test","title":"Test"}]
            }),
        )
        .unwrap()
    }

    #[test]
    fn start_creates_pending_steps() {
        let state = started();
        let workflow = state.workflow.unwrap();
        assert_eq!(workflow.status, WorkflowStatus::Running);
        assert_eq!(workflow.steps[0].status, StepStatus::Pending);
    }

    #[test]
    fn checkpoint_and_complete_follow_the_workflow() {
        let state = started();
        let state = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"build","status":"completed","note":"ok"}),
        )
        .unwrap();
        assert_eq!(
            state.workflow.as_ref().unwrap().steps[0].status,
            StepStatus::Completed
        );
        assert!(apply_action(&state, &json!({"action":"complete"})).is_err());
        let state = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"test","status":"completed"}),
        )
        .unwrap();
        let state = apply_action(&state, &json!({"action":"complete"})).unwrap();
        assert_eq!(state.workflow.unwrap().status, WorkflowStatus::Completed);
    }

    #[test]
    fn pause_resume_and_fail_validate_status() {
        let state = started();
        let state = apply_action(&state, &json!({"action":"pause","reason":"waiting"})).unwrap();
        assert_eq!(
            state.workflow.as_ref().unwrap().status,
            WorkflowStatus::Paused
        );
        let state = apply_action(&state, &json!({"action":"resume"})).unwrap();
        let state = apply_action(&state, &json!({"action":"fail","reason":"blocked"})).unwrap();
        assert_eq!(
            state.workflow.as_ref().unwrap().status,
            WorkflowStatus::Failed
        );
        assert!(apply_action(&state, &json!({"action":"resume"})).is_err());
    }

    #[test]
    fn rejects_duplicate_steps_and_unknown_step() {
        assert!(apply_action(
            &DurableState::default(),
            &json!({"action":"start","name":"x","steps":[{"id":"a","title":"A"},{"id":"a","title":"Again"}]})
        )
        .is_err());
        assert!(
            apply_action(&started(), &json!({"action":"checkpoint","step":"missing"})).is_err()
        );
    }

    #[test]
    fn state_round_trips_as_session_entry_data() {
        let state = started();
        let value = serde_json::to_value(&state).unwrap();
        let restored: DurableState = serde_json::from_value(value).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn reset_removes_workflow() {
        let state = apply_action(&started(), &json!({"action":"reset"})).unwrap();
        assert_eq!(state, DurableState::default());
    }

    #[test]
    fn interrupted_run_is_explicitly_recoverable_without_replaying_tools() {
        let mut state = DurableState::default();
        start_run(&mut state);
        assert_eq!(state.run.as_ref().unwrap().status, RunStatus::Running);
        record_tool_start(&mut state, "call-1", "shell", "{\"cmd\":\"make test\"}");

        let interrupted = mark_interrupted(state);
        let run = interrupted.run.as_ref().unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(run.tools[0].status, ToolExecutionStatus::Interrupted);

        let mut recovered = interrupted;
        recover_run(&mut recovered).unwrap();
        let run = recovered.run.as_ref().unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.recovery_of, "run-1");
        assert_eq!(run.tools[0].status, ToolExecutionStatus::Interrupted);
    }

    #[test]
    fn tool_journal_is_bounded_and_terminal_results_are_idempotent() {
        let mut state = DurableState::default();
        start_run(&mut state);
        for index in 0..(MAX_JOURNAL_ENTRIES + 3) {
            let id = format!("call-{index}");
            record_tool_start(&mut state, &id, "tool", "{}");
            record_tool_end(&mut state, &id, "tool", "ok", false);
        }
        let run = state.run.as_ref().unwrap();
        assert_eq!(run.tools.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(run.tools[0].call_id, "call-3");
        assert_eq!(run.tools[0].status, ToolExecutionStatus::Completed);

        record_tool_end(&mut state, "call-3", "tool", "different", true);
        let tool = &state.run.as_ref().unwrap().tools[0];
        assert_eq!(tool.status, ToolExecutionStatus::Completed);
        assert_eq!(tool.result, "ok");
    }

    #[test]
    fn queue_items_are_claimed_and_finished_once() {
        let state = apply_action(
            &DurableState::default(),
            &json!({"action":"enqueue","text":"Deploy the release"}),
        )
        .unwrap();
        assert_eq!(state.queue[0].id, "item-1");
        assert_eq!(state.queue[0].status, QueueItemStatus::Queued);

        let state = apply_action(&state, &json!({"action":"claim","item":"item-1"})).unwrap();
        assert_eq!(state.queue[0].status, QueueItemStatus::Claimed);
        let state = apply_action(
            &state,
            &json!({"action":"finish","item":"item-1","status":"completed","note":"verified"}),
        )
        .unwrap();
        assert_eq!(state.queue[0].status, QueueItemStatus::Completed);
        assert_eq!(state.queue[0].note, "verified");
        assert!(apply_action(&state, &json!({"action":"claim","item":"item-1"})).is_err());
    }

    #[test]
    fn queue_rejects_invalid_transitions_and_is_bounded() {
        let mut state = DurableState::default();
        for index in 0..MAX_QUEUE_ITEMS {
            state = apply_action(
                &state,
                &json!({"action":"enqueue","text":format!("task-{index}")}),
            )
            .unwrap();
        }
        assert!(apply_action(&state, &json!({"action":"enqueue","text":"overflow"})).is_err());
        assert!(apply_action(
            &state,
            &json!({"action":"finish","item":"item-1","status":"completed"})
        )
        .is_err());
        assert!(apply_action(&state, &json!({"action":"cancel","item":"missing"})).is_err());
    }

    fn append_logged(
        log: &mut Vec<Value>,
        fail: bool,
    ) -> impl FnOnce(Value) -> Result<String, String> + '_ {
        move |payload| {
            if fail {
                return Err("mock AppendEntry failure".into());
            }
            let id = format!("entry-{}", log.len() + 1);
            log.push(json!({
                "type": "custom",
                "customType": ENTRY_TYPE,
                "id": id,
                "data": payload["data"].clone(),
            }));
            Ok(id)
        }
    }

    fn commit_logged(
        state: &DurableState,
        meta: &JournalMeta,
        next: DurableState,
        log: &mut Vec<Value>,
    ) -> Result<(DurableState, JournalMeta), String> {
        commit_candidate(state, meta, next, append_logged(log, false))
    }

    #[test]
    fn ambiguous_result_before_start_replays_through_settle() {
        let mut log = Vec::new();
        let mut state = DurableState::default();
        let mut meta = JournalMeta::default();
        let call_id = "ambiguous-call";
        let params = "参数".repeat(400);
        let result = "结果".repeat(400);

        let mut candidate = state.clone();
        start_run(&mut candidate);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        record_tool_end(&mut candidate, call_id, "tool", &result, false);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        record_tool_start(&mut candidate, call_id, "tool", &params);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        if let Some(run) = candidate.run.as_mut() {
            run.status = RunStatus::Settled;
            run.outcome = "settled".into();
            for tool in &mut run.tools {
                if tool.status == ToolExecutionStatus::Started {
                    tool.status = ToolExecutionStatus::Interrupted;
                    tool.result = "run settled without a durable tool result".into();
                }
            }
        }
        let (committed, _) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        state = committed;
        assert_eq!(decode_branch(&log).unwrap().0, state);
    }

    #[test]
    fn long_tool_ids_use_distinct_stable_ids_and_replay_cleanly() {
        let mut log = Vec::new();
        let mut state = DurableState::default();
        let mut meta = JournalMeta::default();
        let prefix = "x".repeat(MAX_SUMMARY_CHARS);
        let first_id = format!("{prefix}a");
        let second_id = format!("{prefix}b");

        let mut candidate = state.clone();
        start_run(&mut candidate);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        record_tool_start(&mut candidate, &first_id, "tool", "first");
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        record_tool_start(&mut candidate, &second_id, "tool", "second");
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        record_tool_end(&mut candidate, &second_id, "tool", "second result", false);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        let tools = &state.run.as_ref().unwrap().tools;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].status, ToolExecutionStatus::Started);
        assert_eq!(tools[1].status, ToolExecutionStatus::Completed);
        assert_eq!(tools[1].result, "second result");
        assert_eq!(decode_branch(&log).unwrap().0, state);

        let mut candidate = state.clone();
        if let Some(run) = candidate.run.as_mut() {
            run.status = RunStatus::Settled;
            run.outcome = "settled".into();
            for tool in &mut run.tools {
                if tool.status == ToolExecutionStatus::Started {
                    tool.status = ToolExecutionStatus::Interrupted;
                    tool.result = "run settled without a durable tool result".into();
                }
            }
        }
        let (committed, _) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        state = committed;
        assert_eq!(decode_branch(&log).unwrap().0, state);
    }

    #[test]
    fn encoded_long_tool_ids_do_not_collide_with_short_native_ids() {
        let long_id = format!("{}tail", "x".repeat(MAX_SUMMARY_CHARS));
        let short_id = tool_call_id(&long_id);
        assert_eq!(short_id.chars().count(), MAX_SUMMARY_CHARS);

        let mut state = DurableState::default();
        start_run(&mut state);
        record_tool_start(&mut state, &long_id, "tool", "long");
        record_tool_start(&mut state, &short_id, "tool", "short");
        record_tool_end(&mut state, &short_id, "tool", "short result", false);

        let tools = &state.run.unwrap().tools;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].status, ToolExecutionStatus::Started);
        assert_eq!(tools[1].status, ToolExecutionStatus::Completed);
        assert_eq!(tools[1].result, "short result");
        assert_ne!(tools[0].call_key, tools[1].call_key);

        let value = serde_json::to_value(tools).unwrap();
        assert!(value[0]["callKey"].is_string());
        let legacy: Vec<ToolExecution> = serde_json::from_value(json!([{
            "callId": "legacy",
            "name": "tool",
            "status": "started"
        }]))
        .unwrap();
        assert_eq!(legacy[0].call_key, None);
    }

    #[test]
    fn old_hashed_long_tool_ids_remain_replayable_after_hash_upgrade() {
        let long_id = format!("{}tail", "x".repeat(MAX_SUMMARY_CHARS));
        let old_id = legacy_hashed_tool_id(&long_id).unwrap();
        let mut state = DurableState::default();
        start_run(&mut state);
        state.run.as_mut().unwrap().tools.push(ToolExecution {
            call_id: old_id,
            call_key: None,
            name: "tool".into(),
            params: String::new(),
            status: ToolExecutionStatus::Started,
            result: String::new(),
        });

        record_tool_end(&mut state, &long_id, "tool", "legacy result", false);
        let tool = &state.run.unwrap().tools[0];
        assert_eq!(tool.status, ToolExecutionStatus::Completed);
        assert_eq!(tool.result, "legacy result");
    }

    #[test]
    fn legacy_truncated_tool_ids_are_not_left_active_after_restore() {
        let mut state = DurableState::default();
        start_run(&mut state);
        state.run.as_mut().unwrap().tools.push(ToolExecution {
            call_id: format!("{}…", "x".repeat(MAX_SUMMARY_CHARS)),
            call_key: None,
            name: "tool".into(),
            params: String::new(),
            status: ToolExecutionStatus::Started,
            result: String::new(),
        });

        let restored = mark_unresolvable_legacy_tools(state);
        let tool = &restored.run.unwrap().tools[0];
        assert_eq!(tool.status, ToolExecutionStatus::Unknown);
        assert!(!tool.result.is_empty());
    }

    #[test]
    fn legacy_long_tool_ids_do_not_fallback_to_a_new_colliding_id() {
        let prefix = "x".repeat(MAX_SUMMARY_CHARS);
        let old_id = format!("{prefix}old");
        let new_id = format!("{prefix}new");
        let mut state = DurableState::default();
        start_run(&mut state);
        state.run.as_mut().unwrap().tools.push(ToolExecution {
            call_id: summary(&old_id),
            call_key: None,
            name: "tool".into(),
            params: "old".into(),
            status: ToolExecutionStatus::Started,
            result: String::new(),
        });

        record_tool_start(&mut state, &new_id, "tool", "new");
        record_tool_end(&mut state, &new_id, "tool", "new result", false);

        let tools = &state.run.as_ref().unwrap().tools;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].status, ToolExecutionStatus::Started);
        assert_eq!(tools[1].status, ToolExecutionStatus::Completed);
        assert_eq!(tools[1].result, "new result");
        assert_ne!(tools[0].call_id, tools[1].call_id);
    }

    fn append_action(
        state: DurableState,
        meta: &JournalMeta,
        params: &Value,
        log: &mut Vec<Value>,
    ) -> (DurableState, JournalMeta) {
        let next = apply_action(&state, params).unwrap();
        commit_logged(&state, meta, next, log).unwrap()
    }

    #[test]
    fn unversioned_snapshot_is_not_a_restore_base_or_delta_anchor() {
        let legacy = json!({
            "type":"custom", "customType":ENTRY_TYPE, "id":"legacy-1",
            "data":{"workflow":null}
        });
        assert!(decode_branch(std::slice::from_ref(&legacy)).is_err());
        let mut log = Vec::new();
        let (state, _) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            started(),
            &mut log,
        )
        .unwrap();
        let mut delta = log[0].clone();
        delta["data"]["kind"] = json!("delta");
        delta["data"].as_object_mut().unwrap().remove("state");
        delta["data"]["changes"] = json!([{"op":"setWorkflow","workflow":null}]);
        delta["data"]["baseEntryId"] = json!("legacy-1");
        assert!(decode_branch(&[legacy.clone(), delta]).is_err());

        // A canonical self-contained snapshot may obsolete an invalid prefix;
        // it does not migrate or use that prefix as a replay anchor.
        log.insert(0, legacy);
        let (restored, restored_meta) = decode_branch(&log).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored_meta.anchor_entry_id, Some("entry-1".into()));
        assert!(!restored_meta.force_snapshot);
    }

    #[test]
    fn journal_round_trip_keeps_all_100_tools_and_is_smaller_than_snapshots() {
        let mut log = Vec::new();
        let mut state = DurableState::default();
        let mut meta = JournalMeta::default();
        let mut full_snapshot_bytes = 0usize;
        let mut next = state.clone();
        start_run(&mut next);
        let (committed, next_meta) = commit_logged(&state, &meta, next, &mut log).unwrap();
        state = committed;
        meta = next_meta;
        full_snapshot_bytes += serde_json::to_vec(&state).unwrap().len();

        for index in 0..100 {
            let id = format!("call-{index}");
            let mut candidate = state.clone();
            record_tool_start(&mut candidate, &id, "tool", &"参数".repeat(400));
            let (committed, next_meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
            state = committed;
            meta = next_meta;
            full_snapshot_bytes += serde_json::to_vec(&state).unwrap().len();

            let mut candidate = state.clone();
            record_tool_end(&mut candidate, &id, "tool", &"结果".repeat(400), false);
            let (committed, next_meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
            state = committed;
            meta = next_meta;
            full_snapshot_bytes += serde_json::to_vec(&state).unwrap().len();
        }
        let journal_bytes: usize = log
            .iter()
            .map(|entry| serde_json::to_vec(&entry["data"]).unwrap().len())
            .sum();
        let (restored, _) = decode_branch(&log).unwrap();
        assert_eq!(restored, state);
        assert!(log.iter().any(|entry| entry["data"]["kind"] == "delta"));
        let snapshot_count = log
            .iter()
            .filter(|entry| entry["data"]["kind"] == "snapshot")
            .count();
        assert!(snapshot_count >= 2, "cadence did not emit later snapshots");
        assert!(journal_bytes < full_snapshot_bytes / 3);
        assert_eq!(state.run.as_ref().unwrap().tools.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(state.run.as_ref().unwrap().tools[0].call_id, "call-36");
    }

    #[test]
    fn workflow_queue_reset_and_forget_run_replay_exactly() {
        let mut log = Vec::new();
        let mut state = DurableState::default();
        let mut meta = JournalMeta::default();
        for action in [
            json!({"action":"start","name":"Ship","steps":[{"id":"one","title":"One"}]}),
            json!({"action":"checkpoint","step":"one","status":"completed"}),
            json!({"action":"enqueue","text":"test"}),
            json!({"action":"claim","item":"item-1"}),
            json!({"action":"finish","item":"item-1","status":"failed","note":"no"}),
        ] {
            (state, meta) = append_action(state, &meta, &action, &mut log);
        }
        let mut candidate = state.clone();
        start_run(&mut candidate);
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        let mut candidate = state.clone();
        candidate = apply_action(&candidate, &json!({"action":"reset"})).unwrap();
        (state, meta) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        let mut candidate = state.clone();
        candidate = apply_action(&candidate, &json!({"action":"forget_run"})).unwrap();
        let (final_state, _) = commit_logged(&state, &meta, candidate, &mut log).unwrap();
        state = final_state;
        let (restored, _) = decode_branch(&log).unwrap();
        assert_eq!(restored, state);
        assert!(restored.workflow.is_none());
        assert!(restored.run.is_none());
        assert_eq!(restored.queue[0].status, QueueItemStatus::Failed);
    }

    #[test]
    fn no_op_and_append_failure_do_not_advance_state_or_cadence() {
        let current = started();
        let meta = JournalMeta::default();
        let mut log = Vec::new();
        let before = (current.clone(), meta.clone());
        let (same, same_meta) = commit_candidate(&current, &meta, current.clone(), |_| {
            panic!("no-op must not append")
        })
        .unwrap();
        assert_eq!(same, before.0);
        assert_eq!(same_meta, before.1);

        let mut next = current.clone();
        next.workflow.as_mut().unwrap().reason = Some("later".into());
        assert!(
            commit_candidate(&current, &meta, next.clone(), append_logged(&mut log, true),)
                .is_err()
        );
        assert!(log.is_empty());
        assert_eq!(meta.anchor_entry_id, None);
        let (retry, retry_meta) =
            commit_candidate(&current, &meta, next, append_logged(&mut log, false)).unwrap();
        assert_eq!(retry.revision, 1);
        assert_eq!(retry_meta.anchor_entry_id, Some("entry-1".into()));
    }

    #[test]
    fn unversioned_snapshots_and_incomplete_journal_envelopes_are_rejected() {
        let native = |data: Value| {
            json!({
                "id": "native-entry",
                "type": "custom",
                "customType": ENTRY_TYPE,
                "data": data,
            })
        };

        let missing_discriminators = native(json!({
            "kind": "snapshot",
            "baseRevision": 0,
            "revision": 1,
            "state": {"workflow": null},
            "changes": []
        }));
        assert!(decode_branch(&[missing_discriminators]).is_err());
        assert!(decode_branch(&[native(json!({}))]).is_err());
        assert!(decode_branch(&[native(json!({"unknown": true}))]).is_err());
        assert!(decode_branch(&[native(json!({
            "workflow": {
                "name": "",
                "status": "running",
                "steps": [{"id": "s", "title": "S", "status": "pending"}]
            }
        }))])
        .is_err());
        assert!(decode_branch(&[native(json!({
            "workflow": null,
            "queue": [{
                "id": "",
                "text": "work",
                "status": "queued",
                "createdRevision": 0,
                "updatedRevision": 0
            }]
        }))])
        .is_err());

        for data in [
            json!({"workflow": null}),
            serde_json::to_value(DurableState::default()).unwrap(),
            serde_json::to_value(started()).unwrap(),
        ] {
            assert!(decode_branch(&[native(data)]).is_err());
        }
    }

    #[test]
    fn filtered_branch_rejects_foreign_and_malformed_native_envelopes() {
        let mut log = Vec::new();
        commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            started(),
            &mut log,
        )
        .unwrap();
        let mut malformed = vec![Value::Null, json!({}), json!({"type":"message"})];
        for (field, value) in [
            ("type", json!("message")),
            ("customType", json!("rpi-durable-runtime")),
            ("customType", json!("unrelated")),
            ("id", json!(null)),
            ("id", json!(7)),
            ("id", json!("")),
        ] {
            let mut entry = log[0].clone();
            entry[field] = value;
            malformed.push(entry);
        }
        for field in ["type", "customType", "id", "data"] {
            let mut entry = log[0].clone();
            entry.as_object_mut().unwrap().remove(field);
            malformed.push(entry);
        }
        for entry in malformed {
            assert!(
                decode_branch(std::slice::from_ref(&entry)).is_err(),
                "{entry}"
            );
            assert!(
                decode_branch(&[entry.clone(), log[0].clone()]).is_err(),
                "{entry}"
            );
            assert!(
                decode_branch(&[log[0].clone(), entry.clone()]).is_err(),
                "{entry}"
            );
        }
        let runtime = json!({
            "id":"old-runtime", "type":"custom", "customType":"rpi-durable-runtime",
            "data":{"run":null,"queue":[]}
        });
        assert!(decode_branch(&[runtime]).is_err());
    }

    #[test]
    fn legacy_after_versioned_record_is_rejected() {
        let mut log = Vec::new();
        let (state, meta) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            started(),
            &mut log,
        )
        .unwrap();
        let mut next = state.clone();
        next.workflow.as_mut().unwrap().reason = Some("new".into());
        let _ = commit_logged(&state, &meta, next, &mut log).unwrap();
        log.push(json!({
            "id": "downgrade",
            "type": "custom",
            "customType": ENTRY_TYPE,
            "data": {"workflow": null}
        }));
        assert!(decode_branch(&log).is_err());
    }

    #[test]
    fn malformed_gap_wrong_anchor_and_wrong_version_fail_closed() {
        let mut log = Vec::new();
        let current = DurableState::default();
        let meta = JournalMeta::default();
        let (state, meta) = commit_logged(&current, &meta, started(), &mut log).unwrap();
        let mut next = state.clone();
        start_run(&mut next);
        let (_state, _meta) = commit_logged(&state, &meta, next, &mut log).unwrap();
        let mut wrong_anchor = log.clone();
        wrong_anchor[1]["data"]["baseEntryId"] = json!("wrong-branch");
        assert!(decode_branch(&wrong_anchor).is_err());

        let mut wrong_version = log.clone();
        wrong_version[1]["data"]["version"] = json!(999);
        assert!(decode_branch(&wrong_version).is_err());

        let mut missing = log.clone();
        missing.remove(0);
        assert!(decode_branch(&missing).is_err());
        let mut malformed = log;
        malformed[0]["data"]["format"] = json!(JOURNAL_FORMAT);
        malformed[0]["data"]["kind"] = json!("delta");
        assert!(decode_branch(&malformed).is_err());
    }

    #[test]
    fn fork_prefix_and_navigation_do_not_cross_branch_deltas() {
        let mut prefix = Vec::new();
        let current = DurableState::default();
        let meta = JournalMeta::default();
        let (prefix_state, prefix_meta) =
            commit_logged(&current, &meta, started(), &mut prefix).unwrap();
        let mut left = prefix.clone();
        let mut right = prefix.clone();
        let mut left_next = prefix_state.clone();
        left_next.workflow.as_mut().unwrap().name = "left".into();
        let (left_state, _) =
            commit_logged(&prefix_state, &prefix_meta, left_next, &mut left).unwrap();
        let mut right_next = prefix_state.clone();
        right_next.workflow.as_mut().unwrap().name = "right".into();
        let (right_state, _) =
            commit_logged(&prefix_state, &prefix_meta, right_next, &mut right).unwrap();
        assert_eq!(decode_branch(&left).unwrap().0, left_state);
        assert_eq!(decode_branch(&right).unwrap().0, right_state);
        assert_ne!(
            decode_branch(&left).unwrap().0,
            decode_branch(&right).unwrap().0
        );
    }

    #[test]
    fn interruption_checkpoint_is_commit_before_publish_and_retries_after_failure() {
        let mut log = Vec::new();
        let current = DurableState::default();
        let meta = JournalMeta::default();
        let mut running = current.clone();
        start_run(&mut running);
        let (running, meta) = commit_logged(&current, &meta, running, &mut log).unwrap();
        let interrupted = mark_interrupted(running.clone());
        assert!(commit_candidate(
            &running,
            &meta,
            interrupted.clone(),
            append_logged(&mut log, true)
        )
        .is_err());
        assert_eq!(decode_branch(&log).unwrap().0, running);
        let (recovered, _) =
            commit_candidate(&running, &meta, interrupted, append_logged(&mut log, false)).unwrap();
        assert_eq!(
            recovered.run.as_ref().unwrap().status,
            RunStatus::Interrupted
        );
        assert_eq!(decode_branch(&log).unwrap().0, recovered);
    }

    #[test]
    fn native_entry_identity_uses_context_specific_string_fields() {
        assert_eq!(
            append_entry_identity(&json!({"entryId":"append-1"})),
            Some("append-1".into())
        );
        assert_eq!(
            branch_entry_identity(&json!({
                "type":"custom",
                "id":"branch-1",
                "customType":ENTRY_TYPE,
                "data":{"workflow":null}
            })),
            Some("branch-1".into())
        );
        assert_eq!(branch_entry_identity(&json!({"entryId":"wrong"})), None);
        assert_eq!(branch_entry_identity(&json!({"id":7})), None);
        assert_eq!(append_entry_identity(&json!({"entryId":7})), None);
    }

    #[test]
    fn complete_snapshot_is_a_self_contained_replay_base() {
        let mut log = Vec::new();
        let (state, meta) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            started(),
            &mut log,
        )
        .unwrap();
        let first_delta = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"build","status":"completed"}),
        )
        .unwrap();
        let (state, meta) = commit_logged(&state, &meta, first_delta, &mut log).unwrap();
        let mut snapshot_meta = meta;
        snapshot_meta.force_snapshot = true;
        let second_delta = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"test","status":"completed"}),
        )
        .unwrap();
        let (snapshot_state, snapshot_meta) =
            commit_logged(&state, &snapshot_meta, second_delta, &mut log).unwrap();
        assert_eq!(log[2]["data"]["kind"], "snapshot");

        let final_state = apply_action(
            &snapshot_state,
            &json!({"action":"checkpoint","step":"build","status":"in_progress","note":"recheck"}),
        )
        .unwrap();
        let (final_state, _) =
            commit_logged(&snapshot_state, &snapshot_meta, final_state, &mut log).unwrap();

        let truncated = log[2..].to_vec();
        assert_eq!(decode_branch(&truncated).unwrap().0, final_state);
        assert_eq!(decode_branch(&truncated[..1]).unwrap().0, snapshot_state);

        let mut corrupt_prefix = log;
        corrupt_prefix[1]["data"]["version"] = json!(999);
        assert_eq!(decode_branch(&corrupt_prefix).unwrap().0, final_state);
    }

    #[test]
    fn suffix_after_snapshot_still_requires_exact_delta_chain() {
        let mut log = Vec::new();
        let (state, meta) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            started(),
            &mut log,
        )
        .unwrap();
        let next = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"build","status":"completed"}),
        )
        .unwrap();
        let (state, meta) = commit_logged(&state, &meta, next, &mut log).unwrap();
        let next = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"test","status":"in_progress"}),
        )
        .unwrap();
        let (_, _) = commit_logged(&state, &meta, next, &mut log).unwrap();
        let mut gap = log.clone();
        gap[2]["data"]["revision"] = json!(99);
        assert!(decode_branch(&gap).is_err());
        let mut reordered = log.clone();
        reordered.swap(1, 2);
        assert!(decode_branch(&reordered).is_err());
        let mut wrong_base = log;
        wrong_base[2]["data"]["baseEntryId"] = json!("wrong-base");
        assert!(decode_branch(&wrong_base).is_err());
    }

    #[test]
    fn checkpoint_with_a_large_workflow_serializes_only_the_changed_step() {
        let steps = (0..MAX_STEPS)
            .map(|index| json!({"id":format!("step-{index}"),"title":format!("Step {index}")}))
            .collect::<Vec<_>>();
        let mut state = apply_action(
            &DurableState::default(),
            &json!({"action":"start","name":"Large","steps":steps}),
        )
        .unwrap();
        for step in state.workflow.as_mut().unwrap().steps.iter_mut().take(64) {
            step.note = Some("existing note".into());
        }
        let mut log = Vec::new();
        let (committed, meta) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            state,
            &mut log,
        )
        .unwrap();
        state = committed;
        let next = apply_action(
            &state,
            &json!({"action":"checkpoint","step":"step-127","status":"completed","note":"last"}),
        )
        .unwrap();
        let (_, _) = commit_logged(&state, &meta, next, &mut log).unwrap();
        let record = &log[1]["data"];
        assert_eq!(record["kind"], "delta");
        assert_eq!(record["changes"].as_array().unwrap().len(), 1);
        assert_eq!(record["changes"][0]["op"], "updateStep");
        assert_eq!(record["changes"][0]["step_id"], "step-127");
        assert!(
            serde_json::to_vec(record).unwrap().len()
                < serde_json::to_vec(&log[0]["data"]).unwrap().len()
        );
    }

    #[test]
    fn workflow_metadata_has_a_typed_delta() {
        let mut state = started();
        let mut log = Vec::new();
        let (committed, meta) = commit_logged(
            &DurableState::default(),
            &JournalMeta::default(),
            state,
            &mut log,
        )
        .unwrap();
        state = committed;
        let next = apply_action(&state, &json!({"action":"pause","reason":"waiting"})).unwrap();
        let (_, _) = commit_logged(&state, &meta, next, &mut log).unwrap();
        assert_eq!(log[1]["data"]["kind"], "delta");
        assert_eq!(log[1]["data"]["changes"][0]["op"], "setWorkflowMeta");
    }

    #[test]
    fn restore_errors_are_not_rendered_as_a_healthy_empty_state() {
        let rendered = error_result("status", "GetSessionBranch failed");
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["details"]["error"], "GetSessionBranch failed");
        assert!(value["details"].get("workflow").is_none());
        assert!(value["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Error:"));
    }

    #[test]
    fn old_checkpoint_data_is_not_restored_with_default_run_and_queue() {
        for data in [
            json!({"workflow":null}),
            json!({"workflow":null,"run":null,"queue":[],"revision":0}),
        ] {
            let entry = json!({
                "id":"old-checkpoint", "type":"custom", "customType":ENTRY_TYPE,
                "data":data
            });
            assert!(decode_branch(&[entry]).is_err());
        }
    }
}
