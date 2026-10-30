use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use rpi_plugin_sdk::{RuntimeActionId, StbString, StbStringRef};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ffi_child_actions_leave_real_parent_transcript_tools_and_config_unchanged() {
    let (host, cell) = HarnessActionHost::new_empty(vec![], std::env::temp_dir(), Handle::current(),
        BTreeMap::new(), rpi_extensions::ToolCallContext::default());
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(), models: vec![provider], ..Default::default()
    }).await.unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    let host = Arc::new(host);
    let bridge = rpi_extensions::ActionBridge::new(Handle::current(), host.clone());
    let before_context = host.get_execution_context(json!({})).await.unwrap();
    let before_tools = host.get_active_tools(json!({})).await.unwrap();
    let before_history = host.get_session_branch(json!({})).await.unwrap();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(json!({
        "sessionId":"trusted-child", "rootSessionId":before_context["sessionId"], "childAgent":false
    })));
    let invocation = scope.run(async { bridge.capture_invocation(tokio_util::sync::CancellationToken::new()) }).await;
    let handle = invocation.user_data() as usize;
    let registration = bridge.user_data() as usize;
    std::thread::spawn(move || {
        for (action, args) in [
            (RuntimeActionId::SendMessage, json!({"text":"must not append", "triggerTurn":false})),
            (RuntimeActionId::SendUserMessage, json!({"text":"must not prompt"})),
            (RuntimeActionId::AppendEntry, json!({"customType":"must-not-append", "data":{"bad":true}})),
            (RuntimeActionId::SetActiveTools, json!({"tools":[]})),
            (RuntimeActionId::SetSessionName, json!({"name":"must-not-change"})),
            (RuntimeActionId::SetThinkingLevel, json!({"level":"high"})),
            (RuntimeActionId::SetModel, json!({"model":"must-not-change"})),
            (RuntimeActionId::GetSessionBranch, json!({})),
            (RuntimeActionId::NewSession, json!({})),
        ] {
            for id in [handle, registration] {
                let mut args = args.clone();
                args["childAgent"] = json!(false);
                args["sessionId"] = json!("root");
                args["runtimeHandle"] = json!(registration);
                let mut out = StbString::empty();
                assert_eq!(rpi_extensions::trampoline_runtime_action(action.into(),
                    StbStringRef::from_str(&args.to_string()), &mut out, id as *mut std::ffi::c_void), 1, "{action:?}");
                unsafe { rpi_extensions::host_free_string(out) };
            }
        }
    }).join().unwrap();
    assert_eq!(host.get_active_tools(json!({})).await.unwrap(), before_tools);
    assert_eq!(host.get_session_branch(json!({})).await.unwrap(), before_history);
    assert_eq!(host.get_execution_context(json!({})).await.unwrap(), before_context);
    drop(invocation);
    // Normal root FFI append still reaches the same production host.
    let mut out = StbString::empty();
    assert_eq!(rpi_extensions::trampoline_runtime_action(RuntimeActionId::AppendEntry.into(),
        StbStringRef::from_str(r#"{"customType":"root-compatible","data":{"ok":true}}"#), &mut out, bridge.user_data()), 0);
    unsafe { rpi_extensions::host_free_string(out) };
    assert_ne!(host.get_session_branch(json!({})).await.unwrap(), before_history);
}
