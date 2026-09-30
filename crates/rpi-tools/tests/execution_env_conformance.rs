//! Mirrors `packages/agent/test/harness/nodejs-env.test.ts` (subset) — runs the
//! SAME `FileSystem`/`Shell` conformance suite against both `InMemoryExecutionEnv`
//! and `OsExecutionEnv` (over a tempdir), so the two backends are exercised by
//! identical assertions. `Shell` is only sanity-checked on `Os` (real bash); the
//! rich shell behavior lives in the bash-tool tests.

use std::sync::Arc;

use rpi_tools::{
    ExecutionEnv, ExecutionErrorCode, FileContent, FileKind, InMemoryExecutionEnv, OsExecutionEnv,
    Shell, ShellExecOptions,
};
use tokio_util::sync::CancellationToken;

/// The conformance suite, generic over any `ExecutionEnv`. Each case seeds a
/// relative path against the env's cwd and asserts the documented behavior.
async fn run_suite(env: Arc<dyn ExecutionEnv>) {
    // absolute_path / join_path.
    let abs = env
        .absolute_path("foo/bar.txt", None)
        .await
        .expect("absolute_path");
    assert!(abs.is_absolute(), "absolute_path yields absolute");
    let joined = env
        .join_path(&["a", "b", "c"], None)
        .await
        .expect("join_path");
    assert!(joined.ends_with("c"));

    // write_file creates parent dirs + round-trips text.
    env.write_file("dir/sub/file.txt", FileContent::Text("hello".into()), None)
        .await
        .expect("write_file");
    let text = env
        .read_text_file("dir/sub/file.txt", None)
        .await
        .expect("read_text_file");
    assert_eq!(text, "hello");

    // read_binary_file matches.
    let bytes = env
        .read_binary_file("dir/sub/file.txt", None)
        .await
        .expect("read_binary_file");
    assert_eq!(bytes, b"hello");

    // append_file.
    env.append_file("dir/sub/file.txt", FileContent::Text(" world".into()), None)
        .await
        .expect("append_file");
    let appended = env
        .read_text_file("dir/sub/file.txt", None)
        .await
        .expect("read after append");
    assert_eq!(appended, "hello world");

    // file_info reports a File.
    let info = env
        .file_info("dir/sub/file.txt", None)
        .await
        .expect("file_info");
    assert_eq!(info.kind, FileKind::File);
    assert_eq!(info.size, 11);

    // exists true/false.
    assert!(env.exists("dir/sub/file.txt", None).await.expect("exists"));
    assert!(!env.exists("nope.txt", None).await.expect("exists false"));
}

#[tokio::test]
async fn in_memory_conforms() {
    // Use an OS-absolute root so `absolute_path(...).is_absolute()` holds on
    // Windows too (where `/tmp/work` is NOT absolute — it lacks a drive).
    let root = std::env::temp_dir().join("pi-tools-im-conf");
    let _ = std::fs::create_dir_all(&root);
    let env: Arc<dyn ExecutionEnv> = Arc::new(InMemoryExecutionEnv::with_cwd(root.clone()));
    run_suite(env).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn os_conforms_over_tempdir() {
    // Skip on systems without bash — the FileSystem suite itself is OS-agnostic,
    // but we only run the Os backend where a real shell exists.
    let tmp = std::env::temp_dir().join(format!("pi-tools-conf-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
    run_suite(env).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn os_cancel_interrupts_even_when_timeout_is_set() {
    let tmp = std::env::temp_dir().join(format!("pi-tools-cancel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let env = OsExecutionEnv::with_cwd(tmp.clone());
    let cancel = CancellationToken::new();
    let cancel_after_start = cancel.clone();
    let started = std::time::Instant::now();

    let cancel_task = async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_after_start.cancel();
    };
    let exec_task = env.exec(
        "sleep 30",
        ShellExecOptions {
            timeout: Some(2.0),
            cancel: Some(&cancel),
            ..Default::default()
        },
    );
    let (_, result) = tokio::join!(cancel_task, exec_task);
    let _ = std::fs::remove_dir_all(&tmp);

    match result {
        Err(error) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        Err(error) => assert_eq!(error.code, ExecutionErrorCode::Aborted),
        Ok(_) => panic!("cancelled command unexpectedly completed"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "cancellation waited for the configured timeout"
    );
}
