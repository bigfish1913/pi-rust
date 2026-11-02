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

/// `sendUserMessage` 必须认 `deliverAs`。
///
/// 以前它一律 `prompt_text(..)`：跑着时 lane 忙 → 报错 → 而这个动作是阻塞到
/// 整轮跑完的，桥只看 `dispatched`，错误没人看 → 消息静默丢失；
/// steer / followUp / nextTurn 也都变成「开新一轮」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_user_message_queues_while_the_lane_is_busy() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    // 慢流：跑着的时候才有窗口把消息排进去。
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.5),
    );
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    let lane = harness.lane("main");
    let running = tokio::spawn({
        let lane = lane.clone();
        async move { lane.prompt_text("go", Vec::new()).await }
    });
    // 等 run 真的被接纳，再发消息。
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if harness
                .snapshot()
                .await
                .map(|s| s.active.is_some())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the run must become active");

    let value = host
        .send_user_message(json!({"text": "插队的消息", "deliverAs": "steer"}))
        .await
        .expect("发消息不能因为 lane 忙就失败");
    assert_eq!(
        value["status"], "queued",
        "跑着时发消息应当进队列，而不是开新一轮或报错：{value}"
    );

    // 队列里真的有它（不是被吞掉）。
    let queued = lane.clear_queue().await.unwrap();
    assert_eq!(queued.steering, vec!["插队的消息".to_string()]);

    let _ = lane.abort().await;
    let _ = running.await;
}
