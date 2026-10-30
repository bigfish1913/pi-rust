// Included in modes::tests; kept separate from parent-owned bridge tests.
#[tokio::test]
async fn rpc_system_prompt_resend_reload_and_legacy() {
    use crate::rpc::system_prompt::*;
    let (harness, provider) = rpc_test_harness("unused", 0.0).await;
    harness.set_system_prompt(Some("native".into())).await.unwrap();
    let state = Arc::new(tokio::sync::Mutex::new(RpcModeState {
        current: "default".into(), base_prompt: None, base_prompt_saved: false, base_tools: None,
    }));
    let lane = harness.lane("main");
    let first = serde_json::json!({"prompt":"first"});
    rpc_set_system_prompt(&lane, &harness, &state, &first).await.unwrap();
    let leaf = harness.session().get_leaf_id().await.unwrap();
    rpc_set_system_prompt(&lane, &harness, &state, &first).await.unwrap();
    assert_eq!(leaf, harness.session().get_leaf_id().await.unwrap());
    // A fresh harness on the native session models load/resume (no process cache).
    let restored = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(), models: vec![provider],
        session: harness.session().clone(), allow_existing_session: true,
        system_prompt: Some("different startup base".into()), ..Default::default()
    }).await.unwrap();
    restore_system_prompt(&restored).await.unwrap();
    let restored_lane = restored.lane("main");
    rpc_set_system_prompt(&restored_lane, &restored, &state, &first).await.unwrap();
    assert_eq!(leaf, restored.session().get_leaf_id().await.unwrap());
    rpc_set_system_prompt(&restored_lane, &restored, &state, &serde_json::json!({"prompt":"second"})).await.unwrap();
    assert_eq!(restored.get_system_prompt().await.unwrap().as_deref(), Some("native\n\nsecond"));
    rpc_set_system_prompt(&restored_lane, &restored, &state, &serde_json::json!({"prompt":""})).await.unwrap();
    assert_eq!(restored.get_system_prompt().await.unwrap().as_deref(), Some("native"));
    restored.append_custom_entry(SYSTEM_PROMPT_ENTRY, Some(serde_json::json!({"version":1,"prompt":"legacy effective"}))).await.unwrap();
    restore_system_prompt(&restored).await.unwrap();
    let legacy_leaf = restored.session().get_leaf_id().await.unwrap();
    assert!(rpc_set_system_prompt(&restored_lane, &restored, &state, &first).await.is_err());
    assert_eq!(legacy_leaf, restored.session().get_leaf_id().await.unwrap());
    assert_eq!(restored.get_system_prompt().await.unwrap().as_deref(), Some("legacy effective"));
    rpc_set_system_prompt(&restored_lane, &restored, &state, &serde_json::json!({"prompt":"", "mode":"replace"})).await.unwrap();
    restore_system_prompt(&restored).await.unwrap();
    assert_eq!(restored.get_system_prompt().await.unwrap().as_deref(), Some(""));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_system_prompt_provider_gets_append_exactly_once() {
    use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep, faux_assistant_message};
    use tokio::io::AsyncWriteExt;
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::factory(|context| {
        assert_eq!(context.system_prompt.as_deref(), Some("native defaults\n\nextra instruction"));
        faux_assistant_message("done", StopReason::Stop)
    })]);
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        model: provider.default_model().clone(), models: vec![provider],
        system_prompt: Some("native defaults".into()), ..Default::default()
    }).await.unwrap();
    let (mut client, output, task) = start_test_rpc(harness.clone(), rpi_extensions::UiDialogMailbox::new());
    client.write_all(b"{\"type\":\"set_system_prompt\",\"id\":1,\"prompt\":\"extra instruction\"}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 1).await["success"], true);
    // Restoring a snapshot must not append the instruction a second time.
    crate::rpc::system_prompt::restore_system_prompt(&harness).await.unwrap();
    client.write_all(b"{\"type\":\"set_system_prompt\",\"id\":4,\"prompt\":\"extra instruction\"}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 4).await["success"], true);
    client.write_all(b"{\"type\":\"get_state\",\"id\":2}\n{\"type\":\"prompt\",\"id\":3,\"message\":\"test\"}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 2).await["data"]["systemPrompt"], "native defaults\n\nextra instruction");
    client.shutdown().await.unwrap();
    assert_eq!(task.await.unwrap(), 0);
    assert!(output.messages().iter().any(|m| m["type"] == "agent_settled" && m["outcome"] == "completed"));
    assert!(rpc_protocol::SUPPORTED_COMMANDS.contains(&"set_system_prompt"));
}

#[tokio::test]
async fn rpc_system_prompt_validation_replace_and_bounds() {
    let (harness, _) = rpc_test_harness("unused", 0.0).await;
    harness.set_system_prompt(Some("defaults".into())).await.unwrap();
    let lane = harness.lane("main");
    let state = Arc::new(tokio::sync::Mutex::new(RpcModeState {
        current: "default".into(), base_prompt: None, base_prompt_saved: false, base_tools: None,
    }));
    for invalid in [serde_json::json!({}), serde_json::json!({"prompt":false}),
        serde_json::json!({"prompt":"x", "mode":"invalid"}),
        serde_json::json!({"prompt":"x", "mode":null}),
        serde_json::json!({"prompt":"é".repeat(crate::rpc::system_prompt::MAX_SYSTEM_PROMPT_BYTES / 2 + 1)})] {
        assert!(rpc_set_system_prompt(&lane, &harness, &state, &invalid).await.is_err());
        assert_eq!(harness.get_system_prompt().await.unwrap().as_deref(), Some("defaults"));
    }
    rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":"replacement", "mode":"replace"})).await.unwrap();
    assert_eq!(harness.get_system_prompt().await.unwrap().as_deref(), Some("replacement"));
    state.lock().await.current = "plan".into();
    assert!(rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":"no"})).await.is_err());
    state.lock().await.current = "default".into();
    let max = "x".repeat(crate::rpc::system_prompt::MAX_SYSTEM_PROMPT_BYTES);
    rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":max, "mode":"replace"})).await.unwrap();
    rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":"overflow"})).await.unwrap();
    assert_eq!(harness.get_system_prompt().await.unwrap().as_deref(), Some("defaults\n\noverflow"));
    let too_large = "x".repeat(crate::rpc::system_prompt::MAX_SYSTEM_PROMPT_BYTES - 1);
    assert!(rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":too_large})).await.is_err());
    rpc_set_system_prompt(&lane, &harness, &state, &serde_json::json!({"prompt":"", "mode":"replace"})).await.unwrap();
    assert_eq!(harness.get_system_prompt().await.unwrap().as_deref(), Some(""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_system_prompt_rejects_active_prompt() {
    use tokio::io::AsyncWriteExt;
    let (harness, _) = rpc_test_harness("slow output", 0.1).await;
    harness.set_system_prompt(Some("defaults".into())).await.unwrap();
    let (mut client, output, task) = start_test_rpc(harness.clone(), rpi_extensions::UiDialogMailbox::new());
    client.write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n{\"type\":\"set_system_prompt\",\"id\":2,\"prompt\":\"not allowed\"}\n").await.unwrap();
    assert_eq!(output.wait_for(|m| m["id"] == 2).await["success"], false);
    assert_eq!(harness.get_system_prompt().await.unwrap().as_deref(), Some("defaults"));
    assert!(crate::rpc::system_prompt::persisted_system_prompt(&harness).await.unwrap().is_none());
    client.write_all(b"{\"type\":\"abort\",\"id\":3}\n").await.unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(task.await.unwrap(), 0);
}
