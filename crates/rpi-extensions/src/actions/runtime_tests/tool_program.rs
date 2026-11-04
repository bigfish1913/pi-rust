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

// Run a real native invocation body around the ABI adapter. Its captured
// ToolProgram must cross spawn_blocking, and resolution must precede destroy.
use crate::tool::{PluginToolAdapter, PluginToolHandle};
use rpi_agent::ToolExecutionMode;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};
struct ReceiptDrive(Value);
extern "C" fn receipt_execute(
    _: StbStringRef,
    args: StbString,
    free: Option<rpi_plugin_sdk::FreeStringFn>,
) -> StepHandle {
    let owned = unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(args, free) };
    let args: Value = serde_json::from_str(&owned.to_string_lossy()).unwrap();
    Box::into_raw(Box::new(ReceiptDrive(args))) as StepHandle
}
extern "C" fn receipt_poll(
    handle: StepHandle,
    _: Option<ToolPartialCb>,
    _: *mut c_void,
) -> StepResult {
    let drive = unsafe { &*(handle as *const ReceiptDrive) };
    StepResult::done(StbString::from_string(drive.0["done"].to_string()))
}
extern "C" fn receipt_cancel(_: StepHandle) {}
extern "C" fn receipt_waker(_: StepHandle, _: Option<rpi_plugin_sdk::ToolWakeCb>, _: *mut c_void) {}
extern "C" fn receipt_destroy(handle: StepHandle) {
    let drive = unsafe { Box::from_raw(handle as *mut ReceiptDrive) };
    let args = drive.0;
    let bridge =
        ActionBridge::resolve_handle(args["__rpi"]["runtimeHandle"].as_u64().unwrap() as usize)
            .unwrap();
    let (rc, _) = invoke(
        &bridge,
        RuntimeActionId::ToolProgram,
        json!({"op":"release_result","resultId":args["resultId"],"__rpiRuntimeKey":args["__rpi"]["runtimeKey"]}),
    );
    assert_eq!(
        rc, 0,
        "destroy must still have authenticated cleanup access"
    );
}
struct ReceiptParent {
    schema: Tool,
    bridge: Arc<ActionBridge>,
    wake: bool,
    error: bool,
    invalid: u8,
}
#[async_trait::async_trait]
impl AgentTool for ReceiptParent {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "receipt parent"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let p = rpi_agent::tool_program::current().unwrap();
        let id = p
            .handle(json!({"op":"start","name":"effect","arguments":{},"resultMode":"reference"}))
            .await
            .unwrap()["requestId"]
            .clone();
        loop {
            let reply = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if reply["status"] != "running" {
                assert_eq!(reply["status"], "done");
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            self.bridge
                .resolve_result_reference(id.as_str().unwrap())
                .is_err(),
            "registration cannot select receipts"
        );
        let mut done = json!({"$rpiToolResult":1,"isError":self.error,"resultReference":{"version":1,"id":id}});
        match self.invalid {
            1 => done["isError"] = json!(!self.error),
            2 => done["result"] = json!({}),
            3 => done["resultReference"]["version"] = json!(2),
            4 => done["resultReference"]["id"] = json!("tp-foreign"),
            5 => done["resultReference"] = json!(null),
            6 => done["resultReference"]["id"] = json!(7),
            7 => done["$rpiToolResult"] = json!(2),
            _ => {}
        }
        let adapter = PluginToolAdapter::new(
            self.schema.clone(),
            PluginToolHandle {
                execute_fn: receipt_execute,
                set_waker_fn: self.wake.then_some(receipt_waker),
                poll_fn: receipt_poll,
                cancel_fn: receipt_cancel,
                destroy_fn: receipt_destroy,
                plugin_free_string: crate::host_free_string,
                runtime_handle: Some(self.bridge.user_data() as usize),
            },
            crate::loader::PluginKeepalive::empty(),
        );
        let result = adapter
            .execute(
                "native-parent",
                json!({"done":done,"resultId":id}),
                signal,
                update,
            )
            .await;
        assert!(
            p.resolve_result_reference(id.as_str().unwrap()).is_err(),
            "destroy released receipt after resolution"
        );
        if self.invalid != 0 {
            assert!(matches!(result, Err(AgentError::Tool(_))), "{result:?}");
        }
        result
    }
}
struct ReceiptEffect {
    schema: Tool,
    error: bool,
    empty: bool,
}
#[async_trait::async_trait]
impl AgentTool for ReceiptEffect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "native receipt"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let mut usage = rpi_ai::types::Usage::zero();
        usage.total_tokens = 3;
        let native = AgentToolResult {
            content: if self.empty {
                vec![]
            } else {
                vec![rpi_agent::TextContentOrImage::Text(serde_json::from_value(json!({"type":"text","text":"native 😀".repeat(200_000),"textSignature":"native-signature"})).unwrap())]
            },
            details: json!({"business":{"runtimeKey":"customer-owned"},"large":"é".repeat(600_000)}),
            usage: Some(usage),
            added_tool_names: vec!["native-added".into()],
            terminate: true,
        };
        if self.error {
            Err(AgentError::StructuredTool(Box::new(native)))
        } else {
            Ok(native)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_receipt_attachment_crosses_both_drivers_before_destroy_with_authoritative_accounting(
) {
    for wake in [false, true] {
        for (error, empty, invalid) in [
            (false, false, 0),
            (true, false, 0),
            (false, true, 0),
            (true, true, 0),
            (false, true, 1),
            (false, true, 2),
            (false, true, 3),
            (false, true, 4),
            (false, true, 5),
            (false, true, 6),
            (false, true, 7),
        ] {
            let bridge = ActionBridge::new(Handle::current(), Arc::new(Host::default()))
                .for_plugin("receipt");
            let schema: Tool =
                serde_json::from_value(json!({"name":"codemode","description":"program",
                "parameters":{"type":"object","x-rpi-result-envelope":1}}))
                .unwrap();
            let effect = ReceiptEffect {
                schema: serde_json::from_value(
                    json!({"name":"effect","description":"native","parameters":{"type":"object"}}),
                )
                .unwrap(),
                error,
                empty,
            };
            let context = rpi_agent::AgentContext {
                tools: vec![
                    Arc::new(ReceiptParent {
                        schema,
                        bridge,
                        wake,
                        error,
                        invalid,
                    }),
                    Arc::new(effect),
                ],
                ..Default::default()
            };
            let (events, messages) = agent_common::run_and_collect(
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
            let results: Vec<_> = messages
                .iter()
                .filter_map(|m| match m {
                    rpi_agent::AgentMessage::ToolResult(r) => Some(r),
                    _ => None,
                })
                .collect();
            assert_eq!(
                results.len(),
                1,
                "receipts cannot forge transcript tool calls"
            );
            let result = results[0];
            assert_eq!(result.is_error, error || invalid != 0);
            assert_eq!(
                result.usage.as_ref().unwrap().total_tokens,
                3,
                "no attachment double count"
            );
            assert_eq!(result.added_tool_names, vec!["native-added"]);
            let end = events
                .iter()
                .find_map(|e| match e {
                    rpi_agent::AgentEvent::ToolExecutionEnd { result, .. } => Some(result),
                    _ => None,
                })
                .unwrap();
            assert!(
                end.terminate,
                "finalized termination survives selection failure"
            );
            if invalid == 0 {
                assert_eq!(end.details["business"]["runtimeKey"], "customer-owned");
                assert_eq!(result.content.len(), if empty { 0 } else { 1 });
                if !empty {
                    let block = serde_json::to_value(&result.content[0]).unwrap();
                    assert_eq!(block["textSignature"], "native-signature");
                    assert!(block["text"].as_str().unwrap().len() > 1024 * 1024);
                }
            }
        }
    }
}
