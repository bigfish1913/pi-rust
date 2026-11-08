//! Actual-source full replay fixtures. Separate real service and synthetic v2 stores.
use rpi_agent::AgentMessage;
use rpi_ai::types::UserMessage;
use rpi_durable::{
    control::{ControlAction, ControlState},
    execution::{ExecutionControl, RunStatus},
    ledger::{
        insert::{Insert, InsertStep},
        Node, NodeRef,
    },
};
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{
        experimental_ledger::{
            self as codec,
            control::{
                execution::source::{
                    inspect_execution_branch, ExecutionBranchError, ExecutionBranchInspection,
                },
                TransitionPolicy,
            },
            DecodedCandidate, Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE,
            FORMAT,
        },
        DurableActor, DurableReceipt,
    },
    session::types::*,
    types::AgentHarnessOptions,
};
use std::{collections::HashMap, future::Future, time::Duration};
const OWNER: &str = "owner";
const EPOCH: &str = "execution-replay";
async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("bounded replay fixture")
}
fn read_policy(h: &AgentHarness) -> ReadPolicy {
    ReadPolicy::new_epoch(h.session().storage().metadata().id, "main", EPOCH).unwrap()
}
fn policy(h: &AgentHarness) -> TransitionPolicy {
    TransitionPolicy::new(
        read_policy(h),
        vec![
            DurableActor::owner(OWNER).unwrap(),
            DurableActor::child("child").unwrap(),
        ],
    )
    .unwrap()
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
#[derive(Default)]
struct Tree {
    nodes: HashMap<(String, u16), Node>,
    root: Option<NodeRef>,
    previous: Option<Previous>,
}
impl Tree {
    fn push(
        &mut self,
        mut receipt: DurableReceipt,
        state: ControlState,
        exec: ExecutionControl,
    ) -> Record {
        let previous = self.previous.clone();
        let ordinal = previous.as_ref().map_or(1, |p| p.ordinal + 1);
        receipt.expected.entry_id = previous.as_ref().map(|p| p.entry_id.clone());
        let mut insert = Insert::new(
            &receipt.request_id,
            codec::receipt_reference(&receipt, ordinal).unwrap(),
            self.root.clone(),
            &Sha256NodeHasher,
        )
        .unwrap();
        loop {
            match insert.step() {
                InsertStep::NeedNode(r) => insert
                    .provide(
                        self.nodes
                            .get(&(r.address.entry_id.clone(), r.address.slot))
                            .unwrap(),
                        &Sha256NodeHasher,
                    )
                    .unwrap(),
                InsertStep::Ready(_) => break,
                other => panic!("fixture insertion: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        for n in &batch.nodes {
            self.nodes
                .insert((n.address.entry_id.clone(), n.address.slot), n.clone());
        }
        self.root = Some(batch.root.clone());
        self.previous = Some(Previous {
            entry_id: receipt.entry_id.clone(),
            ordinal,
        });
        Record {
            format: FORMAT.into(),
            version: 2,
            epoch: Epoch {
                session_id: receipt.expected.session_id.clone(),
                lane: "main".into(),
                epoch_id: EPOCH.into(),
            },
            ordinal,
            previous,
            state,
            execution_control: exec,
            receipt,
            nodes: batch.nodes,
            root: batch.root,
        }
    }
}
use rpi_ai::{
    providers::faux::faux_assistant_message,
    types::{Content, StopReason},
};
use rpi_harness::{
    durable::{
        DurableError, DurableMutation, DurableOutcome, DurableService, ExperimentalDurablePolicy,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, Session, SystemClock,
    },
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::Arc;

type Audit = (Vec<Entry>, Vec<LaneRecord>, Vec<LanePointer>, u64);
async fn audit(h: &AgentHarness) -> Audit {
    let s = h.session().storage();
    (
        s.find_entries(&EntryQuery::default()).await.unwrap(),
        s.find_records(&RecordQuery::default()).await.unwrap(),
        s.get_lanes().await.unwrap(),
        s.history_revision().await.unwrap(),
    )
}
async fn harness(s: Arc<dyn SessionStorage>) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session: Session::new(s, None),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn experimental_harness(s: Arc<dyn SessionStorage>) -> AgentHarness {
    AgentHarness::create_experimental(
        AgentHarnessOptions {
            session: Session::new(s, None),
            allow_existing_session: true,
            require_native_durable: true,
            ..Default::default()
        },
        ExperimentalDurablePolicy::new(EPOCH, vec![OWNER.into()]).unwrap(),
    )
    .await
    .unwrap()
}
async fn jsonl(fs: Arc<dyn FileSystem>, file: &str, id: &str) -> Arc<dyn SessionStorage> {
    Arc::new(
        JsonlSessionStorage::create(
            fs,
            file,
            JsonlV4Header::new(id.into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap(),
    )
}
fn memory_store(id: &str) -> Arc<dyn SessionStorage> {
    Arc::new(rpi_harness::session::memory::InMemorySessionStorage::new(
        SessionMetadata {
            id: id.into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ))
}
struct Pair {
    left: AgentHarness,
    right: AgentHarness,
    tree: Tree,
    records: Vec<Record>,
    dir: Option<tempfile::TempDir>,
}
impl Pair {
    async fn new(files: bool) -> Self {
        let (left, right, dir) = if files {
            let dir = tempfile::tempdir().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
            (
                harness(jsonl(fs.clone(), "left.jsonl", "synthetic-service").await).await,
                experimental_harness(jsonl(fs, "right.jsonl", "synthetic-v2").await).await,
                Some(dir),
            )
        } else {
            (
                harness(memory_store("synthetic-service")).await,
                experimental_harness(memory_store("synthetic-v2")).await,
                None,
            )
        };
        Self {
            left,
            right,
            tree: Tree::default(),
            records: vec![],
            dir,
        }
    }
    async fn act(
        &mut self,
        actor: DurableActor,
        action: ControlAction,
        run: Option<&str>,
    ) -> DurableOutcome {
        let service = DurableService::bind(&self.left, actor);
        let old = service.snapshot().await.unwrap();
        let leaf = self
            .right
            .session()
            .view("main")
            .get_leaf_id()
            .await
            .unwrap();
        let scope = bounded(codec::source::read_branch(
            self.right.session(),
            "main",
            leaf.as_deref(),
            &read_policy(&self.right),
        ))
        .await
        .unwrap()
        .scope;
        assert_eq!(
            old.scope.history_revision, scope.history_revision,
            "same native sequence, not revision rebasing"
        );
        let id = format!("v2-{}", self.records.len() + 1);
        let out = bounded(service.mutate(DurableMutation {
            request_id: format!("request-{id}"),
            expected: old.scope,
            action,
            run_id: run.map(str::to_owned),
        }))
        .await
        .unwrap();
        let mut receipt = out.receipt.clone();
        receipt.entry_id = id.clone();
        receipt.expected.session_id = scope.session_id;
        receipt.expected.lane = scope.lane;
        receipt.expected.binding_token = scope.binding_token;
        receipt.expected.leaf_id = scope.selected_leaf_id;
        receipt.expected.history_revision = scope.history_revision;
        let record = self.tree.push(
            receipt,
            out.snapshot.state.clone(),
            out.snapshot.execution_control.clone(),
        );
        assert_eq!(candidate_one(&record).record(), &record);
        self.right
            .session()
            .append_entry(
                ProvisionedEntry {
                    id,
                    kind: ProvisionedKind::Custom {
                        custom_type: CUSTOM_TYPE.into(),
                        data: Some(serde_json::to_value(&record).unwrap()),
                    },
                },
                "main",
            )
            .await
            .unwrap();
        self.records.push(record);
        assert_eq!(
            self.left
                .session()
                .storage()
                .history_revision()
                .await
                .unwrap(),
            self.right
                .session()
                .storage()
                .history_revision()
                .await
                .unwrap()
        );
        out
    }
    async fn reject(
        &self,
        actor: DurableActor,
        action: ControlAction,
        run: Option<&str>,
    ) -> DurableError {
        let before = (audit(&self.left).await, audit(&self.right).await);
        let service = DurableService::bind(&self.left, actor);
        let err = service
            .mutate(DurableMutation {
                request_id: "rejected-request".into(),
                expected: service.snapshot().await.unwrap().scope,
                action,
                run_id: run.map(str::to_owned),
            })
            .await
            .unwrap_err();
        assert_eq!((audit(&self.left).await, audit(&self.right).await), before);
        err
    }
    async fn run(&self, id: &str, status: RunStatus, unknown: bool) {
        for h in [&self.left, &self.right] {
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
            h.session()
                .append_entry(
                    ProvisionedEntry {
                        id: format!("{id}-prompt"),
                        kind: ProvisionedKind::Message {
                            message: AgentMessage::User(UserMessage::new(
                                "private differential prompt",
                                0,
                            )),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            if unknown {
                let mut msg = faux_assistant_message("private assistant", StopReason::ToolUse);
                msg.content.push(Content::tool_call(
                    format!("{id}-call"),
                    "write",
                    serde_json::json!({"private":"arguments"}),
                ));
                h.session()
                    .append_entry(
                        ProvisionedEntry {
                            id: format!("{id}-assistant"),
                            kind: ProvisionedKind::Message {
                                message: AgentMessage::Assistant(Box::new(msg)),
                                terminate: None,
                            },
                        },
                        "main",
                    )
                    .await
                    .unwrap();
                h.session()
                    .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                        base: base(&format!("{id}-invocation")),
                        run_id: id.into(),
                        assistant_entry_id: format!("{id}-assistant"),
                        tool_index: 0,
                        tool_call_id: format!("{id}-call"),
                        tool_name: "write".into(),
                        effective_args: serde_json::json!({"private":"effective arguments"}),
                        result_entry_id: format!("{id}-reserved-result"),
                        replay: ToolReplay::Safe,
                    }))
                    .await
                    .unwrap();
            }
            if status != RunStatus::Open {
                finish(h, id, status.clone()).await;
            }
        }
    }
    async fn finish_open(&self, id: &str) {
        for h in [&self.left, &self.right] {
            finish(h, id, RunStatus::Completed).await;
        }
    }
    async fn verify(&self) -> ExecutionBranchInspection {
        let left_before = audit(&self.left).await;
        let right_before = audit(&self.right).await;
        let leaf = self
            .right
            .session()
            .view("main")
            .get_leaf_id()
            .await
            .unwrap();
        let result = bounded(inspect_execution_branch(
            &self.right,
            "main",
            leaf.as_deref(),
            &policy(&self.right),
        ))
        .await
        .unwrap();
        let actual = DurableService::bind(&self.left, owner())
            .snapshot()
            .await
            .unwrap();
        assert_eq!(result.transitions().state(), &actual.state);
        assert_eq!(
            result.transitions().execution_control(),
            &actual.execution_control
        );
        assert_eq!(
            result.transitions().ledger().receipts(),
            self.records.len() as u64
        );
        assert_eq!(result.scope().current_leaf_id, leaf);
        assert_eq!(result.scope().selected_leaf_id, leaf);
        assert_eq!(audit(&self.left).await, left_before);
        assert_eq!(audit(&self.right).await, right_before);
        result
    }
}
fn candidate_one(r: &Record) -> DecodedCandidate {
    codec::decode(
        &serde_json::to_vec(r).unwrap(),
        &r.receipt.entry_id,
        CUSTOM_TYPE,
        &ReadPolicy::new_epoch(r.epoch.session_id.clone(), "main", EPOCH).unwrap(),
    )
    .unwrap()
}
async fn finish(h: &AgentHarness, id: &str, status: RunStatus) {
    let interrupted = status == RunStatus::Interrupted;
    assert!(interrupted || status == RunStatus::Completed);
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finished")),
            run_id: id.into(),
            outcome: if interrupted {
                OperationOutcome::Aborted
            } else {
                OperationOutcome::Completed
            },
            error: interrupted.then(|| OperationError {
                code: "interrupted".into(),
                message: "private error".into(),
            }),
            result: Some(RunResultIdentity {
                leaf_id: h
                    .session()
                    .view("main")
                    .get_leaf_id()
                    .await
                    .unwrap()
                    .unwrap(),
                final_entry_id: None,
                assistant_entry_id: None,
            }),
        }))
        .await
        .unwrap();
}
fn owner() -> DurableActor {
    DurableActor::owner(OWNER).unwrap()
}
async fn trace(p: &mut Pair) {
    assert!(
        !p.act(owner(), ControlAction::Reset, None)
            .await
            .receipt
            .changed
    );
    p.act(owner(),serde_json::from_value(serde_json::json!({"action":"start","name":"work","steps":[{"id":"s","title":"Verify"}]})).unwrap(),None).await;
    let out = p
        .act(
            owner(),
            ControlAction::Enqueue {
                text: "claimed work".into(),
            },
            None,
        )
        .await;
    let item = out.snapshot.state.queue[0].id.clone();
    p.act(
        DurableActor::child("child").unwrap(),
        ControlAction::Claim { item: item.clone() },
        None,
    )
    .await;
    let action: ControlAction = serde_json::from_value(
        serde_json::json!({"action":"finish","item":item,"status":"completed"}),
    )
    .unwrap();
    assert!(matches!(
        p.reject(owner(), action, None).await,
        DurableError::PermissionDenied
    ));
    p.act(owner(), ControlAction::Cancel { item, note: None }, None)
        .await;
    p.run("run-a", RunStatus::Interrupted, true).await;
    let recovered = p.act(owner(), ControlAction::Recover, Some("run-a")).await;
    let recovery = recovered
        .snapshot
        .execution_control
        .recovery_request
        .unwrap();
    assert_eq!(recovery.unknown_invocation_ids, vec!["run-a-invocation"]);
    assert_eq!(
        recovery.observed_history_revision,
        recovered.receipt.expected.history_revision
    );
    p.act(owner(), ControlAction::Reset, None).await;
    let noop = p.act(owner(), ControlAction::Reset, None).await;
    assert!(!noop.receipt.changed);
    assert_eq!(
        noop.snapshot
            .execution_control
            .recovery_request
            .as_ref()
            .unwrap()
            .run_id,
        "run-a"
    );
    p.run("run-b", RunStatus::Interrupted, false).await;
    p.act(owner(), ControlAction::Recover, Some("run-b")).await;
    let other = p
        .act(owner(), ControlAction::ForgetRun, Some("run-a"))
        .await;
    assert_eq!(
        other
            .snapshot
            .execution_control
            .recovery_request
            .unwrap()
            .run_id,
        "run-b"
    );
    p.act(
        owner(),
        ControlAction::Enqueue {
            text: "keep execution intent".into(),
        },
        None,
    )
    .await;
    let forgotten = p
        .act(owner(), ControlAction::ForgetRun, Some("run-b"))
        .await;
    assert!(forgotten
        .snapshot
        .execution_control
        .recovery_request
        .is_none());
    p.run("run-open", RunStatus::Open, false).await;
    p.act(owner(), ControlAction::ForgetRun, Some("run-open"))
        .await;
    p.finish_open("run-open").await;
    p.run("run-completed", RunStatus::Completed, false).await;
    p.act(owner(), ControlAction::ForgetRun, Some("run-completed"))
        .await;
    p.act(owner(), ControlAction::Reset, None).await;
    assert!(
        !p.act(owner(), ControlAction::Reset, None)
            .await
            .receipt
            .changed
    );
    assert_eq!(p.records.len(), 16);
}

#[tokio::test]
async fn memory_physical_source_replays_interleaved_control_execution_and_explicit_historical_branch(
) {
    bounded(async {
        let mut p = Pair::new(false).await;
        trace(&mut p).await;
        p.verify().await;
        let before = audit(&p.right).await;
        let old = inspect_execution_branch(&p.right, "main", Some("v2-6"), &policy(&p.right))
            .await
            .unwrap();
        assert_eq!(old.transitions().state(), &p.records[5].state);
        assert_eq!(
            old.transitions().execution_control(),
            &p.records[5].execution_control
        );
        assert_eq!(
            old.transitions()
                .execution_control()
                .recovery_request
                .as_ref()
                .unwrap()
                .unknown_invocation_ids,
            vec!["run-a-invocation"]
        );
        assert_eq!(old.scope().selected_leaf_id.as_deref(), Some("v2-6"));
        assert_eq!(old.scope().current_leaf_id.as_deref(), Some("v2-16"));
        let empty = inspect_execution_branch(&p.right, "main", None, &policy(&p.right))
            .await
            .unwrap();
        assert_eq!(empty.transitions().state(), &ControlState::default());
        assert_eq!(
            empty.transitions().execution_control(),
            &ExecutionControl::default()
        );
        assert_eq!(empty.transitions().ledger().receipts(), 0);
        assert_eq!(audit(&p.right).await, before);
        p.right
            .session()
            .append_entry(
                ProvisionedEntry {
                    id: "noncontrol-tip".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::User(UserMessage::new("private tip", 0)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let before = audit(&p.right).await;
        let tip =
            inspect_execution_branch(&p.right, "main", Some("noncontrol-tip"), &policy(&p.right))
                .await
                .unwrap();
        assert_eq!(tip.transitions().state(), &p.records[15].state);
        assert_eq!(
            tip.transitions().execution_control(),
            &p.records[15].execution_control
        );
        assert_eq!(audit(&p.right).await, before);
    })
    .await
}
#[tokio::test]
async fn jsonl_full_source_replay_preserves_bytes_and_reopens_without_trusting_historical_binding()
{
    bounded(async {
        let mut p = Pair::new(true).await;
        trace(&mut p).await;
        p.left.session().storage().drain().await;
        p.right.session().storage().drain().await;
        let dir = p.dir.as_ref().unwrap();
        let left = std::fs::read(dir.path().join("left.jsonl")).unwrap();
        let right = std::fs::read(dir.path().join("right.jsonl")).unwrap();
        let out = p.verify().await;
        let old_binding = out.scope().binding_token.clone();
        assert_eq!(std::fs::read(dir.path().join("left.jsonl")).unwrap(), left);
        assert_eq!(
            std::fs::read(dir.path().join("right.jsonl")).unwrap(),
            right
        );
        let dir = p.dir.take().unwrap();
        drop(p);
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let a = harness(Arc::new(
            JsonlSessionStorage::load(
                fs.clone(),
                "left.jsonl",
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap(),
        ))
        .await;
        let b = experimental_harness(Arc::new(
            JsonlSessionStorage::load(
                fs,
                "right.jsonl",
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap(),
        ))
        .await;
        let before = (audit(&a).await, audit(&b).await);
        let fresh = inspect_execution_branch(&b, "main", Some("v2-16"), &policy(&b))
            .await
            .unwrap();
        assert_ne!(fresh.scope().binding_token, old_binding);
        assert_eq!(fresh.transitions().state(), out.transitions().state());
        assert_eq!(
            fresh.transitions().execution_control(),
            out.transitions().execution_control()
        );
        let actual = DurableService::bind(&a, owner()).snapshot().await.unwrap();
        assert_eq!(fresh.transitions().state(), &actual.state);
        assert_eq!(
            fresh.transitions().execution_control(),
            &actual.execution_control
        );
        assert_eq!((audit(&a).await, audit(&b).await), before);
        assert_eq!(std::fs::read(dir.path().join("left.jsonl")).unwrap(), left);
        assert_eq!(
            std::fs::read(dir.path().join("right.jsonl")).unwrap(),
            right
        );
    })
    .await
}
#[tokio::test]
async fn full_source_missing_selection_and_wrong_session_policy_fail_without_writes() {
    let p = Pair::new(false).await;
    let before = audit(&p.right).await;
    assert!(matches!(
        inspect_execution_branch(&p.right, "main", Some("missing"), &policy(&p.right)).await,
        Err(ExecutionBranchError::Source(
            codec::source::SourceError::Metadata
        ))
    ));
    assert!(matches!(
        inspect_execution_branch(&p.right, "main", None, &policy(&p.left)).await,
        Err(ExecutionBranchError::Source(
            codec::source::SourceError::Policy
        ))
    ));
    assert_eq!(audit(&p.right).await, before);
}

#[path = "experimental_ledger_execution_source/probe.rs"]
mod probe;
use std::sync::atomic::Ordering::SeqCst;
async fn single(open_at_receipt: bool) -> Arc<AgentHarness> {
    let mut p = Pair::new(false).await;
    p.run(
        "run",
        if open_at_receipt {
            RunStatus::Open
        } else {
            RunStatus::Interrupted
        },
        false,
    )
    .await;
    if !open_at_receipt {
        p.act(owner(), ControlAction::Recover, Some("run")).await;
    } else {
        // Deliberately invalid execution transition, but a locally valid physical
        // source and canonical ledger. A later interruption cannot legitimize it.
        let selected = p.right.session().view("main").get_leaf_id().await.unwrap();
        let scope = codec::source::read_branch(
            p.right.session(),
            "main",
            selected.as_deref(),
            &read_policy(&p.right),
        )
        .await
        .unwrap()
        .scope;
        let r = p.tree.push(
            DurableReceipt {
                request_id: "request-invalid-open".into(),
                actor: OWNER.into(),
                expected: rpi_harness::durable::DurableScope {
                    session_id: scope.session_id,
                    lane: scope.lane,
                    binding_token: scope.binding_token,
                    leaf_id: scope.selected_leaf_id,
                    history_revision: scope.history_revision,
                    history_generation: 0,
                    entry_id: None,
                    control_revision: 0,
                },
                action: ControlAction::Recover,
                run_id: Some("run".into()),
                changed: true,
                entry_id: "v2-1".into(),
                control_revision: 1,
            },
            ControlState {
                revision: 1,
                ..Default::default()
            },
            ExecutionControl {
                forgotten_run_ids: vec![],
                recovery_request: Some(rpi_durable::execution::RecoveryRequest {
                    run_id: "run".into(),
                    started_seq: 1,
                    observed_history_revision: scope.history_revision,
                    unknown_invocation_ids: vec![],
                }),
            },
        );
        candidate_one(&r);
        p.right
            .session()
            .append_entry(
                ProvisionedEntry {
                    id: "v2-1".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: CUSTOM_TYPE.into(),
                        data: Some(serde_json::to_value(&r).unwrap()),
                    },
                },
                "main",
            )
            .await
            .unwrap();
        for h in [&p.left, &p.right] {
            finish(h, "run", RunStatus::Interrupted).await;
        }
    }
    Arc::new(p.right)
}
async fn probed(open: bool) -> (Arc<AgentHarness>, Arc<probe::Probe>) {
    let h = single(open).await;
    let probe = probe::Probe::new(h.session().storage());
    h.session().set_storage(probe.clone());
    probe.reset();
    (h, probe)
}
async fn external(h: &AgentHarness, probe: &probe::Probe, kind: &str) {
    match kind {
        "append" => {
            probe
                .inner
                .append_entry(
                    ProvisionedEntry {
                        id: "external-tip".into(),
                        kind: ProvisionedKind::Message {
                            message: AgentMessage::User(UserMessage::new("external fixture", 0)),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        "navigate" => probe
            .inner
            .move_lane("main", Some("run-prompt"))
            .await
            .unwrap(),
        "rebind" => h.session().set_storage(memory_store("synthetic-rebound")),
        _ => panic!("unknown external fixture change"),
    }
}
#[tokio::test]
async fn future_terminal_cannot_legitimize_recover_recorded_while_open() {
    let h = single(true).await;
    let before = audit(&h).await;
    assert!(matches!(
        inspect_execution_branch(&h, "main", Some("v2-1"), &policy(&h)).await,
        Err(ExecutionBranchError::Transition(
            codec::control::execution::ExecutionTransitionError::Execution(
                rpi_durable::execution::ExecutionError::NotInterrupted
            )
        ))
    ));
    assert_eq!(audit(&h).await, before);
}
#[tokio::test]
async fn post_native_final_real_outer_fence_catches_append_navigation_rebind_on_success_and_error()
{
    bounded(async {
        for open in [false, true] {
            for kind in ["append", "navigate", "rebind"] {
                let (h, p) = probed(open).await;
                let dry = inspect_execution_branch(&h, "main", Some("v2-1"), &policy(&h)).await;
                assert_eq!(dry.is_ok(), !open);
                assert_eq!(p.revision_calls.load(SeqCst), 8);
                assert_eq!(p.lane_calls.load(SeqCst), 8);
                p.reset();
                p.pause_revision_at.store(7, SeqCst);
                let reader = h.clone();
                let task = tokio::spawn(async move {
                    inspect_execution_branch(&reader, "main", Some("v2-1"), &policy(&reader)).await
                });
                bounded(p.captured.notified()).await;
                external(&h, &p, kind).await;
                p.release.notify_one();
                assert!(
                    matches!(
                        bounded(task).await.unwrap(),
                        Err(ExecutionBranchError::Source(
                            codec::source::SourceError::Conflict
                        ))
                    ),
                    "{open}/{kind}"
                );
                assert_eq!(p.revision_calls.load(SeqCst), 8);
                assert_eq!(p.lane_calls.load(SeqCst), 8);
                assert_eq!(p.writes.load(SeqCst), 0);
            }
        }
    })
    .await
}
#[tokio::test]
async fn both_outer_final_io_complete_and_read_fault_precedes_change_and_collected_errors() {
    bounded(async {
        for open in [false, true] {
            for lane_fault in [false, true] {
                let (h, p) = probed(open).await;
                p.pause_revision_at.store(7, SeqCst);
                if lane_fault {
                    p.fail_lane_at.store(8, SeqCst);
                } else {
                    p.fail_revision_at.store(8, SeqCst);
                }
                let reader = h.clone();
                let task = tokio::spawn(async move {
                    inspect_execution_branch(&reader, "main", Some("v2-1"), &policy(&reader)).await
                });
                bounded(p.captured.notified()).await;
                external(&h, &p, "append").await;
                p.release.notify_one();
                assert!(matches!(
                    bounded(task).await.unwrap(),
                    Err(ExecutionBranchError::Source(
                        codec::source::SourceError::Read
                    ))
                ));
                assert_eq!(p.lane_calls.load(SeqCst), 8);
                assert_eq!(p.revision_calls.load(SeqCst), 8);
                assert_eq!(p.writes.load(SeqCst), 0);
            }
        }
    })
    .await
}
#[tokio::test]
async fn cancel_after_actual_native_page_capture_has_no_late_reader_worker_or_write() {
    let (h, p) = probed(false).await;
    let before = audit(&h).await;
    p.reset();
    p.pause_page_at.store(1, SeqCst);
    let reader = h.clone();
    let task = tokio::spawn(async move {
        inspect_execution_branch(&reader, "main", Some("v2-1"), &policy(&reader)).await
    });
    bounded(p.captured.notified()).await;
    task.abort();
    assert!(bounded(task).await.unwrap_err().is_cancelled());
    let stopped = p.reads.load(SeqCst);
    p.release.notify_one();
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(p.reads.load(SeqCst), stopped);
    assert_eq!(p.writes.load(SeqCst), 0);
    assert_eq!(audit(&h).await, before);
    p.pause_page_at.store(0, SeqCst);
    p.reset();
    assert!(
        inspect_execution_branch(&h, "main", Some("v2-1"), &policy(&h))
            .await
            .is_ok()
    );
    assert_eq!(p.writes.load(SeqCst), 0);
}
#[tokio::test]
async fn native_page_fault_is_collected_but_still_fenced_by_real_outer_observations() {
    bounded(async {
        for changed in [false, true] {
            let (h, p) = probed(false).await;
            let before = audit(&h).await;
            p.reset();
            p.fail_page.store(true, SeqCst);
            p.pause_revision_at.store(7, SeqCst);
            let reader = h.clone();
            let task = tokio::spawn(async move {
                inspect_execution_branch(&reader, "main", Some("v2-1"), &policy(&reader)).await
            });
            bounded(p.captured.notified()).await;
            if changed {
                external(&h, &p, "append").await;
            }
            p.release.notify_one();
            let out = bounded(task).await.unwrap();
            if changed {
                assert!(matches!(
                    out,
                    Err(ExecutionBranchError::Source(
                        codec::source::SourceError::Conflict
                    ))
                ));
            } else {
                assert!(matches!(out, Err(ExecutionBranchError::Native(_))));
                assert_eq!(audit(&h).await, before);
            }
            assert_eq!(p.writes.load(SeqCst), 0);
        }
    })
    .await
}

#[path = "experimental_ledger_execution_source/adversarial.rs"]
mod adversarial;
