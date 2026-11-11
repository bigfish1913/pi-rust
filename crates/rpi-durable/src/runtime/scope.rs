//! Scope and bounded observation DTOs for the durable runtime.

use crate::domain::control::ControlState;
use crate::domain::execution::{ExecutionControl, RunCheckpoint, RunStatus};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableScope {
    pub session_id: String,
    pub lane: String,
    /// In-process binding identity, NOT durable cross-process generation fencing.
    pub binding_token: String,
    pub leaf_id: Option<String>,
    pub history_revision: u64,
    /// Persistent whole-history replacement generation. Zero is the
    /// compatibility value for legacy journals and unsupported replacements.
    #[serde(default)]
    pub history_generation: u64,
    pub entry_id: Option<String>,
    pub control_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableSnapshot {
    pub scope: DurableScope,
    pub state: ControlState,
    #[serde(default)]
    pub execution_control: ExecutionControl,
}

/// Metadata from a fully proven native run, not a second execution fact log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableRunSummary {
    pub id: String,
    pub started_seq: u64,
    pub finished_record_id: Option<String>,
    pub finished_seq: Option<u64>,
    pub status: RunStatus,
}

/// Bounded discovery on the captured branch; never automatic run selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableExecutionIndexSnapshot {
    pub scope: DurableScope,
    /// Proven, non-forgotten runs, in descending start sequence order.
    pub runs: Vec<DurableRunSummary>,
    /// Candidate runs not provable on this branch (including sibling runs or
    /// missing anchors). Their identities are not exposed as branch members.
    pub unavailable_runs: usize,
    pub control: ExecutionControl,
}

/// Explicit continuation of a frozen-scope execution scan. This is an unsigned
/// position, not authorization or proof of complete traversal. Callers may not
/// combine pages from different scopes; any observed scope change conflicts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableExecutionCursor {
    pub scope: DurableScope,
    /// Exclusive upper bound in the native history sequence domain.
    pub before_seq: u64,
}

/// One newest-first window of lane records, with only fully proven run starts
/// exposed. An empty run list can still have a continuation. No total run count
/// or automatic latest/recovery decision is implied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableExecutionPage {
    pub scope: DurableScope,
    pub runs: Vec<DurableRunSummary>,
    pub unavailable_runs: usize,
    pub control: ExecutionControl,
    pub scanned_records: usize,
    pub next: Option<DurableExecutionCursor>,
}

/// Explicit-run observation: no latest-run inference from lane-global history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableExecutionSnapshot {
    pub scope: DurableScope,
    pub run: Option<RunCheckpoint>,
    pub control: ExecutionControl,
}
