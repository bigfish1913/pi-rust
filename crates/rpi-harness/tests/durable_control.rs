//! Integration contracts for the native P2 durable-control API. No provider calls.
#[path = "durable_control/admission.rs"]
mod admission;
#[path = "support/durable_control_probe.rs"]
mod probe;
#[path = "durable_control/lifecycle.rs"]
mod settlement;
use probe::Probe;
use rpi_agent::AgentMessage;
use rpi_ai::types::UserMessage;
use rpi_durable::control::{ControlError, QueueStatus, WorkflowStatus};
use rpi_harness::{
    agent_harness::{AgentHarness, BranchWriteGuard, ScopedEntry},
    durable::{DurableActor, DurableError, DurableMutation, DurableOutcome, DurableService},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
        types::*,
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

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("operation timed out")
}
async fn harness(store: Arc<dyn SessionStorage>) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session: Session::new(store, None),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn memory() -> AgentHarness {
    harness(AgentHarnessOptions::default().session.storage()).await
}
fn service(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner("owner").unwrap())
}
fn start() -> Value {
    json!({"action":"start", "name":"workflow", "steps":[{"id":"step", "title":"Test"}]})
}
async fn request(s: &DurableService, id: &str, action: Value) -> DurableMutation {
    DurableMutation {
        request_id: id.into(),
        expected: s.snapshot().await.unwrap().scope,
        action: serde_json::from_value(action).unwrap(),
        run_id: None,
    }
}
async fn apply(s: &DurableService, id: &str, action: Value) -> DurableOutcome {
    bounded(s.mutate(request(s, id, action).await))
        .await
        .unwrap()
}
async fn entries(h: &AgentHarness) -> Vec<Entry> {
    h.session()
        .storage()
        .find_entries(&EntryQuery {
            entry_type: Some("custom"),
            custom_type: Some("rpi.durable.control.v1".into()),
            ..Default::default()
        })
        .await
        .unwrap()
}
async fn lifecycle(h: &AgentHarness) -> rpi_durable::control::ControlState {
    let s = service(h);
    assert_eq!(s.snapshot().await.unwrap().scope.control_revision, 0);
    apply(&s, "start", start()).await;
    assert!(matches!(
        s.mutate(request(&s, "premature", json!({"action":"complete"})).await)
            .await,
        Err(DurableError::Control(ControlError::IncompleteSteps))
    ));
    apply(&s, "pause", json!({"action":"pause", "reason":"waiting"})).await;
    apply(&s, "resume", json!({"action":"resume"})).await;
    apply(
        &s,
        "checkpoint",
        json!({"action":"checkpoint", "step":"step", "status":"completed", "note":"verified"}),
    )
    .await;
    let done = apply(&s, "complete", json!({"action":"complete"})).await;
    assert_eq!(
        done.snapshot.state.workflow.unwrap().status,
        WorkflowStatus::Completed
    );
    let queued = apply(
        &s,
        "enqueue",
        json!({"action":"enqueue", "text":"check queue"}),
    )
    .await;
    let item = &queued.snapshot.state.queue[0].id;
    let claimed = apply(
        &s,
        "claim",
        json!({"action":"claim", "item":item, "claimedBy":"spoofed"}),
    )
    .await;
    assert_eq!(
        claimed.snapshot.state.queue[0].claimed_by.as_deref(),
        Some("owner")
    );
    let finished = apply(
        &s,
        "finish",
        json!({"action":"finish", "item":item, "status":"completed"}),
    )
    .await;
    assert_eq!(
        finished.snapshot.state.queue[0].status,
        QueueStatus::Completed
    );
    assert_eq!(
        s.request_outcome("finish").await.unwrap(),
        Some(finished.receipt)
    );
    assert_eq!(entries(h).await.len(), 8);
    finished.snapshot.state
}
async fn reopened(store: Arc<dyn SessionStorage>, expected: rpi_durable::control::ControlState) {
    let h = harness(store).await;
    let s = service(&h);
    assert_eq!(s.snapshot().await.unwrap().state, expected);
    assert!(s.request_outcome("finish").await.unwrap().is_some());
    apply(&s, "restart", start()).await;
    apply(&s, "fail", json!({"action":"fail", "reason":"deliberate"})).await;
    assert_eq!(
        s.snapshot().await.unwrap().state.workflow.unwrap().status,
        WorkflowStatus::Failed
    );
}
#[tokio::test]
async fn lifecycle_memory_reopen() {
    let h = memory().await;
    let expected = lifecycle(&h).await;
    let store = h.session().storage();
    drop(h);
    reopened(store, expected).await;
}
#[tokio::test]
async fn lifecycle_jsonl_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs.clone(),
        "session.jsonl",
        JsonlV4Header::new("durable".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = harness(Arc::new(store)).await;
    let expected = lifecycle(&h).await;
    h.drain_session().await;
    drop(h);
    let store = JsonlSessionStorage::load(
        fs,
        "session.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    reopened(Arc::new(store), expected).await;
}
#[tokio::test]
async fn concurrent_claim_and_actor_permissions() {
    let h = memory().await;
    let owner = service(&h);
    let a = DurableService::bind(&h, DurableActor::child("a").unwrap());
    let b = DurableService::bind(&h, DurableActor::child("b").unwrap());
    for action in [start(), json!({"action":"reset"})] {
        assert!(matches!(
            a.mutate(request(&a, "forbidden", action).await).await,
            Err(DurableError::PermissionDenied)
        ));
    }
    apply(
        &owner,
        "enqueue",
        json!({"action":"enqueue", "text":"race"}),
    )
    .await;
    let ra = request(&a, "claim-a", json!({"action":"claim", "item":"item-1"})).await;
    let rb = request(&b, "claim-b", json!({"action":"claim", "item":"item-1"})).await;
    let (x, y) = bounded(async { tokio::join!(a.mutate(ra), b.mutate(rb)) }).await;
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    assert!(matches!(
        if x.is_err() {
            x.unwrap_err()
        } else {
            y.unwrap_err()
        },
        DurableError::Conflict
    ));
    let claimed = owner.snapshot().await.unwrap();
    let winner = claimed.state.queue[0].claimed_by.as_deref().unwrap();
    let loser = if winner == "a" { &b } else { &a };
    for action in [
        json!({"action":"finish", "item":"item-1", "status":"completed"}),
        json!({"action":"cancel", "item":"item-1"}),
    ] {
        assert!(matches!(
            loser.mutate(request(loser, "steal", action).await).await,
            Err(DurableError::PermissionDenied)
        ));
    }
    assert!(matches!(
        owner
            .mutate(
                request(
                    &owner,
                    "root-finish",
                    json!({"action":"finish", "item":"item-1", "status":"completed"})
                )
                .await
            )
            .await,
        Err(DurableError::PermissionDenied)
    ));
    apply(
        &owner,
        "root-cancel",
        json!({"action":"cancel", "item":"item-1"}),
    )
    .await;
    assert_eq!(
        owner.snapshot().await.unwrap().state.queue[0].status,
        QueueStatus::Cancelled
    );
}
#[tokio::test]
async fn dedup_conflicts_and_noop_receipts() {
    let h = memory().await;
    let s = service(&h);
    let reset = request(&s, "reset", json!({"action":"reset"})).await;
    let outcome = s.mutate(reset.clone()).await.unwrap();
    assert!(!outcome.receipt.changed);
    assert_eq!(outcome.snapshot.scope.control_revision, 0);
    assert_ne!(outcome.snapshot.scope, reset.expected);
    assert!(s.mutate(reset.clone()).await.unwrap().replayed);
    assert_eq!(entries(&h).await.len(), 1);
    let mut conflict = reset.clone();
    conflict.expected = outcome.snapshot.scope;
    assert!(matches!(
        s.mutate(conflict).await,
        Err(DurableError::Conflict)
    ));
    let mut conflict = reset.clone();
    conflict.action = serde_json::from_value(start()).unwrap();
    assert!(matches!(
        s.mutate(conflict).await,
        Err(DurableError::Conflict)
    ));
    let other = DurableService::bind(&h, DurableActor::owner("other").unwrap());
    assert!(matches!(
        other.mutate(reset.clone()).await,
        Err(DurableError::Conflict)
    ));
    assert!(other.request_outcome("reset").await.unwrap().is_none());
    apply(&s, "start", start()).await;
    assert!(matches!(s.mutate(reset).await, Err(DurableError::Conflict)));
    assert!(
        s.snapshot().await.unwrap().state.workflow.is_some(),
        "old no-op reset cannot erase newer state"
    );
    assert_eq!(entries(&h).await.len(), 2);
}
#[tokio::test]
async fn stale_message_navigation_rebind_and_native_write() {
    let h = memory().await;
    let s = service(&h);
    let old = request(&s, "stale", start()).await;
    h.append_message(AgentMessage::User(UserMessage::new("ordinary", 0)))
        .await
        .unwrap();
    assert!(matches!(s.mutate(old).await, Err(DurableError::Conflict)));
    let old = request(&s, "navigation", start()).await;
    h.session().move_lane("main", None).await.unwrap();
    assert!(matches!(s.mutate(old).await, Err(DurableError::Conflict)));
    let old = request(&s, "native", start()).await;
    h.session()
        .append_custom_entry("other-native", None)
        .await
        .unwrap();
    assert!(matches!(s.mutate(old).await, Err(DurableError::Conflict)));
    let old = request(&s, "rebind", start()).await;
    h.session().set_storage(h.session().storage());
    assert_ne!(
        old.expected.binding_token,
        s.snapshot().await.unwrap().scope.binding_token
    );
    assert!(matches!(s.mutate(old).await, Err(DurableError::Conflict)));
}
#[tokio::test]
async fn lane_session_isolation_closed_and_weak_owner() {
    let h = memory().await;
    h.session()
        .storage()
        .create_lane("side", None)
        .await
        .unwrap();
    let main = service(&h);
    let side = DurableService::bind_lane(&h, DurableActor::owner("owner").unwrap(), "side");
    apply(&main, "main-start", start()).await;
    assert!(side.snapshot().await.unwrap().state.workflow.is_none());
    apply(
        &side,
        "side-start",
        json!({"action":"start", "name":"side", "steps":[{"id":"s", "title":"Side"}]}),
    )
    .await;
    assert_eq!(
        main.snapshot().await.unwrap().state.workflow.unwrap().name,
        "workflow"
    );
    let another = memory().await;
    assert!(service(&another)
        .snapshot()
        .await
        .unwrap()
        .state
        .workflow
        .is_none());
    h.set_session(another.session().clone()).await.unwrap();
    assert!(main.snapshot().await.unwrap().state.workflow.is_none());
    h.close().await.unwrap();
    assert!(matches!(main.snapshot().await, Err(DurableError::Closed)));
    assert!(matches!(
        main.request_outcome("main-start").await,
        Err(DurableError::Closed)
    ));
    let h = memory().await;
    let weak = service(&h);
    let mutation = request(&weak, "weak", start()).await;
    drop(h);
    assert!(matches!(weak.snapshot().await, Err(DurableError::Closed)));
    assert!(matches!(
        weak.mutate(mutation).await,
        Err(DurableError::Closed)
    ));
}
#[tokio::test]
async fn malformed_legacy_and_control_payloads_fail_closed() {
    for custom_type in [
        "durable-state",
        "rpi-durable-runtime",
        "rpi.durable.control.v1",
    ] {
        for data in [None, Some(json!("malformed")), Some(json!({}))] {
            let h = memory().await;
            // Session is the trusted low-level corruption-injection boundary.
            h.session()
                .append_custom_entry(custom_type, data)
                .await
                .unwrap();
            h.append_message(AgentMessage::User(UserMessage::new("later entry", 0)))
                .await
                .unwrap();
            let error = service(&h).snapshot().await.unwrap_err();
            if custom_type == "rpi.durable.control.v1" {
                assert!(
                    matches!(error, DurableError::InvalidJournal(_)),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(error, DurableError::UnsupportedJournalFormat),
                    "{error:?}"
                );
            }
        }
    }
}
#[tokio::test]
async fn reserved_namespace_rejected_for_normal_lane_and_scoped_writes() {
    let h = memory().await;
    h.session()
        .storage()
        .create_lane("side", None)
        .await
        .unwrap();
    for custom_type in ["rpi.durable.control.v1", "rpi.durable.future"] {
        assert!(h.append_custom_entry(custom_type, None).await.is_err());
        assert!(h
            .lane("side")
            .append_custom_entry(custom_type, None)
            .await
            .is_err());
        let scope = service(&h).snapshot().await.unwrap().scope;
        assert!(h
            .append_scoped_entry(
                &BranchWriteGuard {
                    session_id: scope.session_id,
                    leaf_id: scope.leaf_id,
                    custom_type: custom_type.into(),
                    entry_id: None
                },
                ScopedEntry::Custom {
                    custom_type: custom_type.into(),
                    data: None
                }
            )
            .await
            .is_err());
    }
    assert_eq!(entries(&h).await.len(), 0);
    h.append_custom_entry("ordinary", None).await.unwrap();
}
#[tokio::test]
async fn receipt_and_request_bounds_and_execution_facts() {
    let h = memory().await;
    let s = service(&h);
    for action in ["status", "queue"] {
        assert!(matches!(
            s.mutate(request(&s, action, json!({"action":action})).await)
                .await,
            Err(DurableError::InvalidRequest(_))
        ));
    }
    for action in ["recover", "forget_run"] {
        assert!(matches!(
            s.mutate(request(&s, action, json!({"action":action})).await)
                .await,
            Err(DurableError::InvalidRequest(_))
        ));
    }
    let oversized = json!({"action":"start", "name":"large", "steps":(0..100).map(|i| json!({"id":format!("s{i}"), "title":"x".repeat(1000)})).collect::<Vec<_>>()});
    assert!(matches!(
        s.mutate(request(&s, "oversized", oversized).await).await,
        Err(DurableError::LimitExceeded)
    ));
    assert!(entries(&h).await.is_empty());
    for i in 0..64 {
        apply(&s, &format!("noop-{i}"), json!({"action":"reset"})).await;
    }
    let last = request(&s, "overflow", start()).await;
    let before = s.snapshot().await.unwrap();
    assert!(matches!(
        s.mutate(last).await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(before.scope.control_revision, 0);
    assert_eq!(entries(&h).await.len(), 64);
}
async fn probed() -> (AgentHarness, Arc<Probe>) {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    (harness(p.clone()).await, p)
}
#[tokio::test]
async fn reads_fail_closed_even_after_successful_snapshot() {
    let (h, p) = probed().await;
    let s = service(&h);
    apply(&s, "start", start()).await;
    let mutation = request(&s, "reset", json!({"action":"reset"})).await;
    for revision in [false, true] {
        p.fail_reads.store(!revision, Ordering::SeqCst);
        p.fail_revision.store(revision, Ordering::SeqCst);
        assert!(matches!(
            s.snapshot().await,
            Err(DurableError::ReadFailure(_))
        ));
        assert!(matches!(
            s.request_outcome("start").await,
            Err(DurableError::ReadFailure(_))
        ));
        assert!(matches!(
            s.mutate(mutation.clone()).await,
            Err(DurableError::ReadFailure(_))
        ));
    }
    p.fail_reads.store(false, Ordering::SeqCst);
    p.fail_revision.store(false, Ordering::SeqCst);
    assert!(s.snapshot().await.unwrap().state.workflow.is_some());
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn precommit_failure_can_retry_without_phantom_state() {
    let (h, p) = probed().await;
    let s = service(&h);
    let mutation = request(&s, "start", start()).await;
    p.mode.store(1, Ordering::SeqCst);
    assert!(matches!(
        s.mutate(mutation.clone()).await,
        Err(DurableError::CommitUncertain(_))
    ));
    assert!(s.snapshot().await.unwrap().state.workflow.is_none());
    assert!(s.request_outcome("start").await.unwrap().is_none());
    assert_eq!(p.commits.load(Ordering::SeqCst), 0);
    assert!(!s.mutate(mutation).await.unwrap().replayed);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn lost_ack_identical_retry_discovers_receipt_without_duplicate() {
    let (h, p) = probed().await;
    let s = service(&h);
    let mutation = request(&s, "start", start()).await;
    p.mode.store(2, Ordering::SeqCst);
    assert!(matches!(
        s.mutate(mutation.clone()).await,
        Err(DurableError::CommitUncertain(_))
    ));
    assert!(s.request_outcome("start").await.unwrap().is_some());
    assert!(s.snapshot().await.unwrap().state.workflow.is_some());
    let reconciled = s.reconcile(&mutation).await.unwrap().unwrap();
    assert!(reconciled.replayed);
    assert!(s.mutate(mutation).await.unwrap().replayed);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    assert_eq!(entries(&h).await.len(), 1);
}
async fn dropped_caller(mode: usize) {
    let (h, p) = probed().await;
    let s = service(&h);
    let mutation = request(&s, "start", start()).await;
    p.mode.store(mode, Ordering::SeqCst);
    let worker = {
        let s = s.clone();
        let mutation = mutation.clone();
        tokio::spawn(async move { s.mutate(mutation).await })
    };
    if mode == 3 {
        bounded(p.entered.notified()).await;
    } else {
        bounded(p.committed.notified()).await;
    }
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    // A cancelled caller must not release the message gate while storage is live.
    let message = h.append_message(AgentMessage::User(UserMessage::new("after durable", 0)));
    tokio::pin!(message);
    assert!(futures::poll!(message.as_mut()).is_pending());
    // Release an actual storage boundary, not a sleep-based approximation.
    p.release.notify_one();
    // Drive the queued message waiter before queuing another fair-mutex waiter.
    bounded(message).await.unwrap();
    let receipt = bounded(s.request_outcome("start")).await.unwrap().unwrap();
    assert_eq!(receipt.request_id, "start");
    assert!(bounded(s.mutate(mutation)).await.unwrap().replayed);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    assert_eq!(entries(&h).await.len(), 1);
}
#[tokio::test]
async fn dropped_caller_before_actual_append_keeps_owned_completion() {
    dropped_caller(3).await;
}
#[tokio::test]
async fn dropped_caller_after_actual_append_keeps_owned_completion() {
    dropped_caller(4).await;
}
#[tokio::test]
async fn independent_harnesses_are_fenced_by_storage_conditional_append() {
    let (h, p) = probed().await;
    let other = harness(p.inner.clone()).await;
    let a = service(&h);
    let b = service(&other);
    let ra = request(&a, "a-start", start()).await;
    let rb = request(&b, "b-start", start()).await;
    p.mode.store(3, Ordering::SeqCst);
    let first = tokio::spawn(async move { a.mutate(ra).await });
    bounded(p.entered.notified()).await;
    b.mutate(rb).await.unwrap();
    p.release.notify_one();
    assert!(matches!(
        bounded(first).await.unwrap(),
        Err(DurableError::CommitUncertain(_))
    ));
    assert_eq!(entries(&h).await.len(), 1);
    assert!(service(&h)
        .request_outcome("a-start")
        .await
        .unwrap()
        .is_none());
    assert!(service(&h)
        .request_outcome("b-start")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn invalid_receipt_chains_and_state_are_not_empty_workflows() {
    let seed = memory().await;
    apply(&service(&seed), "seed", start()).await;
    let Entry::Custom(record) = entries(&seed).await.pop().unwrap() else {
        panic!("custom entry required")
    };
    let payload = record.data.unwrap();
    for damage in [
        "format",
        "version",
        "previous",
        "empty-receipts",
        "duplicate",
        "actor",
        "session",
        "lane",
        "state",
        "revision",
    ] {
        let h = memory().await;
        let s = service(&h);
        let mut corrupt = payload.clone();
        // Make the otherwise-valid seed refer to this session and entry, so each
        // test exercises a specific corrupt field rather than foreign identity.
        corrupt["receipts"][0]["expected"]["sessionId"] = json!(h.session_id());
        corrupt["receipts"][0]["entryId"] = json!("corrupt");
        match damage {
            "format" => corrupt["format"] = json!("not-native"),
            "version" => corrupt["version"] = json!(2),
            "previous" => corrupt["previousEntryId"] = json!("missing"),
            "empty-receipts" => corrupt["receipts"] = json!([]),
            "duplicate" => {
                let receipt = corrupt["receipts"][0].clone();
                corrupt["receipts"].as_array_mut().unwrap().push(receipt);
            }
            "actor" => corrupt["receipts"][0]["actor"] = json!("untrusted actor"),
            "session" => corrupt["receipts"][0]["expected"]["sessionId"] = json!("foreign"),
            "lane" => corrupt["receipts"][0]["expected"]["lane"] = json!("side"),
            "state" => corrupt["state"]["workflow"]["steps"] = json!([]),
            "revision" => corrupt["state"]["revision"] = json!(0),
            _ => unreachable!(),
        }
        h.session()
            .storage()
            .append_entry(
                ProvisionedEntry {
                    id: "corrupt".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: "rpi.durable.control.v1".into(),
                        data: Some(corrupt),
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert!(
            matches!(s.snapshot().await, Err(DurableError::InvalidJournal(_))),
            "{damage}"
        );
        assert!(
            matches!(
                s.request_outcome("seed").await,
                Err(DurableError::InvalidJournal(_))
            ),
            "{damage}"
        );
    }
}

#[tokio::test]
async fn record_budget_rejects_growth_without_committing_or_resetting_state() {
    let h = memory().await;
    let s = service(&h);
    // Escaped control characters consume six JSON bytes per scalar. Each legal
    // request is below 64 KiB, but accumulated state + receipts exceed 1 MiB.
    let text = "\u{1}".repeat(4000);
    let mut rejected = false;
    for i in 0..32 {
        let mutation = request(
            &s,
            &format!("large-{i}"),
            json!({"action":"enqueue", "text":text}),
        )
        .await;
        assert!(serde_json::to_vec(&mutation).unwrap().len() < 64 * 1024);
        let before = s.snapshot().await.unwrap();
        match s.mutate(mutation).await {
            Ok(outcome) => assert_eq!(outcome.snapshot.state.queue.len(), i + 1),
            Err(DurableError::LimitExceeded) => {
                assert_eq!(s.snapshot().await.unwrap(), before);
                assert_eq!(entries(&h).await.len(), i);
                assert!(s
                    .request_outcome(&format!("large-{i}"))
                    .await
                    .unwrap()
                    .is_none());
                rejected = true;
                break;
            }
            other => panic!("unexpected record-budget outcome: {other:?}"),
        }
    }
    assert!(
        rejected,
        "record cap must apply before the 32-item domain cap"
    );
    for entry in entries(&h).await {
        let Entry::Custom(custom) = entry else {
            unreachable!()
        };
        assert!(serde_json::to_vec(&custom.data.unwrap()).unwrap().len() <= 1024 * 1024);
    }
    // An oversized record already in storage is likewise never fake empty state.
    let h = memory().await;
    h.session()
        .append_custom_entry(
            "rpi.durable.control.v1",
            Some(json!({"oversized":"x".repeat(1024 * 1024)})),
        )
        .await
        .unwrap();
    assert!(matches!(
        service(&h).snapshot().await,
        Err(DurableError::LimitExceeded)
    ));
}

#[tokio::test]
async fn child_can_finish_own_claim_and_reset_preserves_manual_queue() {
    let h = memory().await;
    let root = service(&h);
    let child = DurableService::bind(&h, DurableActor::child("child").unwrap());
    apply(&root, "start", start()).await;
    apply(
        &child,
        "enqueue",
        json!({"action":"enqueue", "text":"owned work"}),
    )
    .await;
    apply(&child, "claim", json!({"action":"claim", "item":"item-1"})).await;
    let outcome = apply(
        &child,
        "finish",
        json!({"action":"finish", "item":"item-1", "status":"failed", "note":"verified failure"}),
    )
    .await;
    assert_eq!(outcome.receipt.actor, "child");
    assert_eq!(outcome.snapshot.state.queue[0].status, QueueStatus::Failed);
    assert_eq!(
        outcome.snapshot.state.queue[0].claimed_by.as_deref(),
        Some("child")
    );
    let queue = outcome.snapshot.state.queue;
    let reset = apply(&root, "reset", json!({"action":"reset"})).await;
    assert!(reset.receipt.changed);
    assert!(reset.snapshot.state.workflow.is_none());
    assert_eq!(reset.snapshot.state.queue, queue);
}

#[tokio::test]
async fn rebind_during_commit_ack_never_mixes_receipt_and_replacement_state() {
    for same_id in [false, true] {
        let (h, p) = probed().await;
        let shared = AgentHarness::create(AgentHarnessOptions {
            session: h.session().clone(),
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let replacement: Arc<dyn SessionStorage> = if same_id {
            Arc::new(rpi_harness::session::memory::InMemorySessionStorage::new(
                p.inner.metadata(),
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            ))
        } else {
            AgentHarnessOptions::default().session.storage()
        };
        let s = service(&h);
        let mutation = request(&s, "ack-rebind", start()).await;
        p.mode.store(4, Ordering::SeqCst);
        let worker = {
            let s = s.clone();
            tokio::spawn(async move { s.mutate(mutation).await })
        };
        bounded(p.committed.notified()).await;
        bounded(shared.set_session(Session::new(replacement, None)))
            .await
            .unwrap();
        p.release.notify_one();
        assert!(matches!(
            bounded(worker).await.unwrap(),
            Err(DurableError::CommitUncertain(_))
        ));
        assert!(s.snapshot().await.unwrap().state.workflow.is_none());
        assert!(s.request_outcome("ack-rebind").await.unwrap().is_none());
        // The accepted write was not redirected or lost: inspect its original
        // storage via a newly bound owner, not the replacement facade.
        let original = harness(p.inner.clone()).await;
        assert!(service(&original)
            .request_outcome("ack-rebind")
            .await
            .unwrap()
            .is_some());
        assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn navigation_during_commit_ack_does_not_report_hidden_receipt_as_success() {
    let (h, p) = probed().await;
    let s = service(&h);
    let mutation = request(&s, "ack-navigation", start()).await;
    p.mode.store(4, Ordering::SeqCst);
    let worker = {
        let s = s.clone();
        tokio::spawn(async move { s.mutate(mutation).await })
    };
    bounded(p.committed.notified()).await;
    p.inner.move_lane("main", None).await.unwrap();
    p.release.notify_one();
    assert!(matches!(
        bounded(worker).await.unwrap(),
        Err(DurableError::CommitUncertain(_))
    ));
    assert!(s.request_outcome("ack-navigation").await.unwrap().is_none());
    assert_eq!(entries(&h).await.len(), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn postcommit_read_failure_is_uncertain_and_discoverable_without_reappend() {
    let (h, p) = probed().await;
    let s = service(&h);
    let mutation = request(&s, "ack-read", start()).await;
    p.mode.store(4, Ordering::SeqCst);
    let worker = {
        let s = s.clone();
        let mutation = mutation.clone();
        tokio::spawn(async move { s.mutate(mutation).await })
    };
    bounded(p.committed.notified()).await;
    p.fail_reads.store(true, Ordering::SeqCst);
    p.release.notify_one();
    assert!(matches!(
        bounded(worker).await.unwrap(),
        Err(DurableError::CommitUncertain(_))
    ));
    p.fail_reads.store(false, Ordering::SeqCst);
    assert!(s.request_outcome("ack-read").await.unwrap().is_some());
    assert!(s.mutate(mutation).await.unwrap().replayed);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn truncated_receipt_prefix_is_corruption_not_a_forgotten_request() {
    let seed = memory().await;
    let s = service(&seed);
    apply(&s, "first", start()).await;
    apply(&s, "second", json!({"action":"pause"})).await;
    let Entry::Custom(record) = entries(&seed)
        .await
        .into_iter()
        .max_by_key(Entry::seq)
        .unwrap()
    else {
        panic!("custom entry required")
    };
    let h = memory().await;
    let s = service(&h);
    let mutation = request(&s, "first", start()).await;
    let mut corrupt = record.data.unwrap();
    let receipts = corrupt["receipts"].as_array_mut().unwrap();
    assert_eq!(receipts.len(), 2);
    receipts.remove(0);
    receipts[0]["expected"]["sessionId"] = json!(h.session_id());
    h.session()
        .storage()
        .append_entry(
            ProvisionedEntry {
                id: record.base.id,
                kind: ProvisionedKind::Custom {
                    custom_type: "rpi.durable.control.v1".into(),
                    data: Some(corrupt),
                },
            },
            "main",
        )
        .await
        .unwrap();
    assert!(matches!(
        s.snapshot().await,
        Err(DurableError::InvalidJournal(_))
    ));
    assert!(matches!(
        s.request_outcome("first").await,
        Err(DurableError::InvalidJournal(_))
    ));
    assert!(matches!(
        s.mutate(mutation).await,
        Err(DurableError::InvalidJournal(_))
    ));
    assert_eq!(entries(&h).await.len(), 1);
}

#[tokio::test]
async fn invalid_identities_and_foreign_scopes_do_not_write() {
    let h = memory().await;
    let s = service(&h);
    for id in ["", "white space", "slash/path", "é", &"x".repeat(129)] {
        assert!(matches!(
            DurableActor::owner(id),
            Err(DurableError::InvalidRequest(_))
        ));
        assert!(matches!(
            DurableActor::child(id),
            Err(DurableError::InvalidRequest(_))
        ));
        let mutation = request(&s, id, start()).await;
        assert!(matches!(
            s.mutate(mutation).await,
            Err(DurableError::InvalidRequest(_))
        ));
        assert!(matches!(
            s.request_outcome(id).await,
            Err(DurableError::InvalidRequest(_))
        ));
    }
    let another = memory().await;
    let foreign = request(&service(&another), "foreign", start()).await;
    assert!(matches!(
        s.mutate(foreign).await,
        Err(DurableError::Conflict)
    ));
    h.session()
        .storage()
        .create_lane("side", None)
        .await
        .unwrap();
    let side = DurableService::bind_lane(&h, DurableActor::owner("owner").unwrap(), "side");
    let foreign = request(&side, "wrong-lane", start()).await;
    assert!(matches!(
        s.mutate(foreign).await,
        Err(DurableError::Conflict)
    ));
    let old = request(&s, "old-session", start()).await;
    h.set_session(another.session().clone()).await.unwrap();
    assert!(matches!(s.mutate(old).await, Err(DurableError::Conflict)));
    assert!(entries(&h).await.is_empty());
}
