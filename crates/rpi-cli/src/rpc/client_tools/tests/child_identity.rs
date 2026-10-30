use super::*;
use rpi_agent::tool_execution_scope::ToolExecutionScope;

fn child(id: &str) -> Arc<ToolExecutionScope> {
    Arc::new(ToolExecutionScope::with_context(json!({
        "sessionId":id,"parentSessionId":"session-test","rootSessionId":"session-test"
    })))
}
async fn next_request(f: &mut Fixture, method: &str, session: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = f.rx.recv().await.unwrap();
            if event["type"] == "client_request" {
                assert_eq!(event["method"], method);
                assert_eq!(event["sessionId"], session);
                return event;
            }
        }
    }).await.unwrap()
}

#[tokio::test]
async fn two_children_same_root_and_method_keep_identity_and_denial_is_denial() {
    let mut f = Fixture::new(1000);
    let mut tasks = Vec::new();
    for id in ["child-a", "child-b"] {
        let backend = f.backend();
        tasks.push(tokio::spawn(async move {
            child(id).run(backend.read_text("buffer", CancellationToken::new())).await
        }));
    }
    let mut requests = Vec::new();
    while requests.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(3), f.rx.recv()).await.unwrap().unwrap();
        if event["type"] == "client_request" { requests.push(event); }
    }
    assert_ne!(requests[0]["requestId"], requests[1]["requestId"]);
    let mut ids: Vec<_> = requests.iter().map(|r| r["sessionId"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, ["child-a", "child-b"]);
    for request in &requests {
        assert_eq!(request["method"], "fs/read_text_file");
        f.reply(request, Err("permission denied by parent policy".into()));
    }
    for task in tasks { assert!(task.await.unwrap().unwrap_err().contains("permission denied")); }
    let events = f.events.clone();
    f.finish().await;
    for event in events.lock().unwrap().iter().filter(|e| e["type"] == "client_request_closed") {
        let original = requests.iter().find(|r| r["requestId"] == event["requestId"]).unwrap();
        assert_eq!(event["sessionId"], original["sessionId"]);
    }
}

#[tokio::test]
async fn terminal_owner_cleanup_retains_child_after_origin_task_exits() {
    let mut f = Fixture::new(1000);
    let backend = f.backend();
    let origin = tokio::spawn(async move {
        child("terminal-child").run(backend.exec("echo child", None, CancellationToken::new())).await
    });
    let create = next_request(&mut f, "terminal/create", "terminal-child").await;
    origin.abort();
    assert!(origin.await.unwrap_err().is_cancelled());
    // The invocation task-local no longer exists. The terminal owner still has
    // its captured host provenance when create finally completes.
    f.reply(&create, Ok(json!({"terminalId":"owned-child-terminal"})));
    let kill = next_request(&mut f, "terminal/kill", "terminal-child").await;
    f.reply(&kill, Ok(json!({})));
    let release = next_request(&mut f, "terminal/release", "terminal-child").await;
    f.reply(&release, Ok(json!({})));
    let events = f.events.clone();
    f.finish().await;
    for event in events.lock().unwrap().iter().filter(|e| e["type"] == "client_request_closed") {
        assert_eq!(event["sessionId"], "terminal-child");
    }
}

#[tokio::test]
async fn child_without_trusted_identity_never_falls_back_to_root() {
    let f = Fixture::new(1000);
    let result = Arc::new(ToolExecutionScope::default()).run(
        f.backend().request("fs/read_text_file", json!({"sessionId":"forged"}),
            &CancellationToken::new(), Duration::from_secs(1), false)
    ).await;
    assert!(result.unwrap_err().contains("trusted session identity"));
    assert!(f.events.lock().unwrap().is_empty());
    f.finish().await;
}
