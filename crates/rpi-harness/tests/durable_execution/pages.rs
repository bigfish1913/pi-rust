//! Core-only explicit execution paging; all fixtures close operations before the next start.
#[path = "pages/adversarial.rs"]
mod adversarial;
use super::*;
use rpi_harness::durable::{
    DurableExecutionCursor, DurableExecutionPage, DurableRunSummary, MAX_EXECUTION_PAGE_RECORDS,
};
use std::collections::HashSet;

async fn page_compaction(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
            source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
            intent: OperationIntent::Compaction {
                custom_instructions: Some("private compaction".into()),
                result_entry_id: format!("{id}-result"),
            },
        }))
        .await
        .unwrap();
    finish(h, id, OperationOutcome::Completed, None, false).await;
}

async fn assert_summary(s: &DurableService, summary: &DurableRunSummary) {
    let explicit = s.execution(&summary.id).await.unwrap().run.unwrap();
    assert_eq!(summary.started_seq, explicit.started_seq);
    assert_eq!(summary.finished_record_id, explicit.finished_record_id);
    assert_eq!(summary.finished_seq, explicit.finished_seq);
    assert_eq!(summary.status, explicit.status);
}

// A hard iteration limit catches non-progress without relying on a timeout alone.
async fn collect_pages(s: &DurableService) -> Vec<DurableExecutionPage> {
    let snapshot = s.snapshot().await.unwrap();
    let mut cursor: Option<DurableExecutionCursor> = None;
    let mut pages = Vec::new();
    let mut previous_start = u64::MAX;
    let mut ids = HashSet::new();
    for _ in 0..16 {
        let page = bounded(s.execution_page(cursor.as_ref())).await.unwrap();
        assert_eq!(page.scope, snapshot.scope);
        assert_eq!(page.control, snapshot.execution_control);
        assert!(page.scanned_records <= MAX_EXECUTION_PAGE_RECORDS);
        for summary in &page.runs {
            assert!(summary.started_seq < previous_start);
            if let Some(cursor) = &cursor {
                assert!(summary.started_seq < cursor.before_seq);
            }
            previous_start = summary.started_seq;
            assert!(
                ids.insert(summary.id.clone()),
                "duplicate start across pages"
            );
            assert_summary(s, summary).await;
        }
        assert!(!serde_json::to_string(&page).unwrap().contains("private"));
        if let Some(next) = &page.next {
            assert_eq!(page.scanned_records, MAX_EXECUTION_PAGE_RECORDS);
            assert_eq!(next.scope, snapshot.scope);
            assert!(next.before_seq > 0);
            if let Some(cursor) = &cursor {
                assert!(next.before_seq < cursor.before_seq);
            }
        }
        cursor = page.next.clone();
        pages.push(page);
        if cursor.is_none() {
            return pages;
        }
    }
    panic!("execution paging failed to terminate");
}

#[tokio::test]
async fn seventy_proven_runs_page_without_relaxing_the_old_index_limit() {
    bounded(async {
        assert_eq!(MAX_EXECUTION_PAGE_RECORDS, 64);
        let h = memory().await;
        for i in 0..70 {
            interrupted(&h, &format!("page-run-{i:02}")).await;
        }
        let s = service(&h);
        let native = facts(&h).await;
        let receipts = controls(&h).await;
        let revision = h.session().storage().history_revision().await.unwrap();
        assert!(matches!(
            s.execution_index().await,
            Err(DurableError::LimitExceeded)
        ));
        let pages = collect_pages(&s).await;
        assert!(pages.len() >= 3);
        assert_eq!(pages.iter().map(|p| p.scanned_records).sum::<usize>(), 140);
        assert!(pages.iter().all(|p| p.unavailable_runs == 0));
        let ids: Vec<_> = pages
            .iter()
            .flat_map(|p| p.runs.iter().map(|r| r.id.clone()))
            .collect();
        let expected: Vec<_> = (0..70).rev().map(|i| format!("page-run-{i:02}")).collect();
        assert_eq!(ids, expected);
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, receipts);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_completed_result_and_forget_preserve_native_audit() {
    bounded(async {
        let tmp = tempfile::tempdir().unwrap();
        let provider = FauxProvider::new(FauxScript::new().with_text("private page answer"));
        let h = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            session: Session::new(
                Arc::new(
                    JsonlSessionStorage::create(
                        Arc::new(OsExecutionEnv::with_cwd(tmp.path().into())),
                        "pages.jsonl",
                        JsonlV4Header::new("page-faux".into(), 1, "/".into(), None, None, None),
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
        let returned = h.prompt_text("private page prompt", vec![]).await.unwrap();
        for i in 0..65 {
            page_compaction(&h, &format!("page-jsonl-compaction-{i}")).await;
        }
        let s = service(&h);
        let audit = h.get_result(&returned.run_id).await.unwrap().unwrap();
        h.session().storage().drain().await;
        let path = tmp.path().join("pages.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let native = facts(&h).await;
        let receipts = controls(&h).await;
        let revision = h.session().storage().history_revision().await.unwrap();
        let pages = collect_pages(&s).await;
        assert!(pages.len() >= 3);
        let rows: Vec<_> = pages.iter().flat_map(|p| &p.runs).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, returned.run_id);
        assert_eq!(rows[0].status, RunStatus::Completed);
        assert!(pages.iter().all(|p| p.unavailable_runs == 0));
        let old_cursor = pages[0].next.clone().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, receipts);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );

        s.mutate(
            request(
                &s,
                "page-jsonl-forget",
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
        assert!(matches!(
            s.execution_page(Some(&old_cursor)).await,
            Err(DurableError::Conflict)
        ));
        let pages = collect_pages(&s).await;
        assert!(pages
            .iter()
            .all(|p| p.runs.is_empty() && p.unavailable_runs == 0));
        assert!(pages
            .iter()
            .all(|p| p.control.forgotten_run_ids == vec![returned.run_id.clone()]));
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
            ) => {
                assert_eq!((a, b, c), (x, y, z));
            }
            _ => panic!("forget changed completed native result"),
        }
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await.len(), receipts.len() + 1);
        assert_eq!(std::fs::read(&path).unwrap(), forgotten_bytes);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            forgotten_revision
        );
    })
    .await;
}

#[tokio::test]
async fn empty_filtered_windows_require_explicit_continuation() {
    bounded(async {
        let h = memory().await;
        interrupted(&h, "page-old-visible").await;
        for i in 0..65 {
            page_compaction(&h, &format!("page-filter-{i}")).await;
        }
        let s = service(&h);
        let native = facts(&h).await;
        let revision = h.session().storage().history_revision().await.unwrap();
        let first = s.execution_page(None).await.unwrap();
        assert!(first.runs.is_empty());
        assert_eq!(first.unavailable_runs, 0);
        assert_eq!(first.scanned_records, 64);
        assert!(first.next.is_some());
        assert_eq!(s.execution_page(None).await.unwrap(), first);
        let pages = collect_pages(&s).await;
        assert_eq!(pages.len(), 3);
        assert_eq!(
            pages.iter().map(|p| p.scanned_records).collect::<Vec<_>>(),
            vec![64, 64, 4]
        );
        assert!(pages[1].runs.is_empty());
        assert_eq!(pages[2].runs.len(), 1);
        assert_eq!(pages[2].runs[0].id, "page-old-visible");
        assert!(pages.iter().all(|p| p.unavailable_runs == 0));
        assert_eq!(facts(&h).await, native);
        assert!(controls(&h).await.is_empty());
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
    })
    .await;
}

#[tokio::test]
async fn branch_proof_and_forget_filter_candidates_across_raw_windows() {
    bounded(async {
        let h = memory().await;
        append(
            &h,
            "page-shared-source",
            AgentMessage::User(UserMessage::new("private source", 0)),
        )
        .await;
        interrupted(&h, "page-sibling").await;
        h.session()
            .move_lane("main", Some("page-shared-source"))
            .await
            .unwrap();
        start(&h, "page-unanchored").await;
        h.session()
            .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                base: base("page-anchor-gap"),
                run_id: "page-unanchored".into(),
            }))
            .await
            .unwrap();
        finish(
            &h,
            "page-unanchored",
            OperationOutcome::Aborted,
            None,
            false,
        )
        .await;
        interrupted(&h, "page-forgotten").await;
        let s = service(&h);
        s.mutate(
            request(
                &s,
                "page-forget",
                ControlAction::ForgetRun,
                Some("page-forgotten"),
            )
            .await,
        )
        .await
        .unwrap();
        for i in 0..65 {
            page_compaction(&h, &format!("page-branch-compaction-{i}")).await;
        }
        interrupted(&h, "page-selected").await;
        h.session()
            .storage()
            .create_lane("page-other-lane", None)
            .await
            .unwrap();
        let mut other_base = base("page-other-run");
        other_base.lane = "page-other-lane".into();
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
        let native = facts(&h).await;
        let receipts = controls(&h).await;
        let revision = h.session().storage().history_revision().await.unwrap();
        let pages = collect_pages(&s).await;
        assert!(pages.len() >= 3);
        assert_eq!(pages.iter().map(|p| p.unavailable_runs).sum::<usize>(), 2);
        let rows: Vec<_> = pages.iter().flat_map(|p| &p.runs).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "page-selected");
        assert_eq!(
            pages.iter().map(|p| p.scanned_records).sum::<usize>(),
            native.iter().filter(|r| r.lane() == "main").count()
        );
        for page in &pages {
            let text = serde_json::to_string(page).unwrap();
            for hidden in [
                "page-sibling",
                "page-unanchored",
                "page-other-run",
                "private",
            ] {
                assert!(!text.contains(hidden), "page exposed {hidden}");
            }
            assert!(page.runs.iter().all(|r| r.id != "page-forgotten"));
            assert_eq!(page.control.forgotten_run_ids, vec!["page-forgotten"]);
        }
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, receipts);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
    })
    .await;
}

#[tokio::test]
async fn cursor_dto_scope_and_positions_are_checked_without_implicit_restart() {
    bounded(async {
        let h = memory().await;
        interrupted(&h, "page-cursor-run").await;
        for i in 0..33 {
            page_compaction(&h, &format!("page-cursor-compaction-{i}")).await;
        }
        let s = service(&h);
        let page = s.execution_page(None).await.unwrap();
        let cursor = page.next.clone().unwrap();
        let encoded = serde_json::to_value(&cursor).unwrap();
        assert_eq!(
            serde_json::from_value::<DurableExecutionCursor>(encoded.clone()).unwrap(),
            cursor
        );
        assert_eq!(
            serde_json::from_value::<DurableExecutionPage>(serde_json::to_value(&page).unwrap())
                .unwrap(),
            page
        );
        let mut unknown = encoded.clone();
        unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<DurableExecutionCursor>(unknown).is_err());
        let mut unknown_page = serde_json::to_value(&page).unwrap();
        unknown_page["totalRunCount"] = json!(1);
        assert!(serde_json::from_value::<DurableExecutionPage>(unknown_page).is_err());
        for (field, value) in [
            ("sessionId", json!("another-session")),
            ("lane", json!("another-lane")),
            ("bindingToken", json!("another-binding")),
            ("leafId", json!("another-leaf")),
            ("historyRevision", json!(cursor.scope.history_revision + 1)),
            ("entryId", json!("another-control-entry")),
            ("controlRevision", json!(cursor.scope.control_revision + 1)),
        ] {
            let mut changed = encoded.clone();
            changed["scope"][field] = value;
            let changed: DurableExecutionCursor = serde_json::from_value(changed).unwrap();
            assert!(
                matches!(
                    s.execution_page(Some(&changed)).await,
                    Err(DurableError::Conflict)
                ),
                "scope field {field}"
            );
        }
        for before_seq in [0, cursor.scope.history_revision + 1] {
            let invalid = DurableExecutionCursor {
                before_seq,
                ..cursor.clone()
            };
            assert!(matches!(
                s.execution_page(Some(&invalid)).await,
                Err(DurableError::InvalidRequest(_))
            ));
        }
        // The unsigned position may intentionally skip history; it is not an authorization token.
        let manual = DurableExecutionCursor {
            before_seq: 1,
            ..cursor.clone()
        };
        let empty = s.execution_page(Some(&manual)).await.unwrap();
        assert!(empty.runs.is_empty() && empty.next.is_none());
        assert_eq!(empty.scanned_records, 0);
        let other = memory().await;
        assert!(matches!(
            service(&other).execution_page(Some(&cursor)).await,
            Err(DurableError::Conflict)
        ));
        append(
            &h,
            "page-cursor-native-append",
            AgentMessage::User(UserMessage::new("private append", 0)),
        )
        .await;
        assert!(matches!(
            s.execution_page(Some(&cursor)).await,
            Err(DurableError::Conflict)
        ));
        let fresh = s.execution_page(None).await.unwrap().next.unwrap();
        s.mutate(
            request(
                &s,
                "page-cursor-forget",
                ControlAction::ForgetRun,
                Some("page-cursor-run"),
            )
            .await,
        )
        .await
        .unwrap();
        assert!(matches!(
            s.execution_page(Some(&fresh)).await,
            Err(DurableError::Conflict)
        ));
        let fresh = s.execution_page(None).await.unwrap().next.unwrap();
        h.session().move_lane("main", None).await.unwrap();
        assert!(matches!(
            s.execution_page(Some(&fresh)).await,
            Err(DurableError::Conflict)
        ));
        assert!(s.execution_page(None).await.is_ok());
    })
    .await;
}
