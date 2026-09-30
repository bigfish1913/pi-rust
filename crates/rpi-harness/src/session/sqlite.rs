//! SQLite-backed session repository.
//!
//! The reducer remains the source of truth for session invariants. SQLite
//! stores one metadata row and a serialized reducer snapshot per session,
//! giving callers a durable, queryable repository without duplicating the
//! JSONL reducer logic. Writes are serialized per connection and committed
//! atomically after the in-memory mutation succeeds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{SessionError, SessionResult};
use crate::session::memory::{Clock, InMemorySessionStorage, SystemClock};
use crate::session::state::SessionState;
use crate::session::types::{
    BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery, SessionCreateOptions,
    SessionMetadata, SessionRepo, SessionStats, SessionStorage,
};

#[derive(Clone)]
pub struct SqliteSessionStorage {
    db: Arc<Mutex<Connection>>,
    inner: InMemorySessionStorage,
}

impl SqliteSessionStorage {
    fn new(
        db: Arc<Mutex<Connection>>,
        metadata: SessionMetadata,
        state: SessionState,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            db,
            inner: InMemorySessionStorage::from_state(metadata, state, clock, ids),
        }
    }

    fn persist(&self) -> SessionResult<()> {
        let metadata = self.inner.metadata();
        let state = serde_json::to_string(&self.inner.snapshot_state())
            .map_err(|e| SessionError::storage(format!("serialize SQLite session: {e}")))?;
        let changed = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute(
                "UPDATE sessions SET state_json = ?1 WHERE id = ?2",
                params![state, metadata.id],
            )
            .map_err(|e| SessionError::storage(format!("persist SQLite session: {e}")))?;
        if changed != 1 {
            return Err(SessionError::not_found(format!(
                "Session not found: {}",
                metadata.id
            )));
        }
        Ok(())
    }

    fn refresh(&self) -> SessionResult<()> {
        let id = self.inner.metadata().id;
        let state_json: String = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .query_row(
                "SELECT state_json FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| SessionError::storage(format!("refresh SQLite session: {e}")))?;
        let state = serde_json::from_str(&state_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite session: {e}")))?;
        self.inner.replace_state(state);
        Ok(())
    }
}

#[async_trait]
impl SessionStorage for SqliteSessionStorage {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.refresh()?;
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.refresh()?;
        self.inner.create_lane(lane, at).await?;
        self.persist()
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.refresh()?;
        self.inner.move_lane(lane, to).await?;
        self.persist()
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.refresh()?;
        let result = self.inner.append_entry(entry, lane).await?;
        self.persist()?;
        Ok(result)
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.refresh()?;
        let result = self.inner.append_record(record).await?;
        self.persist()?;
        Ok(result)
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.refresh()?;
        self.inner.get_entry(id).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.refresh()?;
        self.inner.find_entries(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.refresh()?;
        self.inner
            .find_entries_on_branch(query, bounds, start)
            .await
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.refresh()?;
        self.inner.find_records(query).await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.refresh()?;
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.refresh()?;
        self.inner.get_log(options).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.refresh()?;
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.refresh()?;
        self.inner.set_name(name).await?;
        self.persist()
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.refresh()?;
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.refresh()?;
        self.inner.set_label(id, label).await?;
        self.persist()
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.refresh()?;
        self.inner.get_stats().await
    }
}

pub struct SqliteSessionRepo {
    path: PathBuf,
    db: Arc<Mutex<Connection>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl SqliteSessionRepo {
    pub fn open(path: impl AsRef<Path>) -> SessionResult<Self> {
        Self::with_clock_ids(
            path,
            Arc::new(SystemClock),
            Arc::new(crate::session::DefaultIdGenerator::new()),
        )
    }

    pub fn with_clock_ids(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SessionError::storage(format!("create SQLite parent: {e}")))?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| SessionError::storage(format!("open SQLite database: {e}")))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 parent_session_id TEXT,
                 state_json TEXT NOT NULL
             );",
        )
        .map_err(|e| SessionError::storage(format!("initialize SQLite database: {e}")))?;
        Ok(Self {
            path,
            db: Arc::new(Mutex::new(conn)),
            clock,
            ids,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn insert(&self, metadata: &SessionMetadata, state: &SessionState) -> SessionResult<()> {
        let state = serde_json::to_string(state)
            .map_err(|e| SessionError::storage(format!("serialize SQLite state: {e}")))?;
        self.db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute(
                "INSERT INTO sessions (id, created_at, parent_session_id, state_json) VALUES (?1, ?2, ?3, ?4)",
                params![metadata.id, metadata.created_at, metadata.parent_session_id, state],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    SessionError::already_exists(format!("Session already exists: {}", metadata.id))
                }
                other => SessionError::storage(format!("insert SQLite session: {other}")),
            })?;
        Ok(())
    }

    fn load_row(&self, id: &str) -> SessionResult<Option<(SessionMetadata, SessionState)>> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        db.query_row(
            "SELECT id, created_at, parent_session_id, state_json FROM sessions WHERE id = ?1",
            params![id],
            |row| {
                let metadata = SessionMetadata {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    parent_session_id: row.get(2)?,
                };
                let state_json: String = row.get(3)?;
                let state = serde_json::from_str(&state_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok((metadata, state))
            },
        )
        .optional()
        .map_err(|e| SessionError::storage(format!("read SQLite session: {e}")))
    }
}

#[async_trait]
impl SessionRepo for SqliteSessionRepo {
    type Storage = SqliteSessionStorage;

    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        let metadata = SessionMetadata {
            id: options.id.clone().unwrap_or_else(|| self.ids.next()),
            created_at: self.clock.now_ms(),
            parent_session_id: options.parent_session_id.clone(),
        };
        let state = SessionState::new();
        self.insert(&metadata, &state)?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            state,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }

    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        let (metadata, state) = self.load_row(&metadata.id)?.ok_or_else(|| {
            SessionError::not_found(format!("Session not found: {}", metadata.id))
        })?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            state,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }

    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let mut stmt = db
            .prepare(
                "SELECT id, created_at, parent_session_id FROM sessions ORDER BY created_at DESC",
            )
            .map_err(|e| SessionError::storage(format!("list SQLite sessions: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionMetadata {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    parent_session_id: row.get(2)?,
                })
            })
            .map_err(|e| SessionError::storage(format!("list SQLite sessions: {e}")))?;
        rows.map(|row| row.map_err(|e| SessionError::storage(format!("read SQLite metadata: {e}"))))
            .collect()
    }

    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        self.db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute("DELETE FROM sessions WHERE id = ?1", params![metadata.id])
            .map_err(|e| SessionError::storage(format!("delete SQLite session: {e}")))?;
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        let source_storage = self.open(source).await?;
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let metadata = SessionMetadata {
            id,
            created_at: self.clock.now_ms(),
            parent_session_id: Some(source.id.clone()),
        };
        let forked = source_storage.inner.fork_from(
            metadata.clone(),
            fork,
            self.clock.clone(),
            self.ids.clone(),
        )?;
        let state = forked.snapshot_state();
        self.insert(&metadata, &state)?;
        Ok(SqliteSessionStorage::new(
            self.db.clone(),
            metadata,
            state,
            self.clock.clone(),
            self.ids.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::{EntryQuery, ForkOptions, ProvisionedKind};
    use rpi_agent::message::AgentMessage;

    fn repo() -> (SqliteSessionRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteSessionRepo::with_clock_ids(
            dir.path().join("sessions.db"),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .unwrap();
        (repo, dir)
    }

    #[tokio::test]
    async fn round_trip_append_and_fork() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("root".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        session
            .append_entry(
                ProvisionedEntry {
                    id: "m1".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("hello", 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let reopened = repo.open(&session.metadata()).await.unwrap();
        assert_eq!(
            reopened
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
        let second_handle = repo.open(&session.metadata()).await.unwrap();
        second_handle
            .append_entry(
                ProvisionedEntry {
                    id: "m2".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("world", 2)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(
            session
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .len(),
            2
        );
        let fork = repo
            .fork(
                &session.metadata(),
                &SessionCreateOptions {
                    id: Some("child".into()),
                    parent_session_id: None,
                    metadata: None,
                },
                &ForkOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(fork.metadata().parent_session_id.as_deref(), Some("root"));
        assert_eq!(repo.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_id_is_already_exists_and_delete_works() {
        let (repo, _dir) = repo();
        repo.create(&SessionCreateOptions {
            id: Some("same".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
        let err = match repo
            .create(&SessionCreateOptions {
                id: Some("same".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
        {
            Ok(_) => panic!("expected duplicate"),
            Err(e) => e,
        };
        assert_eq!(err.code, crate::error::SessionErrorCode::AlreadyExists);
        repo.delete(&SessionMetadata {
            id: "same".into(),
            created_at: 0,
            parent_session_id: None,
        })
        .await
        .unwrap();
        assert!(repo.list().await.unwrap().is_empty());
    }
}
