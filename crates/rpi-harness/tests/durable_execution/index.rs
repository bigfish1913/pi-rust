//! Read-only, branch-proven execution discovery; no provider credentials required.
//! Parent adds captured-index-scan race/cancellation cases to the fixture tests.
#[path = "index/adversarial.rs"]
mod adversarial;
use super::*;
use rpi_harness::durable::MAX_EXECUTION_INDEX_STARTS;

async fn compaction(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
            source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
            intent: OperationIntent::Compaction {
                custom_instructions: Some("private compaction instructions".into()),
                result_entry_id: format!("{id}-result"),
            },
        }))
        .await
        .unwrap();
    // Respect the native one-open-operation-per-lane storage contract.
    finish(h, id, OperationOutcome::Completed, None, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_real_faux_index_matches_explicit_metadata_and_preserves_audit_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = FauxProvider::new(FauxScript::new().with_text("private index answer"));
    let h = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        session: Session::new(
            Arc::new(
                JsonlSessionStorage::create(
                    Arc::new(OsExecutionEnv::with_cwd(tmp.path().into())),
                    "index.jsonl",
                    JsonlV4Header::new("index-faux".into(), 1, "/".into(), None, None, None),
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            ),
            None,
        ),
        ..Default::default()
    })
    .await
    .unwrap();
    let returned = bounded(h.prompt_text("private index prompt", vec![]))
        .await
        .unwrap();
    prompt_anchor(&h, "index-newer-open").await;
    let s = service(&h);
    h.session().storage().drain().await;
    let path = tmp.path().join("index.jsonl");
    let bytes = std::fs::read(&path).unwrap();
    let revision = h.session().storage().history_revision().await.unwrap();
    let native = facts(&h).await;
    let receipts = controls(&h).await;
    let index = bounded(s.execution_index()).await.unwrap();
    let snapshot = s.snapshot().await.unwrap();
    assert_eq!(index.scope, snapshot.scope);
    assert_eq!(index.control, snapshot.execution_control);
    assert_eq!(index.unavailable_runs, 0);
    assert_eq!(index.runs.len(), 2);
    assert_eq!(index.runs[0].id, "index-newer-open");
    assert_eq!(index.runs[0].status, RunStatus::Open);
    assert_eq!(index.runs[1].id, returned.run_id);
    assert_eq!(index.runs[1].status, RunStatus::Completed);
    assert!(index.runs[0].started_seq > index.runs[1].started_seq);
    for summary in &index.runs {
        let explicit = s.execution(&summary.id).await.unwrap().run.unwrap();
        assert_eq!(summary.started_seq, explicit.started_seq);
        assert_eq!(summary.finished_record_id, explicit.finished_record_id);
        assert_eq!(summary.finished_seq, explicit.finished_seq);
        assert_eq!(summary.status, explicit.status);
    }
    assert!(!serde_json::to_string(&index).unwrap().contains("private"));
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert_eq!(facts(&h).await, native);
    assert_eq!(controls(&h).await, receipts);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let audit = h.get_result(&returned.run_id).await.unwrap().unwrap();
    s.mutate(
        request(
            &s,
            "index-forget-real",
            ControlAction::ForgetRun,
            Some(&returned.run_id),
        )
        .await,
    )
    .await
    .unwrap();
    h.session().storage().drain().await;
    let forgotten_bytes = std::fs::read(&path).unwrap();
    let forgotten_revision = h.session().storage().history_revision().await.unwrap();
    let index = s.execution_index().await.unwrap();
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].id, "index-newer-open");
    assert_eq!(
        index.control.forgotten_run_ids,
        vec![returned.run_id.clone()]
    );
    assert_eq!(index.unavailable_runs, 0);
    assert_eq!(facts(&h).await, native);
    assert_eq!(controls(&h).await.len(), receipts.len() + 1);
    assert_eq!(std::fs::read(&path).unwrap(), forgotten_bytes);
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        forgotten_revision
    );
    let after = h.get_result(&returned.run_id).await.unwrap().unwrap();
    match (&audit.outcome, &after.outcome) {
        (
            HarnessRunOutcome::Completed {
                leaf_id: a,
                final_entry_id: b,
                final_message: c,
            },
            HarnessRunOutcome::Completed {
                leaf_id: x,
                final_entry_id: y,
                final_message: z,
            },
        ) => assert_eq!((a, b, c), (x, y, z)),
        _ => panic!("forget changed completed native result"),
    }
}

#[tokio::test]
async fn selected_branch_excludes_shared_source_sibling_and_unanchored_ids() {
    let h = memory().await;
    append(
        &h,
        "index-shared-source",
        AgentMessage::User(UserMessage::new("private shared", 0)),
    )
    .await;
    interrupted(&h, "index-sibling").await;
    h.session()
        .move_lane("main", Some("index-shared-source"))
        .await
        .unwrap();
    start(&h, "index-no-anchor").await;
    // A shared source is not proof of admission; break start/prompt adjacency.
    h.session()
        .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
            base: base("index-admission-gap"),
            run_id: "index-no-anchor".into(),
        }))
        .await
        .unwrap();
    finish(
        &h,
        "index-no-anchor",
        OperationOutcome::Aborted,
        None,
        false,
    )
    .await;
    compaction(&h, "index-compaction").await;
    prompt_anchor(&h, "index-selected-open").await;
    tool(&h, "index-selected-open", 0, ToolReplay::Never).await;
    h.session()
        .storage()
        .create_lane("index-other-lane", None)
        .await
        .unwrap();
    let mut other_base = base("index-other-lane-run");
    other_base.lane = "index-other-lane".into();
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: other_base,
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let revision = h.session().storage().history_revision().await.unwrap();
    let native = facts(&h).await;
    let s = service(&h);
    let index = s.execution_index().await.unwrap();
    assert_eq!(index.unavailable_runs, 2);
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].id, "index-selected-open");
    assert_eq!(index.runs[0].status, RunStatus::Open);
    assert!(index.runs[0].finished_record_id.is_none());
    assert!(index.runs[0].finished_seq.is_none());
    let explicit = s
        .execution("index-selected-open")
        .await
        .unwrap()
        .run
        .unwrap();
    assert_eq!(index.runs[0].started_seq, explicit.started_seq);
    assert_eq!(explicit.tools.len(), 1);
    let value = serde_json::to_value(&index).unwrap();
    let summary = value["runs"][0].as_object().unwrap();
    let mut keys: Vec<_> = summary.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "finishedRecordId",
            "finishedSeq",
            "id",
            "startedSeq",
            "status"
        ]
    );
    let text = value.to_string();
    for hidden in [
        "private",
        "index-sibling",
        "index-no-anchor",
        "index-other-lane-run",
        "effectiveArgs",
        "tools",
    ] {
        assert!(!text.contains(hidden), "index exposed {hidden}");
    }
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert_eq!(facts(&h).await, native);
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn recover_then_forget_updates_index_control_without_extra_receipts() {
    let h = memory().await;
    interrupted(&h, "index-recoverable").await;
    let s = service(&h);
    let native = facts(&h).await;
    let recovered = s
        .mutate(
            request(
                &s,
                "index-recover",
                ControlAction::Recover,
                Some("index-recoverable"),
            )
            .await,
        )
        .await
        .unwrap();
    let index = s.execution_index().await.unwrap();
    assert_eq!(index.scope, recovered.snapshot.scope);
    assert_eq!(index.control, recovered.snapshot.execution_control);
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].status, RunStatus::Interrupted);
    assert_eq!(
        index.control.recovery_request.as_ref().unwrap().run_id,
        "index-recoverable"
    );
    assert_eq!(controls(&h).await.len(), 1);
    let forgotten = s
        .mutate(
            request(
                &s,
                "index-forget",
                ControlAction::ForgetRun,
                Some("index-recoverable"),
            )
            .await,
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let index = s.execution_index().await.unwrap();
        assert_eq!(index.scope, forgotten.snapshot.scope);
        assert_eq!(index.control, forgotten.snapshot.execution_control);
        assert!(index.control.recovery_request.is_none());
        assert_eq!(index.control.forgotten_run_ids, vec!["index-recoverable"]);
        assert!(index.runs.is_empty());
        assert_eq!(index.unavailable_runs, 0);
    }
    assert_eq!(facts(&h).await, native);
    assert_eq!(controls(&h).await.len(), 2);
    assert!(s.request_outcome("index-recover").await.unwrap().is_some());
    assert!(s.request_outcome("index-forget").await.unwrap().is_some());
}

#[tokio::test]
async fn lane_start_budget_counts_forgotten_and_non_run_candidates_without_truncation() {
    assert_eq!(MAX_EXECUTION_INDEX_STARTS, 64);
    let h = memory().await;
    interrupted(&h, "index-budget-forgotten").await;
    let s = service(&h);
    s.mutate(
        request(
            &s,
            "index-budget-forget",
            ControlAction::ForgetRun,
            Some("index-budget-forgotten"),
        )
        .await,
    )
    .await
    .unwrap();
    for i in 0..MAX_EXECUTION_INDEX_STARTS - 2 {
        compaction(&h, &format!("index-budget-compaction-{i}")).await;
    }
    prompt_anchor(&h, "index-budget-visible").await;
    let revision = h.session().storage().history_revision().await.unwrap();
    let index = s.execution_index().await.unwrap();
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].id, "index-budget-visible");
    assert_eq!(index.unavailable_runs, 0);
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    finish(
        &h,
        "index-budget-visible",
        OperationOutcome::Completed,
        None,
        true,
    )
    .await;
    compaction(&h, "index-budget-overflow").await;
    let native = facts(&h).await;
    let revision = h.session().storage().history_revision().await.unwrap();
    assert!(matches!(
        s.execution_index().await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(facts(&h).await, native);
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert_eq!(controls(&h).await.len(), 1);
    // Discovery's bound does not change the existing explicit-run API.
    assert_eq!(
        s.execution("index-budget-visible")
            .await
            .unwrap()
            .run
            .unwrap()
            .status,
        RunStatus::Completed
    );
}

#[tokio::test]
async fn empty_and_unavailable_are_distinct_from_invalid_journal_and_read_failure() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    let s = service(&h);
    let empty = s.execution_index().await.unwrap();
    assert!(empty.scope.leaf_id.is_none());
    assert!(empty.runs.is_empty());
    assert_eq!(empty.unavailable_runs, 0);
    start(&h, "index-unanchored-only").await;
    let unavailable = s.execution_index().await.unwrap();
    assert!(unavailable.runs.is_empty());
    assert_eq!(unavailable.unavailable_runs, 1);
    finish(
        &h,
        "index-unanchored-only",
        OperationOutcome::Aborted,
        None,
        false,
    )
    .await;
    interrupted(&h, "index-corrupt-terminal").await;
    // Use a distinct physical record ID; duplicate IDs are rejected by storage.
    // Two terminals for one run must be refused by discovery, not hidden.
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("index-corrupt-terminal-second-finish"),
            run_id: "index-corrupt-terminal".into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
    let native = facts(&h).await;
    let revision = h.session().storage().history_revision().await.unwrap();
    assert!(matches!(
        s.execution_index().await,
        Err(DurableError::InvalidJournal(_))
    ));
    for revision_failure in [false, true] {
        p.fail_reads.store(!revision_failure, Ordering::SeqCst);
        p.fail_revision.store(revision_failure, Ordering::SeqCst);
        assert!(matches!(
            s.execution_index().await,
            Err(DurableError::ReadFailure(_))
        ));
    }
    p.fail_reads.store(false, Ordering::SeqCst);
    p.fail_revision.store(false, Ordering::SeqCst);
    assert_eq!(facts(&h).await, native);
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
    drop(h);
    assert!(matches!(
        s.execution_index().await,
        Err(DurableError::Closed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_control_load_releases_index_gate_without_writes() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "index-cancel-read").await;
    let s = service(&h);
    let native = facts(&h).await;
    let revision = p.inner.history_revision().await.unwrap();
    p.pause_branch_reads.store(true, Ordering::SeqCst);
    let reader = tokio::spawn({
        let s = s.clone();
        async move { s.execution_index().await }
    });
    // Probe pauses the control-journal branch read inside durable_load, NOT a final scan.
    bounded(p.branch_read_entered.notified()).await;
    reader.abort();
    assert!(bounded(reader).await.unwrap_err().is_cancelled());
    let index = bounded(s.execution_index()).await.unwrap();
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].id, "index-cancel-read");
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    assert_eq!(facts(&h).await, native);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebind_during_control_load_conflicts_even_with_empty_run_history() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    // A non-run entry makes durable_load perform its control branch query.
    append(
        &h,
        "index-rebind-anchor",
        AgentMessage::User(UserMessage::new("private anchor", 0)),
    )
    .await;
    let replacement = memory().await;
    let s = service(&h);
    let revision = p.inner.history_revision().await.unwrap();
    p.pause_branch_reads.store(true, Ordering::SeqCst);
    let reader = tokio::spawn({
        let s = s.clone();
        async move { s.execution_index().await }
    });
    bounded(p.branch_read_entered.notified()).await;
    h.session().set_storage(replacement.session().storage());
    p.release_branch_read.notify_one();
    assert!(matches!(
        bounded(reader).await.unwrap(),
        Err(DurableError::Conflict)
    ));
    assert!(bounded(s.execution_index()).await.unwrap().runs.is_empty());
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
    assert!(controls(&replacement).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_scan_rechecks_revision_even_when_no_run_proof_reaches_its_fence() {
    for kind in ["empty", "unavailable", "forgotten", "over-budget"] {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        let s = service(&h);
        match kind {
            "unavailable" => start(&h, "index-scan-unanchored").await,
            "forgotten" => {
                interrupted(&h, "index-scan-forgotten").await;
                s.mutate(
                    request(
                        &s,
                        "index-scan-forget",
                        ControlAction::ForgetRun,
                        Some("index-scan-forgotten"),
                    )
                    .await,
                )
                .await
                .unwrap();
            }
            "over-budget" => {
                for i in 0..=MAX_EXECUTION_INDEX_STARTS {
                    compaction(&h, &format!("index-scan-over-{i}")).await;
                }
            }
            _ => {}
        }
        let before = facts(&h).await;
        let revision = p.inner.history_revision().await.unwrap();
        let control_count = controls(&h).await.len();
        p.pause_index_scans.store(true, Ordering::SeqCst);
        let reader = tokio::spawn({
            let s = s.clone();
            async move { s.execution_index().await }
        });
        bounded(p.index_scan_captured.notified()).await;
        // Change only native history after the exact scan is captured. No new
        // branch admission can accidentally turn an unavailable run into proof.
        p.inner
            .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                base: base("index-scan-observed-change"),
                run_id: "index-foreign-driver".into(),
            }))
            .await
            .unwrap();
        let changed_revision = p.inner.history_revision().await.unwrap();
        assert!(changed_revision > revision);
        p.release_index_scan.notify_one();
        assert!(
            matches!(bounded(reader).await.unwrap(), Err(DurableError::Conflict)),
            "{kind}"
        );
        assert_eq!(p.inner.history_revision().await.unwrap(), changed_revision);
        assert_eq!(facts(&h).await.len(), before.len() + 1);
        assert_eq!(controls(&h).await.len(), control_count);
        assert_eq!(p.attempts.load(Ordering::SeqCst), control_count);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_empty_scan_checks_binding_and_cancellation_releases_gate() {
    for rebind in [false, true] {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        let replacement = memory().await;
        let s = service(&h);
        let revision = p.inner.history_revision().await.unwrap();
        p.pause_index_scans.store(true, Ordering::SeqCst);
        let reader = tokio::spawn({
            let s = s.clone();
            async move { s.execution_index().await }
        });
        bounded(p.index_scan_captured.notified()).await;
        if rebind {
            h.session().set_storage(replacement.session().storage());
            p.release_index_scan.notify_one();
            assert!(matches!(
                bounded(reader).await.unwrap(),
                Err(DurableError::Conflict)
            ));
        } else {
            reader.abort();
            assert!(bounded(reader).await.unwrap_err().is_cancelled());
        }
        let fresh = bounded(s.execution_index()).await.unwrap();
        assert!(fresh.runs.is_empty());
        assert_eq!(fresh.unavailable_runs, 0);
        assert_eq!(p.inner.history_revision().await.unwrap(), revision);
        assert!(facts(&h).await.is_empty());
        assert!(controls(&h).await.is_empty());
        assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    }
    let h = memory().await;
    let s = service(&h);
    h.close().await.unwrap();
    assert!(matches!(
        s.execution_index().await,
        Err(DurableError::Closed)
    ));
}
