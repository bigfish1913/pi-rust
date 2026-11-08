//! SQLite durable backend.
//!
//! This is an optional backend implementation. The schema stores the same
//! backend-neutral [`JournalFrame`] shape as JSONL and assigns sequences inside
//! the durable storage boundary.

use super::backend::{
    next_frame, next_frames, BackendError, DurableBackend, JournalFrame, JournalMutation,
};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use std::path::Path;

pub struct SqliteBackend {
    connection: Connection,
}

impl SqliteBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS durable_journal (
                sequence INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL
            );",
        )?;
        let mut backend = Self { connection };
        // Validate existing rows before exposing the backend. Otherwise an
        // externally-corrupted database could pass open() based only on MAX()
        // and fail later during load/replay.
        backend.load()?;
        Ok(backend)
    }

    pub fn in_memory() -> Result<Self, BackendError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE durable_journal (
                sequence INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL
            );",
        )?;
        let mut backend = Self { connection };
        backend.load()?;
        Ok(backend)
    }

    fn current_sequence_from(connection: &Connection) -> Result<Option<u64>, BackendError> {
        let value: Option<i64> =
            connection.query_row("SELECT MAX(sequence) FROM durable_journal", [], |row| {
                row.get(0)
            })?;
        value
            .map(|value| {
                u64::try_from(value)
                    .map_err(|_| BackendError::Invalid("negative journal sequence".into()))
            })
            .transpose()
    }

    fn load_from(connection: &Connection) -> Result<Vec<JournalFrame>, BackendError> {
        let mut statement = connection.prepare(
            "SELECT sequence, timestamp, kind, payload
             FROM durable_journal ORDER BY sequence ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let sequence: i64 = row.get(0)?;
            let timestamp = row.get(1)?;
            let kind = row.get(2)?;
            let payload: String = row.get(3)?;
            Ok((sequence, timestamp, kind, payload))
        })?;

        let mut frames: Vec<JournalFrame> = Vec::new();
        for row in rows {
            let (sequence, timestamp, kind, payload) = row?;
            let sequence = u64::try_from(sequence)
                .map_err(|_| BackendError::Invalid("negative journal sequence".into()))?;
            let frame = JournalFrame {
                sequence,
                timestamp,
                kind,
                payload: serde_json::from_str(&payload)?,
            };
            frame.validate()?;
            if let Some(previous) = frames.last() {
                let expected = previous
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| BackendError::Invalid("journal sequence overflow".into()))?;
                if expected != frame.sequence {
                    return Err(BackendError::Invalid(
                        "journal sequence is not contiguous".into(),
                    ));
                }
            }
            frames.push(frame);
        }
        Ok(frames)
    }
}

impl DurableBackend for SqliteBackend {
    fn load(&mut self) -> Result<Vec<JournalFrame>, BackendError> {
        Self::load_from(&self.connection)
    }

    fn append(
        &mut self,
        mutation: JournalMutation,
        expected_sequence: Option<u64>,
    ) -> Result<JournalFrame, BackendError> {
        // IMMEDIATE makes the head read, CAS check and insert one serialized
        // writer operation across independent SQLite connections.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::load_from(&transaction)?;
        let current = Self::current_sequence_from(&transaction)?;
        let frame = next_frame(current, mutation, expected_sequence)?;
        insert_frame(&transaction, &frame)?;
        transaction.commit()?;
        Ok(frame)
    }

    fn append_batch(
        &mut self,
        mutations: Vec<JournalMutation>,
        expected_sequence: Option<u64>,
    ) -> Result<Vec<JournalFrame>, BackendError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::load_from(&transaction)?;
        let current = Self::current_sequence_from(&transaction)?;
        let frames = next_frames(current, mutations, expected_sequence)?;
        if frames.is_empty() {
            transaction.commit()?;
            return Ok(frames);
        }

        for frame in &frames {
            insert_frame(&transaction, frame)?;
        }
        transaction.commit()?;
        Ok(frames)
    }

    fn head(&mut self) -> Result<Option<u64>, BackendError> {
        Ok(Self::load_from(&self.connection)?
            .last()
            .map(|frame| frame.sequence))
    }
}

fn insert_frame(transaction: &Transaction<'_>, frame: &JournalFrame) -> Result<(), BackendError> {
    let payload = serde_json::to_string(&frame.payload)?;
    transaction.execute(
        "INSERT INTO durable_journal(sequence, timestamp, kind, payload)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            i64::try_from(frame.sequence).map_err(|_| {
                BackendError::Invalid("journal sequence exceeds SQLite range".into())
            })?,
            frame.timestamp,
            frame.kind,
            payload,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_is_transactional_and_assigns_contiguous_sequences() {
        let mut backend = SqliteBackend::in_memory().unwrap();
        let mutation = |sequence| JournalMutation {
            timestamp: sequence as i64,
            kind: "test".into(),
            payload: serde_json::json!({"sequence": sequence}),
        };
        let frames = backend
            .append_batch(vec![mutation(1), mutation(2)], None)
            .unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let invalid = JournalMutation {
            timestamp: 3,
            kind: "bad\nkind".into(),
            payload: serde_json::json!({}),
        };
        assert!(backend
            .append_batch(vec![mutation(3), invalid], Some(2))
            .is_err());
        assert_eq!(backend.head().unwrap(), Some(2));
    }

    #[test]
    fn head_rechecks_full_journal_after_external_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let mut backend = SqliteBackend::open(&path).unwrap();
        let mutation = JournalMutation {
            timestamp: 1,
            kind: "test".into(),
            payload: serde_json::json!({"sequence": 1}),
        };
        backend.append(mutation, None).unwrap();

        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO durable_journal(sequence, timestamp, kind, payload)
                 VALUES (3, 3, 'test', '{\"sequence\":3}')",
                [],
            )
            .unwrap();

        assert!(matches!(
            backend.head(),
            Err(BackendError::Invalid(message)) if message.contains("not contiguous")
        ));
    }

    #[test]
    fn appends_with_revision_cas_and_assigns_sequences() {
        let mut backend = SqliteBackend::in_memory().unwrap();
        let mutation = |sequence| JournalMutation {
            timestamp: sequence as i64,
            kind: "test".into(),
            payload: serde_json::json!({"sequence": sequence}),
        };
        assert_eq!(backend.append(mutation(1), None).unwrap().sequence, 1);
        assert_eq!(backend.append(mutation(2), Some(1)).unwrap().sequence, 2);
        assert_eq!(backend.head().unwrap(), Some(2));
        assert!(matches!(
            backend.append(mutation(3), Some(0)),
            Err(BackendError::Conflict { .. })
        ));
    }
}
