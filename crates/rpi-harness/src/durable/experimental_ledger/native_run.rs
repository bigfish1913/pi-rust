//! Explicit as-of native run facts, not control replay or journal authority.
//! The real current scope is observed independently of the historical record
//! view. No synthetic checkpoint is accepted, no provider/tool is invoked, and
//! the private read view cannot mutate storage or escape this invocation.
mod records;
use super::{
    source::{read_branch, SourceError, SourceScope},
    ReadPolicy,
};
use crate::{
    agent_harness::AgentHarness,
    durable::{validate_identity, validate_run_id, DurableError, DurableScope},
    session::Session,
};
use rpi_durable::execution::RunCheckpoint;
use std::sync::Arc;

pub const MAX_NATIVE_RECORDS: usize = 4096;
pub const MAX_NATIVE_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NativeRunInspection {
    scope: SourceScope,
    as_of_revision: u64,
    run: RunCheckpoint,
}
impl NativeRunInspection {
    /// Actual current observation, NOT the historical watermark.
    pub fn scope(&self) -> &SourceScope {
        &self.scope
    }
    pub fn as_of_revision(&self) -> u64 {
        self.as_of_revision
    }
    pub fn run(&self) -> &RunCheckpoint {
        &self.run
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeRunError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("invalid historical native run watermark or selected leaf")]
    Watermark,
    #[error("invalid historical native lane record page")]
    Records,
    #[error(transparent)]
    Native(#[from] DurableError),
}

/// Inspect one explicit run on one explicit leaf (None is empty, never latest).
/// ReadPolicy binds an experimental epoch/session/lane, not actor authority.
/// The leaf must already exist at `as_of_revision <= actual current revision`.
/// All lane records at/before that watermark are snapshotted within fixed logical
/// budgets before the shared native proof can query them. Later facts are never
/// mistaken for earlier terminal/tool outcomes. This does NOT replay receipts or
/// ExecutionControl, authenticate policy origin, or yield a VerifiedJournal.
/// Once captured, actual outer leaf/revision/binding checks run on errors too;
/// both final I/O complete before Read > Conflict > collected-error precedence.
/// Observations are not an atomic/ABA fence. Cancellation spawns no worker and
/// does not guarantee final observations. Backend resolver honesty is assumed.
pub async fn inspect_native_run(
    harness: &AgentHarness,
    lane: &str,
    selected_leaf_id: Option<&str>,
    as_of_revision: u64,
    run_id: &str,
    policy: &ReadPolicy,
) -> Result<NativeRunInspection, NativeRunError> {
    inspect_native_run_session(
        harness.session(),
        lane,
        selected_leaf_id,
        as_of_revision,
        run_id,
        policy,
    )
    .await
}

// Startup admission uses the real Session before a harness or recovery exists.
pub(crate) async fn inspect_native_run_session(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
    as_of_revision: u64,
    run_id: &str,
    policy: &ReadPolicy,
) -> Result<NativeRunInspection, NativeRunError> {
    validate_identity(lane).map_err(|_| SourceError::Identity)?;
    if let Some(id) = selected_leaf_id {
        validate_identity(id).map_err(|_| SourceError::Identity)?;
    }
    validate_run_id(run_id)?;
    let pinned = source.pin(Arc::clone(source.id_generator()));
    let binding = pinned.binding_token();
    let storage = pinned.storage();
    let session_id = storage.metadata().id;
    validate_identity(&session_id).map_err(|_| SourceError::Identity)?;
    if session_id != policy.epoch.session_id || lane != policy.epoch.lane {
        return Err(SourceError::Policy.into());
    }
    let identity = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read)?;
    let revision = identity.revision;
    let history_generation = identity.generation;
    let view = pinned.view(lane);
    let leaf = view.get_leaf_id().await.map_err(|_| SourceError::Read)?;
    let scope = SourceScope {
        session_id: session_id.clone(),
        lane: lane.into(),
        binding_token: binding.clone(),
        selected_leaf_id: selected_leaf_id.map(str::to_owned),
        current_leaf_id: leaf.clone(),
        history_revision: revision,
        history_generation,
    };
    let result: Result<RunCheckpoint, NativeRunError> = async {
        if as_of_revision > revision {
            return Err(NativeRunError::Watermark);
        }
        let fresh = read_branch(&pinned, lane, selected_leaf_id, policy).await?;
        if fresh.scope != scope {
            return Err(SourceError::Conflict.into());
        }
        // The source walker has already checked the full path. Check the tip's
        // additional historical bound; descendants may not leak into this view.
        if let Some(id) = selected_leaf_id {
            let entry = storage
                .get_entry(id)
                .await
                .map_err(|_| SourceError::Read)?
                .ok_or(SourceError::Metadata)?;
            if entry.id() != id || entry.seq() == 0 || entry.seq() > as_of_revision {
                return Err(NativeRunError::Watermark);
            }
        }
        let historical = records::capture(
            Arc::clone(&storage),
            lane,
            selected_leaf_id,
            as_of_revision,
            history_generation,
        )
        .await?;
        let historical_scope = DurableScope {
            session_id,
            lane: lane.into(),
            binding_token: binding.clone(),
            leaf_id: selected_leaf_id.map(str::to_owned),
            history_revision: as_of_revision,
            history_generation,
            entry_id: None,
            control_revision: 0,
        };
        // Same native interval/branch/tool/terminal proof as the live service.
        // Its logical revision check is relative to this private historical view;
        // it is NOT the real-current observation performed below.
        Ok(AgentHarness::durable_read_execution_from_storage(
            source,
            &historical_scope,
            run_id,
            &historical,
        )
        .await?)
    }
    .await;
    let leaf_after = view.get_leaf_id().await.map_err(|_| SourceError::Read);
    let history_identity_after = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read);
    let binding_after = source.binding_token();
    let leaf_after = leaf_after?;
    let history_identity_after = history_identity_after?;
    if leaf_after != leaf || history_identity_after != identity || binding_after != binding {
        return Err(SourceError::Conflict.into());
    }
    Ok(NativeRunInspection {
        scope,
        as_of_revision,
        run: result?,
    })
}
