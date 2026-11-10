//! Opt-in native-journal admission contracts. Synthetic Memory/JSONL and Faux
//! fixtures only; no external provider or credential/configuration lookup.
use rpi_agent::{
    AgentError, AgentMessage, AgentTool, AgentToolResult, CollectorEmitter, ToolResultPartial,
};
use rpi_ai::types::{Schema, Tool, UserMessage};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    durable::{DurableActor, DurableError, DurableMutation, DurableOutcome, DurableService},
    session::{types::*, Session},
    types::{AgentHarnessOptions, HarnessTool},
};
use serde_json::{json, Value};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[path = "durable_session_policy/automatic.rs"]
mod automatic;
#[path = "durable_session_policy/compaction.rs"]
mod compaction;
#[path = "durable_session_policy/navigation.rs"]
mod navigation;
#[path = "support/durable_control_probe.rs"]
mod probe;
#[path = "durable_session_policy/resume.rs"]
mod resume;

const CONTROL: &str = "rpi.durable.control.v1";

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("session-policy operation timed out")
}
fn memory() -> Session {
    use rpi_harness::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: format!("native-policy-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst)),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )),
        None,
    )
}
fn options(session: Session) -> AgentHarnessOptions {
    AgentHarnessOptions {
        session,
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    }
}
async fn strict(session: Session) -> AgentHarness {
    bounded(AgentHarness::create(options(session)))
        .await
        .unwrap()
}
fn service(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner("policy-owner").unwrap())
}
async fn request(s: &DurableService, id: &str, action: Value) -> DurableMutation {
    DurableMutation {
        request_id: id.into(),
        expected: s.snapshot().await.unwrap().scope,
        action: serde_json::from_value(action).unwrap(),
        run_id: None,
    }
}
async fn apply(s: &DurableService, id: &str, action: Value) -> DurableOutcome {
    bounded(s.mutate(request(s, id, action).await))
        .await
        .unwrap()
}
async fn seeded() -> AgentHarness {
    let h = strict(memory()).await;
    let s = service(&h);
    apply(
        &s,
        "start",
        json!({"action":"start", "name":"policy", "steps":[{"id":"one", "title":"Keep state"}]}),
    )
    .await;
    apply(
        &s,
        "enqueue",
        json!({"action":"enqueue", "text":"Keep durable queue"}),
    )
    .await;
    h
}
async fn image(session: &Session) -> Value {
    let storage = session.storage();
    json!({
        "entries": storage.find_entries(&EntryQuery::default()).await.unwrap(),
        "records": storage.find_records(&RecordQuery::default()).await.unwrap(),
        "lanes": storage.get_lanes().await.unwrap(),
        "revision": storage.history_revision().await.unwrap(),
        "progress": storage.read_progress("completed-run").await.unwrap(),
    })
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
async fn open_operation(session: &Session) {
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("open-run"),
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
}
// A reclaimable frame makes validation-before-pruning observable without a
// new storage wrapper: rejected admission must not retire this payload.
async fn prune_sentinel(session: &Session) {
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("completed-run"),
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
    session
        .storage()
        .append_progress(vec![AssistantFrameRecord {
            base: base("completed-frame"),
            run_id: "completed-run".into(),
            stream_index: 0,
            op: AssistantFrameOp::Append,
            frame: Some(json!({"payload":"must survive rejected admission"})),
        }])
        .await
        .unwrap();
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("completed-terminal"),
            run_id: "completed-run".into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
    assert!(!session
        .storage()
        .read_progress("completed-run")
        .await
        .unwrap()
        .is_empty());
}
async fn custom(session: &Session, lane: &str, id: &str, kind: &str, data: Option<Value>) {
    session
        .storage()
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Custom {
                    custom_type: kind.into(),
                    data,
                },
            },
            lane,
        )
        .await
        .unwrap();
}

struct NoCalls {
    schema: Tool,
    calls: AtomicUsize,
}
impl NoCalls {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            schema: Tool {
                name: "policy-effect".into(),
                description: "Must not run during admission".into(),
                parameters: Schema::new(json!({"type":"object"})),
                constrained_sampling: None,
            },
            calls: AtomicUsize::new(0),
        })
    }
}
#[async_trait::async_trait]
impl AgentTool for NoCalls {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "policy-effect"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(AgentError::tool("admission must never invoke a tool"))
    }
}

#[tokio::test]
async fn required_create_rejects_both_legacy_types_before_prune_or_recovery() {
    for (lane, kind) in [
        ("main", "durable-state"),
        ("main", "rpi-durable-runtime"),
        ("side", "durable-state"),
        ("side", "rpi-durable-runtime"),
    ] {
        let session = memory();
        if lane == "side" {
            session.create_lane("side", None).await.unwrap();
        }
        custom(
            &session,
            lane,
            "legacy",
            kind,
            Some(json!({"opaque":"legacy"})),
        )
        .await;
        prune_sentinel(&session).await;
        open_operation(&session).await;
        let before = image(&session).await;
        let tool = NoCalls::new();
        let mut opts = options(session.clone());
        opts.tools = vec![HarnessTool::new(tool.clone())];
        assert!(bounded(AgentHarness::create(opts)).await.is_err(), "{kind}");
        assert_eq!(
            image(&session).await,
            before,
            "{kind}: admission wrote or pruned storage"
        );
        assert_eq!(
            session
                .storage()
                .find_open_operations("main", None)
                .await
                .unwrap()
                .len(),
            1
        );
        let terminals = session
            .find_records(&RecordQuery {
                record_type: Some("operation_finished"),
                run_id: Some("open-run".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            terminals.is_empty(),
            "must not append an Interrupted terminal"
        );
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn rejected_targets_preserve_binding_queues_receipts_and_child_scope_on_every_lane() {
    let seed = seeded().await;
    let payload = seed
        .session()
        .storage()
        .find_entries(&EntryQuery {
            entry_type: Some("custom"),
            custom_type: Some(CONTROL.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry {
            Entry::Custom(c) if c.data.as_ref().unwrap()["state"]["revision"] == json!(1) => c.data,
            _ => None,
        })
        .unwrap();
    let h = seeded().await;
    h.follow_up(AgentMessage::User(UserMessage::new(
        "old queued message",
        0,
    )))
    .await
    .unwrap();
    let s = service(&h);
    let old = s.snapshot().await.unwrap();
    let receipt = s.request_outcome("enqueue").await.unwrap();
    let queued = h.queued_messages().await.unwrap();
    // create_child constructs only a local harness/scope; it does not start a run.
    let child = h
        .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
        .await
        .unwrap();
    for lane in ["main", "side"] {
        for damage in [
            "durable-state",
            "rpi-durable-runtime",
            "corrupt",
            "foreign-receipt",
        ] {
            let target = memory();
            if lane == "side" {
                target.create_lane("side", None).await.unwrap();
            }
            let (kind, data) = match damage {
                "corrupt" => (CONTROL, Some(json!({"format":"broken"}))),
                "foreign-receipt" => {
                    let mut data = payload.clone();
                    data["receipts"][0]["entryId"] = json!("bad-target");
                    data["receipts"][0]["expected"]["lane"] = json!(lane);
                    // Leave only the session identity invalid.
                    data["receipts"][0]["expected"]["sessionId"] = json!(seed.session_id());
                    (CONTROL, Some(data))
                }
                legacy => (legacy, Some(json!("opaque"))),
            };
            custom(&target, lane, "bad-target", kind, data).await;
            prune_sentinel(&target).await;
            let before_target = image(&target).await;
            let before_old = image(h.session()).await;
            assert!(
                bounded(h.set_session(target.clone())).await.is_err(),
                "{lane}/{damage}"
            );
            assert_eq!(
                image(&target).await,
                before_target,
                "{lane}/{damage}: target pruned"
            );
            assert_eq!(image(h.session()).await, before_old);
            assert_eq!(
                s.snapshot().await.unwrap(),
                old,
                "{lane}/{damage}: binding changed"
            );
            assert_eq!(s.request_outcome("enqueue").await.unwrap(), receipt);
            assert_eq!(h.queued_messages().await.unwrap(), queued);
            assert!(
                !child.scope.is_cancelled(),
                "{lane}/{damage}: old child cancelled"
            );
        }
    }
}

#[tokio::test]
async fn valid_native_same_identity_reopens_and_switches_with_full_state_and_receipts() {
    let source = seeded().await;
    source
        .follow_up(AgentMessage::User(UserMessage::new("target queue", 0)))
        .await
        .unwrap();
    let expected_queue = source.queued_messages().await.unwrap();
    let source_service = service(&source);
    let expected = source_service.snapshot().await.unwrap();
    let start_receipt = source_service.request_outcome("start").await.unwrap();
    let enqueue_receipt = source_service.request_outcome("enqueue").await.unwrap();
    let target = source.session().clone();
    drop(source_service);
    drop(source);
    let reopened = strict(target.clone()).await;
    assert_eq!(
        service(&reopened).snapshot().await.unwrap().state,
        expected.state
    );
    assert_eq!(
        service(&reopened).request_outcome("enqueue").await.unwrap(),
        enqueue_receipt
    );
    drop(reopened);
    let h = strict(memory()).await;
    let s = service(&h);
    let stale = request(
        &s,
        "prior-binding",
        json!({"action":"enqueue", "text":"must not land"}),
    )
    .await;
    let prior_token = stale.expected.binding_token.clone();
    bounded(h.set_session(target.clone())).await.unwrap();
    let current = s.snapshot().await.unwrap();
    assert_eq!(current.scope.session_id, target.storage().metadata().id);
    assert_ne!(current.scope.binding_token, prior_token);
    assert_eq!(current.state, expected.state);
    assert_eq!(current.execution_control, expected.execution_control);
    assert_eq!(s.request_outcome("start").await.unwrap(), start_receipt);
    assert_eq!(s.request_outcome("enqueue").await.unwrap(), enqueue_receipt);
    assert_eq!(h.queued_messages().await.unwrap(), expected_queue);
    assert!(matches!(
        bounded(s.mutate(stale)).await,
        Err(DurableError::Conflict)
    ));
    assert_eq!(s.snapshot().await.unwrap(), current);
    assert_eq!(s.request_outcome("prior-binding").await.unwrap(), None);
}

#[tokio::test]
async fn copied_native_entries_and_records_cannot_adopt_a_new_session_identity() {
    let source = seeded().await;
    source
        .follow_up(AgentMessage::User(UserMessage::new("foreign queue", 0)))
        .await
        .unwrap();
    let target = memory();
    assert_ne!(source.session_id(), target.storage().metadata().id);
    for entry in source
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
    {
        let Entry::Custom(c) = entry else {
            panic!("seed contains only native custom entries")
        };
        custom(&target, "main", &c.base.id, &c.custom_type, c.data).await;
    }
    let records = source
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    assert!(
        !records.is_empty(),
        "copy must include queue records as well as control entries"
    );
    for record in records {
        target.append_record(record).await.unwrap();
    }
    let before = image(&target).await;
    assert!(bounded(AgentHarness::create(options(target.clone())))
        .await
        .is_err());
    assert_eq!(image(&target).await, before);
    let h = seeded().await;
    let s = service(&h);
    let old = s.snapshot().await.unwrap();
    let queues = h.queued_messages().await.unwrap();
    assert!(bounded(h.set_session(target.clone())).await.is_err());
    assert_eq!(s.snapshot().await.unwrap(), old);
    assert_eq!(h.queued_messages().await.unwrap(), queues);
    assert_eq!(image(&target).await, before);
    assert_eq!(s.request_outcome("prior-binding").await.unwrap(), None);
}

#[tokio::test]
async fn validation_read_failure_is_redacted_and_never_publishes_or_prunes() {
    let h = seeded().await;
    let before = service(&h).snapshot().await.unwrap();
    let queues = h.queued_messages().await.unwrap();
    let base = memory();
    // Empty branches short-circuit without a branch-storage query. Give the
    // injected read failure an actual selected leaf to traverse.
    custom(&base, "main", "anchor", "synthetic-policy-anchor", None).await;
    prune_sentinel(&base).await;
    let before_target = image(&base).await;
    let probe = probe::Probe::new(base.storage());
    probe.fail_reads.store(true, Ordering::SeqCst);
    let target = Session::new(probe.clone(), None);
    let created = AgentHarness::create(options(target.clone())).await;
    let error = match created {
        Ok(_) => panic!("read failure must reject startup"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("Native durable session validation failed"));
    assert!(!error.contains("injected"));
    let error = h.set_session(target).await.unwrap_err().to_string();
    assert!(error.contains("Native durable session validation failed"));
    assert!(!error.contains("injected"));
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    assert_eq!(h.queued_messages().await.unwrap(), queues);
    assert_eq!(image(&base).await, before_target);
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(probe.commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn target_revision_race_and_cancellation_during_prepare_preserve_the_old_session() {
    let source = seeded().await;
    let base = source.session().clone();
    let probe = probe::Probe::new(base.storage());
    let h = seeded().await;
    h.follow_up(AgentMessage::User(UserMessage::new("old queued", 0)))
        .await
        .unwrap();
    let child = h
        .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
        .await
        .unwrap();
    let before = service(&h).snapshot().await.unwrap();
    let queues = h.queued_messages().await.unwrap();
    let old_entries = image(h.session()).await;
    for cancel in [false, true] {
        probe.pause_queue_reads.store(true, Ordering::SeqCst);
        let target = Session::new(probe.clone(), None);
        let owner = h.clone();
        let worker = tokio::spawn(async move { owner.set_session(target).await });
        bounded(probe.queue_read_entered.notified()).await;
        assert_eq!(h.session_id(), before.scope.session_id);
        if cancel {
            worker.abort();
            assert!(bounded(worker).await.unwrap_err().is_cancelled());
        } else {
            // Simulate a writer outside the harness gate after validation but
            // before publication. This is a revision-race test, not a claim of
            // atomic cross-process fencing.
            base.append_custom_entry("synthetic-policy-race", None)
                .await
                .unwrap();
            probe.release_queue_read.notify_one();
            let error = bounded(worker).await.unwrap().unwrap_err().to_string();
            assert!(error.contains("changed during session preparation"));
        }
        assert_eq!(bounded(service(&h).snapshot()).await.unwrap(), before);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert_eq!(image(h.session()).await, old_entries);
        assert!(!child.scope.is_cancelled());
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(probe.commits.load(Ordering::SeqCst), 0);
    }
    // Dropping preparation released the gate, and no detached publish remains.
    probe.release_queue_read.notify_one();
    h.set_session(memory()).await.unwrap();
    assert_ne!(h.session_id(), before.scope.session_id);
    assert!(child.scope.is_cancelled());
}
