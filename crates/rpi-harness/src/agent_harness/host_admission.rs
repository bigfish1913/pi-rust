//! Request-linked native input admission.
//!
//! An ordinary Harness prompt is persisted by the run loop, but a caller has no
//! way to learn *whether its exact input became part of the native journal*
//! before the model runs. This module adds that boundary without inventing a
//! second journal:
//!
//! 1. The caller supplies a request identity and the exact prompts.
//! 2. The harness appends one native custom entry containing the request and
//!    its messages under the ordinary writer gate, using the request id as the
//!    entry id (so retries are idempotent and a duplicate is detectable).
//! 3. `Session::sync_history` proves the file and its parent directory reached
//!    an explicit physical barrier and returns the covered identity.
//!
//! Only then does the caller receive a [`HostAdmissionReceipt`]. The receipt
//! proves durable input custody, **not** execution: a later run records
//! `operation_started` with `source_leaf_id` equal to the admission entry, and
//! [`HostAdmissionExecution`] reports whether that structural link exists.
//! Nothing in this module replays or resumes admitted work; crash recovery is
//! an observation, and a decision to rerun belongs to the Host operator.
//!
//! The admission entry is a custom entry with no context projector, so it never
//! enters the model context. Its payload is bounded independently of the
//! ordinary prompt budget.

use super::*;
use crate::durable::{validate_identity, DurableError};
use crate::session::jsonl::{sha256_bytes, sha256_hex};
use crate::session::types::{
    Entry, EntryOrder, EntryQuery, HistoryIdentity, OperationIntent, ProvisionedEntry,
    ProvisionedKind, RecordQuery,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Native custom-entry type. Reserved Host namespace, never a model message.
pub const HOST_ADMISSION_TYPE: &str = "rpi.host.admission.v1";
const FORMAT: &str = "rpi-host-admission";
const VERSION: u32 = 2;
// The unreleased v1 record used SHA256(SHA256(serialized messages)). Preserve
// its receipt identity on restore; all new records use ordinary SHA-256.
const LEGACY_VERSION: u32 = 1;

/// Bound on the exact messages carried by one admission record. This is
/// deliberately smaller than the ordinary prompt budget: the record duplicates
/// the input inside the same journal so a crash cannot lose it.
pub const MAX_HOST_ADMISSION_BYTES: usize = 512 * 1024;
/// Maximum messages carried by one admission.
pub const MAX_HOST_ADMISSION_MESSAGES: usize = 64;
/// Maximum admissions enumerated by one reconciliation listing. Targeted
/// per-request lookups are not limited by this.
pub const MAX_HOST_ADMISSIONS: usize = 64;

fn invalid(message: &str) -> DurableError {
    DurableError::InvalidJournal(format!("native host admission: {message}"))
}
fn read_error(error: impl std::fmt::Display) -> DurableError {
    DurableError::ReadFailure(error.to_string())
}

/// The operation whose input is being admitted. Only prompt input is covered;
/// resume/compaction operate on already-durable history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum HostAdmissionKind {
    Prompt {},
}

/// Explicit claim about one request's input. `request_id` must be a non-nil UUID
/// and is also the native entry id, so the same request cannot be admitted
/// twice with different content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostAdmissionRequest {
    pub request_id: String,
    pub host_id: String,
    pub agent_id: String,
    pub kind: HostAdmissionKind,
    pub messages: Vec<AgentMessage>,
}

/// Proof that the exact input was committed and synced at `history`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostAdmissionReceipt {
    pub request_id: String,
    pub host_id: String,
    pub agent_id: String,
    pub kind: HostAdmissionKind,
    /// Hex SHA-256 over the serialized admitted messages (v2 records). Legacy
    /// v1 receipts retain their original double-SHA256 fingerprint on restore.
    pub digest: String,
    pub message_count: usize,
    pub message_bytes: usize,
    pub entry_id: String,
    pub entry_seq: u64,
    /// Identity covered by the physical sync. Later writes may advance it.
    pub history: HistoryIdentity,
}

/// Result of one admission attempt. `replayed` means the exact claim was
/// already durable; a caller must not execute the same request id again.
#[derive(Clone, Debug, PartialEq)]
pub struct HostAdmissionOutcome {
    pub receipt: HostAdmissionReceipt,
    pub replayed: bool,
}

/// Whether a native run structurally claimed this admitted input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum HostAdmissionExecution {
    /// No `operation_started` names the admission entry as its source leaf.
    /// The input is durable, but no execution is proven (and none is replayed).
    Admitted {},
    /// The immediately following native operation started from the admission
    /// entry and carried these exact prompts as the suffix of its input.
    Started {
        #[serde(rename = "runId")]
        run_id: String,
        finished: bool,
    },
}

/// Reconciliation observation for one admitted request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostAdmissionStatus {
    pub receipt: HostAdmissionReceipt,
    pub execution: HostAdmissionExecution,
}

/// The persisted record. `admitted` is the identity *before* the admission
/// append; the receipt identity is `revision + 1` at the same generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionRecord {
    format: String,
    version: u32,
    request: HostAdmissionRequest,
    digest: String,
    message_count: usize,
    message_bytes: usize,
    admitted: HistoryIdentity,
}

/// Serialized message digest. Used for both the record and run-link checks.
fn message_digest(messages: &[AgentMessage]) -> Result<(String, usize), DurableError> {
    versioned_digest(messages, VERSION)
}

fn versioned_digest(
    messages: &[AgentMessage],
    version: u32,
) -> Result<(String, usize), DurableError> {
    let bytes = serde_json::to_vec(messages)
        .map_err(|e| DurableError::InvalidRequest(format!("messages are not serializable: {e}")))?;
    let digest = match version {
        LEGACY_VERSION => sha256_hex(sha256_bytes(&bytes)),
        VERSION => sha256_hex(&bytes),
        _ => return Err(DurableError::UnsupportedJournalFormat),
    };
    Ok((digest, bytes.len()))
}

fn validate_request(request: &HostAdmissionRequest) -> Result<usize, DurableError> {
    validate_identity(&request.request_id)?;
    validate_identity(&request.host_id)?;
    validate_identity(&request.agent_id)?;
    let id = Uuid::parse_str(&request.request_id).map_err(|_| {
        // The request id doubles as the native entry id, so it must be a
        // non-nil UUID to keep retries exact and collisions implausible.
        DurableError::InvalidRequest("host admissions require a non-nil UUID request id".into())
    })?;
    if id.is_nil() {
        return Err(DurableError::InvalidRequest(
            "host admissions require a non-nil UUID request id".into(),
        ));
    }
    if request.messages.is_empty() {
        return Err(DurableError::InvalidRequest(
            "host admission requires at least one message".into(),
        ));
    }
    if request.messages.len() > MAX_HOST_ADMISSION_MESSAGES {
        return Err(DurableError::LimitExceeded);
    }
    let (_, bytes) = message_digest(&request.messages)?;
    if bytes > MAX_HOST_ADMISSION_BYTES {
        return Err(DurableError::LimitExceeded);
    }
    Ok(bytes)
}

/// Hex digest of the exact messages an admission claim would persist.
pub fn host_admission_digest(messages: &[AgentMessage]) -> Result<String, DurableError> {
    message_digest(messages).map(|(digest, _)| digest)
}

/// Validate a caller-supplied admission claim without writing anything.
pub fn validate_host_admission(request: &HostAdmissionRequest) -> Result<(), DurableError> {
    validate_request(request).map(|_| ())
}

/// Reject a record that is not the exact shape this version wrote.
fn validate_record(record: &AdmissionRecord) -> Result<(), DurableError> {
    if record.format != FORMAT || !matches!(record.version, LEGACY_VERSION | VERSION) {
        return Err(DurableError::UnsupportedJournalFormat);
    }
    let bytes = validate_request(&record.request)?;
    let (digest, _) = versioned_digest(&record.request.messages, record.version)?;
    if record.message_count != record.request.messages.len()
        || record.message_bytes != bytes
        || record.digest != digest
    {
        return Err(invalid("record digest or message bounds disagree"));
    }
    Ok(())
}

fn decode(entry: &Entry) -> Result<AdmissionRecord, DurableError> {
    let Entry::Custom(custom) = entry else {
        return Err(invalid("admission entry is not a custom entry"));
    };
    if custom.custom_type != HOST_ADMISSION_TYPE {
        return Err(invalid("admission entry has a foreign custom type"));
    }
    let data = custom
        .data
        .clone()
        .ok_or_else(|| invalid("admission entry has no payload"))?;
    let record: AdmissionRecord = serde_json::from_value(data)
        .map_err(|_| invalid("admission payload has an unsupported shape"))?;
    validate_record(&record)?;
    if entry.id() != record.request.request_id {
        return Err(invalid("admission entry id differs from its request id"));
    }
    Ok(record)
}

fn receipt(entry: &Entry, record: &AdmissionRecord) -> Result<HostAdmissionReceipt, DurableError> {
    let expected_seq = record
        .admitted
        .revision
        .checked_add(1)
        .ok_or(DurableError::LimitExceeded)?;
    // The entry is stamped with the reducer revision it created. Anything else
    // means the record was moved onto a different history image.
    if entry.seq() != expected_seq {
        return Err(invalid(
            "admission entry sequence disagrees with its record",
        ));
    }
    Ok(HostAdmissionReceipt {
        request_id: record.request.request_id.clone(),
        host_id: record.request.host_id.clone(),
        agent_id: record.request.agent_id.clone(),
        kind: record.request.kind,
        digest: record.digest.clone(),
        message_count: record.message_count,
        message_bytes: record.message_bytes,
        entry_id: entry.id().to_string(),
        entry_seq: entry.seq(),
        history: HistoryIdentity {
            revision: expected_seq,
            generation: record.admitted.generation,
        },
    })
}

/// Does `original` end with exactly the admitted messages? The run loop may
/// prepend queued next-run messages, so the admitted client input is a suffix.
fn claims_prompt(
    original: &[AgentMessage],
    admitted: &[AgentMessage],
    digest: &str,
    version: u32,
) -> bool {
    if original.len() < admitted.len() {
        return false;
    }
    let Some(suffix) = original.get(original.len() - admitted.len()..) else {
        return false;
    };
    versioned_digest(suffix, version).is_ok_and(|(actual, _)| actual == digest)
}

impl AgentHarness {
    /// Append and physically sync the exact input under an explicit request
    /// identity. Returns only after a history sync covered the admission entry.
    ///
    /// Idempotent for the same request id and identical content; conflicting
    /// content is rejected. A failed or uncertain append never yields a receipt.
    pub async fn admit_host_request(
        &self,
        request: HostAdmissionRequest,
    ) -> Result<HostAdmissionOutcome, DurableError> {
        self.admit_host_request_inner(request, true)
            .await?
            .ok_or_else(|| invalid("new admission was unexpectedly absent"))
    }

    /// Re-prove an existing identical admission under the native writer gate.
    /// Never appends absent input or starts execution. Safe while a native run
    /// is active; callers must not use entry visibility as a substitute for this
    /// physical barrier after an uncertain/abandoned admission attempt.
    pub async fn resync_host_admission(
        &self,
        request: HostAdmissionRequest,
    ) -> Result<Option<HostAdmissionOutcome>, DurableError> {
        self.admit_host_request_inner(request, false).await
    }

    async fn admit_host_request_inner(
        &self,
        request: HostAdmissionRequest,
        allow_new: bool,
    ) -> Result<Option<HostAdmissionOutcome>, DurableError> {
        let message_bytes = validate_request(&request)?;
        let message_count = request.messages.len();
        let (digest, _) = message_digest(&request.messages)?;
        let _writes = self.message_writes.lock().await;
        self.durable_check_owner()?;
        // A run may have started between the owner check and this gate.
        self.durable_check_owner()?;
        let lane = self.lane.clone();
        let session = self.session.pin(Arc::clone(self.session.id_generator()));
        session
            .with_mutation(|target| async move {
                // An existing entry with this id is either our own prior
                // admission (idempotent retry) or a collision.
                let existing = target
                    .find_entries(&EntryQuery {
                        entry_id: Some(request.request_id.clone()),
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await
                    .map_err(read_error)?;
                if let Some(entry) = existing.first() {
                    let record = decode(entry)?;
                    if record.request != request {
                        return Err(DurableError::Conflict);
                    }
                    // The original append may have failed after the bytes were
                    // visible. A retry must re-prove the physical boundary, not
                    // reuse the first call's abandoned sync.
                    let after = target
                        .sync_history()
                        .await
                        .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
                    if after.generation != record.admitted.generation
                        || after.revision < entry.seq()
                    {
                        return Err(DurableError::CommitUncertain(
                            "history was replaced after the admission was recorded".into(),
                        ));
                    }
                    return Ok(Some(HostAdmissionOutcome {
                        receipt: receipt(entry, &record)?,
                        replayed: true,
                    }));
                }
                if !allow_new {
                    return Ok(None);
                }
                let before = target
                    .storage()
                    .history_identity()
                    .await
                    .map_err(read_error)?;
                let record = AdmissionRecord {
                    format: FORMAT.into(),
                    version: VERSION,
                    request,
                    digest,
                    message_count,
                    message_bytes,
                    admitted: before,
                };
                let data = serde_json::to_value(&record)
                    .map_err(|_| invalid("admission payload is not serializable"))?;
                let entry = target
                    .append_entry_if_revision(
                        ProvisionedEntry {
                            id: record.request.request_id.clone(),
                            kind: ProvisionedKind::Custom {
                                custom_type: HOST_ADMISSION_TYPE.into(),
                                data: Some(data),
                            },
                        },
                        &lane,
                        before.revision,
                    )
                    .await
                    .map_err(|error| {
                        // A definite CAS/duplicate rejection is not an uncertain
                        // commit; IO/persistence errors must not be retried blindly.
                        let text = error.to_string();
                        if text.contains("conflict") || text.contains("already") {
                            DurableError::Conflict
                        } else {
                            DurableError::CommitUncertain(text)
                        }
                    })?;
                let after = target
                    .sync_history()
                    .await
                    .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
                if after.generation != before.generation
                    || after.revision
                        != before
                            .revision
                            .checked_add(1)
                            .ok_or(DurableError::LimitExceeded)?
                    || entry.seq() != after.revision
                {
                    return Err(DurableError::CommitUncertain(
                        "history changed while the admission was being synchronized".into(),
                    ));
                }
                Ok(Some(HostAdmissionOutcome {
                    receipt: receipt(&entry, &record)?,
                    replayed: false,
                }))
            })
            .await
    }

    /// Read one request's admission and execution observation. Before returning
    /// a receipt, re-prove a strict physical barrier over its pinned history.
    /// No entry is appended and no execution is started by this observation.
    pub async fn host_admission(
        &self,
        request_id: &str,
    ) -> Result<Option<HostAdmissionStatus>, DurableError> {
        validate_identity(request_id)?;
        self.durable_check_owner()?;
        let session = self.session.pin(Arc::clone(self.session.id_generator()));
        session
            .with_mutation(|session| async move {
                let entries = session
                    .find_entries(&EntryQuery {
                        entry_id: Some(request_id.into()),
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await
                    .map_err(read_error)?;
                let Some(entry) = entries.first() else {
                    return Ok(None);
                };
                let covered = session
                    .sync_history()
                    .await
                    .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
                self.host_admission_status(&session, entry, covered)
                    .await
                    .map(Some)
            })
            .await
    }

    /// Bounded enumeration for crash reconciliation. Fails instead of silently
    /// truncating; per-request lookups remain available above the limit.
    pub async fn host_admissions(&self) -> Result<Vec<HostAdmissionStatus>, DurableError> {
        self.durable_check_owner()?;
        let session = self.session.pin(Arc::clone(self.session.id_generator()));
        session
            .with_mutation(|session| async move {
                let entries = session
                    .find_entries(&EntryQuery {
                        custom_type: Some(HOST_ADMISSION_TYPE.into()),
                        order: Some(EntryOrder::OldestFirst),
                        limit: Some(MAX_HOST_ADMISSIONS + 1),
                        ..Default::default()
                    })
                    .await
                    .map_err(read_error)?;
                if entries.len() > MAX_HOST_ADMISSIONS {
                    return Err(DurableError::LimitExceeded);
                }
                if entries.is_empty() {
                    return Ok(Vec::new());
                }
                // One barrier covers the entire bounded snapshot. No entry is
                // appended and no admitted prompt is replayed by observation.
                let covered = session
                    .sync_history()
                    .await
                    .map_err(|error| DurableError::CommitUncertain(error.to_string()))?;
                let mut seen = std::collections::HashSet::new();
                let mut statuses = Vec::with_capacity(entries.len());
                for entry in &entries {
                    let status = self.host_admission_status(&session, entry, covered).await?;
                    if !seen.insert(status.receipt.request_id.clone()) {
                        return Err(invalid("duplicate request id in admission records"));
                    }
                    statuses.push(status);
                }
                Ok(statuses)
            })
            .await
    }

    async fn host_admission_status(
        &self,
        session: &Session,
        entry: &Entry,
        covered: HistoryIdentity,
    ) -> Result<HostAdmissionStatus, DurableError> {
        let record = decode(entry)?;
        if covered.generation != record.admitted.generation || covered.revision < entry.seq() {
            return Err(DurableError::CommitUncertain(
                "history was replaced after the admission was recorded".into(),
            ));
        }
        // Only the first native operation after admission is eligible. Do not
        // search later starts for a convenient match: navigation can reuse an
        // old leaf, which must not retroactively claim this admitted request.
        let starts = session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                record_type: Some("operation_started"),
                after_seq: Some(entry.seq()),
                order: Some(EntryOrder::OldestFirst),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(read_error)?;
        let execution = match starts.first() {
            Some(LaneRecord::OperationStarted(started))
                if started.source_leaf_id.as_deref() == Some(entry.id())
                    && matches!(&started.intent, OperationIntent::Run { original_prompt, .. }
                        if claims_prompt(original_prompt, &record.request.messages, &record.digest, record.version)) =>
            {
                let finished = session
                    .find_records(&RecordQuery {
                        lane: Some(self.lane.clone()),
                        record_type: Some("operation_finished"),
                        run_id: Some(started.base.id.clone()),
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await
                    .map_err(read_error)?
                    .iter()
                    .any(|record| {
                        matches!(record, LaneRecord::OperationFinished(done)
                            if done.run_id == started.base.id)
                    });
                HostAdmissionExecution::Started {
                    run_id: started.base.id.clone(),
                    finished,
                }
            }
            Some(LaneRecord::OperationStarted(_)) => HostAdmissionExecution::Admitted {},
            Some(_) => return Err(invalid("record query returned a non-start record")),
            None => HostAdmissionExecution::Admitted {},
        };
        Ok(HostAdmissionStatus {
            receipt: receipt(entry, &record)?,
            execution,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
    use crate::session::memory::SystemClock;
    use crate::session::session::{DefaultIdGenerator, Session};
    use crate::session::types::{
        OperationFinishedRecord, OperationOutcome, OperationStartedRecord, RecordBase,
        SessionStorage,
    };
    use rpi_ai::{Api, Model};
    use rpi_tools::env::{FileContent, ReadFile, WriterLease};
    use rpi_tools::{FileError, FileSystem, OsExecutionEnv};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_util::sync::CancellationToken;

    fn header() -> JsonlV4Header {
        JsonlV4Header::new("admission-session".into(), 1, "/".into(), None, None, None)
    }

    struct Fixture {
        _root: tempfile::TempDir,
        fs: Arc<dyn FileSystem>,
        storage: Arc<JsonlSessionStorage>,
        path: String,
    }
    impl Fixture {
        async fn new() -> Self {
            Self::with_wrapper(Arc::new(SyncGate::new(false))).await
        }
        async fn with_wrapper(wrapper: Arc<SyncGate>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(
                root.path().canonicalize().unwrap(),
            ));
            let wrapper: Arc<dyn FileSystem> = Arc::new(SyncGateFs {
                inner: fs,
                gate: wrapper,
            });
            let path = "session.jsonl";
            let storage = Arc::new(
                JsonlSessionStorage::create(
                    wrapper.clone(),
                    path,
                    header(),
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            );
            Self {
                _root: root,
                fs: wrapper,
                storage,
                path: path.into(),
            }
        }
        fn options(&self) -> AgentHarnessOptions {
            AgentHarnessOptions {
                model: Model::new("m", "m", Api::Faux, "faux", "http://x"),
                session: Session::new(self.storage.clone(), None),
                allow_existing_session: true,
                require_native_durable: true,
                ..Default::default()
            }
        }
        async fn harness(&self) -> AgentHarness {
            AgentHarness::create(self.options()).await.unwrap()
        }
        async fn reopen(&self) -> AgentHarness {
            self.storage.drain().await;
            self.storage.release_writer_lease();
            let loaded = JsonlSessionStorage::load(
                self.fs.clone(),
                &self.path,
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap();
            AgentHarness::create(AgentHarnessOptions {
                session: Session::new(Arc::new(loaded), None),
                allow_existing_session: true,
                require_native_durable: true,
                ..self.options()
            })
            .await
            .unwrap()
        }
    }

    /// Only the strict barrier is intercepted; every other call is the real
    /// OS backend, so admission failures exercise the production writer queue.
    struct SyncGate {
        fail: AtomicBool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl SyncGate {
        fn new(fail: bool) -> Self {
            Self {
                fail: AtomicBool::new(fail),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    struct SyncGateFs {
        inner: Arc<dyn FileSystem>,
        gate: Arc<SyncGate>,
    }
    macro_rules! delegate {
        ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
            #[async_trait::async_trait]
            impl FileSystem for SyncGateFs {
                fn cwd(&self) -> &std::path::Path { self.inner.cwd() }
                async fn cleanup(&self) { self.inner.cleanup().await; }
                async fn sync_file_and_parent(&self, path: &str, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                    self.gate.calls.fetch_add(1, Ordering::SeqCst);
                    if self.gate.fail.load(Ordering::SeqCst) {
                        // The file sync succeeds; only the directory barrier
                        // fails, so the append is visible but unproven.
                        self.inner.sync_file(path, false, cancel).await?;
                        return Err(FileError::new(rpi_tools::FileErrorCode::Unknown, "injected admission sync failure"));
                    }
                    self.inner.sync_file_and_parent(path, cancel).await
                }
                $(async fn $name(&self, $($arg: $ty),*) -> Result<$ret, FileError> {
                    self.inner.$name($($arg),*).await
                })*
            }
        };
    }
    delegate! {
        absolute_path(path: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
        join_path(parts: &[&str], cancel: Option<&CancellationToken>) -> std::path::PathBuf;
        read_text_file(path: &str, cancel: Option<&CancellationToken>) -> String;
        read_text_lines(path: &str, max_lines: Option<usize>, cancel: Option<&CancellationToken>) -> Vec<String>;
        read_binary_file(path: &str, cancel: Option<&CancellationToken>) -> Vec<u8>;
        write_file(path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> ();
        append_file(path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> ();
        open_append_file(path: &str) -> Option<Box<dyn rpi_tools::env::AppendFile>>;
        open_read_file(path: &str) -> Option<Arc<dyn ReadFile>>;
        try_acquire_writer_lease(path: &str) -> Option<Box<dyn WriterLease>>;
        sync_file(path: &str, data_only: bool, cancel: Option<&CancellationToken>) -> ();
        sync_directory(path: &str, cancel: Option<&CancellationToken>) -> ();
        truncate_file(path: &str, len: u64, cancel: Option<&CancellationToken>) -> ();
        rename_file(source: &str, dest: &str, cancel: Option<&CancellationToken>) -> ();
        file_info(path: &str, cancel: Option<&CancellationToken>) -> rpi_tools::env::FileInfo;
        list_dir(path: &str, cancel: Option<&CancellationToken>) -> Vec<rpi_tools::env::FileInfo>;
        canonical_path(path: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
        exists(path: &str, cancel: Option<&CancellationToken>) -> bool;
        create_dir(path: &str, recursive: bool, cancel: Option<&CancellationToken>) -> ();
        remove(path: &str, recursive: bool, force: bool, cancel: Option<&CancellationToken>) -> ();
        create_temp_dir(prefix: Option<&str>, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
        create_temp_file(prefix: &str, suffix: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
    }

    fn request(text: &str) -> HostAdmissionRequest {
        HostAdmissionRequest {
            request_id: Uuid::now_v7().to_string(),
            host_id: "host-1".into(),
            agent_id: "agent-1".into(),
            kind: HostAdmissionKind::Prompt {},
            messages: vec![rpi_ai::UserMessage::new(text, 1).into()],
        }
    }

    fn started(run_id: &str, source: &str, prompt: Vec<AgentMessage>) -> LaneRecord {
        LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: run_id.into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: Some(source.into()),
            intent: OperationIntent::Run {
                original_prompt: prompt,
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        })
    }

    fn finished(run_id: &str) -> LaneRecord {
        LaneRecord::OperationFinished(OperationFinishedRecord {
            base: RecordBase {
                id: format!("{run_id}-finished"),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            run_id: run_id.into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        })
    }

    #[tokio::test]
    async fn admission_is_idempotent_durable_and_never_claims_execution() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let request = request("exact input");
        let receipt = harness
            .admit_host_request(request.clone())
            .await
            .unwrap()
            .receipt;
        assert_eq!(receipt.request_id, request.request_id);
        assert_eq!(receipt.entry_id, request.request_id);
        assert_eq!(receipt.entry_seq, receipt.history.revision);
        assert_eq!(receipt.history.generation, 0);
        assert_eq!(receipt.message_count, 1);
        assert!(receipt.message_bytes > 0);
        assert_eq!(receipt.digest.len(), 64);
        let identity = fixture.storage.history_identity().await.unwrap();
        assert_eq!(identity, receipt.history);

        // The same claim returns the same proof without a second entry.
        let retried = harness.admit_host_request(request.clone()).await.unwrap();
        assert!(retried.replayed);
        assert_eq!(retried.receipt, receipt);
        assert_eq!(
            fixture.storage.history_identity().await.unwrap(),
            receipt.history
        );

        // Different content under a used request id is a conflict, not an alias.
        let mut conflicting = request.clone();
        conflicting.messages = vec![rpi_ai::UserMessage::new("different", 1).into()];
        assert!(matches!(
            harness.admit_host_request(conflicting).await,
            Err(DurableError::Conflict)
        ));
        assert_eq!(
            fixture.storage.history_identity().await.unwrap(),
            receipt.history
        );

        let status = harness
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .expect("admission is readable");
        assert_eq!(status.receipt, receipt);
        assert_eq!(status.execution, HostAdmissionExecution::Admitted {});
        assert!(harness
            .host_admission(&Uuid::now_v7().to_string())
            .await
            .unwrap()
            .is_none());
        assert_eq!(harness.host_admissions().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn admission_rejects_invalid_bounds_without_writing() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let before = fixture.storage.history_identity().await.unwrap();
        let mut nil = request("nil");
        nil.request_id = Uuid::nil().to_string();
        let mut not_uuid = request("not-uuid");
        not_uuid.request_id = "request-1".into();
        let mut empty = request("empty");
        empty.messages.clear();
        let mut too_many = request("many");
        too_many.messages = (0..=MAX_HOST_ADMISSION_MESSAGES)
            .map(|i| rpi_ai::UserMessage::new(format!("m{i}"), 1).into())
            .collect();
        let mut too_large = request("large");
        too_large.messages =
            vec![rpi_ai::UserMessage::new("x".repeat(MAX_HOST_ADMISSION_BYTES + 1), 1).into()];
        for (request, expected) in [
            (nil, Some("non-nil UUID")),
            (not_uuid, Some("non-nil UUID")),
            (empty, Some("at least one message")),
            (too_many, None),
            (too_large, None),
        ] {
            let error = harness.admit_host_request(request).await.unwrap_err();
            assert!(
                matches!(
                    error,
                    DurableError::LimitExceeded | DurableError::InvalidRequest(_)
                ),
                "{error}"
            );
            if let Some(expected) = expected {
                assert!(error.to_string().contains(expected));
            }
        }
        assert_eq!(
            fixture.storage.history_identity().await.unwrap(),
            before,
            "rejected admissions must not write"
        );
        assert!(harness.host_admissions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn admission_links_the_first_native_operation_and_reconciles_after_reopen() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let request = request("linked input");
        let receipt = harness
            .admit_host_request(request.clone())
            .await
            .unwrap()
            .receipt;
        let status = harness
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.execution, HostAdmissionExecution::Admitted {});

        harness
            .session
            .append_record(started(
                "run-1",
                &receipt.entry_id,
                request.messages.clone(),
            ))
            .await
            .unwrap();
        let status = harness
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.execution,
            HostAdmissionExecution::Started {
                run_id: "run-1".into(),
                finished: false,
            }
        );
        harness
            .session
            .append_record(finished("run-1"))
            .await
            .unwrap();
        let before = fixture.storage.history_identity().await.unwrap();

        // Crash/reopen: the same journal answers with the same proof and link.
        let reopened = fixture.reopen().await;
        let status = reopened
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.receipt, receipt);
        assert_eq!(
            status.execution,
            HostAdmissionExecution::Started {
                run_id: "run-1".into(),
                finished: true,
            }
        );
        assert!(fixture.storage.history_identity().await.unwrap().revision >= before.revision);
        let listed = reopened.host_admissions().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].receipt, receipt);
    }

    #[tokio::test]
    async fn queued_next_run_prefix_still_links_the_admitted_suffix() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let request = request("client prompt");
        let receipt = harness
            .admit_host_request(request.clone())
            .await
            .unwrap()
            .receipt;
        let queued: AgentMessage = rpi_ai::UserMessage::new("queued next run", 1).into();
        let mut original = vec![queued];
        original.extend(request.messages.iter().cloned());
        harness
            .session
            .append_record(started("run-queued", &receipt.entry_id, original))
            .await
            .unwrap();
        let status = harness
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            status.execution,
            HostAdmissionExecution::Started { ref run_id, finished: false } if run_id == "run-queued"
        ));
    }

    #[tokio::test]
    async fn an_unrelated_operation_does_not_claim_an_admitted_prompt() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let request = request("never started");
        let receipt = harness
            .admit_host_request(request.clone())
            .await
            .unwrap()
            .receipt;
        harness
            .session
            .append_record(started("run-other", "some-other-leaf", vec![]))
            .await
            .unwrap();
        harness
            .session
            .append_record(finished("run-other"))
            .await
            .unwrap();
        // A later branch navigation may reuse the old leaf and prompts. It
        // cannot retroactively make the first unrelated start a valid claim.
        harness
            .session
            .append_record(started(
                "later-reused-leaf",
                &receipt.entry_id,
                request.messages,
            ))
            .await
            .unwrap();
        let status = harness
            .host_admission(&receipt.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.execution, HostAdmissionExecution::Admitted {});
    }

    #[tokio::test]
    async fn admission_record_never_enters_the_model_context() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let receipt = harness
            .admit_host_request(request("private admission"))
            .await
            .unwrap()
            .receipt;
        let entry = harness
            .session
            .find_entries(&EntryQuery {
                entry_id: Some(receipt.entry_id.clone()),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap()
            .pop()
            .unwrap();
        let mut options = crate::session::context::SessionContextBuildOptions::default();
        options.entry_projectors.insert(
            HOST_ADMISSION_TYPE.into(),
            Arc::new(|_, _, _| panic!("native admission must not invoke an extension projector")),
        );
        let context = crate::session::context::build_session_context_owned(vec![entry], &options);
        assert!(context.messages.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_admissions_serialize_contiguous_sequences() {
        let fixture = Fixture::new().await;
        let harness = Arc::new(fixture.harness().await);
        let mut tasks = Vec::new();
        for i in 0..8 {
            let harness = harness.clone();
            tasks.push(tokio::spawn(async move {
                harness
                    .admit_host_request(request(&format!("concurrent-{i}")))
                    .await
            }));
        }
        let mut receipts = Vec::new();
        for task in tasks {
            receipts.push(task.await.unwrap().unwrap().receipt);
        }
        receipts.sort_by_key(|receipt| receipt.entry_seq);
        assert_eq!(
            receipts.iter().map(|r| r.entry_seq).collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert!(receipts.iter().all(|r| r.history.generation == 0));
        let listed = harness.host_admissions().await.unwrap();
        assert_eq!(listed.len(), 8);
        let mut seqs: Vec<_> = listed.iter().map(|s| s.receipt.entry_seq).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn failed_sync_is_uncertain_and_a_retry_reproves_the_boundary() {
        let gate = Arc::new(SyncGate::new(true));
        let fixture = Fixture::with_wrapper(gate.clone()).await;
        let harness = fixture.harness().await;
        let request = request("uncertain input");
        let error = harness
            .admit_host_request(request.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, DurableError::CommitUncertain(_)), "{error}");
        assert!(gate.calls.load(Ordering::SeqCst) >= 1);
        // Entry visibility is not proof. Receipt observations must also fail
        // closed until a strict barrier can cover the persisted claim.
        assert_eq!(
            fixture
                .storage
                .find_entries(&EntryQuery {
                    entry_id: Some(request.request_id.clone()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1,
        );
        assert!(matches!(
            harness.host_admission(&request.request_id).await,
            Err(DurableError::CommitUncertain(_))
        ));
        assert!(matches!(
            harness.host_admissions().await,
            Err(DurableError::CommitUncertain(_))
        ));
        // A retry while the barrier still fails must not reuse the abandoned
        // sync and turn visible bytes into a receipt.
        assert!(matches!(
            harness.admit_host_request(request.clone()).await,
            Err(DurableError::CommitUncertain(_))
        ));
        // Clearing the fault lets the identical request prove the boundary.
        gate.fail.store(false, Ordering::SeqCst);
        let receipt = harness.admit_host_request(request).await.unwrap().receipt;
        assert_eq!(receipt.entry_seq, 1);
        assert_eq!(harness.host_admissions().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn public_custom_apis_cannot_mint_host_admission_authority() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let before = fixture.storage.history_identity().await.unwrap();
        for custom_type in [HOST_ADMISSION_TYPE, "rpi.host.admission.future"] {
            assert!(harness
                .append_custom_entry(custom_type, None)
                .await
                .is_err());
            assert!(harness
                .session
                .view("main")
                .append_custom_entry(custom_type, None)
                .await
                .is_err());
        }
        assert_eq!(fixture.storage.history_identity().await.unwrap(), before);
        // Native admission uses the private conditional append, not these APIs.
        harness
            .admit_host_request(request("native authority"))
            .await
            .unwrap();
    }

    #[test]
    fn digest_is_standard_sha256_not_a_second_hash_of_the_digest() {
        // SHA-256 known vector for the exact serialized empty array, independently
        // calculated (not generated through the helper under test).
        assert_eq!(
            host_admission_digest(&[]).unwrap(),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
        let messages = vec![rpi_ai::UserMessage::new("known", 1).into()];
        assert_eq!(
            serde_json::to_string(&messages).unwrap(),
            r#"[{"kind":"user","role":"user","content":"known","timestamp":1}]"#,
        );
        assert_eq!(
            host_admission_digest(&messages).unwrap(),
            "5f80e607476034a853e3ba60baa5e05277d57b1f1ecb6a4a8f579b5ca2338d0c",
        );
        assert_ne!(
            host_admission_digest(&[]).unwrap(),
            versioned_digest(&[], LEGACY_VERSION).unwrap().0
        );
    }

    #[tokio::test]
    async fn resync_never_admits_an_absent_or_conflicting_request() {
        let gate = Arc::new(SyncGate::new(false));
        let fixture = Fixture::with_wrapper(gate.clone()).await;
        let harness = fixture.harness().await;
        let request = request("resync only");
        let before = fixture.storage.history_identity().await.unwrap();
        assert!(harness
            .resync_host_admission(request.clone())
            .await
            .unwrap()
            .is_none());
        assert_eq!(fixture.storage.history_identity().await.unwrap(), before);
        assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
        let receipt = harness
            .admit_host_request(request.clone())
            .await
            .unwrap()
            .receipt;
        gate.fail.store(true, Ordering::SeqCst);
        let mut conflict = request.clone();
        conflict.messages = vec![rpi_ai::UserMessage::new("not the same input", 1).into()];
        assert!(matches!(
            harness.resync_host_admission(conflict).await,
            Err(DurableError::Conflict)
        ));
        assert!(matches!(
            harness.resync_host_admission(request.clone()).await,
            Err(DurableError::CommitUncertain(_))
        ));
        gate.fail.store(false, Ordering::SeqCst);
        let retry = harness
            .resync_host_admission(request)
            .await
            .unwrap()
            .unwrap();
        assert!(retry.replayed);
        assert_eq!(retry.receipt, receipt);
        assert_eq!(
            fixture.storage.history_identity().await.unwrap(),
            receipt.history
        );
    }

    #[tokio::test]
    async fn legacy_digest_receipt_and_execution_link_survive_reopen_and_retry() {
        let fixture = Fixture::new().await;
        let harness = fixture.harness().await;
        let request = request("legacy recorded input");
        let (digest, message_bytes) = versioned_digest(&request.messages, LEGACY_VERSION).unwrap();
        let before = fixture.storage.history_identity().await.unwrap();
        let record = AdmissionRecord {
            format: FORMAT.into(),
            version: LEGACY_VERSION,
            request: request.clone(),
            digest: digest.clone(),
            message_count: request.messages.len(),
            message_bytes,
            admitted: before,
        };
        let entry = harness
            .session
            .append_entry_if_revision(
                ProvisionedEntry {
                    id: request.request_id.clone(),
                    kind: ProvisionedKind::Custom {
                        custom_type: HOST_ADMISSION_TYPE.into(),
                        data: Some(serde_json::to_value(&record).unwrap()),
                    },
                },
                "main",
                before.revision,
            )
            .await
            .unwrap();
        harness.session.sync_history().await.unwrap();
        harness
            .session
            .append_record(started("legacy-run", entry.id(), request.messages.clone()))
            .await
            .unwrap();
        harness
            .session
            .append_record(finished("legacy-run"))
            .await
            .unwrap();
        let reopened = fixture.reopen().await;
        let status = reopened
            .host_admission(&request.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.receipt.digest, digest);
        assert_ne!(
            status.receipt.digest,
            host_admission_digest(&request.messages).unwrap()
        );
        assert!(matches!(
            status.execution,
            HostAdmissionExecution::Started { finished: true, .. }
        ));
        let after = reopened.session.storage().history_identity().await.unwrap();
        let retried = reopened
            .resync_host_admission(request)
            .await
            .unwrap()
            .unwrap();
        assert!(retried.replayed);
        assert_eq!(retried.receipt, status.receipt);
        assert_eq!(
            reopened.session.storage().history_identity().await.unwrap(),
            after
        );
    }
}
