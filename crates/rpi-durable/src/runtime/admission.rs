//! Admission requests, errors and identity validation.

use crate::domain::control::{ControlAction, ControlError};
use crate::domain::execution::ExecutionError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableMutation {
    pub request_id: String,
    pub expected: super::scope::DurableScope,
    pub action: ControlAction,
    /// Required only for recover/forget_run. Never guess the latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DurableError {
    #[error("durable owner is closed")]
    Closed,
    #[error("durable lifecycle is sealed")]
    Sealed,
    #[error("durable mutation is blocked during native recovery")]
    Recovering,
    #[error("durable branch/revision/request conflict")]
    Conflict,
    #[error("durable actor is not authorized for this action or claim")]
    PermissionDenied,
    #[error("invalid durable request: {0}")]
    InvalidRequest(String),
    #[error("invalid durable control journal: {0}")]
    InvalidJournal(String),
    #[error("unsupported durable journal format")]
    UnsupportedJournalFormat,
    #[error("durable bounded journal/request budget exhausted")]
    LimitExceeded,
    #[error("durable observation failed: {0}")]
    ReadFailure(String),
    #[error("durable commit outcome is uncertain; inspect or retry the identical request: {0}")]
    CommitUncertain(String),
    #[error("native execution is unavailable on the selected branch: {0}")]
    ExecutionUnavailable(String),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error(transparent)]
    Control(#[from] ControlError),
}

pub fn validate_identity(id: &str) -> Result<(), DurableError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(DurableError::InvalidRequest(
            "identity must be 1..128 ASCII alphanumeric/dot/dash/underscore bytes".into(),
        ));
    }
    Ok(())
}

pub fn validate_run_id(id: &str) -> Result<(), DurableError> {
    if id.is_empty()
        || id.trim() != id
        || id.chars().count() > 512
        || id.chars().any(char::is_control)
    {
        return Err(DurableError::InvalidRequest("invalid native run id".into()));
    }
    Ok(())
}

pub fn validate_execution_target(
    action: &ControlAction,
    run_id: Option<&str>,
) -> Result<(), DurableError> {
    match (
        matches!(action, ControlAction::Recover | ControlAction::ForgetRun),
        run_id,
    ) {
        (true, Some(id)) => validate_run_id(id),
        (false, None) => Ok(()),
        _ => Err(DurableError::InvalidRequest(
            "runId is required only for recover/forget_run".into(),
        )),
    }
}
