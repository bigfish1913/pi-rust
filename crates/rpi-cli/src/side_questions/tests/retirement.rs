use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_preparation_waiter_does_not_drop_the_owned_operation_or_its_context() {
    let parent = parent(FauxScript::new().with_text("unused")).await;
    let manager = SideQuestions::default();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let (release, gate) = std::sync::mpsc::channel();
    let gate = Mutex::new(gate);
    let blocked_sink: SideQuestionSink = Arc::new(move |_| {
        if let Some(entered) = entered.lock().unwrap().take() {
            entered.send(()).unwrap();
            gate.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        Ok(json!({"ok":true}))
    });
    let service = manager.clone();
    let harness = parent.clone();
    let waiter = tokio::spawn(async move {
        service
            .handle(
                &harness,
                7,
                json!({"op":"start","question":"q"}),
                blocked_sink,
            )
            .await
    });
    ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    manager.begin_shutdown();
    {
        let wait = manager.wait_shutdown();
        tokio::pin!(wait);
        assert!(
            futures::poll!(wait.as_mut()).is_pending(),
            "foreign/native preparation is still running"
        );
    }
    assert!(manager
        .handle(&parent, 7, json!({"op":"show"}), sink())
        .await
        .is_err());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), manager.wait_shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(manager.views().is_empty());
    assert_eq!(manager.0.capacity.available_permits(), 8);
    manager.wait_shutdown().await.unwrap();
    parent.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_thread_workers_watchers_and_queued_closes_are_all_settled() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = SideQuestions::default();
    command(&manager, &parent, json!({"op":"start","question":"first"})).await;
    running(&manager).await;
    let queued = command(&manager, &parent, json!({"op":"start","question":"queued"})).await;
    assert_eq!(queued["status"], "queued");
    manager.cancel_owner(7);
    assert!(manager.views().is_empty(), "registry removal is not a join");
    manager.begin_shutdown();
    tokio::time::timeout(Duration::from_secs(10), manager.wait_shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(manager.0.capacity.available_permits(), 8);
    assert!(manager.0.registry.lock().unwrap().threads.is_empty());
    manager.begin_shutdown();
    manager.wait_shutdown().await.unwrap();
    parent.close().await.unwrap();
}
