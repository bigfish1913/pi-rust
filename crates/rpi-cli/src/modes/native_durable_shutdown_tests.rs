//! Normal dispatcher shutdown fences accepted producers, then backend drain.
//! Root-future cancellation is deliberately a separate, weaker contract.
use super::*;
#[path = "../../../rpi-harness/tests/support/durable_control_probe.rs"]
mod probe;
use probe::Probe;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::{
    durable::{DurableActor, DurableService},
    session::Session,
    types::AgentHarnessOptions,
};
use serde_json::{json, Value};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncRead, AsyncWriteExt, BufReader, DuplexStream, ReadBuf},
    sync::Notify,
};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("shutdown boundary timed out")
}

#[derive(Clone, Default)]
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    changed: Arc<Notify>,
    fail: Arc<AtomicBool>,
    failed: Arc<Notify>,
}
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            self.failed.notify_one();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test pipe closed",
            ));
        }
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        self.changed.notify_one();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Capture {
    fn frames(&self) -> Vec<Value> {
        self.bytes
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect()
    }
    async fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            // notify_one retains a permit if the writer races the inspection.
            let changed = self.changed.notified();
            if let Some(frame) = self.frames().into_iter().find(&predicate) {
                return frame;
            }
            changed.await;
        }
    }
}

// Observing EOF proves the dispatcher actually reached its shutdown join, not
// merely that a newly spawned root has not been scheduled yet.
struct Input {
    inner: BufReader<DuplexStream>,
    eof: Arc<Notify>,
}
impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl AsyncBufRead for Input {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_fill_buf(cx);
        if matches!(&result, Poll::Ready(Ok(bytes)) if bytes.is_empty()) {
            this.eof.notify_one();
        }
        result
    }
    fn consume(self: Pin<&mut Self>, amount: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amount);
    }
}
struct Root(tokio::task::JoinHandle<i32>);
impl Drop for Root {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Fixture {
    harness: AgentHarness,
    probe: Arc<Probe>,
    provider: Arc<FauxProvider>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.probe.release.notify_one();
        self.probe.release_drain.notify_one();
    }
}
impl Fixture {
    async fn new() -> Self {
        let probe = Probe::new(AgentHarnessOptions::default().session.storage());
        let provider = FauxProvider::new(FauxScript::new());
        let harness = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(probe.clone(), None),
            allow_existing_session: true,
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
        Self {
            harness,
            probe,
            provider,
        }
    }
    fn service(&self) -> DurableService {
        DurableService::bind(&self.harness, DurableActor::owner("native-cli").unwrap())
    }
    fn spawn(&self, enabled: bool) -> (DuplexStream, Capture, Root, Arc<Notify>) {
        let (client, server) = tokio::io::duplex(8192);
        let capture = Capture::default();
        let output = capture.clone();
        let eof = Arc::new(Notify::new());
        let input = Input {
            inner: BufReader::new(server),
            eof: eof.clone(),
        };
        let harness = self.harness.clone();
        let root = Root(tokio::spawn(async move {
            rpc_with_native_durable_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                enabled,
                input,
                output,
            )
            .await
        }));
        (client, capture, root, eof)
    }
    async fn request(&self) -> Value {
        json!({"type":"durable_mutate", "id":"mutation", "version":1,
            "requestId":"shutdown-start", "expected":self.service().snapshot().await.unwrap().scope,
            "action":{"action":"start","name":"shutdown","steps":[{"id":"s","title":"Persist"}]}})
    }
    async fn idle(&self) {
        assert_eq!(self.provider.state().call_count.load(Ordering::SeqCst), 0);
        assert!(self
            .harness
            .session()
            .storage()
            .find_records(&RecordQuery {
                record_type: Some("operation_started"),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert!(self
            .harness
            .session()
            .storage()
            .find_entries(&EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
    }
    async fn exact_receipt(&self, request: &Value) -> Value {
        let receipt = self
            .service()
            .request_outcome("shutdown-start")
            .await
            .unwrap()
            .unwrap();
        let receipt = serde_json::to_value(receipt).unwrap();
        assert_eq!(receipt["actor"], "native-cli");
        assert_eq!(receipt["requestId"], "shutdown-start");
        assert_eq!(receipt["expected"], request["expected"]);
        assert_eq!(receipt["action"], request["action"]);
        assert_eq!(receipt["controlRevision"], 1);
        assert_eq!(receipt["changed"], true);
        assert_eq!(
            self.harness
                .session()
                .storage()
                .find_entries(&EntryQuery {
                    entry_type: Some("custom"),
                    custom_type: Some("rpi.durable.control.v1".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1
        );
        let replay = crate::native_durable::rpc(&self.harness, request, true)
            .await
            .unwrap();
        assert_eq!(replay["outcome"]["replayed"], true);
        assert_eq!(replay["outcome"]["receipt"], receipt);
        assert_eq!(self.probe.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(self.probe.commits.load(Ordering::SeqCst), 1);
        receipt
    }
}
async fn send(input: &mut DuplexStream, frame: Value) {
    input
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
}
async fn boundary(probe: &Probe, mode: usize) {
    if mode == 3 {
        probe.entered.notified().await;
    } else {
        probe.committed.notified().await;
    }
    assert_eq!(probe.commits.load(Ordering::SeqCst), usize::from(mode == 4));
}
fn assert_flushed(capture: &Capture, receipt: &Value, stop: bool) {
    let frames = capture.frames();
    assert!(frames.iter().all(|v| !matches!(
        v["type"].as_str(),
        Some("agent_start" | "tool_execution_start")
    )));
    let response = frames
        .iter()
        .find(|v| v["type"] == "response" && v["id"] == "mutation")
        .unwrap();
    assert_eq!(response["success"], true);
    assert_eq!(&response["data"]["outcome"]["receipt"], receipt);
    assert_eq!(
        frames
            .iter()
            .filter(|v| v["type"] == "session_entry_committed"
                && v["customType"] == "rpi.durable.control.v1"
                && v["entryId"] == receipt["entryId"])
            .count(),
        1
    );
    if stop {
        let ack = frames
            .iter()
            .find(|v| v["type"] == "response" && v["id"] == "stop")
            .unwrap();
        assert_eq!(ack["success"], true);
    }
}

async fn normal_shutdown(stop: bool) {
    for mode in [3, 4] {
        let fixture = Fixture::new().await;
        let request = fixture.request().await;
        fixture.probe.mode.store(mode, Ordering::SeqCst);
        fixture.probe.pause_drains.store(true, Ordering::SeqCst);
        let (mut input, capture, mut root, eof) = fixture.spawn(true);
        send(&mut input, request.clone()).await;
        boundary(&fixture.probe, mode).await;
        if stop {
            send(&mut input, json!({"type":"stop","id":"stop"})).await;
            // Native stop first enters cooperative reconciliation, which may
            // itself wait for this admitted mutation's message gate. Do not
            // require its ACK before releasing the real append/ACK boundary
            // below.
        } else {
            input.shutdown().await.unwrap();
            eof.notified().await;
        }
        assert!(futures::poll!(&mut root.0).is_pending());
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
        fixture.probe.release.notify_one();
        fixture.probe.drain_entered.notified().await;
        assert!(futures::poll!(&mut root.0).is_pending());
        // The writer is independent: don't impose an order on commit delivery.
        let response = capture
            .until(|v| v["type"] == "response" && v["id"] == "mutation")
            .await;
        assert_eq!(response["success"], true);
        fixture.probe.release_drain.notify_one();
        assert_eq!((&mut root.0).await.unwrap(), 0);
        let receipt = fixture.exact_receipt(&request).await;
        assert_flushed(&capture, &receipt, stop);
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
        fixture.idle().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_stop_reconciles_an_active_prompt_before_shutdown_drain() {
    bounded(async {
        let fixture = Fixture::new().await;
        let (mut input, capture, mut root, _) = fixture.spawn(true);
        capture.until(|v| v["type"] == "ready").await;
        send(
            &mut input,
            json!({"type":"prompt","id":"prompt","message":"test"}),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.provider.state().call_count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        send(&mut input, json!({"type":"stop","id":"stop"})).await;
        assert_eq!((&mut root.0).await.unwrap(), 0);

        let settled = capture
            .frames()
            .into_iter()
            .find(|value| value["type"] == "agent_settled")
            .expect("native stop must publish a run settlement");
        let run_id = settled["runId"].as_str().expect("settled run id");
        assert!(fixture.harness.get_result(run_id).await.unwrap().is_some());
        assert!(!fixture
            .harness
            .session()
            .storage()
            .find_records(&RecordQuery {
                record_type: Some("operation_finished"),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            capture.frames().iter().find(|v| v["id"] == "stop").unwrap()["success"],
            true
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_eof_joins_mutation_then_drains_and_flushes() {
    bounded(normal_shutdown(false)).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_stop_ack_does_not_skip_mutation_drain_or_commit_flush() {
    bounded(normal_shutdown(true)).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_pipe_still_joins_admitted_mutation_and_drains_without_fake_delivery() {
    bounded(async {
        for mode in [3, 4] {
            let fixture = Fixture::new().await;
            let request = fixture.request().await;
            let (mut input, capture, mut root, _) = fixture.spawn(true);
            capture.until(|v| v["type"] == "ready").await;
            fixture.probe.mode.store(mode, Ordering::SeqCst);
            fixture.probe.pause_drains.store(true, Ordering::SeqCst);
            send(&mut input, request.clone()).await;
            boundary(&fixture.probe, mode).await;
            capture.fail.store(true, Ordering::SeqCst);
            send(&mut input, json!({"type":"ping","id":"broken-ping"})).await;
            capture.failed.notified().await;
            assert!(futures::poll!(&mut root.0).is_pending());
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
            fixture.probe.release.notify_one();
            fixture.probe.drain_entered.notified().await;
            assert!(futures::poll!(&mut root.0).is_pending());
            fixture.probe.release_drain.notify_one();
            assert_eq!((&mut root.0).await.unwrap(), 1);
            fixture.exact_receipt(&request).await;
            assert!(!capture
                .frames()
                .iter()
                .any(|v| v["type"] == "response"
                    && (v["id"] == "mutation" || v["id"] == "broken-ping")));
            fixture.idle().await;
        }
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_shutdown_never_drains_and_native_cleanup_does_not_decode_bad_journal() {
    bounded(async {
        for (enabled, stop) in [(false, false), (false, true), (true, false)] {
            let fixture = Fixture::new().await;
            if enabled {
                fixture
                    .harness
                    .session()
                    .append_custom_entry("rpi.durable.control.v1", Some(json!("malformed")))
                    .await
                    .unwrap();
            }
            let before = serde_json::to_value(
                fixture
                    .probe
                    .inner
                    .find_entries(&EntryQuery::default())
                    .await
                    .unwrap(),
            )
            .unwrap();
            let revision = fixture.probe.inner.history_revision().await.unwrap();
            fixture.probe.pause_drains.store(true, Ordering::SeqCst);
            let (mut input, capture, mut root, _) = fixture.spawn(enabled);
            // Startup SessionValues lookup is a real journal read. Inject only
            // after ready so this test reaches cleanup rather than setup error.
            capture.until(|v| v["type"] == "ready").await;
            fixture.probe.fail_reads.store(true, Ordering::SeqCst);
            fixture.probe.fail_revision.store(true, Ordering::SeqCst);
            if stop {
                send(&mut input, json!({"type":"stop","id":"stop"})).await;
            } else {
                input.shutdown().await.unwrap();
            }
            if enabled {
                fixture.probe.drain_entered.notified().await;
                assert!(futures::poll!(&mut root.0).is_pending());
                fixture.probe.release_drain.notify_one();
            }
            assert_eq!((&mut root.0).await.unwrap(), 0);
            assert_eq!(
                fixture.probe.drain_calls.load(Ordering::SeqCst),
                usize::from(enabled)
            );
            assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
            assert_eq!(
                fixture.probe.inner.history_revision().await.unwrap(),
                revision
            );
            assert_eq!(
                serde_json::to_value(
                    fixture
                        .probe
                        .inner
                        .find_entries(&EntryQuery::default())
                        .await
                        .unwrap()
                )
                .unwrap(),
                before
            );
            assert!(!capture
                .frames()
                .iter()
                .any(|v| v["type"] == "response" && v["success"] == false));
            fixture.idle().await;
        }
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_abort_only_certifies_retained_cleanup_and_core_owned_completion() {
    bounded(async {
        for mode in [3, 4] {
            let fixture = Fixture::new().await;
            let request = fixture.request().await;
            fixture.probe.mode.store(mode, Ordering::SeqCst);
            let (mut input, _capture, mut root, _) = fixture.spawn(true);
            send(&mut input, request.clone()).await;
            boundary(&fixture.probe, mode).await;
            root.0.abort();
            assert!((&mut root.0).await.unwrap_err().is_cancelled());
            fixture.probe.release.notify_one();
            // Core service observation queues behind admitted work; never infer
            // receipt identity from a different actor or an invented response.
            fixture.exact_receipt(&request).await;
            fixture.idle().await;
            // Excluded: root abort is NOT normal join/flush certification and
            // makes no assertion that the new backend drain was ever invoked.
        }
    })
    .await;
}
