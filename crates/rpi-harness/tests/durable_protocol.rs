//! Public protocol foundation contracts; synthetic Memory/TempDir JSONL only.
//! No CLI dependency, provider, extension, adapter process or alternate writer.
#[path = "durable_protocol/pages.rs"]
mod pages;
#[path = "support/durable_control_probe.rs"]
mod probe;
use probe::Probe;
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{
        protocol::{self, Failure},
        DurableActor, DurableMutation, DurableService,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::{EntryQuery, SessionStorage},
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::{
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("synthetic operation timed out")
}
async fn harness(store: Arc<dyn SessionStorage>) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session: Session::new(store, None),
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn memory() -> AgentHarness {
    harness(AgentHarnessOptions::default().session.storage()).await
}
fn owner(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner("adapter-owner").unwrap())
}
fn start() -> Value {
    json!({"action":"start", "name":"shared-wire", "steps":[{"id":"s","title":"Verify"}]})
}
async fn call(service: &DurableService, request: Value) -> Result<Value, Failure> {
    // Exercise the safe public raw entry, not ordinary Value deserialization.
    let prepared = protocol::parse_rpc(&serde_json::to_string(&request).unwrap(), true)?;
    bounded(protocol::execute_rpc(service, prepared)).await
}
async fn mutation(service: &DurableService, id: &str, action: Value) -> Value {
    json!({"type":"durable_mutate", "version":1, "requestId":id,
        "expected":service.snapshot().await.unwrap().scope, "action":action})
}
async fn lookup(service: &DurableService, id: &str) -> Value {
    call(
        service,
        json!({"type":"get_durable_request","version":1,"requestId":id}),
    )
    .await
    .unwrap()["receipt"]
        .clone()
}
async fn control_count(h: &AgentHarness) -> usize {
    h.session()
        .storage()
        .find_entries(&EntryQuery {
            custom_type: Some("rpi.durable.control.v1".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .len()
}
async fn probed() -> (AgentHarness, Arc<Probe>) {
    let probe = Probe::new(AgentHarnessOptions::default().session.storage());
    (harness(probe.clone()).await, probe)
}

#[test]
fn raw_preparation_is_strict_bounded_and_needs_no_service() {
    let state = r#"{"type":"get_durable_state","version":1}"#;
    assert!(protocol::parse_rpc(state, true).is_ok());
    assert_eq!(
        protocol::parse_rpc(state, false).unwrap_err().code,
        "disabled"
    );
    for raw in [
        r#"{"type":"get_durable_state","version":1,"version":1}"#,
        r#"{"type":"durable_mutate","version":1,"action":{"action":"reset","\u0061ction":"reset"}}"#,
        r#"{"type":"durable_mutate","version":1,"action":{"steps":[{"id":"a","id":"b"}]}}"#,
        r#"{"type":"get_durable_state","version":1,"__rpi":{"actor":"a","actor":"b"}}"#,
        r#"{"type":"get_durable_state","version":1,"actor":"owner"}"#,
        r#"{"type":"get_durable_state","version":1,"lane":"main"}"#,
        r#"{"type":"get_durable_state","version":1,"enabled":true}"#,
        r#"{"type":"get_durable_state","version":1.0}"#,
        r#"{"type":"get_durable_state","version":1}{}"#,
    ] {
        assert_eq!(
            protocol::parse_rpc(raw, true).unwrap_err().code,
            "invalid_request",
            "{raw}"
        );
    }
    let exact = format!(
        "{}{}",
        " ".repeat(protocol::MAX_REQUEST_BYTES - state.len()),
        state
    );
    assert!(protocol::parse_rpc(&exact, true).is_ok());
    assert_eq!(
        protocol::parse_rpc(&(exact + " "), true).unwrap_err().code,
        "limit_exceeded"
    );
    // 682 six-byte escapes plus quotes fit (4094); one more must fail.
    assert!(protocol::decode_rpc(
        &json!({"type":"get_durable_state","version":1,"id":"\u{1}".repeat(protocol::MAX_ID_BYTES / 6)}),
        true
    ).is_ok());
    for id in [
        json!("x".repeat(protocol::MAX_ID_BYTES)),
        json!("\u{1}".repeat(protocol::MAX_ID_BYTES / 6 + 1)),
    ] {
        assert_eq!(
            protocol::decode_rpc(
                &json!({"type":"get_durable_state","version":1,"id":id}),
                true
            )
            .unwrap_err()
            .code,
            "limit_exceeded"
        );
    }
    assert_eq!(protocol::decode_rpc(&json!({"type":"get_durable_state","version":1,"id":"x".repeat(protocol::MAX_REQUEST_BYTES)}), true).unwrap_err().code, "limit_exceeded");
    let body = json!({"body":"x".repeat(protocol::MAX_RESPONSE_BYTES)});
    assert_eq!(
        protocol::response_budget(body.clone(), None)
            .unwrap_err()
            .code,
        "limit_exceeded"
    );
    let late = protocol::response_budget(body, Some("committed-request")).unwrap_err();
    assert_eq!(late.code, "commit_uncertain");
    assert_eq!(late.request_id.as_deref(), Some("committed-request"));
}

async fn roundtrip(h: &AgentHarness) -> Value {
    let service = owner(h);
    let mut first = mutation(&service, "start", start()).await;
    first["id"] = json!("transport-a");
    let result = call(&service, first.clone()).await.unwrap();
    assert_eq!(result["outcome"]["receipt"]["actor"], "adapter-owner");
    assert_eq!(
        service
            .snapshot()
            .await
            .unwrap()
            .state
            .workflow
            .unwrap()
            .name,
        "shared-wire"
    );
    first["id"] = json!("transport-b");
    assert_eq!(
        call(&service, first.clone()).await.unwrap()["outcome"]["replayed"],
        true
    );
    first["action"]["name"] = json!("different payload");
    assert_eq!(call(&service, first).await.unwrap_err().code, "conflict");
    let queued = call(
        &service,
        mutation(
            &service,
            "enqueue",
            json!({"action":"enqueue","text":"manual"}),
        )
        .await,
    )
    .await
    .unwrap();
    let item = queued["outcome"]["snapshot"]["state"]["queue"][0]["id"].clone();
    call(
        &service,
        mutation(&service, "claim", json!({"action":"claim","item":item})).await,
    )
    .await
    .unwrap();
    // Native service and wire calls act on one receipt/journal, not copies.
    let done = service
        .mutate(DurableMutation {
            request_id: "finish".into(),
            expected: service.snapshot().await.unwrap().scope,
            action: serde_json::from_value(
                json!({"action":"finish","item":item,"status":"completed"}),
            )
            .unwrap(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        lookup(&service, "finish").await,
        serde_json::to_value(&done.receipt).unwrap()
    );
    assert_eq!(control_count(h).await, 4);
    serde_json::to_value(done.snapshot.state).unwrap()
}
#[tokio::test]
async fn direct_memory_and_jsonl_use_one_journal_and_survive_reopen() {
    let h = memory().await;
    roundtrip(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs.clone(),
        "wire.jsonl",
        JsonlV4Header::new(
            "synthetic-protocol-session".into(),
            1,
            "/synthetic".into(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = harness(Arc::new(store)).await;
    let state = roundtrip(&h).await;
    let scope = owner(&h).snapshot().await.unwrap().scope;
    h.drain_session().await;
    drop(h);
    let before = std::fs::read(dir.path().join("wire.jsonl")).unwrap();
    let store = JsonlSessionStorage::load(
        fs,
        "wire.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = harness(Arc::new(store)).await;
    let service = owner(&h);
    let observed = call(&service, json!({"type":"get_durable_state","version":1}))
        .await
        .unwrap();
    assert_eq!(observed["state"]["state"], state);
    assert_eq!(lookup(&service, "finish").await["requestId"], "finish");
    assert_ne!(
        observed["state"]["scope"]["bindingToken"],
        scope.binding_token
    );
    assert_eq!(
        std::fs::read(dir.path().join("wire.jsonl")).unwrap(),
        before
    );
}

#[tokio::test]
async fn invalid_payloads_and_foreign_guards_never_append() {
    let (h, probe) = probed().await;
    let service = owner(&h);
    let baseline = service.snapshot().await.unwrap();
    let good = mutation(&service, "candidate", start()).await;
    let foreign = memory().await;
    let foreign_scope = owner(&foreign).snapshot().await.unwrap().scope;
    for (request, code) in [
        (
            json!({"type":"durable_mutate","version":1,"action":start()}),
            "invalid_request",
        ),
        (
            json!({"type":"durable_mutate","version":1,"requestId":"missing","expected":{},"action":start()}),
            "invalid_request",
        ),
        (
            {
                let mut v = good.clone();
                v["action"]["actor"] = json!("forged");
                v
            },
            "invalid_request",
        ),
        (
            {
                let mut v = good.clone();
                v["expected"]["extra"] = json!(true);
                v
            },
            "invalid_request",
        ),
        (
            {
                let mut v = good.clone();
                v["expected"] = serde_json::to_value(foreign_scope).unwrap();
                v
            },
            "conflict",
        ),
        (
            {
                let mut v = good.clone();
                v["expected"]["historyGeneration"] = json!(baseline.scope.history_generation + 1);
                v
            },
            "conflict",
        ),
        (
            {
                let mut v = good.clone();
                v["runId"] = json!("unexpected-run");
                v
            },
            "invalid_request",
        ),
    ] {
        assert_eq!(call(&service, request).await.unwrap_err().code, code);
        assert_eq!(service.snapshot().await.unwrap(), baseline);
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn trusted_binding_controls_actor_lane_and_weak_lifetime() {
    let h = memory().await;
    h.session()
        .storage()
        .create_lane("side", None)
        .await
        .unwrap();
    let main = owner(&h);
    let side = DurableService::bind_lane(&h, DurableActor::owner("side-owner").unwrap(), "side");
    call(&side, mutation(&side, "side-start", start()).await)
        .await
        .unwrap();
    assert!(main.snapshot().await.unwrap().state.workflow.is_none());
    assert_eq!(side.snapshot().await.unwrap().scope.lane, "side");
    let queued = protocol::flat_request(&main, json!({"action":"enqueue","requestId":"enqueue","text":"claim me","__rpi":{"actor":"forged"}}), None).await.unwrap();
    assert_eq!(queued["outcome"]["receipt"]["actor"], "adapter-owner");
    let item = queued["outcome"]["snapshot"]["state"]["queue"][0]["id"].clone();
    let child = DurableService::bind(&h, DurableActor::child("adapter-child").unwrap());
    assert_eq!(
        call(
            &child,
            mutation(&child, "child-reset", json!({"action":"reset"})).await
        )
        .await
        .unwrap_err()
        .code,
        "permission_denied"
    );
    call(
        &child,
        mutation(&child, "child-claim", json!({"action":"claim","item":item})).await,
    )
    .await
    .unwrap();
    assert_eq!(
        lookup(&child, "child-claim").await["actor"],
        "adapter-child"
    );
    assert!(lookup(&main, "child-claim").await.is_null());
    assert!(lookup(&child, "enqueue").await.is_null());
    let other = DurableService::bind(&h, DurableActor::child("other-child").unwrap());
    assert_eq!(
        call(
            &other,
            mutation(
                &other,
                "bad-finish",
                json!({"action":"finish","item":item,"status":"completed"})
            )
            .await
        )
        .await
        .unwrap_err()
        .code,
        "permission_denied"
    );
    let prepared =
        protocol::parse_rpc(r#"{"type":"get_durable_state","version":1}"#, true).unwrap();
    h.close().await.unwrap();
    assert_eq!(
        protocol::execute_rpc(&main, prepared.clone())
            .await
            .unwrap_err()
            .code,
        "closed"
    );
    drop(h);
    assert_eq!(
        protocol::execute_rpc(&side, prepared)
            .await
            .unwrap_err()
            .code,
        "closed"
    );
}

#[tokio::test]
async fn uncertain_wire_results_reconcile_only_identical_guarded_requests() {
    for mode in [1, 2] {
        let (h, probe) = probed().await;
        let service = owner(&h);
        let request = mutation(&service, "uncertain", start()).await;
        probe.mode.store(mode, Ordering::SeqCst);
        let error = call(&service, request.clone()).await.unwrap_err();
        assert_eq!(error.code, "commit_uncertain");
        assert_eq!(error.request_id.as_deref(), Some("uncertain"));
        assert!(!error.message.contains("ACK"));
        assert_eq!(lookup(&service, "uncertain").await.is_null(), mode == 1);
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
        let replayed = call(&service, request).await.unwrap();
        assert_eq!(replayed["outcome"]["replayed"], mode == 2);
        assert_eq!(
            probe.attempts.load(Ordering::SeqCst),
            if mode == 1 { 2 } else { 1 }
        );
        assert_eq!(probe.commits.load(Ordering::SeqCst), 1);
        assert_eq!(control_count(&h).await, 1);
    }
}

#[tokio::test]
async fn dropping_rpc_future_keeps_core_owned_completion_before_and_after_commit() {
    for mode in [3, 4] {
        let (h, probe) = probed().await;
        let service = owner(&h);
        let request = mutation(&service, "owned", start()).await;
        let prepared = protocol::parse_rpc(&request.to_string(), true).unwrap();
        probe.mode.store(mode, Ordering::SeqCst);
        let worker = {
            let service = service.clone();
            tokio::spawn(async move { protocol::execute_rpc(&service, prepared).await })
        };
        if mode == 3 {
            bounded(probe.entered.notified()).await;
        } else {
            bounded(probe.committed.notified()).await;
        }
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let read = service.snapshot();
        tokio::pin!(read);
        assert!(futures::poll!(read.as_mut()).is_pending());
        probe.release.notify_one();
        bounded(read).await.unwrap();
        assert_eq!(lookup(&service, "owned").await["requestId"], "owned");
        assert_eq!(
            call(&service, request).await.unwrap()["outcome"]["replayed"],
            true
        );
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.commits.load(Ordering::SeqCst), 1);
        assert_eq!(control_count(&h).await, 1);
    }
}

#[tokio::test]
async fn flat_read_and_guard_sampling_cancel_without_a_worker_or_append() {
    let (h, probe) = probed().await;
    let service = owner(&h);
    call(&service, mutation(&service, "seed", start()).await)
        .await
        .unwrap();
    let before = service.snapshot().await.unwrap();
    for params in [
        json!({"action":"status"}),
        json!({"action":"enqueue","requestId":"cancel-before-admission","text":"never"}),
    ] {
        probe.pause_branch_reads.store(true, Ordering::SeqCst);
        let signal = CancellationToken::new();
        let worker = {
            let service = service.clone();
            let signal = signal.clone();
            tokio::spawn(
                async move { protocol::flat_request(&service, params, Some(&signal)).await },
            )
        };
        bounded(probe.branch_read_entered.notified()).await;
        signal.cancel();
        assert_eq!(
            bounded(worker).await.unwrap().unwrap_err().code,
            "cancelled"
        );
        assert_eq!(bounded(service.snapshot()).await.unwrap(), before);
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control_count(&h).await, 1);
    }
}
