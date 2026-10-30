use super::*;
use rpi_agent::tool_execution_scope::{with_invocation_binding, InvocationToolCallId};

async fn idle(manager: &ChildAgents) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.active.load(Ordering::Acquire) != 0 { tokio::task::yield_now().await; }
    }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_jsonl_child_reopens_with_trusted_header_and_isolated_root() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_string_lossy().into_owned();
    let session = crate::session::create_jsonl_session(temp.path(), &cwd).await.unwrap();
    let root_id = session.storage().metadata().id;
    let root_path = crate::session::list_session_files_sync(temp.path()).pop().unwrap();
    let before = std::fs::read(&root_path).unwrap();
    let provider = FauxProvider::new(FauxScript::new().with_text("persisted child answer"));
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(), models: vec![provider], session,
        ..Default::default()
    }).await.unwrap();
    let manager = ChildAgents::default();
    manager.set_session_factory(crate::session::native_child_session_factory(temp.path().into()));
    let result = with_invocation_binding(InvocationToolCallId::new("trusted-native-call"), CancellationToken::new(),
        manager.handle(&parent, 7, json!({"op":"start","task":"report", "parentToolCallId":"spoof", "rootSessionId":"spoof"}))).await.unwrap();
    let id = result["childId"].as_str().unwrap();
    assert_eq!(wait(&manager, &parent, id).await["result"]["text"], "persisted child answer");
    idle(&manager).await;
    let children = root_path.parent().unwrap().join(".children").join(&root_id);
    let files = crate::session::list_session_files_sync(&children);
    assert_eq!(files.len(), 1);
    // Ordinary root discovery never descends into the root-owned child area.
    assert_eq!(crate::session::list_session_files_sync(temp.path()), vec![root_path.clone()]);
    let bytes = std::fs::read_to_string(&files[0]).unwrap();
    let header: Value = serde_json::from_str(bytes.lines().next().unwrap()).unwrap();
    assert_eq!(header["id"], id);
    assert_eq!(header["parentSessionId"], root_id);
    assert_eq!(header["metadata"]["kind"], "delegated-child");
    assert_eq!(header["metadata"]["version"], 1);
    assert_eq!(header["metadata"]["rootSessionId"], root_id);
    assert_eq!(header["metadata"]["parentToolCallId"], "trusted-native-call");
    assert!(!bytes.contains("spoof"));
    assert!(!bytes.contains("assistant_frame"));
    let _ = parent.close().await;
    assert_eq!(std::fs::read(&root_path).unwrap(), before);
    drop(manager);
    drop(parent);
    // Storage-only reopen: no harness, hooks, model calls or recovery runs.
    let fs: Arc<dyn rpi_tools::FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into()));
    let reopened = rpi_harness::session::jsonl::JsonlSessionStorage::load(fs, &files[0].to_string_lossy(), Arc::new(SystemClock), Arc::new(DefaultIdGenerator::new())).await.unwrap();
    use rpi_harness::session::types::{SessionStorage, EntryQuery};
    let entries = reopened.find_entries(&EntryQuery::default()).await.unwrap();
    assert!(serde_json::to_string(&entries).unwrap().contains("persisted child answer"));
    assert_eq!(reopened.metadata().id, id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ephemeral_native_policy_creates_no_files() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent(FauxScript::new().with_text("memory")).await;
    let manager = ChildAgents::default();
    manager.set_session_factory(crate::session::native_child_session_factory(temp.path().into()));
    let started = manager.handle(&parent, 7, json!({"op":"start","task":"report"})).await.unwrap();
    wait(&manager, &parent, started["childId"].as_str().unwrap()).await;
    idle(&manager).await;
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_failure_and_cancelled_creation_never_announce_and_recover_capacity() {
    let parent = parent(FauxScript::new().with_text("ok")).await;
    let manager = ChildAgents::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = events.clone();
    manager.set_event_sink(Some(Arc::new(move |event| {
        output.lock().unwrap().push(event); Box::pin(async {})
    })));
    manager.set_session_factory(Arc::new(|_| Box::pin(async { Err("injected create failure".into()) })));
    assert!(manager.handle(&parent, 7, json!({"op":"start","task":"report"})).await.unwrap_err().contains("injected"));
    idle(&manager).await;
    assert!(manager.jobs.lock().unwrap().is_empty());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, r) = (entered.clone(), release.clone());
    manager.set_session_factory(Arc::new(move |request| {
        let (e, r) = (e.clone(), r.clone());
        Box::pin(async move { e.notify_one(); r.notified().await; Ok(memory_child_session(&request)) })
    }));
    let mut start = Box::pin(manager.handle(&parent, 7, json!({"op":"start","task":"report"})));
    tokio::select! { _ = entered.notified() => {}, _ = &mut start => panic!("factory should block") }
    // Drop the actual caller future, not just its pinned reference.
    drop(start);
    release.notify_one();
    idle(&manager).await;
    assert!(events.lock().unwrap().is_empty());
    assert!(manager.jobs.lock().unwrap().is_empty());
}
