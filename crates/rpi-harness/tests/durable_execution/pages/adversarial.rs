//! Captured actual page responses and opt-in faults; synthetic storage only.
use super::*;

async fn response(p: &Probe) -> RecordPage {
    p.inner
        .record_page(&RecordPageQuery {
            lane: "main".into(),
            after_seq: None,
            before_seq: None,
            limit: 65,
            from_end: true,
        })
        .await
        .unwrap()
}
fn copy_page(page: &RecordPage) -> RecordPage {
    RecordPage {
        records: page.records.clone(),
        total: page.total,
    }
}
fn change_seq(record: &mut LaneRecord, seq: u64) {
    match record {
        LaneRecord::OperationStarted(started) => started.base.seq = seq,
        LaneRecord::OperationFinished(finished) => finished.base.seq = seq,
        _ => panic!("fixture kind"),
    }
}
async fn unchanged(
    p: &Probe,
    h: &AgentHarness,
    rev: u64,
    native: &[LaneRecord],
    control: &[Entry],
) {
    assert_eq!(p.inner.history_revision().await.unwrap(), rev);
    assert_eq!(facts(h).await, native);
    assert_eq!(controls(h).await, control);
    assert_eq!(p.attempts.load(Ordering::SeqCst), control.len());
    assert_eq!(p.commits.load(Ordering::SeqCst), control.len());
}

#[tokio::test]
async fn exact_raw_window_boundaries_do_not_emit_phantom_or_missing_continuations() {
    bounded(async {
        for count in [0, 1, 64, 65] {
            let h = memory().await;
            for i in 0..count {
                h.session()
                    .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                        base: base(&format!("raw-boundary-{count}-{i}")),
                        run_id: "synthetic-unused".into(),
                    }))
                    .await
                    .unwrap();
            }
            let s = service(&h);
            let native = facts(&h).await;
            let first = s.execution_page(None).await.unwrap();
            assert!(first.runs.is_empty() && first.unavailable_runs == 0);
            assert_eq!(first.scanned_records, count.min(64));
            assert_eq!(first.next.is_some(), count == 65);
            if let Some(cursor) = first.next {
                let last = s.execution_page(Some(&cursor)).await.unwrap();
                assert_eq!(last.scanned_records, 1);
                assert!(last.next.is_none() && last.runs.is_empty());
            }
            assert_eq!(facts(&h).await, native);
            assert!(controls(&h).await.is_empty());
        }
    })
    .await;
}

#[tokio::test]
async fn malformed_page_shape_counts_and_scan_proof_pairs_fail_closed() {
    bounded(async {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        interrupted(&h, "page-adversarial-old").await;
        interrupted(&h, "page-adversarial-new").await;
        let s = service(&h);
        let baseline = s.execution_page(None).await.unwrap();
        let rev = p.inner.history_revision().await.unwrap();
        let native = facts(&h).await;
        let control = controls(&h).await;
        let original = response(&p).await;
        assert_eq!(original.records.len(), 4);
        for fault in ["lane", "zero", "future", "descending", "equal", "count", "oversized", "swapped-ids", "duplicate-id"] {
            let mut bad = copy_page(&original);
            match fault {
                "lane" => match &mut bad.records[0] {
                    LaneRecord::OperationStarted(started) => started.base.lane = "foreign".into(),
                    _ => panic!("start"),
                },
                "zero" => change_seq(&mut bad.records[0], 0),
                "future" => change_seq(&mut bad.records[3], rev + 1),
                "descending" => bad.records.reverse(),
                "equal" => { let seq = bad.records[0].seq(); change_seq(&mut bad.records[1], seq); },
                "count" => bad.total += 1,
                "oversized" => { while bad.records.len() <= 65 { bad.records.push(original.records[0].clone()); } bad.total = bad.records.len(); },
                "swapped-ids" | "duplicate-id" => {
                    let a = bad.records[0].id().to_owned();
                    let b = bad.records[2].id().to_owned();
                    if let LaneRecord::OperationStarted(started) = &mut bad.records[0] { started.base.id = b; }
                    if fault == "swapped-ids" {
                        if let LaneRecord::OperationStarted(started) = &mut bad.records[2] { started.base.id = a; }
                    }
                },
                _ => unreachable!(),
            }
            *p.execution_page_override.lock().unwrap() = Some(bad);
            assert!(matches!(s.execution_page(None).await, Err(DurableError::InvalidJournal(_))), "{fault}");
            assert_eq!(s.execution_page(None).await.unwrap(), baseline);
            unchanged(&p, &h, rev, &native, &control).await;
        }
        p.fail_pages.store(true, Ordering::SeqCst);
        assert!(matches!(s.execution_page(None).await, Err(DurableError::ReadFailure(message)) if message.contains("execution page read")));
        p.fail_pages.store(false, Ordering::SeqCst);
        // Same-ID storage replacement rotates the binding even when underlying
        // history is identical; pages cannot carry across that publication.
        let cursor = DurableExecutionCursor { scope: baseline.scope.clone(), before_seq: rev };
        h.session().set_storage(h.session().storage());
        assert!(matches!(s.execution_page(Some(&cursor)).await, Err(DurableError::Conflict)));
        assert!(s.execution_page(None).await.is_ok());
        unchanged(&p, &h, rev, &native, &control).await;
    }).await;
}

#[tokio::test]
async fn lookahead_is_validated_and_exclusive_cursor_cannot_repeat_boundary() {
    bounded(async {
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        for i in 0..34 {
            page_compaction(&h, &format!("page-lookahead-{i}")).await;
        }
        let s = service(&h);
        let original = response(&p).await;
        assert_eq!(original.records.len(), 65);
        let mut bad = copy_page(&original);
        // Only the older lookahead (index 0 in ascending storage response) is bad.
        change_seq(&mut bad.records[0], 0);
        *p.execution_page_override.lock().unwrap() = Some(bad);
        assert!(matches!(
            s.execution_page(None).await,
            Err(DurableError::InvalidJournal(_))
        ));
        let first = s.execution_page(None).await.unwrap();
        let cursor = first.next.unwrap();
        let mut bad = response(&p).await;
        // Simulate storage ignoring the exclusive cursor with an otherwise valid window.
        bad.records.retain(|r| r.seq() <= cursor.before_seq);
        bad.total = bad.records.len();
        assert!(bad.records.iter().any(|r| r.seq() == cursor.before_seq));
        *p.execution_page_override.lock().unwrap() = Some(bad);
        assert!(matches!(
            s.execution_page(Some(&cursor)).await,
            Err(DurableError::InvalidJournal(_))
        ));
        let next = s.execution_page(Some(&cursor)).await.unwrap();
        assert_eq!(next.scanned_records, 4);
        assert!(next.runs.is_empty() && next.next.is_none());
    })
    .await;
}

#[tokio::test]
async fn final_leaf_and_revision_faults_override_empty_filtered_unavailable_and_bad_pages() {
    bounded(async {
        for state in ["empty", "unavailable", "forgotten", "filtered", "invalid"] {
            let p = Probe::new(AgentHarnessOptions::default().session.storage());
            let h = harness(p.clone()).await;
            match state {
                "unavailable" => {
                    start(&h, "page-no-anchor").await;
                }
                "forgotten" => {
                    interrupted(&h, "page-forgotten-fault").await;
                    let s = service(&h);
                    s.mutate(
                        request(
                            &s,
                            "page-fault-forget",
                            ControlAction::ForgetRun,
                            Some("page-forgotten-fault"),
                        )
                        .await,
                    )
                    .await
                    .unwrap();
                }
                "filtered" => page_compaction(&h, "page-filtered-fault").await,
                _ => {}
            }
            let s = service(&h);
            let rev = p.inner.history_revision().await.unwrap();
            let native = facts(&h).await;
            let control = controls(&h).await;
            for leaf_fault in [true, false] {
                p.pause_execution_pages.store(true, Ordering::SeqCst);
                let s = s.clone();
                let reader = tokio::spawn(async move { s.execution_page(None).await });
                p.execution_page_captured.notified().await;
                if state == "invalid" {
                    *p.execution_page_override.lock().unwrap() = Some(RecordPage {
                        records: vec![],
                        total: 1,
                    });
                }
                if leaf_fault {
                    p.fail_lanes.store(true, Ordering::SeqCst);
                } else {
                    p.fail_revision.store(true, Ordering::SeqCst);
                }
                p.release_execution_page.notify_one();
                let error = reader.await.unwrap().unwrap_err();
                let marker = if leaf_fault {
                    "lane pointer read"
                } else {
                    "revision failure"
                };
                assert!(
                    matches!(error, DurableError::ReadFailure(message) if message.contains(marker)),
                    "{state}/{marker}"
                );
                p.fail_lanes.store(false, Ordering::SeqCst);
                p.fail_revision.store(false, Ordering::SeqCst);
                let normal = service(&h).execution_page(None).await.unwrap();
                assert!(normal.runs.is_empty());
                assert_eq!(normal.unavailable_runs, usize::from(state == "unavailable"));
                unchanged(&p, &h, rev, &native, &control).await;
            }
        }
    })
    .await;
}

#[tokio::test]
async fn actual_page_capture_conflicts_on_history_and_binding_movement_and_cancel_has_no_worker() {
    bounded(async {
        for rebind in [false, true] {
            let p = Probe::new(AgentHarnessOptions::default().session.storage());
            let h = harness(p.clone()).await;
            p.pause_execution_pages.store(true, Ordering::SeqCst);
            let s = service(&h);
            let reader = tokio::spawn(async move { s.execution_page(None).await });
            p.execution_page_captured.notified().await;
            if rebind {
                h.session().set_storage(h.session().storage());
            } else {
                h.session()
                    .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                        base: base("page-external-write"),
                        run_id: "no-run".into(),
                    }))
                    .await
                    .unwrap();
            }
            let rev = p.inner.history_revision().await.unwrap();
            let native = facts(&h).await;
            p.release_execution_page.notify_one();
            assert!(matches!(reader.await.unwrap(), Err(DurableError::Conflict)));
            assert!(service(&h).execution_page(None).await.is_ok());
            unchanged(&p, &h, rev, &native, &[]).await;
        }
        let p = Probe::new(AgentHarnessOptions::default().session.storage());
        let h = harness(p.clone()).await;
        let rev = p.inner.history_revision().await.unwrap();
        p.pause_execution_pages.store(true, Ordering::SeqCst);
        let s = service(&h);
        let reader = tokio::spawn(async move { s.execution_page(None).await });
        p.execution_page_captured.notified().await;
        *p.execution_page_override.lock().unwrap() = Some(RecordPage {
            records: vec![],
            total: 1,
        });
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        assert!(p.execution_page_override.lock().unwrap().is_some());
        assert!(matches!(
            service(&h).execution_page(None).await,
            Err(DurableError::InvalidJournal(_))
        ));
        let empty = service(&h).execution_page(None).await.unwrap();
        assert!(empty.runs.is_empty() && empty.next.is_none());
        unchanged(&p, &h, rev, &[], &[]).await;
    })
    .await;
}
