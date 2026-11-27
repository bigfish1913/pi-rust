use rpi_tools::{FileContent, FileErrorCode, FileSystem, InMemoryExecutionEnv, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn strict_sync_default_refuses_legacy_noop_success() {
    let fs: &dyn FileSystem = &InMemoryExecutionEnv::with_cwd("/".into());
    fs.sync_file("/journal", false, None).await.unwrap();
    fs.sync_directory("/", None).await.unwrap();
    assert_eq!(
        fs.sync_file_and_parent("/journal", None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotSupported,
    );
}

#[cfg(unix)]
#[tokio::test]
async fn strict_sync_real_file_and_parent_preserves_contents() {
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    fs.write_file("journal", FileContent::Text("accepted\n".into()), None)
        .await
        .unwrap();
    fs.sync_file_and_parent("journal", None).await.unwrap();
    assert_eq!(
        fs.read_text_file("journal", None).await.unwrap(),
        "accepted\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn strict_sync_rejects_missing_directory_symlink_and_fifo_without_creating_files() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    assert!(fs.sync_file_and_parent("absent", None).await.is_err());
    assert!(!root.path().join("absent").exists());
    fs.write_file("journal", FileContent::Text("retained".into()), None)
        .await
        .unwrap();
    symlink(root.path().join("journal"), root.path().join("link")).unwrap();
    assert_eq!(
        fs.sync_file_and_parent("link", None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotSupported
    );
    assert_eq!(
        fs.sync_file_and_parent(".", None).await.unwrap_err().code,
        FileErrorCode::NotSupported
    );
    let fifo = std::process::Command::new("mkfifo")
        .arg(root.path().join("fifo"))
        .status()
        .unwrap();
    assert!(fifo.success());
    // The OS metadata adapter rejects special files as Invalid before the
    // strict regular-file check. Either path must refuse without opening it.
    assert!(fs.sync_file_and_parent("fifo", None).await.is_err());
    assert_eq!(
        fs.read_text_file("journal", None).await.unwrap(),
        "retained"
    );
}

#[tokio::test]
async fn strict_sync_honors_entry_cancellation_without_creating_a_file() {
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(fs
        .sync_file_and_parent("absent", Some(&cancel))
        .await
        .is_err());
    assert!(!root.path().join("absent").exists());
}

#[cfg(not(unix))]
#[tokio::test]
async fn strict_sync_does_not_promote_unsupported_directory_sync() {
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    fs.write_file("journal", FileContent::Text("retained".into()), None)
        .await
        .unwrap();
    assert_eq!(
        fs.sync_file_and_parent("journal", None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotSupported
    );
}
