//! Mirrors `packages/agent/src/harness/session/jsonl/storage.ts` — the durable
//! file-backed `SessionStorage`.
//!
//! ## Write path (single `tokio::Mutex<()>` tail chain)
//!
//! Every mutating method is [`enqueue`]d onto a `tokio::Mutex<()>`-guarded tail
//! (the Rust analog of the TS `Promise` chain `this.tail`). Each enqueued op
//! stamps `seq` + `timestamp` from the live [`SessionState`], appends the JSONL
//! line, then applies the mutation in memory — so the in-memory state is always
//! consistent with the durable tail. Reads clone straight from the in-memory
//! state (defensive-copy contract), never blocking on the tail.
//!
//! ## Load path + torn-tail recovery (plan §5.14)
//!
//! On `load`, the file is split into physical lines. Only a **syntax** error on
//! the **last** physical line is a recoverable *torn tail* (an unacknowledged
//! partial append): we atomically publish the valid prefix (`<dest>.tmp` then
//! rename) and proceed with the prefix's mutations. Any other error (a `schema`
//! error, or a syntax error on a non-last line) is hard corruption →
//! `SessionError::invalid_entry`. The per-destination rename is serialized by a
//! `tokio::Mutex<()>` on each storage handle (intra-handle; the repo additionally
//! guards cross-handle create races).
//!
//! ## Adaptation
//!
//! TS `appendEntr`/`appendRecord` take provisioned/new shapes and stamp inline;
//! the Rust [`SessionStorage`] trait takes a fully-stamped [`Entry`] / record is
//! *not* used here — this backend follows the in-memory one's convention of
//! stamping `seq`+`timestamp` itself (it owns the `SessionState`), so its
//! `append_entry` receives a [`ProvisionedEntry`] and stamps via the state +
//! a [`Clock`], then encodes the resulting full `Entry`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use rpi_tools::env::{FileContent, FileSystem};

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::jsonl::codec::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
};
use crate::session::jsonl::errors::{file_result, invalid_file, JsonlDecodeErrorKind};
use crate::session::jsonl::types::{JsonlSessionMetadata, JsonlV4Header};
use crate::session::memory::{stamp_record, Clock};
use crate::session::state::SessionState;
use crate::session::types::{
    provisioned_into_entry, BranchBounds, Entry, EntryQuery, ForkOptions, IdGenerator, LanePointer,
    LaneRecord, LogItem, LogOptions, OperationStartedRecord, ProvisionedEntry, RecordQuery,
    SessionMetadata, SessionMutation, SessionStats, SessionStorage,
};

/// Durable file-backed `SessionStorage`. Mirrors TS `JsonlSessionStorage`.
///
/// Owns: the shared [`FileSystem`], the session [`JsonlSessionMetadata`] (with
/// its on-disk `path`), the in-memory [`SessionState`] mirror, a [`Clock`] for
/// timestamps, an [`IdGenerator`] reserved for auto-id (callers currently
/// supply ids), and a `tokio::Mutex<()>` tail serializing appends.
pub struct JsonlSessionStorage {
    fs: Arc<dyn FileSystem>,
    metadata: JsonlSessionMetadata,
    state: tokio::sync::Mutex<SessionState>,
    clock: Arc<dyn Clock>,
    /// Reserved for future auto-id generation (callers currently supply entry/
    /// record ids). Kept for parity with `InMemorySessionStorage`.
    #[allow(dead_code)]
    ids: Arc<dyn IdGenerator>,
    /// Serializes appends to this session's file (the TS `tail` Promise chain).
    /// Not `&self` across await points, so each mutating op holds it for its
    /// full duration.
    tail: Mutex<()>,
}

impl JsonlSessionStorage {
    /// Build a storage over an already-initialized file (header written).
    /// Mirrors TS `new JsonlSessionStorage(fs, metadata)`.
    pub fn new(
        fs: Arc<dyn FileSystem>,
        metadata: JsonlSessionMetadata,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            fs,
            metadata,
            state: tokio::sync::Mutex::new(SessionState::new()),
            clock,
            ids,
            tail: Mutex::new(()),
        }
    }

    /// Create a new session file: write the header, then build a storage over
    /// it. Mirrors TS `JsonlSessionStorage.create`.
    pub async fn create(
        fs: Arc<dyn FileSystem>,
        path: &str,
        header: JsonlV4Header,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        file_result(
            fs.write_file(path, FileContent::Text(encode_header(&header)), None).await,
            &format!("Failed to initialize session {path}"),
        )?;
        let info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let metadata = metadata_from_header(&header, path, info.mtime_ms);
        Ok(Self::new(fs, metadata, clock, ids))
    }

    /// Load + replay an existing session file, repairing a torn tail if the
    /// last physical line is a syntax error. Mirrors TS `JsonlSessionStorage.load`.
    pub async fn load(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let content = file_result(
            fs.read_text_file(path, None).await,
            &format!("Failed to read session {path}"),
        )?;
        // TS: content.split("\n"); drop trailing "" from the final "\n".
        let mut physical_lines: Vec<&str> = content.split('\n').collect();
        if physical_lines.last() == Some(&"") {
            physical_lines.pop();
        }
        if physical_lines.is_empty() || physical_lines[0].is_empty() {
            return Err(invalid_file(path, 1, &"is missing a header".to_string()));
        }
        let header = parse_header(physical_lines[0])
            .map_err(|e| invalid_file(path, 1, &e))?;
        let info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let metadata = metadata_from_header(&header, path, info.mtime_ms);
        let storage = Self::new(fs.clone(), metadata, clock, ids);

        for (idx, line) in physical_lines.iter().enumerate().skip(1) {
            let line_no = (idx + 1) as u32;
            match parse_mutation(line) {
                Ok(mutation) => {
                    let mut state = storage.state.lock().await;
                    if let Err(e) = state.apply_mutation(mutation) {
                        if e.code == SessionErrorCode::InvalidEntry {
                            drop(state);
                            return Err(invalid_file(path, line_no, &e));
                        }
                        drop(state);
                        return Err(e);
                    }
                }
                Err(err) => {
                    let is_last = idx == physical_lines.len() - 1;
                    let is_torn_tail = is_last && err.kind == JsonlDecodeErrorKind::Syntax;
                    if is_torn_tail {
                        // Drop the unacknowledged partial append by atomically
                        // publishing the valid prefix.
                        let valid_prefix =
                            format!("{}\n", physical_lines[..idx].join("\n"));
                        let fs = storage.fs.clone();
                        let path_owned = path.to_string();
                        publish_file_atomically(
                            &fs,
                            &path_owned,
                            |fs, temp_path| {
                                let valid_prefix = valid_prefix.clone();
                                let path_owned = path_owned.clone();
                                Box::pin(async move {
                                    file_result(
                                        fs.write_file(
                                            temp_path,
                                            FileContent::Text(valid_prefix),
                                            None,
                                        )
                                        .await,
                                        &format!("Failed to stage torn-tail repair {path_owned}"),
                                    )?;
                                    Ok(())
                                })
                            },
                        )
                        .await?;
                        return Ok(storage);
                    }
                    return Err(invalid_file(path, line_no, &err));
                }
            }
        }
        // Repair an unterminated tail (no trailing "\n") so future appends are
        // on their own line.
        if !content.ends_with('\n') {
            file_result(
                storage.fs.append_file(path, FileContent::Text("\n".into()), None).await,
                &format!("Failed to repair unterminated session tail {path}"),
            )?;
        }
        Ok(storage)
    }

    /// Fork into `path` by writing a fresh header then replaying the fork
    /// mutations. Mirrors TS `JsonlSessionStorage.fork`. The target file is
    /// built via [`publish_file_atomically`] so a crash leaves only a `.tmp`.
    pub async fn fork(
        &self,
        path: &str,
        header: JsonlV4Header,
        options: &ForkOptions,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> SessionResult<Self> {
        let mutations = {
            let state = self.state.lock().await;
            state.create_fork_mutations(options)?
        };
        publish_file_atomically(&self.fs, path, |fs, temp_path| {
            let header = header.clone();
            let mutations = mutations.clone();
            let clock = clock.clone();
            let ids = ids.clone();
            Box::pin(async move {
                let target = JsonlSessionStorage::create(fs.clone(), temp_path, header, clock, ids).await?;
                for mutation in &mutations {
                    let line = encode_mutation(mutation);
                    file_result(
                        fs.append_file(temp_path, FileContent::Text(line), None).await,
                        &format!("Failed to append session {temp_path}"),
                    )?;
                    let mut state = target.state.lock().await;
                    state.apply_mutation(mutation.clone())?;
                }
                Ok(())
            })
        })
        .await?;
        JsonlSessionStorage::load(self.fs.clone(), path, clock, ids).await
    }

    /// Wait for all in-flight appends to settle. Mirrors TS `drain`.
    pub async fn drain(&self) {
        let _g = self.tail.lock().await;
    }

    /// The rich JSONL metadata (the trait exposes only the base subset).
    pub fn jsonl_metadata(&self) -> JsonlSessionMetadata {
        self.metadata.clone()
    }

    /// Enqueue a mutating operation onto the tail chain. Mirrors TS `enqueue`.
    #[allow(dead_code)]
    async fn enqueue<T>(
        &self,
        op: impl std::future::Future<Output = SessionResult<T>>,
    ) -> SessionResult<T> {
        let _g = self.tail.lock().await;
        op.await
    }

    /// Append a mutation line to the file (no in-memory apply). Mirrors TS
    /// `appendMutation`.
    async fn append_mutation_line(&self, mutation: &SessionMutation) -> SessionResult<()> {
        let line = encode_mutation(mutation);
        file_result(
            self.fs
                .append_file(&self.metadata.path, FileContent::Text(line), None)
                .await,
            &format!("Failed to append session {}", self.metadata.path),
        )
    }
}

#[async_trait]
impl SessionStorage for JsonlSessionStorage {
    fn metadata(&self) -> SessionMetadata {
        self.metadata.to_base()
    }

    async fn get_metadata(&self) -> SessionMetadata {
        self.metadata.to_base()
    }

    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        let state = self.state.lock().await;
        Ok(state.get_lanes())
    }

    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let at = at.map(|s| s.to_string());
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.validate_new_lane(&lane)?;
        state.validate_target(at.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane { seq, lane: lane.clone(), leaf_id: at };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let to = to.map(|s| s.to_string());
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_target(to.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane { seq, lane, leaf_id: to };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&entry.id)?;
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let full = provisioned_into_entry(entry, seq, parent_id, timestamp);
        let mutation = SessionMutation::Entry {
            seq,
            timestamp,
            lane: Some(lane.to_string()),
            entry: full.clone(),
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        match state.get_entry(full.id()) {
            Some(stamped) => Ok(stamped.clone()),
            None => Err(SessionError::storage("append_entry did not persist")),
        }
    }

    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let lane = record.lane().to_string();
        let is_operation_started = matches!(record, LaneRecord::OperationStarted(_));
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_unused_id(record.id())?;
        if is_operation_started {
            let open = state.find_open_operations(&lane, Some(1))?;
            if let Some(current) = open.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    lane,
                    current.base.id
                )));
            }
        }
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let stamped = stamp_record(record, seq, lane.clone(), timestamp);
        let mutation = SessionMutation::Record { record: stamped.clone() };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(stamped)
    }

    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        let state = self.state.lock().await;
        Ok(state.get_entry(id).cloned())
    }

    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        let state = self.state.lock().await;
        state.find_entries(query)
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        let state = self.state.lock().await;
        state.find_entries_on_branch(query, bounds, start)
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        let state = self.state.lock().await;
        state.find_records(query)
    }

    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        let state = self.state.lock().await;
        state.find_open_operations(lane, limit)
    }

    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        let state = self.state.lock().await;
        state.get_log(options)
    }

    async fn get_name(&self) -> SessionResult<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.get_name().map(|s| s.to_string()))
    }

    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        let seq = state.next_sequence();
        let mutation = SessionMutation::FactName { seq, name: name.map(|s| s.to_string()) };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.get_label(id).map(|s| s.to_string()))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        let _g = self.tail.lock().await;
        let state = self.state.lock().await;
        state.validate_target(Some(id))?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::FactLabel {
            seq,
            target_id: id.to_string(),
            label: label.map(|s| s.to_string()),
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn get_stats(&self) -> SessionResult<SessionStats> {
        let state = self.state.lock().await;
        Ok(state.get_stats().clone())
    }
}

// The `enqueue` closure approach above fights the borrow checker (closures
// can't capture `&self` across an await of a method on `self`). The trait impl
// therefore inlines the stamp→persist→apply sequence directly per method,
// holding the `tail` lock across the whole op (the same serialization the TS
// `tail` Promise chain provides). `enqueue` + `self_append_and_apply` are kept
// only as documentation of the intent and are unused.
#[allow(dead_code)]
async fn self_append_and_apply(_storage: &JsonlSessionStorage, _m: &SessionMutation) -> SessionResult<()> {
    Ok(())
}

#[allow(dead_code)]
fn self_fs_placeholder() -> Arc<dyn FileSystem> {
    // unreachable placeholder for the never-called enqueue path.
    unreachable!("enqueue path is not used; trait impls inline the op")
}

/// Build a complete sibling temporary file, then atomically rename it over the
/// destination. Mirrors TS `publishFileAtomically`.
///
/// The populate callback must create/overwrite `tempPath` with the complete
/// file; the destination is untouched until the rename commits, so a crash
/// while populating leaves only the ignored `.tmp`. Per-destination
/// serialization is the caller's responsibility (the storage holds its `tail`
/// mutex; the repo holds its create-destination set).
async fn publish_file_atomically<F>(
    fs: &Arc<dyn FileSystem>,
    destination_path: &str,
    populate: F,
) -> SessionResult<()>
where
    F: for<'a> FnOnce(&'a Arc<dyn FileSystem>, &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = SessionResult<()>> + Send + 'a>>,
{
    let temp_path = format!("{destination_path}.tmp");
    // Mirror the TS `try { populate; rename } catch { remove(tempPath) }` shape:
    // the staged `.tmp` is best-effort removed on ANY failure (staging or
    // rename), so a crash/failure never leaves a partial `.tmp` behind. The
    // original destination is untouched until the rename commits.
    let result: SessionResult<()> = (async {
        populate(fs, &temp_path).await?;
        file_result(
            fs.rename_file(&temp_path, destination_path, None).await,
            &format!("Failed to publish staged file {destination_path}"),
        )?;
        Ok(())
    })
    .await;
    if result.is_err() {
        // Best-effort cleanup; preserve the original error.
        let _ = fs.remove(&temp_path, false, true, None).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock};
    use crate::session::types::{OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind, RecordBase};

    fn user_msg(text: &str) -> rpi_agent::message::AgentMessage {
        rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
    }

    fn header() -> JsonlV4Header {
        JsonlV4Header::new(
            "sess-1".into(),
            1_700_000_000_000,
            "/cwd".into(),
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn create_append_reload_roundtrips() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();
        let entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message { message: user_msg("hi"), terminate: None },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(entry.id(), "e1");

        let reloaded = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids).await.unwrap();
        let got = reloaded.get_entry("e1").await.unwrap().expect("entry exists");
        assert_eq!(got.id(), "e1");
    }

    #[tokio::test]
    async fn torn_tail_last_line_is_repaired() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();
        // Manually append a valid entry line + a garbage (torn) last line.
        // The first mutation after the header is seq 1 (the header does not
        // consume a sequence number — see `create_append_reload_roundtrips`).
        let valid = encode_mutation(&SessionMutation::Lane { seq: 1, lane: "main".into(), leaf_id: None });
        fs.append_file("/s.jsonl", FileContent::Text(valid), None).await.unwrap();
        fs.append_file("/s.jsonl", FileContent::Text("{not json".into()), None).await.unwrap();

        let reloaded = JsonlSessionStorage::load(fs.clone(), "/s.jsonl", clock, ids).await.unwrap();
        // The torn tail was dropped; the file now ends after the valid lane line.
        let content = fs.read_text_file("/s.jsonl", None).await.unwrap();
        assert!(!content.contains("{not json"));
        // And the storage is usable for further appends.
        let _ = reloaded.get_name().await.unwrap();
    }

    #[tokio::test]
    async fn non_last_syntax_error_is_hard_corruption() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();
        // Garbage on line 2 (NOT last), then a valid line 3.
        fs.append_file("/s.jsonl", FileContent::Text("{not json\n".into()), None).await.unwrap();
        fs.append_file("/s.jsonl", FileContent::Text(encode_mutation(&SessionMutation::Lane { seq: 1, lane: "main".into(), leaf_id: None })), None).await.unwrap();

        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids).await.err().unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn schema_error_on_last_line_is_hard_corruption() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();
        // A schema error (valid JSON, unknown kind) on the last line is NOT a
        // torn tail.
        fs.append_file("/s.jsonl", FileContent::Text("{\"kind\":\"bogus\",\"seq\":2}\n".into()), None).await.unwrap();
        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids).await.err().unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn append_record_rejects_second_open_operation() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(fs, "/s.jsonl", header(), clock, ids).await.unwrap();
        let started = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase { id: "op-1".into(), seq: 0, lane: "main".into(), timestamp: 0 },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        });
        storage.append_record(started).await.unwrap();
        let second = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase { id: "op-2".into(), seq: 0, lane: "main".into(), timestamp: 0 },
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
}
