//! Lightweight directory queries must agree with list_dir without stale caches.
use rpi_tools::{FileContent, FileErrorCode, FileSystem, InMemoryExecutionEnv, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

async fn check_suffix_queries(env: &dyn FileSystem) {
    env.create_dir("sessions", true, None).await.unwrap();
    env.create_dir("sessions/directory_id.jsonl", true, None)
        .await
        .unwrap();
    env.write_file(
        "sessions/log_id.jsonl.progress.jsonl",
        FileContent::from("sidecar"),
        None,
    )
    .await
    .unwrap();
    assert!(!env
        .has_file_with_suffix("sessions", "_id.jsonl", None)
        .await
        .unwrap());
    env.write_file("sessions/log_id.jsonl", FileContent::from("header"), None)
        .await
        .unwrap();
    assert!(env
        .has_file_with_suffix("sessions", "_id.jsonl", None)
        .await
        .unwrap());
    env.remove("sessions/log_id.jsonl", false, false, None)
        .await
        .unwrap();
    assert!(!env
        .has_file_with_suffix("sessions", "_id.jsonl", None)
        .await
        .unwrap());
    // An external creator after a negative query must be visible immediately.
    env.write_file("sessions/later_id.jsonl", FileContent::from("header"), None)
        .await
        .unwrap();
    assert!(env
        .has_file_with_suffix("sessions", "_id.jsonl", None)
        .await
        .unwrap());
    assert_eq!(
        env.has_file_with_suffix("missing", "_id.jsonl", None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotFound
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        env.has_file_with_suffix("sessions", "_id.jsonl", Some(&cancel))
            .await
            .unwrap_err()
            .code,
        FileErrorCode::Aborted
    );
}

#[tokio::test]
async fn default_query_matches_in_memory_directory_semantics() {
    let root = tempfile::tempdir().unwrap();
    let env = InMemoryExecutionEnv::with_cwd(root.path().to_path_buf());
    check_suffix_queries(&env).await;
}

#[tokio::test]
async fn os_query_matches_directory_semantics() {
    let root = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    check_suffix_queries(&env).await;
}

#[cfg(unix)]
#[tokio::test]
async fn os_query_counts_symlinks_without_following_and_skips_special_files() {
    let root = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    let _socket =
        std::os::unix::net::UnixListener::bind(root.path().join("socket_id.jsonl")).unwrap();
    assert!(!env
        .has_file_with_suffix(".", "_id.jsonl", None)
        .await
        .unwrap());
    std::os::unix::fs::symlink("missing-target", root.path().join("dangling_id.jsonl")).unwrap();
    assert!(env
        .has_file_with_suffix(".", "_id.jsonl", None)
        .await
        .unwrap());
}
