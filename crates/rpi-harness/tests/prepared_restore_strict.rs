//! A matching size/mtime is not a content fingerprint (including atomic copy).
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    DefaultIdGenerator, Session, SystemClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::{
    fs::{File, FileTimes},
    sync::Arc,
};

async fn changed_source_is_rejected(atomic_replace: bool) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.jsonl");
    let env = Arc::new(OsExecutionEnv::with_cwd(root.path().to_path_buf()));
    let storage = Arc::new(
        JsonlSessionStorage::create(
            env.clone(),
            path.to_str().unwrap(),
            JsonlV4Header::new(
                "same-stat".into(),
                1,
                root.path().to_string_lossy().into_owned(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    );
    let session = Session::new(storage.clone(), None);
    session
        .view("main")
        .append_message(rpi_ai::UserMessage::new("one", 1).into())
        .await
        .unwrap();
    session.drain().await;
    storage.release_writer_lease();
    drop(session);
    drop(storage);
    let original = std::fs::read_to_string(&path).unwrap();
    let original_mtime = path.metadata().unwrap().modified().unwrap();
    let prepared = JsonlSessionStorage::prepare_restore(
        env.clone(),
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let changed = original.replace("\"one\"", "\"two\"");
    assert_ne!(original, changed);
    assert_eq!(original.len(), changed.len());
    let original_info = env.file_info(path.to_str().unwrap(), None).await.unwrap();
    let target = if atomic_replace {
        root.path().join("replacement.jsonl")
    } else {
        path.clone()
    };
    std::fs::write(&target, &changed).unwrap();
    File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_times(FileTimes::new().set_modified(original_mtime))
        .unwrap();
    if atomic_replace {
        std::fs::rename(&target, &path).unwrap();
    }
    let changed_info = env.file_info(path.to_str().unwrap(), None).await.unwrap();
    assert_eq!(
        (original_info.size, original_info.mtime_ms),
        (changed_info.size, changed_info.mtime_ms)
    );
    let result = prepared.commit_strict().await;
    assert!(
        result.is_err(),
        "same size/mtime concealed a different replay generation (atomic={atomic_replace})"
    );
    let error = result.err().unwrap();
    assert!(
        error
            .to_string()
            .contains("changed during restore preparation"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    // Rejection must not keep writer ownership or mutate the newer journal.
    let lease = env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .unwrap();
    assert!(lease.is_some());
}

#[tokio::test]
async fn strict_restore_rejects_same_stamp_in_place_rewrite() {
    changed_source_is_rejected(false).await;
}
#[tokio::test]
async fn strict_restore_rejects_same_stamp_atomic_replacement() {
    changed_source_is_rejected(true).await;
}

async fn empty_history(root: &tempfile::TempDir) -> (Arc<OsExecutionEnv>, std::path::PathBuf) {
    let env = Arc::new(OsExecutionEnv::with_cwd(root.path().to_path_buf()));
    let path = root.path().join("stable.jsonl");
    let storage = JsonlSessionStorage::create(
        env.clone(),
        path.to_str().unwrap(),
        JsonlV4Header::new(
            "strict-stable".into(),
            1,
            root.path().to_string_lossy().into_owned(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    storage.drain().await;
    storage.release_writer_lease();
    (env, path)
}

#[tokio::test]
async fn strict_restore_accepts_unchanged_source_and_holds_real_writer_lease() {
    let root = tempfile::tempdir().unwrap();
    let (env, path) = empty_history(&root).await;
    let original = std::fs::read(&path).unwrap();
    let prepared = JsonlSessionStorage::prepare_restore(
        env.clone(),
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    assert_eq!(prepared.facts().await.unwrap().metadata.id, "strict-stable");
    let storage = prepared.commit_strict().await.unwrap();
    assert!(env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    storage.release_writer_lease();
    assert!(env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn strict_restore_rejects_competing_writer_without_repairing_source() {
    let root = tempfile::tempdir().unwrap();
    let (env, path) = empty_history(&root).await;
    let prepared = JsonlSessionStorage::prepare_restore(
        env.clone(),
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let foreign = env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    assert!(prepared.commit_strict().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .is_err());
    drop(foreign);
    assert!(env
        .try_acquire_writer_lease(path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
}
