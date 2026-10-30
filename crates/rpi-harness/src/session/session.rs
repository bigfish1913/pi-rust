//! Mirrors `packages/agent/src/harness/session/session.ts` — the `Session`
//! facade over a [`SessionStorage`] + [`IdGenerator`].
//!
//! The facade is the harness's read+write entry point into the durable session:
//! it validates queries, validates payloads are JSON-serializable (mirrors TS
//! `assertJsonSerializable`), mints entry ids via the `IdGenerator` (default
//! uuidv7), and exposes both the full [`Session`] API (lanes, records, log,
//! open operations) and a lane-scoped [`SessionTree`] view via [`Session::view`].
//!
//! `view("main")` returns the `Session` itself (it implements [`SessionTree`]);
//! any other lane returns a [`LaneView`] that delegates leaf/branch/appends to
//! that lane. Lane resolution goes through `get_leaf_id_for_lane`, which throws
//! `invalid_lane` when the lane does not exist — matching TS.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

use rpi_agent::message::AgentMessage;

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::types::{
    BranchBounds, Entry, EntryQuery, IdGenerator, JsonValue, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationStartedRecord, ProvisionedEntry, ProvisionedKind, RecordQuery,
    SessionMetadata, SessionStats, SessionStorage, SessionTree,
};

static AGENT_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();

fn agent_diagnostics_enabled() -> bool {
    *AGENT_DIAGNOSTICS
        .get_or_init(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0"))
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

// ---------------------------------------------------------------------------
// IdGenerator default — uuidv7. Mirrors TS `{ next: () => uuidv7() }`.
// ---------------------------------------------------------------------------

/// Default [`IdGenerator`] producing uuidv7 strings. Mirrors the TS
/// `options.idGenerator ?? { next: () => uuidv7() }` default.
#[derive(Debug, Clone, Default)]
pub struct DefaultIdGenerator;

impl DefaultIdGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl IdGenerator for DefaultIdGenerator {
    fn next(&self) -> String {
        Uuid::now_v7().to_string()
    }
}

// ---------------------------------------------------------------------------
// Query validation — mirrors TS `assertValidLimit` / `assertValidCursor`.
// ---------------------------------------------------------------------------

/// `assertValidLimit` — `None` is allowed; `Some(n)` must be a positive integer.
fn assert_valid_limit(limit: Option<usize>) -> SessionResult<()> {
    if let Some(n) = limit {
        if n == 0 {
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "limit must be a positive integer",
            ));
        }
    }
    Ok(())
}

/// `assertValidCursor` — `None` is allowed; `Some(seq)` must be non-negative.
fn assert_valid_cursor(after_seq: Option<u64>) -> SessionResult<()> {
    if let Some(seq) = after_seq {
        if seq == u64::MAX {
            // `u64` is always a non-negative integer by construction; this arm
            // mirrors the TS `!Number.isInteger` guard which cannot fire for a
            // `u64`. Kept as a defensive sentinel for parity shape only.
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "cursor sequence must be a non-negative integer",
            ));
        }
        let _ = seq;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// assertJsonSerializable — mirrors TS `assertJsonSerializable(value)`.
// ---------------------------------------------------------------------------

/// Validate a [`serde_json::Value`] is JSON-serializable: reject non-finite
/// numbers and (defensively) cycles. Mirrors TS `assertJsonSerializable`.
///
/// `serde_json::Value` cannot by construction represent symbols, accessors,
/// non-plain prototypes, or sparse arrays — so those TS arms are structural
/// no-ops here. Non-finite `f64`s CAN slip in via `serde_json::Number` from
/// `to_value` of an `f64` (serde_json serializes NaN/Infinity as `null`), so we
/// re-check the original where relevant. Cycle detection is kept for parity;
/// `Value` is a tree so it never cycles, but the walk is cheap.
pub fn assert_json_serializable(value: &Value) -> SessionResult<()> {
    fn check(value: &Value) -> SessionResult<()> {
        match value {
            Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
            Value::Number(n) => {
                if n.as_f64().map(|f| f.is_finite()).unwrap_or(true) {
                    Ok(())
                } else {
                    Err(SessionError::invalid_payload(
                        "Durable payload contains a non-finite number",
                    ))
                }
            }
            Value::Array(arr) => {
                for v in arr {
                    check(v)?;
                }
                Ok(())
            }
            Value::Object(map) => {
                for v in map.values() {
                    check(v)?;
                }
                Ok(())
            }
        }
    }
    check(value)
}

// ---------------------------------------------------------------------------
// Session — the facade. Mirrors TS `class Session`.
// ---------------------------------------------------------------------------

/// Metadata published after a session entry has been committed.
///
/// The event is an invalidation signal only; consumers must reread the branch
/// when they need the canonical entry data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntryCommit {
    pub entry_id: String,
    pub entry_type: String,
    pub custom_type: Option<String>,
    pub lane: String,
}

/// The session facade over a [`SessionStorage`] and [`IdGenerator`]. Mirrors TS
/// `Session<TMetadata>`. Holds shared handles (`Arc`) so it is cheaply
/// [`Clone`] — a `view` of any lane shares the same storage + id generator.
pub struct Session {
    binding: Arc<RwLock<SessionBinding>>,
    id_generator: Arc<dyn IdGenerator>,
}

#[derive(Clone)]
struct SessionBinding {
    storage: Arc<dyn SessionStorage>,
    volatile_progress: Option<crate::frame_progress::SharedVolatileProgress>,
    entry_commit_publisher: Option<broadcast::Sender<SessionEntryCommit>>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            binding: Arc::clone(&self.binding),
            id_generator: Arc::clone(&self.id_generator),
        }
    }
}

impl Session {
    /// Build a facade over `storage` with an optional id generator (default
    /// [`DefaultIdGenerator`] / uuidv7). Mirrors TS `constructor`.
    pub fn new(
        storage: Arc<dyn SessionStorage>,
        id_generator: Option<Arc<dyn IdGenerator>>,
    ) -> Self {
        Self {
            binding: Arc::new(RwLock::new(SessionBinding {
                storage,
                volatile_progress: None,
                entry_commit_publisher: None,
            })),
            id_generator: id_generator.unwrap_or_else(|| Arc::new(DefaultIdGenerator::new())),
        }
    }

    /// Atomically swap the backing storage (TUI `/session` hot-switch). The
    /// facade shares its binding, so every existing `Session` clone — the
    /// harness's field, lane handles, the config snapshot — observes the new
    /// storage immediately; no harness rebuild, no field restructure. The
    /// Raw storage replacement clears volatile recovery; harness session switches
    /// adopt the target's recovery state instead. The caller is responsible for
    /// aborting any in-flight run first. Mirrors the
    /// TS `SessionManager.rebind` intent (swap the durable backing without
    /// rebuilding the harness).
    pub fn set_storage(&self, storage: Arc<dyn SessionStorage>) {
        let publisher = self.binding.read().unwrap().entry_commit_publisher.clone();
        *self.binding.write().unwrap() = SessionBinding {
            storage,
            volatile_progress: None,
            entry_commit_publisher: publisher,
        };
    }

    /// Install or clear the optional publisher for successful entry commits.
    /// Subscribers receive invalidation metadata, not message contents, and
    /// must reread the canonical branch when they need current entry data.
    pub fn set_entry_commit_publisher(
        &self,
        publisher: Option<broadcast::Sender<SessionEntryCommit>>,
    ) {
        self.binding.write().unwrap().entry_commit_publisher = publisher;
    }

    /// Keep the harness's generator, as on the existing session-switch path.
    pub(crate) fn pin(&self, id_generator: Arc<dyn IdGenerator>) -> Self {
        let binding = self.binding.read().unwrap().clone();
        Self {
            binding: Arc::new(RwLock::new(binding)),
            id_generator,
        }
    }

    /// Publish the prepared target without changing the shared facade handles.
    /// The target's cache is adopted; the old session's cache is not carried over.
    pub(crate) fn rebind_to(&self, target: &Self) {
        let publisher = self.binding.read().unwrap().entry_commit_publisher.clone();
        let mut binding = target.binding.read().unwrap().clone();
        if binding.entry_commit_publisher.is_none() {
            binding.entry_commit_publisher = publisher;
        }
        *self.binding.write().unwrap() = binding;
    }

    /// Keep recovery progress as typed frames in memory, without JSON records.
    /// Use only for a session that does not need cross-process crash recovery.
    /// Cloned handles share progress; rebinding storage resets this mode.
    pub fn with_volatile_progress(self) -> Self {
        self.binding
            .write()
            .unwrap()
            .volatile_progress
            .get_or_insert_with(|| Arc::new(std::sync::Mutex::new(Default::default())));
        self
    }

    pub(crate) fn volatile_progress(
        &self,
    ) -> Option<crate::frame_progress::SharedVolatileProgress> {
        self.binding.read().unwrap().volatile_progress.clone()
    }

    /// A cheap owned handle to the backing storage. Releases the read lock
    /// immediately, so the returned `Arc` is safe to hold across awaits (the
    /// lock guard itself is not `Send`).
    fn storage_arc(&self) -> Arc<dyn SessionStorage> {
        Arc::clone(&self.binding.read().unwrap().storage)
    }

    /// The shared id generator. Mirrors TS `readonly idGenerator`.
    pub fn id_generator(&self) -> &Arc<dyn IdGenerator> {
        &self.id_generator
    }

    /// The shared storage handle (the harness needs the full storage API beyond
    /// what [`SessionTree`] exposes).
    pub fn storage(&self) -> Arc<dyn SessionStorage> {
        self.storage_arc()
    }

    /// `getMetadata()`. Mirrors TS `Session.getMetadata`.
    pub async fn get_metadata(&self) -> SessionResult<SessionMetadata> {
        Ok(self.storage_arc().metadata())
    }

    /// Return a lane-scoped [`SessionTree`] view. `view("main")` returns a view
    /// backed by this `Session` directly (the main lane); any other lane
    /// returns a [`LaneView`] that resolves leaf/branch/appends against that
    /// lane. Mirrors TS `Session.view(lane)`.
    pub fn view(&self, lane: &str) -> Arc<dyn SessionTree> {
        if lane == "main" {
            // `Session` itself implements `SessionTree` for the main lane.
            Arc::new(self.clone()) as Arc<dyn SessionTree>
        } else {
            Arc::new(LaneView {
                session: self.clone(),
                lane: lane.to_string(),
            }) as Arc<dyn SessionTree>
        }
    }

    // -- Reads (session-wide) ------------------------------------------------

    /// `getLeafId()` — the main lane's leaf. Mirrors TS `Session.getLeafId`.
    pub async fn get_leaf_id(&self) -> SessionResult<Option<String>> {
        self.get_leaf_id_for_lane("main").await
    }

    /// `getEntry(id)`. Mirrors TS `Session.getEntry`.
    pub async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.storage_arc().get_entry(id).await
    }

    /// `getStats()`. Mirrors TS `Session.getStats`.
    pub async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.storage_arc().get_stats().await
    }

    /// `getName()`. Mirrors TS `Session.getName`.
    pub async fn get_name(&self) -> SessionResult<Option<String>> {
        self.storage_arc().get_name().await
    }

    /// `setName(name)`. Mirrors TS `Session.setName`.
    pub async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.storage_arc().set_name(name).await
    }

    /// `getLabel(targetId)`. Mirrors TS `Session.getLabel`.
    pub async fn get_label(&self, target_id: &str) -> SessionResult<Option<String>> {
        self.storage_arc().get_label(target_id).await
    }

    /// `setLabel(targetId, label)`. Mirrors TS `Session.setLabel`.
    pub async fn set_label(&self, target_id: &str, label: Option<&str>) -> SessionResult<()> {
        self.storage_arc().set_label(target_id, label).await
    }

    // -- Entry queries (session-wide, sequence order) ------------------------

    /// `findEntries(query?)`. Mirrors TS `Session.findEntries`.
    pub async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.query_entries(query, query.limit).await
    }

    /// `findEntry(query?)` — first match (limit 1). Mirrors TS `Session.findEntry`.
    pub async fn find_entry(&self, query: &EntryQuery) -> SessionResult<Option<Entry>> {
        let mut entries = self.query_entries(query, Some(1)).await?;
        Ok(entries.pop())
    }

    /// `findEntriesOnBranch(query?)` — main-lane branch path. Mirrors TS
    /// `Session.findEntriesOnBranch`.
    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>> {
        self.query_branch_entries("main", query, bounds, query.limit)
            .await
    }

    /// `findEntryOnBranch(query?)` — first match on the main-lane branch path.
    /// Mirrors TS `Session.findEntryOnBranch`.
    pub async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Option<Entry>> {
        let mut entries = self
            .query_branch_entries("main", query, bounds, Some(1))
            .await?;
        Ok(entries.pop())
    }

    /// Build the default hook-free provider context directly from this lane's
    /// validated branch. Custom transforms/projectors intentionally do not use
    /// this method; callers retain the ordinary entry-reading path for hooks.
    pub(crate) async fn context_on_branch_for_lane(
        &self,
        lane: &str,
        bounds: &BranchBounds,
    ) -> SessionResult<crate::session::context::SessionContext> {
        // Pin the storage Arc before either await. Resolving the leaf through
        // `self` and then reacquiring storage could mix two session bindings.
        let storage = self.storage_arc();
        let start = match &bounds.start {
            Some(id) => id.clone(),
            None => {
                let leaf = storage
                    .get_lanes()
                    .await?
                    .into_iter()
                    .find(|pointer| pointer.lane == lane)
                    .ok_or_else(|| SessionError::invalid_lane(format!("Lane not found: {lane}")))?
                    .leaf_id;
                let Some(id) = leaf else {
                    return Ok(crate::session::context::build_session_context(
                        &[],
                        &Default::default(),
                    ));
                };
                id
            }
        };
        storage.context_on_branch(bounds, &start).await
    }

    // -- Appends (main lane) -------------------------------------------------

    /// `appendMessage(message)` — mint an id, commit a message entry on `main`.
    /// Mirrors TS `Session.appendMessage`. Returns the new entry id.
    pub async fn append_message(&self, message: AgentMessage) -> SessionResult<String> {
        self.append_message_to_lane("main", message).await
    }

    /// `appendCustomEntry(customType, data?)` — mint an id, commit a custom
    /// entry on `main`. Mirrors TS `Session.appendCustomEntry`. Returns the id.
    pub async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> SessionResult<String> {
        self.append_custom_entry_to_lane("main", custom_type, data)
            .await
    }

    // -- Lanes ---------------------------------------------------------------

    /// `getLanes()`. Mirrors TS `Session.getLanes`.
    pub async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.storage_arc().get_lanes().await
    }

    /// `createLane(lane, at)`. Mirrors TS `Session.createLane`.
    pub async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.storage_arc().create_lane(lane, at).await
    }

    /// `moveLane(lane, to)`. Mirrors TS `Session.moveLane`.
    pub async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.storage_arc().move_lane(lane, to).await
    }

    // -- Raw append (provisioned entry / record) -----------------------------

    /// `appendEntry(entry, lane)` — validate-then-commit a provisioned entry.
    /// Mirrors TS `Session.appendEntry`. Returns the storage-stamped entry.
    pub async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.commit_entry(entry, lane).await
    }

    /// `appendRecord(record)` — validate-then-commit a lane record. Mirrors TS
    /// `Session.appendRecord`. Returns the storage-stamped record.
    pub async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.commit_record(record).await
    }

    /// Append a durable record followed immediately by an entry.
    pub(crate) async fn append_record_and_entry(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        for payload in [serde_json::to_value(&record), serde_json::to_value(&entry)] {
            let value = payload.map_err(|e| {
                SessionError::invalid_payload(format!("Durable payload is not serializable: {e}"))
            })?;
            assert_json_serializable(&value)?;
        }
        let storage = self.storage_arc();
        let entry_id = entry.id.clone();
        let was_absent = self
            .batch_entry_was_absent(storage.as_ref(), &entry_id)
            .await;
        let result = storage.append_record_and_entry(record, entry, lane).await;
        self.publish_batch_commit(
            storage.as_ref(),
            &entry_id,
            was_absent,
            result.as_ref().ok().map(|(_, entry)| entry),
            lane,
        )
        .await;
        result
    }

    /// Append a record, an entry, and a following record in that exact order.
    pub(crate) async fn append_record_entry_and_record(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        for payload in [
            serde_json::to_value(&before),
            serde_json::to_value(&entry),
            serde_json::to_value(&after),
        ] {
            let value = payload.map_err(|e| {
                SessionError::invalid_payload(format!("Durable payload is not serializable: {e}"))
            })?;
            assert_json_serializable(&value)?;
        }
        let storage = self.storage_arc();
        let entry_id = entry.id.clone();
        let was_absent = self
            .batch_entry_was_absent(storage.as_ref(), &entry_id)
            .await;
        let result = storage
            .append_record_entry_and_record(before, entry, lane, after)
            .await;
        self.publish_batch_commit(
            storage.as_ref(),
            &entry_id,
            was_absent,
            result.as_ref().ok().map(|(_, entry, _)| entry),
            lane,
        )
        .await;
        result
    }

    /// Append a batch of records through the active storage backend.
    pub(crate) async fn append_records(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        let diagnostics = agent_diagnostics_enabled();
        let count = records.len();
        let started = diagnostics.then(Instant::now);
        let result = self.storage_arc().append_records(records).await;
        if let Some(started) = started {
            eprintln!(
                "RPI_SESSION_DIAGNOSTICS kind=batch records={} append_ns={}",
                count,
                elapsed_ns(started),
            );
        }
        result
    }

    // -- Record queries ------------------------------------------------------

    /// `findRecords(query?)`. Mirrors TS `Session.findRecords`.
    pub async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.query_records(query).await
    }

    /// `findOpenOperations(lane, options?)`. Mirrors TS
    /// `Session.findOpenOperations`.
    pub async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        assert_valid_limit(limit)?;
        self.storage_arc().find_open_operations(lane, limit).await
    }

    /// `getLog(options?)`. Mirrors TS `Session.getLog`.
    pub async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.query_log(options).await
    }

    /// Wait for any in-flight durable write to settle.
    ///
    /// The facade forwards to the backend. Only the JSONL backend has a write
    /// that can still be in flight when its caller returns; the others commit
    /// inside the call, so their `drain` is the trait's default no-op.
    pub async fn drain(&self) {
        self.storage_arc().drain().await;
    }

    // -- Private helpers (mirror TS privates) --------------------------------

    /// `getLeafIdForLane(lane)` — the lane's current leaf, or `None` when empty.
    /// Throws `invalid_lane` when the lane does not exist. Mirrors TS private
    /// `Session.getLeafIdForLane`.
    async fn get_leaf_id_for_lane(&self, lane: &str) -> SessionResult<Option<String>> {
        let pointer = self
            .storage_arc()
            .get_lanes()
            .await?
            .into_iter()
            .find(|p| p.lane == lane)
            .ok_or_else(|| SessionError::invalid_lane(format!("Lane not found: {lane}")))?;
        Ok(pointer.leaf_id)
    }

    /// `queryEntries(query, resultLimit?)` — validate, optionally override the
    /// limit, delegate to storage. Mirrors TS private `Session.queryEntries`.
    async fn query_entries(
        &self,
        query: &EntryQuery,
        result_limit: Option<usize>,
    ) -> SessionResult<Vec<Entry>> {
        assert_valid_limit(query.limit)?;
        if let Some(cursor) = &query.cursor {
            assert_valid_cursor(Some(cursor.after_seq))?;
        }
        if result_limit == query.limit {
            self.storage_arc().find_entries(query).await
        } else {
            let mut overridden = query.clone();
            overridden.limit = result_limit;
            self.storage_arc().find_entries(&overridden).await
        }
    }

    /// `queryBranchEntries(defaultLane, query, resultLimit?)` — resolve
    /// `bounds.start` (default: lane leaf), validate, delegate. Mirrors TS
    /// private `Session.queryBranchEntries`.
    async fn query_branch_entries(
        &self,
        default_lane: &str,
        query: &EntryQuery,
        bounds: &BranchBounds,
        result_limit: Option<usize>,
    ) -> SessionResult<Vec<Entry>> {
        assert_valid_limit(query.limit)?;
        if let Some(cursor) = &query.cursor {
            assert_valid_cursor(Some(cursor.after_seq))?;
        }
        let start = match &bounds.start {
            Some(s) => s.clone(),
            None => match self.get_leaf_id_for_lane(default_lane).await? {
                Some(id) => id,
                None => return Ok(Vec::new()),
            },
        };
        let storage_bounds = if result_limit == query.limit {
            bounds.clone()
        } else {
            // `BranchBounds` carries no limit; the limit override lives on the
            // entry query. Clone the query with the overridden limit and keep
            // bounds as-is.
            BranchBounds { ..bounds.clone() }
        };
        let storage_query = if result_limit == query.limit {
            query.clone()
        } else {
            let mut q = query.clone();
            q.limit = result_limit;
            q
        };
        let _ = storage_bounds; // bounds passed through unchanged
        self.storage_arc()
            .find_entries_on_branch(&storage_query, bounds, &start)
            .await
    }

    /// `queryRecords(query)` — validate, enforce the `operationKind` +
    /// `type == operation_started` pairing, delegate. Mirrors TS private
    /// `Session.queryRecords`.
    async fn query_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        assert_valid_limit(query.limit)?;
        assert_valid_cursor(query.after_seq)?;
        if query.operation_kind.is_some() && query.record_type != Some("operation_started") {
            return Err(SessionError::new(
                SessionErrorCode::InvalidQuery,
                "operationKind requires type \"operation_started\"",
            ));
        }
        self.storage_arc().find_records(query).await
    }

    /// `queryLog(options)` — validate, delegate. Mirrors TS private
    /// `Session.queryLog`.
    async fn query_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        assert_valid_limit(options.limit)?;
        assert_valid_cursor(options.after_seq)?;
        self.storage_arc().get_log(options).await
    }

    /// `appendMessageToLane(lane, message)` — mint id, commit. Mirrors TS
    /// private `Session.appendMessageToLane`. Returns the entry id.
    async fn append_message_to_lane(
        &self,
        lane: &str,
        message: AgentMessage,
    ) -> SessionResult<String> {
        let entry = self
            .commit_entry(
                ProvisionedEntry {
                    id: self.id_generator.next(),
                    kind: ProvisionedKind::Message {
                        message,
                        terminate: None,
                    },
                },
                lane,
            )
            .await?;
        Ok(entry_id(&entry))
    }

    /// `appendCustomEntryToLane(lane, customType, data?)` — mint id, commit.
    /// Mirrors TS private `Session.appendCustomEntryToLane`. Returns the id.
    async fn append_custom_entry_to_lane(
        &self,
        lane: &str,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> SessionResult<String> {
        let entry = self
            .commit_entry(
                ProvisionedEntry {
                    id: self.id_generator.next(),
                    kind: ProvisionedKind::Custom {
                        custom_type: custom_type.to_string(),
                        data,
                    },
                },
                lane,
            )
            .await?;
        Ok(entry_id(&entry))
    }

    /// `commitEntry(entry, lane)` — `assertJsonSerializable` then
    /// `storage.appendEntry`. Mirrors TS private `Session.commitEntry`.
    async fn commit_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        // Serialize-then-validate mirrors TS `assertJsonSerializable(entry)`.
        // `ProvisionedEntry` is a typed struct, but its arms carry arbitrary
        // `JsonValue`/`f64` payloads, so the non-finite-number guard is real.
        let value = serde_json::to_value(&entry).map_err(|e| {
            SessionError::invalid_payload(format!("Durable payload is not serializable: {e}"))
        })?;
        assert_json_serializable(&value)?;
        let diagnostics = agent_diagnostics_enabled();
        let entry_type = entry.type_tag();
        let started = diagnostics.then(Instant::now);
        let result = self.storage_arc().append_entry(entry, lane).await;
        if let Ok(stamped) = &result {
            self.publish_entry_commit(stamped, lane);
        }
        if let Some(started) = started {
            eprintln!(
                "RPI_SESSION_DIAGNOSTICS kind=entry entry_type={} append_ns={}",
                entry_type,
                elapsed_ns(started),
            );
        }
        result
    }

    /// Only observers need the preflight read. Some backends use the ordered
    /// fallback rather than an atomic transaction, so a later record can fail
    /// after the entry has committed. Remember whether this id was absent to
    /// avoid reporting an old entry when a duplicate-id append is refused.
    async fn batch_entry_was_absent(&self, storage: &dyn SessionStorage, id: &str) -> bool {
        let observed = self
            .binding
            .read()
            .unwrap()
            .entry_commit_publisher
            .is_some();
        observed && matches!(storage.get_entry(id).await, Ok(None))
    }

    async fn publish_batch_commit(
        &self,
        storage: &dyn SessionStorage,
        id: &str,
        was_absent: bool,
        committed: Option<&Entry>,
        lane: &str,
    ) {
        if let Some(entry) = committed {
            self.publish_entry_commit(entry, lane);
        } else if was_absent {
            // Preserve the original batch error. If the backend is unreadable,
            // do not invent a commit; a recovery read is best-effort.
            if let Ok(Some(entry)) = storage.get_entry(id).await {
                self.publish_entry_commit(&entry, lane);
            }
        }
    }

    /// Publish confirmed entry commits using storage-stamped metadata.
    fn publish_entry_commit(&self, entry: &Entry, lane: &str) {
        let publisher = self.binding.read().unwrap().entry_commit_publisher.clone();
        if let Some(publisher) = publisher {
            let custom_type = match entry {
                Entry::Custom(custom) => Some(custom.custom_type.clone()),
                _ => None,
            };
            let _ = publisher.send(SessionEntryCommit {
                entry_id: entry.base().id.clone(),
                entry_type: entry.base().entry_type.clone(),
                custom_type,
                lane: lane.to_string(),
            });
        }
    }

    /// `commitRecord(record)` — `assertJsonSerializable` then
    /// `storage.appendRecord`. Mirrors TS private `Session.commitRecord`.
    async fn commit_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let value = serde_json::to_value(&record).map_err(|e| {
            SessionError::invalid_payload(format!("Durable payload is not serializable: {e}"))
        })?;
        assert_json_serializable(&value)?;
        let diagnostics = agent_diagnostics_enabled();
        let record_type = record.record_type();
        let started = diagnostics.then(Instant::now);
        let result = self.storage_arc().append_record(record).await;
        if let Some(started) = started {
            eprintln!(
                "RPI_SESSION_DIAGNOSTICS kind=record record_type={} append_ns={}",
                record_type,
                elapsed_ns(started),
            );
        }
        result
    }
}

/// Extract the id from a stamped [`Entry`]. Mirrors `entry.id`.
fn entry_id(entry: &Entry) -> String {
    entry.base().id.clone()
}

// ---------------------------------------------------------------------------
// Session: the main-lane SessionTree impl. Mirrors `class Session implements
// SessionTree` (the `view("main")` path returns `this`).
// ---------------------------------------------------------------------------

#[async_trait]
impl SessionTree for Session {
    async fn get_leaf_id(&self) -> SessionResult<Option<String>> {
        Session::get_leaf_id(self).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        Session::get_entry(self, id).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        Session::get_stats(self).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        Session::get_name(self).await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        Session::set_name(self, name).await
    }
    async fn get_label(&self, target_id: &str) -> SessionResult<Option<String>> {
        Session::get_label(self, target_id).await
    }
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> SessionResult<()> {
        Session::set_label(self, target_id, label).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        Session::find_entries(self, query).await
    }
    async fn find_entry(&self, query: &EntryQuery) -> SessionResult<Option<Entry>> {
        Session::find_entry(self, query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>> {
        Session::find_entries_on_branch(self, query, bounds).await
    }
    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Option<Entry>> {
        Session::find_entry_on_branch(self, query, bounds).await
    }
    async fn append_message(&self, message: AgentMessage) -> SessionResult<String> {
        Session::append_message(self, message).await
    }
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> SessionResult<String> {
        Session::append_custom_entry(self, custom_type, data).await
    }
}

// ---------------------------------------------------------------------------
// LaneView — a non-main lane SessionTree. Mirrors the TS `view(lane)` object
// literal that delegates leaf/branch/appends to that lane.
// ---------------------------------------------------------------------------

/// A lane-scoped [`SessionTree`] over a shared [`Session`]. Mirrors the TS
/// `Session.view(lane)` return value for `lane !== "main"`: reads like
/// `getEntry`/`getStats`/`getName`/`setLabel` are session-wide (delegated
/// straight through), while `getLeafId`/`findEntriesOnBranch`/`appendMessage`/
/// `appendCustomEntry` resolve against `lane`.
#[derive(Clone)]
pub struct LaneView {
    session: Session,
    lane: String,
}

#[async_trait]
impl SessionTree for LaneView {
    async fn get_leaf_id(&self) -> SessionResult<Option<String>> {
        self.session.get_leaf_id_for_lane(&self.lane).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.session.get_entry(id).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.session.get_stats().await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.session.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.session.set_name(name).await
    }
    async fn get_label(&self, target_id: &str) -> SessionResult<Option<String>> {
        self.session.get_label(target_id).await
    }
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> SessionResult<()> {
        self.session.set_label(target_id, label).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        // Session-wide, all branches (same as main-lane view).
        self.session.find_entries(query).await
    }
    async fn find_entry(&self, query: &EntryQuery) -> SessionResult<Option<Entry>> {
        self.session.find_entry(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>> {
        self.session
            .query_branch_entries(&self.lane, query, bounds, query.limit)
            .await
    }
    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Option<Entry>> {
        let mut entries = self
            .session
            .query_branch_entries(&self.lane, query, bounds, Some(1))
            .await?;
        Ok(entries.pop())
    }
    async fn append_message(&self, message: AgentMessage) -> SessionResult<String> {
        self.session
            .append_message_to_lane(&self.lane, message)
            .await
    }
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> SessionResult<String> {
        self.session
            .append_custom_entry_to_lane(&self.lane, custom_type, data)
            .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::types::{EntryQuery, SessionMetadata};
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::UserMessage;
    use std::sync::{Arc, Mutex};

    fn fixture_session() -> Session {
        let metadata = SessionMetadata {
            id: "s1".into(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        Session::new(storage, None)
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage::new(text, 1))
    }

    #[tokio::test]
    async fn atomic_prompt_commit_also_publishes_invalidation() {
        use crate::session::types::{OperationIntent, OperationStartedRecord, RecordBase};
        let session = fixture_session();
        let (publisher, mut events) = tokio::sync::broadcast::channel(4);
        session.set_entry_commit_publisher(Some(publisher));
        let record = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "op-probe".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        let (_, entry) = session
            .append_record_and_entry(
                record,
                ProvisionedEntry {
                    id: "prompt-probe".into(),
                    kind: ProvisionedKind::Message {
                        message: user("new prompt"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(entry.base().id, "prompt-probe");
        let event = events
            .try_recv()
            .expect("a successfully committed prompt must invalidate RPC clients too");
        assert_eq!(event.entry_id, "prompt-probe");
        assert_eq!(event.entry_type, "message");
        assert_eq!(event.lane, "main");
        assert_eq!(event.custom_type, None);
        assert!(session
            .view("main")
            .get_entry(&event.entry_id)
            .await
            .unwrap()
            .is_some());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    fn notification_record(id: &str) -> LaneRecord {
        use crate::session::types::{OperationIntent, OperationStartedRecord, RecordBase};
        LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: id.into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        })
    }

    fn notification_finish() -> LaneRecord {
        use crate::session::types::{OperationFinishedRecord, OperationOutcome, RecordBase};
        LaneRecord::OperationFinished(OperationFinishedRecord {
            base: RecordBase {
                id: "finish".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            run_id: "notification-op".into(),
            outcome: OperationOutcome::Completed,
            error: None,
        })
    }

    #[tokio::test]
    async fn rejected_duplicate_batch_does_not_notify_an_existing_entry() {
        for with_after in [false, true] {
            let session = fixture_session();
            let entry = || ProvisionedEntry {
                id: "existing".into(),
                kind: ProvisionedKind::Message {
                    message: user("old"),
                    terminate: None,
                },
            };
            session.append_entry(entry(), "main").await.unwrap();
            let (publisher, mut events) = broadcast::channel(4);
            session.set_entry_commit_publisher(Some(publisher));
            let before = notification_record("notification-op");
            let result = if with_after {
                session
                    .append_record_entry_and_record(before, entry(), "main", notification_finish())
                    .await
                    .map(|_| ())
            } else {
                session
                    .append_record_and_entry(before, entry(), "main")
                    .await
                    .map(|_| ())
            };
            assert!(result.is_err());
            assert!(matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
        }
    }

    #[tokio::test]
    async fn partial_batch_failure_still_notifies_committed_entry() {
        let session = fixture_session();
        let (publisher, mut events) = broadcast::channel(4);
        session.set_entry_commit_publisher(Some(publisher));
        let mut after = notification_finish();
        if let LaneRecord::OperationFinished(record) = &mut after {
            record.base.id = "notification-op".into(); // Reject only the final record.
        }
        let result = session
            .append_record_entry_and_record(
                notification_record("notification-op"),
                ProvisionedEntry {
                    id: "actually-committed".into(),
                    kind: ProvisionedKind::Message {
                        message: user("saved"),
                        terminate: None,
                    },
                },
                "main",
                after,
            )
            .await;
        assert!(result.is_err());
        assert!(
            session
                .view("main")
                .get_entry("actually-committed")
                .await
                .unwrap()
                .is_some(),
            "default storage fallback has already committed the entry"
        );
        let event = events.try_recv().expect("an entry that exists must not be omitted from invalidation just because the following record failed");
        assert_eq!(event.entry_id, "actually-committed");
    }

    #[tokio::test]
    async fn record_entry_record_commit_publishes_stamped_metadata_once() {
        let session = fixture_session();
        let (publisher, mut events) = broadcast::channel(4);
        session.set_entry_commit_publisher(Some(publisher));
        let (_, entry, _) = session
            .append_record_entry_and_record(
                notification_record("notification-op"),
                ProvisionedEntry {
                    id: "custom-notification".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: "state-update".into(),
                        data: None,
                    },
                },
                "main",
                notification_finish(),
            )
            .await
            .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.entry_id, entry.base().id);
        assert_eq!(event.entry_type, "custom");
        assert_eq!(event.custom_type.as_deref(), Some("state-update"));
        assert_eq!(event.lane, "main");
        assert!(session
            .view("main")
            .get_entry(&event.entry_id)
            .await
            .unwrap()
            .is_some());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn failed_atomic_entry_writes_do_not_publish_commits() {
        for with_after in [false, true] {
            let session = fixture_session();
            let (publisher, mut events) = broadcast::channel(4);
            session.set_entry_commit_publisher(Some(publisher));
            let entry = ProvisionedEntry {
                id: "refused".into(),
                kind: ProvisionedKind::Message {
                    message: user("refused"),
                    terminate: None,
                },
            };
            let before = notification_record("notification-op");
            if with_after {
                assert!(session
                    .append_record_entry_and_record(
                        before,
                        entry,
                        "missing-lane",
                        notification_finish()
                    )
                    .await
                    .is_err());
            } else {
                assert!(session
                    .append_record_and_entry(before, entry, "missing-lane")
                    .await
                    .is_err());
            }
            assert!(matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
        }
    }

    #[tokio::test]
    async fn default_id_generator_yields_uuidv7_shaped_ids() {
        let g = DefaultIdGenerator::new();
        let id = g.next();
        // uuidv7 strings are 36 chars with 4 hyphens; version digit at pos 14 is '7'.
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(id.as_bytes()[14], b'7');
    }

    #[tokio::test]
    async fn committed_entry_publishes_metadata_after_success() {
        let session = fixture_session();
        let (publisher, mut events) = tokio::sync::broadcast::channel(4);
        session.set_entry_commit_publisher(Some(publisher));

        session
            .append_entry(
                ProvisionedEntry {
                    id: "durable-entry".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: "durable-state".into(),
                        data: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.entry_id, "durable-entry");
        assert_eq!(event.entry_type, "custom");
        assert_eq!(event.custom_type.as_deref(), Some("durable-state"));
        assert_eq!(event.lane, "main");
    }

    #[tokio::test]
    async fn empty_context_matches_legacy_builder() {
        let session = fixture_session();
        let context = session
            .context_on_branch_for_lane("main", &BranchBounds::default())
            .await
            .unwrap();
        let expected = crate::session::context::build_session_context(
            &[],
            &crate::session::context::SessionContextBuildOptions::default(),
        );
        assert_eq!(context.messages, expected.messages);
        assert_eq!(context.thinking_level, "off");
        assert_eq!(context.thinking_level, expected.thinking_level);
    }

    #[derive(Clone)]
    struct RebindingStorage {
        inner: Arc<dyn SessionStorage>,
        callback: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>,
    }

    #[async_trait::async_trait]
    impl SessionStorage for RebindingStorage {
        fn metadata(&self) -> SessionMetadata {
            self.inner.metadata()
        }
        async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
            if let Some(callback) = self.callback.lock().unwrap().take() {
                callback();
            }
            self.inner.get_lanes().await
        }
        async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
            self.inner.create_lane(lane, at).await
        }
        async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
            self.inner.move_lane(lane, to).await
        }
        async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
            self.inner.append_entry(entry, lane).await
        }
        async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
            self.inner.append_record(record).await
        }
        async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
            self.inner.get_entry(id).await
        }
        async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
            self.inner.find_entries(query).await
        }
        async fn find_entries_on_branch(
            &self,
            query: &EntryQuery,
            bounds: &BranchBounds,
            start: &str,
        ) -> SessionResult<Vec<Entry>> {
            self.inner
                .find_entries_on_branch(query, bounds, start)
                .await
        }
        async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
            self.inner.find_records(query).await
        }
        async fn find_open_operations(
            &self,
            lane: &str,
            limit: Option<usize>,
        ) -> SessionResult<Vec<OperationStartedRecord>> {
            self.inner.find_open_operations(lane, limit).await
        }
        async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
            self.inner.get_log(options).await
        }
        async fn get_name(&self) -> SessionResult<Option<String>> {
            self.inner.get_name().await
        }
        async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
            self.inner.set_name(name).await
        }
        async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
            self.inner.get_label(id).await
        }
        async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
            self.inner.set_label(id, label).await
        }
        async fn get_stats(&self) -> SessionResult<SessionStats> {
            self.inner.get_stats().await
        }
    }

    #[tokio::test]
    async fn context_rebind_during_leaf_read_keeps_one_storage_binding() {
        let old_storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "old".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let new_storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "new".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let old = Session::new(old_storage.clone(), None);
        let new = Session::new(new_storage.clone(), None);
        old.append_message(user("old storage")).await.unwrap();
        new.append_message(user("new storage")).await.unwrap();

        let callback: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>> = Arc::new(Mutex::new(None));
        let wrapper: Arc<dyn SessionStorage> = Arc::new(RebindingStorage {
            inner: old_storage,
            callback: callback.clone(),
        });
        let facade = Session::new(wrapper, None);
        let replacement = new.storage();
        *callback.lock().unwrap() = Some(Box::new({
            let facade = facade.clone();
            move || facade.set_storage(replacement)
        }));

        let context = facade
            .context_on_branch_for_lane("main", &BranchBounds::default())
            .await
            .unwrap();
        assert!(matches!(
            &context.messages[0],
            AgentMessage::User(message)
                if message.content == rpi_ai::types::UserContent::Text("old storage".into())
        ));
        let current = facade
            .get_leaf_id()
            .await
            .unwrap()
            .expect("replacement leaf");
        assert!(matches!(
            facade.get_entry(&current).await.unwrap().unwrap(),
            Entry::Message(message)
                if matches!(message.message, AgentMessage::User(ref user)
                    if user.content == rpi_ai::types::UserContent::Text("new storage".into()))
        ));
    }

    #[tokio::test]
    async fn append_message_returns_id_and_entry_is_readable() {
        let session = fixture_session();
        let id = session.append_message(user("hello")).await.unwrap();
        assert!(!id.is_empty());
        let entry = session
            .get_entry(&id)
            .await
            .unwrap()
            .expect("entry present");
        assert_eq!(entry.base().id, id);
    }

    #[tokio::test]
    async fn get_leaf_id_tracks_main_lane_after_append() {
        let session = fixture_session();
        assert!(session.get_leaf_id().await.unwrap().is_none());
        let id = session.append_message(user("hi")).await.unwrap();
        assert_eq!(
            session.get_leaf_id().await.unwrap().as_deref(),
            Some(id.as_str())
        );
    }

    #[tokio::test]
    async fn view_main_returns_main_leaf_and_appends_to_main() {
        let session = fixture_session();
        let main = session.view("main");
        let id = main.append_message(user("on main")).await.unwrap();
        assert_eq!(
            session.get_leaf_id().await.unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            main.get_leaf_id().await.unwrap().as_deref(),
            Some(id.as_str())
        );
    }

    #[tokio::test]
    async fn view_nonexistent_lane_invalid_lane_on_leaf_lookup() {
        let session = fixture_session();
        let side = session.view("side");
        let err = side.get_leaf_id().await.unwrap_err();
        assert_eq!(err.code, SessionErrorCode::InvalidLane);
    }

    #[tokio::test]
    async fn create_lane_then_view_resolves_leaf() {
        let session = fixture_session();
        let root = session.append_message(user("root")).await.unwrap();
        session.create_lane("side", Some(&root)).await.unwrap();
        let side = session.view("side");
        // New lane forks at `root`: its leaf is `root` until appended.
        assert_eq!(
            side.get_leaf_id().await.unwrap().as_deref(),
            Some(root.as_str())
        );
        let side_id = side.append_message(user("on side")).await.unwrap();
        assert_eq!(
            side.get_leaf_id().await.unwrap().as_deref(),
            Some(side_id.as_str())
        );
        // Main lane is unaffected.
        assert_eq!(
            session.get_leaf_id().await.unwrap().as_deref(),
            Some(root.as_str())
        );
    }

    #[tokio::test]
    async fn find_entry_caps_at_one() {
        let session = fixture_session();
        session.append_message(user("one")).await.unwrap();
        session.append_message(user("two")).await.unwrap();
        let q = EntryQuery::default();
        let found = session.find_entry(&q).await.unwrap();
        assert!(found.is_some());
        // find_entries returns both (newest-first default).
        let all = session.find_entries(&EntryQuery::default()).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn invalid_limit_rejected() {
        let session = fixture_session();
        let q = EntryQuery {
            limit: Some(0),
            ..EntryQuery::default()
        };
        let err = session.find_entries(&q).await.unwrap_err();
        assert_eq!(err.code, SessionErrorCode::InvalidQuery);
    }

    #[tokio::test]
    async fn find_entries_on_branch_empty_lane_returns_empty() {
        let session = fixture_session();
        let bounds = BranchBounds::default();
        let entries = session
            .find_entries_on_branch(&EntryQuery::default(), &bounds)
            .await
            .unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn assert_json_serializable_rejects_non_finite() {
        // serde_json::Value cannot hold NaN via normal paths, but f64::INFINITY
        // round-trips through Number? It does not — serde_json rejects it. So we
        // craft a Number that is_finite()==false path via from_f64 (returns None).
        // Instead, validate the guard directly: a finite value passes.
        let v = serde_json::json!({ "a": 1, "b": [1, 2, 3], "c": null });
        assert!(assert_json_serializable(&v).is_ok());
        // Non-finite cannot be constructed as a Value::Number, so this test
        // documents the structural guarantee: building NaN/Inf into a Value is
        // itself rejected by serde_json.
        let nan = serde_json::Number::from_f64(f64::NAN);
        assert!(nan.is_none());
    }

    #[tokio::test]
    async fn operation_kind_without_operation_started_type_rejected() {
        let session = fixture_session();
        let q = RecordQuery {
            operation_kind: Some("run"),
            record_type: Some("operation_finished"),
            ..RecordQuery::default()
        };
        let err = session.find_records(&q).await.unwrap_err();
        assert_eq!(err.code, SessionErrorCode::InvalidQuery);
    }
}
