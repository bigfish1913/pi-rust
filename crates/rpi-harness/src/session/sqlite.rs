//! SQLite-backed session repository.
//!
//! The reducer remains the source of truth for session invariants. SQLite
//! stores one metadata row and a serialized reducer snapshot per session,
//! giving callers a durable, queryable repository without duplicating the
//! JSONL reducer logic. Writes are serialized per connection and committed
//! atomically after the in-memory mutation succeeds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{SessionError, SessionResult};
use crate::session::memory::{Clock, InMemorySessionStorage, SystemClock};
use crate::session::state::SessionState;
use crate::session::types::{
    BranchBounds, Entry, EntryQuery, ForkOptions, HistoryCheckpoint, HistoryIdentity,
    HistoryReplacement, IdGenerator, LanePointer, LaneRecord, LogItem, LogOptions,
    OperationStartedRecord, ProvisionedEntry, RecordQuery, SessionCreateOptions, SessionMetadata,
    SessionRepo, SessionStats, SessionStorage,
};

#[derive(Clone)]
pub struct SqliteSessionStorage {
    db: Arc<Mutex<Connection>>,
    inner: InMemorySessionStorage,
    gate: Arc<tokio::sync::Mutex<()>>,
    /// Persisted per-session mutation fence observed by this binding.
    writer_epoch: Arc<AtomicU64>,
}

impl SqliteSessionStorage {
    fn new(
        db: Arc<Mutex<Connection>>,
        metadata: SessionMetadata,
        state: SessionState,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        gate: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            db,
            inner: InMemorySessionStorage::from_state(metadata, state, clock, ids),
            gate,
            writer_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    fn persist_cas(&self, previous: &str) -> SessionResult<bool> {
        let metadata = self.inner.metadata();
        let state = serde_json::to_string(&self.inner.snapshot_state())
            .map_err(|e| SessionError::storage(format!("serialize SQLite session: {e}")))?;
        let expected_epoch = self.writer_epoch.load(Ordering::Acquire);
        let changed = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .execute(
                "UPDATE sessions SET state_json = ?1, writer_epoch = writer_epoch + 1 WHERE id = ?2 AND state_json = ?3 AND writer_epoch = ?4",
                params![state, metadata.id, previous, i64::try_from(expected_epoch).map_err(|_| SessionError::storage("SQLite writer epoch exceeds INTEGER"))?],
            )
            .map_err(|e| SessionError::storage(format!("persist SQLite session: {e}")))?;
        if changed == 1 {
            self.writer_epoch.fetch_add(1, Ordering::AcqRel);
        }
        Ok(changed == 1)
    }

    async fn write_retry<T, F, Fut>(&self, op: F) -> SessionResult<T>
    where
        F: Fn(InMemorySessionStorage) -> Fut,
        Fut: std::future::Future<Output = SessionResult<T>>,
    {
        // Refresh, reduce, and CAS the exact snapshot that was refreshed. A
        // concurrent connection either wins the CAS or causes this operation
        // to replay against the newer state; its successful return is durable.
        let _gate = self.gate.lock().await;
        for _ in 0..16 {
            let previous = self.refresh_unlocked()?;
            match op(self.inner.clone()).await {
                Ok(value) => match self.persist_cas(&previous) {
                    Ok(true) => return Ok(value),
                    Ok(false) => continue,
                    Err(error) => {
                        self.restore_snapshot(&previous)?;
                        return Err(error);
                    }
                },
                Err(error) => {
                    self.restore_snapshot(&previous)?;
                    return Err(error);
                }
            }
        }
        Err(SessionError::storage(
            "SQLite write conflicted too many times",
        ))
    }

    fn restore_snapshot(&self, state_json: &str) -> SessionResult<()> {
        let state = serde_json::from_str(state_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite session: {e}")))?;
        self.inner.replace_state(state);
        Ok(())
    }

    fn refresh_unlocked(&self) -> SessionResult<String> {
        let id = self.inner.metadata().id;
        let (state_json, writer_epoch): (String, i64) = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .query_row(
                "SELECT state_json, writer_epoch FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| SessionError::storage(format!("refresh SQLite session: {e}")))?;
        let writer_epoch = u64::try_from(writer_epoch)
            .map_err(|_| SessionError::storage("SQLite writer epoch is negative"))?;
        let state = serde_json::from_str(&state_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite session: {e}")))?;
        self.inner.replace_state(state);
        self.writer_epoch.store(writer_epoch, Ordering::Release);
        Ok(state_json)
    }

    async fn refresh_read(&self) -> SessionResult<tokio::sync::OwnedMutexGuard<()>> {
        let gate = self.gate.clone().lock_owned().await;
        self.refresh_unlocked()?;
        Ok(gate)
    }
}

mod progress;

#[async_trait]
impl SessionStorage for SqliteSessionStorage {
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.prune_completed_progress_sql().await
    }

    async fn append_progress(
        &self,
        records: Vec<crate::session::types::AssistantFrameRecord>,
    ) -> SessionResult<Vec<crate::session::types::AssistantFrameRecord>> {
        self.append_progress_sql(records).await
    }
    async fn read_progress(
        &self,
        run_id: &str,
    ) -> SessionResult<Vec<crate::session::types::AssistantFrameRecord>> {
        self.read_progress_sql(run_id).await
    }
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        let _gate = self.refresh_read().await?;
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let at = at.map(str::to_string);
        self.write_retry(move |inner| {
            let lane = lane.clone();
            let at = at.clone();
            async move { inner.create_lane(&lane, at.as_deref()).await }
        })
        .await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let to = to.map(str::to_string);
        self.write_retry(move |inner| {
            let lane = lane.clone();
            let to = to.clone();
            async move { inner.move_lane(&lane, to.as_deref()).await }
        })
        .await
    }
    async fn append_queue_cancellations(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        self.write_retry(move |inner| {
            let records = records.clone();
            async move { inner.append_queue_cancellations(records).await }
        })
        .await
    }

    async fn history_revision(&self) -> SessionResult<u64> {
        let _gate = self.refresh_read().await?;
        self.inner.history_revision().await
    }

    async fn history_generation(&self) -> SessionResult<u64> {
        let _gate = self.gate.clone().lock_owned().await;
        let id = self.inner.metadata().id;
        let generation: i64 = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?
            .query_row(
                "SELECT history_generation FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| SessionError::storage(format!("read SQLite history generation: {e}")))?;
        u64::try_from(generation)
            .map_err(|_| SessionError::storage("SQLite history generation is negative"))
    }

    async fn history_identity(&self) -> SessionResult<HistoryIdentity> {
        let _gate = self.gate.clone().lock_owned().await;
        let id = self.inner.metadata().id;
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let (state_json, generation): (String, i64) = db
            .query_row(
                "SELECT state_json, history_generation FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| SessionError::storage(format!("read SQLite history identity: {e}")))?;
        let state: SessionState = serde_json::from_str(&state_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite history identity: {e}")))?;
        Ok(HistoryIdentity {
            revision: state.sequence(),
            generation: u64::try_from(generation)
                .map_err(|_| SessionError::storage("SQLite history generation is negative"))?,
        })
    }

    async fn history_checkpoint(&self) -> SessionResult<Option<HistoryCheckpoint>> {
        let _gate = self.gate.clone().lock_owned().await;
        let id = self.inner.metadata().id;
        let db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let (state_json, generation, checkpoint_published, archive_json):
            (String, i64, i64, Option<String>) = db
            .query_row(
                "SELECT state_json, history_generation, checkpoint_published, checkpoint_archive_json FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| SessionError::storage(format!("read SQLite history checkpoint: {e}")))?;
        let state: SessionState = serde_json::from_str(&state_json).map_err(|e| {
            SessionError::storage(format!("decode SQLite history checkpoint state: {e}"))
        })?;
        let generation = u64::try_from(generation)
            .map_err(|_| SessionError::storage("SQLite history generation is negative"))?;
        if checkpoint_published == 0 {
            return Ok(None);
        }
        let archive = archive_json
            .map(|archive_json| serde_json::from_str(&archive_json))
            .transpose()
            .map_err(|e| SessionError::storage(format!("decode SQLite receipt archive: {e}")))?;
        let checkpoint = HistoryCheckpoint::new(
            HistoryIdentity {
                revision: state.sequence(),
                generation,
            },
            state,
        )?;
        let checkpoint = match archive {
            Some(archive) => checkpoint.with_receipt_archive(archive)?,
            None => checkpoint,
        };
        Ok(Some(checkpoint))
    }

    async fn replace_history_with_checkpoint_if_revision_and_generation(
        &self,
        expected_revision: u64,
        expected_generation: u64,
        checkpoint: HistoryCheckpoint,
    ) -> SessionResult<HistoryIdentity> {
        checkpoint.validate()?;
        if checkpoint.history_identity
            != (HistoryIdentity {
                revision: expected_revision,
                generation: expected_generation,
            })
        {
            return Err(SessionError::storage(
                "history checkpoint identity does not match replacement CAS",
            ));
        }
        let replacement_revision = checkpoint.state.sequence();
        let replacement_json = serde_json::to_string(&checkpoint.state)
            .map_err(|e| SessionError::storage(format!("serialize SQLite checkpoint: {e}")))?;
        let archive_json = checkpoint
            .receipt_archive
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| SessionError::storage(format!("serialize SQLite receipt archive: {e}")))?;
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| SessionError::storage("history replacement generation exhausted"))?;
        let next_generation_sql = i64::try_from(next_generation)
            .map_err(|_| SessionError::storage("SQLite history generation exceeds INTEGER"))?;
        let expected_generation_sql = i64::try_from(expected_generation)
            .map_err(|_| SessionError::storage("SQLite history generation exceeds INTEGER"))?;
        let id = self.inner.metadata().id;
        let mut db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let tx = db.transaction().map_err(|e| {
            SessionError::storage(format!("begin SQLite checkpoint replacement: {e}"))
        })?;
        let (current_json, current_generation, current_writer_epoch): (String, i64, i64) = tx
            .query_row(
                "SELECT state_json, history_generation, writer_epoch FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| SessionError::storage(format!("read SQLite checkpoint identity: {e}")))?;
        let current_state: SessionState = serde_json::from_str(&current_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite checkpoint state: {e}")))?;
        let current_generation = u64::try_from(current_generation)
            .map_err(|_| SessionError::storage("SQLite history generation is negative"))?;
        if current_writer_epoch < 0 {
            return Err(SessionError::storage("SQLite writer epoch is negative"));
        }
        if current_state.sequence() != expected_revision
            || current_generation != expected_generation
        {
            return Err(SessionError::storage(format!(
                "history checkpoint identity conflict: expected revision {expected_revision}, generation {expected_generation}; actual revision {}, generation {current_generation}",
                current_state.sequence()
            )));
        }
        let changed = tx
            .execute(
                "UPDATE sessions SET state_json = ?1, history_generation = ?2, writer_epoch = writer_epoch + 1, checkpoint_published = 1, checkpoint_archive_json = ?3 WHERE id = ?4 AND state_json = ?5 AND history_generation = ?6 AND writer_epoch = ?7",
                params![
                    replacement_json,
                    next_generation_sql,
                    archive_json,
                    id,
                    current_json,
                    expected_generation_sql,
                    current_writer_epoch
                ],
            )
            .map_err(|e| SessionError::storage(format!("publish SQLite checkpoint: {e}")))?;
        if changed != 1 {
            return Err(SessionError::storage(
                "history checkpoint identity conflict during publish",
            ));
        }
        tx.commit()
            .map_err(|e| SessionError::storage(format!("commit SQLite checkpoint: {e}")))?;
        drop(db);
        self.inner.replace_state(checkpoint.state);
        self.writer_epoch.store(
            u64::try_from(current_writer_epoch).unwrap() + 1,
            Ordering::Release,
        );
        Ok(HistoryIdentity {
            revision: replacement_revision,
            generation: next_generation,
        })
    }

    async fn replace_history_if_revision_and_generation(
        &self,
        expected_revision: u64,
        expected_generation: u64,
        replacement: HistoryReplacement,
    ) -> SessionResult<HistoryIdentity> {
        let _gate = self.gate.clone().lock_owned().await;
        let replacement_revision = replacement.state.sequence();
        let replacement_json = serde_json::to_string(&replacement.state)
            .map_err(|e| SessionError::storage(format!("serialize SQLite replacement: {e}")))?;
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| SessionError::storage("history replacement generation exhausted"))?;
        let next_generation_sql = i64::try_from(next_generation)
            .map_err(|_| SessionError::storage("SQLite history generation exceeds INTEGER"))?;
        let expected_generation_sql = i64::try_from(expected_generation)
            .map_err(|_| SessionError::storage("SQLite history generation exceeds INTEGER"))?;
        let id = self.inner.metadata().id;
        let mut db = self
            .db
            .lock()
            .map_err(|_| SessionError::storage("SQLite connection poisoned"))?;
        let tx = db
            .transaction()
            .map_err(|e| SessionError::storage(format!("begin SQLite replacement: {e}")))?;
        let (current_json, current_generation, current_writer_epoch): (String, i64, i64) = tx
            .query_row(
                "SELECT state_json, history_generation, writer_epoch FROM sessions WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| SessionError::storage(format!("read SQLite replacement identity: {e}")))?;
        let current_state: SessionState = serde_json::from_str(&current_json)
            .map_err(|e| SessionError::storage(format!("decode SQLite replacement state: {e}")))?;
        let current_generation = u64::try_from(current_generation)
            .map_err(|_| SessionError::storage("SQLite history generation is negative"))?;
        if current_writer_epoch < 0 {
            return Err(SessionError::storage("SQLite writer epoch is negative"));
        }
        if current_state.sequence() != expected_revision
            || current_generation != expected_generation
        {
            return Err(SessionError::storage(format!(
                "history replacement identity conflict: expected revision {expected_revision}, generation {expected_generation}; actual revision {}, generation {current_generation}",
                current_state.sequence()
            )));
        }
        let changed = tx
            .execute(
                "UPDATE sessions SET state_json = ?1, history_generation = ?2, writer_epoch = writer_epoch + 1, checkpoint_published = 0, checkpoint_archive_json = NULL WHERE id = ?3 AND state_json = ?4 AND history_generation = ?5 AND writer_epoch = ?6",
                params![
                    replacement_json,
                    next_generation_sql,
                    id,
                    current_json,
                    expected_generation_sql,
                    current_writer_epoch
                ],
            )
            .map_err(|e| SessionError::storage(format!("publish SQLite replacement: {e}")))?;
        if changed != 1 {
            return Err(SessionError::storage(
                "history replacement identity conflict during publish",
            ));
        }
        tx.commit()
            .map_err(|e| SessionError::storage(format!("commit SQLite replacement: {e}")))?;
        drop(db);
        self.inner.replace_state(replacement.state);
        self.writer_epoch.store(
            u64::try_from(current_writer_epoch).unwrap() + 1,
            Ordering::Release,
        );
        Ok(HistoryIdentity {
            revision: replacement_revision,
            generation: next_generation,
        })
    }

    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        let lane = lane.to_owned();
        self.write_retry(move |inner| {
            let entry = entry.clone();
            let lane = lane.clone();
            async move { inner.append_entry_if_revision(entry, &lane, expected).await }
        })
        .await
    }

    async fn append_record_if_revision(
        &self,
        record: LaneRecord,
        expected: u64,
    ) -> SessionResult<LaneRecord> {
        self.write_retry(move |inner| {
            let record = record.clone();
            async move { inner.append_record_if_revision(record, expected).await }
        })
        .await
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        let lane = lane.to_string();
        self.write_retry(move |inner| {
            let entry = entry.clone();
            let lane = lane.clone();
            async move { inner.append_entry(entry, &lane).await }
        })
        .await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.write_retry(move |inner| {
            let record = record.clone();
            async move { inner.append_record(record).await }
        })
        .await
    }
    async fn message_summaries(
        &self,
        lane: &str,
        query: &crate::session::types::MessageSummaryQuery,
    ) -> SessionResult<crate::session::types::MessageSummaryPage> {
        let _gate = self.refresh_read().await?;
        self.inner.message_summaries(lane, query).await
    }
    async fn message_window(
        &self,
        lane: &str,
        query: &crate::session::types::MessageWindowQuery,
    ) -> SessionResult<crate::session::types::MessageWindowPage> {
        let _gate = self.refresh_read().await?;
        self.inner.message_window(lane, query).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        let _gate = self.refresh_read().await?;
        self.inner.get_entry(id).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        let _gate = self.refresh_read().await?;
        self.inner.find_entries(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        let _gate = self.refresh_read().await?;
        self.inner
            .find_entries_on_branch(query, bounds, start)
            .await
    }
    async fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        let _gate = self.refresh_read().await?;
        SessionStorage::context_on_branch(&self.inner, bounds, start).await
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        let _gate = self.refresh_read().await?;
        self.inner.find_records(query).await
    }
    async fn record_page(
        &self,
        query: &crate::session::types::RecordPageQuery,
    ) -> SessionResult<crate::session::types::RecordPage> {
        let _gate = self.refresh_read().await?;
        self.inner.record_page(query).await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        let _gate = self.refresh_read().await?;
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        let _gate = self.refresh_read().await?;
        self.inner.get_log(options).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        let _gate = self.refresh_read().await?;
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        let name = name.map(str::to_string);
        self.write_retry(move |inner| {
            let name = name.clone();
            async move { inner.set_name(name.as_deref()).await }
        })
        .await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        let _gate = self.refresh_read().await?;
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        let id = id.to_string();
        let label = label.map(str::to_string);
        self.write_retry(move |inner| {
            let id = id.clone();
            let label = label.clone();
            async move { inner.set_label(&id, label.as_deref()).await }
        })
        .await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        let _gate = self.refresh_read().await?;
        self.inner.get_stats().await
    }
}

pub struct SqliteSessionRepo {
    path: PathBuf,
    db: Arc<Mutex<Connection>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    gate: Arc<tokio::sync::Mutex<()>>,
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
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| SessionError::storage(format!("configure SQLite busy timeout: {e}")))?;
        // Reject native experimental artifacts BEFORE WAL/schema initialization.
        // Compiled unconditionally: feature-off legacy writers must reject too.
        let application_id: i64 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(|e| SessionError::storage(format!("read SQLite format identity: {e}")))?;
        if matches!(application_id, 0x5250494e | 0x5250494f | 0x52504951) {
            return Err(SessionError::storage(
                "experimental native artifact cannot be opened by the legacy SQLite writer",
            ));
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 parent_session_id TEXT,
                 state_json TEXT NOT NULL,
                 writer_epoch INTEGER NOT NULL DEFAULT 0,
                 history_generation INTEGER NOT NULL DEFAULT 0,
                 checkpoint_published INTEGER NOT NULL DEFAULT 0,
                 checkpoint_archive_json TEXT
             );
             CREATE TABLE IF NOT EXISTS progress_heads (
                 session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                 sequence INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS progress_frames (
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 sequence INTEGER NOT NULL, id TEXT NOT NULL, lane TEXT NOT NULL,
                 run_id TEXT NOT NULL, stream_index INTEGER NOT NULL, frame_json TEXT NOT NULL,
                 PRIMARY KEY(session_id, sequence), UNIQUE(session_id,id)
             );
             CREATE INDEX IF NOT EXISTS progress_run ON progress_frames(session_id,run_id,sequence);
             PRAGMA foreign_keys = ON;",
        )
        .map_err(|e| SessionError::storage(format!("initialize SQLite database: {e}")))?;
        let has_writer_epoch: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('sessions') WHERE name = 'writer_epoch'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                SessionError::storage(format!("inspect SQLite writer epoch schema: {e}"))
            })?;
        if has_writer_epoch.is_none() {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN writer_epoch INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| {
                SessionError::storage(format!("upgrade SQLite writer epoch schema: {e}"))
            })?;
        }
        let has_generation: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('sessions') WHERE name = 'history_generation'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SessionError::storage(format!("inspect SQLite generation schema: {e}")))?;
        if has_generation.is_none() {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN history_generation INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| SessionError::storage(format!("upgrade SQLite generation schema: {e}")))?;
        }
        let has_checkpoint: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('sessions') WHERE name = 'checkpoint_published'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SessionError::storage(format!("inspect SQLite checkpoint schema: {e}")))?;
        if has_checkpoint.is_none() {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN checkpoint_published INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| SessionError::storage(format!("upgrade SQLite checkpoint schema: {e}")))?;
        }
        let has_checkpoint_archive: Option<String> = conn
            .query_row(
                "SELECT name FROM pragma_table_info('sessions') WHERE name = 'checkpoint_archive_json'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| SessionError::storage(format!("inspect SQLite checkpoint archive schema: {e}")))?;
        if has_checkpoint_archive.is_none() {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN checkpoint_archive_json TEXT",
                [],
            )
            .map_err(|e| {
                SessionError::storage(format!("upgrade SQLite checkpoint archive schema: {e}"))
            })?;
        }
        Ok(Self {
            path,
            db: Arc::new(Mutex::new(conn)),
            clock,
            ids,
            gate: Arc::new(tokio::sync::Mutex::new(())),
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
            self.gate.clone(),
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
            self.gate.clone(),
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
            self.gate.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "experimental-native-store")]
    use crate::session::experimental_native::ExperimentalNativeRepo;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::{
        AssistantFrameOp, AssistantFrameRecord, EntryOrder, EntryQuery, ForkOptions,
        ProvisionedKind, RecordBase,
    };
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

    #[cfg(feature = "experimental-native-store")]
    #[tokio::test]
    async fn legacy_sqlite_repo_rejects_experimental_native_artifact_before_initialization() {
        let dir = tempfile::tempdir().unwrap();
        let native = ExperimentalNativeRepo::new(dir.path().to_owned())
            .await
            .unwrap();
        let storage = native
            .create(&SessionCreateOptions {
                id: Some("format-isolation".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let metadata = storage.metadata();
        drop(storage);

        let path = dir.path().join("format-isolation.rpinative");
        let before = std::fs::read(&path).unwrap();
        let error = match SqliteSessionRepo::open(&path) {
            Ok(_) => panic!("legacy SQLite must reject native artifacts"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("experimental native artifact cannot be opened"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            !std::path::PathBuf::from(format!("{}-wal", path.display())).exists(),
            "rejection must occur before legacy WAL initialization"
        );

        let reopened = native.open(&metadata).await.unwrap();
        assert_eq!(reopened.metadata(), metadata);
    }

    #[tokio::test]
    async fn context_projection_refreshes_and_matches_branch_query() {
        let (repo, _dir) = repo();
        let first = repo
            .create(&SessionCreateOptions {
                id: Some("context".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        first
            .append_entry(
                ProvisionedEntry {
                    id: "m1".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("one", 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let second = repo.open(&first.metadata()).await.unwrap();
        second
            .append_entry(
                ProvisionedEntry {
                    id: "m2".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("two", 2)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let actual = first
            .context_on_branch(&BranchBounds::default(), "m2")
            .await
            .unwrap();
        let path = first
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
                "m2",
            )
            .await
            .unwrap();
        let expected = crate::session::context::build_session_context(&path, &Default::default());
        assert_eq!(actual.messages, expected.messages);
        assert_eq!(actual.messages.len(), 2);
    }

    #[tokio::test]
    async fn replacement_cas_persists_generation_and_rejects_stale_handles() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("replacement".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        session
            .append_entry(
                ProvisionedEntry {
                    id: "before".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("before", 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(
            session
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT writer_epoch FROM sessions WHERE id = 'replacement'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let progress = session
            .append_progress(vec![AssistantFrameRecord {
                base: RecordBase {
                    id: "replacement-progress".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "replacement-run".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"text":"pending"})),
            }])
            .await
            .unwrap();
        assert_eq!(
            session
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT writer_epoch FROM sessions WHERE id = 'replacement'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let replacement_state = session.inner.snapshot_state();
        let stale_handle = repo.open(&session.metadata()).await.unwrap();

        let checkpoint = replacement_state
            .history_checkpoint(HistoryIdentity {
                revision: 1,
                generation: 0,
            })
            .unwrap()
            .with_receipt_archive(
                crate::durable::experimental_ledger::archive::ReceiptArchive::empty(),
            )
            .unwrap();
        let identity = session
            .replace_history_with_checkpoint_if_revision_and_generation(1, 0, checkpoint)
            .await
            .unwrap();
        assert_eq!(identity.revision, 1);
        assert_eq!(identity.generation, 1);
        assert_eq!(
            session
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT writer_epoch FROM sessions WHERE id = 'replacement'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );

        let reopened = repo.open(&session.metadata()).await.unwrap();
        assert_eq!(
            reopened.history_identity().await.unwrap(),
            HistoryIdentity {
                revision: 1,
                generation: 1
            }
        );
        assert_eq!(reopened.history_revision().await.unwrap(), 1);
        assert_eq!(reopened.history_generation().await.unwrap(), 1);
        assert_eq!(
            reopened.read_progress("replacement-run").await.unwrap(),
            progress
        );
        let persisted = reopened.history_checkpoint().await.unwrap().unwrap();
        assert_eq!(persisted.history_identity, identity);
        assert!(persisted.receipt_archive.is_some());
        assert!(reopened.get_entry("before").await.unwrap().is_some());
        let stale = stale_handle
            .replace_history_if_revision_and_generation(
                1,
                0,
                HistoryReplacement {
                    state: replacement_state,
                    canonical_mutations: None,
                },
            )
            .await
            .unwrap_err();
        assert!(stale.to_string().contains("identity conflict"));
        assert_eq!(reopened.history_generation().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn checkpoint_with_archive_round_trips_after_reopen() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("archive-roundtrip".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        let checkpoint = SessionState::new()
            .history_checkpoint(HistoryIdentity {
                revision: 0,
                generation: 0,
            })
            .unwrap()
            .with_receipt_archive(
                crate::durable::experimental_ledger::archive::ReceiptArchive::empty(),
            )
            .unwrap();
        let published = session
            .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
            .await
            .unwrap();
        let persisted = session.history_checkpoint().await.unwrap().unwrap();
        assert_eq!(persisted.history_identity, published);
        assert!(persisted.receipt_archive.is_some());

        let reopened = repo.open(&session.metadata()).await.unwrap();
        let persisted = reopened.history_checkpoint().await.unwrap().unwrap();
        assert_eq!(persisted.history_identity, published);
        assert!(persisted.receipt_archive.is_some());
    }

    #[tokio::test]
    async fn old_sqlite_schema_migrates_checkpoint_archive_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old-schema.db");
        let connection = Connection::open(&path).unwrap();
        let state = serde_json::to_string(&SessionState::new()).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, parent_session_id TEXT, state_json TEXT NOT NULL, history_generation INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions (id, created_at, parent_session_id, state_json) VALUES (?1, ?2, NULL, ?3)",
                rusqlite::params!["legacy-schema", 0i64, state],
            )
            .unwrap();
        drop(connection);

        let repo = SqliteSessionRepo::with_clock_ids(
            &path,
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .unwrap();
        let session = repo
            .open(&SessionMetadata {
                id: "legacy-schema".into(),
                created_at: 0,
                parent_session_id: None,
            })
            .await
            .unwrap();
        assert!(session.history_checkpoint().await.unwrap().is_none());
        let checkpoint = SessionState::new()
            .history_checkpoint(HistoryIdentity {
                revision: 0,
                generation: 0,
            })
            .unwrap()
            .with_receipt_archive(
                crate::durable::experimental_ledger::archive::ReceiptArchive::empty(),
            )
            .unwrap();
        session
            .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
            .await
            .unwrap();
        assert!(session.history_checkpoint().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn checkpoint_archive_corruption_fails_closed_without_rewriting_state() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("archive-corruption".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        let checkpoint = SessionState::new()
            .history_checkpoint(HistoryIdentity {
                revision: 0,
                generation: 0,
            })
            .unwrap()
            .with_receipt_archive(
                crate::durable::experimental_ledger::archive::ReceiptArchive::empty(),
            )
            .unwrap();
        let published = session
            .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
            .await
            .unwrap();
        let id = session.metadata().id;
        repo.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET checkpoint_archive_json = ?1 WHERE id = ?2",
                rusqlite::params!["{}", id],
            )
            .unwrap();
        assert!(session.history_checkpoint().await.is_err());
        assert_eq!(session.history_identity().await.unwrap(), published);
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

    #[derive(Default)]
    struct ReviewGatedClock {
        armed: std::sync::atomic::AtomicBool,
        reached: std::sync::Mutex<usize>,
        changed: std::sync::Condvar,
    }
    impl Clock for ReviewGatedClock {
        fn now_ms(&self) -> i64 {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                let mut reached = self.reached.lock().unwrap();
                *reached += 1;
                self.changed.notify_all();
                if *reached < 2 {
                    // A serialized implementation may hold its transaction while
                    // stamping, so bound this gate instead of deadlocking it.
                    let _ = self
                        .changed
                        .wait_timeout_while(reached, std::time::Duration::from_secs(2), |count| {
                            *count < 2
                        })
                        .unwrap();
                }
            }
            1
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn review_sqlite_two_handles_must_preserve_both_successful_appends() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ReviewGatedClock::default());
        let repo = SqliteSessionRepo::with_clock_ids(
            dir.path().join("review.sqlite"),
            clock.clone(),
            Arc::new(CounterIdGenerator::new()),
        )
        .unwrap();
        let first = repo
            .create(&SessionCreateOptions {
                id: Some("review".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        let second = repo.open(&first.metadata()).await.unwrap();
        let metadata = first.metadata();
        clock.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let append = |storage: SqliteSessionStorage, id: &'static str| {
            tokio::spawn(async move {
                storage
                    .append_entry(
                        ProvisionedEntry {
                            id: id.into(),
                            kind: ProvisionedKind::Message {
                                message: AgentMessage::from(rpi_ai::types::UserMessage::new(id, 0)),
                                terminate: None,
                            },
                        },
                        "main",
                    )
                    .await
                    .unwrap()
            })
        };
        let left = append(first, "writer-one");
        let right = append(second, "writer-two");
        let (a, b) = tokio::join!(left, right);
        let (a, b) = (a.unwrap(), b.unwrap());
        clock
            .armed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let reloaded = repo.open(&metadata).await.unwrap();
        let all = reloaded.find_entries(&EntryQuery::default()).await.unwrap();
        eprintln!(
            "review SQLite concurrent handles: successful_seqs=({}, {}), reopened_entry_ids={:?}",
            a.seq(),
            b.seq(),
            all.iter().map(Entry::id).collect::<Vec<_>>()
        );
        assert_eq!(
            all.len(),
            2,
            "both appends returned success, but one whole-state snapshot overwrote the other"
        );
    }

    #[tokio::test]
    async fn review_sqlite_failed_persist_rolls_back_memory_and_disk() {
        let (repo, _dir) = repo();
        let session = repo
            .create(&SessionCreateOptions {
                id: Some("rollback".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        session
            .db
            .lock()
            .unwrap()
            .execute_batch("CREATE TRIGGER reject_session_update BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'review rejection'); END;")
            .unwrap();
        let result = session
            .append_entry(
                ProvisionedEntry {
                    id: "rejected".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::from(rpi_ai::types::UserMessage::new("rejected", 1)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await;
        assert!(result.is_err());
        assert!(session.get_entry("rejected").await.unwrap().is_none());
        let reopened = repo.open(&session.metadata()).await.unwrap();
        assert!(reopened.get_entry("rejected").await.unwrap().is_none());
    }
}
