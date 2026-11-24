use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_observes_actual_future_destruction_not_just_its_ready_output() {
    struct ReadyWithSlowDrop {
        started: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        dropped: Arc<AtomicBool>,
    }
    impl Future for ReadyWithSlowDrop {
        type Output = ();
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            std::task::Poll::Ready(())
        }
    }
    impl Drop for ReadyWithSlowDrop {
        fn drop(&mut self) {
            let _ = self.started.take().unwrap().send(());
            self.release.recv_timeout(Duration::from_secs(10)).unwrap();
            self.dropped.store(true, Ordering::Release);
        }
    }
    let tasks = CliTaskScope::default();
    let admission = tasks.admit().unwrap();
    let (started, rx) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    admission.spawn(
        &Handle::current(),
        ReadyWithSlowDrop {
            started: Some(started),
            release: gate,
            dropped: dropped.clone(),
        },
    );
    drop(admission);
    tasks.seal();
    rx.await.unwrap();
    let wait = tasks.wait();
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap()
        .unwrap();
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_reply_waiter_keeps_preparation_owned_with_frozen_invocation_context() {
    let tasks = Arc::new(CliTaskScope::default());
    let scope = Arc::new(
        rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(
            serde_json::json!({"agentId":"caller"}),
        ),
    );
    let signal = CancellationToken::new();
    let invocation = signal.clone();
    let (started, rx) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let completed = Arc::new(AtomicUsize::new(0));
    let count = completed.clone();
    let service = tasks.clone();
    let waiter = tokio::spawn(async move {
        rpi_agent::tool_execution_scope::with_invocation_signal(
            invocation,
            scope.run(service.call(async move {
                assert!(rpi_agent::tool_execution_scope::is_child_execution());
                assert_eq!(
                    rpi_agent::tool_execution_scope::execution_context().unwrap()["agentId"],
                    "caller"
                );
                let token = rpi_agent::tool_execution_scope::invocation_signal().unwrap();
                started.send(()).unwrap();
                gate.await.unwrap();
                assert!(token.is_cancelled());
                count.fetch_add(1, Ordering::Release);
            })),
        )
        .await
    });
    rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    tasks.seal();
    signal.cancel();
    assert!(!rpi_agent::tool_execution_scope::is_child_execution());
    let wait = tasks.wait();
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn admitted_continuation_handoff_cannot_open_an_empty_window_after_seal() {
    let tasks = CliTaskScope::default();
    let admission = tasks.admit().unwrap();
    tasks.seal();
    assert!(tasks.signal().is_cancelled());
    assert!(tasks.admit().is_err());
    {
        let wait = tasks.wait();
        tokio::pin!(wait);
        assert!(futures::poll!(wait.as_mut()).is_pending());
    }
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();
    admission.spawn(&Handle::current(), async move {
        flag.store(true, Ordering::Release);
    });
    drop(admission);
    tasks.wait().await.unwrap();
    assert!(finished.load(Ordering::Acquire));
    tasks.wait().await.unwrap();
}

#[tokio::test]
async fn panicked_or_failed_cleanup_is_sticky_unknown_even_after_task_exit() {
    let tasks = CliTaskScope::default();
    let admission = tasks.admit().unwrap();
    admission.spawn(&Handle::current(), async {
        panic!("owned auxiliary fixture panic")
    });
    drop(admission);
    tasks.seal();
    assert!(tasks.wait().await.is_err());
    assert!(tasks.wait().await.is_err());
    let tasks = CliTaskScope::default();
    tasks.admit().unwrap().mark_unknown();
    tasks.seal();
    assert!(tasks.wait().await.is_err());
}
