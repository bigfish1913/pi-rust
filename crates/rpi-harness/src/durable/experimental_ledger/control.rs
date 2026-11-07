//! Pure control-only transition inspection, NOT a source-bound verified journal.
//!
//! Reuses the existing control reducer and mirrors the current host's claim and
//! role checks, under an explicitly caller-attested fixed actor roster. It does
//! not discover/authenticate host identities or bind native source envelopes.
//! All execution actions or nonempty execution control require future native run
//! proof support and fail closed here. No I/O, writer, provider or admission.
use super::{
    chain::{inspect_chain, ChainError, LedgerChainInspection},
    DecodedCandidate, ReadPolicy,
};
use crate::durable::DurableActor;
use rpi_durable::{
    control::{ControlAction, ControlState, QueueStatus},
    execution::ExecutionControl,
};
use std::collections::HashSet;

pub mod execution;
pub mod source;

pub const MAX_POLICY_ACTORS: usize = 256;

/// Fixed caller-attested roles, never inferred from receipt actors or claims.
/// Construction does not authenticate the caller's actual host/owner authority.
#[derive(Clone, Debug)]
pub struct TransitionPolicy {
    read: ReadPolicy,
    actors: Vec<DurableActor>,
}
impl TransitionPolicy {
    pub fn new(read: ReadPolicy, actors: Vec<DurableActor>) -> Result<Self, TransitionError> {
        if actors.is_empty() || actors.len() > MAX_POLICY_ACTORS {
            return Err(TransitionError::Policy);
        }
        let mut ids = HashSet::new();
        for actor in &actors {
            if !ids.insert(actor.id.as_str()) {
                return Err(TransitionError::Policy);
            }
        }
        Ok(Self { read, actors })
    }
}

/// Relative replay result under the provided policy, not an authority/snapshot.
/// Private construction, not deserializable, no conversion to live durable state.
#[derive(Clone, Debug)]
pub struct ControlTransitionInspection {
    ledger: LedgerChainInspection,
    state: ControlState,
}
impl ControlTransitionInspection {
    pub fn ledger(&self) -> &LedgerChainInspection {
        &self.ledger
    }
    pub fn state(&self) -> &ControlState {
        &self.state
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("invalid experimental control transition policy")]
    Policy,
    #[error("experimental control actor is not permitted")]
    Permission,
    #[error("experimental execution transitions require native run facts")]
    ExecutionFactsRequired,
    #[error("invalid experimental control transition")]
    Transition,
    #[error(transparent)]
    Chain(#[from] ChainError),
}

/// Validate the canonical map, then replay the complete control-only history
/// from default state. No partial prefix is returned. Empty input proves only
/// default state relative to the supplied input, never native history absence.
/// This synchronous bounded replay has no cooperative cancellation points or
/// physical memory/CPU guarantees beyond its underlying logical input budgets.
pub fn inspect_control_transitions(
    records: &[DecodedCandidate],
    policy: &TransitionPolicy,
) -> Result<ControlTransitionInspection, TransitionError> {
    let ledger = inspect_chain(records, &policy.read)?;
    let mut state = ControlState::default();
    for candidate in records {
        let record = candidate.record();
        let receipt = &record.receipt;
        let action = &receipt.action;
        if receipt.run_id.is_some()
            || matches!(action, ControlAction::Recover | ControlAction::ForgetRun)
            || record.execution_control != ExecutionControl::default()
        {
            return Err(TransitionError::ExecutionFactsRequired);
        }
        let actor = policy
            .actors
            .iter()
            .find(|actor| actor.id == receipt.actor)
            .ok_or(TransitionError::Permission)?;
        let mut next = prepare_control_candidate(&state, action, actor)?;
        let changed = next != state;
        if changed {
            next.revision = next
                .revision
                .checked_add(1)
                .ok_or(TransitionError::Transition)?;
        }
        next.validate().map_err(|_| TransitionError::Transition)?;
        if receipt.expected.control_revision != state.revision
            || receipt.changed != changed
            || receipt.control_revision != next.revision
            || record.state != next
        {
            return Err(TransitionError::Transition);
        }
        state = next;
    }
    Ok(ControlTransitionInspection { ledger, state })
}

// Shared with the explicit execution inspector, without publication revision.
// The control-only public entry point above still rejects every execution input.
fn prepare_control_candidate(
    state: &ControlState,
    action: &ControlAction,
    actor: &DurableActor,
) -> Result<ControlState, TransitionError> {
    if !actor.may_replace_workflow
        && matches!(action, ControlAction::Start { .. } | ControlAction::Reset)
    {
        return Err(TransitionError::Permission);
    }
    if let ControlAction::Finish { item, .. } | ControlAction::Cancel { item, .. } = action {
        if let Some(existing) = state.queue.iter().find(|entry| entry.id == item.trim()) {
            if existing.status == QueueStatus::Claimed
                && existing.claimed_by.as_deref() != Some(actor.id.as_str())
                && !(actor.may_replace_workflow && matches!(action, ControlAction::Cancel { .. }))
            {
                return Err(TransitionError::Permission);
            }
        }
    }
    let mut next = state
        .apply(action)
        .map_err(|_| TransitionError::Transition)?;
    if let ControlAction::Claim { item } = action {
        let claimed = next
            .queue
            .iter_mut()
            .find(|entry| entry.id == item.trim())
            .ok_or(TransitionError::Transition)?;
        claimed.claimed_by = Some(actor.id.clone());
    }
    Ok(next)
}
