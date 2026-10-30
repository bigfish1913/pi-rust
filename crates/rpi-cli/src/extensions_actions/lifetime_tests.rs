use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_reverse_bindings_fail_closed_but_still_reclaim_their_children() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("before harness"));
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    );
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    assert!(host.get_execution_context(json!({})).await.is_ok());
    let child = host
        .child_agent(7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = &child["childId"];
    drop(parent);
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("after harness"));
    assert!(host
        .child_agent(7, json!({"op":"start","task":"stale"}))
        .await
        .unwrap_err()
        .contains("after harness"));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = host
                .child_agent(7, json!({"op":"poll","childId":id}))
                .await
                .unwrap();
            if value["status"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(host
        .child_agent(8, json!({"op":"dispose","childId":id}))
        .await
        .is_err());
    host.child_agent(7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(host
        .child_agent(7, json!({"op":"poll","childId":id}))
        .await
        .is_err());
}

/// A plugin's stop button must be idempotent: aborting an idle lane returns the
/// documented no-op result instead of surfacing `NoActiveRun` as an action
/// error. Aborting a lane that IS running reports the cancelled run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_is_a_noop_when_idle_and_reports_the_run_it_cancelled() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("a slow answer")
            .with_tokens_per_second(1.0),
    );
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    // Idle: no run is in progress, so the stop button is a no-op, not an error.
    assert_eq!(
        host.abort(json!({})).await.unwrap(),
        json!({ "aborted": false })
    );

    // Active: the same action cancels the in-flight run and names it.
    let lane = harness.lane("main");
    let running = tokio::spawn({
        let lane = lane.clone();
        async move { lane.prompt_text("go", Vec::new()).await }
    });
    let aborted = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = host.abort(json!({})).await.unwrap();
            if value["aborted"] == json!(true) {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("abort must observe the active run");
    assert!(
        aborted["runId"].as_str().is_some_and(|id| !id.is_empty()),
        "{aborted}"
    );
    let outcome = running.await.unwrap().unwrap();
    assert!(
        matches!(outcome.outcome, HarnessRunOutcome::Aborted { .. }),
        "{:?}",
        outcome.outcome
    );
    // Settled again: back to the idempotent no-op.
    assert_eq!(
        host.abort(json!({})).await.unwrap(),
        json!({ "aborted": false })
    );
}
