//! Administrative intent/result journal, separate from native prompt admission.
//! This module never executes an Agent. Only a fresh committed admission permits
//! the trusted coordinator to attempt an action; duplicate/uncertain work is not
//! replayed. No credentials, tool payloads or arbitrary result JSON are accepted.
use super::{
    AgentId, CatalogError, CatalogResult, ControlScopeId, HostEpoch, ProfileRef, RequestId, RunId,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[cfg(all(test, unix))]
#[path = "lifecycle_tests.rs"]
mod tests;

/// No eviction: losing request identities would permit unsafe replay. Capacity
/// exhaustion is an explicit pre-effect rejection, not a retention promise.
pub const MAX_LIFECYCLE_REQUESTS: usize = 1024;
const MAX_BYTES: usize = 4096;

/// Re-establish durability for visible committed rows, including retry after a
/// failed/uncertain acknowledgement. An empty read transaction/COMMIT is not a
/// synchronization barrier. Called only outside transactions by the sole worker.
/// FULL checkpoint must copy and sync every WAL frame; busy/partial/non-WAL
/// results cannot grant a fresh execution owner or acknowledge a receipt.
pub(super) fn checkpoint(connection: &Connection) -> CatalogResult<()> {
    let (busy, logged, synced): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 || logged < 0 || synced < logged {
        return Err(CatalogError::LifecycleUncertain);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRequest {
    pub scope: ControlScopeId,
    pub request_id: RequestId,
    pub epoch: HostEpoch,
    pub action: LifecycleAction,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleAction {
    /// Legacy unbound v2 intent. Retained for receipt compatibility; a profile
    /// coordinator must not execute it without an immutable configuration ref.
    Create {
        agent_id: AgentId,
    },
    /// Legacy unbound v2 intent; see `Create`.
    Restore {
        agent_id: AgentId,
        revision: u64,
    },
    CreateProfile {
        profile: ProfileRef,
    },
    RestoreProfile {
        profile: ProfileRef,
        revision: u64,
    },
    /// Exact volatile-ticket native intent retained for historical receipts.
    Cancel {
        agent_id: AgentId,
        revision: u64,
        ticket_id: RunId,
    },
    /// Stable input target, distinct from both this lifecycle request's ID and
    /// any transient duplicate observer ticket. No recovery or replay implied.
    CancelSubmitted {
        agent_id: AgentId,
        revision: u64,
        submitted_request_id: RequestId,
    },
    Close {
        agent_id: AgentId,
        revision: u64,
    },
    Stop {},
}
impl LifecycleAction {
    pub(crate) fn agent(&self) -> Option<AgentId> {
        match self {
            Self::Create { agent_id }
            | Self::Restore { agent_id, .. }
            | Self::Cancel { agent_id, .. }
            | Self::CancelSubmitted { agent_id, .. }
            | Self::Close { agent_id, .. } => Some(*agent_id),
            Self::CreateProfile { profile } | Self::RestoreProfile { profile, .. } => {
                Some(profile.agent_id)
            }
            Self::Stop {} => None,
        }
    }
    pub(crate) fn revision(&self) -> Option<u64> {
        match self {
            Self::Restore { revision, .. }
            | Self::RestoreProfile { revision, .. }
            | Self::Cancel { revision, .. }
            | Self::CancelSubmitted { revision, .. }
            | Self::Close { revision, .. } => Some(*revision),
            _ => None,
        }
    }
}
impl LifecycleRequest {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        if self
            .action
            .revision()
            .is_some_and(|n| !(1..=i64::MAX as u64).contains(&n))
        {
            return Err(CatalogError::InvalidRecord(
                "lifecycle revision outside stored range",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleResult {
    Loaded {
        revision: u64,
    },
    Closed {
        revision: u64,
    },
    /// A cooperative signal only; not proof of execution settlement.
    CancelSignalled {
        signalled: bool,
    },
    /// Native owners settled; not proof the process or listener has exited.
    NativeSettled {},
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleFailure {
    Unauthorized,
    InvalidRequest,
    NotFound,
    StaleEpoch,
    StaleRevision,
    Conflict,
    Busy,
    Capacity,
    PersistenceUncertain,
    Internal,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleState {
    Prepared {},
    Succeeded {
        result: LifecycleResult,
    },
    /// Known pre-effect rejection, not a generic backend failure.
    Rejected {
        code: LifecycleFailure,
    },
    /// Action/settlement may have happened. Inspect; never automatically retry.
    Uncertain {
        code: LifecycleFailure,
    },
    /// Previous owner lost before publishing a result. No replay authority.
    Interrupted {},
}
impl LifecycleState {
    fn kind(&self) -> &'static str {
        match self {
            Self::Prepared {} => "prepared",
            Self::Succeeded { .. } => "succeeded",
            Self::Rejected { .. } => "rejected",
            Self::Uncertain { .. } => "uncertain",
            Self::Interrupted {} => "interrupted",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleReceipt {
    pub request: LifecycleRequest,
    pub status: LifecycleState,
}
impl LifecycleReceipt {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        self.request.validate()?;
        if let LifecycleState::Succeeded { result } = &self.status {
            let compatible = match (&self.request.action, result) {
                (
                    LifecycleAction::Create { .. }
                    | LifecycleAction::Restore { .. }
                    | LifecycleAction::CreateProfile { .. }
                    | LifecycleAction::RestoreProfile { .. },
                    LifecycleResult::Loaded { revision },
                )
                | (LifecycleAction::Close { .. }, LifecycleResult::Closed { revision }) => {
                    (self.request.action.revision().unwrap_or(1)..=i64::MAX as u64)
                        .contains(revision)
                }
                (
                    LifecycleAction::Cancel { .. } | LifecycleAction::CancelSubmitted { .. },
                    LifecycleResult::CancelSignalled { .. },
                )
                | (LifecycleAction::Stop {}, LifecycleResult::NativeSettled {}) => true,
                _ => false,
            };
            if !compatible {
                return Err(CatalogError::InvalidRecord(
                    "lifecycle result does not match action",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct LifecycleAdmission {
    pub receipt: LifecycleReceipt,
    /// Exactly one committed preparer gets true. It is NOT execution success.
    /// A dropped response does not transfer authority to a duplicate request.
    pub fresh: bool,
}

pub(super) fn create_schema(connection: &Connection) -> CatalogResult<()> {
    connection.execute_batch("CREATE TABLE lifecycle_requests (
        scope_id TEXT NOT NULL, request_id TEXT NOT NULL, epoch TEXT NOT NULL,
        agent_id TEXT, status TEXT NOT NULL CHECK(status IN ('prepared','succeeded','rejected','uncertain','interrupted')),
        data TEXT NOT NULL CHECK(length(CAST(data AS BLOB)) <= 4096),
        PRIMARY KEY(scope_id,request_id)
    );
    CREATE INDEX lifecycle_pending ON lifecycle_requests(status,agent_id);
    PRAGMA user_version=2;")?;
    Ok(())
}
fn encode(receipt: &LifecycleReceipt) -> CatalogResult<String> {
    receipt.validate()?;
    let data = serde_json::to_string(receipt)?;
    if data.len() > MAX_BYTES {
        return Err(CatalogError::InvalidRecord(
            "lifecycle record exceeds byte budget",
        ));
    }
    Ok(data)
}
type Stored = (String, String, String, Option<String>, String, String);
fn decode(row: Stored) -> CatalogResult<LifecycleReceipt> {
    let (scope, id, epoch, agent, status, data) = row;
    if data.len() > MAX_BYTES {
        return Err(CatalogError::InvalidRecord(
            "lifecycle record exceeds byte budget",
        ));
    }
    let receipt: LifecycleReceipt = serde_json::from_str(&data)?;
    receipt.validate()?;
    if scope != receipt.request.scope.to_string()
        || id != receipt.request.request_id.to_string()
        || epoch != receipt.request.epoch.to_string()
        || agent != receipt.request.action.agent().map(|id| id.to_string())
        || status != receipt.status.kind()
    {
        return Err(CatalogError::InvalidRecord(
            "lifecycle indexed fields do not match receipt",
        ));
    }
    Ok(receipt)
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
pub(super) fn get(
    connection: &Connection,
    scope: ControlScopeId,
    id: RequestId,
) -> CatalogResult<Option<LifecycleReceipt>> {
    connection.query_row("SELECT scope_id,request_id,epoch,agent_id,status,data FROM lifecycle_requests WHERE scope_id=?1 AND request_id=?2", params![scope.to_string(),id.to_string()], row).optional()?.map(decode).transpose()
}
fn update(connection: &Connection, receipt: &LifecycleReceipt) -> CatalogResult<()> {
    let data = encode(receipt)?;
    if connection.execute(
        "UPDATE lifecycle_requests SET status=?3,data=?4 WHERE scope_id=?1 AND request_id=?2",
        params![
            receipt.request.scope.to_string(),
            receipt.request.request_id.to_string(),
            receipt.status.kind(),
            data
        ],
    )? != 1
    {
        return Err(CatalogError::UnknownLifecycle);
    }
    Ok(())
}
/// Called inside the same startup transaction that classifies Agent state and
/// advances epoch. Validate every bounded row before committing any changes.
pub(super) fn recover(connection: &Connection, previous: HostEpoch) -> CatalogResult<()> {
    let count: usize =
        connection.query_row("SELECT count(*) FROM lifecycle_requests", [], |r| r.get(0))?;
    if count > MAX_LIFECYCLE_REQUESTS {
        return Err(CatalogError::InvalidRecord("lifecycle capacity exceeded"));
    }
    let mut cursor = (String::new(), String::new());
    loop {
        let rows = {
            let mut statement = connection.prepare("SELECT scope_id,request_id,epoch,agent_id,status,data FROM lifecycle_requests WHERE (scope_id,request_id) > (?1,?2) ORDER BY scope_id,request_id LIMIT 128")?;
            let rows = statement
                .query_map(params![cursor.0, cursor.1], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = (row.0.clone(), row.1.clone());
            let mut receipt = decode(row)?;
            if receipt.status == (LifecycleState::Prepared {}) {
                if receipt.request.epoch != previous {
                    return Err(CatalogError::InvalidRecord(
                        "pending lifecycle belongs to another epoch",
                    ));
                }
                receipt.status = LifecycleState::Interrupted {};
                update(connection, &receipt)?;
            }
        }
    }
    Ok(())
}
pub(super) fn has_pending(connection: &Connection, agent: Option<AgentId>) -> CatalogResult<bool> {
    let agent = agent.map(|id| id.to_string());
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM lifecycle_requests WHERE status='prepared' AND (agent_id IS NULL OR ?1 IS NULL OR agent_id=?1))", [&agent], |r| r.get(0))?)
}

pub(super) fn prepare(
    connection: &mut Connection,
    epoch: HostEpoch,
    request: LifecycleRequest,
) -> CatalogResult<LifecycleAdmission> {
    request.validate()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(receipt) = get(&transaction, request.scope, request.request_id)? {
        if receipt.request != request {
            return Err(CatalogError::LifecycleConflict);
        }
        return Ok(LifecycleAdmission {
            receipt,
            fresh: false,
        });
    }
    if request.epoch != epoch {
        return Err(CatalogError::EpochConflict);
    }
    let count: usize =
        transaction.query_row("SELECT count(*) FROM lifecycle_requests", [], |r| r.get(0))?;
    if count >= MAX_LIFECYCLE_REQUESTS {
        return Err(CatalogError::LifecycleCapacity);
    }
    let agent = request.action.agent().map(|id| id.to_string());
    if has_pending(&transaction, request.action.agent())? {
        return Err(CatalogError::LifecycleBusy);
    }
    if let Some(id) = request.action.agent() {
        let record = super::registry::find(&transaction, id)?;
        match (&request.action, record) {
            (LifecycleAction::Create { .. } | LifecycleAction::CreateProfile { .. }, None) => {}
            (LifecycleAction::Create { .. } | LifecycleAction::CreateProfile { .. }, Some(_)) => {
                return Err(CatalogError::AlreadyRegistered)
            }
            (_, None) => return Err(CatalogError::UnknownAgent),
            (_, Some(record)) => {
                let expected = request.action.revision().unwrap();
                if expected != record.revision {
                    return Err(CatalogError::RevisionConflict {
                        expected,
                        actual: record.revision,
                    });
                }
            }
        }
    }
    let receipt = LifecycleReceipt {
        request,
        status: LifecycleState::Prepared {},
    };
    let data = encode(&receipt)?;
    transaction.execute("INSERT INTO lifecycle_requests (scope_id,request_id,epoch,agent_id,status,data) VALUES (?1,?2,?3,?4,'prepared',?5)", params![receipt.request.scope.to_string(),receipt.request.request_id.to_string(),receipt.request.epoch.to_string(),agent,data])?;
    // Once a guarded explicit Close is admitted, a crash during native cleanup
    // must not bring that member back. Whole-Host Stop leaves the set intact.
    if let LifecycleAction::Close { agent_id, .. } = receipt.request.action {
        super::recovery::prepare_close(&transaction, agent_id)?;
    }
    transaction.commit()?;
    Ok(LifecycleAdmission {
        receipt,
        fresh: true,
    })
}
pub(super) fn finish(
    connection: &mut Connection,
    epoch: HostEpoch,
    request: LifecycleRequest,
    status: LifecycleState,
    bindings: &super::recovery::Bindings,
) -> CatalogResult<LifecycleReceipt> {
    if matches!(
        status,
        LifecycleState::Prepared {} | LifecycleState::Interrupted {}
    ) {
        return Err(CatalogError::InvalidRecord(
            "only terminal lifecycle results can be published",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut receipt = get(&transaction, request.scope, request.request_id)?
        .ok_or(CatalogError::UnknownLifecycle)?;
    if receipt.request != request {
        return Err(CatalogError::LifecycleConflict);
    }
    if receipt.status != (LifecycleState::Prepared {}) {
        return if receipt.status == status {
            Ok(receipt)
        } else {
            Err(CatalogError::LifecycleConflict)
        };
    }
    if request.epoch != epoch {
        return Err(CatalogError::EpochConflict);
    }
    receipt.status = status;
    update(&transaction, &receipt)?;
    if let LifecycleAction::Close { agent_id, .. } = request.action {
        super::recovery::finish_close(
            &transaction,
            agent_id,
            matches!(receipt.status, LifecycleState::Rejected { .. }),
        )?;
    }
    if matches!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::Loaded { .. }
        }
    ) {
        if let LifecycleAction::CreateProfile { profile }
        | LifecycleAction::RestoreProfile { profile, .. } = request.action
        {
            if let Some(binding) = bindings.get(&profile.agent_id) {
                super::recovery::set(&transaction, profile.agent_id, *binding)?;
            }
        }
    }
    transaction.commit()?;
    Ok(receipt)
}
