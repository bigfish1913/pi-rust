//! Session-owned durable control and shared prototype wire codec.
//!
//! Workflow/queue mutations use the harness's existing message-write gate and
//! conditional Session append. Explicit-run observations project only committed
//! native records and exact entry proofs, never plugin events. Hosts may reuse
//! [`protocol`] without depending on the CLI or installing an extension.

pub mod experimental_ledger;
mod experimental_policy;
pub use experimental_policy::ExperimentalDurablePolicy;
pub mod legacy;
pub mod protocol;
use rpi_durable::control::{ControlAction, ControlError, ControlState};
use rpi_durable::execution::{ExecutionControl, ExecutionError, RunCheckpoint, RunStatus};
use serde::{Deserialize, Serialize};

use crate::agent_harness::{AgentHarness, WeakAgentHarness};
use crate::session::types::{HistoryCheckpoint, HistoryIdentity};

pub(crate) const CONTROL_TYPE: &str = "rpi.durable.control.v1";
pub(crate) const CONTROL_FORMAT: &str = "rpi-native-durable-control";
pub(crate) const MAX_RECEIPTS: usize = 64;
pub(crate) const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Maximum lane operation starts examined by one execution index read, including
/// non-run and forgotten operations. Larger histories fail rather than truncate.
pub const MAX_EXECUTION_INDEX_STARTS: usize = 64;

/// Raw lane records consumed per explicit execution page, excluding one older
/// lookahead. Includes non-start records; not an underlying IO/allocation bound.
pub const MAX_EXECUTION_PAGE_RECORDS: usize = 64;

/// Reserved control types cannot be authored by public custom-entry APIs.
/// Legacy plugin records remain writable until the coordinated P8/P9 cutover.
pub(crate) fn reserved_control_type(custom_type: &str) -> bool {
    custom_type.starts_with("rpi.durable.")
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableMutation {
    pub request_id: String,
    pub expected: DurableScope,
    pub action: ControlAction,
    /// Required only for recover/forget_run. Never guess the latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Trusted host identity, never inferred from model-supplied tool metadata.
#[derive(Clone, Debug)]
pub struct DurableActor {
    pub(crate) id: String,
    pub(crate) may_replace_workflow: bool,
}
impl DurableActor {
    pub fn owner(id: impl Into<String>) -> Result<Self, DurableError> {
        Self::new(id.into(), true)
    }
    pub fn child(id: impl Into<String>) -> Result<Self, DurableError> {
        Self::new(id.into(), false)
    }
    fn new(id: String, may_replace_workflow: bool) -> Result<Self, DurableError> {
        validate_identity(&id)?;
        Ok(Self {
            id,
            may_replace_workflow,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableReceipt {
    pub request_id: String,
    pub actor: String,
    pub expected: DurableScope,
    pub action: ControlAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub entry_id: String,
    pub control_revision: u64,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableOutcome {
    pub receipt: DurableReceipt,
    pub snapshot: DurableSnapshot,
    pub replayed: bool,
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
    #[error("legacy durable journal requires explicit native import (P8)")]
    LegacyJournalPresent,
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

pub(crate) fn validate_identity(id: &str) -> Result<(), DurableError> {
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

pub(crate) fn validate_run_id(id: &str) -> Result<(), DurableError> {
    if id.is_empty()
        || id.trim() != id
        || id.chars().count() > 512
        || id.chars().any(char::is_control)
    {
        return Err(DurableError::InvalidRequest("invalid native run id".into()));
    }
    Ok(())
}

pub(crate) fn validate_execution_target(
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ControlRecord {
    pub format: String,
    pub version: u32,
    pub previous_entry_id: Option<String>,
    pub state: ControlState,
    #[serde(default)]
    pub execution_control: ExecutionControl,
    pub receipts: Vec<DurableReceipt>,
}

/// An immutable, bounded proof assembled only after the complete canonical
/// control branch has been read and checked. This is a local journal view: it
/// is not a writer capability, a cross-process fence, a cryptographic root, or
/// a power-loss/crash guarantee. In particular, callers must not manufacture
/// one from a snapshot or from a single newest record.
#[derive(Clone, Debug)]
pub struct VerifiedJournal {
    snapshot: DurableSnapshot,
    record: ControlRecord,
}

impl VerifiedJournal {
    /// Build the view after the branch reader has checked predecessor ordering
    /// and cumulative-record continuity. The constructor repeats the
    /// current-record checks so future callers cannot accidentally omit the
    /// final binding checks when introducing another reader.
    pub(crate) fn from_validated(
        snapshot: DurableSnapshot,
        record: ControlRecord,
    ) -> Result<Self, DurableError> {
        let invalid = || DurableError::InvalidJournal("invalid verified journal boundary".into());
        if record.format != CONTROL_FORMAT
            || record.version != 1
            || record.receipts.len() > MAX_RECEIPTS
            || record.state != snapshot.state
            || record.execution_control != snapshot.execution_control
            || record.state.revision != snapshot.scope.control_revision
        {
            return Err(invalid());
        }
        match (snapshot.scope.entry_id.as_deref(), record.receipts.last()) {
            (None, None) => {
                if record.previous_entry_id.is_some()
                    || record.state.revision != 0
                    || record.execution_control != ExecutionControl::default()
                {
                    return Err(invalid());
                }
            }
            (Some(entry_id), Some(last)) if entry_id == last.entry_id => {}
            _ => return Err(invalid()),
        }

        let mut requests = std::collections::HashSet::new();
        let mut entries = std::collections::HashSet::new();
        for (index, receipt) in record.receipts.iter().enumerate() {
            validate_identity(&receipt.request_id).map_err(|_| invalid())?;
            validate_identity(&receipt.actor).map_err(|_| invalid())?;
            validate_execution_target(&receipt.action, receipt.run_id.as_deref())
                .map_err(|_| invalid())?;
            if receipt.expected.session_id != snapshot.scope.session_id
                || receipt.expected.lane != snapshot.scope.lane
                || receipt.expected.history_revision > snapshot.scope.history_revision
                || receipt.expected.history_generation != snapshot.scope.history_generation
                || receipt.entry_id.is_empty()
                || !requests.insert(&receipt.request_id)
                || !entries.insert(&receipt.entry_id)
                || receipt.control_revision
                    != receipt
                        .expected
                        .control_revision
                        .checked_add(u64::from(receipt.changed))
                        .ok_or_else(invalid)?
            {
                return Err(invalid());
            }
            if index == 0 {
                if receipt.expected.entry_id.is_some() || receipt.expected.control_revision != 0 {
                    return Err(invalid());
                }
            } else {
                let previous = &record.receipts[index - 1];
                if receipt.expected.entry_id.as_deref() != Some(previous.entry_id.as_str())
                    || receipt.expected.control_revision != previous.control_revision
                {
                    return Err(invalid());
                }
            }
        }
        if let Some(last) = record.receipts.last() {
            if last.control_revision != record.state.revision
                || last.expected.entry_id != record.previous_entry_id
            {
                return Err(invalid());
            }
        }
        Ok(Self { snapshot, record })
    }

    pub fn snapshot(&self) -> &DurableSnapshot {
        &self.snapshot
    }

    pub fn receipts(&self) -> &[DurableReceipt] {
        &self.record.receipts
    }

    pub fn receipt(&self, request_id: &str) -> Option<&DurableReceipt> {
        self.record
            .receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
    }

    pub(crate) fn into_parts(self) -> (DurableSnapshot, ControlRecord) {
        (self.snapshot, self.record)
    }
}

/// A lightweight binding, not another transcript, cache, or writer.
/// Clones and separately-created bindings coordinate through the same harness.
/// Bind a child's actor to the parent's harness only when the host grants that
/// shared scope. Independent child harnesses stay independent by default.
#[derive(Clone)]
pub struct DurableService {
    harness: WeakAgentHarness,
    actor: DurableActor,
    lane: String,
}
impl DurableService {
    pub fn bind(harness: &AgentHarness, actor: DurableActor) -> Self {
        Self::bind_lane(harness, actor, harness.lane_name())
    }
    pub fn bind_lane(harness: &AgentHarness, actor: DurableActor, lane: impl Into<String>) -> Self {
        Self {
            harness: harness.downgrade(),
            actor,
            lane: lane.into(),
        }
    }
    fn owner(&self) -> Result<AgentHarness, DurableError> {
        Ok(self
            .harness
            .upgrade()
            .ok_or(DurableError::Closed)?
            .durable_on_lane(&self.lane))
    }
    /// Permanently stop durable mutation admission for this harness lifecycle,
    /// settle every mutation admitted before the seal, and return the canonical
    /// post-settlement snapshot captured under the same gate. Reads and exact
    /// receipt reconciliation remain available; a fresh harness is required to
    /// reopen mutation admission. This is not persisted in the journal.
    pub async fn seal(&self) -> Result<DurableSnapshot, DurableError> {
        self.owner()?.durable_seal(self.actor.clone()).await
    }
    /// Wait for preceding gate-ordered control work and backend writes to settle.
    ///
    /// This explicit barrier remains usable after the owner is closed, while a
    /// strong owner still exists. It does not inspect the journal or certify any
    /// mutation's outcome: use exact receipts on a live/reopened owner for that.
    /// Mutations reserve that gate before their first caller yield, even when
    /// the owned completion task has not run yet. Merely constructing a mutation
    /// future is not admission. This is not shutdown, a whole-run wait or a fence
    /// against future calls or independent owners. The host must stop ingress
    /// and coordinate its producers separately. Dropping this future cancels
    /// only this wait, not core-owned mutations.
    pub async fn drain(&self) -> Result<(), DurableError> {
        self.owner()?.durable_drain().await;
        Ok(())
    }
    pub async fn snapshot(&self) -> Result<DurableSnapshot, DurableError> {
        self.owner()?.durable_snapshot().await
    }
    /// Validate an exact canonical snapshot under the shared write gate.
    ///
    /// This is a read-only cut-point check for future compaction/checkpoint
    /// work: it returns the current projection only when every scope and state
    /// fact still matches. It never rewrites or truncates journal storage.
    pub async fn validate_snapshot(
        &self,
        expected: &DurableSnapshot,
    ) -> Result<DurableSnapshot, DurableError> {
        self.owner()?.durable_validate_snapshot(expected).await
    }
    /// Explicit opt-in history checkpoint publication. The caller must provide
    /// a validated checkpoint captured from the current storage identity; this
    /// operation never infers a snapshot, changes the default writer path, or
    /// treats an archive as current authorization proof.
    pub async fn publish_history_checkpoint(
        &self,
        checkpoint: HistoryCheckpoint,
    ) -> Result<HistoryIdentity, DurableError> {
        self.owner()?
            .durable_publish_history_checkpoint(checkpoint)
            .await
    }
    /// Observe the backend's physical history size when it has a stable file
    /// capability. `None` is a deliberate unsupported/unknown result and is
    /// never converted into a resource guarantee.
    pub async fn history_physical_bytes(&self) -> Result<Option<u64>, DurableError> {
        self.owner()?.durable_history_physical_bytes().await
    }
    /// Discover proven run metadata on the captured branch, excluding forgotten
    /// IDs. Missing branch proof is counted, not silently presented as a complete
    /// empty history. Oversized lane histories fail; no latest-run default or
    /// implicit recover/replay is introduced. Dropping this read only cancels it.
    pub async fn execution_index(&self) -> Result<DurableExecutionIndexSnapshot, DurableError> {
        self.owner()?.durable_execution_index().await
    }
    /// Read one bounded raw-record window, or explicitly continue its cursor.
    /// All records count toward the page budget, even when no run is returned.
    /// Cursor scope must match the current binding/branch/history/control scope;
    /// changes conflict rather than silently restarting. No worker holds the
    /// gate between calls; dropping this future cancels only this read.
    pub async fn execution_page(
        &self,
        cursor: Option<&DurableExecutionCursor>,
    ) -> Result<DurableExecutionPage, DurableError> {
        self.owner()?.durable_execution_page(cursor).await
    }
    pub async fn execution(&self, run_id: &str) -> Result<DurableExecutionSnapshot, DurableError> {
        validate_run_id(run_id)?;
        self.owner()?.durable_execution(run_id).await
    }
    pub async fn request_outcome(
        &self,
        request_id: &str,
    ) -> Result<Option<DurableReceipt>, DurableError> {
        validate_identity(request_id)?;
        self.owner()?
            .durable_request_outcome(request_id, &self.actor.id)
            .await
    }
    /// Reconcile an uncertain mutation without admitting another append.
    ///
    /// This is the read-only recovery path for a caller that lost its ACK or
    /// was restarted. The request must be identical, including its original
    /// expected scope; a reopened owner may have a fresh binding token, but the
    /// committed receipt remains authoritative when it is still the current
    /// canonical anchor. `None` means no receipt for this actor/request exists.
    /// A mismatch is a conflict, never a guessed replay or a new write.
    pub async fn reconcile(
        &self,
        request: &DurableMutation,
    ) -> Result<Option<DurableOutcome>, DurableError> {
        validate_identity(&request.request_id)?;
        validate_execution_target(&request.action, request.run_id.as_deref())?;
        if matches!(request.action, ControlAction::Status | ControlAction::Queue) {
            return Err(DurableError::InvalidRequest(
                "use snapshot for read-only actions".into(),
            ));
        }
        self.owner()?
            .durable_reconcile(request.clone(), self.actor.clone())
            .await
    }
    pub async fn mutate(&self, request: DurableMutation) -> Result<DurableOutcome, DurableError> {
        validate_identity(&request.request_id)?;
        validate_execution_target(&request.action, request.run_id.as_deref())?;
        if matches!(request.action, ControlAction::Status | ControlAction::Queue) {
            return Err(DurableError::InvalidRequest(
                "use snapshot for read-only actions".into(),
            ));
        }
        // Core admission reserves the shared gate and transfers its guard or
        // pinned waiter to the owned completion task before this caller yields.
        self.owner()?
            .durable_mutate(request, self.actor.clone())
            .await
    }
}
