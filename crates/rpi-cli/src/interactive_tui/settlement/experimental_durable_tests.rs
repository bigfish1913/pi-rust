//! #52: experimental v2 TUI settlement/collector cross-checks.
//! Synthetic Memory and TempDir JSONL only. These are component tests for the
//! existing TUI task owner; they do not certify PTY/full-TUI shutdown, provider
//! execution, runtime kill, cross-process fencing, or terminal delivery.
use super::*;
use rpi_harness::{
    durable::{
        experimental_ledger, DurableActor, DurableMutation, DurableService,
        ExperimentalDurablePolicy,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
        types::{Entry, EntryQuery, SessionMetadata},
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::json;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

const EPOCH: &str = "tui-settlement-epoch_52.test";
const OWNER: &str = "native-cli";

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("experimental TUI settlement boundary timed out")
}

fn memory_session(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )),
        None,
    )
}

async fn experimental(session: Session) -> AgentHarness {
    AgentHarness::create_experimental(
        AgentHarnessOptions {
            session,
            allow_existing_session: true,
            require_native_durable: true,
            ..Default::default()
        },
        ExperimentalDurablePolicy::new(EPOCH, vec![OWNER.into()]).unwrap(),
    )
    .await
    .unwrap()
}

fn service(harness: &AgentHarness) -> DurableService {
    DurableService::bind(harness, DurableActor::owner(OWNER).unwrap())
}

async fn mutation(harness: &AgentHarness, request_id: &str) -> DurableMutation {
    let service = service(harness);
    DurableMutation {
        request_id: request_id.into(),
        expected: service.snapshot().await.unwrap().scope,
        action: serde_json::from_value(json!({
            "action": "enqueue",
            "text": "synthetic TUI v2 settlement"
        }))
        .unwrap(),
        run_id: None,
    }
}

struct Fixture {
    harness: AgentHarness,
    probe: Arc<super::probe::Probe>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    done: Arc<Notify>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.release.notify_one();
        self.probe.release.notify_one();
        self.probe.release_drain.notify_one();
    }
}

impl Fixture {
    async fn memory(id: &str) -> Self {
        let probe = super::probe::Probe::new(memory_session(id).storage());
        let harness = experimental(Session::new(probe.clone(), None)).await;
        Self {
            harness,
            probe,
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            done: Arc::new(Notify::new()),
        }
    }

    fn producer(
        &self,
        mutation: DurableMutation,
        wait_before_mutate: bool,
    ) -> impl Future<Output = ()> + Send + 'static {
        let harness = self.harness.clone();
        let entered = self.entered.clone();
        let release = self.release.clone();
        let done = self.done.clone();
        async move {
            entered.notify_one();
            if wait_before_mutate {
                release.notified().await;
            }
            service(&harness).mutate(mutation).await.unwrap();
            done.notify_one();
        }
    }
}

async fn assert_exact_v2_receipt(harness: &AgentHarness, mutation: &DurableMutation) {
    let receipt = service(harness)
        .request_outcome(&mutation.request_id)
        .await
        .unwrap()
        .expect("settled v2 producer must leave its exact receipt");
    assert_eq!(receipt.request_id, mutation.request_id);
    assert_eq!(receipt.actor, OWNER);
    assert_eq!(receipt.expected, mutation.expected);
    assert_eq!(receipt.action, mutation.action);
    assert_eq!(receipt.run_id, mutation.run_id);
    let entries = harness
        .session()
        .storage()
        .find_entries(&EntryQuery {
            entry_type: Some("custom"),
            custom_type: Some(experimental_ledger::CUSTOM_TYPE.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let Entry::Custom(entry) = &entries[0] else {
        panic!("v2 settlement must persist a custom ledger entry")
    };
    let value = entry.data.as_ref().unwrap();
    assert_eq!(value["epoch"]["epochId"], EPOCH);
    assert_eq!(value["receipt"]["requestId"], mutation.request_id);
    assert_eq!(value["receipt"], serde_json::to_value(&receipt).unwrap());
    assert!(harness
        .session()
        .storage()
        .find_records(&Default::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn v2_registered_tui_producer_joins_before_lane_and_core_drain() {
    bounded(async {
        let fixture = Fixture::memory("tui52-order").await;
        let mutation = mutation(&fixture.harness, "v2-order").await;
        fixture.probe.pause_drains.store(true, Ordering::SeqCst);
        let tasks = SessionWriteTasks::new(true);
        assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
        fixture.entered.notified().await;
        let batch = tasks.seal();
        let mut settle = Box::pin(settle_native(&fixture.harness, batch));
        assert!(futures::poll!(settle.as_mut()).is_pending());
        assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
        fixture.release.notify_one();
        tokio::select! {
            _ = &mut settle => panic!("v2 settlement bypassed the paused backend"),
            _ = fixture.probe.drain_entered.notified() => {}
        }
        assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 1);
        assert!(futures::poll!(settle.as_mut()).is_pending());
        fixture.probe.release_drain.notify_one();
        assert!(settle.await);
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
        assert_exact_v2_receipt(&fixture.harness, &mutation).await;
        assert_eq!(
            fixture.harness.experimental_durable_epoch().as_deref(),
            Some(EPOCH)
        );
    })
    .await;
}

#[tokio::test]
async fn v2_seal_rejects_late_producer_without_extra_receipt_or_poll() {
    bounded(async {
        let fixture = Fixture::memory("tui52-seal").await;
        let mutation = mutation(&fixture.harness, "v2-seal").await;
        let tasks = SessionWriteTasks::new(true);
        let clone = tasks.clone();
        assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
        fixture.entered.notified().await;
        let batch = clone.seal();
        let late_polls = Arc::new(AtomicUsize::new(0));
        let late_polls_body = late_polls.clone();
        assert!(!tasks.spawn(async move {
            late_polls_body.fetch_add(1, Ordering::SeqCst);
        }));
        fixture.release.notify_one();
        assert!(settle_native(&fixture.harness, batch).await);
        tokio::task::yield_now().await;
        assert_eq!(late_polls.load(Ordering::SeqCst), 0);
        assert_exact_v2_receipt(&fixture.harness, &mutation).await;
    })
    .await;
}

#[tokio::test]
async fn v2_join_failure_still_drains_and_keeps_successful_receipt() {
    bounded(async {
        let fixture = Fixture::memory("tui52-failure").await;
        let mutation = mutation(&fixture.harness, "v2-after-panic").await;
        let tasks = SessionWriteTasks::new(true);
        assert!(tasks.spawn(async { panic!("synthetic TUI v2 producer failure") }));
        assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
        fixture.entered.notified().await;
        fixture.probe.pause_drains.store(true, Ordering::SeqCst);
        let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
        assert!(futures::poll!(settle.as_mut()).is_pending());
        fixture.release.notify_one();
        tokio::select! {
            _ = &mut settle => panic!("join failure skipped v2 cleanup"),
            _ = fixture.probe.drain_entered.notified() => {}
        }
        fixture.probe.release_drain.notify_one();
        assert!(!settle.await);
        assert_exact_v2_receipt(&fixture.harness, &mutation).await;
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
    })
    .await;
}

#[tokio::test]
async fn dropping_v2_settlement_does_not_claim_drain_or_lose_owned_write() {
    bounded(async {
        let fixture = Fixture::memory("tui52-drop").await;
        let mutation = mutation(&fixture.harness, "v2-drop").await;
        let tasks = SessionWriteTasks::new(true);
        assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
        fixture.entered.notified().await;
        let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
        assert!(futures::poll!(settle.as_mut()).is_pending());
        drop(settle);
        fixture.release.notify_one();
        fixture.done.notified().await;
        assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
        assert_exact_v2_receipt(&fixture.harness, &mutation).await;
    })
    .await;
}

#[tokio::test]
async fn v2_jsonl_settlement_reopens_exact_receipt_and_unchanged_bytes() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "tui52.jsonl",
            JsonlV4Header::new("tui52-jsonl".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let harness = experimental(Session::new(Arc::new(storage), None)).await;
        let m = mutation(&harness, "v2-jsonl").await;
        let tasks = SessionWriteTasks::new(true);
        assert!(tasks.spawn({
            let h = harness.clone();
            let m = m.clone();
            async move {
                service(&h).mutate(m).await.unwrap();
            }
        }));
        assert!(settle_native(&harness, tasks.seal()).await);
        assert_exact_v2_receipt(&harness, &m).await;
        let bytes = std::fs::read(dir.path().join("tui52.jsonl")).unwrap();
        harness.close().await.unwrap();
        // The harness close flag does not retire the shared storage binding;
        // drop the owner before a new process/binding acquires this journal's
        // writer lease.
        drop(harness);
        let loaded = JsonlSessionStorage::load(
            fs,
            "tui52.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = experimental(Session::new(Arc::new(loaded), None)).await;
        assert_eq!(
            service(&reopened)
                .request_outcome("v2-jsonl")
                .await
                .unwrap()
                .unwrap()
                .request_id,
            m.request_id
        );
        assert_eq!(
            std::fs::read(dir.path().join("tui52.jsonl")).unwrap(),
            bytes
        );
        assert_exact_v2_receipt(&reopened, &m).await;
        reopened.close().await.unwrap();
    })
    .await;
}
