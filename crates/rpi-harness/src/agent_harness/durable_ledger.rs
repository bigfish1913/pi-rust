//! Private experimental v2 projection and writer, on the existing harness gate.
//! No public proof tokens, alternate writer, auto epoch adoption, or cache.
//! Repeated bounded scans are intentional; backend honesty is still assumed.
use super::AgentHarness;
use crate::{
    durable::{
        experimental_ledger::{
            self as ledger,
            archive::ReceiptArchive,
            chain::ChainError,
            control::{
                execution::{
                    source::{inspect_execution_session, ExecutionBranchError},
                    ExecutionTransitionError,
                },
                TransitionError,
            },
            native_run::NativeRunError,
            source::{self, SourceCandidate, SourceError, SourceScope},
            CodecError, Sha256NodeHasher,
        },
        validate_identity, DurableError, DurableReceipt, DurableScope, DurableSnapshot,
        ExperimentalDurablePolicy,
    },
    session::Session,
};
use rpi_durable::ledger::{Lookup, LookupStep, Node, NodeRef};
use std::sync::Arc;

mod budget;
mod writer;

fn invalid() -> DurableError {
    DurableError::InvalidJournal("invalid experimental source, transition or ledger proof".into())
}
fn read_failure() -> DurableError {
    DurableError::ReadFailure("experimental source read failed".into())
}
fn codec_error(error: CodecError) -> DurableError {
    match error {
        CodecError::Limit => DurableError::LimitExceeded,
        _ => invalid(),
    }
}
fn source_error(error: SourceError) -> DurableError {
    match error {
        SourceError::Read => read_failure(),
        SourceError::Conflict => DurableError::Conflict,
        SourceError::Limit => DurableError::LimitExceeded,
        SourceError::Codec(error) => codec_error(error),
        _ => invalid(),
    }
}
fn transition_error(error: ExecutionTransitionError) -> DurableError {
    match error {
        ExecutionTransitionError::Control(TransitionError::Chain(ChainError::Limit)) => {
            DurableError::LimitExceeded
        }
        ExecutionTransitionError::Control(TransitionError::Permission) => {
            DurableError::PermissionDenied
        }
        ExecutionTransitionError::Execution(error) => DurableError::Execution(error),
        _ => invalid(),
    }
}
fn native_error(error: DurableError) -> DurableError {
    match error {
        DurableError::ReadFailure(_) => read_failure(),
        DurableError::Conflict => DurableError::Conflict,
        DurableError::LimitExceeded => DurableError::LimitExceeded,
        DurableError::Control(error) => DurableError::Control(error),
        DurableError::Execution(error) => DurableError::Execution(error),
        DurableError::ExecutionUnavailable(_) => {
            DurableError::ExecutionUnavailable("experimental native run proof unavailable".into())
        }
        _ => invalid(),
    }
}
fn inspection_error(error: ExecutionBranchError) -> DurableError {
    match error {
        ExecutionBranchError::Source(error) => source_error(error),
        ExecutionBranchError::Transition(error) => transition_error(error),
        ExecutionBranchError::Native(NativeRunError::Source(error)) => source_error(error),
        ExecutionBranchError::Native(NativeRunError::Native(error)) => native_error(error),
        _ => invalid(),
    }
}

/// Only this module constructs this source-bound projection. It is not a public
/// conversion on an L2 inspection, nor a caller-supplied admission capability.
pub(super) struct Loaded {
    pub(super) snapshot: DurableSnapshot,
    source: SourceCandidate,
    /// Historical outcomes retained by an explicit checkpoint archive. This
    /// is lookup data only; writer admission still requires the current
    /// anchor and complete current scope identity.
    archive: Option<ReceiptArchive>,
    // Obtained only from the complete genuine source/native/replay inspector.
    root: Option<NodeRef>,
}

impl Loaded {
    /// Resolve the exact requested node from locally decoded actual source, not
    /// a derived index. Missing bodies never mean a missing request ID.
    fn node(&self, reference: &NodeRef) -> Result<&Node, DurableError> {
        let record = self
            .source
            .records
            .iter()
            .find(|item| item.entry.base.id == reference.address.entry_id)
            .ok_or_else(invalid)?
            .candidate
            .record();
        if record.ordinal != reference.address.ordinal {
            return Err(invalid());
        }
        let node = record
            .nodes
            .get(usize::from(reference.address.slot))
            .ok_or_else(invalid)?;
        if node.address != reference.address
            || node.reference(&Sha256NodeHasher).map_err(|_| invalid())? != *reference
        {
            return Err(invalid());
        }
        Ok(node)
    }

    pub(super) fn receipt(&self, id: &str) -> Result<Option<DurableReceipt>, DurableError> {
        validate_identity(id)?;
        let mut lookup = Lookup::new(id, self.root.clone()).map_err(|_| invalid())?;
        loop {
            match lookup.step() {
                LookupStep::NeedNode(reference) => {
                    let node = self.node(reference)?;
                    lookup
                        .provide(node, &Sha256NodeHasher)
                        .map_err(|_| invalid())?;
                }
                LookupStep::Found(reference) => {
                    let record = self
                        .source
                        .records
                        .iter()
                        .find(|item| item.entry.base.id == reference.entry_id)
                        .ok_or_else(invalid)?
                        .candidate
                        .record();
                    if record.ordinal != reference.ordinal
                        || record.receipt.request_id != id
                        || ledger::receipt_reference(&record.receipt, record.ordinal)
                            .map_err(codec_error)?
                            != *reference
                    {
                        return Err(invalid());
                    }
                    return Ok(Some(record.receipt.clone()));
                }
                LookupStep::Absent => {
                    if let Some(archive) = &self.archive {
                        let archived = archive.lookup(id).map_err(|_| invalid())?;
                        return Ok(archived.map(|entry| entry.receipt.clone()));
                    }
                    return Ok(None);
                }
                LookupStep::Poisoned => return Err(invalid()),
            }
        }
    }
}

pub(super) async fn load_current(
    source: &Session,
    lane: &str,
    config: &ExperimentalDurablePolicy,
) -> Result<Loaded, DurableError> {
    load(source, lane, Selection::Current, config).await
}

/// None is an explicitly empty branch, never an alias for the current leaf.
pub(super) async fn load_branch(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
    config: &ExperimentalDurablePolicy,
) -> Result<Loaded, DurableError> {
    load(source, lane, Selection::Explicit(selected_leaf_id), config).await
}

enum Selection<'a> {
    Current,
    Explicit(Option<&'a str>),
}

async fn load(
    original: &Session,
    lane: &str,
    selection: Selection<'_>,
    config: &ExperimentalDurablePolicy,
) -> Result<Loaded, DurableError> {
    validate_identity(lane)?;
    if let Selection::Explicit(Some(id)) = &selection {
        validate_identity(id)?;
    }
    let pinned = original.pin(Arc::clone(original.id_generator()));
    let binding = pinned.binding_token();
    let storage = pinned.storage();
    let session_id = storage.metadata().id;
    let identity = storage
        .history_identity()
        .await
        .map_err(|_| read_failure())?;
    let revision = identity.revision;
    let history_generation = identity.generation;
    let archive = match storage
        .history_checkpoint()
        .await
        .map_err(|_| read_failure())?
    {
        Some(checkpoint) if checkpoint.history_identity != identity => {
            return Err(DurableError::Conflict);
        }
        Some(checkpoint) => checkpoint.receipt_archive,
        None => None,
    };
    let view = pinned.view(lane);
    let current_leaf = view.get_leaf_id().await.map_err(|_| read_failure())?;
    let selected_leaf = match selection {
        Selection::Current => current_leaf.clone(),
        Selection::Explicit(id) => id.map(str::to_owned),
    };
    let capture = SourceScope {
        session_id: session_id.clone(),
        lane: lane.into(),
        binding_token: binding.clone(),
        selected_leaf_id: selected_leaf.clone(),
        current_leaf_id: current_leaf.clone(),
        history_revision: revision,
        history_generation,
    };
    // Everything after capture is collected so final real observations run even
    // on policy, source, canonical-map, native-fact or reducer failures.
    let result = async {
        let read = config.read_policy(&session_id, lane)?;
        let fresh = source::read_branch(&pinned, lane, selected_leaf.as_deref(), &read)
            .await
            .map_err(source_error)?;
        if fresh.scope != capture {
            return Err(DurableError::Conflict);
        }
        let policy = config.transition_policy(
            read,
            fresh
                .records
                .iter()
                .map(|item| item.candidate.record().receipt.actor.as_str()),
        )?;
        let inspected = inspect_execution_session(&pinned, lane, selected_leaf.as_deref(), &policy)
            .await
            .map_err(inspection_error)?;
        if inspected.scope() != &capture {
            return Err(DurableError::Conflict);
        }
        let transitions = inspected.transitions();
        let root = transitions.ledger().root().cloned();
        // Tie the genuine inspected root and reducers to our actual resolver
        // bodies as well as to the common outer observation.
        match fresh.records.last() {
            Some(last) => {
                let record = last.candidate.record();
                if root.as_ref() != Some(&record.root)
                    || transitions.ledger().receipts() != fresh.records.len() as u64
                    || transitions.state() != &record.state
                    || transitions.execution_control() != &record.execution_control
                {
                    return Err(invalid());
                }
            }
            None if root.is_none() && transitions.ledger().receipts() == 0 => {}
            None => return Err(invalid()),
        }
        let snapshot = DurableSnapshot {
            scope: DurableScope {
                session_id,
                lane: lane.into(),
                binding_token: binding.clone(),
                leaf_id: selected_leaf,
                history_revision: revision,
                history_generation,
                entry_id: fresh.records.last().map(|item| item.entry.base.id.clone()),
                control_revision: transitions.state().revision,
            },
            state: transitions.state().clone(),
            execution_control: transitions.execution_control().clone(),
        };
        Ok(Loaded {
            snapshot,
            source: fresh,
            archive: archive.clone(),
            root,
        })
    }
    .await;
    // BOTH final I/O complete before propagating anything, including errors.
    // Original facade B is observed, not only the pinned facade's stable B.
    let leaf_after = view.get_leaf_id().await.map_err(|_| read_failure());
    let identity_after = storage.history_identity().await.map_err(|_| read_failure());
    let binding_after = original.binding_token();
    let leaf_after = leaf_after?;
    let identity_after = identity_after?;
    if leaf_after != current_leaf
        || identity_after.revision != revision
        || identity_after.generation != history_generation
        || binding_after != binding
    {
        return Err(DurableError::Conflict);
    }
    result
}
