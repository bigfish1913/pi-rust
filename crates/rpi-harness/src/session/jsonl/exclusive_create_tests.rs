//! Exclusive new-Session publication: no truncating fallback or orphan adoption.
use super::*;
use crate::session::memory::{CounterIdGenerator, FakeClock};
use crate::session::types::ProvisionedKind;
use rpi_tools::{FileError, FileErrorCode, InMemoryExecutionEnv, OsExecutionEnv};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
const PATH: &str = "new.jsonl";
const PROGRESS: &str = "new.jsonl.progress.jsonl";
fn header(id: &str) -> JsonlV4Header {
    JsonlV4Header::new(id.into(), 1, "/workspace".into(), None, None, None)
}
async fn create(fs: Arc<dyn FileSystem>, id: &str) -> SessionResult<JsonlSessionStorage> {
    JsonlSessionStorage::create_new(
        fs,
        PATH,
        header(id),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
}
async fn load(fs: Arc<dyn FileSystem>) -> SessionResult<JsonlSessionStorage> {
    JsonlSessionStorage::load(
        fs,
        PATH,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
}
fn os(root: &std::path::Path) -> Arc<dyn FileSystem> {
    Arc::new(OsExecutionEnv::with_cwd(root.to_path_buf()))
}
struct Fault {
    reject_new: AtomicBool,
    race_primary: AtomicBool,
    fail_primary_sync: AtomicBool,
    fail_companion_sync: AtomicBool,
    fail_header: AtomicBool,
    block_companion: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
    creates: AtomicUsize,
}
impl Fault {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reject_new: AtomicBool::new(false),
            race_primary: AtomicBool::new(false),
            fail_primary_sync: AtomicBool::new(false),
            fail_companion_sync: AtomicBool::new(false),
            fail_header: AtomicBool::new(false),
            block_companion: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            creates: AtomicUsize::new(0),
        })
    }
}
struct FaultFs {
    inner: Arc<dyn FileSystem>,
    fault: Arc<Fault>,
}
macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        #[async_trait]
        impl FileSystem for FaultFs {
            fn cwd(&self) -> &std::path::Path { self.inner.cwd() }
            async fn cleanup(&self) { self.inner.cleanup().await; }
            async fn write_new_file(&self, path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                self.fault.creates.fetch_add(1, Ordering::SeqCst);
                if self.fault.reject_new.load(Ordering::SeqCst) {
                    return Err(FileError::new(FileErrorCode::NotSupported, "exclusive create unavailable"));
                }
                if path == PATH && self.fault.race_primary.swap(false, Ordering::SeqCst) {
                    // Another creator won after the caller's absent-path check.
                    self.inner.write_new_file(path, FileContent::Text("raced history".into()), None).await?;
                    self.inner.write_new_file(PROGRESS, FileContent::Text("raced progress".into()), None).await?;
                }
                if path == PROGRESS && self.fault.block_companion.swap(false, Ordering::SeqCst) {
                    self.fault.entered.add_permits(1);
                    self.fault.release.acquire().await.unwrap().forget();
                }
                self.inner.write_new_file(path, content, cancel).await
            }
            async fn append_file(&self, path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                if path == PATH && self.fault.fail_header.load(Ordering::SeqCst) {
                    return Err(FileError::new(FileErrorCode::Unknown, "injected header publication failure"));
                }
                self.inner.append_file(path, content, cancel).await
            }
            async fn sync_file_and_parent(&self, path: &str, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                if (path == PATH && self.fault.fail_primary_sync.load(Ordering::SeqCst))
                    || (path == PROGRESS && self.fault.fail_companion_sync.load(Ordering::SeqCst)) {
                    return Err(FileError::new(FileErrorCode::Unknown, "injected creation sync failure"));
                }
                self.inner.sync_file_and_parent(path, cancel).await
            }
            $(async fn $name(&self, $($arg: $ty),*) -> Result<$ret, FileError> { self.inner.$name($($arg),*).await })*
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

#[tokio::test]
async fn exclusive_create_strictly_publishes_and_reopens_a_complete_pair() {
    let root = tempfile::tempdir().unwrap();
    let fs = os(root.path());
    let storage = create(fs.clone(), "fresh").await.unwrap();
    assert!(storage.writer_lease_held());
    assert_eq!(storage.sync_history().await.unwrap().revision, 0);
    assert!(root.path().join(PROGRESS).exists());
    storage.release_writer_lease();
    let loaded = load(fs).await.unwrap();
    assert_eq!(loaded.metadata().id, "fresh");
    assert!(loaded
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .is_empty());
    loaded.release_writer_lease();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_new_session_creators_preserve_one_winners_header_and_progress() {
    let root = tempfile::tempdir().unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut tasks = Vec::new();
    for i in 0..12 {
        let fs = os(root.path());
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            (i, create(fs, &format!("winner-{i}")).await)
        }));
    }
    let mut winners = Vec::new();
    for task in tasks {
        let (id, result) = task.await.unwrap();
        if let Ok(storage) = result {
            winners.push((id, storage));
        }
    }
    assert_eq!(winners.len(), 1);
    let (id, winner) = winners.pop().unwrap();
    assert_eq!(winner.metadata().id, format!("winner-{id}"));
    winner.release_writer_lease();
    let loaded = load(os(root.path())).await.unwrap();
    assert_eq!(loaded.metadata().id, format!("winner-{id}"));
    loaded.release_writer_lease();
}

#[tokio::test]
async fn existing_history_and_progress_are_never_reset_even_after_lease_release() {
    let root = tempfile::tempdir().unwrap();
    let fs = os(root.path());
    let storage = create(fs.clone(), "existing").await.unwrap();
    storage
        .append_entry(
            ProvisionedEntry {
                id: "kept-entry".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "note".into(),
                    data: Some(serde_json::json!({"keep": true})),
                },
            },
            "main",
        )
        .await
        .unwrap();
    storage.release_writer_lease();
    let history = std::fs::read(root.path().join(PATH)).unwrap();
    let progress = std::fs::read(root.path().join(PROGRESS)).unwrap();
    assert!(create(fs.clone(), "replacement").await.is_err());
    assert_eq!(std::fs::read(root.path().join(PATH)).unwrap(), history);
    assert_eq!(std::fs::read(root.path().join(PROGRESS)).unwrap(), progress);
    let loaded = load(fs).await.unwrap();
    assert_eq!(loaded.metadata().id, "existing");
    assert_eq!(
        loaded
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
    loaded.release_writer_lease();
}

#[tokio::test]
async fn orphan_companion_is_not_adopted_overwritten_or_removed_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let fs = os(root.path());
    std::fs::write(root.path().join(PROGRESS), "foreign orphan").unwrap();
    assert!(create(fs.clone(), "new").await.is_err());
    let partial = std::fs::read(root.path().join(PATH)).unwrap();
    assert_eq!(
        std::fs::read(root.path().join(PROGRESS)).unwrap(),
        b"foreign orphan"
    );
    assert!(create(fs.clone(), "retry").await.is_err());
    assert_eq!(std::fs::read(root.path().join(PATH)).unwrap(), partial);
    assert_eq!(
        std::fs::read(root.path().join(PROGRESS)).unwrap(),
        b"foreign orphan"
    );
    // Failed companion creation leaves only an empty primary reservation, not
    // a readable header that could authorize adoption on the next startup.
    assert!(partial.is_empty());
    assert!(load(fs).await.is_err());
    assert_eq!(std::fs::read(root.path().join(PATH)).unwrap(), partial);
    assert_eq!(
        std::fs::read(root.path().join(PROGRESS)).unwrap(),
        b"foreign orphan"
    );
}

#[tokio::test]
async fn even_a_valid_same_identity_orphan_cannot_be_adopted_by_failed_creation() {
    let root = tempfile::tempdir().unwrap();
    let fs = os(root.path());
    let seed = JsonlSessionStorage::create_new(
        fs.clone(),
        "seed.jsonl",
        header("same-id"),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap();
    seed.release_writer_lease();
    let orphan = std::fs::read(root.path().join("seed.jsonl.progress.jsonl")).unwrap();
    std::fs::write(root.path().join(PROGRESS), &orphan).unwrap();
    assert!(create(fs.clone(), "same-id").await.is_err());
    assert!(std::fs::read(root.path().join(PATH)).unwrap().is_empty());
    assert!(load(fs.clone()).await.is_err());
    assert!(create(fs, "same-id").await.is_err());
    assert_eq!(std::fs::read(root.path().join(PROGRESS)).unwrap(), orphan);
    assert!(std::fs::read(root.path().join(PATH)).unwrap().is_empty());
}

#[tokio::test]
async fn a_creator_winning_the_absent_check_race_is_not_truncated() {
    let root = tempfile::tempdir().unwrap();
    let fault = Fault::new();
    fault.race_primary.store(true, Ordering::SeqCst);
    let fs = Arc::new(FaultFs {
        inner: os(root.path()),
        fault,
    });
    assert!(!root.path().join(PATH).exists());
    assert!(create(fs, "loser").await.is_err());
    assert_eq!(
        std::fs::read(root.path().join(PATH)).unwrap(),
        b"raced history"
    );
    assert_eq!(
        std::fs::read(root.path().join(PROGRESS)).unwrap(),
        b"raced progress"
    );
}

#[tokio::test]
async fn unsupported_creation_never_falls_back_to_truncating_legacy_write() {
    let memory: Arc<dyn FileSystem> = Arc::new(InMemoryExecutionEnv::with_cwd("/".into()));
    assert!(create(memory.clone(), "no-lease").await.is_err());
    assert!(!memory.exists(PATH, None).await.unwrap());
    let root = tempfile::tempdir().unwrap();
    let fault = Fault::new();
    fault.reject_new.store(true, Ordering::SeqCst);
    let fs = Arc::new(FaultFs {
        inner: os(root.path()),
        fault,
    });
    assert!(create(fs, "unsupported").await.is_err());
    assert!(!root.path().join(PATH).exists());
    assert!(!root.path().join(PROGRESS).exists());
}

#[tokio::test]
async fn failure_before_header_publication_never_makes_a_partial_pair_restorable() {
    for fail_sync in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let fault = Fault::new();
        fault.fail_companion_sync.store(fail_sync, Ordering::SeqCst);
        fault.fail_header.store(!fail_sync, Ordering::SeqCst);
        let fs = Arc::new(FaultFs {
            inner: os(root.path()),
            fault: fault.clone(),
        });
        assert!(create(fs.clone(), "unpublished").await.is_err());
        assert!(std::fs::read(root.path().join(PATH)).unwrap().is_empty());
        let companion = std::fs::read(root.path().join(PROGRESS)).unwrap();
        fault.fail_companion_sync.store(false, Ordering::SeqCst);
        fault.fail_header.store(false, Ordering::SeqCst);
        assert!(load(fs.clone()).await.is_err());
        assert!(create(fs, "unpublished").await.is_err());
        assert_eq!(
            std::fs::read(root.path().join(PROGRESS)).unwrap(),
            companion
        );
        assert!(std::fs::read(root.path().join(PATH)).unwrap().is_empty());
    }
}

#[tokio::test]
async fn failed_strict_creation_sync_returns_error_and_never_overwrites_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let fault = Fault::new();
    fault.fail_primary_sync.store(true, Ordering::SeqCst);
    let fs = Arc::new(FaultFs {
        inner: os(root.path()),
        fault: fault.clone(),
    });
    assert!(create(fs.clone(), "uncertain").await.is_err());
    let history = std::fs::read(root.path().join(PATH)).unwrap();
    let progress = std::fs::read(root.path().join(PROGRESS)).unwrap();
    fault.fail_primary_sync.store(false, Ordering::SeqCst);
    assert!(create(fs.clone(), "retry").await.is_err());
    assert_eq!(std::fs::read(root.path().join(PATH)).unwrap(), history);
    assert_eq!(std::fs::read(root.path().join(PROGRESS)).unwrap(), progress);
    // Only explicit restore of the addressed history may recover it, not a
    // blind create retry under a different Session identity.
    let loaded = load(fs).await.unwrap();
    assert_eq!(loaded.metadata().id, "uncertain");
    loaded.release_writer_lease();
}

#[tokio::test(flavor = "current_thread")]
async fn abandoned_creation_keeps_writer_ownership_until_the_pair_is_initialized() {
    let root = tempfile::tempdir().unwrap();
    let fault = Fault::new();
    fault.block_companion.store(true, Ordering::SeqCst);
    let inner = os(root.path());
    let fs: Arc<dyn FileSystem> = Arc::new(FaultFs {
        inner: inner.clone(),
        fault: fault.clone(),
    });
    let creator_fs = fs.clone();
    let caller = tokio::spawn(async move { create(creator_fs, "owned-initialization").await });
    tokio::time::timeout(DEADLINE, fault.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    caller.abort();
    assert!(caller
        .await
        .err()
        .expect("caller must be aborted")
        .is_cancelled());
    assert!(
        inner.try_acquire_writer_lease(PATH).await.is_err(),
        "abandoned waiter released initialization lease too early"
    );
    assert!(create(fs.clone(), "competitor").await.is_err());
    assert!(std::fs::read(root.path().join(PATH)).unwrap().is_empty());
    assert!(!root.path().join(PROGRESS).exists());
    fault.release.add_permits(1);
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(Some(lease)) = inner.try_acquire_writer_lease(PATH).await {
                drop(lease);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let loaded = load(fs).await.unwrap();
    assert_eq!(loaded.metadata().id, "owned-initialization");
    loaded.release_writer_lease();
}
