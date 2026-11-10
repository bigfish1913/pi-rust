//! Read-only TUI discovery contracts; synthetic native facts and Faux only.
use super::*;
use crate::native_durable as native;
use rpi_agent::{agent_tool::AgentTool, AgentMessage};
use rpi_ai::{
    providers::faux::{faux_assistant_message, FauxProvider, FauxScript},
    types::{Content, StopReason, UserMessage},
};
use rpi_harness::{
    agent_harness::{AgentLane, HarnessRunOutcome},
    durable::DurableRunSummary,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::OsExecutionEnv;
use serde_json::{json, Value};
use std::{
    future::Future,
    sync::{atomic::Ordering, Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

// Reuse the actual storage probe, including its captured-index-scan hook.
mod probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rpi-harness/tests/support/durable_control_probe.rs"
    ));
}
use probe::Probe;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("bounded native runs test operation")
}
async fn memory() -> AgentHarness {
    bounded(AgentHarness::create(AgentHarnessOptions::default()))
        .await
        .unwrap()
}
async fn probed(p: Arc<Probe>) -> AgentHarness {
    bounded(AgentHarness::create(AgentHarnessOptions {
        session: Session::new(p, None),
        allow_existing_session: true,
        ..Default::default()
    }))
    .await
    .unwrap()
}
async fn view(h: &AgentHarness) -> Result<String, String> {
    bounded(native::interactive_command(h, " runs ")).await
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
async fn append(h: &AgentHarness, id: &str, message: AgentMessage) {
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Message {
                    message,
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
}
async fn start(h: &AgentHarness, id: &str, anchored: bool) {
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
    if anchored {
        append(
            h,
            &format!("{id}-prompt"),
            AgentMessage::User(UserMessage::new("private prompt payload", 0)),
        )
        .await;
    }
}
async fn finish(h: &AgentHarness, id: &str, identity: bool) {
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finish")),
            run_id: id.into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: if identity {
                Some(RunResultIdentity {
                    leaf_id: h
                        .session()
                        .view("main")
                        .get_leaf_id()
                        .await
                        .unwrap()
                        .unwrap(),
                    final_entry_id: None,
                    assistant_entry_id: None,
                })
            } else {
                None
            },
        }))
        .await
        .unwrap();
}
async fn facts(h: &AgentHarness) -> Value {
    serde_json::to_value(
        h.session()
            .storage()
            .find_records(&RecordQuery::default())
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn receipts(h: &AgentHarness) -> Value {
    serde_json::to_value(
        h.session()
            .storage()
            .find_entries(&EntryQuery {
                custom_type: Some("rpi.durable.control.v1".into()),
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .unwrap()
}
fn typed_index() -> DurableExecutionIndexSnapshot {
    serde_json::from_value(json!({
        "scope": {"sessionId":"synthetic-session", "lane":"main", "bindingToken":"synthetic-binding",
            "leafId":"captured-leaf", "historyRevision":12, "entryId":null, "controlRevision":0},
        "runs":[], "unavailableRuns":9,
        "control":{"forgottenRunIds":["hidden-id"], "recoveryRequest":{
            "runId":"intent-id", "startedSeq":1, "observedHistoryRevision":12,
            "unknownInvocationIds":["unknown-1"]}}
    })).unwrap()
}
fn row(id: String, seq: u64) -> DurableRunSummary {
    serde_json::from_value(json!({"id":id, "startedSeq":seq, "finishedRecordId":null,
        "finishedSeq":null, "status":"open"}))
    .unwrap()
}
fn mandatory(text: &str, verified: usize, unavailable: usize) {
    for needle in [
        "read-only",
        "Session",
        "Captured leaf",
        "history revision",
        "control revision",
        &format!("Verified runs: {verified} | unavailable candidates: {unavailable}"),
        "NOT an interrupted count",
        "not latest-run selection",
        "Choose an explicit runId",
        r#"/durable {"action":"status","runId":"<native-run-id>"}"#,
        "Open != Interrupted",
        "run completed != workflow completed",
        "No IDs truncated; no pagination",
        "Manual queue, not workers",
        "not permission to execute",
        "No automatic recovery or replay",
    ] {
        assert!(text.contains(needle), "missing {needle}: {text}");
    }
    assert!(text.len() <= MAX_TEXT_BYTES);
    assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
}

#[tokio::test]
async fn branch_proof_counts_sibling_and_unanchored_without_exposing_payloads_or_writing() {
    let h = memory().await;
    append(
        &h,
        "shared-source",
        AgentMessage::User(UserMessage::new("private shared payload", 0)),
    )
    .await;
    start(&h, "sibling-run", true).await;
    finish(&h, "sibling-run", true).await;
    h.session()
        .move_lane("main", Some("shared-source"))
        .await
        .unwrap();
    start(&h, "unanchored-run", false).await;
    finish(&h, "unanchored-run", false).await;
    start(&h, "selected-open", true).await;
    let mut assistant = faux_assistant_message("private assistant result", StopReason::ToolUse);
    assistant.content.push(Content::tool_call(
        "provider-call",
        "write",
        json!({"private":"arguments"}),
    ));
    append(
        &h,
        "selected-assistant",
        AgentMessage::Assistant(Box::new(assistant)),
    )
    .await;
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("selected-invocation"),
            run_id: "selected-open".into(),
            assistant_entry_id: "selected-assistant".into(),
            tool_index: 0,
            tool_call_id: "provider-call".into(),
            tool_name: "write".into(),
            effective_args: json!({"private":"effective arguments"}),
            result_entry_id: "reserved-private-result".into(),
            replay: ToolReplay::Never,
        }))
        .await
        .unwrap();
    let s = service(&h).unwrap();
    let before = s.snapshot().await.unwrap();
    let native = facts(&h).await;
    let controls = receipts(&h).await;
    let index = s.execution_index().await.unwrap();
    assert_eq!(index.unavailable_runs, 2);
    assert_eq!(index.runs.len(), 1);
    assert_eq!(index.runs[0].id, "selected-open");
    let text = view(&h).await.unwrap();
    mandatory(&text, 1, 2);
    assert!(text.contains("Run \"selected-open\" | status \"open\""));
    assert!(!text.contains("status \"interrupted\""));
    for hidden in [
        "sibling-run",
        "unanchored-run",
        "private",
        "provider-call",
        "selected-invocation",
        "effectiveArgs",
    ] {
        assert!(!text.contains(hidden), "leaked {hidden}");
    }
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(facts(&h).await, native);
    assert_eq!(receipts(&h).await, controls);
    h.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_faux_completed_read_and_forget_preserve_bytes_revision_and_native_result() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = FauxProvider::new(FauxScript::new().with_text("private faux answer"));
    let h = bounded(AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        session: Session::new(
            Arc::new(
                JsonlSessionStorage::create(
                    Arc::new(OsExecutionEnv::with_cwd(tmp.path().into())),
                    "runs.jsonl",
                    JsonlV4Header::new("synthetic-runs".into(), 1, "/".into(), None, None, None),
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            ),
            None,
        ),
        ..Default::default()
    }))
    .await
    .unwrap();
    let returned = bounded(h.prompt_text("private faux prompt", vec![]))
        .await
        .unwrap();
    let audit = h.get_result(&returned.run_id).await.unwrap().unwrap();
    let s = service(&h).unwrap();
    let native = facts(&h).await;
    for forgotten in [false, true] {
        if forgotten {
            let request = json!({"action":"forget_run", "runId":returned.run_id, "requestId":"runs-forget-001"});
            let ack = native::interactive_command(&h, &request.to_string())
                .await
                .unwrap();
            assert!(ack.contains("Mutation committed ACK"));
        }
        h.session().storage().drain().await;
        let path = tmp.path().join("runs.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let revision = h.session().storage().history_revision().await.unwrap();
        let before = s.snapshot().await.unwrap();
        let controls = receipts(&h).await;
        let text = view(&h).await.unwrap();
        mandatory(&text, usize::from(!forgotten), 0);
        if forgotten {
            assert!(!text.contains(&format!("Run {} |", quoted(&returned.run_id))));
            assert!(text.contains("Forgotten business projections: 1"));
        } else {
            assert!(text.contains(&format!(
                "Run {} | status \"completed\"",
                quoted(&returned.run_id)
            )));
        }
        assert!(!text.contains("private faux"));
        assert_eq!(s.snapshot().await.unwrap(), before);
        assert_eq!(facts(&h).await, native);
        assert_eq!(receipts(&h).await, controls);
        assert_eq!(controls.as_array().unwrap().len(), usize::from(forgotten));
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
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
            other => panic!("native completed result changed: {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    h.close().await.unwrap();
}

#[test]
fn typed_formatter_omits_only_whole_prefix_rows_and_keeps_escaped_complete_ids() {
    let mut index = typed_index();
    index.scope.session_id = "session-界\u{1b}\u{7f}\u{85}\u{9b}".into();
    index.scope.leaf_id = Some("leaf-\"\\\n\u{1b}\u{85}".into());
    index.runs = (0..64)
        .map(|i| {
            row(
                format!(
                    "{i}-{}-\"\\\n\u{0}\u{1b}\u{7f}\u{85}\u{9b}",
                    "界".repeat(110)
                ),
                64 - i,
            )
        })
        .collect();
    let text = render(&index).unwrap();
    mandatory(&text, 64, 9);
    assert!(text.contains(&format!("Session {}", quoted(&index.scope.session_id))));
    assert!(text.contains(&format!("Captured leaf {}", quoted(&index.scope.leaf_id))));
    for needle in [
        "Forgotten business projections: 1",
        "Recovery INTENT ONLY",
        "explicit run \"intent-id\"",
        "unknown invocations 1",
        "audit/get_result not erased",
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    let rows: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("Run "))
        .collect();
    assert!(!rows.is_empty() && rows.len() < 64);
    for (line, summary) in rows.iter().zip(&index.runs) {
        assert_eq!(
            *line,
            format!(
                "Run {} | status {} | started {} | finished {} | finish record {}",
                quoted(&summary.id),
                quoted(&summary.status),
                summary.started_seq,
                quoted(&summary.finished_seq),
                quoted(&summary.finished_record_id)
            )
        );
    }
    for summary in &index.runs[rows.len()..] {
        assert!(!text.contains(&quoted(&summary.id)));
    }
    assert!(text.contains(&display_summary(rows.len(), 64)));
    assert!(text.contains("\\u001b\\u007f\\u0085\\u009b"));
    // Count equality is explicit in the rendered footer, not inferred from truncation.
    assert!(text.contains(&format!("omitted {} whole rows", 64 - rows.len())));
}

#[test]
fn no_row_fits_is_not_empty_and_oversize_mandatory_scope_fails_atomically() {
    let mut index = typed_index();
    index.runs.push(row("界".repeat(MAX_TEXT_BYTES), 1));
    let text = render(&index).unwrap();
    mandatory(&text, 1, 9);
    assert!(text.contains(&display_summary(0, 1)));
    assert!(!text.contains("No proven, non-forgotten run rows"));
    assert!(!text.contains("Run \""));
    index.runs.clear();
    let empty = render(&index).unwrap();
    mandatory(&empty, 0, 9);
    assert!(empty.contains("No proven, non-forgotten run rows"));
    for leaf in [false, true] {
        let mut huge = typed_index();
        if leaf {
            huge.scope.leaf_id = Some("界".repeat(MAX_TEXT_BYTES));
        } else {
            huge.scope.session_id = "界".repeat(MAX_TEXT_BYTES);
        }
        let error = render(&huge).unwrap_err();
        assert!(error.contains("required scope/safety summary exceeds 16 KiB"));
        assert!(error.contains("read-only; no empty run history inferred"));
        assert!(!error.contains('界'));
    }
}

#[tokio::test]
async fn exact_tui_admission_keeps_flat_actions_separate_from_explicit_page_rpc() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = probed(p.clone()).await;
    let s = service(&h).unwrap();
    let before = s.snapshot().await.unwrap();
    let controls = receipts(&h).await;
    let native = facts(&h).await;
    let binding = Arc::new(OnceLock::new());
    assert!(binding.set(h.downgrade()).is_ok());
    let tool = native::NativeDurableTool::new(binding);
    let schema = serde_json::to_value(tool.schema()).unwrap();
    assert!(!schema["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("runs")));
    for args in ["runs extra", "runs\nextra", r#"{"action":"runs"}"#] {
        assert!(
            native::interactive_command(&h, args).await.is_err(),
            "{args}"
        );
    }
    assert_eq!(
        native::command(&h, r#"{"action":"runs"}"#)
            .await
            .unwrap_err()
            .code,
        "invalid_request"
    );
    let error = tool
        .execute(
            "synthetic-provider-call",
            json!({"action":"runs"}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid_request"));
    for request in [
        json!({"type":"runs", "version":1}),
        json!({"type":"get_durable_state", "version":1, "action":"runs"}),
        json!({"type":"durable_mutate", "version":1, "requestId":"runs-invalid", "expected":before.scope, "action":{"action":"runs"}}),
    ] {
        assert_eq!(
            native::rpc(&h, &request, true).await.unwrap_err().code,
            "invalid_request"
        );
    }
    let json_status = native::command(&h, r#"{"action":"status"}"#).await.unwrap();
    assert_eq!(json_status["state"], serde_json::to_value(&before).unwrap());
    for enabled in [false, true] {
        assert_eq!(
            native::capabilities(enabled)["commands"],
            json!([
                "get_durable_state",
                "durable_mutate",
                "get_durable_request",
                "get_durable_runs"
            ])
        );
        assert_eq!(
            native::capabilities(enabled)["executionPages"],
            json!({
                "version":1, "command":"get_durable_runs", "cursor":"scope_bound_exclusive",
                "maxRecords":64, "automatic":false
            })
        );
    }
    assert!(!schema["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("get_durable_runs")));
    assert_eq!(
        native::rpc(&h, &json!({"type":"get_durable_runs","version":1}), true)
            .await
            .unwrap(),
        json!({"version":1,"page":s.execution_page(None).await.unwrap()})
    );
    let help = native::interactive_command(&h, "help").await.unwrap();
    assert!(help.contains("/durable runs"));
    assert!(help.contains("runs is TUI-only, not a JSON/tool/RPC action"));
    assert!(!help
        .split("Flat JSON actions:")
        .nth(1)
        .unwrap()
        .split("Every mutation")
        .next()
        .unwrap()
        .contains("runs"));
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(receipts(&h).await, controls);
    assert_eq!(facts(&h).await, native);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    h.close().await.unwrap();
    p.fail_reads.store(true, Ordering::SeqCst);
    p.fail_revision.store(true, Ordering::SeqCst);
    let oversize = format!("{}runs", " ".repeat(MAX_REQUEST_BYTES));
    let error = native::interactive_command(&h, &oversize)
        .await
        .unwrap_err();
    assert!(error.contains("limit_exceeded"));
    assert!(error.contains("64 KiB"));
    assert!(!error.contains("\"closed\"") && !error.contains("read_failure"));
    assert!(native::interactive_command(&h, "help").await.is_ok());
}

#[tokio::test]
async fn actual_read_failure_bad_control_closed_and_65_start_cap_never_become_empty() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = probed(p.clone()).await;
    append(
        &h,
        "read-anchor",
        AgentMessage::User(UserMessage::new("private anchor", 0)),
    )
    .await;
    let native = facts(&h).await;
    let revision = p.inner.history_revision().await.unwrap();
    for revision_failure in [false, true] {
        p.fail_reads.store(!revision_failure, Ordering::SeqCst);
        p.fail_revision.store(revision_failure, Ordering::SeqCst);
        let error = view(&h).await.unwrap_err();
        assert!(error.contains("read_failure"), "{error}");
        assert!(error.contains("No empty state inferred"));
    }
    p.fail_reads.store(false, Ordering::SeqCst);
    p.fail_revision.store(false, Ordering::SeqCst);
    assert_eq!(facts(&h).await, native);
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    h.session()
        .storage()
        .append_entry(
            ProvisionedEntry {
                id: "bad-control".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "rpi.durable.control.v1".into(),
                    data: Some(json!({"invalid":"control"})),
                },
            },
            "main",
        )
        .await
        .unwrap();
    let revision = p.inner.history_revision().await.unwrap();
    let error = view(&h).await.unwrap_err();
    assert!(error.contains("No empty state inferred"));
    assert!(!error.contains("Verified runs:"));
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    h.close().await.unwrap();
    let error = view(&h).await.unwrap_err();
    assert!(error.contains("closed"), "{error}");
    assert!(error.contains("No empty state inferred"));

    let h = memory().await;
    for i in 0..65 {
        let id = format!("budget-{i}");
        start(&h, &id, false).await;
        finish(&h, &id, false).await;
    }
    let revision = h.session().storage().history_revision().await.unwrap();
    let native = facts(&h).await;
    let error = view(&h).await.unwrap_err();
    assert!(error.contains("limit_exceeded"), "{error}");
    assert!(error.contains("No empty state inferred"));
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert_eq!(facts(&h).await, native);
    assert!(receipts(&h).await.as_array().unwrap().is_empty());
    h.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_actual_captured_index_scan_releases_gate_without_late_writes() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = Arc::new(probed(p.clone()).await);
    start(&h, "cancel-open", true).await;
    let s = service(&h).unwrap();
    let before = s.snapshot().await.unwrap();
    let native = facts(&h).await;
    let controls = receipts(&h).await;
    let revision = p.inner.history_revision().await.unwrap();
    p.pause_index_scans.store(true, Ordering::SeqCst);
    let reader = tokio::spawn({
        let h = h.clone();
        async move { native::interactive_command(&h, "runs").await }
    });
    bounded(p.index_scan_captured.notified()).await;
    reader.abort();
    assert!(bounded(reader).await.unwrap_err().is_cancelled());
    p.release_index_scan.notify_one();
    let fresh = view(&h).await.unwrap();
    mandatory(&fresh, 1, 0);
    assert!(fresh.contains("Run \"cancel-open\" | status \"open\""));
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(p.inner.history_revision().await.unwrap(), revision);
    assert_eq!(facts(&h).await, native);
    assert_eq!(receipts(&h).await, controls);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(p.commits.load(Ordering::SeqCst), 0);
    h.close().await.unwrap();
}
