//! Pure conditional replay including execution intent. Not source-bound journal
//! authority: native capsules prove their own explicit branch/watermark only.
pub mod source;

use super::{prepare_control_candidate, TransitionError, TransitionPolicy};
use crate::durable::experimental_ledger::{
    chain::{inspect_chain, LedgerChainInspection, MAX_CHAIN_RECORDS},
    native_run::NativeRunInspection,
    source::SourceScope,
    DecodedCandidate,
};
use rpi_durable::{
    control::{ControlAction, ControlState},
    execution::{ExecutionControl, ExecutionError},
};

#[derive(Clone, Debug)]
pub struct ExecutionTransitionInspection {
    ledger: LedgerChainInspection,
    state: ControlState,
    execution_control: ExecutionControl,
}
impl ExecutionTransitionInspection {
    pub fn ledger(&self) -> &LedgerChainInspection {
        &self.ledger
    }
    pub fn state(&self) -> &ControlState {
        &self.state
    }
    pub fn execution_control(&self) -> &ExecutionControl {
        &self.execution_control
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionTransitionError {
    #[error(transparent)]
    Control(#[from] TransitionError),
    #[error("missing, extra, misbound or inconsistent historical native run facts")]
    Facts,
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}
fn same_observation(a: &SourceScope, b: &SourceScope) -> bool {
    a.session_id == b.session_id
        && a.lane == b.lane
        && a.binding_token == b.binding_token
        && a.history_revision == b.history_revision
        && a.history_generation == b.history_generation
        && a.current_leaf_id == b.current_leaf_id
}

/// Validate the entire supplied canonical map and replay both reducers from
/// default state. Execution receipts consume exactly one native capsule each,
/// in receipt order; capsules can only be minted by the explicit native reader,
/// never from a caller RunCheckpoint. All capsules must share one observed
/// current binding/revision/leaf, while each historical leaf/watermark must match
/// its receipt. This is relative to supplied records, not native source absence,
/// caller/actor authentication, an atomic fence, or a VerifiedJournal/snapshot.
/// No I/O, writer, provider, auto-recovery, or live admission occurs here.
/// Logical budgets and synchronous non-cooperative replay limits are inherited.
pub fn inspect_execution_transitions(
    records: &[DecodedCandidate],
    policy: &TransitionPolicy,
    facts: &[NativeRunInspection],
) -> Result<ExecutionTransitionInspection, ExecutionTransitionError> {
    if facts.len() > MAX_CHAIN_RECORDS || facts.len() > records.len() {
        return Err(ExecutionTransitionError::Facts);
    }
    let ledger = inspect_chain(records, &policy.read).map_err(TransitionError::from)?;
    let mut state = ControlState::default();
    let mut execution_control = ExecutionControl::default();
    let mut facts = facts.iter();
    let mut observation: Option<&SourceScope> = None;
    for candidate in records {
        let record = candidate.record();
        let receipt = &record.receipt;
        let action = &receipt.action;
        let actor = policy
            .actors
            .iter()
            .find(|actor| actor.id() == receipt.actor)
            .ok_or(TransitionError::Permission)?;
        let (mut next, next_execution) = match action {
            ControlAction::Recover | ControlAction::ForgetRun => {
                if !actor.may_replace_workflow() {
                    return Err(TransitionError::Permission.into());
                }
                let fact = facts.next().ok_or(ExecutionTransitionError::Facts)?;
                let scope = fact.scope();
                if receipt.run_id.as_deref() != Some(fact.run().id.as_str())
                    || receipt.expected.session_id != scope.session_id
                    || receipt.expected.lane != scope.lane
                    || receipt.expected.leaf_id != scope.selected_leaf_id
                    || receipt.expected.history_revision != fact.as_of_revision()
                    || receipt.expected.history_generation != scope.history_generation
                    || observation.is_some_and(|first| !same_observation(first, scope))
                {
                    return Err(ExecutionTransitionError::Facts);
                }
                observation = Some(scope);
                let run = if execution_control.forgotten_run_ids.contains(&fact.run().id) {
                    None
                } else {
                    Some(fact.run())
                };
                (
                    state.clone(),
                    execution_control.apply(action, run, fact.as_of_revision())?,
                )
            }
            _ => {
                if receipt.run_id.is_some() {
                    return Err(TransitionError::Transition.into());
                }
                (
                    prepare_control_candidate(&state, action, actor)?,
                    execution_control.clone(),
                )
            }
        };
        let changed = next != state || next_execution != execution_control;
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
            || record.execution_control != next_execution
        {
            return Err(TransitionError::Transition.into());
        }
        state = next;
        execution_control = next_execution;
    }
    if facts.next().is_some() {
        return Err(ExecutionTransitionError::Facts);
    }
    Ok(ExecutionTransitionInspection {
        ledger,
        state,
        execution_control,
    })
}
