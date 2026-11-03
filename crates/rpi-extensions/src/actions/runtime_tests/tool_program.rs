use super::*;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::{StopReason, Tool};
fn invoke(bridge: &Arc<ActionBridge>, action: RuntimeActionId, args: Value) -> (i32, Value) {
    let mut out = StbString::empty();
    let text = args.to_string();
    let rc = trampoline_runtime_action(
        action.into(),
        StbStringRef::from_str(&text),
        &mut out,
        bridge.user_data(),
    );
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
    };
    (rc, serde_json::from_str(&owned.to_string_lossy()).unwrap())
}
struct Probe {
    schema: Tool,
    bridge: Arc<ActionBridge>,
}
#[async_trait::async_trait]
impl AgentTool for Probe {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "probe"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let driver_signal = signal.child_token();
        let invocation = self.bridge.for_invocation(true, driver_signal.clone());
        let registration = self.bridge.clone();
        tokio::task::spawn_blocking(move || {
            let op = RuntimeActionId::ToolProgram;
            assert_eq!(invoke(&registration, op, json!({"op":"list"})).0, 1);
            assert_eq!(invoke(&invocation, op, json!({"op":"list"})).0, 1);
            assert_eq!(
                invoke(
                    &invocation,
                    op,
                    json!({"op":"list","__rpiRuntimeKey":"forged"})
                )
                .0,
                1
            );
            let key = invocation.invocation_key().unwrap();
            let (rc, result) = invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key}));
            assert_eq!(rc, 0);
            assert_eq!(result["protocolVersion"], 1);
            assert_eq!(result["tools"], json!([]));
            // Names-only legacy action is scoped too; it must not reach this mock's root getter.
            assert_eq!(
                invoke(&invocation, RuntimeActionId::GetActiveTools, json!({})).1,
                json!({"tools":["codemode"]})
            );
            {
                let _guard = invocation.enter_invocation();
                assert_eq!(invoke(&registration, op, json!({"op":"list"})).0, 0);
            }
            driver_signal.cancel();
            assert_eq!(
                invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key})).0,
                1
            );
            // Cleanup is still authenticated and available after cancellation.
            assert_eq!(
                invoke(
                    &invocation,
                    op,
                    json!({"op":"dispose","requestId":"unused","__rpiRuntimeKey":key})
                )
                .0,
                0
            );
            registration.invalidate();
            assert_eq!(
                invoke(&invocation, op, json!({"op":"list","__rpiRuntimeKey":key})).0,
                1
            );
        })
        .await
        .unwrap();
        Ok(AgentToolResult::text("done"))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_program_authentication_and_narrowed_ffi_binding() {
    let bridge =
        ActionBridge::new(Handle::current(), Arc::new(Host::default())).for_plugin("codemode");
    let schema: Tool = serde_json::from_value(
        json!({"name":"codemode","description":"program","parameters":{"type":"object"}}),
    )
    .unwrap();
    let context = rpi_agent::AgentContext {
        tools: vec![Arc::new(Probe { schema, bridge })],
        ..Default::default()
    };
    let (_, messages) = agent_common::run_and_collect(
        vec![agent_common::user_message("run")],
        context,
        agent_common::base_config(),
        agent_common::mock_stream_fn(vec![
            agent_common::assistant_tool_calls(
                vec![("p", "codemode", json!({}))],
                StopReason::ToolUse,
            ),
            agent_common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m,rpi_agent::AgentMessage::ToolResult(r) if r.is_error)),
        "{messages:?}"
    );
}
