//! Pure workflow and manual-queue control. No execution facts or persistence live here.
//!
//! `apply` prepares a candidate; the host must acknowledge persistence before publication.
//! It does not increment `revision`. Queue mutations stamp the *next* revision, so the
//! host must set that revision before calling `validate` on a committed candidate.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;

pub const MAX_STEPS: usize = 128;
pub const MAX_QUEUE_ITEMS: usize = 32;
pub const MAX_NAME_CHARS: usize = 500;
pub const MAX_ID_CHARS: usize = 128;
pub const MAX_TITLE_CHARS: usize = 1_000;
pub const MAX_TEXT_CHARS: usize = 4_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlState {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub next_queue_id: u64,
    #[serde(default)]
    pub workflow: Option<Workflow>,
    #[serde(default)]
    pub queue: Vec<QueueItem>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    #[default]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Assigned and enforced by the host, not inferred by this reducer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
}

/// Start input intentionally ignores supplied status/note: new steps are pending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepInput {
    pub id: String,
    pub title: String,
}

/// Unknown host-injected fields are accepted. Null finish/checkpoint notes leave
/// existing notes unchanged; omitted or null cancel notes become `"cancelled"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlAction {
    Status,
    Queue,
    Start {
        name: String,
        steps: Vec<StepInput>,
    },
    Checkpoint {
        step: String,
        #[serde(default, deserialize_with = "checkpoint_status")]
        status: StepStatus,
        #[serde(default)]
        note: Option<String>,
    },
    Pause {
        #[serde(default)]
        reason: Option<String>,
    },
    Resume,
    Complete,
    Fail {
        #[serde(default)]
        reason: Option<String>,
    },
    Reset,
    Enqueue {
        text: String,
    },
    Claim {
        item: String,
    },
    Finish {
        item: String,
        status: QueueStatus,
        #[serde(default)]
        note: Option<String>,
    },
    Cancel {
        item: String,
        #[serde(default = "default_cancel_note")]
        note: Option<String>,
    },
    Recover,
    ForgetRun,
}

fn default_cancel_note() -> Option<String> {
    Some("cancelled".into())
}

fn checkpoint_status<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<StepStatus, D::Error> {
    Ok(Option::<StepStatus>::deserialize(d)?.unwrap_or_default())
}

/// Stable, typed policy errors; callers need not parse display strings.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },
    #[error("{field} exceeds {max} Unicode scalar values")]
    TextTooLong { field: &'static str, max: usize },
    #[error("stored {field} must be trimmed")]
    UntrimmedText { field: &'static str },
    #[error("workflow must contain 1..={MAX_STEPS} steps")]
    InvalidStepCount,
    #[error("duplicate step id: {id}")]
    DuplicateStepId { id: String },
    #[error("no workflow is active")]
    NoWorkflow,
    #[error("cannot {action} a {status:?} workflow")]
    WorkflowTransition {
        action: &'static str,
        status: WorkflowStatus,
    },
    #[error("step not found: {id}")]
    StepNotFound { id: String },
    #[error("all steps must be completed before completing the workflow")]
    IncompleteSteps,
    #[error("running/completed workflow cannot retain a reason")]
    UnexpectedWorkflowReason,
    #[error("queue cannot contain more than {MAX_QUEUE_ITEMS} items")]
    QueueFull,
    #[error("invalid queue id: {id}")]
    InvalidQueueId { id: String },
    #[error("duplicate queue id: {id}")]
    DuplicateQueueId { id: String },
    #[error("queue counter precedes item: {id}")]
    QueueCounterBehind { id: String },
    #[error("queue item not found: {id}")]
    QueueItemNotFound { id: String },
    #[error("cannot {action} a {status:?} queue item")]
    QueueTransition {
        action: &'static str,
        status: QueueStatus,
    },
    #[error("finish status must be completed or failed")]
    InvalidFinishStatus,
    #[error("invalid queue revisions for {id}")]
    InvalidQueueRevisions { id: String },
    #[error("queued item cannot have a claim owner: {id}")]
    UnexpectedClaimOwner { id: String },
    #[error("control revision overflow")]
    RevisionOverflow,
    #[error("queue id counter overflow")]
    QueueIdOverflow,
    #[error("native execution facts are required")]
    ExecutionFactsRequired,
}

type Result<T> = std::result::Result<T, ControlError>;

fn text(value: &str, field: &'static str, max: usize) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ControlError::EmptyText { field });
    }
    if trimmed.chars().count() > max {
        return Err(ControlError::TextTooLong { field, max });
    }
    Ok(trimmed.to_owned())
}

fn optional_text(value: &Option<String>, field: &'static str) -> Result<Option<String>> {
    value
        .as_deref()
        .map(|s| text(s, field, MAX_TEXT_CHARS))
        .transpose()
}

fn stored_text(value: &str, field: &'static str, max: usize) -> Result<()> {
    let trimmed = text(value, field, max)?;
    if trimmed != value {
        return Err(ControlError::UntrimmedText { field });
    }
    Ok(())
}

fn stored_optional(value: &Option<String>, field: &'static str) -> Result<()> {
    if let Some(value) = value {
        stored_text(value, field, MAX_TEXT_CHARS)?;
    }
    Ok(())
}

impl ControlState {
    /// Validate committed/restored state, including counters and revision stamps.
    /// A prepared queue candidate must first receive its host-committed revision.
    pub fn validate(&self) -> Result<()> {
        if let Some(workflow) = &self.workflow {
            stored_text(&workflow.name, "name", MAX_NAME_CHARS)?;
            if workflow.steps.is_empty() || workflow.steps.len() > MAX_STEPS {
                return Err(ControlError::InvalidStepCount);
            }
            stored_optional(&workflow.reason, "reason")?;
            if matches!(
                workflow.status,
                WorkflowStatus::Running | WorkflowStatus::Completed
            ) && workflow.reason.is_some()
            {
                return Err(ControlError::UnexpectedWorkflowReason);
            }
            let mut ids = HashSet::new();
            for step in &workflow.steps {
                stored_text(&step.id, "step id", MAX_ID_CHARS)?;
                stored_text(&step.title, "step title", MAX_TITLE_CHARS)?;
                stored_optional(&step.note, "note")?;
                if !ids.insert(&step.id) {
                    return Err(ControlError::DuplicateStepId {
                        id: step.id.clone(),
                    });
                }
            }
            if workflow.status == WorkflowStatus::Completed
                && workflow
                    .steps
                    .iter()
                    .any(|s| s.status != StepStatus::Completed)
            {
                return Err(ControlError::IncompleteSteps);
            }
        }
        if self.queue.len() > MAX_QUEUE_ITEMS {
            return Err(ControlError::QueueFull);
        }
        let mut ids = HashSet::new();
        for item in &self.queue {
            let suffix = item
                .id
                .strip_prefix("item-")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0 && item.id == format!("item-{n}"))
                .ok_or_else(|| ControlError::InvalidQueueId {
                    id: item.id.clone(),
                })?;
            if !ids.insert(&item.id) {
                return Err(ControlError::DuplicateQueueId {
                    id: item.id.clone(),
                });
            }
            if suffix > self.next_queue_id {
                return Err(ControlError::QueueCounterBehind {
                    id: item.id.clone(),
                });
            }
            stored_text(&item.text, "text", MAX_TEXT_CHARS)?;
            if !item.note.is_empty() {
                stored_text(&item.note, "note", MAX_TEXT_CHARS)?;
            }
            if let Some(owner) = &item.claimed_by {
                stored_text(owner, "claimed by", MAX_ID_CHARS)?;
                if item.status == QueueStatus::Queued {
                    return Err(ControlError::UnexpectedClaimOwner {
                        id: item.id.clone(),
                    });
                }
            }
            if item.created_revision == 0
                || item.created_revision > item.updated_revision
                || item.updated_revision > self.revision
            {
                return Err(ControlError::InvalidQueueRevisions {
                    id: item.id.clone(),
                });
            }
        }
        Ok(())
    }

    /// Prepare a candidate without changing this state or advancing its revision.
    /// The host owns durability, no-op detection, claim ownership and publication.
    pub fn apply(&self, action: &ControlAction) -> Result<Self> {
        if matches!(action, ControlAction::Recover | ControlAction::ForgetRun) {
            return Err(ControlError::ExecutionFactsRequired);
        }
        self.validate()?;
        let mut next = self.clone();
        match action {
            ControlAction::Status | ControlAction::Queue => {}
            ControlAction::Start { name, steps } => {
                let name = text(name, "name", MAX_NAME_CHARS)?;
                if steps.is_empty() || steps.len() > MAX_STEPS {
                    return Err(ControlError::InvalidStepCount);
                }
                let mut ids = HashSet::new();
                let steps = steps
                    .iter()
                    .map(|step| {
                        let id = text(&step.id, "step id", MAX_ID_CHARS)?;
                        let title = text(&step.title, "step title", MAX_TITLE_CHARS)?;
                        if !ids.insert(id.clone()) {
                            return Err(ControlError::DuplicateStepId { id });
                        }
                        Ok(Step {
                            id,
                            title,
                            status: StepStatus::Pending,
                            note: None,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                next.workflow = Some(Workflow {
                    name,
                    steps,
                    status: WorkflowStatus::Running,
                    reason: None,
                });
            }
            ControlAction::Checkpoint { step, status, note } => {
                let id = text(step, "step", MAX_ID_CHARS)?;
                let note = optional_text(note, "note")?;
                let workflow = next.workflow_mut()?;
                workflow.require_status("checkpoint", &[WorkflowStatus::Running])?;
                let step = workflow
                    .steps
                    .iter_mut()
                    .find(|s| s.id == id)
                    .ok_or(ControlError::StepNotFound { id })?;
                step.status = *status;
                if note.is_some() {
                    step.note = note;
                }
            }
            ControlAction::Pause { reason } => {
                let reason = optional_text(reason, "reason")?;
                let workflow = next.workflow_mut()?;
                workflow.require_status("pause", &[WorkflowStatus::Running])?;
                workflow.status = WorkflowStatus::Paused;
                workflow.reason = reason;
            }
            ControlAction::Resume => {
                let workflow = next.workflow_mut()?;
                workflow.require_status("resume", &[WorkflowStatus::Paused])?;
                workflow.status = WorkflowStatus::Running;
                workflow.reason = None;
            }
            ControlAction::Complete => {
                let workflow = next.workflow_mut()?;
                workflow.require_status("complete", &[WorkflowStatus::Running])?;
                if workflow
                    .steps
                    .iter()
                    .any(|s| s.status != StepStatus::Completed)
                {
                    return Err(ControlError::IncompleteSteps);
                }
                workflow.status = WorkflowStatus::Completed;
                workflow.reason = None;
            }
            ControlAction::Fail { reason } => {
                let reason = optional_text(reason, "reason")?
                    .unwrap_or_else(|| "workflow marked as failed".into());
                let workflow = next.workflow_mut()?;
                workflow
                    .require_status("fail", &[WorkflowStatus::Running, WorkflowStatus::Paused])?;
                workflow.status = WorkflowStatus::Failed;
                workflow.reason = Some(reason);
            }
            ControlAction::Reset => next.workflow = None,
            ControlAction::Enqueue { text: value } => {
                if next.queue.len() >= MAX_QUEUE_ITEMS {
                    return Err(ControlError::QueueFull);
                }
                let text = text(value, "text", MAX_TEXT_CHARS)?;
                next.next_queue_id = next
                    .next_queue_id
                    .checked_add(1)
                    .ok_or(ControlError::QueueIdOverflow)?;
                let revision = self.next_revision()?;
                next.queue.push(QueueItem {
                    id: format!("item-{}", next.next_queue_id),
                    text,
                    status: QueueStatus::Queued,
                    created_revision: revision,
                    updated_revision: revision,
                    note: String::new(),
                    claimed_by: None,
                });
            }
            ControlAction::Claim { item } => {
                let item = next.queue_item_mut(item)?;
                item.require_status("claim", &[QueueStatus::Queued])?;
                item.status = QueueStatus::Claimed;
                item.updated_revision = self.next_revision()?;
            }
            ControlAction::Finish { item, status, note } => {
                if !matches!(status, QueueStatus::Completed | QueueStatus::Failed) {
                    return Err(ControlError::InvalidFinishStatus);
                }
                let note = optional_text(note, "note")?;
                let item = next.queue_item_mut(item)?;
                item.require_status("finish", &[QueueStatus::Claimed])?;
                item.status = *status;
                item.updated_revision = self.next_revision()?;
                if let Some(note) = note {
                    item.note = note;
                }
            }
            ControlAction::Cancel { item, note } => {
                let note = optional_text(note, "note")?.unwrap_or_else(|| "cancelled".into());
                let item = next.queue_item_mut(item)?;
                item.require_status("cancel", &[QueueStatus::Queued, QueueStatus::Claimed])?;
                item.status = QueueStatus::Cancelled;
                item.updated_revision = self.next_revision()?;
                item.note = note;
            }
            ControlAction::Recover | ControlAction::ForgetRun => unreachable!(),
        }
        if next != *self {
            self.next_revision()?;
        }
        Ok(next)
    }

    fn next_revision(&self) -> Result<u64> {
        self.revision
            .checked_add(1)
            .ok_or(ControlError::RevisionOverflow)
    }

    fn workflow_mut(&mut self) -> Result<&mut Workflow> {
        self.workflow.as_mut().ok_or(ControlError::NoWorkflow)
    }

    fn queue_item_mut(&mut self, id: &str) -> Result<&mut QueueItem> {
        let id = text(id, "item", MAX_ID_CHARS)?;
        self.queue
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(ControlError::QueueItemNotFound { id })
    }
}

impl Workflow {
    fn require_status(&self, action: &'static str, allowed: &[WorkflowStatus]) -> Result<()> {
        if !allowed.contains(&self.status) {
            return Err(ControlError::WorkflowTransition {
                action,
                status: self.status,
            });
        }
        Ok(())
    }
}

impl QueueItem {
    fn require_status(&self, action: &'static str, allowed: &[QueueStatus]) -> Result<()> {
        if !allowed.contains(&self.status) {
            return Err(ControlError::QueueTransition {
                action,
                status: self.status,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
