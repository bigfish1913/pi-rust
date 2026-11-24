//! The live request awaits hooks while borrowing history; legacy patches still
//! use their existing sync entry via the default compatibility adapter.
use super::*;
use rpi_ai::{Context, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};
use std::sync::{mpsc, Mutex};

#[derive(Default)]
struct Seen {
    history: usize,
    header: Option<String>,
    calls: usize,
}
struct Probe(Arc<Mutex<Seen>>);
#[async_trait::async_trait]
impl AiProvider for Probe {
    fn id(&self) -> &str {
        "awaited-probe"
    }
    fn models(&self) -> &[Model] {
        &[]
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> rpi_ai::AssistantMessageEventStream {
        {
            let mut seen = self.0.lock().unwrap();
            seen.calls += 1;
            seen.history = context.messages.as_ptr() as usize;
            seen.header = options
                .headers
                .as_ref()
                .and_then(|headers| headers.get("x-hook").cloned());
        }
        let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
        producer
            .push(rpi_ai::AssistantMessageEvent::Done {
                reason: rpi_ai::DoneReason::Stop,
                message: AssistantMessage::empty(model.api.clone(), "awaited-probe", "m", 0),
            })
            .await;
        stream
    }
}
struct Hook {
    entered: Arc<tokio::sync::Notify>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}
impl ProviderHooks for Hook {
    fn before_request(
        &self,
        _: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        panic!("the Harness must use the awaited hook entry");
    }
    fn before_request_async<'a>(
        &'a self,
        model: &'a Model,
        context: &'a Context,
        options: &'a SimpleStreamOptions,
    ) -> rpi_ai::provider::ProviderHookFuture<'a, Option<SimpleStreamOptionsPatch>> {
        let release = self.release.lock().unwrap().take().unwrap();
        let entered = self.entered.clone();
        Box::pin(async move {
            let pointer = context.messages.as_ptr() as usize;
            assert_eq!(options.headers.as_ref().unwrap()["x-hook"], "original");
            tokio::task::spawn_blocking(move || {
                entered.notify_one();
                release
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            })
            .await
            .unwrap();
            assert_eq!(pointer, context.messages.as_ptr() as usize);
            assert_eq!(model.id, "m");
            Some(SimpleStreamOptionsPatch {
                headers: Some([("x-hook".into(), Some("awaited".into()))].into()),
                ..Default::default()
            })
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn request_waits_for_real_worker_then_applies_patch_without_copying_history() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, receiver) = mpsc::channel();
    let stream_fn = AgentHarness::build_stream_fn(
        vec![Arc::new(Probe(seen.clone()))],
        Some(Arc::new(Hook {
            entered: entered.clone(),
            release: Mutex::new(Some(receiver)),
        })),
        AgentHarnessStreamOptions {
            headers: Some([("x-hook".into(), "original".into())].into()),
            ..Default::default()
        },
    )
    .unwrap();
    let model = Model::new(
        "m",
        "M",
        rpi_ai::Api::Faux,
        "awaited-probe",
        "http://localhost",
    );
    let context = Context::new(vec![rpi_ai::UserMessage::new(
        "large history".repeat(4096),
        0,
    )
    .into()]);
    let options = Default::default();
    let mut opening = stream_fn(&model, &context, &options);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            _ = entered.notified() => {},
            _ = &mut opening => panic!("provider opened before worker release"),
        }
    })
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap().calls, 0);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    release.send(()).unwrap();
    opening.await.result().await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.calls, 1);
    assert_eq!(seen.history, context.messages.as_ptr() as usize);
    assert_eq!(seen.header.as_deref(), Some("awaited"));
}
