use super::*;
use rpi_agent::tool_execution_scope::{with_invocation_binding, InvocationToolCallId};

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn trusted_parent_id_is_snapshotted_for_start_and_end_not_json() {
    let parent = parent(FauxScript::new().with_text("child")).await;
    let manager = ChildAgents::default();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let output = events.clone();
    manager.set_event_sink(Some(Arc::new(move |event| {
        let output = output.clone();
        Box::pin(async move { output.lock().unwrap().push(event); })
    })));
    for actual in ["actual-a", "actual-b"] {
        let started = with_invocation_binding(InvocationToolCallId::new(actual), CancellationToken::new(),
            manager.handle(&parent, 7, json!({"op":"start","task":"report","parentToolCallId":"spoof",
                "context":{"parentToolCallId":"spoof"}}))).await.unwrap();
        let id = started["childId"].as_str().unwrap();
        assert_eq!(manager.jobs.lock().unwrap()[id].parent_tool_call_id.as_ref().unwrap().as_str(), actual);
        wait(&manager, &parent, id).await;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.active.load(Ordering::Acquire) != 0 { tokio::task::yield_now().await; }
    }).await.unwrap();
    let events = events.lock().unwrap();
    for actual in ["actual-a", "actual-b"] {
        let matched: Vec<_> = events.iter().filter(|e| e["parentToolCallId"] == actual).collect();
        assert_eq!(matched.iter().filter(|e| e["type"] == "child_start").count(), 1);
        assert_eq!(matched.iter().filter(|e| e["type"] == "child_end").count(), 1);
        let id = &matched[0]["childId"];
        for event in matched { assert_eq!(event["childId"], *id); assert_eq!(event["parentSessionId"], "parent"); }
    }
}
