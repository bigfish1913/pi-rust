//! TUI page contracts. Oversize/control fixtures below exercise only formatting,
//! not native fact validation (in particular, not the 512-character ID domain).
use super::super::super as native;
use super::*;
use rpi_agent::AgentMessage;
use rpi_ai::types::UserMessage;
use rpi_harness::{
    durable::{DurableExecutionCursor, DurableRunSummary, DurableScope},
    session::{types::*, Session},
    types::AgentHarnessOptions,
};
use std::{
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

mod probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rpi-harness/tests/support/durable_control_probe.rs"
    ));
}
use probe::Probe;
#[path = "jsonl_tests.rs"]
mod jsonl_tests;

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("bounded page operation")
}
async fn harness(p: Arc<Probe>) -> AgentHarness {
    bounded(AgentHarness::create(AgentHarnessOptions {
        session: Session::new(p, None),
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    }))
    .await
    .unwrap()
}
fn typed_page() -> DurableExecutionPage {
    let scope = DurableScope {
        session_id: "synthetic-session".into(),
        lane: "main".into(),
        binding_token: "synthetic-binding".into(),
        leaf_id: None,
        history_revision: 140,
        history_generation: 0,
        entry_id: None,
        control_revision: 0,
    };
    DurableExecutionPage {
        next: Some(DurableExecutionCursor {
            scope: scope.clone(),
            before_seq: 77,
        }),
        scope,
        runs: vec![],
        scanned_records: 64,
        unavailable_runs: 3,
        control: serde_json::from_value(json!({
            "forgottenRunIds":["hidden-projection"],
            "recoveryRequest":{
                "runId":"explicit-intent", "startedSeq":9,
                "observedHistoryRevision":140,
                "unknownInvocationIds":["unknown-private-invocation"]
            }
        }))
        .unwrap(),
    }
}
fn row(id: String, started_seq: u64, status: &str) -> DurableRunSummary {
    DurableRunSummary {
        id,
        started_seq,
        status: serde_json::from_value(json!(status)).unwrap(),
        finished_seq: Some(started_seq + 1),
        finished_record_id: Some(format!("finish-{started_seq}")),
    }
}
fn next_args(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("Next page command: /durable "))
}
fn next_cursor(text: &str) -> Option<DurableExecutionCursor> {
    next_args(text)
        .map(|args| serde_json::from_str(args.strip_prefix("runs-page ").unwrap()).unwrap())
}
fn mandatory(text: &str, page: &DurableExecutionPage) {
    let scope: Value = serde_json::to_value(&page.scope).unwrap();
    assert_eq!(scope.as_object().unwrap().len(), 8);
    assert!(text.contains(&format!("Captured scope {}", quoted(&page.scope))));
    for key in native::SCOPE_FIELDS {
        assert!(text.contains(&format!("\"{key}\":")), "missing {key}");
    }
    for needle in [
        "read-only",
        "NOT an interrupted count",
        "No execution.",
        "All verified rows in this page are displayed; none omitted.",
        "never mix scopes",
        "Open != Interrupted",
        "run completed != workflow completed",
        "Manual queue, not workers",
        "not permission to execute",
        "No automatic recovery or replay",
    ] {
        // The no-intent variant uses lower-case 'no execution'.
        assert!(
            text.contains(needle) || (needle == "No execution." && text.contains("no execution.")),
            "{needle}: {text}"
        );
    }
    assert!(text.contains(&format!(
        "Scanned raw records: {} | verified rows: {} | unavailable candidates: {}",
        page.scanned_records,
        page.runs.len(),
        page.unavailable_runs
    )));
    assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
    assert!(text.len() <= MAX_TEXT_BYTES);
}

#[test]
fn typed_empty_window_and_complete_proven_rows_keep_scope_continuation_and_intent() {
    let mut page = typed_page();
    let empty = render(&page).unwrap();
    mandatory(&empty, &page);
    assert!(empty.contains("No verified run rows in THIS raw-record window; this is not a complete empty-history claim."));
    assert_eq!(next_cursor(&empty), page.next);
    assert_eq!(
        empty
            .lines()
            .filter(|line| line.starts_with("Next page command:"))
            .count(),
        1
    );
    assert!(
        empty.contains("Cursor is unsigned position, not authorization or complete-history proof")
    );
    assert!(empty.contains("Forgotten business projections: 1"));
    assert!(empty.contains("Recovery INTENT ONLY: explicit run \"explicit-intent\" | unknown invocations 1 | started sequence 9 | observed history revision 140. No execution."));
    assert!(!empty.contains("hidden-projection") && !empty.contains("unknown-private-invocation"));
    page.next = None;
    for (i, status) in [
        "open",
        "interrupted",
        "completed",
        "aborted",
        "failed",
        "declined",
    ]
    .into_iter()
    .enumerate()
    {
        page.runs.push(row(
            format!("complete-native-id-{i}"),
            100 - i as u64 * 2,
            status,
        ));
    }
    let text = render(&page).unwrap();
    mandatory(&text, &page);
    for run in &page.runs {
        assert!(text.contains(&format!(
            "Run {} | status {} | started {} | finished {} | finish record {}",
            quoted(&run.id),
            quoted(&run.status),
            run.started_seq,
            quoted(&run.finished_seq),
            quoted(&run.finished_record_id)
        )));
    }
    assert_eq!(
        text.lines().filter(|line| line.starts_with("Run ")).count(),
        6
    );
    assert!(
        text.contains("No next cursor in this captured scope. No automatic latest-run selection.")
    );
    assert!(next_args(&text).is_none());
    assert!(!text.contains("Run \"explicit-intent\""));
}

#[test]
fn typed_formatter_escapes_terminal_controls_and_preserves_copyable_cursor() {
    let mut page = typed_page();
    let hostile = "\0\u{1b}[31m\n\r\t\u{7f}\u{85}\u{9b}\"\\/界🦀";
    page.scope.session_id = hostile.into();
    page.scope.lane = hostile.into();
    page.scope.binding_token = hostile.into();
    page.scope.leaf_id = Some(hostile.into());
    page.scope.entry_id = Some(hostile.into());
    page.next.as_mut().unwrap().scope = page.scope.clone();
    page.control.forgotten_run_ids = vec![hostile.into()];
    page.control.recovery_request.as_mut().unwrap().run_id = hostile.into();
    page.control
        .recovery_request
        .as_mut()
        .unwrap()
        .unknown_invocation_ids = vec![hostile.into()];
    page.runs.push(row(hostile.into(), 6, "interrupted"));
    page.runs[0].finished_record_id = Some(hostile.into());
    let text = render(&page).unwrap();
    mandatory(&text, &page);
    assert_eq!(next_cursor(&text), page.next);
    assert!(text.contains("界🦀"));
    for escaped in [
        "\\u0000", "\\u001b", "\\u007f", "\\u0085", "\\u009b", "\\\"", "\\\\", "/",
    ] {
        assert!(text.contains(escaped), "missing {escaped}");
    }
    assert_eq!(
        text.lines().filter(|line| line.starts_with("Run ")).count(),
        1
    );
}

#[test]
fn formatter_budget_is_atomic_and_exactly_16kib_including_footer_and_cursor() {
    let mut page = typed_page();
    for i in 0..64 {
        page.runs.push(row("x".repeat(512), i * 2, "interrupted"));
    }
    let error = render(&page).unwrap_err();
    assert!(error.contains("No partial rows or next cursor displayed"));
    assert!(!error.contains("Next page command:") && !error.contains("Run \""));
    for cursor_only in [false, true] {
        let mut huge = typed_page();
        if cursor_only {
            huge.next.as_mut().unwrap().scope.binding_token = "z".repeat(MAX_TEXT_BYTES);
        } else {
            huge.scope.binding_token = "z".repeat(MAX_TEXT_BYTES);
        }
        let error = render(&huge).unwrap_err();
        assert!(error.contains("16 KiB") && !error.contains("Next page command:"));
    }
    // Deliberately oversized typed ID: this is not a service/domain proof.
    let mut boundary = typed_page();
    boundary.runs.push(row(String::new(), 1, "interrupted"));
    let fixed_bytes_including_footer = render(&boundary).unwrap().len();
    boundary.runs[0].id = "a".repeat(MAX_TEXT_BYTES - fixed_bytes_including_footer);
    let exact = render(&boundary).unwrap();
    assert_eq!(exact.len(), MAX_TEXT_BYTES);
    assert!(exact.ends_with(SAFETY));
    assert_eq!(next_cursor(&exact), boundary.next);
    boundary.runs[0].id.push('a');
    assert!(render(&boundary)
        .unwrap_err()
        .contains("No partial rows or next cursor displayed"));
}

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
async fn interrupted(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
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
    let leaf = format!("{id}-prompt");
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: leaf.clone(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(UserMessage::new("synthetic prompt", 0)),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finish")),
            run_id: id.into(),
            outcome: OperationOutcome::Aborted,
            error: Some(
                serde_json::from_value(
                    json!({"code":"interrupted", "message":"synthetic interruption"}),
                )
                .unwrap(),
            ),
            result: Some(RunResultIdentity {
                leaf_id: leaf,
                final_entry_id: None,
                assistant_entry_id: None,
            }),
        }))
        .await
        .unwrap();
}
async fn audit(h: &AgentHarness) -> (Value, Value, u64) {
    let storage = h.session().storage();
    (
        serde_json::to_value(storage.find_records(&RecordQuery::default()).await.unwrap()).unwrap(),
        serde_json::to_value(
            storage
                .find_entries(&EntryQuery {
                    custom_type: Some("rpi.durable.control.v1".into()),
                    ..Default::default()
                })
                .await
                .unwrap(),
        )
        .unwrap(),
        storage.history_revision().await.unwrap(),
    )
}
fn no_writes(p: &Probe) {
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(p.commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn actual_memory_pages_are_explicit_complete_read_only_and_scope_bound() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    for i in 0..70 {
        interrupted(&h, &format!("run-{i:03}")).await;
    }
    let s = native::service(&h).unwrap();
    let before = s.snapshot().await.unwrap();
    let original = audit(&h).await;
    assert_eq!(original.0.as_array().unwrap().len(), 140);
    let mut args = "runs-page".to_owned();
    let mut cursor = None;
    let mut seen = Vec::new();
    let mut raw_counts = Vec::new();
    let mut first_next = None;
    for index in 0..4 {
        let page = bounded(s.execution_page(cursor.as_ref())).await.unwrap();
        let text = bounded(native::interactive_command(&h, &args))
            .await
            .unwrap();
        assert_eq!(text, render(&page).unwrap());
        mandatory(&text, &page);
        assert_eq!(page.scope, before.scope);
        assert!(page
            .runs
            .iter()
            .all(|r| serde_json::to_value(&r.status).unwrap() == json!("interrupted")));
        let rendered_ids: Vec<String> = text
            .lines()
            .filter_map(|line| line.strip_prefix("Run "))
            .map(|line| {
                let id = line.split(" | status ").next().unwrap();
                serde_json::from_str(id).unwrap()
            })
            .collect();
        assert_eq!(
            rendered_ids,
            page.runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
        );
        if index == 0 {
            assert_eq!(rendered_ids.len(), 32);
            first_next = next_args(&text).map(str::to_owned);
        }
        seen.extend(rendered_ids);
        raw_counts.push(page.scanned_records);
        assert_eq!(next_cursor(&text), page.next);
        assert_eq!(audit(&h).await, original);
        assert_eq!(s.snapshot().await.unwrap(), before);
        no_writes(&p);
        match next_args(&text) {
            Some(next) => {
                args = next.to_owned();
                cursor = next_cursor(&text);
            }
            None => break,
        }
    }
    assert_eq!(raw_counts, [64, 64, 12]);
    assert_eq!(
        seen,
        (0..70)
            .rev()
            .map(|i| format!("run-{i:03}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        seen.iter().collect::<std::collections::HashSet<_>>().len(),
        70
    );
    let legacy = bounded(native::interactive_command(&h, "runs"))
        .await
        .unwrap_err();
    assert!(legacy.contains("limit_exceeded"));
    no_writes(&p);
    native::command(&h, &json!({"action":"enqueue", "requestId":"page-scope-change-001", "text":"manual review only"}).to_string()).await.unwrap();
    let changed = audit(&h).await;
    assert_eq!(changed.0, original.0);
    assert_eq!(changed.1.as_array().unwrap().len(), 1);
    let stale = bounded(native::interactive_command(
        &h,
        first_next.as_deref().unwrap(),
    ))
    .await
    .unwrap_err();
    assert!(stale.contains("conflict"), "{stale}");
    assert_eq!(audit(&h).await, changed);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    h.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parser_admission_read_failure_and_cancelled_captured_page_never_write_or_infer_empty() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = Arc::new(harness(p.clone()).await);
    let s = native::service(&h).unwrap();
    let before = bounded(s.snapshot()).await.unwrap();
    assert!(before.scope.leaf_id.is_none() && before.scope.entry_id.is_none());
    let original = audit(&h).await;
    let empty = bounded(native::interactive_command(&h, "runs-page"))
        .await
        .unwrap();
    let page = bounded(s.execution_page(None)).await.unwrap();
    mandatory(&empty, &page);
    assert!(page.runs.is_empty() && page.next.is_none());
    assert_eq!(page.scope, before.scope);
    let cursor = json!({"scope":before.scope, "beforeSeq":1});
    for field in native::SCOPE_FIELDS {
        let mut missing = cursor.clone();
        missing["scope"].as_object_mut().unwrap().remove(*field);
        let error = bounded(native::interactive_command(
            &h,
            &format!("runs-page {missing}"),
        ))
        .await
        .unwrap_err();
        assert!(error.contains("invalid_request"), "{field}: {error}");
    }
    for args in [
        "runs-page []".to_owned(),
        "runs-page false".into(),
        "runs-page {".into(),
        format!("runs-page {cursor} null"),
        r#"runs-page {"scope":{},"beforeSeq":1,"before\u0053eq":2}"#.into(),
        r#"runs-page {"scope":{"lane":"main","la\u006ee":"main"},"beforeSeq":1}"#.into(),
        "runs-pages".into(),
        "runs-pagex null".into(),
        "runs-page/null".into(),
        r#"{"action":"runs-page"}"#.into(),
    ] {
        let error = bounded(native::interactive_command(&h, &args))
            .await
            .unwrap_err();
        assert!(error.contains("invalid_request"), "{args}: {error}");
        assert!(!error.contains("No verified run rows"));
    }
    assert_eq!(
        native::command(&h, r#"{"action":"runs-page"}"#)
            .await
            .unwrap_err()
            .code,
        "invalid_request"
    );
    assert!(native::interactive_command(&h, "help")
        .await
        .unwrap()
        .contains("/durable runs-page"));
    p.fail_pages.store(true, Ordering::SeqCst);
    let error = bounded(native::interactive_command(&h, "runs-page"))
        .await
        .unwrap_err();
    assert!(
        error.contains("read_failure") && error.contains("No empty state inferred"),
        "{error}"
    );
    assert!(!error.contains("No verified run rows") && next_args(&error).is_none());
    let oversize = format!("{}runs-page", " ".repeat(MAX_REQUEST_BYTES));
    let error = bounded(native::interactive_command(&h, &oversize))
        .await
        .unwrap_err();
    assert!(error.contains("limit_exceeded") && !error.contains("read_failure"));
    p.fail_pages.store(false, Ordering::SeqCst);
    p.pause_execution_pages.store(true, Ordering::SeqCst);
    let reader = tokio::spawn({
        let h = h.clone();
        async move { native::interactive_command(&h, "runs-page").await }
    });
    bounded(p.execution_page_captured.notified()).await;
    reader.abort();
    assert!(bounded(reader).await.unwrap_err().is_cancelled());
    p.pause_execution_pages.store(false, Ordering::SeqCst);
    assert_eq!(
        bounded(native::interactive_command(&h, "runs-page"))
            .await
            .unwrap(),
        empty
    );
    assert_eq!(bounded(s.snapshot()).await.unwrap(), before);
    assert_eq!(audit(&h).await, original);
    no_writes(&p);
    h.close().await.unwrap();
    p.fail_reads.store(true, Ordering::SeqCst);
    p.fail_revision.store(true, Ordering::SeqCst);
    let error = bounded(native::interactive_command(&h, &oversize))
        .await
        .unwrap_err();
    assert!(
        error.contains("limit_exceeded")
            && !error.contains("read_failure")
            && !error.contains("closed")
    );
    no_writes(&p);
}

#[tokio::test]
async fn raw_64kib_exactly_is_admitted_but_one_extra_whitespace_byte_is_not() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    let original = audit(&h).await;
    let plain = native::interactive_command(&h, "runs-page null")
        .await
        .unwrap();
    let args = format!(
        "runs-page{}null",
        " ".repeat(MAX_REQUEST_BYTES - "runs-pagenull".len())
    );
    assert_eq!(args.len(), MAX_REQUEST_BYTES);
    assert_eq!(native::interactive_command(&h, &args).await.unwrap(), plain);
    let error = native::interactive_command(&h, &(args + " "))
        .await
        .unwrap_err();
    assert!(error.contains("limit_exceeded"));
    assert_eq!(audit(&h).await, original);
    no_writes(&p);
    h.close().await.unwrap();
}
