//! Mirrors `packages/agent/src/harness/session/memory.ts` — the in-memory
//! `SessionStorage` + `SessionRepo` back-ends used as the primary test backend
//! and as the reference for the JSONL backend.
//!
//! ## Adaptation notes
//!
//! - **Sharing**: the [`SessionStorage`] trait is `&self`, so a storage handed
//!   out by the repo must be shareable. [`InMemorySessionStorage`] wraps its
//!   mutable state in `Arc<Inner>` and is `Clone` (a cheap Arc bump), so the
//!   repo stores values directly and returns clones — the Rust analog of TS
//!   handing the same `Session` instance to multiple callers.
//! - **Stamping**: TS `appendEntry`/`appendRecord` take provisioned/new shapes
//!   and stamp `seq`+`timestamp` inline before calling `applyMutation`. The
//!   Rust [`SessionStorage`] trait takes a fully-stamped [`Entry`]/[`LaneRecord`];
//!   this backend stamps them itself via the state's `next_sequence` + a
//!   [`Clock`], then delegates to [`SessionState::apply_mutation`]. The state's
//!   consecutive-seq invariant stays authoritative across backends.
//! - **At-most-one open op**: TS `InMemorySessionStorage.appendRecord` rejects a
//!   2nd `operation_started` on a lane that already has an open op (a
//!   storage-level precondition distinct from the reducer-level corruption
//!   check). We mirror that here via [`SessionState::find_open_operations`]
//!   `limit:1`. The reducer still flags two-open as corruption for *recovery*.
//! - **Defensive copies**: reads clone (TS `structuredClone`); the state stores
//!   owned `Entry`/`LaneRecord` so `clone()` is the defensive copy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{SessionError, SessionResult};
use crate::session::state::SessionState;
use crate::session::types::{
    provisioned_into_entry, BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer,
    LaneRecord, LogItem, LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery,
    SessionCreateOptions, SessionMetadata, SessionMutation, SessionRepo, SessionStats,
    SessionStorage,
};

/// A simple shared id-generator producing stringified u64 counters — the
/// in-memory tests' deterministic generator. Production paths use uuidv7 via
/// the JSONL repo. Exposed so integration tests across the harness can share
/// one generator implementation.
#[derive(Debug, Default)]
pub struct CounterIdGenerator {
    n: Mutex<u64>,
}

impl CounterIdGenerator {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGenerator for CounterIdGenerator {
    fn next(&self) -> String {
        let mut x = self.n.lock().expect("counter id generator not poisoned");
        *x += 1;
        format!("id-{x}")
    }
}

/// Wall-clock timestamp source — abstracted so tests can inject a fixed clock.
/// Production uses [`SystemClock`]; tests use [`FakeClock`] for deterministic
/// `timestamp` fields.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Real system clock — `SystemTime::now().duration_since(UNIX_EPOCH)` as ms.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for tests — returns an incrementing ms counter.
#[derive(Debug, Default)]
pub struct FakeClock {
    ms: Mutex<i64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self { ms: Mutex::new(0) }
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        let mut m = self.ms.lock().expect("fake clock not poisoned");
        *m += 1;
        *m
    }
}

/// Mutable inner state shared across clones of an [`InMemorySessionStorage`].
struct Inner {
    state: Mutex<SessionState>,
    clock: Arc<dyn Clock>,
    /// Reserved for future auto-id generation (callers currently supply entry/
    /// record ids). Kept so the constructor signature matches the JSONL repo's
    /// uuidv7-backed storage and stays drop-in replaceable.
    #[allow(dead_code)]
    ids: Arc<dyn IdGenerator>,
}

impl Inner {
    fn with_state<R>(
        &self,
        f: impl FnOnce(&mut SessionState) -> SessionResult<R>,
    ) -> SessionResult<R> {
        let mut state = self.state.lock().expect("state not poisoned");
        f(&mut state)
    }

    fn read_state<R>(&self, f: impl FnOnce(&SessionState) -> R) -> R {
        let state = self.state.lock().expect("state not poisoned");
        f(&state)
    }
}

/// In-memory `SessionStorage`. Owns the metadata + shared [`Inner`] and is
/// `Clone` (Arc bump), so the repo hands out clones and multiple holders share
/// the same durable state. Mirrors TS `InMemorySessionStorage`.
#[derive(Clone)]
pub struct InMemorySessionStorage {
    metadata: SessionMetadata,
    inner: Arc<Inner>,
}

impl InMemorySessionStorage {
    /// Build with a given metadata + clock + id generator. Production paths
    /// use [`SystemClock`] + uuidv7; tests inject deterministic doubles.
    pub fn new(
        metadata: SessionMetadata,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            metadata,
            inner: Arc::new(Inner {
                state: Mutex::new(SessionState::new()),
                clock,
                ids,
            }),
        }
    }

    /// Fork from a source storage — mirrors TS `fork`. Applies the source
    /// state's [`SessionState::create_fork_mutations`] into a fresh backend.
    pub fn fork_from(
        &self,
        metadata: SessionMetadata,
        options: &ForkOptions,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let forked = InMemorySessionStorage::new(metadata, clock, ids);
        let mutations = self
            .inner
            .read_state(|state| state.create_fork_mutations(options))?;
        forked.inner.with_state(|state| {
            for mutation in mutations {
                let _ = state.apply_mutation(mutation)?;
            }
            Ok(())
        })?;
        Ok(forked)
    }
}

#[async_trait]
impl SessionStorage for InMemorySessionStorage {
    fn metadata(&self) -> SessionMetadata {
        self.metadata.clone()
    }

    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        Ok(self.inner.read_state(|state| state.get_lanes()))
    }

    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.validate_new_lane(lane)?;
            state.validate_target(at)?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::Lane {
                seq,
                lane: lane.to_string(),
                leaf_id: at.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.require_lane(lane)?;
            state.validate_target(to)?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::Lane {
                seq,
                lane: lane.to_string(),
                leaf_id: to.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.inner.with_state(|state| {
            let parent_id = state.require_lane(lane)?;
            state.validate_unused_id(&entry.id)?;
            let seq = state.next_sequence();
            let timestamp = self.inner.clock.now_ms();
            let full = provisioned_into_entry(entry, seq, parent_id.clone(), timestamp);
            state.apply_mutation(SessionMutation::Entry {
                seq,
                timestamp,
                lane: Some(lane.to_string()),
                entry: full.clone(),
            })?;
            match state.get_entry(full.id()) {
                Some(stamped) => Ok(stamped.clone()),
                None => Err(SessionError::storage("append_entry did not persist")),
            }
        })
    }

    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let lane = record.lane().to_string();
        let is_operation_started = matches!(record, LaneRecord::OperationStarted(_));
        self.inner.with_state(|state| {
            state.require_lane(&lane)?;
            state.validate_unused_id(record.id())?;
            // At-most-one open op per lane (storage-level precondition, mirrors
            // TS `InMemorySessionStorage.appendRecord`). The reducer still
            // flags two-open as corruption for recovery slices.
            if is_operation_started {
                let open = state.find_open_operations(&lane, Some(1))?;
                if let Some(current) = open.first() {
                    return Err(SessionError::storage(format!(
                        "Lane {} already has an open operation {}",
                        lane, current.base.id
                    )));
                }
            }
            let seq = state.next_sequence();
            let timestamp = self.inner.clock.now_ms();
            let stamped = stamp_record(record, seq, lane.clone(), timestamp);
            state.apply_mutation(SessionMutation::Record {
                record: stamped.clone(),
            })?;
            Ok(stamped)
        })
    }

    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        Ok(self.inner.read_state(|state| state.get_entry(id).cloned()))
    }

    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.read_state(|state| state.find_entries(query))
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.inner
            .read_state(|state| state.find_entries_on_branch(query, bounds, start))
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.inner.read_state(|state| state.find_records(query))
    }

    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner
            .read_state(|state| state.find_open_operations(lane, limit))
    }

    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.read_state(|state| state.get_log(options))
    }

    async fn get_name(&self) -> SessionResult<Option<String>> {
        Ok(self
            .inner
            .read_state(|state| state.get_name().map(|s| s.to_string())))
    }

    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::FactName {
                seq,
                name: name.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        Ok(self
            .inner
            .read_state(|state| state.get_label(id).map(|s| s.to_string())))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.inner.with_state(|state| {
            state.validate_target(Some(id))?;
            let seq = state.next_sequence();
            state.apply_mutation(SessionMutation::FactLabel {
                seq,
                target_id: id.to_string(),
                label: label.map(|s| s.to_string()),
            })?;
            Ok(())
        })
    }

    async fn get_stats(&self) -> SessionResult<SessionStats> {
        Ok(self.inner.read_state(|state| state.get_stats().clone()))
    }
}

/// Stamp a record's `seq`/`lane`/`timestamp` fields. The caller-built record may
/// carry only the intent-level fields; this rebuilds it with the storage-assigned
/// [`RecordBase`] fields set. Mirrors TS `{ ...newRecord, seq, timestamp }`.
///
/// Shared by both in-memory and JSONL storage backends (each stamps `seq` +
/// `timestamp` from its own clock + state before persisting), hence `pub(crate)`.
pub(crate) fn stamp_record(
    record: LaneRecord,
    seq: u64,
    lane: String,
    timestamp: i64,
) -> LaneRecord {
    use crate::session::types::*;
    let stamp = |mut base: RecordBase| {
        base.seq = seq;
        base.lane = lane.clone();
        base.timestamp = timestamp;
        base
    };
    match record {
        LaneRecord::OperationStarted(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::OperationStarted(r)
        }
        LaneRecord::AbortRequested(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::AbortRequested(r)
        }
        LaneRecord::OperationFinished(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::OperationFinished(r)
        }
        LaneRecord::StepAttempt(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::StepAttempt(r)
        }
        LaneRecord::ToolStarted(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::ToolStarted(r)
        }
        LaneRecord::QueueEnqueued(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::QueueEnqueued(r)
        }
        LaneRecord::QueueCancelled(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::QueueCancelled(r)
        }
        LaneRecord::WriteDeferred(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::WriteDeferred(r)
        }
        LaneRecord::Usage(mut r) => {
            r.base = stamp(r.base);
            LaneRecord::Usage(r)
        }
    }
}

/// In-memory `SessionRepo`. Owns all sessions in a `HashMap` keyed by id and
/// hands out `Clone`d [`InMemorySessionStorage`] handles (Arc bumps). Mirrors
/// TS `InMemorySessionRepo`.
pub struct InMemorySessionRepo {
    sessions: Mutex<HashMap<String, InMemorySessionStorage>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
}

impl InMemorySessionRepo {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdGenerator>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            clock,
            ids,
        }
    }

    fn require_storage(&self, id: &str) -> SessionResult<InMemorySessionStorage> {
        let sessions = self.sessions.lock().expect("sessions not poisoned");
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::not_found(format!("Session not found: {id}")))
    }
}

#[async_trait]
impl SessionRepo for InMemorySessionRepo {
    type Storage = InMemorySessionStorage;

    async fn create(&self, options: &SessionCreateOptions) -> SessionResult<Self::Storage> {
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        if sessions.contains_key(&id) {
            return Err(SessionError::already_exists(format!(
                "Session already exists: {id}"
            )));
        }
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: self.clock.now_ms(),
            parent_session_id: options.parent_session_id.clone(),
        };
        let storage = InMemorySessionStorage::new(metadata, self.clock.clone(), self.ids.clone());
        sessions.insert(id, storage.clone());
        Ok(storage)
    }

    async fn open(&self, metadata: &SessionMetadata) -> SessionResult<Self::Storage> {
        self.require_storage(&metadata.id)
    }

    async fn list(&self) -> SessionResult<Vec<SessionMetadata>> {
        let sessions = self.sessions.lock().expect("sessions not poisoned");
        Ok(sessions.values().map(|s| s.metadata()).collect())
    }

    async fn delete(&self, metadata: &SessionMetadata) -> SessionResult<()> {
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        sessions.remove(&metadata.id);
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        options: &SessionCreateOptions,
        fork: &ForkOptions,
    ) -> SessionResult<Self::Storage> {
        let source_storage = self.require_storage(&source.id)?;
        let id = options.id.clone().unwrap_or_else(|| self.ids.next());
        let mut sessions = self.sessions.lock().expect("sessions not poisoned");
        if sessions.contains_key(&id) {
            return Err(SessionError::already_exists(format!(
                "Session already exists: {id}"
            )));
        }
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: self.clock.now_ms(),
            parent_session_id: options
                .parent_session_id
                .clone()
                .or_else(|| Some(source.id.clone())),
        };
        let forked =
            source_storage.fork_from(metadata, fork, self.clock.clone(), self.ids.clone())?;
        sessions.insert(id, forked.clone());
        Ok(forked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{
        OperationIntent, OperationStartedRecord, ProvisionedKind, RecordBase,
    };

    fn counter_ids() -> Arc<dyn IdGenerator> {
        Arc::new(CounterIdGenerator::new())
    }

    fn fresh_storage() -> InMemorySessionStorage {
        InMemorySessionStorage::new(
            SessionMetadata {
                id: "s1".to_string(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            counter_ids(),
        )
    }

    fn rec_base(id: &str, seq: u64, lane: &str) -> RecordBase {
        RecordBase {
            id: id.to_string(),
            seq,
            lane: lane.to_string(),
            timestamp: seq as i64,
        }
    }

    fn user_msg(text: &str) -> rpi_agent::message::AgentMessage {
        rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    #[tokio::test]
    async fn in_memory_roundtrip_entry_and_record() {
        let storage = fresh_storage();
        let entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".to_string(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hello"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .expect("append entry");
        assert_eq!(entry.id(), "e1");
        assert_eq!(entry.seq(), 1);
        assert_eq!(entry.parent_id(), None);

        let got = storage
            .get_entry("e1")
            .await
            .unwrap()
            .expect("entry exists");
        assert_eq!(got.id(), "e1");
    }

    #[tokio::test]
    async fn in_memory_rejects_second_open_operation() {
        let storage = fresh_storage();
        let started = LaneRecord::OperationStarted(OperationStartedRecord {
            base: rec_base("op-1", 0, "main"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        storage.append_record(started).await.expect("first op");

        let second = LaneRecord::OperationStarted(OperationStartedRecord {
            base: rec_base("op-2", 0, "main"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        let err = storage.append_record(second).await.unwrap_err();
        assert!(err.to_string().contains("already has an open operation"));
    }

    #[tokio::test]
    async fn in_memory_fork_branch_copies_entries() {
        let storage = fresh_storage();
        storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".to_string(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hi"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();

        let forked = storage
            .fork_from(
                SessionMetadata {
                    id: "s2".to_string(),
                    created_at: 0,
                    parent_session_id: Some("s1".to_string()),
                },
                &ForkOptions::default(),
                Arc::new(FakeClock::new()),
                counter_ids(),
            )
            .expect("fork");
        let entries = forked.find_entries(&Default::default()).await.unwrap();
        assert!(entries.iter().any(|e| e.id() == "e1"));
    }
}
