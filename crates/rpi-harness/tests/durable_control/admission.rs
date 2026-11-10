//! #18: admission must reach the shared FIFO gate before the caller first yields.
//! All tests use the default current-thread runtime: no spawned core worker can
//! run between the explicit polls below. Pending drains are polled unconstrained
//! so cooperative-budget exhaustion cannot masquerade as gate admission.
use super::*;

// poll! requires Unpin; pin temporary async/Notify futures without introducing
// a yield. Already-pinned caller/drain futures remain owned by their test scope.
macro_rules! poll_pinned {
    ($future:expr) => {{
        let mut future = std::pin::pin!($future);
        futures::poll!(future.as_mut())
    }};
}

async fn synthetic_memory() -> (AgentHarness, Arc<Probe>) {
    let store = rpi_harness::session::memory::InMemorySessionStorage::new(
        SessionMetadata {
            id: "synthetic-admission".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    );
    let p = Probe::new(Arc::new(store));
    (harness(p.clone()).await, p)
}

// Only the directly-polled snapshot is paused, never an owned worker. Still
// release the probe on unwind so later cleanup cannot inherit a test barrier.
struct ReleaseRead(Arc<Probe>);
impl Drop for ReleaseRead {
    fn drop(&mut self) {
        self.0.pause_branch_reads.store(false, Ordering::SeqCst);
        self.0.release_branch_read.notify_one();
    }
}

fn writes(p: &Probe, expected: usize) {
    assert_eq!(p.attempts.load(Ordering::SeqCst), expected);
    assert_eq!(p.commits.load(Ordering::SeqCst), expected);
}

async fn no_execution(h: &AgentHarness) {
    // No invented native audit, terminal, provider, or tool records.
    assert!(h
        .session()
        .storage()
        .find_records(&RecordQuery::default())
        .await
        .unwrap()
        .is_empty());
    assert!(h
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .iter()
        .all(|entry| matches!(entry, Entry::Custom(_))));
}

async fn exact_replay(
    h: &AgentHarness,
    s: &DurableService,
    p: &Probe,
    mutation: DurableMutation,
    count: usize,
) -> DurableOutcome {
    let receipt = bounded(s.request_outcome(&mutation.request_id))
        .await
        .unwrap()
        .expect("drain must not overtake an admitted mutation");
    assert_eq!(receipt.request_id, mutation.request_id);
    assert_eq!(receipt.expected, mutation.expected);
    assert_eq!(receipt.action, mutation.action);
    assert_eq!(receipt.run_id, mutation.run_id);
    assert_eq!(receipt.actor, "owner");
    assert!(receipt.changed);
    let snapshot = bounded(s.snapshot()).await.unwrap();
    assert_eq!(
        snapshot.scope.entry_id.as_deref(),
        Some(receipt.entry_id.as_str())
    );
    assert_eq!(receipt.control_revision, snapshot.scope.control_revision);
    let records = entries(h).await;
    assert_eq!(records.len(), count);
    let Entry::Custom(latest) = records.iter().max_by_key(|entry| entry.seq()).unwrap() else {
        panic!("expected a control entry")
    };
    let persisted: rpi_harness::durable::DurableReceipt = serde_json::from_value(
        latest.data.as_ref().unwrap()["receipts"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_eq!(persisted, receipt);
    let replay = bounded(s.mutate(mutation)).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, receipt);
    assert_eq!(replay.snapshot, snapshot);
    writes(p, count);
    assert_eq!(entries(h).await.len(), count);
    no_execution(h).await;
    replay
}

#[tokio::test]
async fn free_gate_is_owned_before_worker_poll_and_dropped_caller_survives_reopen() {
    bounded(async {
        let (h, p) = synthetic_memory().await;
        let s = service(&h);
        let mutation = request(&s, "free-gate", start()).await;
        let before = p.drain_calls.load(Ordering::SeqCst);
        let mut caller = Box::pin(s.mutate(mutation.clone()));
        assert!(poll_pinned!(caller.as_mut()).is_pending());
        writes(&p, 0);
        let mut drain = Box::pin(s.drain());
        assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        drop(caller); // Do not abort the implementation's owned worker.
        bounded(drain).await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        let outcome = exact_replay(&h, &s, &p, mutation.clone(), 1).await;
        let control = serde_json::to_value(entries(&h).await).unwrap();
        drop(h);
        let reopened = harness(p.inner.clone()).await;
        let reader = service(&reopened);
        assert_eq!(
            reader.snapshot().await.unwrap().state,
            outcome.snapshot.state
        );
        assert_eq!(
            reader.request_outcome("free-gate").await.unwrap(),
            Some(outcome.receipt.clone())
        );
        let replay = reader.mutate(mutation).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, outcome.receipt);
        assert_eq!(
            serde_json::to_value(entries(&reopened).await).unwrap(),
            control
        );
        no_execution(&reopened).await;
    })
    .await;
}

#[tokio::test]
async fn busy_gate_keeps_fifo_position_when_caller_and_snapshot_are_dropped() {
    bounded(async {
        let (h, p) = synthetic_memory().await;
        let _release = ReleaseRead(p.clone());
        let s = service(&h);
        apply(&s, "seed", json!({"action":"reset"})).await;
        let mutation = request(&s, "busy-gate", start()).await;
        let before = p.drain_calls.load(Ordering::SeqCst);
        p.pause_branch_reads.store(true, Ordering::SeqCst);
        let mut snapshot = Box::pin(s.snapshot());
        assert!(poll_pinned!(tokio::task::unconstrained(snapshot.as_mut())).is_pending());
        assert!(
            poll_pinned!(tokio::task::unconstrained(p.branch_read_entered.notified())).is_ready()
        );
        let mut caller = Box::pin(s.mutate(mutation.clone()));
        assert!(poll_pinned!(caller.as_mut()).is_pending());
        writes(&p, 1);
        let mut drain = Box::pin(s.drain());
        assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        drop(caller);
        drop(snapshot); // Release the actual gate owner, without scheduling workers.
        assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
        writes(&p, 1);
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        bounded(drain).await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        exact_replay(&h, &s, &p, mutation, 2).await;
    })
    .await;
}

#[tokio::test]
async fn depleted_coop_budget_still_admits_at_free_and_busy_gates() {
    bounded(async {
        for busy in [false, true] {
            let (h, p) = synthetic_memory().await;
            let _release = ReleaseRead(p.clone());
            let s = service(&h);
            if busy {
                apply(&s, "seed", json!({"action":"reset"})).await;
            }
            let count = usize::from(busy);
            let mutation = request(&s, "budget", start()).await;
            let before = p.drain_calls.load(Ordering::SeqCst);
            let mut snapshot = Box::pin(s.snapshot());
            if busy {
                p.pause_branch_reads.store(true, Ordering::SeqCst);
                assert!(poll_pinned!(tokio::task::unconstrained(snapshot.as_mut())).is_pending());
                assert!(
                    poll_pinned!(tokio::task::unconstrained(p.branch_read_entered.notified()))
                        .is_ready()
                );
            }
            // No yield here: the first caller lock poll must bypass coop only
            // for admission, not move the business body into the public caller.
            while tokio::task::coop::has_budget_remaining() {
                assert!(poll_pinned!(tokio::task::consume_budget()).is_ready());
            }
            assert!(!tokio::task::coop::has_budget_remaining());
            let mut caller = Box::pin(s.mutate(mutation.clone()));
            assert!(poll_pinned!(caller.as_mut()).is_pending());
            writes(&p, count);
            let mut drain = Box::pin(s.drain());
            assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
            drop(caller);
            drop(snapshot);
            assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
            writes(&p, count);
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
            bounded(drain).await.unwrap();
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
            exact_replay(&h, &s, &p, mutation, count + 1).await;
        }
    })
    .await;
}

#[tokio::test]
async fn cancelling_drain_preserves_admission_and_close_before_worker_never_fakes_success() {
    bounded(async {
        for close in [false, true] {
            let (h, p) = synthetic_memory().await;
            let s = service(&h);
            let mutation = request(&s, "cancel-wait", start()).await;
            let before = p.drain_calls.load(Ordering::SeqCst);
            let mut caller = Box::pin(s.mutate(mutation.clone()));
            assert!(poll_pinned!(caller.as_mut()).is_pending());
            let mut cancelled = Box::pin(s.drain());
            assert!(poll_pinned!(tokio::task::unconstrained(cancelled.as_mut())).is_pending());
            drop(caller);
            drop(cancelled);
            let mut retry = Box::pin(s.drain());
            assert!(poll_pinned!(tokio::task::unconstrained(retry.as_mut())).is_pending());
            if close {
                // close is immediate; explicitly poll it to keep the worker
                // unscheduled until after the closed flag has been published.
                assert!(matches!(
                    poll_pinned!(h.close()),
                    std::task::Poll::Ready(Ok(()))
                ));
                assert!(poll_pinned!(tokio::task::unconstrained(retry.as_mut())).is_pending());
            }
            writes(&p, 0);
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
            bounded(retry).await.unwrap();
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
            if close {
                writes(&p, 0);
                assert!(entries(&h).await.is_empty());
                assert!(matches!(s.snapshot().await, Err(DurableError::Closed)));
                assert!(matches!(
                    s.request_outcome("cancel-wait").await,
                    Err(DurableError::Closed)
                ));
                assert!(matches!(
                    s.mutate(mutation).await,
                    Err(DurableError::Closed)
                ));
                s.drain().await.unwrap(); // A closed but live owner can settle.
                let reopened = harness(p.inner.clone()).await;
                assert!(service(&reopened)
                    .request_outcome("cancel-wait")
                    .await
                    .unwrap()
                    .is_none());
                no_execution(&reopened).await;
            } else {
                exact_replay(&h, &s, &p, mutation, 1).await;
            }
            no_execution(&h).await;
        }
    })
    .await;
}

#[tokio::test]
async fn prefilters_do_not_queue_but_binding_errors_and_conflicts_settle_in_fifo_order() {
    bounded(async {
        let (h, p) = synthetic_memory().await;
        let _release = ReleaseRead(p.clone());
        let owner = service(&h);
        let other = DurableService::bind(&h, DurableActor::owner("other").unwrap());
        let child = DurableService::bind(&h, DurableActor::child("child").unwrap());
        apply(&owner, "seed", json!({"action":"reset"})).await;
        let first = request(&owner, "first", start()).await;
        let mut second = first.clone();
        second.request_id = "second".into();
        let mut denied = first.clone();
        denied.request_id = "denied".into();
        let mut invalid = first.clone();
        invalid.request_id = "invalid identity".into();
        let mut readonly = first.clone();
        readonly.action = serde_json::from_value(json!({"action":"status"})).unwrap();
        let mut oversized = first.clone();
        oversized.action = serde_json::from_value(json!({
            "action":"start", "name":"large",
            "steps":(0..100).map(|i| json!({"id":format!("s{i}"), "title":"x".repeat(1000)})).collect::<Vec<_>>()
        })).unwrap();
        assert!(serde_json::to_vec(&oversized).unwrap().len() > 64 * 1024);
        p.pause_branch_reads.store(true, Ordering::SeqCst);
        let mut snapshot = Box::pin(owner.snapshot());
        assert!(poll_pinned!(tokio::task::unconstrained(snapshot.as_mut())).is_pending());
        assert!(poll_pinned!(tokio::task::unconstrained(p.branch_read_entered.notified())).is_ready());
        // These must finish in the caller even with the actual gate occupied;
        // spawning a worker to reject them is not pure pre-admission validation.
        assert!(matches!(poll_pinned!(owner.mutate(invalid)), std::task::Poll::Ready(Err(DurableError::InvalidRequest(_)))));
        assert!(matches!(poll_pinned!(owner.mutate(readonly)), std::task::Poll::Ready(Err(DurableError::InvalidRequest(_)))));
        assert!(matches!(poll_pinned!(owner.mutate(oversized)), std::task::Poll::Ready(Err(DurableError::LimitExceeded))));
        drop(snapshot);
        let before = p.drain_calls.load(Ordering::SeqCst);
        // A valid child Start is admitted, then denied by business validation.
        let mut denied_call = Box::pin(child.mutate(denied));
        assert!(poll_pinned!(denied_call.as_mut()).is_pending());
        let mut early_drain = Box::pin(owner.drain());
        assert!(poll_pinned!(tokio::task::unconstrained(early_drain.as_mut())).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        drop(early_drain);
        let mut first_call = Box::pin(owner.mutate(first.clone()));
        let mut second_call = Box::pin(other.mutate(second));
        assert!(poll_pinned!(first_call.as_mut()).is_pending());
        assert!(poll_pinned!(second_call.as_mut()).is_pending());
        let mut drain = Box::pin(owner.drain());
        assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
        writes(&p, 1);
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        bounded(drain).await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        // Drain must have settled every preceding binding, including failures.
        // Do not await these callers: that could hide an overtaken worker by
        // scheduling it only after the barrier has already returned.
        writes(&p, 2);
        assert!(matches!(poll_pinned!(tokio::task::unconstrained(denied_call.as_mut())), std::task::Poll::Ready(Err(DurableError::PermissionDenied))));
        let success = match poll_pinned!(tokio::task::unconstrained(first_call.as_mut())) {
            std::task::Poll::Ready(Ok(outcome)) => outcome,
            other => panic!("first binding was not settled successfully: {other:?}"),
        };
        assert!(!success.replayed);
        assert!(matches!(poll_pinned!(tokio::task::unconstrained(second_call.as_mut())), std::task::Poll::Ready(Err(DurableError::Conflict))));
        assert!(child.request_outcome("denied").await.unwrap().is_none());
        assert!(other.request_outcome("second").await.unwrap().is_none());
        let replay = exact_replay(&h, &owner, &p, first, 2).await;
        assert_eq!(replay.receipt, success.receipt);
        apply(&owner, "after-errors", json!({"action":"enqueue", "text":"gate remains usable"})).await;
        writes(&p, 3);
        assert_eq!(entries(&h).await.len(), 3);
        no_execution(&h).await;
    })
    .await;
}

#[tokio::test]
async fn jsonl_unpolled_call_is_not_admitted_then_real_handoff_reopens_exact_bytes() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "admission.jsonl",
            JsonlV4Header::new(
                "synthetic-admission-jsonl".into(),
                1,
                "/".into(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(store));
        let h = harness(p.clone()).await;
        let s = service(&h);
        let mutation = request(&s, "jsonl-admission", start()).await;
        let mut caller = Box::pin(s.mutate(mutation.clone()));
        // Merely constructing a future is not admission; this drain neither
        // waits for it nor permanently fences future calls. JSONL setup and
        // this backend drain may yield, before the handoff experiment begins.
        bounded(s.drain()).await.unwrap();
        writes(&p, 0);
        assert!(entries(&h).await.is_empty());
        let before = p.drain_calls.load(Ordering::SeqCst);
        assert!(poll_pinned!(caller.as_mut()).is_pending());
        writes(&p, 0); // Stop at spawn/join Pending, before any JSONL worker I/O.
        let mut drain = Box::pin(s.drain());
        assert!(poll_pinned!(tokio::task::unconstrained(drain.as_mut())).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        drop(caller);
        bounded(drain).await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        let outcome = exact_replay(&h, &s, &p, mutation.clone(), 1).await;
        h.close().await.unwrap();
        bounded(s.drain()).await.unwrap();
        let control = serde_json::to_value(entries(&h).await).unwrap();
        let bytes = std::fs::read(dir.path().join("admission.jsonl")).unwrap();
        drop(h);
        p.inner.release_writer_lease();
        drop(p);
        let loaded = JsonlSessionStorage::load(
            fs,
            "admission.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = harness(Arc::new(loaded)).await;
        let reader = service(&reopened);
        assert_eq!(
            reader.snapshot().await.unwrap().state,
            outcome.snapshot.state
        );
        assert_eq!(
            reader.request_outcome("jsonl-admission").await.unwrap(),
            Some(outcome.receipt.clone())
        );
        let replay = bounded(reader.mutate(mutation)).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, outcome.receipt);
        bounded(reader.drain()).await.unwrap();
        assert_eq!(
            serde_json::to_value(entries(&reopened).await).unwrap(),
            control
        );
        assert_eq!(
            std::fs::read(dir.path().join("admission.jsonl")).unwrap(),
            bytes
        );
        no_execution(&reopened).await;
    })
    .await;
}
