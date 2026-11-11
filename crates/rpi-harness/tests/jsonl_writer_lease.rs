use std::process::Command;
use std::sync::Arc;

use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::SystemClock;
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionStorage;
use rpi_tools::{FileSystem, OsExecutionEnv};

const CHILD_CWD: &str = "RPI_JSONL_LEASE_CHILD_CWD";

#[tokio::test]
async fn jsonl_storage_lease_fences_competing_writers_and_retires_old_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let first = JsonlSessionStorage::create(
        fs.clone(),
        "lease.jsonl",
        JsonlV4Header::new(
            "lease-session".into(),
            1,
            "/synthetic".into(),
            None,
            None,
            None,
        ),
        clock.clone(),
        ids.clone(),
    )
    .await
    .expect("create acquires the storage lease");
    assert!(first.writer_lease_held());

    // Loading is read-only with respect to ownership; the second binding must
    // fail when it explicitly enters the writer role.
    let second = JsonlSessionStorage::load(fs.clone(), "lease.jsonl", clock, ids)
        .await
        .expect("read-only reopen");
    assert!(!second.writer_lease_held());
    let conflict = second
        .acquire_writer_lease()
        .await
        .expect_err("lease conflict");
    assert!(conflict.to_string().contains("writer lease"));

    first.release_writer_lease();
    assert!(!first.writer_lease_held());
    let retired = first
        .create_lane("retired-cannot-write", None)
        .await
        .expect_err("released handle must remain fenced");
    assert!(retired.to_string().contains("released"));

    second
        .acquire_writer_lease()
        .await
        .expect("release permits a fresh binding to acquire");
    assert!(second.writer_lease_held());
    second
        .create_lane("fresh-owner", None)
        .await
        .expect("new owner can append");
    second.release_writer_lease();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("lease.jsonl.lock")).unwrap(),
        "2"
    );
}

#[tokio::test]
async fn jsonl_storage_lease_rejects_an_independent_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let storage = JsonlSessionStorage::create(
        fs,
        "process-lease.jsonl",
        JsonlV4Header::new(
            "process-lease-session".into(),
            1,
            "/synthetic".into(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .expect("parent acquires the storage lease");

    let output = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("jsonl_storage_lease_child_probe")
        .arg("--nocapture")
        .env(CHILD_CWD, dir.path())
        .output()
        .expect("spawn independent lease probe");
    assert!(
        output.status.success(),
        "child lease probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(storage.writer_lease_held());
}

#[tokio::test]
async fn jsonl_storage_lease_child_probe() {
    let Ok(cwd) = std::env::var(CHILD_CWD) else {
        return;
    };
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(cwd.into()));
    let storage = JsonlSessionStorage::load(
        fs,
        "process-lease.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .expect("child reads the committed destination without owning it");
    let error = storage
        .acquire_writer_lease()
        .await
        .expect_err("independent process must not acquire the parent lease");
    assert!(error.to_string().contains("writer lease"));
}
