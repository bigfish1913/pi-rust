//! Writer-ordered physical history synchronization; not request receipts.
use super::*;
use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
use crate::session::session::Session;
use rpi_tools::{FileError, FileErrorCode};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::session::types::ProvisionedKind;

fn header() -> JsonlV4Header {
    JsonlV4Header::new("sync-session".into(), 1, "/cwd".into(), None, None, None)
}
fn review_entry(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(id, 1)),
            terminate: None,
        },
    }
}
struct ReviewWriteThenPause {
    fs: Arc<dyn FileSystem>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    pause_once: bool,
}
#[async_trait::async_trait]
impl rpi_tools::env::AppendFile for ReviewWriteThenPause {
    async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        self.fs
            .append_file("history.jsonl", FileContent::Bytes(bytes.to_vec()), None)
            .await?;
        if self.pause_once {
            self.pause_once = false;
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

/// Only the strict barrier is intercepted. Legacy sync/lease/write methods
/// delegate to the real OS backend, so failures exercise the actual JSONL owner.
struct BarrierFs {
    inner: Arc<dyn FileSystem>,
    entered: Notify,
    release: Notify,
    pause: AtomicBool,
    fail: AtomicBool,
    calls: AtomicUsize,
}
impl BarrierFs {
    fn new(root: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(root.to_path_buf())),
            entered: Notify::new(),
            release: Notify::new(),
            pause: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        })
    }
}
macro_rules! barrier_fs {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        #[async_trait::async_trait]
        impl FileSystem for BarrierFs {
            fn cwd(&self) -> &std::path::Path { self.inner.cwd() }
            async fn cleanup(&self) { self.inner.cleanup().await; }
            async fn sync_file_and_parent(&self, path: &str, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                // The fault is after file sync, before directory sync. File-only
                // success must not manufacture a successful history identity.
                self.inner.sync_file(path, false, cancel).await?;
                self.entered.notify_one();
                if self.pause.load(Ordering::SeqCst) {
                    self.release.notified().await;
                }
                if self.fail.load(Ordering::SeqCst) {
                    return Err(FileError::new(FileErrorCode::Unknown, "injected directory sync failure"));
                }
                self.inner.sync_file_and_parent(path, cancel).await
            }
            $(async fn $name(&self, $($arg: $ty),*) -> Result<$ret, FileError> {
                self.inner.$name($($arg),*).await
            })*
        }
    };
}
barrier_fs! {
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

async fn storage(fs: Arc<dyn FileSystem>, path: &str) -> Arc<JsonlSessionStorage> {
    Arc::new(
        JsonlSessionStorage::create(
            fs,
            path,
            header(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap(),
    )
}
async fn blocked<T>(future: impl std::future::Future<Output = T>) {
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), future)
            .await
            .is_err()
    );
}
async fn entered(fs: &BarrierFs) {
    tokio::time::timeout(std::time::Duration::from_secs(5), fs.entered.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn physical_history_sync_covers_committed_identity_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let concrete = storage(fs.clone(), "history.jsonl").await;
    let dynamic: Arc<dyn SessionStorage> = concrete.clone();
    let session = Session::new(dynamic, Some(Arc::new(CounterIdGenerator::new())));
    assert_eq!(
        session.sync_history().await.unwrap(),
        HistoryIdentity {
            revision: 0,
            generation: 0
        }
    );
    concrete
        .append_entry(review_entry("accepted"), "main")
        .await
        .unwrap();
    let synced = session.sync_history().await.unwrap();
    assert_eq!(synced, concrete.history_identity().await.unwrap());
    assert_eq!(synced.revision, 1);
    concrete.release_writer_lease();
    let restored = JsonlSessionStorage::load(
        fs.inner.clone(),
        "history.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap();
    assert_eq!(restored.history_identity().await.unwrap(), synced);
    assert!(restored.get_entry("accepted").await.unwrap().is_some());
    assert_eq!(fs.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn physical_history_sync_waits_for_append_and_reducer_commit() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let concrete = storage(fs.clone(), "history.jsonl").await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    concrete.install_append_handle(Box::new(ReviewWriteThenPause {
        fs: fs.clone(),
        entered: entered.clone(),
        release: release.clone(),
        pause_once: true,
    }));
    let writer = {
        let s = concrete.clone();
        tokio::spawn(async move { s.append_entry(review_entry("pending"), "main").await })
    };
    entered.notified().await;
    let mut barrier = Box::pin(concrete.sync_history());
    blocked(barrier.as_mut()).await;
    assert_eq!(fs.calls.load(Ordering::SeqCst), 0);
    release.notify_one();
    writer.await.unwrap().unwrap();
    assert_eq!(barrier.await.unwrap().revision, 1);
}

#[tokio::test]
async fn physical_history_sync_cancelled_before_tail_admission_does_not_start_io() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    s.install_append_handle(Box::new(ReviewWriteThenPause {
        fs: fs.clone(),
        entered: entered.clone(),
        release: release.clone(),
        pause_once: true,
    }));
    let writer = {
        let s = s.clone();
        tokio::spawn(async move {
            s.append_entry(review_entry("before-admission"), "main")
                .await
        })
    };
    entered.notified().await;
    let mut sync = Box::pin(s.sync_history());
    assert!(futures::poll!(sync.as_mut()).is_pending());
    drop(sync);
    release.notify_one();
    writer.await.unwrap().unwrap();
    s.drain().await;
    assert_eq!(fs.calls.load(Ordering::SeqCst), 0);
    assert_eq!(s.history_revision().await.unwrap(), 1);
}

#[tokio::test]
async fn abandoned_physical_history_sync_keeps_owner_and_blocks_later_write() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    fs.pause.store(true, Ordering::SeqCst);
    let mut barrier = Box::pin(s.sync_history());
    // Poll on this thread until the detached owner reaches the sync boundary.
    tokio::select! {
        _ = entered(&fs) => {},
        result = &mut barrier => panic!("barrier returned early: {result:?}"),
    }
    drop(barrier);
    let mut next = Box::pin(s.append_entry(review_entry("later"), "main"));
    blocked(next.as_mut()).await;
    blocked(s.drain()).await;
    assert!(s.get_entry("later").await.unwrap().is_none());
    fs.release.notify_one();
    next.await.unwrap();
    s.drain().await;
    fs.pause.store(false, Ordering::SeqCst);
    assert_eq!(s.sync_history().await.unwrap().revision, 1);
}

#[tokio::test]
async fn physical_history_sync_failure_is_not_ack_or_rollback_and_can_be_retried() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    s.append_entry(review_entry("retained"), "main")
        .await
        .unwrap();
    let before = fs.read_binary_file("history.jsonl", None).await.unwrap();
    fs.fail.store(true, Ordering::SeqCst);
    assert!(s
        .sync_history()
        .await
        .unwrap_err()
        .to_string()
        .contains("injected directory sync failure"));
    assert_eq!(
        before,
        fs.read_binary_file("history.jsonl", None).await.unwrap()
    );
    assert!(s.get_entry("retained").await.unwrap().is_some());
    fs.fail.store(false, Ordering::SeqCst);
    assert_eq!(s.sync_history().await.unwrap().revision, 1);
}

#[tokio::test]
async fn physical_history_sync_refuses_uncertain_or_retired_bindings_without_io() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    s.uncertain.store(true, Ordering::SeqCst);
    assert!(s
        .sync_history()
        .await
        .unwrap_err()
        .to_string()
        .contains("unsettled append"));
    s.uncertain.store(false, Ordering::SeqCst);
    s.release_writer_lease();
    assert!(s
        .sync_history()
        .await
        .unwrap_err()
        .to_string()
        .contains("released"));
    assert_eq!(fs.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn physical_history_sync_refuses_lease_retired_during_sync() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    fs.pause.store(true, Ordering::SeqCst);
    let task = {
        let s = s.clone();
        tokio::spawn(async move { s.sync_history().await })
    };
    entered(&fs).await;
    s.release_writer_lease();
    fs.release.notify_one();
    assert!(task
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("released"));
}

#[tokio::test]
async fn physical_history_sync_serializes_history_replacement_and_reports_generation() {
    let root = tempfile::tempdir().unwrap();
    let fs = BarrierFs::new(root.path());
    let s = storage(fs.clone(), "history.jsonl").await;
    fs.pause.store(true, Ordering::SeqCst);
    let task = {
        let s = s.clone();
        tokio::spawn(async move { s.sync_history().await })
    };
    entered(&fs).await;
    let state = s.state.lock().await.clone().into_detached().await.unwrap();
    let mut replace = Box::pin(s.replace_history_if_revision_and_generation(
        0,
        0,
        HistoryReplacement {
            state,
            canonical_mutations: Some(Vec::new()),
        },
    ));
    blocked(replace.as_mut()).await;
    fs.release.notify_one();
    assert_eq!(
        task.await.unwrap().unwrap(),
        HistoryIdentity {
            revision: 0,
            generation: 0
        }
    );
    assert_eq!(
        replace.await.unwrap(),
        HistoryIdentity {
            revision: 0,
            generation: 1
        }
    );
    fs.pause.store(false, Ordering::SeqCst);
    assert_eq!(
        s.sync_history().await.unwrap(),
        HistoryIdentity {
            revision: 0,
            generation: 1
        }
    );
}

#[tokio::test]
async fn physical_history_sync_does_not_promote_memory_or_default_filesystem_noops() {
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    // Legacy syncs are compatible no-ops. The new strict primitive is not.
    fs.sync_file("/memory.jsonl", false, None).await.unwrap();
    fs.sync_directory("/", None).await.unwrap();
    assert_eq!(
        fs.sync_file_and_parent("/memory.jsonl", None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotSupported
    );
    let s = storage(fs, "/memory.jsonl").await;
    assert!(s
        .sync_history()
        .await
        .unwrap_err()
        .to_string()
        .contains("writer lease"));
    let memory: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    assert!(memory
        .sync_history()
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported"));
}
