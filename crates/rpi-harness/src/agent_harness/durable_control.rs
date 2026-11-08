//! Harness admission and conditional-append integration for durable control.
use super::*;
use crate::durable::{
    validate_execution_target, validate_identity, ControlRecord, DurableActor, DurableError,
    DurableMutation, DurableOutcome, DurableReceipt, DurableScope, DurableSnapshot,
    VerifiedJournal, CONTROL_FORMAT, CONTROL_TYPE, MAX_RECEIPTS, MAX_RECORD_BYTES,
    MAX_REQUEST_BYTES,
};
use crate::session::types::{HistoryCheckpoint, HistoryIdentity};
use rpi_durable::control::{ControlAction, ControlState, QueueStatus};
use rpi_durable::execution::ExecutionControl;
use std::collections::HashSet;
use std::io::Write;

struct BudgetWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "durable serialization budget exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bounded<T: serde::Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, DurableError> {
    let mut writer = BudgetWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| DurableError::LimitExceeded)?;
    Ok(writer.bytes)
}
fn read_error(error: impl std::fmt::Display) -> DurableError {
    DurableError::ReadFailure(error.to_string())
}

fn validate_record(
    record: &ControlRecord,
    entry_id: &str,
    entry_seq: u64,
    session_id: &str,
    lane: &str,
    history_generation: u64,
) -> Result<(), DurableError> {
    let invalid =
        || DurableError::InvalidJournal("invalid control format, receipt chain or identity".into());
    if record.format != CONTROL_FORMAT
        || record.version != 1
        || record.receipts.is_empty()
        || record.receipts.len() > MAX_RECEIPTS
    {
        return Err(invalid());
    }
    record
        .state
        .validate()
        .map_err(|e| DurableError::InvalidJournal(e.to_string()))?;
    record
        .execution_control
        .validate()
        .map_err(|e| DurableError::InvalidJournal(e.to_string()))?;
    // This format never evicts receipts. A missing prefix must not silently
    // forget an accepted request and make its ID reusable.
    let first = record.receipts.first().ok_or_else(invalid)?;
    if first.expected.entry_id.is_some() || first.expected.control_revision != 0 {
        return Err(invalid());
    }
    let mut requests = HashSet::new();
    let mut entries = HashSet::new();
    for receipt in &record.receipts {
        validate_identity(&receipt.request_id).map_err(|_| invalid())?;
        validate_identity(&receipt.actor).map_err(|_| invalid())?;
        validate_execution_target(&receipt.action, receipt.run_id.as_deref())
            .map_err(|_| invalid())?;
        if receipt.expected.session_id != session_id
            || receipt.expected.lane != lane
            || receipt.expected.history_generation != history_generation
            || receipt.entry_id.is_empty()
            || !requests.insert(&receipt.request_id)
            || !entries.insert(&receipt.entry_id)
            || receipt.control_revision > record.state.revision
            || receipt
                .expected
                .control_revision
                .checked_add(u64::from(receipt.changed))
                != Some(receipt.control_revision)
            || matches!(receipt.action, ControlAction::Status | ControlAction::Queue)
        {
            return Err(invalid());
        }
    }
    let mut hidden = Vec::new();
    let mut recovery: Option<&DurableReceipt> = None;
    for receipt in &record.receipts {
        match receipt.action {
            ControlAction::Recover => recovery = Some(receipt),
            ControlAction::ForgetRun => {
                let id = receipt.run_id.as_ref().ok_or_else(invalid)?;
                if hidden.contains(id) {
                    return Err(invalid());
                }
                hidden.push(id.clone());
                if recovery.is_some_and(|r| r.run_id.as_ref() == Some(id)) {
                    recovery = None;
                }
            }
            _ => {}
        }
    }
    if hidden != record.execution_control.forgotten_run_ids {
        return Err(invalid());
    }
    match (recovery, record.execution_control.recovery_request.as_ref()) {
        (None, None) => {}
        (Some(receipt), Some(request))
            if receipt.run_id.as_deref() == Some(request.run_id.as_str())
                && receipt.expected.history_revision == request.observed_history_revision => {}
        _ => return Err(invalid()),
    }
    for pair in record.receipts.windows(2) {
        if pair[1].expected.entry_id.as_deref() != Some(pair[0].entry_id.as_str())
            || pair[1].expected.control_revision != pair[0].control_revision
        {
            return Err(invalid());
        }
    }
    let last = record.receipts.last().ok_or_else(invalid)?;
    if last.entry_id != entry_id
        || last.control_revision != record.state.revision
        || last.expected.entry_id != record.previous_entry_id
        || last.expected.history_revision.checked_add(1) != Some(entry_seq)
    {
        return Err(invalid());
    }
    Ok(())
}

impl AgentHarness {
    pub(crate) fn durable_on_lane(mut self, lane: &str) -> Self {
        self.lane = lane.to_owned();
        self
    }

    pub(super) fn durable_check_owner(&self) -> Result<(), DurableError> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(DurableError::Closed);
        }
        if inner.active_run.as_ref().is_some_and(|run| run.recovering) {
            return Err(DurableError::Recovering);
        }
        Ok(())
    }

    pub(super) fn durable_experimental_policy(
        &self,
    ) -> Option<crate::durable::ExperimentalDurablePolicy> {
        self.inner.lock().unwrap().experimental_durable.clone()
    }

    /// Caller owns message_writes; mode is fixed on the harness, not the service.
    pub(super) async fn durable_projection(&self) -> Result<DurableSnapshot, DurableError> {
        self.durable_check_owner()?;
        if let Some(policy) = self.durable_experimental_policy() {
            return super::durable_ledger::load_current(&self.session, &self.lane, &policy)
                .await
                .map(|loaded| loaded.snapshot);
        }
        self.durable_load().await.map(|(snapshot, _)| snapshot)
    }

    /// The caller owns message_writes. Never restore from a process-global cache.
    pub(super) async fn durable_load(
        &self,
    ) -> Result<(DurableSnapshot, ControlRecord), DurableError> {
        self.durable_check_owner()?;
        load_session(&self.session, &self.lane).await
    }
}

/// An explicit empty candidate must never fall back to the current lane leaf.
enum JournalBranch<'a> {
    Current,
    Candidate(Option<&'a str>),
}

async fn load_session(
    source: &Session,
    lane: &str,
) -> Result<(DurableSnapshot, ControlRecord), DurableError> {
    load_branch(source, lane, JournalBranch::Current).await
}

/// The same journal decoder/identity checks serve live control reads and target
/// admission. This reads a pinned candidate, never constructs a harness, performs
/// native recovery, prunes progress, writes receipts, or creates another writer.
async fn load_branch(
    source: &Session,
    lane: &str,
    branch: JournalBranch<'_>,
) -> Result<(DurableSnapshot, ControlRecord), DurableError> {
    let session = source.pin(Arc::clone(source.id_generator()));
    let binding_token = session.binding_token();
    let storage = session.storage();
    let identity = storage.history_identity().await.map_err(read_error)?;
    let revision = identity.revision;
    let history_generation = identity.generation;
    let view = session.view(lane);
    // Also require the bound lane when selecting a detached candidate branch.
    let current_leaf = view.get_leaf_id().await.map_err(read_error)?;
    let leaf_id = match branch {
        JournalBranch::Current => current_leaf,
        JournalBranch::Candidate(target) => {
            if let Some(id) = target {
                if session.get_entry(id).await.map_err(read_error)?.is_none() {
                    return Err(DurableError::ReadFailure("candidate leaf not found".into()));
                }
            }
            target.map(str::to_owned)
        }
    };
    let entries = if leaf_id.is_some() {
        let bounds = BranchBounds {
            start: leaf_id.clone(),
            ..Default::default()
        };
        // Until explicit import exists, legacy data is an error, not empty
        // state. The candidate start never moves a lane pointer for validation.
        for custom_type in [
            "durable-state",
            "rpi-durable-runtime",
            crate::durable::experimental_ledger::CUSTOM_TYPE,
        ] {
            let legacy = view
                .find_entries_on_branch(
                    &EntryQuery {
                        entry_type: Some("custom"),
                        custom_type: Some(custom_type.into()),
                        order: Some(EntryOrder::NewestFirst),
                        limit: Some(1),
                        ..Default::default()
                    },
                    &bounds,
                )
                .await
                .map_err(read_error)?;
            if !legacy.is_empty() {
                if custom_type == crate::durable::experimental_ledger::CUSTOM_TYPE {
                    return Err(DurableError::InvalidJournal(
                        "experimental durable requires explicit epoch mode".into(),
                    ));
                }
                return Err(DurableError::UnsupportedJournalFormat);
            }
        }
        // Read the complete bounded canonical control chain, oldest first.
        // Reading only the newest record would let a malformed or reordered
        // predecessor disappear behind a superficially valid cumulative state.
        let records = view
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("custom"),
                    custom_type: Some(CONTROL_TYPE.into()),
                    order: Some(EntryOrder::OldestFirst),
                    limit: Some(MAX_RECEIPTS + 1),
                    ..Default::default()
                },
                &bounds,
            )
            .await
            .map_err(read_error)?;
        if records.len() > MAX_RECEIPTS {
            return Err(DurableError::InvalidJournal(
                "control journal exceeds bounded receipt chain".into(),
            ));
        }
        records
    } else {
        Vec::new()
    };
    let (entry_id, record) = if entries.is_empty() {
        (
            None,
            ControlRecord {
                format: CONTROL_FORMAT.into(),
                version: 1,
                previous_entry_id: None,
                state: ControlState::default(),
                execution_control: ExecutionControl::default(),
                receipts: Vec::new(),
            },
        )
    } else {
        let mut previous_id = None;
        let mut previous_seq = None;
        let mut previous_receipts: Option<Vec<DurableReceipt>> = None;
        let mut decoded = Vec::with_capacity(entries.len());
        for entry in &entries {
            let Entry::Custom(entry) = entry else {
                return Err(DurableError::InvalidJournal(
                    "unexpected filtered entry".into(),
                ));
            };
            let data = entry
                .data
                .as_ref()
                .ok_or_else(|| DurableError::InvalidJournal("missing control payload".into()))?;
            let bytes = bounded(data, MAX_RECORD_BYTES)?;
            let record: ControlRecord = serde_json::from_slice(&bytes)
                .map_err(|e| DurableError::InvalidJournal(e.to_string()))?;
            validate_record(
                &record,
                &entry.base.id,
                entry.base.seq,
                &storage.metadata().id,
                lane,
                history_generation,
            )?;
            if record.previous_entry_id.as_deref() != previous_id.as_deref() {
                return Err(DurableError::InvalidJournal(
                    "control record predecessor is not the previous canonical entry".into(),
                ));
            }
            if previous_seq.is_some_and(|seq| entry.base.seq <= seq) {
                return Err(DurableError::InvalidJournal(
                    "control records are not in canonical sequence order".into(),
                ));
            }
            if let Some(previous) = &previous_receipts {
                if record.receipts.len() != previous.len() + 1
                    || record.receipts[..previous.len()] != previous[..]
                {
                    return Err(DurableError::InvalidJournal(
                        "control receipt chain is not a contiguous append".into(),
                    ));
                }
            }
            previous_id = Some(entry.base.id.clone());
            previous_seq = Some(entry.base.seq);
            previous_receipts = Some(record.receipts.clone());
            decoded.push((entry.base.id.clone(), record));
        }
        let (entry_id, record) = decoded.pop().expect("non-empty canonical control records");
        (Some(entry_id), record)
    };
    let final_identity = storage.history_identity().await.map_err(read_error)?;
    if final_identity.revision != revision
        || final_identity.generation != history_generation
        || source.binding_token() != binding_token
    {
        return Err(DurableError::Conflict);
    }
    let snapshot = DurableSnapshot {
        scope: DurableScope {
            session_id: storage.metadata().id,
            lane: lane.to_owned(),
            binding_token,
            leaf_id,
            history_revision: revision,
            history_generation,
            entry_id,
            control_revision: record.state.revision,
        },
        state: record.state.clone(),
        execution_control: record.execution_control.clone(),
    };
    let journal = VerifiedJournal::from_validated(snapshot, record)?;
    Ok(journal.into_parts())
}

pub(super) async fn validate_session(
    session: &Session,
    lane: &str,
    policy: Option<&crate::durable::ExperimentalDurablePolicy>,
) -> HarnessResult<()> {
    if let Some(policy) = policy {
        return super::durable_ledger::load_current(session, lane, policy)
            .await
            .map(|_| ())
            .map_err(admission_error);
    }
    load_session(session, lane)
        .await
        .map(|_| ())
        .map_err(admission_error)
}

pub(super) async fn validate_navigation(
    session: &Session,
    lane: &str,
    target: Option<&str>,
    policy: Option<&crate::durable::ExperimentalDurablePolicy>,
) -> HarnessResult<()> {
    if let Some(policy) = policy {
        return super::durable_ledger::load_branch(session, lane, target, policy)
            .await
            .map(|_| ())
            .map_err(admission_error);
    }
    load_branch(session, lane, JournalBranch::Candidate(target))
        .await
        .map(|_| ())
        .map_err(admission_error)
}

fn admission_error(error: DurableError) -> HarnessError {
    // Session policy diagnostics are fixed: do not reflect storage paths or
    // arbitrary journal payloads across the host boundary.
    let message = match error {
        DurableError::UnsupportedJournalFormat => {
            "Unsupported durable journal format; session was not activated"
        }
        DurableError::InvalidJournal(_) => {
            "Invalid native durable journal; session was not activated"
        }
        DurableError::Conflict => {
            "Native durable target changed during validation; session was not activated"
        }
        DurableError::LimitExceeded => {
            "Native durable journal exceeds bounded limits; session was not activated"
        }
        _ => "Native durable session validation failed; session was not activated",
    };
    HarnessError::io(message)
}

fn reconciled_outcome(
    snapshot: DurableSnapshot,
    receipt: Option<DurableReceipt>,
    request: &DurableMutation,
    actor: &DurableActor,
) -> Result<Option<DurableOutcome>, DurableError> {
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    if receipt.actor != actor.id()
        || receipt.action != request.action
        || receipt.expected != request.expected
        || receipt.run_id != request.run_id
        || snapshot.scope.entry_id.as_deref() != Some(receipt.entry_id.as_str())
    {
        return Err(DurableError::Conflict);
    }
    Ok(Some(DurableOutcome {
        receipt,
        snapshot,
        replayed: true,
    }))
}

impl AgentHarness {
    /// Reconcile an exact request under the same gate as mutation and snapshot.
    /// This never appends, retries, or infers a missing outcome.
    pub(crate) async fn durable_reconcile(
        &self,
        request: DurableMutation,
        actor: DurableActor,
    ) -> Result<Option<DurableOutcome>, DurableError> {
        bounded(&request, MAX_REQUEST_BYTES)?;
        let _gate = self.message_writes.lock().await;
        self.durable_check_owner()?;
        if let Some(policy) = self.durable_experimental_policy() {
            policy.check_actor(&actor)?;
            let loaded =
                super::durable_ledger::load_current(&self.session, &self.lane, &policy).await?;
            let receipt = loaded.receipt(&request.request_id)?;
            return reconciled_outcome(loaded.snapshot, receipt, &request, &actor);
        }
        let (snapshot, record) = self.durable_load().await?;
        let receipt = record
            .receipts
            .into_iter()
            .find(|receipt| receipt.request_id == request.request_id);
        reconciled_outcome(snapshot, receipt, &request, &actor)
    }

    /// Seal durable admissions and settle all work admitted before the seal.
    /// Reads remain available, but mutation and session rebinding reject until
    /// a fresh harness is created. The seal is volatile lifecycle state and is
    /// deliberately not persisted as a journal fact.
    pub(crate) async fn durable_seal(
        &self,
        actor: DurableActor,
    ) -> Result<DurableSnapshot, DurableError> {
        let _gate = self.message_writes.lock().await;
        self.durable_check_owner()?;
        if !actor.may_replace_workflow() {
            return Err(DurableError::PermissionDenied);
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.durable_sealed {
                return Err(DurableError::Sealed);
            }
            inner.durable_sealed = true;
        }
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        target.drain().await;
        // The gate is still held: this is the canonical post-settlement
        // projection, not a second unsynchronized read by the caller.
        self.durable_projection().await
    }

    /// A cleanup barrier, not a journal operation. In particular, closed or
    /// corrupt owners must still be able to wait for admitted work to settle.
    pub(crate) async fn durable_drain(&self) {
        let _gate = self.message_writes.lock().await;
        // Pin while ordered against this harness's session replacement. Other
        // facades can rebind independently; never drain their replacement here.
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        target.drain().await;
    }

    pub(crate) async fn durable_snapshot(&self) -> Result<DurableSnapshot, DurableError> {
        let _gate = self.message_writes.lock().await;
        self.durable_projection().await
    }

    pub(crate) async fn durable_validate_snapshot(
        &self,
        expected: &DurableSnapshot,
    ) -> Result<DurableSnapshot, DurableError> {
        let _gate = self.message_writes.lock().await;
        let current = self.durable_projection().await?;
        if &current != expected {
            return Err(DurableError::Conflict);
        }
        Ok(current)
    }

    pub(crate) async fn durable_publish_history_checkpoint(
        &self,
        checkpoint: HistoryCheckpoint,
    ) -> Result<HistoryIdentity, DurableError> {
        let _gate = self.message_writes.lock().await;
        self.durable_check_owner()?;
        checkpoint
            .validate()
            .map_err(|error| DurableError::InvalidJournal(error.to_string()))?;
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let storage = target.storage();
        let identity = storage.history_identity().await.map_err(read_error)?;
        if checkpoint.history_identity != identity {
            return Err(DurableError::Conflict);
        }
        storage
            .replace_history_with_checkpoint_if_revision_and_generation(
                identity.revision,
                identity.generation,
                checkpoint,
            )
            .await
            .map_err(|error| DurableError::CommitUncertain(error.to_string()))
    }

    pub(crate) async fn durable_history_physical_bytes(&self) -> Result<Option<u64>, DurableError> {
        let _gate = self.message_writes.lock().await;
        self.durable_check_owner()?;
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        target
            .storage()
            .history_physical_bytes()
            .await
            .map_err(read_error)
    }

    pub(crate) async fn durable_request_outcome(
        &self,
        request_id: &str,
        actor: &str,
    ) -> Result<Option<DurableReceipt>, DurableError> {
        let _gate = self.message_writes.lock().await;
        self.durable_check_owner()?;
        if let Some(policy) = self.durable_experimental_policy() {
            let loaded =
                super::durable_ledger::load_current(&self.session, &self.lane, &policy).await?;
            return loaded
                .receipt(request_id)
                .map(|receipt| receipt.filter(|receipt| receipt.actor == actor));
        }
        let (_, record) = self.durable_load().await?;
        Ok(record
            .receipts
            .into_iter()
            .find(|receipt| receipt.request_id == request_id && receipt.actor == actor))
    }

    pub(crate) async fn durable_mutate(
        self,
        request: DurableMutation,
        actor: DurableActor,
    ) -> Result<DurableOutcome, DurableError> {
        bounded(&request, MAX_REQUEST_BYTES)?;
        let mut acquiring = Box::pin(self.message_writes.clone().lock_owned());
        // Reserve the existing FIFO gate before the caller first yields. A
        // normal first poll can return Pending *before* enqueueing when Tokio's
        // coop budget is exhausted, so bypass coop for this one lock poll only.
        // No journal/backend work is polled here. The pinned waiter (or ready
        // guard) moves into the same owned completion task without a yield.
        let acquired = futures::poll!(tokio::task::unconstrained(acquiring.as_mut()));
        tokio::spawn(async move {
            let _gate = match acquired {
                std::task::Poll::Ready(guard) => guard,
                std::task::Poll::Pending => acquiring.await,
            };
            self.durable_mutate_locked(request, actor).await
        })
        .await
        .map_err(|error| DurableError::CommitUncertain(error.to_string()))?
    }

    // Only the owned completion task above calls this, holding the original
    // message-write gate through append and post-validation. Never reacquire it.
    async fn durable_mutate_locked(
        self,
        request: DurableMutation,
        actor: DurableActor,
    ) -> Result<DurableOutcome, DurableError> {
        if self.inner.lock().unwrap().durable_sealed {
            return Err(DurableError::Sealed);
        }
        if let Some(policy) = self.durable_experimental_policy() {
            return self
                .durable_ledger_mutate_locked(request, actor, &policy)
                .await;
        }
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let (snapshot, mut record) = self.durable_load().await?;
        if target.binding_token() != snapshot.scope.binding_token {
            return Err(DurableError::Conflict);
        }
        if !actor.may_replace_workflow()
            && matches!(
                request.action,
                ControlAction::Start { .. }
                    | ControlAction::Reset
                    | ControlAction::Recover
                    | ControlAction::ForgetRun
            )
        {
            return Err(DurableError::PermissionDenied);
        }
        if let Some(receipt) = record
            .receipts
            .iter()
            .find(|r| r.request_id == request.request_id)
        {
            if receipt.actor != actor.id()
                || receipt.action != request.action
                || receipt.expected != request.expected
                || receipt.run_id != request.run_id
                || snapshot.scope.entry_id.as_deref() != Some(receipt.entry_id.as_str())
            {
                return Err(DurableError::Conflict);
            }
            return Ok(DurableOutcome {
                receipt: receipt.clone(),
                snapshot,
                replayed: true,
            });
        }
        if snapshot.scope != request.expected {
            return Err(DurableError::Conflict);
        }
        if record.receipts.len() >= MAX_RECEIPTS {
            return Err(DurableError::LimitExceeded);
        }
        if let ControlAction::Finish { item, .. } | ControlAction::Cancel { item, .. } =
            &request.action
        {
            if let Some(existing) = snapshot.state.queue.iter().find(|q| q.id == item.trim()) {
                if existing.status == QueueStatus::Claimed
                    && existing.claimed_by.as_deref() != Some(actor.id())
                    && !(actor.may_replace_workflow()
                        && matches!(request.action, ControlAction::Cancel { .. }))
                {
                    return Err(DurableError::PermissionDenied);
                }
            }
        }
        let mut execution_control = record.execution_control.clone();
        let mut candidate = if let Some(run_id) = request.run_id.as_deref() {
            let run = if execution_control
                .forgotten_run_ids
                .iter()
                .any(|id| id == run_id)
            {
                None
            } else {
                Some(
                    self.durable_read_execution(&target, &snapshot.scope, run_id)
                        .await?,
                )
            };
            execution_control = execution_control.apply(
                &request.action,
                run.as_ref(),
                snapshot.scope.history_revision,
            )?;
            snapshot.state.clone()
        } else {
            snapshot.state.apply(&request.action)?
        };
        if let ControlAction::Claim { item } = &request.action {
            let claimed = candidate
                .queue
                .iter_mut()
                .find(|q| q.id == item.trim())
                .ok_or_else(|| {
                    DurableError::InvalidJournal("claim candidate missing item".into())
                })?;
            claimed.claimed_by = Some(actor.id().to_owned());
        }
        let changed = candidate != snapshot.state || execution_control != record.execution_control;
        if changed {
            candidate.revision = candidate
                .revision
                .checked_add(1)
                .ok_or(rpi_durable::control::ControlError::RevisionOverflow)?;
        }
        candidate.validate()?;
        let entry_id = self.next_entry_id();
        let receipt = DurableReceipt {
            request_id: request.request_id,
            actor: actor.id().to_owned(),
            expected: request.expected,
            action: request.action,
            run_id: request.run_id,
            entry_id: entry_id.clone(),
            control_revision: candidate.revision,
            changed,
        };
        record.previous_entry_id = snapshot.scope.entry_id;
        record.state = candidate;
        record.execution_control = execution_control;
        record.receipts.push(receipt.clone());
        let entry_seq = snapshot
            .scope
            .history_revision
            .checked_add(1)
            .ok_or(DurableError::LimitExceeded)?;
        validate_record(
            &record,
            &entry_id,
            entry_seq,
            &snapshot.scope.session_id,
            &self.lane,
            snapshot.scope.history_generation,
        )?;
        let bytes = bounded(&record, MAX_RECORD_BYTES)?;
        let data = serde_json::from_slice(&bytes)
            .map_err(|e| DurableError::InvalidJournal(e.to_string()))?;
        // A receipt is committed even for a no-op mutation. Otherwise retrying
        // an old no-op reset after a later start could erase new work.
        target
            .append_entry_if_revision(
                ProvisionedEntry {
                    id: entry_id,
                    kind: ProvisionedKind::Custom {
                        custom_type: CONTROL_TYPE.into(),
                        data: Some(data),
                    },
                },
                &self.lane,
                snapshot.scope.history_revision,
            )
            .await
            .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
        let (current, committed) = self
            .durable_load()
            .await
            .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
        // Another owner sharing the Session facade can rebind or navigate
        // while our pinned append awaits its ACK. Never combine A's receipt
        // with B's projection, or report success from a branch hiding the write.
        if current.scope.binding_token != target.binding_token()
            || !committed.receipts.contains(&receipt)
        {
            return Err(DurableError::CommitUncertain(
                "committed target was replaced or receipt is absent from the current branch".into(),
            ));
        }
        Ok(DurableOutcome {
            receipt,
            snapshot: current,
            replayed: false,
        })
    }
}
