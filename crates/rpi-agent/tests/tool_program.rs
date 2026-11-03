//! Offline action-30 runtime safety and normal transcript isolation.
#[path = "common/mod.rs"]
mod common;
use futures::future::BoxFuture;
use rpi_agent::{
    tool_program::{current, ToolProgram},
    AfterToolCallResult, AgentContext, AgentError, AgentTool, AgentToolResult,
    BeforeToolCallResult, ToolExecutionMode, ToolResultPartial,
};
use rpi_ai::types::{StopReason, Tool};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;
type Body = Arc<dyn Fn(Arc<ToolProgram>) -> BoxFuture<'static, ()> + Send + Sync>;
struct Program {
    schema: Tool,
    body: Body,
    mode: ToolExecutionMode,
}
fn schema(name: &str) -> Tool {
    serde_json::from_value(json!({"name":name,"description":"test tool","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}})).unwrap()
}
#[async_trait::async_trait]
impl AgentTool for Program {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "program"
    }
    fn execution_mode(&self) -> rpi_agent::ToolExecutionMode {
        self.mode
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        (self.body)(current().expect("native invocation")).await;
        Ok(AgentToolResult::text("program done"))
    }
}
struct Effect {
    schema: Tool,
    count: Arc<AtomicUsize>,
    delay: u64,
    sequential: bool,
}
#[async_trait::async_trait]
impl AgentTool for Effect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "effect"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        if self.sequential {
            ToolExecutionMode::Sequential
        } else {
            ToolExecutionMode::Parallel
        }
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay)).await;
        Ok(AgentToolResult::text("effect done"))
    }
}
async fn poll(p: &Arc<ToolProgram>, id: &Value) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let r = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if r["status"] != "running" {
                return r;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn start_result(p: &Arc<ToolProgram>) -> Result<Value, String> {
    p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"}}))
        .await
}
async fn start(p: &Arc<ToolProgram>) -> Value {
    p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"}}))
        .await
        .unwrap()["requestId"]
        .clone()
}
async fn run(body: Body, config: rpi_agent::AgentLoopConfig, count: Arc<AtomicUsize>, delay: u64) {
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(Program {
            schema: schema("codemode"),
            body,
            mode: ToolExecutionMode::Sequential,
        }),
        Arc::new(Effect {
            schema: schema("effect"),
            count,
            delay,
            sequential: true,
        }),
    ];
    run_tools(tools, config).await;
}
async fn run_tools(tools: Vec<Arc<dyn AgentTool>>, config: rpi_agent::AgentLoopConfig) {
    let context = AgentContext {
        tools,
        ..Default::default()
    };
    let (events, messages) = common::run_and_collect(
        vec![common::user_message("run")],
        context,
        config,
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![("parent", "codemode", json!({"value":"body"}))],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m,rpi_agent::AgentMessage::ToolResult(r) if r.is_error)),
        "outer tool failed: {messages:?}"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, rpi_agent::AgentMessage::ToolResult(_)))
            .count(),
        1,
        "nested calls must not become transcript messages"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, rpi_agent::AgentEvent::ToolExecutionStart { .. }))
            .count(),
        1,
        "no nested ordinary events"
    );
}
#[tokio::test]
async fn catalog_subset_recursion_validation_hooks_and_finalized_result() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut cfg = common::base_config();
    cfg.before_tool_call = Some(Arc::new(|ctx, _| {
        let block = ctx.tool_call.name == "effect" && ctx.args["value"] == "blocked";
        Box::pin(async move {
            block.then(|| BeforeToolCallResult {
                block: true,
                reason: Some("approval denied".into()),
                ..Default::default()
            })
        })
    }));
    cfg.after_tool_call = Some(Arc::new(|ctx, _| {
        let effect = ctx.tool_call.name == "effect" && !ctx.is_error;
        Box::pin(async move {
            effect.then(|| AfterToolCallResult {
                details: Some(json!({"finalized":true})),
                content: Some(vec![rpi_agent::TextContentOrImage::Image(
                    serde_json::from_value(json!({"type":"image", "data":"AA==", "mimeType":"image/png"})).unwrap()
                )]),
                terminate: Some(true),
                usage: Some(serde_json::from_value(json!({
                    "input":1, "output":2, "cacheRead":0, "cacheWrite":0, "totalTokens":3,
                    "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
                })).unwrap()),
                ..Default::default()
            })
        })
    }));
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let page = p.handle(json!({"op":"list","limit":1})).await.unwrap();
                assert_eq!(page["protocolVersion"], 1);
                assert_eq!(page["tools"][0]["name"], "effect");
                assert!(page["tools"][0]["parameters"].is_object());
                assert!(page["nextOffset"].is_null());
                for name in ["codemode", "root-secret", "__rpiRuntimeKey"] {
                    assert!(p
                        .handle(json!({"op":"start","name":name,"arguments":{}}))
                        .await
                        .is_err());
                }
                assert!(p
                    .handle(json!({"op":"start","name":"effect","arguments":[]}))
                    .await
                    .is_err());
                for arguments in [json!({}), json!({"value":"blocked"})] {
                    let id = p
                        .handle(json!({"op":"start","name":"effect","arguments":arguments}))
                        .await
                        .unwrap()["requestId"]
                        .clone();
                    let reply = poll(&p, &id).await;
                    assert_eq!(reply["status"], "done");
                    assert_eq!(reply["isError"], true);
                }
                let id = start(&p).await;
                let reply = poll(&p, &id).await;
                assert_eq!(reply["isError"], false);
                assert_eq!(reply["result"]["details"]["finalized"], true);
                assert_eq!(reply["result"]["content"][0]["type"], "image");
                assert_eq!(reply["result"]["content"][0]["data"], "AA==");
                assert_eq!(reply["result"]["usage"]["totalTokens"], 3);
                assert_eq!(reply["result"]["terminate"], true);
                assert!(start_result(&p).await.is_err());
            })
        }),
        cfg,
        count.clone(),
        0,
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn sequential_parent_does_not_deadlock_parallel_requests_and_stale_capability_closes() {
    let saved = Arc::new(Mutex::new(None));
    let out = saved.clone();
    let mut cfg = common::base_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;
    let count = Arc::new(AtomicUsize::new(0));
    run(
        Arc::new(move |p| {
            let out = out.clone();
            Box::pin(async move {
                let a = start(&p).await;
                let b = start(&p).await;
                assert_ne!(a, b);
                assert_eq!(poll(&p, &a).await["status"], "done");
                assert_eq!(poll(&p, &b).await["status"], "done");
                *out.lock().unwrap() = Some(p);
            })
        }),
        cfg,
        count.clone(),
        2,
    )
    .await;
    let p = saved.lock().unwrap().take().unwrap();
    assert!(p.handle(json!({"op":"list"})).await.is_err());
    assert!(p
        .handle(json!({"op":"start","name":"effect","arguments":{}}))
        .await
        .is_err());
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn cancellation_timeout_retention_active_limits_and_cleanup() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let first = p.handle(json!({
                "op": "start", "name": "effect", "arguments": {"value": "ok"}, "timeoutMs": 1
            })).await.unwrap()["requestId"].clone();
                assert_eq!(poll(&p, &first).await["status"], "failed");
                // A timed-out noncooperative native future still owns its permit.
                p.handle(json!({"op":"dispose", "requestId": first}))
                    .await
                    .unwrap();
                let mut ids = Vec::new();
                for _ in 0..7 {
                    ids.push(start(&p).await);
                }
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}
                    }))
                    .await
                    .unwrap_err()
                    .contains("limit"));
                p.cancel_all();
                assert!(p.handle(json!({"op":"list"})).await.is_err());
                for id in ids {
                    assert_eq!(poll(&p, &id).await["status"], "failed");
                    for _ in 0..2 {
                        p.handle(json!({"op":"dispose", "requestId": id}))
                            .await
                            .unwrap();
                    }
                }
                assert!(p
                    .handle(json!({"op":"poll", "requestId":"forged"}))
                    .await
                    .is_err());
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        200,
    )
    .await;
}

#[tokio::test]
async fn timed_out_future_retains_sequential_gate_through_cleanup() {
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    run(
        Arc::new(move |p| {
            let observed = observed.clone();
            Box::pin(async move {
                let first = p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}, "timeoutMs":10
                    }))
                    .await
                    .unwrap()["requestId"]
                    .clone();
                assert_eq!(poll(&p, &first).await["status"], "failed");
                assert_eq!(observed.load(Ordering::SeqCst), 1);
                let second = start(&p).await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                assert_eq!(
                    observed.load(Ordering::SeqCst),
                    1,
                    "cleanup still owns exclusive gate"
                );
                assert_eq!(poll(&p, &second).await["status"], "done");
            })
        }),
        common::base_config(),
        count.clone(),
        100,
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn total_starts_budget_and_argument_bounds() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"x".repeat(1024*1024)}
                    }))
                    .await
                    .is_err());
                for _ in 0..128 {
                    let id = start(&p).await;
                    assert_eq!(poll(&p, &id).await["status"], "done");
                    p.handle(json!({"op":"dispose", "requestId":id}))
                        .await
                        .unwrap();
                }
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}
                    }))
                    .await
                    .unwrap_err()
                    .contains("budget"));
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        0,
    )
    .await;
}

#[tokio::test]
async fn parallel_program_admission_fails_closed() {
    run_tools(
        vec![Arc::new(Program {
            schema: schema("codemode"),
            mode: ToolExecutionMode::Parallel,
            body: Arc::new(|p| {
                Box::pin(async move {
                    assert!(p.handle(json!({"op":"list"})).await.is_ok());
                    let error = p
                        .handle(json!({"op":"start", "name":"effect", "arguments":{}}))
                        .await
                        .unwrap_err();
                    assert!(error.contains("Sequential"));
                })
            }),
        })],
        common::base_config(),
    )
    .await;
}

#[tokio::test]
async fn nested_programs_share_active_budget_and_preserve_child_scope() {
    let child = Arc::new(
        rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(
            json!({"tools":["codemode", "nested", "effect"]}),
        ),
    );
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(Program {
            schema: schema("codemode"),
            mode: ToolExecutionMode::Sequential,
            body: Arc::new(|p| {
                Box::pin(async move {
                    let id = p
                        .handle(json!({"op":"start", "name":"nested", "arguments":{"value":"ok"}}))
                        .await
                        .unwrap()["requestId"]
                        .clone();
                    assert_eq!(poll(&p, &id).await["isError"], false);
                })
            }),
        }),
        Arc::new(Program {
            schema: schema("nested"),
            mode: ToolExecutionMode::Sequential,
            body: Arc::new(|p| {
                Box::pin(async move {
                    assert!(rpi_agent::tool_execution_scope::is_child_execution());
                    assert_eq!(
                        rpi_agent::tool_execution_scope::execution_context().unwrap()["tools"],
                        json!(["codemode", "nested", "effect"])
                    );
                    let catalog = p.handle(json!({"op":"list"})).await.unwrap();
                    assert_eq!(catalog["tools"].as_array().unwrap().len(), 1);
                    assert_eq!(catalog["tools"][0]["name"], "effect");
                    let mut ids = Vec::new();
                    for _ in 0..7 {
                        ids.push(start(&p).await);
                    }
                    assert!(start_result(&p)
                        .await
                        .unwrap_err()
                        .contains("invocation-tree"));
                    for id in ids {
                        assert_eq!(poll(&p, &id).await["status"], "done");
                    }
                })
            }),
        }),
        Arc::new(Effect {
            schema: schema("effect"),
            count: Arc::new(AtomicUsize::new(0)),
            delay: 0,
            sequential: true,
        }),
    ];
    child.run(run_tools(tools, common::base_config())).await;
    child.wait_idle().await;
}

#[tokio::test]
async fn driver_signal_cancels_already_started_native_work() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let driver = CancellationToken::new();
                p.bind_signal(driver.clone());
                let id = start(&p).await;
                driver.cancel();
                assert_eq!(poll(&p, &id).await["status"], "failed");
                assert!(start_result(&p).await.is_err());
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        10,
    )
    .await;
}

#[tokio::test]
async fn outer_program_waits_for_timed_out_native_cleanup_before_returning() {
    let began = std::time::Instant::now();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    run(Arc::new(move |p| {
        let observed = observed.clone();
        Box::pin(async move {
            let id = p.handle(json!({"op":"start", "name":"effect", "arguments":{"value":"ok"}, "timeoutMs":10}))
                .await.unwrap()["requestId"].clone();
            assert_eq!(poll(&p, &id).await["status"], "failed");
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            // Return without waiting for the cancelled native effect.
        })
    }), common::base_config(), count, 100).await;
    assert!(began.elapsed() >= std::time::Duration::from_millis(100));
}
