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
use crate::agent_harness::{AgentHarness, WeakAgentHarness};
use crate::session::types::{HistoryCheckpoint, HistoryIdentity};
use rpi_durable::control::ControlAction;
pub use rpi_durable::runtime::{
    validate_execution_target, validate_identity, validate_run_id, ControlRecord, DurableActor,
    DurableError, DurableExecutionCursor, DurableExecutionIndexSnapshot, DurableExecutionPage,
    DurableExecutionSnapshot, DurableMutation, DurableOutcome, DurableReceipt, DurableRunSummary,
    DurableScope, DurableSnapshot, VerifiedJournal, CONTROL_FORMAT, MAX_RECEIPTS,
};

pub(crate) const CONTROL_TYPE: &str = "rpi.durable.control.v1";
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
            .durable_request_outcome(request_id, self.actor.id())
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
