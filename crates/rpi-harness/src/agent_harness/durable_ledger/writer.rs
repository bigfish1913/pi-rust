//! One conditional append on the caller's original gate/owned completion task.
use super::{budget, codec_error, invalid, ledger, load_current, AgentHarness};
use crate::{
    durable::{
        validate_execution_target, validate_identity, DurableActor, DurableError, DurableMutation,
        DurableOutcome, DurableReceipt, ExperimentalDurablePolicy, MAX_REQUEST_BYTES,
    },
    session::types::{ProvisionedEntry, ProvisionedKind},
};
use rpi_durable::{
    control::{ControlAction, ControlError, QueueStatus},
    ledger::insert::{Insert, InsertStep},
};
use std::sync::Arc;

fn uncertain() -> DurableError {
    DurableError::CommitUncertain("experimental append or post-commit validation failed".into())
}

impl AgentHarness {
    /// Caller owns the original message-write gate and the original core-owned
    /// task. Never reacquire the gate, spawn, run a provider, or retry an append.
    pub(in crate::agent_harness) async fn durable_ledger_mutate_locked(
        &self,
        request: DurableMutation,
        actor: DurableActor,
        config: &ExperimentalDurablePolicy,
    ) -> Result<DurableOutcome, DurableError> {
        self.durable_check_owner()?;
        // Role mismatch and owner-only action denials precede even replay.
        config.check_actor(&actor)?;
        if !actor.may_replace_workflow
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
        budget::bounded(&request, MAX_REQUEST_BYTES)?;
        validate_identity(&request.request_id)?;
        validate_execution_target(&request.action, request.run_id.as_deref())?;
        if matches!(request.action, ControlAction::Status | ControlAction::Queue) {
            return Err(DurableError::InvalidRequest(
                "use snapshot for read-only actions".into(),
            ));
        }
        let target = self.session.pin(Arc::clone(self.session.id_generator()));
        let loaded = load_current(&self.session, &self.lane, config).await?;
        let snapshot = &loaded.snapshot;
        if target.binding_token() != snapshot.scope.binding_token {
            return Err(DurableError::Conflict);
        }
        if let Some(receipt) = loaded.receipt(&request.request_id)? {
            // Historical outcome queries remain valid, but only the CURRENT
            // anchor permits an identical retry to report replay success.
            if receipt.actor != actor.id
                || receipt.action != request.action
                || receipt.expected != request.expected
                || receipt.run_id != request.run_id
                || snapshot.scope.entry_id.as_deref() != Some(receipt.entry_id.as_str())
            {
                return Err(DurableError::Conflict);
            }
            return Ok(DurableOutcome {
                receipt,
                snapshot: loaded.snapshot,
                replayed: true,
            });
        }
        if snapshot.scope != request.expected {
            return Err(DurableError::Conflict);
        }
        budget::append_counts(&loaded)?;
        // Prospective roster is bounded independently of configured-owner count.
        // This never promotes a historical actor based on actions or claims.
        let read = config.read_policy(&snapshot.scope.session_id, &self.lane)?;
        config.transition_policy(
            read.clone(),
            loaded
                .source
                .records
                .iter()
                .map(|item| item.candidate.record().receipt.actor.as_str())
                .chain(std::iter::once(actor.id.as_str())),
        )?;
        if let ControlAction::Finish { item, .. } | ControlAction::Cancel { item, .. } =
            &request.action
        {
            if let Some(existing) = snapshot
                .state
                .queue
                .iter()
                .find(|item_| item_.id == item.trim())
            {
                if existing.status == QueueStatus::Claimed
                    && existing.claimed_by.as_deref() != Some(actor.id.as_str())
                    && !(actor.may_replace_workflow
                        && matches!(request.action, ControlAction::Cancel { .. }))
                {
                    return Err(DurableError::PermissionDenied);
                }
            }
        }
        let mut execution_control = snapshot.execution_control.clone();
        let mut candidate = if let Some(run_id) = request.run_id.as_deref() {
            let run = if execution_control
                .forgotten_run_ids
                .iter()
                .any(|id| id == run_id)
            {
                None
            } else {
                // Use the same bounded historical factory that will verify
                // this receipt after commit. A live proof alone could accept a
                // lane whose all-record history exceeds that reader's budget,
                // append a receipt, then make the journal unreadable at ACK.
                let fact = ledger::native_run::inspect_native_run_session(
                    &self.session,
                    &self.lane,
                    snapshot.scope.leaf_id.as_deref(),
                    snapshot.scope.history_revision,
                    run_id,
                    &read,
                )
                .await
                .map_err(|error| {
                    super::inspection_error(
                        ledger::control::execution::source::ExecutionBranchError::Native(error),
                    )
                })?;
                if fact.scope() != &loaded.source.scope {
                    return Err(DurableError::Conflict);
                }
                Some(fact)
            };
            // Preserve structured domain errors from the real reducers.
            execution_control = execution_control.apply(
                &request.action,
                run.as_ref().map(|fact| fact.run()),
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
                .find(|entry| entry.id == item.trim())
                .ok_or_else(invalid)?;
            claimed.claimed_by = Some(actor.id.clone());
        }
        let changed =
            candidate != snapshot.state || execution_control != snapshot.execution_control;
        if changed {
            candidate.revision = candidate
                .revision
                .checked_add(1)
                .ok_or(ControlError::RevisionOverflow)?;
        }
        candidate.validate()?;
        let entry_id = self.next_entry_id();
        let ordinal = loaded
            .source
            .records
            .last()
            .map_or(0, |item| item.candidate.record().ordinal)
            .checked_add(1)
            .ok_or(DurableError::LimitExceeded)?;
        let receipt = DurableReceipt {
            request_id: request.request_id,
            actor: actor.id,
            expected: request.expected,
            action: request.action,
            run_id: request.run_id,
            entry_id: entry_id.clone(),
            control_revision: candidate.revision,
            changed,
        };
        // This validates and bounds the receipt (64 KiB) before its output
        // allocation and binds its exact digest and ordinal to the insertion.
        let reference = ledger::receipt_reference(&receipt, ordinal).map_err(codec_error)?;
        let hasher = ledger::Sha256NodeHasher;
        let mut insert = Insert::new(&receipt.request_id, reference, loaded.root.clone(), &hasher)
            .map_err(|_| invalid())?;
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    let node = loaded.node(reference)?;
                    insert.provide(node, &hasher).map_err(|_| invalid())?;
                }
                InsertStep::Ready(_) => break,
                InsertStep::Poisoned | InsertStep::Taken => return Err(invalid()),
            }
        }
        let batch = insert.take_batch().map_err(|_| invalid())?;
        let record = ledger::Record {
            format: ledger::FORMAT.into(),
            version: 2,
            epoch: ledger::Epoch {
                session_id: snapshot.scope.session_id.clone(),
                lane: self.lane.clone(),
                epoch_id: config.epoch_id().into(),
            },
            ordinal,
            previous: loaded.source.records.last().map(|item| ledger::Previous {
                entry_id: item.entry.base.id.clone(),
                ordinal: item.candidate.record().ordinal,
            }),
            state: candidate,
            execution_control,
            receipt: receipt.clone(),
            nodes: batch.nodes,
            root: batch.root,
        };
        let bytes = budget::bounded(&record, ledger::MAX_RECORD_BYTES)?;
        // Fail closed before publishing anything; no permissive Value-only
        // decode, and no bypass around the strict local codec's proof checks.
        ledger::decode(&bytes, &entry_id, ledger::CUSTOM_TYPE, &read).map_err(codec_error)?;
        let data = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let data = budget::prospective(&loaded, &record, data)?;
        // No-op mutations also persist a receipt. The captured R conditions the
        // sole writer call; there is no separate authority or optimistic cache.
        target
            .append_entry_if_revision(
                ProvisionedEntry {
                    id: entry_id,
                    kind: ProvisionedKind::Custom {
                        custom_type: ledger::CUSTOM_TYPE.into(),
                        data: Some(data),
                    },
                },
                &self.lane,
                snapshot.scope.history_revision,
            )
            .await
            .map_err(|_| uncertain())?;
        let current = load_current(&self.session, &self.lane, config)
            .await
            .map_err(|_| uncertain())?;
        if current.snapshot.scope.binding_token != target.binding_token()
            || current
                .receipt(&receipt.request_id)
                .map_err(|_| uncertain())?
                != Some(receipt.clone())
        {
            return Err(uncertain());
        }
        Ok(DurableOutcome {
            receipt,
            snapshot: current.snapshot,
            replayed: false,
        })
    }
}
