use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// A host-side mutation before the backend assigns its journal sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalMutation {
    pub timestamp: i64,
    pub kind: String,
    pub payload: Value,
}

impl JournalMutation {
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.kind.is_empty() || self.kind.len() > 128 {
            return Err(BackendError::Invalid(
                "journal kind must be 1..128 bytes".into(),
            ));
        }
        if self.kind.chars().any(char::is_control) {
            return Err(BackendError::Invalid(
                "journal kind contains a control character".into(),
            ));
        }
        Ok(())
    }
}

/// One committed durable mutation. The sequence is assigned by the backend,
/// not by an Agent Loop or a frontend adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalFrame {
    pub sequence: u64,
    pub timestamp: i64,
    pub kind: String,
    pub payload: Value,
}

impl JournalFrame {
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.sequence == 0 {
            return Err(BackendError::Invalid(
                "journal sequence must be positive".into(),
            ));
        }
        JournalMutation {
            timestamp: self.timestamp,
            kind: self.kind.clone(),
            payload: self.payload.clone(),
        }
        .validate()
    }
}

/// Errors exposed by durable storage implementations.
#[derive(Debug)]
pub enum BackendError {
    Io(std::io::Error),
    Encode(serde_json::Error),
    Invalid(String),
    Unsupported(String),
    Conflict {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    #[cfg(feature = "sqlite")]
    Sqlite(rusqlite::Error),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "durable storage I/O error: {error}"),
            Self::Encode(error) => write!(f, "durable storage JSON error: {error}"),
            Self::Invalid(message) => write!(f, "invalid durable journal: {message}"),
            Self::Unsupported(message) => {
                write!(f, "unsupported durable storage operation: {message}")
            }
            Self::Conflict { expected, actual } => write!(
                f,
                "durable journal revision conflict: expected {expected:?}, actual {actual:?}"
            ),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(error) => write!(f, "durable SQLite error: {error}"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<std::io::Error> for BackendError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for BackendError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error)
    }
}

#[cfg(feature = "sqlite")]
impl From<rusqlite::Error> for BackendError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Persistence boundary owned by `rpi-durable`.
///
/// The caller supplies the expected current sequence. Implementations assign
/// the next sequence and reject a stale expectation before making the append
/// visible. This is a storage CAS boundary, not a model/tool authorization
/// mechanism.
pub trait DurableBackend {
    fn load(&mut self) -> Result<Vec<JournalFrame>, BackendError>;

    fn append(
        &mut self,
        mutation: JournalMutation,
        expected_sequence: Option<u64>,
    ) -> Result<JournalFrame, BackendError>;

    /// Append a logical batch under one backend revision check. Implementations
    /// must validate and allocate every frame before making the batch visible.
    /// JSONL still does not claim power-loss atomicity; SQLite uses a database
    /// transaction.
    fn append_batch(
        &mut self,
        _mutations: Vec<JournalMutation>,
        _expected_sequence: Option<u64>,
    ) -> Result<Vec<JournalFrame>, BackendError> {
        Err(BackendError::Unsupported(
            "durable batch append is unsupported by this backend".into(),
        ))
    }

    fn head(&mut self) -> Result<Option<u64>, BackendError>;
}

pub(crate) fn next_frames(
    current: Option<u64>,
    mutations: Vec<JournalMutation>,
    expected: Option<u64>,
) -> Result<Vec<JournalFrame>, BackendError> {
    if mutations.is_empty() {
        if current != expected {
            return Err(BackendError::Conflict {
                expected,
                actual: current,
            });
        }
        return Ok(Vec::new());
    }
    let mut frames = Vec::with_capacity(mutations.len());
    let mut previous = current;
    for mutation in mutations {
        let frame = next_frame(
            previous,
            mutation,
            if frames.is_empty() {
                expected
            } else {
                previous
            },
        )?;
        previous = Some(frame.sequence);
        frames.push(frame);
    }
    Ok(frames)
}

pub(crate) fn next_frame(
    current: Option<u64>,
    mutation: JournalMutation,
    expected: Option<u64>,
) -> Result<JournalFrame, BackendError> {
    mutation.validate()?;
    if current != expected {
        return Err(BackendError::Conflict {
            expected,
            actual: current,
        });
    }
    let sequence = current
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| BackendError::Invalid("journal sequence overflow".into()))?;
    Ok(JournalFrame {
        sequence,
        timestamp: mutation.timestamp,
        kind: mutation.kind,
        payload: mutation.payload,
    })
}
