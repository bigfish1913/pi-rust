//! P3 partial execution-core contract: validated native checkpoints and recovery intent.
//!
//! This is not a CLI, scheduler, executor, or persistence layer. The host supplies the
//! explicitly selected run and proves branch membership and committed storage records.
//! Recovery records intent only: even replay-safe tools are never executed here. Apply
//! returns a candidate for host persistence; original run facts remain immutable.

use crate::control::ControlAction;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_TOOLS: usize = 64;
pub const MAX_FORGOTTEN_RUNS: usize = 64;
const MAX_ID_CHARS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Open,
    Interrupted,
    Completed,
    Aborted,
    Failed,
    Declined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Unknown,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Never,
    Safe,
}

/// References only: never contains tool arguments, result content, or transcript text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCheckpoint {
    /// Native tool-started record ID, not the model-supplied call ID.
    pub invocation_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
    pub call_id: String,
    pub name: String,
    pub reserved_result_entry_id: String,
    /// Host-proven committed result reference. A synthetic interruption result may
    /// be present while the actual tool outcome remains Unknown.
    pub result_entry_id: Option<String>,
    pub status: ToolStatus,
    pub replay: ReplayPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunCheckpoint {
    pub id: String,
    pub started_seq: u64,
    pub finished_record_id: Option<String>,
    pub finished_seq: Option<u64>,
    pub status: RunStatus,
    pub final_entry_id: Option<String>,
    pub assistant_entry_id: Option<String>,
    pub tools: Vec<ToolCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryRequest {
    pub run_id: String,
    pub started_seq: u64,
    pub observed_history_revision: u64,
    /// Native invocation IDs in checkpoint order, regardless of replay policy.
    pub unknown_invocation_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionControl {
    /// Run IDs hidden from explicit historical queries, without deleting audit facts.
    /// Bounded by MAX_FORGOTTEN_RUNS; forgetting never evicts an earlier ID.
    #[serde(default)]
    pub forgotten_run_ids: Vec<String>,
    #[serde(default)]
    pub recovery_request: Option<RecoveryRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionError {
    #[error("invalid execution facts: {0}")]
    InvalidFacts(String),
    #[error("no visible execution run was supplied")]
    NoRun,
    #[error("forgotten run limit reached; existing hidden runs are retained")]
    ForgetLimit,
    #[error("recovery requires a natively interrupted run")]
    NotInterrupted,
    #[error("recovery has already been requested for this run")]
    AlreadyRequested,
    #[error("action is not supported by execution control")]
    UnsupportedAction,
}

type Result<T> = std::result::Result<T, ExecutionError>;

fn invalid(message: impl Into<String>) -> ExecutionError {
    ExecutionError::InvalidFacts(message.into())
}

fn identifier(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > MAX_ID_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "{field} must be nonblank, trimmed, control-free, and at most {MAX_ID_CHARS} Unicode scalar values"
        )));
    }
    Ok(())
}

fn optional_identifier(value: Option<&str>, field: &str) -> Result<()> {
    if let Some(value) = value {
        identifier(value, field)?;
    }
    Ok(())
}

impl RunCheckpoint {
    /// Checks structural consistency, not actual storage proof or branch membership.
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id, "run id")?;
        if self.started_seq == 0 {
            return Err(invalid("started sequence must be positive"));
        }
        optional_identifier(self.finished_record_id.as_deref(), "finished record id")?;
        optional_identifier(self.final_entry_id.as_deref(), "final entry id")?;
        optional_identifier(self.assistant_entry_id.as_deref(), "assistant entry id")?;
        match (&self.finished_record_id, self.finished_seq) {
            (None, None) if self.status == RunStatus::Open => {}
            (Some(_), Some(seq)) if self.status != RunStatus::Open && seq > self.started_seq => {}
            _ => return Err(invalid("run status and finished record/sequence disagree")),
        }
        if self.status == RunStatus::Open
            && (self.final_entry_id.is_some() || self.assistant_entry_id.is_some())
        {
            return Err(invalid("open run cannot have terminal entry references"));
        }
        if self.tools.len() > MAX_TOOLS {
            return Err(invalid(format!("run exceeds {MAX_TOOLS} tools")));
        }
        let mut invocations = HashSet::new();
        let mut positions = HashSet::new();
        let mut results = HashSet::new();
        for tool in &self.tools {
            identifier(&tool.invocation_id, "invocation id")?;
            identifier(&tool.assistant_entry_id, "tool assistant entry id")?;
            identifier(&tool.call_id, "call id")?;
            identifier(&tool.name, "tool name")?;
            identifier(&tool.reserved_result_entry_id, "reserved result entry id")?;
            optional_identifier(tool.result_entry_id.as_deref(), "result entry id")?;
            if !invocations.insert(&tool.invocation_id) {
                return Err(invalid("duplicate invocation id"));
            }
            if !positions.insert((&tool.assistant_entry_id, tool.tool_index)) {
                return Err(invalid("duplicate assistant entry/tool index"));
            }
            if !results.insert(&tool.reserved_result_entry_id) {
                return Err(invalid("duplicate reserved result entry id"));
            }
            if let Some(result) = &tool.result_entry_id {
                if result != &tool.reserved_result_entry_id {
                    return Err(invalid(
                        "result entry id differs from reserved result entry id",
                    ));
                }
            } else if tool.status != ToolStatus::Unknown {
                return Err(invalid(
                    "known tool outcome requires a committed result entry id",
                ));
            }
        }
        Ok(())
    }
}

impl RecoveryRequest {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.run_id, "recovery run id")?;
        if self.started_seq == 0 || self.observed_history_revision < self.started_seq {
            return Err(invalid("invalid recovery sequence/revision"));
        }
        if self.unknown_invocation_ids.len() > MAX_TOOLS {
            return Err(invalid(format!("recovery exceeds {MAX_TOOLS} invocations")));
        }
        let mut ids = HashSet::new();
        for id in &self.unknown_invocation_ids {
            identifier(id, "unknown invocation id")?;
            if !ids.insert(id) {
                return Err(invalid("duplicate unknown invocation id"));
            }
        }
        Ok(())
    }
}

impl ExecutionControl {
    pub fn validate(&self) -> Result<()> {
        if self.forgotten_run_ids.len() > MAX_FORGOTTEN_RUNS {
            return Err(invalid(format!(
                "control exceeds {MAX_FORGOTTEN_RUNS} forgotten runs"
            )));
        }
        let mut ids = HashSet::new();
        for id in &self.forgotten_run_ids {
            identifier(id, "forgotten run id")?;
            if !ids.insert(id) {
                return Err(invalid("duplicate forgotten run id"));
            }
        }
        if let Some(request) = &self.recovery_request {
            request.validate()?;
            if self.forgotten_run_ids.contains(&request.run_id) {
                return Err(invalid("recovery cannot target a forgotten run"));
            }
        }
        Ok(())
    }

    /// Prepares recovery intent or hides the explicitly supplied run. Never infers
    /// interruption, synthesizes runs, executes tools, or changes workflow/queue state.
    pub fn apply(
        &self,
        action: &ControlAction,
        run: Option<&RunCheckpoint>,
        observed_history_revision: u64,
    ) -> Result<Self> {
        self.validate()?;
        if !matches!(action, ControlAction::Recover | ControlAction::ForgetRun) {
            return Err(ExecutionError::UnsupportedAction);
        }
        let run = run.ok_or(ExecutionError::NoRun)?;
        run.validate()?;
        if self.forgotten_run_ids.contains(&run.id) {
            return Err(ExecutionError::NoRun);
        }
        let mut candidate = self.clone();
        match action {
            ControlAction::Recover => {
                if run.status != RunStatus::Interrupted {
                    return Err(ExecutionError::NotInterrupted);
                }
                // Validation above guarantees a terminal record and sequence. No
                // sequence arithmetic is needed, including at u64::MAX.
                let finished_seq = run
                    .finished_seq
                    .ok_or_else(|| invalid("interrupted run requires a finished sequence"))?;
                if observed_history_revision < finished_seq
                    || observed_history_revision < run.started_seq
                {
                    return Err(invalid("observed history precedes the selected run"));
                }
                if self
                    .recovery_request
                    .as_ref()
                    .is_some_and(|request| request.run_id == run.id)
                {
                    return Err(ExecutionError::AlreadyRequested);
                }
                candidate.recovery_request = Some(RecoveryRequest {
                    run_id: run.id.clone(),
                    started_seq: run.started_seq,
                    observed_history_revision,
                    unknown_invocation_ids: run
                        .tools
                        .iter()
                        .filter(|tool| tool.status == ToolStatus::Unknown)
                        .map(|tool| tool.invocation_id.clone())
                        .collect(),
                });
            }
            ControlAction::ForgetRun => {
                if candidate.forgotten_run_ids.len() == MAX_FORGOTTEN_RUNS {
                    return Err(ExecutionError::ForgetLimit);
                }
                candidate.forgotten_run_ids.push(run.id.clone());
                if candidate
                    .recovery_request
                    .as_ref()
                    .is_some_and(|request| request.run_id == run.id)
                {
                    candidate.recovery_request = None;
                }
            }
            _ => return Err(ExecutionError::UnsupportedAction),
        }
        candidate.validate()?;
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests;
