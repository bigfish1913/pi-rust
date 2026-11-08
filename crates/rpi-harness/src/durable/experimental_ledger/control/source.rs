//! Explicit native-source adapter for the parent's pure control-only inspector.
//!
//! This separate adapter performs read-only Session I/O; the parent's transition
//! replay stays pure. It reads its own full branch source, never caller-forged
//! SourceCandidate fields, and makes additional final observations after replay.
//! A result is still NOT a VerifiedJournal/snapshot/admission/restore permission:
//! backend honesty, policy-origin authentication, native execution proofs and
//! atomic generation/ABA fencing remain outside this bounded inspection.
use super::{
    inspect_control_transitions, ControlTransitionInspection, TransitionError, TransitionPolicy,
};
use crate::{
    durable::{
        experimental_ledger::source::{read_branch, SourceError, SourceScope},
        validate_identity,
    },
    session::Session,
};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ControlBranchInspection {
    scope: SourceScope,
    control: ControlTransitionInspection,
}
impl ControlBranchInspection {
    pub fn scope(&self) -> &SourceScope {
        &self.scope
    }
    pub fn control(&self) -> &ControlTransitionInspection {
        &self.control
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BranchInspectionError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
}

/// Read and inspect an explicit branch (None means empty, never current/latest).
/// Once the initial scope is captured, errors are fenced too. Final read faults
/// take precedence over observed changes, then changes over collected errors.
/// Initial identity/policy/capture failures have no captured scope to fence.
/// Dropping the root future drops only this reader; no background worker exists,
/// and a cancelled future is not guaranteed to reach the final observations.
pub async fn inspect_control_branch(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
    policy: &TransitionPolicy,
) -> Result<ControlBranchInspection, BranchInspectionError> {
    validate_identity(lane).map_err(|_| SourceError::Identity)?;
    if let Some(leaf) = selected_leaf_id {
        validate_identity(leaf).map_err(|_| SourceError::Identity)?;
    }
    let pinned = source.pin(Arc::clone(source.id_generator()));
    let binding = pinned.binding_token();
    let storage = pinned.storage();
    let session_id = storage.metadata().id;
    validate_identity(&session_id).map_err(|_| SourceError::Identity)?;
    if session_id != policy.read.epoch.session_id || lane != policy.read.epoch.lane {
        return Err(SourceError::Policy.into());
    }
    let history_identity = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read)?;
    let revision = history_identity.revision;
    let view = pinned.view(lane);
    let leaf = view.get_leaf_id().await.map_err(|_| SourceError::Read)?;
    let scope = SourceScope {
        session_id,
        lane: lane.to_owned(),
        binding_token: binding.clone(),
        selected_leaf_id: selected_leaf_id.map(str::to_owned),
        current_leaf_id: leaf.clone(),
        history_revision: revision,
        history_generation: history_identity.generation,
    };
    let result = async {
        let fresh = read_branch(&pinned, lane, selected_leaf_id, &policy.read).await?;
        if fresh.scope != scope {
            return Err(BranchInspectionError::Source(SourceError::Conflict));
        }
        // Move private codec candidates, not cloned bodies or public envelopes.
        // Native source validation above was performed by this invocation.
        let records: Vec<_> = fresh
            .records
            .into_iter()
            .map(|item| item.candidate)
            .collect();
        let control = inspect_control_transitions(&records, policy)?;
        Ok(control)
    }
    .await;
    // Observe every final I/O before propagating any fault or collected error.
    let leaf_after = view.get_leaf_id().await.map_err(|_| SourceError::Read);
    let history_identity_after = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read);
    let binding_after = source.binding_token();
    let leaf_after = leaf_after?;
    let history_identity_after = history_identity_after?;
    if leaf_after != leaf || history_identity_after != history_identity || binding_after != binding
    {
        return Err(SourceError::Conflict.into());
    }
    let control = result?;
    Ok(ControlBranchInspection { scope, control })
}
