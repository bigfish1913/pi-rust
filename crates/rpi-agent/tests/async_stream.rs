//! Both opening contracts feed the same loop; native async works on one worker.
#[path = "common/mod.rs"]
mod common;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use common::{
    assistant_text, assistant_tool_calls, base_config, mock_model, mock_stream_fn, user_message,
};
use rpi_agent::{
    async_stream_fn, AgentBuilder, AgentContext, AgentEvent, AgentMessage, AgentTool,
    AgentToolResult, AsyncStreamFn, CollectorEmitter, StreamFn, StreamSource,
};
use rpi_ai::{Content, StopReason, Tool};
use tokio_util::sync::CancellationToken;

fn yielding_factory(script: StreamFn, seen: Arc<Mutex<Vec<usize>>>) -> AsyncStreamFn {
    async_stream_fn(move |model, context, options| {
        let script = script.clone();
        let seen = seen.clone();
        Box::pin(async move {
            let pointer = context.messages.as_ptr() as usize;
            tokio::task::yield_now().await;
            assert_eq!(pointer, context.messages.as_ptr() as usize);
            seen.lock().unwrap().push(context.messages.len());
            script(model, context, options)
        })
    })
}

struct Echo(Tool);
#[async_trait::async_trait]
impl AgentTool for Echo {
    fn schema(&self) -> &Tool {
        &self.0
    }
    fn label(&self) -> &str {
        "echo"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        Ok(AgentToolResult::text("ok"))
    }
}
fn context() -> AgentContext {
    AgentContext {
        tools: vec![Arc::new(Echo(Tool {
            name: "echo".into(),
            description: "echo".into(),
            parameters: rpi_ai::Schema::new(serde_json::json!({"type":"object","properties":{}})),
            constrained_sampling: None,
        }))],
        ..Default::default()
    }
}
fn script() -> StreamFn {
    mock_stream_fn(vec![
        assistant_tool_calls(
            vec![("call", "echo", serde_json::json!({}))],
            StopReason::ToolUse,
        ),
        assistant_text("done", StopReason::Stop),
    ])
}

#[tokio::test]
async fn async_open_borrows_across_yield_and_matches_sync_tool_lifecycle() {
    let (sync_events, sync_messages) =
        common::run_and_collect(vec![user_message("hi")], context(), base_config(), script()).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let source = yielding_factory(script(), seen.clone());
    let (collector, events) = CollectorEmitter::new();
    let mut config = base_config();
    // Exercise restoring the moved incremental cache only AFTER open completes.
    config.convert_to_llm_incremental = Some(Arc::new(|messages| {
        messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User(user) => Some(rpi_ai::Message::User(user.clone())),
                AgentMessage::Assistant(a) => Some(rpi_ai::Message::Assistant(a.clone())),
                AgentMessage::ToolResult(t) => Some(rpi_ai::Message::ToolResult(t.clone())),
                _ => None,
            })
            .collect()
    }));
    let messages = rpi_agent::run_agent_loop_with_stream(
        vec![user_message("hi")],
        context(),
        config,
        Arc::new(collector),
        source.into(),
    )
    .await
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![1, 3]);
    assert_eq!(
        common::type_tags(&events.lock().unwrap()),
        common::type_tags(&sync_events)
    );
    assert_eq!(messages.len(), sync_messages.len());
    assert_eq!(messages.last(), sync_messages.last());
    let tool_results = |messages: &[AgentMessage]| {
        messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult(t) => {
                    Some((t.tool_call_id.clone(), common::tool_result_text(t)))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(tool_results(&messages), tool_results(&sync_messages));
}

#[tokio::test]
async fn async_continue_and_unanswered_assistant_run_tools_before_next_open() {
    for pending_assistant in [false, true] {
        let mut ctx = context();
        ctx.messages.push(user_message("hi"));
        if pending_assistant {
            ctx.messages
                .push(AgentMessage::Assistant(Box::new(assistant_tool_calls(
                    vec![("call", "echo", serde_json::json!({}))],
                    StopReason::ToolUse,
                ))));
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let factory = yielding_factory(
            mock_stream_fn(vec![assistant_text("done", StopReason::Stop)]),
            seen.clone(),
        );
        let (collector, events) = CollectorEmitter::new();
        let messages = if pending_assistant {
            rpi_agent::run_agent_loop_from_assistant_with_stream(
                ctx,
                base_config(),
                Arc::new(collector),
                factory.into(),
            )
            .await
            .unwrap()
        } else {
            rpi_agent::run_agent_loop_continue_with_stream(
                ctx,
                base_config(),
                Arc::new(collector),
                factory.into(),
            )
            .await
            .unwrap()
        };
        assert_eq!(
            *seen.lock().unwrap(),
            vec![if pending_assistant { 3 } else { 1 }]
        );
        assert_eq!(
            common::tool_execution_end_ids(&events.lock().unwrap()).len(),
            usize::from(pending_assistant)
        );
        assert_eq!(
            messages.last().unwrap().as_assistant().unwrap().stop_reason,
            StopReason::Stop
        );
    }
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct AfterResponse(Arc<AtomicUsize>);
impl rpi_ai::ProviderHooks for AfterResponse {
    fn after_response(&self, _: &rpi_ai::Model, message: &rpi_ai::AssistantMessage) {
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert!(message.timestamp > 0, "abort must carry an epoch timestamp");
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancellation_drops_pending_open_and_preserves_terminal_events() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let factory = async_stream_fn({
        let started = started.clone();
        let dropped = dropped.clone();
        move |_, _, _| {
            let started = started.clone();
            let dropped = dropped.clone();
            Box::pin(async move {
                let _guard = Dropped(dropped);
                started.notify_one();
                std::future::pending().await
            })
        }
    });
    let mut config = base_config();
    let token = config.signal.clone();
    let responses = Arc::new(AtomicUsize::new(0));
    config.provider_hooks = Some(Arc::new(AfterResponse(responses.clone())));
    let (collector, events) = CollectorEmitter::new();
    let run = tokio::spawn(rpi_agent::run_agent_loop_with_stream(
        vec![user_message("hi")],
        AgentContext::default(),
        config,
        Arc::new(collector),
        factory.into(),
    ));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    token.cancel();
    let messages = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(responses.load(Ordering::SeqCst), 1);
    assert_eq!(
        messages.last().unwrap().as_assistant().unwrap().stop_reason,
        StopReason::Aborted
    );
    let events = events.lock().unwrap();
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn pre_cancelled_async_open_does_not_call_factory_but_sync_stays_compatible() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = async_stream_fn({
        let calls = calls.clone();
        move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { panic!("cancelled async factory must not be polled") })
        }
    });
    let model = mock_model();
    let ctx = rpi_ai::Context::new(vec![]);
    let options = rpi_ai::SimpleStreamOptions::default();
    options.signal.cancel();
    let stream = StreamSource::Async(factory)
        .open(&model, &ctx, &options)
        .await;
    assert_eq!(
        stream.result().await.unwrap().stop_reason,
        StopReason::Aborted
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let sync = mock_stream_fn(vec![assistant_text("sync", StopReason::Stop)]);
    let stream = StreamSource::Sync(sync).open(&model, &ctx, &options).await;
    assert_eq!(stream.result().await.unwrap().stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn builder_last_factory_setter_wins_on_current_thread_runtime() {
    for use_async in [true, false] {
        let sync = mock_stream_fn(vec![assistant_text("sync", StopReason::Stop)]);
        let native = yielding_factory(
            mock_stream_fn(vec![assistant_text("async", StopReason::Stop)]),
            Arc::new(Mutex::new(Vec::new())),
        );
        let builder = AgentBuilder::new().model(mock_model());
        let builder = if use_async {
            builder.stream_fn(sync).async_stream_fn(native)
        } else {
            builder.async_stream_fn(native).stream_fn(sync)
        };
        let agent = builder.build().unwrap();
        agent.prompt("hi").await.unwrap();
        let state = agent.state();
        assert!(!state.is_streaming);
        let text = match &state
            .messages
            .last()
            .unwrap()
            .as_assistant()
            .unwrap()
            .content[0]
        {
            Content::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert_eq!(text, if use_async { "async" } else { "sync" });
    }
}

#[tokio::test]
async fn builder_abort_during_open_releases_run_and_allows_another_prompt() {
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let factory = async_stream_fn({
        let started = started.clone();
        let dropped = dropped.clone();
        let next = mock_stream_fn(vec![assistant_text("reused", StopReason::Stop)]);
        move |model, context, options| {
            let started = started.clone();
            let dropped = dropped.clone();
            let next = next.clone();
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 0 {
                    let _guard = Dropped(dropped);
                    started.notify_one();
                    std::future::pending().await
                } else {
                    next(model, context, options)
                }
            })
        }
    });
    let agent = AgentBuilder::new()
        .model(mock_model())
        .async_stream_fn(factory)
        .build()
        .unwrap();
    let runner = agent.clone();
    let task = tokio::spawn(async move { runner.prompt("first").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        started.notified().await;
        agent.abort();
        task.await.unwrap().unwrap();
        agent.wait_for_idle().await;
        assert!(dropped.load(Ordering::SeqCst));
        assert!(!agent.state().is_streaming);
        assert_eq!(
            agent
                .state()
                .messages
                .last()
                .unwrap()
                .as_assistant()
                .unwrap()
                .stop_reason,
            StopReason::Aborted
        );
        agent.prompt("second").await.unwrap();
        assert!(!agent.state().is_streaming);
        assert_eq!(
            agent
                .state()
                .messages
                .last()
                .unwrap()
                .as_assistant()
                .unwrap()
                .stop_reason,
            StopReason::Stop
        );
    })
    .await
    .expect("aborted async opening must release the active run");
}
