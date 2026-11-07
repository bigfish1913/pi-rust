//! P3 explicit native execution projection and recovery-intent integration contracts.
//! Synthetic native records are installed only AFTER harness creation (no startup repair).
#[path = "durable_execution/index.rs"]
mod index;
#[cfg(feature = "experimental-native-store")]
#[path = "durable_execution/native_store.rs"]
mod native_store;
#[path = "durable_execution/pages.rs"]
mod pages;
#[path = "support/durable_control_probe.rs"]
mod probe;
use probe::Probe;
use rpi_agent::AgentMessage;
use rpi_ai::{
    providers::faux::{faux_assistant_message, FauxProvider, FauxScript},
    types::{Content, StopReason, ToolResultMessage, ToolResultRole, UserMessage},
};
use rpi_durable::{
    control::ControlAction,
    execution::{ExecutionError, ReplayPolicy, RunStatus, ToolStatus},
};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome},
    durable::{DurableActor, DurableError, DurableMutation, DurableService},
    frame_progress::INTERRUPTED_TOOL_RESULT,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SqliteSessionRepo, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::json;
use std::{
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("bounded operation")
}
async fn harness(storage: Arc<dyn SessionStorage>) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session: Session::new(storage, None),
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
async fn start(h: &AgentHarness, run: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(run),
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
}
async fn prompt_anchor(h: &AgentHarness, run: &str) {
    start(h, run).await;
    append(
        h,
        &format!("{run}-prompt"),
        AgentMessage::User(UserMessage::new("private prompt payload", 0)),
    )
    .await;
}
async fn finish(
    h: &AgentHarness,
    run: &str,
    outcome: OperationOutcome,
    code: Option<&str>,
    identity: bool,
) {
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{run}-finished")),
            run_id: run.into(),
            outcome,
            error: code.map(|code| OperationError {
                code: code.into(),
                message: "private error".into(),
            }),
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
async fn interrupted(h: &AgentHarness, run: &str) {
    prompt_anchor(h, run).await;
    finish(h, run, OperationOutcome::Aborted, Some("interrupted"), true).await;
}
async fn tool(h: &AgentHarness, run: &str, index: usize, replay: ToolReplay) {
    let assistant = format!("{run}-assistant-{index}");
    let mut msg = faux_assistant_message("private assistant payload", StopReason::ToolUse);
    msg.content.push(Content::tool_call(
        "reused-provider-id",
        "write",
        json!({"secret":"private arguments"}),
    ));
    append(h, &assistant, AgentMessage::Assistant(Box::new(msg))).await;
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base(&format!("{run}-invocation-{index}")),
            run_id: run.into(),
            assistant_entry_id: assistant,
            tool_index: 0,
            tool_call_id: "reused-provider-id".into(),
            tool_name: "write".into(),
            effective_args: json!({"secret":"private effective arguments"}),
            result_entry_id: format!("{run}-result-{index}"),
            replay,
        }))
        .await
        .unwrap();
}
fn result(call: &str, name: &str, error: bool, text: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: call.into(),
        tool_name: name.into(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: error,
        timestamp: 0,
    }))
}
async fn request(
    s: &DurableService,
    id: &str,
    action: ControlAction,
    run: Option<&str>,
) -> DurableMutation {
    DurableMutation {
        request_id: id.into(),
        expected: s.snapshot().await.unwrap().scope,
        action,
        run_id: run.map(str::to_owned),
    }
}
async fn controls(h: &AgentHarness) -> Vec<Entry> {
    h.session()
        .storage()
        .find_entries(&EntryQuery {
            custom_type: Some("rpi.durable.control.v1".into()),
            ..Default::default()
        })
        .await
        .unwrap()
}
async fn facts(h: &AgentHarness) -> Vec<LaneRecord> {
    h.session()
        .storage()
        .find_records(&RecordQuery::default())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_faux_prompt_projects_completed_and_forget_preserves_result_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let p = FauxProvider::new(FauxScript::new().with_text("private faux answer"));
    let h = AgentHarness::create(AgentHarnessOptions {
        model: p.default_model().clone(),
        models: vec![p],
        session: Session::new(
            Arc::new(
                JsonlSessionStorage::create(
                    Arc::new(OsExecutionEnv::with_cwd(tmp.path().into())),
                    "faux.jsonl",
                    JsonlV4Header::new("faux".into(), 1, "/".into(), None, None, None),
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
    let returned = bounded(h.prompt_text("private faux prompt", vec![]))
        .await
        .unwrap();
    let s = service(&h);
    let observed = s.execution(&returned.run_id).await.unwrap();
    let run = observed.run.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert!(run.final_entry_id.is_some());
    let before = facts(&h).await;
    let audit = h.get_result(&returned.run_id).await.unwrap().unwrap();
    let outcome = s
        .mutate(
            request(
                &s,
                "forget",
                ControlAction::ForgetRun,
                Some(&returned.run_id),
            )
            .await,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.receipt.run_id.as_deref(),
        Some(returned.run_id.as_str())
    );
    assert!(s.execution(&returned.run_id).await.unwrap().run.is_none());
    assert_eq!(facts(&h).await, before);
    let after = h.get_result(&returned.run_id).await.unwrap().unwrap();
    assert_eq!(after.run_id, audit.run_id);
    match (&after.outcome, &audit.outcome) {
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
        _ => panic!("forget changed native result audit: {after:?}, {audit:?}"),
    }
    let serialized = serde_json::to_string(&run).unwrap();
    assert!(!serialized.contains("private"));
}

struct RealExecutionProbe {
    schema: rpi_ai::types::Tool,
    calls: std::sync::atomic::AtomicUsize,
    fail: bool,
}
#[async_trait::async_trait]
impl rpi_agent::AgentTool for RealExecutionProbe {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "synthetic execution proof"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(rpi_agent::AgentError::tool("synthetic failure"))
        } else {
            Ok(rpi_agent::AgentToolResult::text("synthetic result"))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_faux_tool_roundtrip_proves_success_and_failure_without_reexecution() {
    for fail in [false, true] {
        let provider = FauxProvider::new(
            FauxScript::new()
                .with_tool_call("probe", json!({}))
                .with_text("done"),
        );
        let tool = Arc::new(RealExecutionProbe {
            schema: rpi_ai::types::Tool {
                name: "probe".into(),
                description: "synthetic".into(),
                parameters: rpi_ai::types::Schema::new(json!({"type":"object"})),
                constrained_sampling: None,
            },
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail,
        });
        let h = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            tools: vec![rpi_harness::types::HarnessTool::new(tool.clone())
                .with_replay(rpi_harness::types::ToolReplay::Safe)],
            ..Default::default()
        })
        .await
        .unwrap();
        let result = bounded(h.prompt_text("exercise synthetic tool", vec![]))
            .await
            .unwrap();
        let s = service(&h);
        let native = s.execution(&result.run_id).await.unwrap().run.unwrap();
        assert_eq!(native.status, RunStatus::Completed);
        assert_eq!(native.tools.len(), 1);
        assert_eq!(
            native.tools[0].status,
            if fail {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            }
        );
        assert_eq!(native.tools[0].replay, ReplayPolicy::Safe);
        assert_eq!(
            native.tools[0].result_entry_id.as_deref(),
            Some(native.tools[0].reserved_result_entry_id.as_str())
        );
        let before = facts(&h).await;
        assert!(matches!(
            s.mutate(
                request(
                    &s,
                    "cannot-recover",
                    ControlAction::Recover,
                    Some(&result.run_id)
                )
                .await
            )
            .await,
            Err(DurableError::Execution(ExecutionError::NotInterrupted))
        ));
        s.mutate(
            request(
                &s,
                "forget-real",
                ControlAction::ForgetRun,
                Some(&result.run_id),
            )
            .await,
        )
        .await
        .unwrap();
        assert!(s.execution(&result.run_id).await.unwrap().run.is_none());
        assert!(h.get_result(&result.run_id).await.unwrap().is_some());
        assert_eq!(facts(&h).await, before);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn explicit_run_missing_unanchored_and_wrong_lane_never_guess_latest() {
    let h = memory().await;
    let s = service(&h);
    assert!(matches!(
        s.execution("missing").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
    start(&h, "unanchored").await;
    finish(&h, "unanchored", OperationOutcome::Completed, None, false).await;
    assert!(matches!(
        s.execution("unanchored").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
    interrupted(&h, "visible").await;
    assert!(matches!(
        s.execution("missing").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
    h.session()
        .create_lane(
            "side",
            h.session()
                .view("main")
                .get_leaf_id()
                .await
                .unwrap()
                .as_deref(),
        )
        .await
        .unwrap();
    let side = DurableService::bind_lane(&h, DurableActor::owner("owner").unwrap(), "side");
    assert!(matches!(
        side.execution("visible").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn native_terminal_visibility_not_owner_absence_controls_interruption() {
    let h = memory().await;
    prompt_anchor(&h, "run").await;
    let s = service(&h);
    assert_eq!(
        s.execution("run").await.unwrap().run.unwrap().status,
        RunStatus::Open
    );
    assert!(matches!(
        s.mutate(request(&s, "early", ControlAction::Recover, Some("run")).await)
            .await,
        Err(DurableError::Execution(ExecutionError::NotInterrupted))
    ));
    assert!(controls(&h).await.is_empty());
    finish(
        &h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        false,
    )
    .await;
    assert_eq!(
        s.execution("run").await.unwrap().run.unwrap().status,
        RunStatus::Interrupted
    );
    s.mutate(request(&s, "recover", ControlAction::Recover, Some("run")).await)
        .await
        .unwrap();
}

#[tokio::test]
async fn native_outcomes_are_exact_and_legacy_terminal_needs_only_branch_anchor() {
    for (outcome, code, status) in [
        (OperationOutcome::Completed, None, RunStatus::Completed),
        (OperationOutcome::Failed, None, RunStatus::Failed),
        (OperationOutcome::Declined, None, RunStatus::Declined),
        (OperationOutcome::Aborted, None, RunStatus::Aborted),
        (
            OperationOutcome::Failed,
            Some("interrupted"),
            RunStatus::Failed,
        ),
        (OperationOutcome::Aborted, Some("other"), RunStatus::Aborted),
        (
            OperationOutcome::Aborted,
            Some("interrupted"),
            RunStatus::Interrupted,
        ),
    ] {
        let h = memory().await;
        prompt_anchor(&h, "run").await;
        finish(&h, "run", outcome, code, false).await;
        assert_eq!(
            service(&h)
                .execution("run")
                .await
                .unwrap()
                .run
                .unwrap()
                .status,
            status
        );
    }
}

#[tokio::test]
async fn exact_tool_invocation_projection_and_synthetic_interruption_stays_unknown() {
    let h = memory().await;
    prompt_anchor(&h, "run").await;
    for i in 0..4 {
        tool(
            &h,
            "run",
            i,
            if i == 0 {
                ToolReplay::Safe
            } else {
                ToolReplay::Never
            },
        )
        .await;
        if i != 0 {
            append(
                &h,
                &format!("run-result-{i}"),
                result(
                    "reused-provider-id",
                    "write",
                    i != 1,
                    if i == 3 {
                        INTERRUPTED_TOOL_RESULT
                    } else {
                        "private tool result"
                    },
                ),
            )
            .await;
        }
    }
    // Same provider ID and tool name in another batch must not settle invocation zero.
    append(
        &h,
        "unrelated-result",
        result(
            "reused-provider-id",
            "write",
            false,
            "private unrelated result",
        ),
    )
    .await;
    finish(
        &h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        true,
    )
    .await;
    let s = service(&h);
    let observed = s.execution("run").await.unwrap();
    let run = observed.run.unwrap();
    assert_eq!(
        run.tools
            .iter()
            .map(|t| t.status.clone())
            .collect::<Vec<_>>(),
        vec![
            ToolStatus::Unknown,
            ToolStatus::Completed,
            ToolStatus::Failed,
            ToolStatus::Unknown
        ]
    );
    assert_eq!(run.tools[0].invocation_id, "run-invocation-0");
    assert_eq!(run.tools[0].replay, ReplayPolicy::Safe);
    assert!(run.tools[0].result_entry_id.is_none());
    assert_eq!(
        run.tools[3].result_entry_id.as_deref(),
        Some("run-result-3")
    );
    let serialized = serde_json::to_string(&run).unwrap();
    assert!(!serialized.contains("private"));
    assert!(!serialized.contains("effectiveArgs"));
    let before = facts(&h).await;
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    let outcome = s.mutate(req.clone()).await.unwrap();
    let recovery = outcome.snapshot.execution_control.recovery_request.unwrap();
    assert_eq!(recovery.run_id, "run");
    assert_eq!(recovery.started_seq, run.started_seq);
    assert_eq!(
        recovery.observed_history_revision,
        req.expected.history_revision
    );
    assert_eq!(
        recovery.unknown_invocation_ids,
        vec!["run-invocation-0", "run-invocation-3"]
    );
    assert_eq!(
        facts(&h).await,
        before,
        "replay-safe is intent only, never scheduling"
    );
    assert!(s.mutate(req).await.unwrap().replayed);
    assert!(matches!(
        s.mutate(request(&s, "again", ControlAction::Recover, Some("run")).await)
            .await,
        Err(DurableError::Execution(ExecutionError::AlreadyRequested))
    ));
}

#[tokio::test]
async fn reserved_result_identity_and_assistant_ordinal_corruption_fail_closed() {
    for mismatch in ["name", "call", "role", "ordinal"] {
        let h = memory().await;
        prompt_anchor(&h, "run").await;
        tool(&h, "run", 0, ToolReplay::Never).await;
        if mismatch == "ordinal" {
            h.session()
                .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                    base: base("bad-ordinal"),
                    run_id: "run".into(),
                    assistant_entry_id: "run-assistant-0".into(),
                    tool_index: 1,
                    tool_call_id: "reused-provider-id".into(),
                    tool_name: "write".into(),
                    effective_args: json!({}),
                    result_entry_id: "other-reserved".into(),
                    replay: ToolReplay::Never,
                }))
                .await
                .unwrap();
        } else {
            append(
                &h,
                "run-result-0",
                match mismatch {
                    "role" => AgentMessage::User(UserMessage::new("not a result", 0)),
                    "call" => result("wrong-call", "write", false, "result"),
                    _ => result("reused-provider-id", "wrong-name", false, "result"),
                },
            )
            .await;
        }
        finish(&h, "run", OperationOutcome::Completed, None, true).await;
        let s = service(&h);
        assert!(
            matches!(
                s.execution("run").await,
                Err(DurableError::InvalidJournal(_))
            ),
            "{mismatch}"
        );
        assert!(
            matches!(
                s.mutate(request(&s, "forget", ControlAction::ForgetRun, Some("run")).await)
                    .await,
                Err(DurableError::InvalidJournal(_))
            ),
            "{mismatch}"
        );
        assert!(controls(&h).await.is_empty());
    }
}

#[tokio::test]
async fn native_invocation_budget_is_hard_64_not_truncation() {
    let h = memory().await;
    prompt_anchor(&h, "run").await;
    for i in 0..64 {
        tool(&h, "run", i, ToolReplay::Never).await;
    }
    assert_eq!(
        service(&h)
            .execution("run")
            .await
            .unwrap()
            .run
            .unwrap()
            .tools
            .len(),
        64
    );
    tool(&h, "run", 64, ToolReplay::Never).await;
    assert!(matches!(
        service(&h).execution("run").await,
        Err(DurableError::LimitExceeded)
    ));
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn immutable_branch_anchor_excludes_sibling_and_off_branch_reserved_results() {
    let h = memory().await;
    prompt_anchor(&h, "run").await;
    tool(&h, "run", 0, ToolReplay::Never).await;
    h.session()
        .storage()
        .create_lane("sibling", Some("run-assistant-0"))
        .await
        .unwrap();
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: "run-result-0".into(),
                kind: ProvisionedKind::Message {
                    message: result("reused-provider-id", "write", false, "result"),
                    terminate: None,
                },
            },
            "sibling",
        )
        .await
        .unwrap();
    finish(
        &h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        false,
    )
    .await;
    assert_eq!(
        service(&h)
            .execution("run")
            .await
            .unwrap()
            .run
            .unwrap()
            .tools[0]
            .status,
        ToolStatus::Unknown
    );
    h.session().move_lane("main", None).await.unwrap();
    assert!(matches!(
        service(&h).execution("run").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
}

#[tokio::test]
async fn owner_only_execution_actions_and_run_id_validation_do_not_write() {
    let h = memory().await;
    interrupted(&h, "run").await;
    let owner = service(&h);
    let child = DurableService::bind(&h, DurableActor::child("child").unwrap());
    for action in [ControlAction::Recover, ControlAction::ForgetRun] {
        assert!(matches!(
            child
                .mutate(request(&child, "child", action.clone(), Some("run")).await)
                .await,
            Err(DurableError::PermissionDenied)
        ));
        assert!(matches!(
            owner
                .mutate(request(&owner, "missing-id", action, None).await)
                .await,
            Err(DurableError::InvalidRequest(_))
        ));
    }
    assert!(matches!(
        owner
            .mutate(request(&owner, "extra-id", ControlAction::Reset, Some("run")).await)
            .await,
        Err(DurableError::InvalidRequest(_))
    ));
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn recovery_and_forget_preserve_workflow_queue_and_native_facts() {
    let h = memory().await;
    let s = service(&h);
    s.mutate(request(&s, "start", serde_json::from_value(json!({"action":"start", "name":"workflow", "steps":[{"id":"step","title":"Test"}]})).unwrap(), None).await).await.unwrap();
    s.mutate(
        request(
            &s,
            "enqueue",
            serde_json::from_value(json!({"action":"enqueue", "text":"manual work"})).unwrap(),
            None,
        )
        .await,
    )
    .await
    .unwrap();
    interrupted(&h, "run").await;
    let before = s.snapshot().await.unwrap();
    let native = facts(&h).await;
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    let recovered = s.mutate(req.clone()).await.unwrap();
    assert_eq!(
        recovered.snapshot.scope.control_revision,
        before.scope.control_revision + 1
    );
    let forgotten = s
        .mutate(request(&s, "forget", ControlAction::ForgetRun, Some("run")).await)
        .await
        .unwrap();
    assert_eq!(forgotten.snapshot.state.workflow, before.state.workflow);
    assert_eq!(forgotten.snapshot.state.queue, before.state.queue);
    assert_eq!(
        forgotten.snapshot.scope.control_revision,
        before.scope.control_revision + 2
    );
    assert_eq!(
        forgotten.snapshot.execution_control.forgotten_run_ids,
        vec!["run"]
    );
    assert!(forgotten
        .snapshot
        .execution_control
        .recovery_request
        .is_none());
    assert!(s.execution("run").await.unwrap().run.is_none());
    assert_eq!(facts(&h).await, native);
    assert!(s.request_outcome("recover").await.unwrap().is_some());
    assert!(
        matches!(s.mutate(req).await, Err(DurableError::Conflict)),
        "obsolete request cannot be replayed across a later receipt"
    );
}

#[tokio::test]
async fn recovery_lost_ack_replays_one_receipt_and_run_id_is_dedup_identity() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let s = service(&h);
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    let before = facts(&h).await;
    p.mode.store(2, Ordering::SeqCst);
    assert!(matches!(
        s.mutate(req.clone()).await,
        Err(DurableError::CommitUncertain(_))
    ));
    let receipt = s.request_outcome("recover").await.unwrap().unwrap();
    assert_eq!(receipt.run_id.as_deref(), Some("run"));
    assert_eq!(s.mutate(req.clone()).await.unwrap().receipt, receipt);
    assert!(s.mutate(req.clone()).await.unwrap().replayed);
    let mut changed = req;
    changed.run_id = Some("other-run".into());
    assert!(matches!(
        s.mutate(changed).await,
        Err(DurableError::Conflict)
    ));
    assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    assert_eq!(controls(&h).await.len(), 1);
    assert_eq!(facts(&h).await, before);
}

#[tokio::test]
async fn recovery_conditional_append_barrier_and_dropped_caller_remain_owned() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let s = service(&h);
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    p.mode.store(4, Ordering::SeqCst); // Commit is visible, ACK remains blocked.
    let writer = tokio::spawn({
        let s = s.clone();
        let req = req.clone();
        async move { s.mutate(req).await }
    });
    bounded(p.committed.notified()).await;
    writer.abort();
    let reader = tokio::spawn({
        let s = s.clone();
        async move { s.execution("run").await }
    });
    tokio::task::yield_now().await;
    assert!(
        !reader.is_finished(),
        "reader must not cross the owned mutation's ACK barrier"
    );
    p.release.notify_one();
    let observed = bounded(reader).await.unwrap().unwrap();
    assert!(observed.control.recovery_request.is_some());
    assert_eq!(observed.run.unwrap().status, RunStatus::Interrupted);
    assert!(s.mutate(req).await.unwrap().replayed);
    assert_eq!(p.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn native_revision_race_at_recovery_append_is_cas_conflict_not_partial_control() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let s = service(&h);
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    p.mode.store(3, Ordering::SeqCst);
    let writer = tokio::spawn({
        let s = s.clone();
        async move { s.mutate(req).await }
    });
    bounded(p.entered.notified()).await;
    // An independent native writer advances history before the conditional control append.
    append(
        &h,
        "external-entry",
        AgentMessage::User(UserMessage::new("external", 0)),
    )
    .await;
    p.release.notify_one();
    assert!(matches!(
        bounded(writer).await.unwrap(),
        Err(DurableError::CommitUncertain(_))
    ));
    assert_eq!(p.commits.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
    assert!(s
        .snapshot()
        .await
        .unwrap()
        .execution_control
        .recovery_request
        .is_none());
}

#[tokio::test]
async fn failed_observation_and_revision_queries_never_append_recovery() {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let s = service(&h);
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    for revision in [false, true] {
        p.fail_reads.store(!revision, Ordering::SeqCst);
        p.fail_revision.store(revision, Ordering::SeqCst);
        assert!(matches!(
            s.execution("run").await,
            Err(DurableError::ReadFailure(_))
        ));
        assert!(matches!(
            s.mutate(req.clone()).await,
            Err(DurableError::ReadFailure(_))
        ));
    }
    p.fail_reads.store(false, Ordering::SeqCst);
    p.fail_revision.store(false, Ordering::SeqCst);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
}

async fn persisted_first(h: &AgentHarness) {
    prompt_anchor(h, "run").await;
    tool(h, "run", 0, ToolReplay::Safe).await;
    finish(
        h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        true,
    )
    .await;
    let s = service(h);
    s.mutate(request(&s, "recover", ControlAction::Recover, Some("run")).await)
        .await
        .unwrap();
}
async fn persisted_recover_then_forget(h: &AgentHarness) {
    let s = service(h);
    let before = facts(h).await;
    let projection = s.execution("run").await.unwrap();
    assert_eq!(projection.run.unwrap().status, RunStatus::Interrupted);
    assert_eq!(
        projection
            .control
            .recovery_request
            .unwrap()
            .unknown_invocation_ids,
        vec!["run-invocation-0"]
    );
    assert!(s.request_outcome("recover").await.unwrap().is_some());
    s.mutate(request(&s, "forget", ControlAction::ForgetRun, Some("run")).await)
        .await
        .unwrap();
    assert_eq!(
        facts(h).await,
        before,
        "reopening must not execute/replay a closed run"
    );
}
async fn persisted_hidden(h: &AgentHarness) {
    let s = service(h);
    assert!(s.execution("run").await.unwrap().run.is_none());
    let snapshot = s.snapshot().await.unwrap();
    assert_eq!(snapshot.execution_control.forgotten_run_ids, vec!["run"]);
    assert!(snapshot.execution_control.recovery_request.is_none());
    assert!(s.request_outcome("recover").await.unwrap().is_some());
    assert!(s.request_outcome("forget").await.unwrap().is_some());
    assert_eq!(facts(h).await.len(), 3);
}

#[tokio::test]
async fn jsonl_reopen_retains_interrupted_recovery_then_forget_without_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let h = harness(Arc::new(
        JsonlSessionStorage::create(
            fs.clone(),
            "execution.jsonl",
            JsonlV4Header::new("execution".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    ))
    .await;
    persisted_first(&h).await;
    h.drain_session().await;
    drop(h);
    let h = harness(Arc::new(
        JsonlSessionStorage::load(
            fs.clone(),
            "execution.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    ))
    .await;
    persisted_recover_then_forget(&h).await;
    h.drain_session().await;
    drop(h);
    let h = harness(Arc::new(
        JsonlSessionStorage::load(
            fs,
            "execution.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    ))
    .await;
    persisted_hidden(&h).await;
}

#[tokio::test]
async fn sqlite_reopen_retains_interrupted_recovery_then_forget_without_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("execution.db");
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let store = repo.create(&SessionCreateOptions::default()).await.unwrap();
    let metadata = store.metadata();
    let h = harness(Arc::new(store)).await;
    persisted_first(&h).await;
    h.drain_session().await;
    drop(h);
    drop(repo);
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
    persisted_recover_then_forget(&h).await;
    h.drain_session().await;
    drop(h);
    drop(repo);
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
    persisted_hidden(&h).await;
}

// Independent native-observation fault boundary; support Probe gates only control appends.
mod native_probe {
    use super::*;
    use rpi_harness::error::{SessionError, SessionResult};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::Notify;
    pub struct NativeProbe {
        pub inner: Arc<dyn SessionStorage>,
        pub fail_records: std::sync::atomic::AtomicBool,
        pub drift: std::sync::atomic::AtomicBool,
        pub queried: std::sync::atomic::AtomicBool,
        pub block_terminal: std::sync::atomic::AtomicBool,
        pub block_records: AtomicBool,
        pub mode: AtomicUsize,
        pub fail_reads: AtomicBool,
        pub fail_revision: AtomicBool,
        pub attempts: AtomicUsize,
        pub commits: AtomicUsize,
        pub entered: Notify,
        pub committed: Notify,
        pub release: Notify,
    }
    impl NativeProbe {
        pub fn new(inner: Arc<dyn SessionStorage>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                fail_records: AtomicBool::new(false),
                drift: AtomicBool::new(false),
                queried: AtomicBool::new(false),
                block_terminal: AtomicBool::new(false),
                block_records: AtomicBool::new(false),
                mode: AtomicUsize::new(0),
                fail_reads: AtomicBool::new(false),
                fail_revision: AtomicBool::new(false),
                attempts: AtomicUsize::new(0),
                commits: AtomicUsize::new(0),
                entered: Notify::new(),
                committed: Notify::new(),
                release: Notify::new(),
            })
        }
    }
    #[async_trait::async_trait]
    impl SessionStorage for NativeProbe {
        fn metadata(&self) -> SessionMetadata {
            self.inner.metadata()
        }
        fn is_ephemeral(&self) -> bool {
            self.inner.is_ephemeral()
        }
        async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
            self.inner.get_lanes().await
        }
        async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
            self.inner.create_lane(lane, at).await
        }
        async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
            self.inner.move_lane(lane, to).await
        }
        async fn append_progress(
            &self,
            records: Vec<AssistantFrameRecord>,
        ) -> SessionResult<Vec<AssistantFrameRecord>> {
            self.inner.append_progress(records).await
        }
        async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
            self.inner.read_progress(run_id).await
        }
        async fn prune_completed_progress(&self) -> SessionResult<usize> {
            self.inner.prune_completed_progress().await
        }
        async fn history_revision(&self) -> SessionResult<u64> {
            if self.fail_revision.load(Ordering::SeqCst) {
                return Err(SessionError::storage("injected revision failure"));
            }
            let revision = self.inner.history_revision().await?;
            Ok(revision
                + u64::from(
                    self.drift.load(Ordering::SeqCst) && self.queried.load(Ordering::SeqCst),
                ))
        }
        async fn append_entry_if_revision(
            &self,
            entry: ProvisionedEntry,
            lane: &str,
            expected: u64,
        ) -> SessionResult<Entry> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let mode = self.mode.swap(0, Ordering::SeqCst);
            self.entered.notify_one();
            if mode == 1 {
                return Err(SessionError::storage("before commit"));
            }
            if mode == 3 {
                self.release.notified().await;
            }
            let result = self
                .inner
                .append_entry_if_revision(entry, lane, expected)
                .await?;
            self.commits.fetch_add(1, Ordering::SeqCst);
            self.committed.notify_one();
            if mode == 4 {
                self.release.notified().await;
            }
            if mode == 2 {
                return Err(SessionError::storage("lost commit ACK"));
            }
            Ok(result)
        }
        async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
            self.inner.append_entry(entry, lane).await
        }
        async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
            if matches!(&record, LaneRecord::OperationFinished(_))
                && self.block_terminal.swap(false, Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.append_record(record).await
        }
        async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
            self.inner.get_entry(id).await
        }
        async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
            self.inner.find_entries(query).await
        }
        async fn find_entries_on_branch(
            &self,
            query: &EntryQuery,
            bounds: &BranchBounds,
            start: &str,
        ) -> SessionResult<Vec<Entry>> {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(SessionError::storage("injected branch read failure"));
            }
            self.inner
                .find_entries_on_branch(query, bounds, start)
                .await
        }

        async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
            if self.block_records.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.queried.store(true, Ordering::SeqCst);
            if self.fail_records.load(Ordering::SeqCst) {
                return Err(SessionError::storage(
                    "injected native record query failure",
                ));
            }
            self.inner.find_records(query).await
        }
        async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage> {
            self.inner.record_page(query).await
        }
        async fn find_open_operations(
            &self,
            lane: &str,
            limit: Option<usize>,
        ) -> SessionResult<Vec<OperationStartedRecord>> {
            self.inner.find_open_operations(lane, limit).await
        }
        async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
            self.inner.get_log(options).await
        }
        async fn get_name(&self) -> SessionResult<Option<String>> {
            self.inner.get_name().await
        }
        async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
            self.inner.set_name(name).await
        }
        async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
            self.inner.get_label(id).await
        }
        async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
            self.inner.set_label(id, label).await
        }
        async fn get_stats(&self) -> SessionResult<SessionStats> {
            self.inner.get_stats().await
        }
        async fn drain(&self) {
            self.inner.drain().await
        }
    }
}

#[tokio::test]
async fn uncommitted_native_terminal_is_not_interrupted_or_recoverable() {
    let p = native_probe::NativeProbe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    prompt_anchor(&h, "run").await;
    p.block_terminal.store(true, Ordering::SeqCst);
    let writer = tokio::spawn({
        let h = h.clone();
        async move {
            finish(
                &h,
                "run",
                OperationOutcome::Aborted,
                Some("interrupted"),
                false,
            )
            .await
        }
    });
    bounded(p.entered.notified()).await;
    let s = service(&h);
    assert_eq!(
        s.execution("run").await.unwrap().run.unwrap().status,
        RunStatus::Open
    );
    assert!(matches!(
        s.mutate(request(&s, "premature", ControlAction::Recover, Some("run")).await)
            .await,
        Err(DurableError::Execution(ExecutionError::NotInterrupted))
    ));
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    p.release.notify_one();
    bounded(writer).await.unwrap();
    assert_eq!(
        s.execution("run").await.unwrap().run.unwrap().status,
        RunStatus::Interrupted
    );
}

#[tokio::test]
async fn native_query_failure_and_observation_end_revision_conflict_do_not_write() {
    let p = native_probe::NativeProbe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let s = service(&h);
    let req = request(&s, "recover", ControlAction::Recover, Some("run")).await;
    p.fail_records.store(true, Ordering::SeqCst);
    assert!(matches!(
        s.execution("run").await,
        Err(DurableError::ReadFailure(_))
    ));
    assert!(matches!(
        s.mutate(req.clone()).await,
        Err(DurableError::ReadFailure(_))
    ));
    p.fail_records.store(false, Ordering::SeqCst);
    p.queried.store(false, Ordering::SeqCst);
    p.drift.store(true, Ordering::SeqCst);
    assert!(matches!(
        s.execution("run").await,
        Err(DurableError::Conflict)
    ));
    p.queried.store(false, Ordering::SeqCst);
    assert!(matches!(s.mutate(req).await, Err(DurableError::Conflict)));
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    p.drift.store(false, Ordering::SeqCst);
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn tool_result_must_follow_start_and_precede_terminal_on_selected_branch() {
    for early in [true, false] {
        let h = memory().await;
        prompt_anchor(&h, "run").await;
        if early {
            append(
                &h,
                "run-result-0",
                result("reused-provider-id", "write", false, "early"),
            )
            .await;
        }
        tool(&h, "run", 0, ToolReplay::Never).await;
        finish(&h, "run", OperationOutcome::Completed, None, false).await;
        if !early {
            append(
                &h,
                "run-result-0",
                result("reused-provider-id", "write", false, "late"),
            )
            .await;
        }
        assert!(matches!(
            service(&h).execution("run").await,
            Err(DurableError::InvalidJournal(_))
        ));
        assert!(controls(&h).await.is_empty());
    }
}

#[tokio::test]
async fn source_leaf_and_atomic_prompt_adjacency_are_required_without_other_proof() {
    let h = memory().await;
    append(
        &h,
        "source",
        AgentMessage::User(UserMessage::new("source", 0)),
    )
    .await;
    start(&h, "run").await;
    // A native record between admission and the first message breaks the atomic admission proof.
    h.session()
        .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
            base: base("abort-request"),
            run_id: "run".into(),
        }))
        .await
        .unwrap();
    append(
        &h,
        "later-prompt",
        AgentMessage::User(UserMessage::new("not atomic", 0)),
    )
    .await;
    finish(
        &h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        false,
    )
    .await;
    assert!(matches!(
        service(&h).execution("run").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
    // Even a terminal entry reference cannot prove a run from a different source branch.
    h.session().move_lane("main", None).await.unwrap();
    append(
        &h,
        "sibling",
        AgentMessage::User(UserMessage::new("sibling", 0)),
    )
    .await;
    assert!(matches!(
        service(&h).execution("run").await,
        Err(DurableError::ExecutionUnavailable(_))
    ));
}

#[tokio::test]
async fn forgotten_run_ids_and_recovery_target_must_reconcile_receipt_ledger() {
    for corrupt_hidden in [false, true] {
        let h = memory().await;
        interrupted(&h, "run").await;
        let s = service(&h);
        s.mutate(request(&s, "recover", ControlAction::Recover, Some("run")).await)
            .await
            .unwrap();
        let control = controls(&h).await.pop().unwrap();
        let Entry::Custom(control) = control else {
            panic!("control record")
        };
        let mut data = control.data.unwrap();
        data["receipts"][0]["entryId"] = json!("forged-control");
        if corrupt_hidden {
            data["executionControl"]["forgottenRunIds"] = json!(["unreceipted-run"]);
        } else {
            data["executionControl"]["recoveryRequest"]["runId"] = json!("different-run");
        }
        h.session()
            .append_entry(
                ProvisionedEntry {
                    id: "forged-control".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: "rpi.durable.control.v1".into(),
                        data: Some(data),
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
            s.execution("run").await,
            Err(DurableError::InvalidJournal(_))
        ));
        assert_eq!(controls(&h).await.len(), 2);
    }
}

#[tokio::test]
async fn terminal_reference_or_exact_tool_assistant_can_anchor_nonadjacent_admission() {
    for tool_anchor in [false, true] {
        let h = memory().await;
        start(&h, "run").await;
        h.session()
            .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                base: base("intervening-record"),
                run_id: "run".into(),
            }))
            .await
            .unwrap();
        if tool_anchor {
            tool(&h, "run", 0, ToolReplay::Safe).await;
        } else {
            append(
                &h,
                "terminal-leaf",
                AgentMessage::User(UserMessage::new("terminal anchor", 0)),
            )
            .await;
        }
        finish(
            &h,
            "run",
            OperationOutcome::Aborted,
            Some("interrupted"),
            !tool_anchor,
        )
        .await;
        let run = service(&h).execution("run").await.unwrap().run.unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(run.tools.len(), usize::from(tool_anchor));
    }
}

#[tokio::test]
async fn corrupt_duplicate_native_invocation_positions_are_invalid_journal_not_domain_error() {
    let h = memory().await;
    prompt_anchor(&h, "run").await;
    tool(&h, "run", 0, ToolReplay::Never).await;
    // Distinct native invocation IDs illegally claim the same assistant ordinal.
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("duplicate-position"),
            run_id: "run".into(),
            assistant_entry_id: "run-assistant-0".into(),
            tool_index: 0,
            tool_call_id: "reused-provider-id".into(),
            tool_name: "write".into(),
            effective_args: json!({}),
            result_entry_id: "duplicate-reserved-result".into(),
            replay: ToolReplay::Never,
        }))
        .await
        .unwrap();
    finish(
        &h,
        "run",
        OperationOutcome::Aborted,
        Some("interrupted"),
        false,
    )
    .await;
    let s = service(&h);
    let error = s.execution("run").await.unwrap_err();
    assert!(
        matches!(error, DurableError::InvalidJournal(_)),
        "corrupt native facts must be InvalidJournal, got {error:?}"
    );
    assert!(controls(&h).await.is_empty());
}

#[tokio::test]
async fn forgetting_another_run_preserves_selected_recovery_request() {
    let h = memory().await;
    interrupted(&h, "first").await;
    interrupted(&h, "second").await;
    let s = service(&h);
    let recovered = s
        .mutate(request(&s, "recover-first", ControlAction::Recover, Some("first")).await)
        .await
        .unwrap();
    let expected = recovered
        .snapshot
        .execution_control
        .recovery_request
        .unwrap();
    let forgotten = s
        .mutate(
            request(
                &s,
                "forget-second",
                ControlAction::ForgetRun,
                Some("second"),
            )
            .await,
        )
        .await
        .unwrap();
    assert_eq!(
        forgotten.snapshot.execution_control.recovery_request,
        Some(expected)
    );
    assert_eq!(
        forgotten.snapshot.execution_control.forgotten_run_ids,
        vec!["second"]
    );
    assert!(s.execution("second").await.unwrap().run.is_none());
    assert_eq!(
        s.execution("first").await.unwrap().run.unwrap().status,
        RunStatus::Interrupted
    );
}

async fn finish_with_identity(h: &AgentHarness, identity: RunResultIdentity) {
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("run-finished"),
            run_id: "run".into(),
            outcome: OperationOutcome::Aborted,
            error: Some(OperationError {
                code: "interrupted".into(),
                message: "interrupted".into(),
            }),
            result: Some(identity),
        }))
        .await
        .unwrap();
}

async fn assert_invalid_observation_and_recovery_without_writes(h: &AgentHarness) {
    let s = service(h);
    let req = request(&s, "recover-corrupt", ControlAction::Recover, Some("run")).await;
    let before = facts(h).await;
    let revision = h.session().storage().history_revision().await.unwrap();
    let error = s.execution("run").await.unwrap_err();
    assert!(
        matches!(error, DurableError::InvalidJournal(_)),
        "{error:?}"
    );
    let error = s.mutate(req).await.unwrap_err();
    assert!(
        matches!(error, DurableError::InvalidJournal(_)),
        "{error:?}"
    );
    assert_eq!(facts(h).await, before);
    assert_eq!(
        h.session().storage().history_revision().await.unwrap(),
        revision
    );
    assert!(controls(h).await.is_empty());
}

#[tokio::test]
async fn future_source_leaf_on_current_branch_is_invalid_not_a_run_anchor() {
    let h = memory().await;
    // Trusted storage permits a forward reference; branch membership alone is insufficient.
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: Some("future-source".into()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    append(
        &h,
        "assistant-anchor",
        AgentMessage::Assistant(Box::new(faux_assistant_message("anchor", StopReason::Stop))),
    )
    .await;
    finish_with_identity(
        &h,
        RunResultIdentity {
            leaf_id: "assistant-anchor".into(),
            final_entry_id: Some("assistant-anchor".into()),
            assistant_entry_id: Some("assistant-anchor".into()),
        },
    )
    .await;
    append(
        &h,
        "future-source",
        AgentMessage::User(UserMessage::new("future source", 0)),
    )
    .await;
    assert_invalid_observation_and_recovery_without_writes(&h).await;
}

#[tokio::test]
async fn terminal_references_must_preserve_assistant_final_and_leaf_order() {
    for assistant_after_final in [true, false] {
        let h = memory().await;
        prompt_anchor(&h, "run").await;
        let order = if assistant_after_final {
            ["final", "assistant", "terminal-leaf"]
        } else {
            ["assistant", "terminal-leaf", "final"]
        };
        for id in order {
            let message = if id == "assistant" {
                AgentMessage::Assistant(Box::new(faux_assistant_message(
                    "answer",
                    StopReason::Stop,
                )))
            } else {
                AgentMessage::User(UserMessage::new(id, 0))
            };
            append(&h, id, message).await;
        }
        // All references precede the terminal record and belong to the current branch.
        finish_with_identity(
            &h,
            RunResultIdentity {
                leaf_id: "terminal-leaf".into(),
                final_entry_id: Some("final".into()),
                assistant_entry_id: Some("assistant".into()),
            },
        )
        .await;
        assert_invalid_observation_and_recovery_without_writes(&h).await;
    }
}

#[tokio::test]
async fn tool_assistant_and_exact_result_cannot_follow_recorded_terminal_leaf() {
    for late_assistant in [false, true] {
        let h = memory().await;
        prompt_anchor(&h, "run").await;
        if !late_assistant {
            tool(&h, "run", 0, ToolReplay::Never).await;
        }
        append(
            &h,
            "terminal-leaf",
            AgentMessage::User(UserMessage::new("terminal snapshot", 0)),
        )
        .await;
        if late_assistant {
            tool(&h, "run", 0, ToolReplay::Never).await;
        } else {
            // Exact reserved identity and successful payload must never become known success.
            append(
                &h,
                "run-result-0",
                result("reused-provider-id", "write", false, "late success"),
            )
            .await;
        }
        finish_with_identity(
            &h,
            RunResultIdentity {
                leaf_id: "terminal-leaf".into(),
                final_entry_id: None,
                assistant_entry_id: None,
            },
        )
        .await;
        assert_invalid_observation_and_recovery_without_writes(&h).await;
    }
}

#[tokio::test]
async fn execution_observation_rebind_during_native_query_conflicts_without_mixed_snapshot() {
    let p = native_probe::NativeProbe::new(AgentHarnessOptions::default().session.storage());
    let h = harness(p.clone()).await;
    interrupted(&h, "run").await;
    let replacement = memory().await;
    prompt_anchor(&replacement, "run").await;
    finish(&replacement, "run", OperationOutcome::Completed, None, true).await;
    let s = service(&h);
    let old_scope = s.snapshot().await.unwrap().scope;
    let old_revision = p.inner.history_revision().await.unwrap();
    p.block_records.store(true, Ordering::SeqCst);
    let reader = tokio::spawn({
        let s = s.clone();
        async move { s.execution("run").await }
    });
    bounded(p.entered.notified()).await;
    // This clone shares the harness binding; the pinned native read remains on p.inner.
    h.session().set_storage(replacement.session().storage());
    p.release.notify_one();
    assert!(matches!(
        bounded(reader).await.unwrap(),
        Err(DurableError::Conflict)
    ));
    let current = s.execution("run").await.unwrap();
    assert_ne!(current.scope.binding_token, old_scope.binding_token);
    assert_eq!(current.run.unwrap().status, RunStatus::Completed);
    assert_eq!(p.inner.history_revision().await.unwrap(), old_revision);
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert!(controls(&h).await.is_empty());
    assert!(controls(&replacement).await.is_empty());
}

#[tokio::test]
async fn service_forgetting_multiple_runs_accumulates_ids_and_keeps_native_audit() {
    let h = memory().await;
    for id in ["first", "second", "third"] {
        interrupted(&h, id).await;
    }
    let s = service(&h);
    let before = facts(&h).await;
    let mut forgotten = Vec::new();
    for id in ["first", "second", "third"] {
        forgotten.push(id.to_owned());
        let outcome = s
            .mutate(
                request(
                    &s,
                    &format!("forget-{id}"),
                    ControlAction::ForgetRun,
                    Some(id),
                )
                .await,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.snapshot.execution_control.forgotten_run_ids,
            forgotten
        );
        for hidden in &forgotten {
            assert!(s.execution(hidden).await.unwrap().run.is_none());
        }
    }
    assert_eq!(facts(&h).await, before);
    assert_eq!(controls(&h).await.len(), 3);
}
