//! Explicit, isolated runtime acceptance for the existing experimental row store.
//! No selection/default backend changes; logical audit equality, not SQLite file equality.
use super::*;
use rpi_harness::{
    durable::{DurableExecutionCursor, DurableExecutionPage, MAX_EXECUTION_PAGE_RECORDS},
    session::experimental_native::ExperimentalNativeRepo,
};
use std::collections::HashSet;

async fn native_compaction(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
            source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
            intent: OperationIntent::Compaction {
                custom_instructions: Some("private native compaction".into()),
                result_entry_id: format!("{id}-result"),
            },
        }))
        .await
        .unwrap();
    finish(h, id, OperationOutcome::Completed, None, false).await;
}

async fn read_all(s: &DurableService) -> Vec<DurableExecutionPage> {
    let snapshot = s.snapshot().await.unwrap();
    let mut cursor: Option<DurableExecutionCursor> = None;
    let mut pages = Vec::new();
    let mut seen = HashSet::new();
    let mut last = u64::MAX;
    for _ in 0..16 {
        let page = s.execution_page(cursor.as_ref()).await.unwrap();
        assert_eq!(page.scope, snapshot.scope);
        assert_eq!(page.control, snapshot.execution_control);
        assert!(page.scanned_records <= MAX_EXECUTION_PAGE_RECORDS);
        for row in &page.runs {
            assert!(row.started_seq < last);
            last = row.started_seq;
            assert!(seen.insert(row.id.clone()));
            let proof = s.execution(&row.id).await.unwrap().run.unwrap();
            assert_eq!(row.started_seq, proof.started_seq);
            assert_eq!(row.status, proof.status);
            assert_eq!(row.finished_seq, proof.finished_seq);
            assert_eq!(row.finished_record_id, proof.finished_record_id);
        }
        assert!(!serde_json::to_string(&page).unwrap().contains("private"));
        if let Some(next) = &page.next {
            assert_eq!(next.scope, snapshot.scope);
            assert_eq!(page.scanned_records, MAX_EXECUTION_PAGE_RECORDS);
            assert!(next.before_seq > 0);
            if let Some(previous) = &cursor {
                assert!(next.before_seq < previous.before_seq);
            }
        }
        cursor = page.next.clone();
        pages.push(page);
        if cursor.is_none() {
            return pages;
        }
    }
    panic!("native paging did not terminate");
}

#[tokio::test]
async fn native_seventy_runs_page_and_reopen_without_loosening_index() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let store = repo
            .create(&SessionCreateOptions {
                id: Some("durable-native-seventy".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let metadata = store.metadata();
        let h = harness(Arc::new(store)).await;
        for i in 0..70 {
            interrupted(&h, &format!("native-run-{i:02}")).await;
        }
        let s = service(&h);
        let native = facts(&h).await;
        let audit = controls(&h).await;
        let scope = s.snapshot().await.unwrap().scope;
        assert!(matches!(
            s.execution_index().await,
            Err(DurableError::LimitExceeded)
        ));
        let pages = read_all(&s).await;
        assert_eq!(pages.len(), 3);
        assert_eq!(
            pages.iter().map(|p| p.scanned_records).collect::<Vec<_>>(),
            [64, 64, 12]
        );
        assert!(pages.iter().all(|p| p.unavailable_runs == 0));
        let ids: Vec<_> = pages
            .iter()
            .flat_map(|p| p.runs.iter().map(|r| r.id.clone()))
            .collect();
        assert_eq!(
            ids,
            (0..70)
                .rev()
                .map(|i| format!("native-run-{i:02}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, audit);
        assert_eq!(s.snapshot().await.unwrap().scope, scope);
        let cursor = pages[0].next.clone().unwrap();
        s.drain().await.unwrap();
        drop(s);
        drop(h);
        drop(repo);

        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
        let s = service(&h);
        assert!(matches!(
            s.execution_page(Some(&cursor)).await,
            Err(DurableError::Conflict)
        ));
        let reopened = read_all(&s).await;
        for (old, new) in pages.iter().zip(&reopened) {
            assert_ne!(old.scope.binding_token, new.scope.binding_token);
            assert_eq!(old.scope.history_revision, new.scope.history_revision);
            assert_eq!(old.scope.control_revision, new.scope.control_revision);
            assert_eq!(old.runs, new.runs);
            assert_eq!(old.scanned_records, new.scanned_records);
            assert_eq!(old.unavailable_runs, new.unavailable_runs);
        }
        assert_eq!(reopened.len(), pages.len());
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, audit);
        s.drain().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_real_faux_result_behind_filtered_pages_forget_and_reopen() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let store = repo
            .create(&SessionCreateOptions {
                id: Some("durable-native-faux".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let metadata = store.metadata();
        let provider = FauxProvider::new(FauxScript::new().with_text("private native answer"));
        let h = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            session: Session::new(Arc::new(store), None),
            require_native_durable: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let returned = h
            .prompt_text("private native prompt", vec![])
            .await
            .unwrap();
        for i in 0..65 {
            native_compaction(&h, &format!("native-compaction-{i}")).await;
        }
        let s = service(&h);
        let result = h.get_result(&returned.run_id).await.unwrap().unwrap();
        let native = facts(&h).await;
        let pages = read_all(&s).await;
        assert!(pages.len() >= 3);
        assert!(pages[0].runs.is_empty() && pages[0].next.is_some());
        assert!(pages[1].runs.is_empty() && pages[1].next.is_some());
        let rows: Vec<_> = pages.iter().flat_map(|p| &p.runs).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, returned.run_id);
        assert_eq!(rows[0].status, RunStatus::Completed);
        let cursor = pages[0].next.clone().unwrap();
        let forgotten = s
            .mutate(
                request(
                    &s,
                    "native-forget",
                    ControlAction::ForgetRun,
                    Some(&returned.run_id),
                )
                .await,
            )
            .await
            .unwrap();
        assert!(matches!(
            s.execution_page(Some(&cursor)).await,
            Err(DurableError::Conflict)
        ));
        let hidden = read_all(&s).await;
        assert!(hidden
            .iter()
            .all(|p| p.runs.is_empty() && p.unavailable_runs == 0));
        assert_eq!(facts(&h).await, native);
        let observed = h.get_result(&returned.run_id).await.unwrap().unwrap();
        assert_eq!(observed.run_id, result.run_id);
        match (&observed.outcome, &result.outcome) {
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
            _ => panic!("native completed result audit changed"),
        }
        let audit = controls(&h).await;
        let scope = s.snapshot().await.unwrap().scope;
        assert_eq!(audit.len(), 1);
        s.drain().await.unwrap();
        drop(s);
        drop(h);
        drop(repo);

        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        // No provider/model is installed on the reopened owner.
        let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
        let s = service(&h);
        assert_eq!(
            s.snapshot().await.unwrap().scope.history_revision,
            scope.history_revision
        );
        assert_eq!(
            s.request_outcome("native-forget").await.unwrap(),
            Some(forgotten.receipt)
        );
        assert!(read_all(&s).await.iter().all(|p| p.runs.is_empty()));
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, audit);
        let observed = h.get_result(&returned.run_id).await.unwrap().unwrap();
        assert_eq!(observed.run_id, result.run_id);
        match (&observed.outcome, &result.outcome) {
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
            _ => panic!("native completed result audit changed"),
        }
        s.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn native_captured_page_external_append_conflicts_and_cancel_releases_gate() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let store = repo
            .create(&SessionCreateOptions {
                id: Some("durable-native-page-race".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let p = Probe::new(Arc::new(store));
        let h = harness(p.clone()).await;
        interrupted(&h, "native-captured").await;
        let s = service(&h);
        let audit = controls(&h).await;
        let native = facts(&h).await;
        p.pause_execution_pages.store(true, Ordering::SeqCst);
        let read = {
            let s = s.clone();
            tokio::spawn(async move { s.execution_page(None).await })
        };
        p.execution_page_captured.notified().await;
        // Bypass this harness gate, but still use the real native storage writer.
        p.inner
            .append_entry(
                ProvisionedEntry {
                    id: "native-external-entry".into(),
                    kind: ProvisionedKind::Message {
                        message: UserMessage::new("private external", 0).into(),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        p.pause_execution_pages.store(false, Ordering::SeqCst);
        p.release_execution_page.notify_one();
        assert!(matches!(read.await.unwrap(), Err(DurableError::Conflict)));
        let scope = s.snapshot().await.unwrap().scope;
        let fresh = s.execution_page(None).await.unwrap();
        assert_eq!(fresh.runs.len(), 1);
        assert_eq!(fresh.scope, scope);

        p.pause_execution_pages.store(true, Ordering::SeqCst);
        let cancelled = {
            let s = s.clone();
            tokio::spawn(async move { s.execution_page(None).await })
        };
        p.execution_page_captured.notified().await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        p.pause_execution_pages.store(false, Ordering::SeqCst);
        assert_eq!(s.execution_page(None).await.unwrap(), fresh);
        assert_eq!(facts(&h).await, native);
        assert_eq!(controls(&h).await, audit);
        assert_eq!(s.snapshot().await.unwrap().scope, scope);
        assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
        s.drain().await.unwrap();
    })
    .await;
}
