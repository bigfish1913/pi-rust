//! Source-bound composition of experimental source, canonical ledger, historical
//! native facts and full transition replay. Read-only observations, not admission.
use super::{
    inspect_execution_transitions, same_observation, ExecutionTransitionError,
    ExecutionTransitionInspection,
};
use crate::{
    agent_harness::AgentHarness,
    durable::{
        experimental_ledger::{
            chain::inspect_chain,
            control::{TransitionError, TransitionPolicy},
            native_run::{inspect_native_run_session, NativeRunError},
            source::{read_branch, SourceError, SourceScope},
        },
        validate_identity,
    },
    session::Session,
};
use rpi_durable::control::ControlAction;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ExecutionBranchInspection {
    scope: SourceScope,
    transitions: ExecutionTransitionInspection,
}
impl ExecutionBranchInspection {
    pub fn scope(&self) -> &SourceScope {
        &self.scope
    }
    pub fn transitions(&self) -> &ExecutionTransitionInspection {
        &self.transitions
    }
}
#[derive(Debug, thiserror::Error)]
pub enum ExecutionBranchError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Native(#[from] NativeRunError),
    #[error(transparent)]
    Transition(#[from] ExecutionTransitionError),
}

/// Inspect one explicit branch; None means empty, never current/latest. Reads
/// its own complete source and mints every execution fact itself. Neither public
/// source envelopes nor caller capsules, scopes or checkpoints are proof inputs.
/// All minted actual-current observations must match this invocation's captured
/// original scope, not merely agree with each other. Historical selected leaves
/// and revisions are still those of their respective receipts.
///
/// After initial capture, both final real I/O run even on collected errors:
/// final read faults > outer observed change > collected source/native/replay
/// error. Initial capture failures have no baseline to fence. Cancellation drops
/// only this reader, spawns no worker and need not reach final observations.
/// Repeated bounded source scans and canonical checks are intentional; budgets
/// are logical, not physical RSS/CPU/I/O, and pure work is not cooperatively
/// preemptible. Backend honesty and caller-attested policy remain assumptions.
/// No VerifiedJournal, snapshot/admission/restore authority, policy-origin
/// authentication, atomic generation/ABA fence, writer or provider is added.
pub async fn inspect_execution_branch(
    harness: &AgentHarness,
    lane: &str,
    selected_leaf_id: Option<&str>,
    policy: &TransitionPolicy,
) -> Result<ExecutionBranchInspection, ExecutionBranchError> {
    inspect_execution_session(harness.session(), lane, selected_leaf_id, policy).await
}

// Crate-private proof reuse; never constructs a harness to validate admission.
pub(crate) async fn inspect_execution_session(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
    policy: &TransitionPolicy,
) -> Result<ExecutionBranchInspection, ExecutionBranchError> {
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
        lane: lane.into(),
        binding_token: binding.clone(),
        selected_leaf_id: selected_leaf_id.map(str::to_owned),
        current_leaf_id: leaf.clone(),
        history_revision: revision,
        history_generation: history_identity.generation,
    };
    let result: Result<ExecutionTransitionInspection, ExecutionBranchError> = async {
        let fresh = read_branch(&pinned, lane, selected_leaf_id, &policy.read).await?;
        if fresh.scope != scope {
            return Err(SourceError::Conflict.into());
        }
        let records: Vec<_> = fresh
            .records
            .into_iter()
            .map(|item| item.candidate)
            .collect();
        // Reject noncanonical source before scanning any native run. The pure
        // replay deliberately repeats this check rather than accepting a new
        // caller-supplied or cached "prevalidated ledger" authority.
        inspect_chain(&records, &policy.read)
            .map_err(TransitionError::from)
            .map_err(ExecutionTransitionError::from)?;
        let mut facts = Vec::new();
        for candidate in &records {
            let receipt = &candidate.record().receipt;
            if matches!(
                receipt.action,
                ControlAction::Recover | ControlAction::ForgetRun
            ) {
                let run_id = receipt
                    .run_id
                    .as_deref()
                    .ok_or(ExecutionTransitionError::Facts)?;
                let fact = inspect_native_run_session(
                    source,
                    lane,
                    receipt.expected.leaf_id.as_deref(),
                    receipt.expected.history_revision,
                    run_id,
                    &policy.read,
                )
                .await?;
                if !same_observation(&scope, fact.scope()) {
                    return Err(SourceError::Conflict.into());
                }
                facts.push(fact);
            }
        }
        Ok(inspect_execution_transitions(&records, policy, &facts)?)
    }
    .await;
    // No early ? until both final I/O and the original facade observation finish.
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
    let transitions = result?;
    Ok(ExecutionBranchInspection { scope, transitions })
}
