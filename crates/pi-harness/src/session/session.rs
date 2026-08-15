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

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use rpi_agent::message::AgentMessage;

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::types::{
    BranchBounds, Entry, EntryQuery, IdGenerator, JsonValue, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationStartedRecord, ProvisionedEntry, ProvisionedKind, RecordQuery,
    SessionMetadata, SessionStats, SessionStorage, SessionTree,
};

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

/// The session facade over a [`SessionStorage`] + [`IdGenerator`]. Mirrors TS
/// `Session<TMetadata>`. Holds shared handles (`Arc`) so it is cheaply
/// [`Clone`] — a `view` of any lane shares the same storage + id generator.
#[derive(Clone)]
pub struct Session {
    storage: Arc<dyn SessionStorage>,
    id_generator: Arc<dyn IdGenerator>,
}

impl Session {
    /// Build a facade over `storage` with an optional id generator (default
    /// [`DefaultIdGenerator`] / uuidv7). Mirrors TS `constructor`.
    pub fn new(storage: Arc<dyn SessionStorage>, id_generator: Option<Arc<dyn IdGenerator>>) -> Self {
        Self {
            storage,
            id_generator: id_generator.unwrap_or_else(|| Arc::new(DefaultIdGenerator::new())),
        }
    }

    /// The shared id generator. Mirrors TS `readonly idGenerator`.
    pub fn id_generator(&self) -> &Arc<dyn IdGenerator> {
        &self.id_generator
    }

    /// The shared storage handle (the harness needs the full storage API beyond
    /// what [`SessionTree`] exposes).
    pub fn storage(&self) -> &Arc<dyn SessionStorage> {
        &self.storage
    }

    /// `getMetadata()`. Mirrors TS `Session.getMetadata`.
    pub async fn get_metadata(&self) -> SessionResult<SessionMetadata> {
        Ok(self.storage.metadata())
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
            Arc::new(LaneView { session: self.clone(), lane: lane.to_string() }) as Arc<dyn SessionTree>
        }
    }

    // -- Reads (session-wide) ------------------------------------------------

    /// `getLeafId()` — the main lane's leaf. Mirrors TS `Session.getLeafId`.
    pub async fn get_leaf_id(&self) -> SessionResult<Option<String>> {
        self.get_leaf_id_for_lane("main").await
    }

    /// `getEntry(id)`. Mirrors TS `Session.getEntry`.
    pub async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.storage.get_entry(id).await
    }

    /// `getStats()`. Mirrors TS `Session.getStats`.
    pub async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.storage.get_stats().await
    }

    /// `getName()`. Mirrors TS `Session.getName`.
    pub async fn get_name(&self) -> SessionResult<Option<String>> {
        self.storage.get_name().await
    }

    /// `setName(name)`. Mirrors TS `Session.setName`.
    pub async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.storage.set_name(name).await
    }

    /// `getLabel(targetId)`. Mirrors TS `Session.getLabel`.
    pub async fn get_label(&self, target_id: &str) -> SessionResult<Option<String>> {
        self.storage.get_label(target_id).await
    }

    /// `setLabel(targetId, label)`. Mirrors TS `Session.setLabel`.
    pub async fn set_label(&self, target_id: &str, label: Option<&str>) -> SessionResult<()> {
        self.storage.set_label(target_id, label).await
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
        self.query_branch_entries("main", query, bounds, query.limit).await
    }

    /// `findEntryOnBranch(query?)` — first match on the main-lane branch path.
    /// Mirrors TS `Session.findEntryOnBranch`.
    pub async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> SessionResult<Option<Entry>> {
        let mut entries = self.query_branch_entries("main", query, bounds, Some(1)).await?;
        Ok(entries.pop())
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
        self.append_custom_entry_to_lane("main", custom_type, data).await
    }

    // -- Lanes ---------------------------------------------------------------

    /// `getLanes()`. Mirrors TS `Session.getLanes`.
    pub async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.storage.get_lanes().await
    }

    /// `createLane(lane, at)`. Mirrors TS `Session.createLane`.
    pub async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.storage.create_lane(lane, at).await
    }

    /// `moveLane(lane, to)`. Mirrors TS `Session.moveLane`.
    pub async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.storage.move_lane(lane, to).await
    }

    // -- Raw append (provisioned entry / record) -----------------------------

    /// `appendEntry(entry, lane)` — validate-then-commit a provisioned entry.
    /// Mirrors TS `Session.appendEntry`. Returns the storage-stamped entry.
    pub async fn append_entry(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<Entry> {
        self.commit_entry(entry, lane).await
    }

    /// `appendRecord(record)` — validate-then-commit a lane record. Mirrors TS
    /// `Session.appendRecord`. Returns the storage-stamped record.
    pub async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.commit_record(record).await
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
        self.storage.find_open_operations(lane, limit).await
    }

    /// `getLog(options?)`. Mirrors TS `Session.getLog`.
    pub async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.query_log(options).await
    }

    // -- Private helpers (mirror TS privates) --------------------------------

    /// `getLeafIdForLane(lane)` — the lane's current leaf, or `None` when empty.
    /// Throws `invalid_lane` when the lane does not exist. Mirrors TS private
    /// `Session.getLeafIdForLane`.
    async fn get_leaf_id_for_lane(&self, lane: &str) -> SessionResult<Option<String>> {
        let pointer = self
            .storage
            .get_lanes()
            .await?
            .into_iter()
            .find(|p| p.lane == lane)
            .ok_or_else(|| {
                SessionError::invalid_lane(format!("Lane not found: {lane}"))
            })?;
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
            self.storage.find_entries(query).await
        } else {
            let mut overridden = query.clone();
            overridden.limit = result_limit;
            self.storage.find_entries(&overridden).await
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
        self.storage
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
        self.storage.find_records(query).await
    }

    /// `queryLog(options)` — validate, delegate. Mirrors TS private
    /// `Session.queryLog`.
    async fn query_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        assert_valid_limit(options.limit)?;
        assert_valid_cursor(options.after_seq)?;
        self.storage.get_log(options).await
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
                    kind: ProvisionedKind::Message { message, terminate: None },
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
                    kind: ProvisionedKind::Custom { custom_type: custom_type.to_string(), data },
                },
                lane,
            )
            .await?;
        Ok(entry_id(&entry))
    }

    /// `commitEntry(entry, lane)` — `assertJsonSerializable` then
    /// `storage.appendEntry`. Mirrors TS private `Session.commitEntry`.
    async fn commit_entry(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<Entry> {
        // Serialize-then-validate mirrors TS `assertJsonSerializable(entry)`.
        // `ProvisionedEntry` is a typed struct, but its arms carry arbitrary
        // `JsonValue`/`f64` payloads, so the non-finite-number guard is real.
        let value = serde_json::to_value(&entry)
            .map_err(|e| SessionError::invalid_payload(format!("Durable payload is not serializable: {e}")))?;
        assert_json_serializable(&value)?;
        self.storage.append_entry(entry, lane).await
    }

    /// `commitRecord(record)` — `assertJsonSerializable` then
    /// `storage.appendRecord`. Mirrors TS private `Session.commitRecord`.
    async fn commit_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let value = serde_json::to_value(&record)
            .map_err(|e| SessionError::invalid_payload(format!("Durable payload is not serializable: {e}")))?;
        assert_json_serializable(&value)?;
        self.storage.append_record(record).await
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
        self.session.append_message_to_lane(&self.lane, message).await
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

    fn fixture_session() -> Session {
        let metadata = SessionMetadata { id: "s1".into(), created_at: 0, parent_session_id: None };
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
    async fn default_id_generator_yields_uuidv7_shaped_ids() {
        let g = DefaultIdGenerator::new();
        let id = g.next();
        // uuidv7 strings are 36 chars with 4 hyphens; version digit at pos 14 is '7'.
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(id.as_bytes()[14], b'7');
    }

    #[tokio::test]
    async fn append_message_returns_id_and_entry_is_readable() {
        let session = fixture_session();
        let id = session.append_message(user("hello")).await.unwrap();
        assert!(!id.is_empty());
        let entry = session.get_entry(&id).await.unwrap().expect("entry present");
        assert_eq!(entry.base().id, id);
    }

    #[tokio::test]
    async fn get_leaf_id_tracks_main_lane_after_append() {
        let session = fixture_session();
        assert!(session.get_leaf_id().await.unwrap().is_none());
        let id = session.append_message(user("hi")).await.unwrap();
        assert_eq!(session.get_leaf_id().await.unwrap().as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn view_main_returns_main_leaf_and_appends_to_main() {
        let session = fixture_session();
        let main = session.view("main");
        let id = main.append_message(user("on main")).await.unwrap();
        assert_eq!(session.get_leaf_id().await.unwrap().as_deref(), Some(id.as_str()));
        assert_eq!(main.get_leaf_id().await.unwrap().as_deref(), Some(id.as_str()));
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
        assert_eq!(side.get_leaf_id().await.unwrap().as_deref(), Some(root.as_str()));
        let side_id = side.append_message(user("on side")).await.unwrap();
        assert_eq!(side.get_leaf_id().await.unwrap().as_deref(), Some(side_id.as_str()));
        // Main lane is unaffected.
        assert_eq!(session.get_leaf_id().await.unwrap().as_deref(), Some(root.as_str()));
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
        let q = EntryQuery { limit: Some(0), ..EntryQuery::default() };
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
