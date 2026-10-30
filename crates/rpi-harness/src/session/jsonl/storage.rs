//! Mirrors `packages/agent/src/harness/session/jsonl/storage.ts` — the durable
//! file-backed `SessionStorage`.
//!
//! ## Write path (single `tokio::Mutex<()>` tail chain)
//!
//! Each mutating method first reserves the shared tail, then dispatches an owner
//! that keeps that reservation while stamping, appending the JSONL line, and
//! applying the reducer. Dropping the caller's future cancels only its wait for
//! the result; the owner still completes the append and reducer commit. Reads
//! clone straight from the in-memory state (defensive-copy contract), never
//! blocking on the tail.
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
//! ### Duplicate-seq recovery (divergent writers)
//!
//! Two writers appending to one JSONL diverge their `sequence` counters, which
//! used to make the whole session unopenable. `load` handles the following
//! cases:
//!
//! - **An exact stale seq** (`seq <= sequence`): a mutation identical to the
//!   already applied raw mutation is a duplicate and may be dropped.
//! - **A divergent stale seq**: same sequence with different content is
//!   corruption and is rejected without rewriting the source file.
//! - **A forward gap** (`seq > sequence + 1`): the other writer's counter was
//!   ahead. The counter is resynced up to the gap, leaving raw seqs untouched so
//!   future appends stay consistent with the file.
//!
//! An entry that no longer chains to the lane leaf (its parent was the other
//! writer's leaf) is re-parented onto the current leaf. When any duplicate was
//! dropped the file is atomically republished without it. The host additionally
//! takes an OS advisory lock per session (see `pi-cli`'s `claim_session_lock`)
//! so a second process is refused rather than allowed to write concurrently.
//!
//! ## Adaptation
//!
//! TS `appendEntr`/`appendRecord` take provisioned/new shapes and stamp inline;
//! the Rust [`SessionStorage`] trait takes a fully-stamped [`Entry`] / record is
//! *not* used here — this backend follows the in-memory one's convention of
//! stamping `seq`+`timestamp` itself (it owns the `SessionState`), so its
//! `append_entry` receives a [`ProvisionedEntry`] and stamps via the state +
//! a [`Clock`], then encodes the resulting full `Entry`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use rpi_tools::env::{FileContent, FileSystem};

use crate::error::{SessionError, SessionErrorCode, SessionResult};
use crate::session::jsonl::codec::{
    encode_header, encode_mutation, encode_mutation_into, metadata_from_header, parse_header,
    parse_mutation,
};
use crate::session::jsonl::errors::{file_result, invalid_file, JsonlDecodeErrorKind};
use crate::session::jsonl::types::{JsonlSessionMetadata, JsonlV4Header};
use crate::session::memory::{stamp_record, Clock};
use crate::session::state::{ApplyOutcome, SessionState};
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
    state: Arc<tokio::sync::Mutex<SessionState>>,
    clock: Arc<dyn Clock>,
    /// Reserved for future auto-id generation (callers currently supply entry/
    /// record ids). Kept for parity with `InMemorySessionStorage`.
    #[allow(dead_code)]
    ids: Arc<dyn IdGenerator>,
    /// Serializes appends to this session's file (the TS `tail` Promise chain).
    /// Not `&self` across await points, so each mutating op holds it for its
    /// full duration.
    tail: Arc<Mutex<()>>,
    append: Arc<Mutex<Option<Box<dyn rpi_tools::env::AppendFile>>>>,
    needs_reconcile: Arc<std::sync::atomic::AtomicBool>,
    /// Set when a failed append cannot be proven/truncated safely.
    uncertain: Arc<AtomicBool>,
}

impl Clone for JsonlSessionStorage {
    fn clone(&self) -> Self {
        Self {
            fs: self.fs.clone(),
            metadata: self.metadata.clone(),
            state: self.state.clone(),
            clock: self.clock.clone(),
            ids: self.ids.clone(),
            tail: self.tail.clone(),
            append: self.append.clone(),
            needs_reconcile: self.needs_reconcile.clone(),
            uncertain: self.uncertain.clone(),
        }
    }
}

impl JsonlSessionStorage {
    /// Replace the append handle. Test-only: lets a probe make the backend
    /// refuse or tear a write without a real disk fault.
    #[cfg(test)]
    pub fn install_append_handle(&self, handle: Box<dyn rpi_tools::env::AppendFile>) {
        // The handle can only be swapped while nothing is writing, which is
        // always true at the point a probe installs one.
        let mut slot = self
            .append
            .try_lock()
            .expect("install_append_handle requires an idle append slot");
        *slot = Some(handle);
    }

    /// Detached owner: cancellation of a caller cannot skip append + reducer commit.
    async fn run_owned<T, F, Fut>(&self, op: F) -> SessionResult<T>
    where
        T: Send + 'static,
        F: FnOnce(JsonlSessionStorage) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = SessionResult<T>> + Send + 'static,
    {
        let reservation = self.tail.clone().lock_owned().await;
        let worker = self.clone();
        tokio::spawn(async move {
            let _reservation = reservation;
            if worker.uncertain.load(Ordering::Acquire) {
                return Err(SessionError::storage(format!(
                    "Session {} has an unsettled append; reopen it before writing",
                    worker.metadata.path
                )));
            }
            if let Err(error) = worker.reconcile_if_needed().await {
                return Err(error);
            }
            op(worker).await
        })
        .await
        .map_err(|e| SessionError::storage(format!("JSONL mutation task failed: {e}")))?
    }

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
            state: Arc::new(tokio::sync::Mutex::new(SessionState::new())),
            clock,
            ids,
            tail: Arc::new(Mutex::new(())),
            append: Arc::new(Mutex::new(None)),
            needs_reconcile: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            uncertain: Arc::new(AtomicBool::new(false)),
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
            fs.write_file(path, FileContent::Text(encode_header(&header)), None)
                .await,
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
        let header = parse_header(physical_lines[0]).map_err(|e| invalid_file(path, 1, &e))?;
        let info = file_result(
            fs.file_info(path, None).await,
            &format!("Failed to read session metadata {path}"),
        )?;
        let metadata = metadata_from_header(&header, path, info.mtime_ms);
        let storage = Self::new(fs.clone(), metadata, clock, ids);

        // Lines kept through the duplicate-seq recovery, so a healed file can
        // be republished without the dropped duplicates (see after the loop).
        let mut kept_lines: Vec<&str> = Vec::new();
        let mut dropped_duplicates = false;
        let mut seen_lines: HashMap<u64, &str> = HashMap::new();
        for (idx, line) in physical_lines.iter().enumerate().skip(1) {
            let line_no = (idx + 1) as u32;
            match parse_mutation(line) {
                Ok(mutation) => {
                    let mut state = storage.state.lock().await;
                    // A stale sequence is safe to drop only when the raw
                    // mutation is exactly identical to the one already applied.
                    // A different mutation with the same sequence is corruption;
                    // reject it before any repair or source rewrite.
                    if mutation.seq() <= state.sequence() {
                        // A duplicate is safe to ignore only when the complete
                        // mutation is byte/shape-equivalent to the one already
                        // reduced.  Same-seq divergent data is corruption: do
                        // not silently delete it or rewrite the source file.
                        let exact_duplicate = seen_lines
                            .get(&mutation.seq())
                            .and_then(|previous| parse_mutation(previous).ok())
                            .as_ref()
                            == Some(&mutation);
                        if exact_duplicate {
                            dropped_duplicates = true;
                            drop(state);
                            continue;
                        }
                        let detail =
                            format!("conflicting mutations share sequence {}", mutation.seq());
                        drop(state);
                        return Err(invalid_file(path, line_no, &detail));
                    }
                    // A divergent writer's counter can leave a forward gap (its
                    // seqs were consumed elsewhere). Accept the gap by moving
                    // the counter up to it. Raw seqs stay untouched, so future
                    // appends stay consistent with the file.
                    if mutation.seq() > state.next_sequence() {
                        state.resync_sequence(mutation.seq() - 1);
                    }
                    let original_seq = mutation.seq();
                    let mutation = chain_onto_lane_leaf(mutation, &state);
                    if let Err(e) = state.apply_mutation(mutation) {
                        if e.code == SessionErrorCode::InvalidEntry {
                            drop(state);
                            return Err(invalid_file(path, line_no, &e));
                        }
                        drop(state);
                        return Err(e);
                    }
                    seen_lines.insert(original_seq, *line);
                    kept_lines.push(*line);
                }
                Err(err) => {
                    // Assistant frames are transient streaming progress, not
                    // session history. Native Pi treats an undecodable frame
                    // as lost progress rather than making the whole session
                    // unopenable. Keep all other schema errors strict.
                    if err.kind == JsonlDecodeErrorKind::Schema && is_assistant_frame_append(*line)
                    {
                        tracing::warn!(line = line_no, "skipping undecodable assistant frame");
                        continue;
                    }
                    let is_last = idx == physical_lines.len() - 1;
                    let is_torn_tail = is_last && err.kind == JsonlDecodeErrorKind::Syntax;
                    if is_torn_tail {
                        // Drop the unacknowledged partial append by atomically
                        // publishing the valid prefix.
                        let valid_prefix = format!("{}\n", physical_lines[..idx].join("\n"));
                        let fs = storage.fs.clone();
                        let path_owned = path.to_string();
                        publish_file_atomically(&fs, &path_owned, |fs, temp_path| {
                            let valid_prefix = valid_prefix.clone();
                            let path_owned = path_owned.clone();
                            Box::pin(async move {
                                file_result(
                                    fs.write_file(temp_path, FileContent::Text(valid_prefix), None)
                                        .await,
                                    &format!("Failed to stage torn-tail repair {path_owned}"),
                                )?;
                                Ok(())
                            })
                        })
                        .await?;
                        return Ok(storage);
                    }
                    return Err(invalid_file(path, line_no, &err));
                }
            }
        }
        // Persist the duplicate-seq recovery: atomically republish the file
        // without the dropped duplicates so the log no longer carries them.
        // Seq values are untouched (no renumbering), so subsequent appends
        // stay consistent with the file.
        if dropped_duplicates {
            let mut repaired = String::with_capacity(content.len());
            repaired.push_str(physical_lines[0]);
            repaired.push('\n');
            for line in &kept_lines {
                repaired.push_str(line);
                repaired.push('\n');
            }
            let fs = storage.fs.clone();
            let path_owned = path.to_string();
            publish_file_atomically(&fs, &path_owned, |fs, temp_path| {
                let repaired = repaired.clone();
                let path_owned = path_owned.clone();
                Box::pin(async move {
                    file_result(
                        fs.write_file(temp_path, FileContent::Text(repaired), None)
                            .await,
                        &format!("Failed to stage session repair {path_owned}"),
                    )?;
                    Ok(())
                })
            })
            .await?;
            return Ok(storage);
        }
        // Repair an unterminated tail (no trailing "\n") so future appends are
        // on their own line.
        if !content.ends_with('\n') {
            file_result(
                storage
                    .fs
                    .append_file(path, FileContent::Text("\n".into()), None)
                    .await,
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
                let target =
                    JsonlSessionStorage::create(fs.clone(), temp_path, header, clock, ids).await?;
                for mutation in &mutations {
                    let line = encode_mutation(mutation);
                    file_result(
                        fs.append_file(temp_path, FileContent::Text(line), None)
                            .await,
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
    ///
    /// Thin alias for the trait method so the concrete type keeps the inherent
    /// entry point the module docs refer to.
    pub async fn drain(&self) {
        <Self as SessionStorage>::drain(self).await;
    }

    /// The rich JSONL metadata (the trait exposes only the base subset).
    pub fn jsonl_metadata(&self) -> JsonlSessionMetadata {
        self.metadata.clone()
    }

    async fn reconcile_if_needed(&self) -> SessionResult<()> {
        if self.uncertain.load(Ordering::Acquire) {
            return Err(SessionError::storage(format!(
                "Session {} has an unsettled append; reopen it before writing",
                self.metadata.path
            )));
        }
        if !self.needs_reconcile.load(Ordering::Acquire) {
            return Ok(());
        }
        let recovered = Self::load(
            self.fs.clone(),
            &self.metadata.path,
            self.clock.clone(),
            self.ids.clone(),
        )
        .await?;
        let recovered_state = recovered.state.lock().await.clone();
        *self.state.lock().await = recovered_state;
        self.needs_reconcile.store(false, Ordering::Release);
        Ok(())
    }

    async fn append_encoded_bytes(&self, lines: Vec<u8>) -> SessionResult<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut append = self.append.lock().await;
        if append.is_none() {
            *append = file_result(
                self.fs.open_append_file(&self.metadata.path).await,
                &format!("Failed to open session {}", self.metadata.path),
            )?;
        }
        let result = if let Some(writer) = append.as_mut() {
            let result = writer.append(&lines).await;
            if result.is_err() {
                *append = None;
            }
            result
        } else {
            self.fs
                .append_file(&self.metadata.path, FileContent::Bytes(lines.clone()), None)
                .await
        };
        if let Err(error) = result {
            // The reducer has not seen this batch. Remove the whole batch,
            // including any complete lines before a torn final line.
            drop(append);
            if self.cut_back_failed_append(&lines).await.is_err() {
                self.uncertain.store(true, Ordering::SeqCst);
                self.needs_reconcile.store(true, Ordering::Release);
            }
            return file_result(
                Err(error),
                &format!("Failed to append session {}", self.metadata.path),
            );
        }
        Ok(())
    }

    /// Cut the failed batch back only when its bytes are provably the file tail.
    async fn cut_back_failed_append(&self, batch: &[u8]) -> Result<(), ()> {
        let bytes = self
            .fs
            .read_binary_file(&self.metadata.path, None)
            .await
            .map_err(|_| ())?;
        let start = failed_append_start(&bytes, batch).ok_or(())?;
        if start == bytes.len() {
            return Ok(());
        }
        self.fs
            .truncate_file(&self.metadata.path, start as u64, None)
            .await
            .map_err(|_| ())
    }

    /// Append a frame-only batch without cloning the complete session state.
    /// Frame records do not change lanes or open-operation state, so their
    /// sequence and id validation can use a small local batch index.
    async fn append_frame_record_batch(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        let state = self.state.lock().await;
        let mut next_seq = state.next_sequence();
        let mut ids = HashSet::with_capacity(records.len());
        let mut mutations = Vec::with_capacity(records.len());
        let mut encoded_lines = Vec::with_capacity(records.len() * 256);

        for record in records {
            let lane = record.lane().to_string();
            state.require_lane(&lane)?;
            if !ids.insert(record.id().to_string()) {
                return Err(SessionError::storage(format!(
                    "Record id already used in batch: {}",
                    record.id()
                )));
            }
            state.validate_unused_id(record.id())?;
            let timestamp = self.clock.now_ms();
            let stamped = stamp_record(record, next_seq, lane, timestamp);
            next_seq += 1;
            let mutation = SessionMutation::Record { record: stamped };
            mutations.push(mutation);
        }
        drop(state);

        for mutation in &mutations {
            encode_mutation_into(mutation, &mut encoded_lines);
        }
        self.append_encoded_bytes(encoded_lines).await?;
        let mut state = self.state.lock().await;
        let mut stamped_records = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            match state.apply_mutation(mutation)? {
                ApplyOutcome::Record(stamped) => stamped_records.push(stamped),
                _ => {
                    return Err(SessionError::storage(
                        "append_frame_record_batch did not persist a record",
                    ));
                }
            }
        }
        Ok(stamped_records)
    }

    /// Append one non-frame record without cloning the complete session state.
    ///
    /// Record validation is completed while the session tail is held. The
    /// state is only advanced after the durable append succeeds, so a failed
    /// write leaves the in-memory state unchanged while avoiding the O(history)
    /// candidate-state clone on the normal single-record path.
    async fn append_single_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        let lane = record.lane().to_string();
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_unused_id(record.id())?;
        if matches!(record, LaneRecord::OperationStarted(_)) {
            if let Some(current) = state.find_open_operations(&lane, Some(1))?.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    lane, current.base.id
                )));
            }
        }
        let seq = state.next_sequence();
        let timestamp = self.clock.now_ms();
        let stamped = stamp_record(record, seq, lane, timestamp);
        let mutation = SessionMutation::Record { record: stamped };
        let mut encoded = Vec::with_capacity(256);
        encode_mutation_into(&mutation, &mut encoded);
        drop(state);

        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        match state.apply_mutation(mutation)? {
            ApplyOutcome::Record(stamped) => Ok(stamped),
            _ => Err(SessionError::storage("append_record did not persist")),
        }
    }

    /// Append a batch of lane records atomically from the reducer's point of
    /// view. The candidate state is advanced before the file write, but the
    /// live state is replaced only after the whole encoded batch is visible.
    async fn append_record_batch_owned(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        if records
            .iter()
            .all(|record| matches!(record, LaneRecord::AssistantFrame(_)))
        {
            return self.append_frame_record_batch(records).await;
        }
        if records.len() == 1 {
            return Ok(vec![
                self.append_single_record(records.into_iter().next().expect("non-empty records"))
                    .await?,
            ]);
        }
        let state = self.state.lock().await;
        let mut candidate = state.clone();
        let mut encoded_lines = Vec::with_capacity(records.len() * 256);
        let mut stamped_records = Vec::with_capacity(records.len());

        for record in records {
            let lane = record.lane().to_string();
            candidate.require_lane(&lane)?;
            candidate.validate_unused_id(record.id())?;
            if matches!(record, LaneRecord::OperationStarted(_)) {
                if let Some(current) = candidate.find_open_operations(&lane, Some(1))?.first() {
                    return Err(SessionError::storage(format!(
                        "Lane {} already has an open operation {}",
                        lane, current.base.id
                    )));
                }
            }
            let seq = candidate.next_sequence();
            let timestamp = self.clock.now_ms();
            let stamped = stamp_record(record, seq, lane, timestamp);
            let mutation = SessionMutation::Record { record: stamped };
            encode_mutation_into(&mutation, &mut encoded_lines);
            match candidate.apply_mutation(mutation)? {
                ApplyOutcome::Record(stamped) => stamped_records.push(stamped),
                _ => {
                    return Err(SessionError::storage(
                        "append_record_batch did not persist a record",
                    ));
                }
            }
        }
        drop(state);

        self.append_encoded_bytes(encoded_lines).await?;
        let mut state = self.state.lock().await;
        *state = candidate;
        Ok(stamped_records)
    }

    /// Append one mutation line to the file (no in-memory apply).
    async fn append_mutation_line(&self, mutation: &SessionMutation) -> SessionResult<()> {
        let mut encoded = Vec::with_capacity(256);
        encode_mutation_into(mutation, &mut encoded);
        self.append_encoded_bytes(encoded).await
    }
}

/// Return the start of a failed batch only when its complete or partial bytes
/// are an exact suffix beginning at a physical line boundary.
fn failed_append_start(bytes: &[u8], batch: &[u8]) -> Option<usize> {
    if bytes.ends_with(batch) {
        return Some(bytes.len() - batch.len());
    }

    // Search the longest matching suffix first. A failed multi-line write may
    // have committed one or more complete lines before tearing the next line;
    // choosing the shortest suffix would leave those committed-but-unreduced
    // lines in the journal.
    let window_start = bytes.len().saturating_sub(batch.len());
    let mut boundaries = vec![0];
    boundaries.extend(
        bytes
            .iter()
            .enumerate()
            .filter_map(|(i, b)| (*b == b'\n').then_some(i + 1)),
    );
    boundaries
        .into_iter()
        .filter(|&start| start >= window_start && start < bytes.len())
        .filter(|&start| batch.starts_with(&bytes[start..]))
        .min()
        .or_else(|| {
            // No bytes from this batch are present. A complete prior line is
            // the only safe proof that there is no foreign unterminated tail.
            (bytes.is_empty() || bytes.last() == Some(&b'\n')).then_some(bytes.len())
        })
}

/// Normalize an entry's parent onto the lane's current leaf before applying.
///
/// A valid log always satisfies `entry.parent_id == lane.leaf` (the validator
/// enforces it), so this is a no-op for well-formed sessions. When two writers
/// diverge, an entry can land with a parent the local state never reached;
/// re-chaining onto the leaf recovers the session instead of refusing to open
/// it. Non-entry mutations and entries without a named lane pass through.
fn chain_onto_lane_leaf(mutation: SessionMutation, state: &SessionState) -> SessionMutation {
    if let SessionMutation::Entry {
        seq,
        timestamp,
        lane,
        mut entry,
    } = mutation
    {
        if let Some(lane_name) = lane.clone() {
            if let Ok(leaf) = state.require_lane(&lane_name) {
                if entry.parent_id().map(str::to_string) != leaf {
                    entry.base_mut().parent_id = leaf;
                }
            }
        }
        SessionMutation::Entry {
            seq,
            timestamp,
            lane,
            entry,
        }
    } else {
        mutation
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
        let at = at.map(str::to_string);
        self.run_owned(move |s| async move { s.create_lane_owned(&lane, at.as_deref()).await })
            .await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let to = to.map(str::to_string);
        self.run_owned(move |s| async move { s.move_lane_owned(&lane, to.as_deref()).await })
            .await
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        let lane = lane.to_string();
        self.run_owned(move |s| async move { s.append_entry_owned(entry, &lane).await })
            .await
    }
    async fn append_record_and_entry(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        let lane = lane.to_string();
        self.run_owned(move |s| async move {
            s.append_record_and_entry_owned(record, entry, &lane).await
        })
        .await
    }
    async fn append_record_entry_and_record(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        let lane = lane.to_string();
        self.run_owned(move |s| async move {
            s.append_record_entry_and_record_owned(before, entry, &lane, after)
                .await
        })
        .await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.run_owned(move |s| async move { s.append_record_owned(record).await })
            .await
    }
    async fn append_records(&self, records: Vec<LaneRecord>) -> SessionResult<Vec<LaneRecord>> {
        self.run_owned(move |s| async move { s.append_records_owned(records).await })
            .await
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

    async fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        let state = self.state.lock().await;
        state.context_on_branch(bounds, start)
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
        let name = name.map(str::to_string);
        self.run_owned(move |s| async move { s.set_name_owned(name.as_deref()).await })
            .await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        let state = self.state.lock().await;
        Ok(state.get_label(id).map(|s| s.to_string()))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        let id = id.to_string();
        let label = label.map(str::to_string);
        self.run_owned(move |s| async move { s.set_label_owned(&id, label.as_deref()).await })
            .await
    }

    async fn get_stats(&self) -> SessionResult<SessionStats> {
        let state = self.state.lock().await;
        Ok(state.get_stats().clone())
    }

    /// Wait for the append that is in flight, if any, to settle.
    ///
    /// Every mutating method serialises through `self.tail` and holds it for the
    /// whole stamp -> persist -> apply sequence, so acquiring it here waits for a
    /// write that has already been admitted. `self.append` would not do: it is
    /// released once the bytes are handed to the writer, which happens before the
    /// caller's `await` on the append returns.
    async fn drain(&self) {
        let _reservation = self.tail.lock().await;
    }
}

impl JsonlSessionStorage {
    async fn create_lane_owned(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let at = at.map(|s| s.to_string());
        let state = self.state.lock().await;
        state.validate_new_lane(&lane)?;
        state.validate_target(at.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane {
            seq,
            lane: lane.clone(),
            leaf_id: at,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn move_lane_owned(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        let lane = lane.to_string();
        let to = to.map(|s| s.to_string());
        let state = self.state.lock().await;
        state.require_lane(&lane)?;
        state.validate_target(to.as_deref())?;
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane {
            seq,
            lane,
            leaf_id: to,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }

    async fn append_entry_owned(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<Entry> {
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
            entry: full,
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        match state.apply_mutation(mutation)? {
            ApplyOutcome::Entry(stamped) => Ok(stamped),
            _ => Err(SessionError::storage("append_entry did not persist")),
        }
    }

    async fn append_record_and_entry_owned(
        &self,
        record: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
    ) -> SessionResult<(LaneRecord, Entry)> {
        let state = self.state.lock().await;
        let record_lane = record.lane().to_string();
        state.require_lane(&record_lane)?;
        state.validate_unused_id(record.id())?;
        if matches!(record, LaneRecord::OperationStarted(_)) {
            if let Some(current) = state.find_open_operations(&record_lane, Some(1))?.first() {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation {}",
                    record_lane, current.base.id
                )));
            }
        }
        let parent_id = state.require_lane(lane)?;
        state.validate_unused_id(&entry.id)?;
        if entry.id == record.id() {
            return Err(SessionError::already_exists(format!(
                "Session id already exists: {}",
                entry.id
            )));
        }
        let record_seq = state.next_sequence();
        let stamped_record = stamp_record(record, record_seq, record_lane, self.clock.now_ms());
        let record_mutation = SessionMutation::Record {
            record: stamped_record,
        };
        let entry_timestamp = self.clock.now_ms();
        let entry_mutation = SessionMutation::Entry {
            seq: record_seq + 1,
            timestamp: entry_timestamp,
            lane: Some(lane.to_string()),
            entry: provisioned_into_entry(entry, record_seq + 1, parent_id, entry_timestamp),
        };
        let mut encoded = Vec::with_capacity(512);
        encode_mutation_into(&record_mutation, &mut encoded);
        encode_mutation_into(&entry_mutation, &mut encoded);
        drop(state);
        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        let committed_record = match state.apply_mutation(record_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("append_record did not persist")),
        };
        let committed_entry = match state.apply_mutation(entry_mutation)? {
            ApplyOutcome::Entry(stamped) => stamped,
            _ => return Err(SessionError::storage("append_entry did not persist")),
        };
        Ok((committed_record, committed_entry))
    }

    async fn append_record_entry_and_record_owned(
        &self,
        before: LaneRecord,
        entry: ProvisionedEntry,
        lane: &str,
        after: LaneRecord,
    ) -> SessionResult<(LaneRecord, Entry, LaneRecord)> {
        let state = self.state.lock().await;
        let before_lane = before.lane().to_string();
        let after_lane = after.lane().to_string();
        state.require_lane(&before_lane)?;
        state.require_lane(lane)?;
        state.require_lane(&after_lane)?;
        state.validate_unused_id(before.id())?;
        state.validate_unused_id(&entry.id)?;
        state.validate_unused_id(after.id())?;
        if before.id() == entry.id || before.id() == after.id() || entry.id == after.id() {
            return Err(SessionError::already_exists(
                "Session mutation ids must be distinct",
            ));
        }
        // Check only the touched lanes through borrowed state predicates. This
        // avoids cloning operation payloads for common non-start triples while
        // still modeling the before record before validating an after-start.
        let before_starts = matches!(before, LaneRecord::OperationStarted(_));
        if before_starts && state.has_open_operation_except(&before_lane, None) {
            return Err(SessionError::storage(format!(
                "Lane {} already has an open operation",
                before_lane
            )));
        }
        if matches!(after, LaneRecord::OperationStarted(_)) {
            let before_frees_matching = match &before {
                LaneRecord::OperationFinished(finished) if before_lane == after_lane => {
                    Some(finished.run_id.as_str())
                }
                _ => None,
            };
            let effective_open = if before_starts && before_lane == after_lane {
                true
            } else {
                state.has_open_operation_except(&after_lane, before_frees_matching)
            };
            if effective_open {
                return Err(SessionError::storage(format!(
                    "Lane {} already has an open operation",
                    after_lane
                )));
            }
        }
        let parent_id = state.require_lane(lane)?;
        let first_seq = state.next_sequence();
        let before_mutation = SessionMutation::Record {
            record: stamp_record(before, first_seq, before_lane, self.clock.now_ms()),
        };
        let entry_timestamp = self.clock.now_ms();
        let entry_mutation = SessionMutation::Entry {
            seq: first_seq + 1,
            timestamp: entry_timestamp,
            lane: Some(lane.to_string()),
            entry: provisioned_into_entry(entry, first_seq + 1, parent_id, entry_timestamp),
        };
        let after_mutation = SessionMutation::Record {
            record: stamp_record(after, first_seq + 2, after_lane, self.clock.now_ms()),
        };
        let mut encoded = Vec::with_capacity(768);
        encode_mutation_into(&before_mutation, &mut encoded);
        encode_mutation_into(&entry_mutation, &mut encoded);
        encode_mutation_into(&after_mutation, &mut encoded);
        drop(state);

        self.append_encoded_bytes(encoded).await?;
        let mut state = self.state.lock().await;
        let committed_before = match state.apply_mutation(before_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("before record did not persist")),
        };
        let committed_entry = match state.apply_mutation(entry_mutation)? {
            ApplyOutcome::Entry(stamped) => stamped,
            _ => return Err(SessionError::storage("entry did not persist")),
        };
        let committed_after = match state.apply_mutation(after_mutation)? {
            ApplyOutcome::Record(stamped) => stamped,
            _ => return Err(SessionError::storage("after record did not persist")),
        };
        Ok((committed_before, committed_entry, committed_after))
    }

    async fn append_record_owned(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.append_record_batch_owned(vec![record])
            .await?
            .pop()
            .ok_or_else(|| SessionError::storage("append_record returned no record"))
    }

    async fn append_records_owned(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        self.append_record_batch_owned(records).await
    }
    async fn set_name_owned(&self, name: Option<&str>) -> SessionResult<()> {
        let state = self.state.lock().await;
        let seq = state.next_sequence();
        let mutation = SessionMutation::FactName {
            seq,
            name: name.map(|s| s.to_string()),
        };
        drop(state);
        self.append_mutation_line(&mutation).await?;
        let mut state = self.state.lock().await;
        state.apply_mutation(mutation)?;
        Ok(())
    }
}

impl JsonlSessionStorage {
    async fn set_label_owned(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
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
}

/// Whether a raw JSONL line is an append of transient assistant-frame progress.
///
/// This intentionally only identifies the record envelope. The nested frame is
/// allowed to fail schema decoding; ordinary records must remain strict.
fn is_assistant_frame_append(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("kind").and_then(serde_json::Value::as_str) == Some("record")
        && value.get("type").and_then(serde_json::Value::as_str) == Some("assistant_frame")
        && value.get("op").and_then(serde_json::Value::as_str) == Some("append")
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
    F: for<'a> FnOnce(
        &'a Arc<dyn FileSystem>,
        &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SessionResult<()>> + Send + 'a>,
    >,
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
    use crate::session::types::{
        EntryOrder, OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind,
        RecordBase,
    };

    #[tokio::test]
    async fn context_projection_matches_jsonl_branch_query() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs,
            "/context.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        for (id, text) in [("e1", "old"), ("e2", "visible"), ("e3", "latest")] {
            storage
                .append_entry(
                    ProvisionedEntry {
                        id: id.into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new(text, 0).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        let bounds = BranchBounds {
            stop_at_type: Some("message".into()),
            ..Default::default()
        };
        let actual = storage.context_on_branch(&bounds, "e3").await.unwrap();
        let path = storage
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &bounds,
                "e3",
            )
            .await
            .unwrap();
        let expected = crate::session::context::build_session_context(&path, &Default::default());
        assert_eq!(actual.messages, expected.messages);
    }

    #[tokio::test]
    async fn append_returns_the_owned_payload_but_keeps_storage_defensive() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let jsonl = JsonlSessionStorage::create(
            fs,
            "/owned.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let memory = crate::session::memory::InMemorySessionStorage::new(
            SessionMetadata {
                id: "memory".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        );
        let storages: Vec<Arc<dyn SessionStorage>> = vec![Arc::new(jsonl), Arc::new(memory)];
        for storage in storages {
            let text = "owned append".repeat(4096);
            let original_pointer = text.as_ptr() as usize;
            let expected = text.clone();
            let mut returned = storage
                .append_entry(
                    ProvisionedEntry {
                        id: "entry".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new(text, 0).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            let Entry::Message(message) = &mut returned else {
                panic!("expected message")
            };
            let rpi_agent::AgentMessage::User(user) = &mut message.message else {
                panic!("expected user")
            };
            let rpi_ai::UserContent::Text(text) = &mut user.content else {
                panic!("expected text")
            };
            assert_eq!(text.as_ptr() as usize, original_pointer);
            text.clear();
            let Entry::Message(stored) = storage.get_entry("entry").await.unwrap().unwrap() else {
                panic!("expected stored message")
            };
            let rpi_agent::AgentMessage::User(user) = stored.message else {
                panic!("expected stored user")
            };
            assert_eq!(user.content, rpi_ai::UserContent::Text(expected));

            let payload = "owned frame".repeat(4096);
            let original_pointer = payload.as_ptr() as usize;
            let expected = payload.clone();
            let mut returned = storage
                .append_record(LaneRecord::AssistantFrame(
                    crate::session::types::AssistantFrameRecord {
                        base: RecordBase {
                            id: "frame".into(),
                            seq: 0,
                            lane: "main".into(),
                            timestamp: 0,
                        },
                        run_id: "run".into(),
                        stream_index: 0,
                        op: crate::session::types::AssistantFrameOp::Append,
                        frame: Some(serde_json::Value::String(payload)),
                    },
                ))
                .await
                .unwrap();
            let LaneRecord::AssistantFrame(frame) = &mut returned else {
                panic!("expected frame")
            };
            let Some(serde_json::Value::String(payload)) = &mut frame.frame else {
                panic!("expected frame payload")
            };
            assert_eq!(payload.as_ptr() as usize, original_pointer);
            payload.clear();
            let records = storage.find_records(&RecordQuery::default()).await.unwrap();
            let LaneRecord::AssistantFrame(frame) = &records[0] else {
                panic!("expected stored frame")
            };
            assert_eq!(frame.frame, Some(serde_json::Value::String(expected)));
        }
    }

    #[tokio::test]
    async fn append_records_batches_frame_records_with_contiguous_sequences() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs,
            "/batch.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let frame = |id: &str| {
            LaneRecord::AssistantFrame(crate::session::types::AssistantFrameRecord {
                base: RecordBase {
                    id: id.into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "run".into(),
                stream_index: 0,
                op: crate::session::types::AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"id": id})),
            })
        };
        let returned = storage
            .append_records(vec![frame("frame-1"), frame("frame-2")])
            .await
            .unwrap();
        assert_eq!(
            returned.iter().map(LaneRecord::seq).collect::<Vec<_>>(),
            [1, 2]
        );
        let stored = storage.find_records(&RecordQuery::default()).await.unwrap();
        assert_eq!(stored.len(), 2);
        let mut ids = stored.iter().map(|record| record.id()).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["frame-1", "frame-2"]);
    }

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
    async fn reusable_os_append_keeps_each_acknowledged_entry_visible_to_reload() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        for index in 0..3 {
            let id = format!("e{index}");
            storage
                .append_entry(
                    ProvisionedEntry {
                        id: id.clone(),
                        kind: ProvisionedKind::Message {
                            message: user_msg("visible 🐈"),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            let reader =
                JsonlSessionStorage::load(fs.clone(), "session.jsonl", clock.clone(), ids.clone())
                    .await
                    .unwrap();
            assert!(reader.get_entry(&id).await.unwrap().is_some());
            assert_eq!(
                reader
                    .find_entries(&EntryQuery::default())
                    .await
                    .unwrap()
                    .len(),
                index + 1
            );
        }
    }

    #[tokio::test]
    async fn cached_append_reopens_after_an_atomic_session_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let provisioned = |id: &str| ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        };
        storage
            .append_entry(provisioned("before"), "main")
            .await
            .unwrap();
        let prefix = fs.read_text_file("session.jsonl", None).await.unwrap();
        fs.write_file("replacement.tmp", FileContent::Text(prefix), None)
            .await
            .unwrap();
        fs.rename_file("replacement.tmp", "session.jsonl", None)
            .await
            .unwrap();
        storage
            .append_entry(provisioned("after"), "main")
            .await
            .unwrap();
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .unwrap();
        assert!(reloaded.get_entry("after").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cached_append_reopens_after_a_native_file_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let entry = |id: &str| ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        };
        storage
            .append_entry(entry("before-native"), "main")
            .await
            .unwrap();
        let path = temp.path().join("session.jsonl");
        let replacement = temp.path().join("replacement.tmp");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        // Deliberately bypass FileSystem: editors, plugins and subprocesses can
        // replace a path without updating any process-local generation counter.
        std::fs::rename(&replacement, &path).unwrap();
        storage
            .append_entry(entry("after-native"), "main")
            .await
            .unwrap();
        assert!(storage.get_entry("after-native").await.unwrap().is_some());
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .unwrap();
        assert!(
            reloaded.get_entry("after-native").await.unwrap().is_some(),
            "an acknowledged append must reach the current file"
        );
    }

    struct RefusingAppend(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for RefusingAppend {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl rpi_tools::env::AppendFile for RefusingAppend {
        async fn append(&mut self, _: &[u8]) -> Result<(), rpi_tools::FileError> {
            Err(rpi_tools::FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "injected append failure",
            ))
        }
    }

    #[tokio::test]
    async fn failed_append_does_not_commit_state_and_discards_the_failed_writer() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs,
            "/s.jsonl",
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let freed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        *storage.append.lock().await = Some(Box::new(RefusingAppend(Arc::clone(&freed))));
        let entry = ProvisionedEntry {
            id: "retry".into(),
            kind: ProvisionedKind::Message {
                message: user_msg("retry"),
                terminate: None,
            },
        };
        assert!(storage.append_entry(entry.clone(), "main").await.is_err());
        assert!(storage.get_entry("retry").await.unwrap().is_none());
        assert_eq!(freed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(storage.append_entry(entry, "main").await.unwrap().seq(), 1);
    }

    /// How a reusable append handle left the file when it reported failure.
    #[derive(Clone, Copy)]
    enum Tear {
        /// The first half of the line landed, then the write failed.
        Half,
        /// The whole line landed, but the write still reported failure.
        Whole,
        /// Bytes that are not a prefix of the line.
        Foreign,
    }

    /// A reusable append handle that writes part of a line and then fails, the
    /// way `write_all` does when the disk fills or an I/O error interrupts it.
    struct TearingAppend {
        fs: Arc<dyn FileSystem>,
        path: String,
        tear: Tear,
    }

    #[async_trait::async_trait]
    impl rpi_tools::env::AppendFile for TearingAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), rpi_tools::FileError> {
            let torn: &[u8] = match self.tear {
                Tear::Half => &bytes[..bytes.len() / 2],
                Tear::Whole => bytes,
                Tear::Foreign => b"XYZ",
            };
            self.fs
                .append_file(
                    &self.path,
                    FileContent::Text(String::from_utf8_lossy(torn).into_owned()),
                    None,
                )
                .await?;
            Err(rpi_tools::FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "injected partial write",
            ))
        }
    }

    fn provisioned_message(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        }
    }

    /// A session on a real directory, so the retry uses the production reusable
    /// append handle rather than the one-shot fallback.
    async fn session_on_disk(
        temp: &tempfile::TempDir,
    ) -> (
        Arc<dyn FileSystem>,
        JsonlSessionStorage,
        Arc<dyn Clock>,
        Arc<dyn IdGenerator>,
    ) {
        let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temp.path().to_path_buf(),
        ));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        (fs, storage, clock, ids)
    }

    async fn install_tearing_writer(
        storage: &JsonlSessionStorage,
        fs: &Arc<dyn FileSystem>,
        tear: Tear,
    ) {
        *storage.append.lock().await = Some(Box::new(TearingAppend {
            fs: fs.clone(),
            path: "session.jsonl".into(),
            tear,
        }));
    }

    #[tokio::test]
    async fn failed_append_that_tore_a_prefix_is_cut_back_before_the_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Half).await;
        assert!(storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .is_err());
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        // A later line stops the retry from being the physical last line, so a
        // fragment left in front of it would make the file unopenable.
        storage
            .append_entry(provisioned_message("later"), "main")
            .await
            .unwrap();
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads after a torn append and a retry");
        for id in ["kept", "retry", "later"] {
            assert!(
                reloaded.get_entry(id).await.unwrap().is_some(),
                "acknowledged entry {id} must survive reload"
            );
        }
    }

    #[test]
    fn failed_append_start_proves_only_the_fragment_of_its_own_line() {
        let line: &[u8] = b"{\"seq\":2}\n";
        let head: &[u8] = b"h\n{\"seq\":1}\n";
        // Nothing was written: the tail is clean and nothing needs cutting.
        assert_eq!(failed_append_start(head, line), Some(head.len()));
        // A fragment of this line after the last newline is this append's own.
        let torn = [head, b"{\"se"].concat();
        assert_eq!(failed_append_start(&torn, line), Some(head.len()));
        // A fragment that differs from the line is not provably this append's.
        let foreign = [head, b"{\"seq\":3"].concat();
        assert_eq!(failed_append_start(&foreign, line), None);
        let unknown = [head, b"XYZ"].concat();
        assert_eq!(failed_append_start(&unknown, line), None);
        // A whole copy of the line at the end is removed from its start.
        let whole = [head, line].concat();
        assert_eq!(failed_append_start(&whole, line), Some(head.len()));
        // Without a trailing newline, a complete line is still this append's.
        let unterminated = [head, &line[..line.len() - 1]].concat();
        assert_eq!(failed_append_start(&unterminated, line), Some(head.len()));
        assert_eq!(failed_append_start(b"", line), Some(0));
    }

    #[tokio::test]
    async fn acknowledged_retry_after_a_torn_prefix_survives_reload_when_nothing_follows() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        install_tearing_writer(&storage, &fs, Tear::Half).await;
        assert!(storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .is_err());
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        // The retry is the last physical line, so load's torn-tail repair would
        // drop the whole line, fragment and acknowledged retry together.
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(
            reloaded.get_entry("retry").await.unwrap().is_some(),
            "an acknowledged append must survive reload"
        );
    }

    #[tokio::test]
    async fn failed_append_that_landed_whole_is_removed_before_the_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Whole).await;
        assert!(storage
            .append_entry(provisioned_message("rejected"), "main")
            .await
            .is_err());
        // The caller may retry with different content. The rejected line must not
        // reappear on reload and shadow the acknowledged retry.
        storage
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("the retry is acknowledged");
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(reloaded.get_entry("retry").await.unwrap().is_some());
        assert!(
            reloaded.get_entry("rejected").await.unwrap().is_none(),
            "a rejected append must not be committed on reload"
        );
    }

    #[tokio::test]
    async fn failed_append_with_an_unknown_tail_stays_uncertain_until_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let (fs, storage, clock, ids) = session_on_disk(&temp).await;
        storage
            .append_entry(provisioned_message("kept"), "main")
            .await
            .unwrap();
        install_tearing_writer(&storage, &fs, Tear::Foreign).await;
        assert!(storage
            .append_entry(provisioned_message("rejected"), "main")
            .await
            .is_err());
        assert!(
            storage
                .append_entry(provisioned_message("retry"), "main")
                .await
                .is_err(),
            "writes must be refused while the tail cannot be proven to be this append's"
        );
        let reopened =
            JsonlSessionStorage::load(fs.clone(), "session.jsonl", clock.clone(), ids.clone())
                .await
                .expect("reopen repairs the unknown tail");
        reopened
            .append_entry(provisioned_message("retry"), "main")
            .await
            .expect("writes are accepted after reopen");
        let reloaded = JsonlSessionStorage::load(fs, "session.jsonl", clock, ids)
            .await
            .expect("the session reloads");
        assert!(reloaded.get_entry("kept").await.unwrap().is_some());
        assert!(reloaded.get_entry("retry").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn create_append_reload_roundtrips() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let entry = storage
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("hi"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(entry.id(), "e1");

        let reloaded = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .unwrap();
        let got = reloaded
            .get_entry("e1")
            .await
            .unwrap()
            .expect("entry exists");
        assert_eq!(got.id(), "e1");
    }

    #[tokio::test]
    async fn torn_tail_last_line_is_repaired() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // Manually append a valid entry line + a garbage (torn) last line.
        // The first mutation after the header is seq 1 (the header does not
        // consume a sequence number — see `create_append_reload_roundtrips`).
        let valid = encode_mutation(&SessionMutation::Lane {
            seq: 1,
            lane: "main".into(),
            leaf_id: None,
        });
        fs.append_file("/s.jsonl", FileContent::Text(valid), None)
            .await
            .unwrap();
        fs.append_file("/s.jsonl", FileContent::Text("{not json".into()), None)
            .await
            .unwrap();

        let reloaded = JsonlSessionStorage::load(fs.clone(), "/s.jsonl", clock, ids)
            .await
            .unwrap();
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

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // Garbage on line 2 (NOT last), then a valid line 3.
        fs.append_file("/s.jsonl", FileContent::Text("{not json\n".into()), None)
            .await
            .unwrap();
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(encode_mutation(&SessionMutation::Lane {
                seq: 1,
                lane: "main".into(),
                leaf_id: None,
            })),
            None,
        )
        .await
        .unwrap();

        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn undecodable_assistant_frame_does_not_block_load() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        JsonlSessionStorage::create(fs.clone(), "/s.jsonl", header(), clock.clone(), ids.clone())
            .await
            .unwrap();

        // The envelope is a valid assistant-frame append, but its nested frame
        // variant is not. This is recoverable progress loss, not history loss.
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(
                r#"{"kind":"record","type":"assistant_frame","id":"bad","seq":1,"lane":"main","timestamp":1,"runId":"run","streamIndex":0,"op":"append","frame":{"type":"not_a_real_frame_variant"}}
"#.into(),
            ),
            None,
        )
        .await
        .unwrap();
        fs.append_file(
            "/s.jsonl",
            FileContent::Text(encode_mutation(&SessionMutation::Lane {
                seq: 2,
                lane: "main".into(),
                leaf_id: None,
            })),
            None,
        )
        .await
        .unwrap();

        let reloaded = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .expect("an undecodable assistant frame must not block load");
        assert_eq!(reloaded.get_name().await.unwrap(), None);
        reloaded
            .append_entry(
                ProvisionedEntry {
                    id: "e1".into(),
                    kind: ProvisionedKind::Message {
                        message: user_msg("after recovery"),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .expect("session remains writable after skipping frame");
    }

    #[tokio::test]
    async fn schema_error_on_last_line_is_hard_corruption() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let _ = JsonlSessionStorage::create(
            fs.clone(),
            "/s.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        // A schema error (valid JSON, unknown kind) on the last line is NOT a
        // torn tail.
        fs.append_file(
            "/s.jsonl",
            FileContent::Text("{\"kind\":\"bogus\",\"seq\":2}\n".into()),
            None,
        )
        .await
        .unwrap();
        let err = JsonlSessionStorage::load(fs, "/s.jsonl", clock, ids)
            .await
            .err()
            .unwrap();
        assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    }

    #[tokio::test]
    async fn append_record_rejects_second_open_operation() {
        let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
        let fs: Arc<dyn FileSystem> = Arc::new(env);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());

        let storage = JsonlSessionStorage::create(fs, "/s.jsonl", header(), clock, ids)
            .await
            .unwrap();
        let started = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "op-1".into(),
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
        storage.append_record(started).await.unwrap();
        let second = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "op-2".into(),
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
        let err = storage.append_record(second).await.unwrap_err();
        assert!(err.to_string().contains("already has an open operation"));
    }

    struct ReviewWriteThenPause {
        fs: Arc<dyn FileSystem>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        pause_once: bool,
    }

    #[async_trait::async_trait]
    impl rpi_tools::env::AppendFile for ReviewWriteThenPause {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), rpi_tools::FileError> {
            self.fs
                .append_file("/review.jsonl", FileContent::Bytes(bytes.to_vec()), None)
                .await?;
            if self.pause_once {
                self.pause_once = false;
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(())
        }
    }

    fn review_entry(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: user_msg(id),
                terminate: None,
            },
        }
    }

    #[tokio::test]
    async fn review_cancelled_append_must_not_erase_the_next_acknowledged_entry() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let storage = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                "/review.jsonl",
                header(),
                clock.clone(),
                ids.clone(),
            )
            .await
            .unwrap(),
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *storage.append.lock().await = Some(Box::new(ReviewWriteThenPause {
            fs: fs.clone(),
            entered: entered.clone(),
            release: release.clone(),
            pause_once: true,
        }));
        let task = tokio::spawn({
            let storage = storage.clone();
            async move {
                storage
                    .append_entry(review_entry("cancelled"), "main")
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.notify_one();
        let acknowledged = storage
            .append_entry(review_entry("acknowledged"), "main")
            .await
            .unwrap();
        assert!(storage.get_entry("acknowledged").await.unwrap().is_some());
        let before = fs.read_text_file("/review.jsonl", None).await.unwrap();
        let reloaded = JsonlSessionStorage::load(fs.clone(), "/review.jsonl", clock, ids)
            .await
            .unwrap();
        let found = reloaded.get_entry("acknowledged").await.unwrap().is_some();
        eprintln!("review cancelled-append: acknowledged_seq={}, durable_lines_before_reload={}, acknowledged_survives={}, physical_file_after_reload={}", acknowledged.seq(), before.lines().count(), found, fs.read_text_file("/review.jsonl", None).await.unwrap().lines().count());
        assert!(
            found,
            "a successful append disappeared on reload after an earlier append future was aborted"
        );
    }

    #[tokio::test]
    async fn review_distinct_entries_with_a_sequence_collision_must_not_be_silently_deleted() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
        let first = JsonlSessionStorage::create(
            fs.clone(),
            "/review.jsonl",
            header(),
            clock.clone(),
            ids.clone(),
        )
        .await
        .unwrap();
        let second =
            JsonlSessionStorage::load(fs.clone(), "/review.jsonl", clock.clone(), ids.clone())
                .await
                .unwrap();
        let a = first
            .append_entry(review_entry("writer-one"), "main")
            .await
            .unwrap();
        let b = second
            .append_entry(review_entry("writer-two"), "main")
            .await
            .unwrap();
        let before = fs.read_text_file("/review.jsonl", None).await.unwrap();
        let error = match JsonlSessionStorage::load(fs.clone(), "/review.jsonl", clock, ids).await {
            Ok(_) => panic!("divergent same-seq mutation must be rejected"),
            Err(error) => error,
        };
        let after = fs.read_text_file("/review.jsonl", None).await.unwrap();
        eprintln!(
            "review divergent-writers: seqs=({}, {}), error={}, source_unchanged={}",
            a.seq(),
            b.seq(),
            error,
            before == after
        );
        assert!(error.to_string().contains("conflicting mutations"));
        assert_eq!(
            before, after,
            "conflict detection must not rewrite the source file"
        );
    }

    #[test]
    fn review_native_cancelled_append_must_keep_later_acknowledged_data() {
        // Occupy the sole blocking worker to cancel precisely after native I/O
        // is scheduled. This exercises the real OsAppendFile, not a fake writer.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(directory.path().to_path_buf()));
            let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
            let ids: Arc<dyn IdGenerator> = Arc::new(CounterIdGenerator::new());
            let storage = JsonlSessionStorage::create(fs.clone(), "native.jsonl", header(), clock.clone(), ids.clone()).await.unwrap();
            storage.append_entry(review_entry("native-seed"), "main").await.unwrap();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || { entered_tx.send(()).unwrap(); release_rx.recv().unwrap(); });
            entered_rx.await.unwrap();
            let mut interrupted = Box::pin(storage.append_entry(review_entry("native-cancelled"), "main"));
            assert!(futures::poll!(interrupted.as_mut()).is_pending());
            drop(interrupted);
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            let acknowledged = storage.append_entry(review_entry("native-acknowledged"), "main").await.unwrap();
            assert!(storage.get_entry("native-acknowledged").await.unwrap().is_some());
            let before = fs.read_text_file("native.jsonl", None).await.unwrap();
            let reloaded = JsonlSessionStorage::load(fs.clone(), "native.jsonl", clock, ids).await.unwrap();
            let found = reloaded.get_entry("native-acknowledged").await.unwrap().is_some();
            eprintln!("review native OS cancellation: acknowledged_seq={}, lines_before_reload={}, prior_cancelled_write_visible={}, acknowledged_survives_reload={}, lines_after_reload={}", acknowledged.seq(), before.lines().count(), before.contains("native-cancelled"), found, fs.read_text_file("native.jsonl", None).await.unwrap().lines().count());
            assert!(found, "the real native append writer persisted the cancelled write but its missing reducer commit caused a later successful entry to be dropped on reload");
        });
    }

    /// `drain` must not return while an admitted write is still in flight.
    ///
    /// Reuses the `ReviewWriteThenPause` handle' that the review regression
    /// installs: it appends the bytes and then parks, so the write is on the
    /// file but the append future has not resolved. `drain` therefore has to
    /// wait for the *append*, not for the bytes to be handed over.
    ///
    /// The pre-fix contract said "wait for all in-flight appends to settle" while
    /// the body took `self.append` — the lock the writer had already released
    /// before its `await` resolved. Taking `self.tail` is what actually waits,
    /// because every mutating method holds it across stamp -> persist -> apply.
    #[tokio::test]
    async fn drain_waits_for_an_in_flight_append() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                "/review.jsonl",
                header(),
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )
            .await
            .unwrap(),
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *storage.append.lock().await = Some(Box::new(ReviewWriteThenPause {
            fs: Arc::clone(&fs),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            pause_once: true,
        }));

        let writer = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.append_entry(review_entry("parked"), "main").await })
        };
        entered.notified().await;

        let drained =
            tokio::time::timeout(std::time::Duration::from_millis(50), storage.drain()).await;
        assert!(
            drained.is_err(),
            "drain returned while an admitted append was still in flight"
        );

        release.notify_one();
        writer.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), storage.drain())
            .await
            .expect("drain must complete once the append settled");
        assert!(storage.get_entry("parked").await.unwrap().is_some());
    }

    /// The wait must be reachable through the trait object, not just the type.
    ///
    /// The facade and the harness forwarding chain only ever hold
    /// `dyn SessionStorage`. A `drain` that existed only as an inherent method
    /// would leave the production path with nothing to call, which is exactly how
    /// the wait stayed dead code before.
    #[tokio::test]
    async fn drain_through_the_trait_object_waits_for_an_in_flight_append() {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let concrete = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                "/review-dyn.jsonl",
                header(),
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            )
            .await
            .unwrap(),
        );
        let storage: Arc<dyn SessionStorage> = Arc::clone(&concrete) as Arc<dyn SessionStorage>;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *concrete.append.lock().await = Some(Box::new(ReviewWriteThenPause {
            fs: Arc::clone(&fs),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            pause_once: true,
        }));

        let writer = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move {
                storage
                    .append_entry(review_entry("parked-dyn"), "main")
                    .await
            })
        };
        entered.notified().await;

        let drained =
            tokio::time::timeout(std::time::Duration::from_millis(50), storage.drain()).await;
        assert!(
            drained.is_err(),
            "the trait entry point returned while an append was still in flight"
        );

        release.notify_one();
        writer.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), storage.drain())
            .await
            .expect("drain must complete once the append settled");
    }
}
