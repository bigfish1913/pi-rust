use super::AgentState;

pub type HostResult<T> = Result<T, HostError>;

/// Fixed diagnostic labels: do not put prompts, credentials or raw argv here.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HostError {
    #[error("invalid host input: {0}")]
    InvalidInput(&'static str),
    #[error("invalid Agent transition {from:?} -> {to:?}")]
    InvalidTransition { from: AgentState, to: AgentState },
    #[error("mini Agent cannot be restored after losing its memory state")]
    MiniStateLost,
    #[error("resident Agent limit {limit} reached")]
    AtCapacity { limit: usize },
    #[error("Agent already owns a resident slot")]
    AlreadyResident,
    #[error("host capacity state is poisoned")]
    Poisoned,
}

pub type CatalogResult<T> = Result<T, CatalogError>;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Policy(#[from] HostError),
    #[error("host registry is already owned by another instance")]
    AlreadyRunning,
    #[error("host registry path must be private, owned by this user, and not a symlink")]
    UnsafePath,
    #[error("host catalog is not supported on this platform yet")]
    UnsupportedPlatform,
    #[error("unsupported host registry schema version {0}")]
    UnsupportedVersion(i64),
    #[error("invalid stored host record: {0}")]
    InvalidRecord(&'static str),
    #[error("Agent is already registered")]
    AlreadyRegistered,
    #[error("Session reference is already registered to another Agent")]
    SessionAlreadyRegistered,
    #[error("unknown Agent")]
    UnknownAgent,
    #[error("Agent revision changed (expected {expected}, actual {actual})")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("Host epoch changed")]
    EpochConflict,
    #[error("lifecycle request identity conflicts with the stored request or result")]
    LifecycleConflict,
    #[error("lifecycle journal capacity exhausted; no intent was accepted")]
    LifecycleCapacity,
    #[error("another lifecycle intent conflicts with this action; no new intent was accepted")]
    LifecycleBusy,
    #[error("unknown lifecycle request")]
    UnknownLifecycle,
    #[error("lifecycle journal synchronization is uncertain; inspect before retrying")]
    LifecycleUncertain,
    #[error("host catalog worker stopped; inspect state before retrying an uncertain mutation")]
    WorkerStopped,
    #[error("host registry I/O failed")]
    Io(#[from] std::io::Error),
    #[error("host registry database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("host registry encoding is invalid")]
    Encoding(#[from] serde_json::Error),
}
