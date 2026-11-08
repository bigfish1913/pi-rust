//! Opt-in backend-response faults; no production adapter or journal is changed.
use super::*;

async fn starts(p: &Probe) -> Vec<LaneRecord> {
    p.inner
        .find_records(&RecordQuery {
            lane: Some("main".into()),
            record_type: Some("operation_started"),
            order: Some(EntryOrder::NewestFirst),
            limit: Some(65),
            ..Default::default()
        })
        .await
        .unwrap()
}
fn started(record: &mut LaneRecord) -> &mut OperationStartedRecord {
    match record {
        LaneRecord::OperationStarted(started) => started,
        _ => panic!("fixture expected native start"),
    }
}
async fn assert_unchanged(
    p: &Probe,
    h: &AgentHarness,
    revision: u64,
    native: &[LaneRecord],
    control: &[Entry],
) {
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    assert_eq!(facts(h).await, native);
    assert_eq!(controls(h).await, control);
    assert_eq!(p.attempts.load(Ordering::SeqCst), control.len());
    assert_eq!(p.commits.load(Ordering::SeqCst), control.len());
}

#[tokio::test]
async fn malformed_scan_shapes_never_return_partial_or_empty_success() {
    bounded(async {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        interrupted(&h, "adversarial-old").await;
        interrupted(&h, "adversarial-new").await;
        let s = service(&h);
        let valid = s.execution_index().await.unwrap();
        assert_eq!(valid.runs.len(), 2);
        let native = facts(&h).await;
        let control = controls(&h).await;
        let revision = p.inner.history_revision().await.unwrap();
        let original = starts(&p).await;
        assert_eq!(original.len(), 2);
        for fault in [
            "variant",
            "lane",
            "zero",
            "future",
            "ascending",
            "equal-seq",
        ] {
            let mut response = original.clone();
            // Keep one valid first candidate where possible: a later invalid
            // record must invalidate the whole result, not expose a partial row.
            match fault {
                "variant" => {
                    response[1] = LaneRecord::AbortRequested(AbortRequestedRecord {
                        base: base("adversarial-wrong-kind"),
                        run_id: "adversarial-old".into(),
                    })
                }
                "lane" => started(&mut response[1]).base.lane = "foreign-lane".into(),
                "zero" => started(&mut response[1]).base.seq = 0,
                "future" => started(&mut response[1]).base.seq = revision + 1,
                "ascending" => response.reverse(),
                "equal-seq" => {
                    let seq = started(&mut response[0]).base.seq;
                    started(&mut response[1]).base.seq = seq;
                }
                _ => unreachable!(),
            }
            *p.index_scan_override.lock().unwrap() = Some(response);
            assert!(
                matches!(
                    s.execution_index().await,
                    Err(DurableError::InvalidJournal(_))
                ),
                "{fault}"
            );
            assert!(p.index_scan_override.lock().unwrap().is_none());
            assert_eq!(
                s.execution_index().await.unwrap(),
                valid,
                "fresh read after {fault}"
            );
            assert_unchanged(&p, &h, revision, &native, &control).await;
        }
    })
    .await;
}

#[tokio::test]
async fn well_ordered_scan_sequence_must_match_the_exact_native_proof() {
    bounded(async {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        interrupted(&h, "adversarial-proof-old").await;
        interrupted(&h, "adversarial-proof-new").await;
        let s = service(&h);
        let valid = s.execution_index().await.unwrap();
        let original = starts(&p).await;
        let native = facts(&h).await;
        let control = controls(&h).await;
        let revision = p.inner.history_revision().await.unwrap();
        let mut swapped = original.clone();
        let id = started(&mut swapped[0]).base.id.clone();
        started(&mut swapped[0]).base.id = started(&mut swapped[1]).base.id.clone();
        started(&mut swapped[1]).base.id = id;
        // Shape/sequence checks alone accept this descending scan, and both
        // explicit ID proofs individually succeed. But those proofs refer to
        // the opposite native start sequences: returning them would lie about
        // the index's descending order and its scan/proof consistency.
        let mut duplicate = original.clone();
        started(&mut duplicate[1]).base.id = started(&mut duplicate[0]).base.id.clone();
        for response in [swapped, duplicate] {
            *p.index_scan_override.lock().unwrap() = Some(response);
            assert!(
                matches!(
                    s.execution_index().await,
                    Err(DurableError::InvalidJournal(_))
                ),
                "individually valid run proofs must not certify mismatched scan identity/sequence"
            );
            assert_eq!(s.execution_index().await.unwrap(), valid);
            assert_unchanged(&p, &h, revision, &native, &control).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn faults_after_actual_scan_reach_outer_leaf_and_revision_checks_on_early_paths() {
    bounded(async {
        for state in [
            "empty",
            "unavailable",
            "forgotten",
            "invalid",
            "over-budget",
        ] {
            for leaf_fault in [false, true] {
                let p = Probe::new(AgentHarnessOptions::default().session.storage());
                let h = harness(p.clone()).await;
                let s = service(&h);
                match state {
                    "unavailable" => start(&h, "adversarial-unanchored").await,
                    "forgotten" => {
                        interrupted(&h, "adversarial-forgotten").await;
                        s.mutate(
                            request(
                                &s,
                                "adversarial-forget",
                                ControlAction::ForgetRun,
                                Some("adversarial-forgotten"),
                            )
                            .await,
                        )
                        .await
                        .unwrap();
                    }
                    "invalid" => {
                        // One-shot response corruption, not an actual journal
                        // write. There is no successful per-run proof here.
                        *p.index_scan_override.lock().unwrap() =
                            Some(vec![LaneRecord::AbortRequested(AbortRequestedRecord {
                                base: base("adversarial-invalid-variant"),
                                run_id: "not-a-branch-run".into(),
                            })]);
                    }
                    "over-budget" => {
                        for i in 0..=MAX_EXECUTION_INDEX_STARTS {
                            compaction(&h, &format!("adversarial-over-{i}")).await;
                        }
                    }
                    _ => {}
                }
                let native = facts(&h).await;
                let control = controls(&h).await;
                let revision = p.inner.history_revision().await.unwrap();
                p.pause_index_scans.store(true, Ordering::SeqCst);
                let reader = tokio::spawn({
                    let s = s.clone();
                    async move { s.execution_index().await }
                });
                p.index_scan_captured.notified().await;
                // durable_load and the actual bounded query already succeeded.
                // Only now enable a fault. Unavailable has nil leaf and fails
                // proof before a success fence; all other cases skip proof.
                p.fail_lanes.store(leaf_fault, Ordering::SeqCst);
                p.fail_revision.store(!leaf_fault, Ordering::SeqCst);
                p.release_index_scan.notify_one();
                let error = reader.await.unwrap().unwrap_err();
                let DurableError::ReadFailure(message) = error else {
                    panic!("final {state}/{leaf_fault} did not preserve ReadFailure: {error:?}");
                };
                assert!(message.contains(if leaf_fault {
                    "lane pointer read"
                } else {
                    "revision failure"
                }));
                p.fail_lanes.store(false, Ordering::SeqCst);
                p.fail_revision.store(false, Ordering::SeqCst);
                let fresh = s.execution_index().await;
                if state == "over-budget" {
                    assert!(matches!(fresh, Err(DurableError::LimitExceeded)));
                } else {
                    let fresh = fresh.unwrap();
                    assert!(fresh.runs.is_empty());
                    assert_eq!(fresh.unavailable_runs, usize::from(state == "unavailable"));
                }
                assert_unchanged(&p, &h, revision, &native, &control).await;
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_captured_malformed_response_does_not_spawn_a_late_reader() {
    bounded(async {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        let s = service(&h);
        let native = facts(&h).await;
        let control = controls(&h).await;
        let revision = p.inner.history_revision().await.unwrap();
        p.pause_index_scans.store(true, Ordering::SeqCst);
        let reader = tokio::spawn({
            let s = s.clone();
            async move { s.execution_index().await }
        });
        p.index_scan_captured.notified().await;
        *p.index_scan_override.lock().unwrap() =
            Some(vec![LaneRecord::AbortRequested(AbortRequestedRecord {
                base: base("adversarial-cancel-response"),
                run_id: "not-a-run".into(),
            })]);
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        // The cancelled reader never consumes the one-shot response. The next
        // fresh caller must see/reject it; a third normal read then succeeds.
        assert!(p.index_scan_override.lock().unwrap().is_some());
        p.release_index_scan.notify_one();
        assert!(matches!(
            s.execution_index().await,
            Err(DurableError::InvalidJournal(_))
        ));
        let fresh = s.execution_index().await.unwrap();
        assert!(fresh.runs.is_empty());
        assert_eq!(fresh.unavailable_runs, 0);
        assert_unchanged(&p, &h, revision, &native, &control).await;
    })
    .await;
}
