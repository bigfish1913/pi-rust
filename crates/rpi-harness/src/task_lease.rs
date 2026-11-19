//! Durable task leases + terminal receipts for externally driven sessions.
//!
//! ## Why this is engine-owned
//!
//! A hosted-room driver (or any external scheduler) runs one *task* at a time in
//! a session: it needs a fencing token (a lease) so two drivers cannot prompt
//! the same session concurrently, and it needs a durable terminal receipt so a
//! driver that restarts can tell "finished / failed / unclear" apart instead of
//! guessing from message counts. rpi's session layer previously exposed only
//! run-level events; this module adds the missing task identity:
//!
//! - [`TaskIdentity`] = `task_id` + `execution_generation` (the fencing epoch);
//! - [`acquire`] takes a lease for one identity (conflict when another live
//!   lease exists; an **expired** lease is never revived in place — the caller
//!   must run [`recover`] or open a new generation);
//! - [`renew`] extends a live lease (expired leases fail with [`TaskLeaseError::Expired`]);
//! - [`finish`] records status + terminal receipt (persisted next to the
//!   session file: `<session>.tasks.json`, atomic write under an OS file lock);
//! - [`recover`] turns "running" records whose lease expired into
//!   `Indeterminate` and returns them — the honest outcome when the process
//!   died mid-turn;
//! - [`ensure_live`] verifies that a caller names the exact, **unexpired**
//!   identity before it acts (stale tokens are refused, never silently applied
//!   to another turn).
//!
//! Every mutating operation (acquire / renew / finish / recover) serializes the
//! whole read-modify-write under an advisory OS lock on
//! `<session>.tasks.json.lock`, so two drivers in different processes cannot
//! both pass the conflict check, and a stale writer cannot replace a newer
//! lease or receipt. Atomic rename alone is not a fencing guarantee.
//!
//! The module is storage + policy only; the caller still owns "run the turn".

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdentity {
    pub task_id: String,
    pub execution_generation: u64,
}

impl TaskIdentity {
    pub fn new(task_id: impl Into<String>, execution_generation: u64) -> Self {
        Self {
            task_id: task_id.into(),
            execution_generation,
        }
    }

    fn key(&self) -> String {
        format!("{}:{}", self.task_id, self.execution_generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Settled,
    Failed,
    Cancelled,
    Indeterminate,
}

impl TaskStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Settled => "settled",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLease {
    pub lease_id: String,
    pub task_id: String,
    pub execution_generation: u64,
    pub acquired_at: f64,
    pub expires_at: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub execution_generation: u64,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<TaskLease>,
    pub started_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<f64>,
    /// Terminal error text for Failed/Indeterminate records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Opaque terminal receipt (run id / outcome marker), written by finish().
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    /// Caller-owned payload carried with the record (e.g. a driver stores the
    /// room coordinates here so recovery can attribute an indeterminate task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskLeaseError {
    /// Another live lease holds this session for a different identity.
    Conflict {
        holder_task: String,
        holder_generation: u64,
    },
    /// The identity no longer matches what this session is running.
    Stale,
    /// The live lease's TTL has elapsed. The caller must recover the record
    /// (→ Indeterminate) or start a new execution_generation; an expired lease
    /// is never silently revived or allowed to renew/finish.
    Expired,
    /// The lease id does not match the live lease.
    LeaseLost,
    /// The task already reached a terminal state.
    AlreadyFinished(TaskStatus),
    /// Malformed identity (empty task id / zero generation).
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for TaskLeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskLeaseError::Conflict {
                holder_task,
                holder_generation,
            } => write!(
                f,
                "session is leased by task {holder_task} (generation {holder_generation})"
            ),
            TaskLeaseError::Stale => write!(f, "stale task identity (generation moved)"),
            TaskLeaseError::Expired => {
                write!(
                    f,
                    "task lease expired (recover the task or open a new generation)"
                )
            }
            TaskLeaseError::LeaseLost => write!(f, "task lease was lost (expired or replaced)"),
            TaskLeaseError::AlreadyFinished(status) => {
                write!(f, "task already finished ({})", status.as_wire())
            }
            TaskLeaseError::Invalid(message) => write!(f, "invalid task identity: {message}"),
            TaskLeaseError::Io(message) => write!(f, "task lease io error: {message}"),
        }
    }
}

impl std::error::Error for TaskLeaseError {}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn lease_path(session_path: &Path) -> PathBuf {
    let mut name = session_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".into());
    name.push_str(".tasks.json");
    session_path.with_file_name(name)
}

fn lease_lock_path(session_path: &Path) -> PathBuf {
    let mut name = session_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".into());
    name.push_str(".tasks.json.lock");
    session_path.with_file_name(name)
}

/// Unique temp name per write (pid + process-wide counter): two concurrent
/// writers must never share `<file>.tasks.json.tmp`, which would let them
/// clobber each other's bytes before the rename.
fn unique_temp_path(path: &Path) -> PathBuf {
    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "tasks.json".into());
    name.push_str(&format!(".{}.{}.tmp", std::process::id(), counter));
    path.with_file_name(name)
}

/// Advisory OS lock guard for one session's task document.
///
/// The lock file is the ownership capability (fs2 keeps an OS advisory lock on
/// the open handle); dropping the guard releases it. Every mutating operation
/// holds this for the whole read-modify-write, so cross-process writers
/// serialize instead of racing the conflict check.
struct LeaseGuard {
    file: File,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn lock_session(session_path: &Path) -> Result<LeaseGuard, TaskLeaseError> {
    let path = lease_lock_path(session_path);
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| TaskLeaseError::Io(format!("{}: {e}", path.display())))?;
    // Bounded wait: a competing writer holds the lock for one small JSON
    // rewrite; 2s is generous, and exceeding it is a genuine error (never a
    // silent un-serialized write).
    let deadline = now_secs() + 2.0;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(LeaseGuard { file }),
            Err(e) if lock_busy(&e) && now_secs() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(TaskLeaseError::Io(format!(
                    "task lease lock {} unavailable: {e}",
                    path.display()
                )))
            }
        }
    }
}

/// `fs2` reports a held lock as `WouldBlock` on Unix; Windows surfaces
/// `ERROR_LOCK_VIOLATION` (33) / `ERROR_SHARING_VIOLATION` (32) whose `kind()`
/// is not `WouldBlock`, so both are treated as "busy, retry".
fn lock_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskDocument {
    #[serde(default)]
    records: BTreeMap<String, TaskRecord>,
}

fn load(session_path: &Path) -> Result<TaskDocument, TaskLeaseError> {
    let path = lease_path(session_path);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<TaskDocument>(&text)
            .map_err(|e| TaskLeaseError::Io(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TaskDocument::default()),
        Err(e) => Err(TaskLeaseError::Io(format!("{}: {e}", path.display()))),
    }
}

fn store(session_path: &Path, document: &TaskDocument) -> Result<(), TaskLeaseError> {
    let path = lease_path(session_path);
    let text = serde_json::to_string_pretty(document)
        .map_err(|e| TaskLeaseError::Io(format!("serialize: {e}")))?;
    let tmp = unique_temp_path(&path);
    fs::write(&tmp, text).map_err(|e| TaskLeaseError::Io(format!("write: {e}")))?;
    fs::rename(&tmp, &path).map_err(|e| TaskLeaseError::Io(format!("rename: {e}")))?;
    Ok(())
}

fn validate(identity: &TaskIdentity) -> Result<(), TaskLeaseError> {
    if identity.task_id.trim().is_empty() {
        return Err(TaskLeaseError::Invalid("task_id is empty".into()));
    }
    if identity.execution_generation == 0 {
        return Err(TaskLeaseError::Invalid(
            "execution_generation must be >= 1".into(),
        ));
    }
    Ok(())
}

/// Take (or renew) the lease for `identity`. Returns the live lease.
///
/// - same identity + same lease id → renewal (extends expiry);
/// - another identity holding a live lease → [`TaskLeaseError::Conflict`];
/// - an expired lease -> [`TaskLeaseError::Expired`]; the caller must run
///   [`recover`] (retiring the record as `Indeterminate`) or open a new
///   execution_generation;
    session_path: &Path,
    identity: &TaskIdentity,
    lease_id: &str,
    ttl_secs: f64,
) -> Result<TaskLease, TaskLeaseError> {
    acquire_with_meta(session_path, identity, lease_id, ttl_secs, None)
}

/// [`acquire`] with a caller-owned metadata payload stored on the record.
pub fn acquire_with_meta(
    session_path: &Path,
    identity: &TaskIdentity,
    lease_id: &str,
    ttl_secs: f64,
    meta: Option<serde_json::Value>,
) -> Result<TaskLease, TaskLeaseError> {
    validate(identity)?;
    if lease_id.trim().is_empty() {
        return Err(TaskLeaseError::Invalid("lease_id is empty".into()));
    }
    // Serialize the whole read-modify-write: two processes must not both pass
    // the live-lease conflict check below.
    let _guard = lock_session(session_path)?;
    let mut document = load(session_path)?;
    let now = now_secs();
    let key = identity.key();

    // Any other live lease is a conflict regardless of key.
    for (other_key, record) in document.records.iter_mut() {
        if other_key == &key {
            continue;
        }
        if let Some(lease) = &record.lease {
            if record.status == TaskStatus::Running && lease.expires_at > now {
                return Err(TaskLeaseError::Conflict {
                    holder_task: lease.task_id.clone(),
                    holder_generation: lease.execution_generation,
                });
            }
        }
    }

    if let Some(record) = document.records.get(&key) {
        match record.status {
            TaskStatus::Running => {
                let Some(existing) = record.lease.clone() else {
                    return Err(TaskLeaseError::LeaseLost);
                };
                // An expired lease is never revived in place: the caller must
                // run recover() (→ Indeterminate) or open a new generation.
                if existing.expires_at <= now {
                    return Err(TaskLeaseError::Expired);
                }
                if existing.lease_id != lease_id {
                    return Err(TaskLeaseError::Conflict {
                        holder_task: existing.task_id.clone(),
                        holder_generation: existing.execution_generation,
                    });
                }
            }
            terminal => return Err(TaskLeaseError::AlreadyFinished(terminal)),
        }
    }

    let lease = TaskLease {
        lease_id: lease_id.to_owned(),
        task_id: identity.task_id.clone(),
        execution_generation: identity.execution_generation,
        acquired_at: now,
        expires_at: now + ttl_secs.max(1.0),
    };
    let record = document.records.entry(key).or_insert_with(|| TaskRecord {
        task_id: identity.task_id.clone(),
        execution_generation: identity.execution_generation,
        status: TaskStatus::Running,
        lease: None,
        started_at: now,
        finished_at: None,
        error: None,
        receipt: None,
        meta: None,
    });
    record.status = TaskStatus::Running;
    record.lease = Some(lease.clone());
    record.error = None;
    record.finished_at = None;
    if meta.is_some() {
        record.meta = meta;
    }
    store(session_path, &document)?;
    Ok(lease)
}

/// Renew a live lease. Fails when the lease was replaced/expired or the task is
/// no longer running.
pub fn renew(
    session_path: &Path,
    identity: &TaskIdentity,
    lease_id: &str,
    ttl_secs: f64,
) -> Result<TaskLease, TaskLeaseError> {
    validate(identity)?;
    let _guard = lock_session(session_path)?;
    let mut document = load(session_path)?;
    let key = identity.key();
    let now = now_secs();
    let record = document
        .records
        .get_mut(&key)
        .ok_or(TaskLeaseError::Stale)?;
    if record.status != TaskStatus::Running {
        return Err(TaskLeaseError::AlreadyFinished(record.status));
    }
    let Some(lease) = record.lease.as_mut() else {
        return Err(TaskLeaseError::LeaseLost);
    };
    if lease.lease_id != lease_id {
        return Err(TaskLeaseError::LeaseLost);
    }
    // Expiry is a fence: an expired lease must not be extendable (the driver
    // may already have lost the turn to another owner after recover()).
    if lease.expires_at <= now {
        return Err(TaskLeaseError::Expired);
    }
    lease.expires_at = now + ttl_secs.max(1.0);
    let out = lease.clone();
    store(session_path, &document)?;
    Ok(out)
}

/// Record a terminal status for the leased task. `status` must be terminal
/// (Settled / Failed / Cancelled); the lease id must match.
pub fn finish(
    session_path: &Path,
    identity: &TaskIdentity,
    lease_id: &str,
    status: TaskStatus,
    error: Option<&str>,
    receipt: Option<&str>,
) -> Result<TaskRecord, TaskLeaseError> {
    validate(identity)?;
    match status {
        TaskStatus::Settled | TaskStatus::Failed | TaskStatus::Cancelled => {}
        other => {
            return Err(TaskLeaseError::Invalid(format!(
                "finish() requires a terminal status, got {}",
                other.as_wire()
            )))
        }
    }
    let _guard = lock_session(session_path)?;
    let mut document = load(session_path)?;
    let key = identity.key();
    let record = document
        .records
        .get_mut(&key)
        .ok_or(TaskLeaseError::Stale)?;
    if record.status != TaskStatus::Running {
        return Err(TaskLeaseError::AlreadyFinished(record.status));
    }
    let Some(lease) = record.lease.as_ref() else {
        return Err(TaskLeaseError::LeaseLost);
    };
    if lease.lease_id != lease_id {
        return Err(TaskLeaseError::LeaseLost);
    }
    if lease.expires_at <= now_secs() {
        return Err(TaskLeaseError::Expired);
    }
    record.status = status;
    record.lease = None;
    record.finished_at = Some(now_secs());
    record.error = error.map(str::to_owned);
    record.receipt = receipt.map(str::to_owned);
    let out = record.clone();
    store(session_path, &document)?;
    Ok(out)
}

/// Read one task record.
pub fn status(
    session_path: &Path,
    identity: &TaskIdentity,
) -> Result<Option<TaskRecord>, TaskLeaseError> {
    validate(identity)?;
    Ok(load(session_path)?.records.get(&identity.key()).cloned())
}

/// All task records (newest generation last).
pub fn list(session_path: &Path) -> Result<Vec<TaskRecord>, TaskLeaseError> {
    Ok(load(session_path)?.records.values().cloned().collect())
}

/// Retire expired running leases as `Indeterminate` and return the records that
/// were retired. A driver calls this on startup before planning new work.
pub fn recover(session_path: &Path) -> Result<Vec<TaskRecord>, TaskLeaseError> {
    let _guard = lock_session(session_path)?;
    let mut document = load(session_path)?;
    let now = now_secs();
    let mut retired = Vec::new();
    for record in document.records.values_mut() {
        if record.status != TaskStatus::Running {
            continue;
        }
        let expired = record
            .lease
            .as_ref()
            .map(|lease| lease.expires_at <= now)
            .unwrap_or(true);
        if expired {
            record.status = TaskStatus::Indeterminate;
            record.lease = None;
            record.finished_at = Some(now);
            if record.error.is_none() {
                record.error = Some("driver restarted while the task was running".into());
            }
            retired.push(record.clone());
        }
    }
    if !retired.is_empty() {
        store(session_path, &document)?;
    }
    Ok(retired)
}

/// Fencing check for interrupts: returns Ok(()) only when `identity` is the
/// live **unexpired** running task. A stale or expired token is refused —
/// never applied to another turn.
pub fn ensure_live(
    session_path: &Path,
    identity: &TaskIdentity,
    lease_id: Option<&str>,
) -> Result<TaskRecord, TaskLeaseError> {
    validate(identity)?;
    let document = load(session_path)?;
    let record = document
        .records
        .get(&identity.key())
        .cloned()
        .ok_or(TaskLeaseError::Stale)?;
    if record.status != TaskStatus::Running {
        return Err(TaskLeaseError::AlreadyFinished(record.status));
    }
    let Some(lease) = record.lease.as_ref() else {
        return Err(TaskLeaseError::LeaseLost);
    };
    if lease.expires_at <= now_secs() {
        return Err(TaskLeaseError::Expired);
    }
    if let Some(expected) = lease_id {
        if expected != lease.lease_id {
            return Err(TaskLeaseError::LeaseLost);
        }
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_file() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("s.jsonl")
    }

    #[test]
    fn acquire_conflicts_with_a_live_lease() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap();
        let error = acquire(&p, &TaskIdentity::new("t2", 1), "lease-b", 60.0).unwrap_err();
        assert!(matches!(error, TaskLeaseError::Conflict { .. }));
    }

    #[test]
    fn same_identity_renews_with_same_lease() {
        let dir = session_file();
        let p = path(&dir);
        let first = acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap();
        let renewed = renew(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap();
        assert_eq!(renewed.lease_id, first.lease_id);
        assert!(renewed.expires_at >= first.expires_at);
    }

    #[test]
    fn finish_requires_matching_lease_and_sets_receipt() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap();
        let bad = finish(
            &p,
            &TaskIdentity::new("t1", 1),
            "lease-b",
            TaskStatus::Settled,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(bad, TaskLeaseError::LeaseLost);
        let done = finish(
            &p,
            &TaskIdentity::new("t1", 1),
            "lease-a",
            TaskStatus::Settled,
            None,
            Some("run-1"),
        )
        .unwrap();
        assert_eq!(done.status, TaskStatus::Settled);
        assert_eq!(done.receipt.as_deref(), Some("run-1"));
        let again = finish(
            &p,
            &TaskIdentity::new("t1", 1),
            "lease-a",
            TaskStatus::Failed,
            Some("x"),
            None,
        )
        .unwrap_err();
        assert!(matches!(again, TaskLeaseError::AlreadyFinished(_)));
    }

    #[test]
    fn recover_marks_expired_running_tasks_indeterminate() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let retired = recover(&p).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].status, TaskStatus::Indeterminate);
        assert!(retired[0].error.is_some());
        // A fresh generation may now run.
        acquire(&p, &TaskIdentity::new("t1", 2), "lease-b", 60.0).unwrap();
    }

    #[test]
    fn ensure_live_fences_stale_generations() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap();
        ensure_live(&p, &TaskIdentity::new("t1", 1), Some("lease-a")).unwrap();
        let stale = ensure_live(&p, &TaskIdentity::new("t1", 2), Some("lease-a")).unwrap_err();
        assert_eq!(stale, TaskLeaseError::Stale);
        let lost = ensure_live(&p, &TaskIdentity::new("t1", 1), Some("lease-b")).unwrap_err();
        assert_eq!(lost, TaskLeaseError::LeaseLost);
    }

    #[test]
    fn meta_roundtrips_and_survives_recovery() {
        let dir = session_file();
        let p = path(&dir);
        let meta = serde_json::json!({ "room_id": "r1", "member_id": "m1" });
        acquire_with_meta(
            &p,
            &TaskIdentity::new("t1", 1),
            "lease-a",
            1.0,
            Some(meta.clone()),
        )
        .unwrap();
        let record = status(&p, &TaskIdentity::new("t1", 1)).unwrap().unwrap();
        assert_eq!(record.meta.as_ref(), Some(&meta));
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let retired = recover(&p).unwrap();
        assert_eq!(retired[0].meta.as_ref(), Some(&meta));
    }

    /// Expiry fences renewal: after the TTL a stale driver cannot extend the
    /// lease (reviewer blocker #1).
    #[test]
    fn renew_after_expiry_fails() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let error = renew(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap_err();
        assert_eq!(error, TaskLeaseError::Expired);
    }

    /// Expiry fences finishing: an expired lease cannot record a terminal
    /// receipt as the live owner.
    #[test]
    fn finish_after_expiry_fails() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let error = finish(
            &p,
            &TaskIdentity::new("t1", 1),
            "lease-a",
            TaskStatus::Settled,
            None,
            Some("r"),
        )
        .unwrap_err();
        assert_eq!(error, TaskLeaseError::Expired);
    }

    /// Expiry fences the interrupt path: `ensure_live` must not bless an
    /// expired lease.
    #[test]
    fn ensure_live_after_expiry_fails() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let error = ensure_live(&p, &TaskIdentity::new("t1", 1), Some("lease-a")).unwrap_err();
        assert_eq!(error, TaskLeaseError::Expired);
    }

    /// Same-identity acquire does not revive an expired lease in place; the
    /// caller must recover first or open a new generation.
    #[test]
    fn same_identity_acquire_does_not_revive_expired_lease() {
        let dir = session_file();
        let p = path(&dir);
        acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 1.0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let error = acquire(&p, &TaskIdentity::new("t1", 1), "lease-a", 60.0).unwrap_err();
        assert_eq!(error, TaskLeaseError::Expired);
        // After recover() marks it indeterminate, a new generation may run.
        let retired = recover(&p).unwrap();
        assert_eq!(retired.len(), 1);
        acquire(&p, &TaskIdentity::new("t1", 2), "lease-b", 60.0).unwrap();
    }

    /// Concurrent writers serialize on the task-document lock: exactly one of
    /// N racing acquires wins, the rest get a conflict (never an un-serialized
    /// double lease) (reviewer blocker #2).
    #[test]
    fn concurrent_acquires_yield_exactly_one_lease() {
        let dir = session_file();
        let p = path(&dir);
        // Seed the document so the file exists before the race; the acquire
        // TTL floor is 1s, so wait past it before the racers start.
        acquire(&p, &TaskIdentity::new("seed", 1), "lease-seed", 0.05).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for index in 0..8usize {
            let session = p.clone();
            let gate = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                gate.wait();
                acquire(
                    &session,
                    &TaskIdentity::new(format!("race-{index}"), 1),
                    &format!("lease-race-{index}"),
                    60.0,
                )
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let winners = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(winners, 1, "exactly one concurrent acquire may win");
        assert!(results
            .iter()
            .filter(|result| result.is_err())
            .all(|result| matches!(result, Err(TaskLeaseError::Conflict { .. }))));
    }
}
