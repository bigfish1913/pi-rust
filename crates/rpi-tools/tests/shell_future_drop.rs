#![cfg(unix)]
use rpi_tools::{OsExecutionEnv, Shell, ShellExecOptions};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_shell_future_stops_owned_descendants_only() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let run = |name: &'static str| {
        let env = env.clone();
        tokio::spawn(async move {
            env.exec(
                &format!("(while true; do echo tick >> {name}; sleep 0.05; done) & wait"),
                ShellExecOptions::default(),
            )
            .await
        })
    };
    let first = run("first");
    let second = run("second");
    for _ in 0..200 {
        if std::fs::metadata(dir.path().join("first")).is_ok()
            && std::fs::metadata(dir.path().join("second")).is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(dir.path().join("first").exists());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = std::fs::read(dir.path().join("first")).unwrap();
    let live = std::fs::read(dir.path().join("second")).unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        stopped,
        std::fs::read(dir.path().join("first")).unwrap(),
        "orphan shell descendant survived dropped future"
    );
    assert!(
        std::fs::read(dir.path().join("second")).unwrap().len() > live.len(),
        "another invocation was killed"
    );
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
}
