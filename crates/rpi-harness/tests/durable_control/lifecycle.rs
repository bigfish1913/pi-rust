//! Explicit settlement barriers, not success receipts or whole-run fences.
//! Registered by the parent as `settlement` to avoid the root `lifecycle` helper.
use super::*;

// The public caller may be aborted; its admitted mutation is intentionally owned
// by the implementation. Always release test storage barriers on unwind/timeout
// so a failing assertion cannot strand that owned worker.
struct MutationTask {
    caller: tokio::task::JoinHandle<Result<DurableOutcome, DurableError>>,
    probe: Arc<Probe>,
}
impl MutationTask {
    fn spawn(s: &DurableService, mutation: DurableMutation, probe: &Arc<Probe>) -> Self {
        let s = s.clone();
        Self {
            caller: tokio::spawn(async move { s.mutate(mutation).await }),
            probe: probe.clone(),
        }
    }
    async fn abort_caller(&mut self) {
        self.caller.abort();
        assert!(bounded(&mut self.caller).await.unwrap_err().is_cancelled());
    }
}
impl Drop for MutationTask {
    fn drop(&mut self) {
        self.caller.abort();
        self.probe.release.notify_one();
        self.probe.release_drain.notify_one();
    }
}

// Also unwind backend-only pauses if an assertion/outer timeout fails. This
// holds storage, never a strong harness owner, and does not participate in drain.
struct ReleaseBarriers(Arc<Probe>);
impl Drop for ReleaseBarriers {
    fn drop(&mut self) {
        self.0.release.notify_one();
        self.0.release_drain.notify_one();
    }
}

async fn append_boundary(p: &Probe, mode: usize) {
    match mode {
        3 => bounded(p.entered.notified()).await,
        4 => bounded(p.committed.notified()).await,
        _ => panic!("a real before-append or after-commit barrier is required"),
    }
}

async fn no_native_terminal(h: &AgentHarness) {
    assert!(h
        .session()
        .storage()
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
}

// Deliberately synthetic audit facts, not provider execution or get_result
// evidence. A declined operation does not invent an assistant/result identity.
async fn append_native_audit(h: &AgentHarness) {
    let base = |id: &str| RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    };
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("synthetic-audit"),
            source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("synthetic-audit-declined"),
            run_id: "synthetic-audit".into(),
            outcome: OperationOutcome::Declined,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn drain_waits_for_owned_append_before_and_after_commit_despite_caller_abort() {
    bounded(async {
        for mode in [3, 4] {
            let (h, p) = probed().await;
            let s = service(&h);
            let mutation = request(&s, "owned-start", start()).await;
            let drains_before = p.drain_calls.load(Ordering::SeqCst);
            p.mode.store(mode, Ordering::SeqCst);
            let mut task = MutationTask::spawn(&s, mutation.clone(), &p);
            append_boundary(&p, mode).await;
            assert_eq!(p.commits.load(Ordering::SeqCst), usize::from(mode == 4));
            task.abort_caller().await;

            let mut drain = Box::pin(s.drain());
            assert!(futures::poll!(drain.as_mut()).is_pending());
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), drains_before);
            p.release.notify_one();
            bounded(drain).await.unwrap();
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), drains_before + 1);
            let receipt = s.request_outcome("owned-start").await.unwrap().unwrap();
            let replay = s.mutate(mutation).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.receipt, receipt);
            assert_eq!(replay.snapshot.state.workflow.unwrap().name, "workflow");
            assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(p.commits.load(Ordering::SeqCst), 1);
            assert_eq!(entries(&h).await.len(), 1);
            no_native_terminal(&h).await;
        }
        // Ok is settlement only: an append failure (or lost ACK) is not
        // retroactively converted into a successful mutation by draining.
        for mode in [1, 2] {
            let (h, p) = probed().await;
            let s = service(&h);
            let mutation = request(&s, "uncertain", start()).await;
            p.mode.store(mode, Ordering::SeqCst);
            assert!(matches!(
                s.mutate(mutation).await,
                Err(DurableError::CommitUncertain(_))
            ));
            let before = p.drain_calls.load(Ordering::SeqCst);
            s.drain().await.unwrap();
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
            assert_eq!(p.commits.load(Ordering::SeqCst), usize::from(mode == 2));
            assert_eq!(entries(&h).await.len(), usize::from(mode == 2));
            assert_eq!(
                s.request_outcome("uncertain").await.unwrap().is_some(),
                mode == 2
            );
            no_native_terminal(&h).await;
        }
    })
    .await;
}

#[tokio::test]
async fn close_is_immediate_but_drain_settles_admitted_work_and_rejects_waiters() {
    bounded(async {
        for mode in [3, 4] {
            let (h, p) = probed().await;
            let s = service(&h);
            let accepted = request(&s, "accepted", start()).await;
            let rejected = request(&s, "must-not-commit", start()).await;
            let child = h
                .create_child(
                    Session::new(
                        Arc::new(rpi_harness::session::memory::InMemorySessionStorage::new(
                            SessionMetadata {
                                id: format!("settlement-child-{mode}"),
                                created_at: 0,
                                parent_session_id: None,
                            },
                            Arc::new(SystemClock),
                            Arc::new(DefaultIdGenerator::new()),
                        )),
                        None,
                    ),
                    Arc::new(rpi_agent::CollectorEmitter::new().0),
                    1,
                    Some(vec![]),
                )
                .await
                .unwrap();
            assert!(!child.scope.is_cancelled());
            p.mode.store(mode, Ordering::SeqCst);
            let mut task = MutationTask::spawn(&s, accepted.clone(), &p);
            append_boundary(&p, mode).await;

            // This real gate waiter precedes drain in the FIFO mutex queue.
            let mut queued_read = Box::pin(s.snapshot());
            assert!(futures::poll!(queued_read.as_mut()).is_pending());
            // Polling mutate starts its owned admission attempt, but does not
            // assume that a spawned worker has already reached the gate.
            let mut queued_mutation = Box::pin(s.mutate(rejected.clone()));
            assert!(futures::poll!(queued_mutation.as_mut()).is_pending());
            bounded(h.close()).await.unwrap();
            assert!(child.scope.is_cancelled());
            let drains_before = p.drain_calls.load(Ordering::SeqCst);
            let mut drain = Box::pin(s.drain());
            assert!(futures::poll!(drain.as_mut()).is_pending());
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), drains_before);
            p.release.notify_one();
            let (read, mutation, settled, caller) =
                tokio::join!(queued_read, queued_mutation, drain, &mut task.caller);
            assert!(matches!(read, Err(DurableError::Closed)));
            assert!(matches!(mutation, Err(DurableError::Closed)));
            settled.unwrap();
            assert!(matches!(
                caller.unwrap(),
                Err(DurableError::CommitUncertain(_))
            ));
            assert!(matches!(s.snapshot().await, Err(DurableError::Closed)));
            assert!(matches!(
                s.request_outcome("accepted").await,
                Err(DurableError::Closed)
            ));
            assert!(matches!(
                s.execution("valid-run-id").await,
                Err(DurableError::Closed)
            ));
            assert!(matches!(
                s.mutate(rejected).await,
                Err(DurableError::Closed)
            ));
            assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(p.commits.load(Ordering::SeqCst), 1);
            assert_eq!(entries(&h).await.len(), 1);
            no_native_terminal(&h).await;

            let reopened = harness(p.inner.clone()).await;
            let reader = service(&reopened);
            let receipt = reader.request_outcome("accepted").await.unwrap().unwrap();
            assert_eq!(receipt.expected, accepted.expected);
            assert_eq!(receipt.action, accepted.action);
            let replay = reader.mutate(accepted).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.receipt, receipt);
            assert!(reader
                .request_outcome("must-not-commit")
                .await
                .unwrap()
                .is_none());
            assert_eq!(entries(&reopened).await.len(), 1);
            no_native_terminal(&reopened).await;
        }
    })
    .await;
}

#[tokio::test]
async fn physical_history_size_is_explicitly_optional_for_memory() {
    bounded(async {
        let h = memory().await;
        let service = service(&h);
        assert_eq!(service.history_physical_bytes().await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn canonical_snapshot_validation_is_an_exact_read_only_cut_point() {
    bounded(async {
        let h = memory().await;
        let s = service(&h);
        let before = s.snapshot().await.unwrap();
        assert_eq!(s.validate_snapshot(&before).await.unwrap(), before);
        apply(&s, "cut-point-start", start()).await;
        assert!(matches!(
            s.validate_snapshot(&before).await,
            Err(DurableError::Conflict)
        ));
        let current = s.snapshot().await.unwrap();
        assert_eq!(s.validate_snapshot(&current).await.unwrap(), current);
        assert_eq!(entries(&h).await.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn whole_history_replacement_changes_snapshot_identity_without_changing_revision() {
    bounded(async {
        let store = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "generation-scope".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let h = harness(store.clone()).await;
        let s = service(&h);
        let baseline = s.snapshot().await.unwrap();
        let state = store.snapshot_state();
        let identity = store.history_identity().await.unwrap();
        let next = store
            .replace_history_if_revision_and_generation(
                identity.revision,
                identity.generation,
                HistoryReplacement {
                    state,
                    canonical_mutations: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(next.revision, identity.revision);
        assert_eq!(next.generation, identity.generation + 1);
        assert_eq!(
            store.history_identity().await.unwrap().generation,
            next.generation
        );
        assert_eq!(
            s.snapshot().await.unwrap().scope.history_generation,
            next.generation
        );
        assert!(matches!(
            s.validate_snapshot(&baseline).await,
            Err(DurableError::Conflict)
        ));
        let current = s.snapshot().await.unwrap();
        assert_eq!(
            current.scope.history_revision,
            baseline.scope.history_revision
        );
        assert_eq!(
            current.scope.history_generation,
            baseline.scope.history_generation + 1
        );
    })
    .await;
}

#[tokio::test]
async fn replacement_with_stale_control_scope_fails_closed() {
    bounded(async {
        let store = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "stale-control-generation".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let h = harness(store.clone()).await;
        let s = service(&h);
        apply(&s, "stale-control-start", start()).await;
        let state = store.snapshot_state();
        let identity = store.history_identity().await.unwrap();
        store
            .replace_history_if_revision_and_generation(
                identity.revision,
                identity.generation,
                HistoryReplacement {
                    state,
                    canonical_mutations: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            s.snapshot().await,
            Err(DurableError::InvalidJournal(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn jsonl_snapshot_validation_rejects_external_writer_and_reopen_staleness() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "snapshot-validation.jsonl",
            JsonlV4Header::new(
                "snapshot-validation".into(),
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
        let shared: Arc<dyn SessionStorage> = Arc::new(store);
        let first = harness(shared.clone()).await;
        assert_eq!(
            first
                .session()
                .storage()
                .history_generation()
                .await
                .unwrap(),
            0
        );
        let first_service = service(&first);
        let baseline = first_service.snapshot().await.unwrap();
        let second = harness(shared.clone()).await;
        apply(&service(&second), "external-start", start()).await;
        assert!(matches!(
            first_service.validate_snapshot(&baseline).await,
            Err(DurableError::Conflict)
        ));
        let current = first_service.snapshot().await.unwrap();
        first.close().await.unwrap();
        second.close().await.unwrap();
        first_service.drain().await.unwrap();
        shared.release_writer_lease();
        let loaded = JsonlSessionStorage::load(
            fs,
            "snapshot-validation.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = harness(Arc::new(loaded)).await;
        assert_eq!(
            reopened
                .session()
                .storage()
                .history_generation()
                .await
                .unwrap(),
            0
        );
        let reopened_service = service(&reopened);
        assert!(matches!(
            reopened_service.validate_snapshot(&baseline).await,
            Err(DurableError::Conflict)
        ));
        assert!(matches!(
            reopened_service.validate_snapshot(&current).await,
            Err(DurableError::Conflict)
        ));
        let reopened_current = reopened_service.snapshot().await.unwrap();
        assert_eq!(
            reopened_service
                .validate_snapshot(&reopened_current)
                .await
                .unwrap(),
            reopened_current
        );
        reopened.close().await.unwrap();
        reopened_service.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn seal_waits_for_an_admitted_mutation_before_returning_the_cut_point() {
    bounded(async {
        let (h, p) = probed().await;
        let s = service(&h);
        let mutation = request(&s, "seal-admitted", start()).await;
        p.mode.store(3, Ordering::SeqCst);
        let mut task = MutationTask::spawn(&s, mutation.clone(), &p);
        append_boundary(&p, 3).await;
        let mut seal = Box::pin(s.seal());
        assert!(futures::poll!(seal.as_mut()).is_pending());
        task.abort_caller().await;
        p.release.notify_one();
        let sealed = seal.await.unwrap();
        assert_eq!(sealed.state.workflow.unwrap().name, "workflow");
        assert_eq!(
            s.request_outcome("seal-admitted")
                .await
                .unwrap()
                .unwrap()
                .request_id,
            "seal-admitted"
        );
        assert!(matches!(
            s.mutate(mutation).await,
            Err(DurableError::Sealed)
        ));
    })
    .await;
}

#[tokio::test]
async fn seal_is_volatile_across_provider_free_reopen() {
    bounded(async {
        let h = memory().await;
        let store = h.session().storage();
        let s = service(&h);
        apply(&s, "volatile-start", start()).await;
        s.seal().await.unwrap();
        h.close().await.unwrap();
        s.drain().await.unwrap();
        let reopened = harness(store).await;
        let reopened_service = service(&reopened);
        let paused = apply(
            &reopened_service,
            "after-reopen",
            json!({"action":"pause", "reason":"reopened"}),
        )
        .await;
        assert_eq!(
            paused.snapshot.state.workflow.unwrap().status,
            WorkflowStatus::Paused
        );
        reopened.close().await.unwrap();
        reopened_service.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn seal_is_shared_by_independent_services_on_one_harness() {
    bounded(async {
        let h = memory().await;
        let first = service(&h);
        let second = DurableService::bind(&h, DurableActor::owner("second-owner").unwrap());
        let mutation = request(&second, "shared-seal", start()).await;
        let sealed = first.seal().await.unwrap();
        assert!(sealed.state.workflow.is_none());
        assert!(matches!(
            second.mutate(mutation).await,
            Err(DurableError::Sealed)
        ));
        assert!(second.snapshot().await.unwrap().state.workflow.is_none());
        second.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_waiting_seal_releases_only_its_gate_waiter() {
    bounded(async {
        let (h, p) = probed().await;
        let s = service(&h);
        let mutation = request(&s, "seal-cancelled", start()).await;
        p.mode.store(3, Ordering::SeqCst);
        let mut task = MutationTask::spawn(&s, mutation, &p);
        append_boundary(&p, 3).await;
        let mut seal = Box::pin(s.seal());
        assert!(futures::poll!(seal.as_mut()).is_pending());
        drop(seal);
        p.release.notify_one();
        let outcome = (&mut task.caller).await.unwrap().unwrap();
        assert_eq!(outcome.receipt.request_id, "seal-cancelled");
        let sealed = s.seal().await.unwrap();
        assert_eq!(sealed.state.workflow.unwrap().name, "workflow");
    })
    .await;
}

#[tokio::test]
async fn seal_settles_admitted_work_and_rejects_future_mutations() {
    bounded(async {
        let h = memory().await;
        let s = service(&h);
        apply(&s, "sealed-start", start()).await;
        let before = entries(&h).await;
        let next = request(
            &s,
            "sealed-next",
            json!({"action":"pause", "reason":"later"}),
        )
        .await;
        let child = DurableService::bind(&h, DurableActor::child("child-sealer").unwrap());
        assert!(matches!(
            child.seal().await,
            Err(DurableError::PermissionDenied)
        ));
        let sealed = s.seal().await.unwrap();
        assert_eq!(sealed, s.snapshot().await.unwrap());
        assert!(matches!(
            s.mutate(next.clone()).await,
            Err(DurableError::Sealed)
        ));
        assert!(matches!(s.seal().await, Err(DurableError::Sealed)));
        assert_eq!(entries(&h).await, before);
        assert_eq!(
            s.snapshot().await.unwrap().state.workflow.unwrap().name,
            "workflow"
        );
        assert!(s.reconcile(&next).await.unwrap().is_none());
        s.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_gate_waiting_drain_neither_cancels_mutation_nor_leaks_waiter() {
    bounded(async {
        let (h, p) = probed().await;
        let s = service(&h);
        let mutation = request(&s, "survives-cancelled-drain", start()).await;
        p.mode.store(3, Ordering::SeqCst);
        let mut task = MutationTask::spawn(&s, mutation, &p);
        append_boundary(&p, 3).await;
        let before = p.drain_calls.load(Ordering::SeqCst);
        let mut cancelled = Box::pin(s.drain());
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        let mut retry = Box::pin(s.drain());
        assert!(futures::poll!(retry.as_mut()).is_pending());
        let mut message =
            Box::pin(h.append_message(AgentMessage::User(UserMessage::new("after barrier", 0))));
        assert!(futures::poll!(message.as_mut()).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        p.release.notify_one();
        let (settled, message, caller) = tokio::join!(retry, message, &mut task.caller);
        settled.unwrap();
        message.unwrap();
        let outcome = caller.unwrap().unwrap();
        assert_eq!(
            s.request_outcome("survives-cancelled-drain").await.unwrap(),
            Some(outcome.receipt)
        );
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        s.drain().await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 2);
        assert_eq!(p.commits.load(Ordering::SeqCst), 1);
        assert_eq!(entries(&h).await.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn canonical_control_reopen_rejects_a_self_consistent_orphan_predecessor() {
    let h = memory().await;
    let s = service(&h);
    apply(&s, "start", start()).await;
    apply(&s, "pause", json!({"action":"pause", "reason":"waiting"})).await;
    let entries = entries(&h).await;
    assert_eq!(entries.len(), 2);
    let second = entries.iter().max_by_key(|entry| entry.seq()).unwrap();
    let Entry::Custom(second) = second else {
        panic!("expected canonical custom entry");
    };
    let mut payload = second.data.clone().expect("control payload");
    // Keep the second record internally coherent, but point it at an ID that
    // is not the first canonical entry. A newest-only reader would accept this
    // cumulative state; the canonical-chain reader must fail closed.
    payload["previousEntryId"] = json!("orphan-predecessor");
    payload["receipts"][0]["entryId"] = json!("orphan-predecessor");
    payload["receipts"][1]["expected"]["entryId"] = json!("orphan-predecessor");
    payload["receipts"][1]["entryId"] = json!("manual-canonical-entry");
    let revision = h.session().storage().history_revision().await.unwrap();
    h.session()
        .storage()
        .append_entry_if_revision(
            ProvisionedEntry {
                id: "manual-canonical-entry".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "rpi.durable.control.v1".into(),
                    data: Some(payload),
                },
            },
            "main",
            revision,
        )
        .await
        .unwrap();
    let result = s.snapshot().await;
    assert!(
        matches!(result, Err(DurableError::InvalidJournal(_))),
        "unexpected snapshot result: {result:?}"
    );
}

#[tokio::test]
async fn backend_drain_holds_gate_and_cancellation_releases_it_without_detached_drain() {
    bounded(async {
        let (h, p) = probed().await;
        let _release_on_drop = ReleaseBarriers(p.clone());
        let s = service(&h);
        let before = p.drain_calls.load(Ordering::SeqCst);
        p.pause_drains.store(true, Ordering::SeqCst);
        let mut drain = Box::pin(s.drain());
        assert!(futures::poll!(drain.as_mut()).is_pending());
        p.drain_entered.notified().await;
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        let mut message =
            Box::pin(h.append_message(AgentMessage::User(UserMessage::new("queued message", 0))));
        assert!(futures::poll!(message.as_mut()).is_pending());
        // No release_drain notification: dropping this future must drop its gate
        // directly, not leave an owned backend-drain worker holding it forever.
        drop(drain);
        bounded(message).await.unwrap();
        s.drain().await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 2);
        assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
        assert!(entries(&h).await.is_empty());
        // Cancellation makes no assertion about rollback of backend effects.
    })
    .await;
}

#[tokio::test]
async fn drain_needs_only_a_live_owner_not_open_state_readable_journal_or_receipts() {
    bounded(async {
        for custom_type in [
            "durable-state",
            "rpi-durable-runtime",
            "rpi.durable.control.v1",
        ] {
            let (h, p) = probed().await;
            h.session()
                .append_custom_entry(custom_type, Some(json!("malformed")))
                .await
                .unwrap();
            let s = service(&h);
            assert!(matches!(
                s.snapshot().await,
                Err(DurableError::UnsupportedJournalFormat | DurableError::InvalidJournal(_))
            ));
            let before_entries =
                serde_json::to_value(p.inner.find_entries(&EntryQuery::default()).await.unwrap())
                    .unwrap();
            let before_revision = p.inner.history_revision().await.unwrap();
            p.fail_reads.store(true, Ordering::SeqCst);
            p.fail_revision.store(true, Ordering::SeqCst);
            let before = p.drain_calls.load(Ordering::SeqCst);
            s.drain().await.unwrap();
            h.close().await.unwrap();
            s.drain().await.unwrap();
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 2);
            assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
            assert_eq!(p.inner.history_revision().await.unwrap(), before_revision);
            assert_eq!(
                serde_json::to_value(p.inner.find_entries(&EntryQuery::default()).await.unwrap())
                    .unwrap(),
                before_entries
            );
            drop(h);
            assert!(matches!(s.drain().await, Err(DurableError::Closed)));
            assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 2);
        }
        let (h, p) = probed().await;
        let s = service(&h);
        h.close().await.unwrap();
        s.drain().await.unwrap();
        assert!(entries(&h).await.is_empty());
        let before = p.drain_calls.load(Ordering::SeqCst);
        drop(h);
        assert!(matches!(s.drain().await, Err(DurableError::Closed)));
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    })
    .await;
}

#[tokio::test]
async fn memory_close_and_drain_preserve_exact_workflow_manual_receipts_and_native_audit() {
    bounded(async {
        let h = memory().await;
        append_native_audit(&h).await;
        let s = service(&h);
        let workflow = apply(&s, "workflow-start", start()).await;
        let mutation = request(
            &s,
            "manual-enqueue",
            json!({"action":"enqueue", "text":"manual item"}),
        )
        .await;
        let manual = s.mutate(mutation.clone()).await.unwrap();
        let store = h.session().storage();
        let control_before = serde_json::to_value(entries(&h).await).unwrap();
        let audit_before = store.find_records(&RecordQuery::default()).await.unwrap();
        assert_eq!(audit_before.len(), 2);
        h.close().await.unwrap();
        s.drain().await.unwrap();
        drop(h);
        let reopened = harness(store.clone()).await;
        let reader = service(&reopened);
        assert_eq!(
            reader.snapshot().await.unwrap().state,
            manual.snapshot.state
        );
        assert_eq!(
            reader.request_outcome("workflow-start").await.unwrap(),
            Some(workflow.receipt)
        );
        assert_eq!(
            reader.request_outcome("manual-enqueue").await.unwrap(),
            Some(manual.receipt.clone())
        );
        let replay = reader.mutate(mutation).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, manual.receipt);
        reader.drain().await.unwrap();
        assert_eq!(
            serde_json::to_value(entries(&reopened).await).unwrap(),
            control_before
        );
        assert_eq!(
            serde_json::to_value(store.find_records(&RecordQuery::default()).await.unwrap())
                .unwrap(),
            serde_json::to_value(audit_before).unwrap()
        );
    })
    .await;
}

#[tokio::test]
async fn jsonl_real_commit_survives_aborted_caller_close_drain_and_provider_free_reopen() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "settlement.jsonl",
            JsonlV4Header::new("settlement".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(store));
        let h = harness(p.clone()).await;
        append_native_audit(&h).await;
        let s = service(&h);
        let workflow = apply(&s, "workflow-start", start()).await;
        // Consume the earlier success notification before using the real second
        // commit as our mode-4 barrier (Notify retains one unused permit).
        p.committed.notified().await;
        let mutation = request(
            &s,
            "jsonl-manual",
            json!({"action":"enqueue", "text":"persisted manual item"}),
        )
        .await;
        p.mode.store(4, Ordering::SeqCst);
        let mut task = MutationTask::spawn(&s, mutation.clone(), &p);
        append_boundary(&p, 4).await;
        assert_eq!(p.commits.load(Ordering::SeqCst), 2);
        task.abort_caller().await;
        h.close().await.unwrap();
        let before = p.drain_calls.load(Ordering::SeqCst);
        let mut drain = Box::pin(s.drain());
        assert!(futures::poll!(drain.as_mut()).is_pending());
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
        p.release.notify_one();
        drain.await.unwrap();
        assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
        let control = entries(&h).await;
        assert_eq!(control.len(), 2);
        let latest = control.iter().max_by_key(|e| e.seq()).unwrap();
        let Entry::Custom(latest) = latest else {
            panic!("control entry required")
        };
        let payload = latest.data.as_ref().unwrap();
        let receipt: rpi_harness::durable::DurableReceipt =
            serde_json::from_value(payload["receipts"][1].clone()).unwrap();
        let expected_state: rpi_durable::control::ControlState =
            serde_json::from_value(payload["state"].clone()).unwrap();
        assert_eq!(expected_state.workflow.as_ref().unwrap().name, "workflow");
        assert_eq!(expected_state.queue.len(), 1);
        assert_eq!(expected_state.queue[0].text, "persisted manual item");
        assert_eq!(receipt.request_id, mutation.request_id);
        assert_eq!(receipt.expected, mutation.expected);
        assert_eq!(receipt.action, mutation.action);
        assert_eq!(receipt.entry_id, latest.base.id);
        let audit_before = p.inner.find_records(&RecordQuery::default()).await.unwrap();
        assert_eq!(audit_before.len(), 2);
        let bytes = std::fs::read(dir.path().join("settlement.jsonl")).unwrap();
        drop(task);
        drop(h);
        p.inner.release_writer_lease();
        drop(p);
        let loaded = JsonlSessionStorage::load(
            fs,
            "settlement.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = harness(Arc::new(loaded)).await;
        let reader = service(&reopened);
        assert_eq!(reader.snapshot().await.unwrap().state, expected_state);
        assert_eq!(
            reader.request_outcome("workflow-start").await.unwrap(),
            Some(workflow.receipt)
        );
        assert_eq!(
            reader.request_outcome("jsonl-manual").await.unwrap(),
            Some(receipt.clone())
        );
        let reconciled = reader.reconcile(&mutation).await.unwrap().unwrap();
        assert!(reconciled.replayed);
        assert_eq!(reconciled.receipt, receipt);
        let replay = reader.mutate(mutation).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, receipt);
        reopened.close().await.unwrap();
        reader.drain().await.unwrap();
        assert_eq!(
            serde_json::to_value(entries(&reopened).await).unwrap(),
            serde_json::to_value(control).unwrap()
        );
        assert_eq!(
            serde_json::to_value(
                reopened
                    .session()
                    .storage()
                    .find_records(&RecordQuery::default())
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(audit_before).unwrap()
        );
        assert_eq!(
            std::fs::read(dir.path().join("settlement.jsonl")).unwrap(),
            bytes
        );
    })
    .await;
}

#[tokio::test]
async fn lost_ack_reopen_reconciles_the_canonical_fact_without_reappend() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "uncertain.jsonl",
            JsonlV4Header::new("uncertain".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let probe = Probe::new(Arc::new(store));
        let h = harness(probe.clone()).await;
        let owner = service(&h);
        let mutation = request(&owner, "reopen-lost-ack", start()).await;

        // The append reaches the canonical JSONL journal, but its acknowledgement
        // is lost. The caller must not manufacture a failed fact or append again.
        probe.mode.store(2, Ordering::SeqCst);
        assert!(matches!(
            owner.mutate(mutation.clone()).await,
            Err(DurableError::CommitUncertain(_))
        ));
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(probe.commits.load(Ordering::SeqCst), 1);
        let before = entries(&h).await;
        assert_eq!(before.len(), 1);
        let before_bytes = std::fs::read(dir.path().join("uncertain.jsonl")).unwrap();
        let receipt = owner
            .request_outcome("reopen-lost-ack")
            .await
            .unwrap()
            .expect("the canonical journal owns the committed outcome");

        drop(h);
        probe.inner.release_writer_lease();
        drop(probe);
        let loaded = JsonlSessionStorage::load(
            fs,
            "uncertain.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = harness(Arc::new(loaded)).await;
        let reader = service(&reopened);

        let reconciled = reader
            .reconcile(&mutation)
            .await
            .unwrap()
            .expect("reopen must find the same canonical request identity");
        assert!(reconciled.replayed);
        assert_eq!(reconciled.receipt, receipt);
        let replay = reader.mutate(mutation).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, receipt);
        assert_eq!(entries(&reopened).await, before);
        assert_eq!(
            std::fs::read(dir.path().join("uncertain.jsonl")).unwrap(),
            before_bytes
        );
        reader.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn drain_pins_original_backend_and_blocks_same_harness_session_switch_only() {
    bounded(async {
        let (h, original) = probed().await;
        let _release_on_drop = ReleaseBarriers(original.clone());
        let replacement = Probe::new(AgentHarnessOptions::default().session.storage());
        let s = service(&h);
        let old_before = original.drain_calls.load(Ordering::SeqCst);
        let new_before = replacement.drain_calls.load(Ordering::SeqCst);
        original.pause_drains.store(true, Ordering::SeqCst);
        let mut drain = Box::pin(s.drain());
        assert!(futures::poll!(drain.as_mut()).is_pending());
        original.drain_entered.notified().await;
        let mut switch = Box::pin(h.set_session(Session::new(replacement.clone(), None)));
        assert!(futures::poll!(switch.as_mut()).is_pending());
        assert_eq!(h.session_id(), original.inner.metadata().id);
        // Direct facade rebinds are explicitly outside the harness gate fence.
        // Even one of these cannot redirect the already-pinned backend drain.
        h.session().set_storage(replacement.clone());
        assert!(futures::poll!(drain.as_mut()).is_pending());
        assert_eq!(original.drain_calls.load(Ordering::SeqCst), old_before + 1);
        assert_eq!(replacement.drain_calls.load(Ordering::SeqCst), new_before);
        original.release_drain.notify_one();
        let (settled, switched) = tokio::join!(drain, switch);
        settled.unwrap();
        switched.unwrap();
        assert_eq!(h.session_id(), replacement.inner.metadata().id);
        s.drain().await.unwrap();
        assert_eq!(original.drain_calls.load(Ordering::SeqCst), old_before + 1);
        assert_eq!(
            replacement.drain_calls.load(Ordering::SeqCst),
            new_before + 1
        );
        assert_eq!(original.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(replacement.attempts.load(Ordering::SeqCst), 0);
        // No claim that this fences independent harnesses, future gate waiters,
        // whole runs, or mutations not yet queued at the message-write gate.
    })
    .await;
}
