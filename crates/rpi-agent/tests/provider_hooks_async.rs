//! Awaited terminal hooks must not park a current-thread executor or publish
//! MessageEnd before their actual worker finishes. Both terminal paths matter.
#[path = "common/mod.rs"]
mod common;

use common::{assistant_text, base_config, mock_stream_fn, user_message};
use rpi_agent::{async_stream_fn, AgentContext, AgentEvent, AgentMessage, CollectorEmitter};
use rpi_ai::{AssistantMessage, Model, ProviderHooks, StopReason};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

struct AwaitedAfter {
    entered: Arc<tokio::sync::Notify>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    completed: Arc<AtomicBool>,
    expected_reason: StopReason,
}
impl ProviderHooks for AwaitedAfter {
    fn after_response(&self, _: &Model, _: &AssistantMessage) {
        panic!("the loop must use the awaited hook entry");
    }
    fn after_response_async<'a>(
        &'a self,
        model: &'a Model,
        message: &'a AssistantMessage,
    ) -> rpi_ai::provider::ProviderHookFuture<'a, ()> {
        let release = self.release.lock().unwrap().take().unwrap();
        let entered = self.entered.clone();
        let completed = self.completed.clone();
        let expected = self.expected_reason;
        Box::pin(async move {
            assert_eq!(message.stop_reason, expected);
            let model_id = &model.id;
            tokio::task::spawn_blocking(move || {
                entered.notify_one();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
                completed.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();
            assert!(!model_id.is_empty());
            assert_eq!(message.stop_reason, expected, "borrow survives worker wait");
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_fallback_and_cancel_hooks_await_workers_before_message_end() {
    for mode in ["terminal", "result-only", "cancel"] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let opening = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let (release, receiver) = mpsc::channel();
        let mut config = base_config();
        let signal = config.signal.clone();
        config.provider_hooks = Some(Arc::new(AwaitedAfter {
            entered: entered.clone(),
            release: Mutex::new(Some(receiver)),
            completed: completed.clone(),
            expected_reason: if mode == "cancel" {
                StopReason::Aborted
            } else {
                StopReason::Stop
            },
        }));
        let script = mock_stream_fn(vec![assistant_text("finished", StopReason::Stop)]);
        let factory = async_stream_fn({
            let opening = opening.clone();
            move |model, context, options| {
                let script = script.clone();
                let opening = opening.clone();
                Box::pin(async move {
                    opening.notify_one();
                    if mode == "cancel" {
                        std::future::pending::<()>().await;
                    }
                    let mut stream = script(model, context, options);
                    if mode == "result-only" {
                        // result() retains the terminal value after the stream
                        // events have already been consumed by an adapter.
                        while stream.next().await.is_some() {}
                    }
                    stream
                })
            }
        });
        let (collector, events) = CollectorEmitter::new();
        let run = tokio::spawn(rpi_agent::run_agent_loop_with_stream(
            vec![user_message("hi")],
            AgentContext::default(),
            config,
            Arc::new(collector),
            factory.into(),
        ));
        tokio::time::timeout(Duration::from_secs(2), opening.notified())
            .await
            .unwrap();
        if mode == "cancel" {
            signal.cancel();
        }
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(
            !run.is_finished(),
            "{mode}: terminal publication ran ahead of worker"
        );
        assert!(!completed.load(Ordering::SeqCst));
        assert!(!events.lock().unwrap().iter().any(|event| matches!(
            event,
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(_),
                ..
            }
        )));
        // Timer can run while the actual worker is blocked, with only one
        // async worker thread. No block_in_place/nested runtime is permitted.
        tokio::time::sleep(Duration::from_millis(2)).await;
        release.send(()).unwrap();
        let messages = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(
            messages.last().unwrap().as_assistant().unwrap().stop_reason,
            if mode == "cancel" {
                StopReason::Aborted
            } else {
                StopReason::Stop
            }
        );
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::Assistant(_),
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
    }
}
