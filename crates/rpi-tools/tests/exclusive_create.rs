use rpi_tools::{FileContent, FileErrorCode, FileSystem, InMemoryExecutionEnv, OsExecutionEnv};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn default_backend_refuses_to_emulate_exclusive_create() {
    let fs = InMemoryExecutionEnv::with_cwd("/".into());
    assert_eq!(
        fs.write_new_file("/new", FileContent::Text("new".into()), None)
            .await
            .unwrap_err()
            .code,
        FileErrorCode::NotSupported,
    );
    assert!(!fs.exists("/new", None).await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_exclusive_creators_have_one_complete_winner() {
    let root = tempfile::tempdir().unwrap();
    let start = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for i in 0..16 {
        let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
        let start = start.clone();
        tasks.push(tokio::spawn(async move {
            let bytes = vec![i as u8; 128 * 1024];
            start.wait().await;
            (
                i,
                fs.write_new_file("new", FileContent::Bytes(bytes), None)
                    .await,
            )
        }));
    }
    let mut winners = Vec::new();
    for task in tasks {
        let (id, outcome) = task.await.unwrap();
        if outcome.is_ok() {
            winners.push(id);
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(
        std::fs::read(root.path().join("new")).unwrap(),
        vec![winners[0] as u8; 128 * 1024]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.path().join("new"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn existing_file_is_never_truncated_and_parent_is_not_created() {
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    fs.write_file("old", FileContent::Text("old history".into()), None)
        .await
        .unwrap();
    for path in ["old", "absent/new", "."] {
        assert!(fs
            .write_new_file(path, FileContent::Text("must not replace".into()), None)
            .await
            .is_err());
    }
    assert_eq!(
        std::fs::read(root.path().join("old")).unwrap(),
        b"old history"
    );
    assert!(!root.path().join("absent").exists());
}

#[tokio::test]
async fn entry_cancellation_does_not_create_a_file() {
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        fs.write_new_file("new", FileContent::Text("cancelled".into()), Some(&cancel))
            .await
            .unwrap_err()
            .code,
        FileErrorCode::Aborted
    );
    assert!(!root.path().join("new").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn exclusive_create_refuses_final_symlinks_hardlinks_and_fifo_without_following() {
    use std::os::unix::fs::{symlink, MetadataExt};
    let root = tempfile::tempdir().unwrap();
    let fs = OsExecutionEnv::with_cwd(root.path().to_path_buf());
    std::fs::write(root.path().join("history"), "kept").unwrap();
    std::fs::hard_link(root.path().join("history"), root.path().join("hard")).unwrap();
    symlink(root.path().join("history"), root.path().join("link")).unwrap();
    symlink(root.path().join("missing"), root.path().join("broken")).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(root.path().join("fifo"))
        .status()
        .unwrap()
        .success());
    let inode = std::fs::metadata(root.path().join("history"))
        .unwrap()
        .ino();
    for path in ["history", "hard", "link", "broken", "fifo"] {
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(3),
            fs.write_new_file(path, FileContent::Text("replacement".into()), None)
        )
        .await
        .unwrap()
        .is_err());
    }
    assert_eq!(std::fs::read(root.path().join("history")).unwrap(), b"kept");
    assert_eq!(
        std::fs::metadata(root.path().join("history"))
            .unwrap()
            .ino(),
        inode
    );
    assert!(!root.path().join("missing").exists());
}
