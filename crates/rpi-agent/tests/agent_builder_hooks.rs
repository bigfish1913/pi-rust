//! Raw AgentBuilder forwards the original loop callbacks without a Session.
//! These are SDK regressions, not production Host/binary/DLL acceptance.
#[path = "common/mod.rs"]
mod common;

use common::{assistant_text, assistant_tool_calls, mock_model, mock_stream_fn};
use rpi_agent::{
    async_stream_fn, AfterToolCallResult, AgentBuilder, AgentError, AgentEvent, AgentMessage,
    AgentOptions, AgentTool, AgentToolResult, BeforeToolCallResult,
};
use rpi_ai::{AssistantMessage, Model, ProviderHooks, StopReason, Tool};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Echo {
    schema: Tool,
    calls: Arc<AtomicUsize>,
    fail: bool,
}
#[async_trait::async_trait]
impl AgentTool for Echo {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "echo"
    }
    async fn execute(
        &self,
        _: &str,
        args: serde_json::Value,
        _: CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(args["value"], "policy");
        if self.fail {
            Err(AgentError::tool("original tool failure"))
        } else {
            Ok(AgentToolResult::text("original tool result"))
        }
    }
}

struct CountTerminal(Arc<AtomicUsize>);
impl ProviderHooks for CountTerminal {
    fn after_response(&self, _: &Model, _: &AssistantMessage) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct WrongTerminal;
impl ProviderHooks for WrongTerminal {
    fn after_response(&self, _: &Model, _: &AssistantMessage) {
        panic!("the last builder setter must win");
    }
}

#[test]
fn agent_options_struct_literal_remains_source_compatible() {
    let options = AgentOptions {
        initial_state: None,
        convert_to_llm: None,
        stream_fn: None,
        thinking_level: None,
        queue_mode: None,
        follow_up_mode: None,
        tool_execution: None,
        session_id: None,
    };
    assert!(options.session_id.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn builder_forwards_tool_policy_result_overrides_and_legacy_terminal_hooks() {
    for mode in ["allowed", "blocked", "tool-error"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let before_calls = Arc::new(AtomicUsize::new(0));
        let after_calls = Arc::new(AtomicUsize::new(0));
        let terminal_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let script = mock_stream_fn(vec![
            assistant_tool_calls(
                vec![("call", "echo", serde_json::json!({"value":"original"}))],
                StopReason::ToolUse,
            ),
            assistant_text("done", StopReason::Stop),
        ]);
        let factory = async_stream_fn({
            let requests = requests.clone();
            move |model, context, options| {
                let script = script.clone();
                let requests = requests.clone();
                Box::pin(async move {
                    assert!(options.session_id.is_none());
                    tokio::task::yield_now().await;
                    assert!(options.session_id.is_none());
                    if requests.fetch_add(1, Ordering::SeqCst) == 1 {
                        let result = context.messages.last().unwrap();
                        let encoded = serde_json::to_value(result).unwrap();
                        assert_eq!(encoded["role"], "toolResult");
                        assert_eq!(encoded["isError"], mode == "blocked");
                        assert_eq!(
                            encoded["content"][0]["text"],
                            if mode == "blocked" {
                                "policy veto"
                            } else {
                                "transformed"
                            }
                        );
                    }
                    script(model, context, options)
                })
            }
        });
        let agent = AgentBuilder::new()
            .model(mock_model())
            .tools(vec![Arc::new(Echo {
                schema: Tool {
                    name: "echo".into(),
                    description: "echo".into(),
                    parameters: rpi_ai::Schema::new(serde_json::json!({
                        "type":"object", "properties":{"value":{"type":"string"}},
                        "required":["value"], "additionalProperties":false
                    })),
                    constrained_sampling: None,
                },
                calls: calls.clone(),
                fail: mode == "tool-error",
            })])
            .async_stream_fn(factory)
            .before_tool_call(Arc::new(|_, _| panic!("replaced before hook")))
            .before_tool_call(Arc::new({
                let before_calls = before_calls.clone();
                move |context, signal| {
                    assert!(!signal.is_cancelled());
                    assert_eq!(context.args["value"], "original");
                    before_calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        Some(BeforeToolCallResult {
                            block: mode == "blocked",
                            reason: Some("policy veto".into()),
                            args: Some(serde_json::json!({"value":"policy"})),
                            ..Default::default()
                        })
                    })
                }
            }))
            .after_tool_call(Arc::new(|_, _| panic!("replaced after hook")))
            .after_tool_call(Arc::new({
                let after_calls = after_calls.clone();
                move |context, signal| {
                    assert!(!signal.is_cancelled());
                    assert_eq!(context.args["value"], "policy");
                    assert_eq!(context.is_error, mode == "tool-error");
                    after_calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        tokio::task::yield_now().await;
                        Some(AfterToolCallResult {
                            content: Some(AgentToolResult::text("transformed").content),
                            details: Some(serde_json::json!({"policy":"applied"})),
                            is_error: Some(false),
                            ..Default::default()
                        })
                    })
                }
            }))
            .provider_hooks(Arc::new(WrongTerminal))
            .provider_hooks(Arc::new(CountTerminal(terminal_calls.clone())))
            .build()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), agent.prompt("hello {{literal}}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(terminal_calls.load(Ordering::SeqCst), 2);
        assert_eq!(before_calls.load(Ordering::SeqCst), 1);
        // Preserve the original veto contract: no executor or second transform.
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(mode != "blocked"));
        assert_eq!(
            after_calls.load(Ordering::SeqCst),
            usize::from(mode != "blocked")
        );
        let state = agent.state();
        assert!(!state.is_streaming);
        assert_eq!(state.messages.len(), 4);
        let user = serde_json::to_value(&state.messages[0]).unwrap();
        assert_eq!(user["content"], "hello {{literal}}");
        if mode != "blocked" {
            let result = serde_json::to_value(&state.messages[2]).unwrap();
            assert_eq!(result["details"]["policy"], "applied");
        }
    }
}

struct GatedTerminal {
    entered: Arc<tokio::sync::Notify>,
    receiver: Mutex<Option<mpsc::Receiver<()>>>,
    completed: Arc<AtomicBool>,
    expected: StopReason,
}
impl ProviderHooks for GatedTerminal {
    fn after_response(&self, _: &Model, _: &AssistantMessage) {
        panic!("AgentBuilder must forward the awaited terminal entry");
    }
    fn after_response_async<'a>(
        &'a self,
        model: &'a Model,
        message: &'a AssistantMessage,
    ) -> rpi_ai::provider::ProviderHookFuture<'a, ()> {
        let receiver = self.receiver.lock().unwrap().take().unwrap();
        let entered = self.entered.clone();
        let completed = self.completed.clone();
        let expected = self.expected;
        Box::pin(async move {
            assert_eq!(message.stop_reason, expected);
            let model_id = model.id.clone();
            tokio::task::spawn_blocking(move || {
                entered.notify_one();
                receiver.recv_timeout(Duration::from_secs(10)).unwrap();
                completed.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();
            assert_eq!(model.id, model_id, "model borrow survives worker wait");
            assert_eq!(message.stop_reason, expected);
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn builder_terminal_hooks_hold_publication_for_success_fallback_error_and_abort() {
    for mode in ["terminal", "result-only", "error", "cancel"] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let opened = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let (release, receiver) = mpsc::channel();
        let reason = match mode {
            "error" => StopReason::Error,
            "cancel" => StopReason::Aborted,
            _ => StopReason::Stop,
        };
        let script = mock_stream_fn(vec![assistant_text("finished", reason)]);
        let factory = async_stream_fn({
            let opened = opened.clone();
            move |model, context, options| {
                let script = script.clone();
                let opened = opened.clone();
                Box::pin(async move {
                    assert!(options.session_id.is_none());
                    opened.notify_one();
                    if mode == "cancel" {
                        std::future::pending::<()>().await;
                    }
                    let mut stream = script(model, context, options);
                    if mode == "result-only" {
                        while stream.next().await.is_some() {}
                    }
                    stream
                })
            }
        });
        let agent = AgentBuilder::new()
            .model(mock_model())
            .async_stream_fn(factory)
            .provider_hooks(Arc::new(GatedTerminal {
                entered: entered.clone(),
                receiver: Mutex::new(Some(receiver)),
                completed: completed.clone(),
                expected: reason,
            }))
            .build()
            .unwrap();
        let mut events = agent.subscribe();
        let running = agent.clone();
        let run = tokio::spawn(async move { running.prompt("hello").await });
        tokio::time::timeout(Duration::from_secs(5), opened.notified())
            .await
            .unwrap();
        if mode == "cancel" {
            agent.abort();
        }
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(!run.is_finished(), "{mode}: worker still owns the callback");
        assert!(!completed.load(Ordering::SeqCst));
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::Assistant(_),
                        ..
                    } | AgentEvent::AgentEnd { .. }
                ),
                "{mode}: terminal event escaped the worker barrier"
            );
        }
        // A real blocking worker must not park this single async executor.
        tokio::time::sleep(Duration::from_millis(1)).await;
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        agent.wait_for_idle().await;
        assert!(completed.load(Ordering::SeqCst));
        let mut terminal_messages = 0;
        while let Ok(event) = events.try_recv() {
            if let AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
                ..
            } = event
            {
                assert_eq!(message.stop_reason, reason);
                terminal_messages += 1;
            }
        }
        assert_eq!(terminal_messages, 1, "{mode}: terminal observer fires once");
        assert!(!agent.state().is_streaming);
    }
}
