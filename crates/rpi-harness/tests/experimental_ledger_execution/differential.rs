//! Two separate real sessions: v1 service authority on the left, synthetic v2
//! fixture on the right. No v1 journal is imported, rewritten or fed as v2 facts.
use super::*;
use rpi_ai::{
    providers::faux::faux_assistant_message,
    types::{Content, StopReason},
};
use rpi_durable::execution::{ExecutionError, ToolStatus};
use rpi_harness::{
    durable::{DurableError, DurableMutation, DurableOutcome, DurableService},
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
                harness(jsonl(fs, "right.jsonl", "synthetic-v2").await).await,
                Some(dir),
            )
        } else {
            (
                harness(memory_store("synthetic-service")).await,
                harness(memory_store("synthetic-v2")).await,
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
    async fn verify(&self) -> Vec<NativeRunInspection> {
        let left_before = audit(&self.left).await;
        let right_before = audit(&self.right).await;
        let facts = mint(&self.right, &self.records).await;
        let input = candidates(&self.records);
        let result = inspect_execution_transitions(&input, &policy(&self.right), &facts).unwrap();
        let actual = DurableService::bind(&self.left, DurableActor::owner(OWNER).unwrap())
            .snapshot()
            .await
            .unwrap();
        assert_eq!(result.state(), &actual.state);
        assert_eq!(result.execution_control(), &actual.execution_control);
        assert_eq!(result.ledger().receipts(), self.records.len() as u64);
        let selected = self
            .right
            .session()
            .view("main")
            .get_leaf_id()
            .await
            .unwrap();
        let source = codec::source::read_branch(
            self.right.session(),
            "main",
            selected.as_deref(),
            &read_policy(&self.right),
        )
        .await
        .unwrap();
        assert_eq!(source.records.len(), self.records.len());
        for (from_source, expected) in source.records.iter().zip(&self.records) {
            assert_eq!(from_source.candidate.record(), expected);
        }
        assert_eq!(audit(&self.left).await, left_before);
        assert_eq!(audit(&self.right).await, right_before);
        facts
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
async fn mint(h: &AgentHarness, records: &[Record]) -> Vec<NativeRunInspection> {
    let mut facts = vec![];
    for r in records {
        if let Some(run) = r.receipt.run_id.as_deref() {
            let fact = bounded(inspect_native_run(
                h,
                "main",
                r.receipt.expected.leaf_id.as_deref(),
                r.receipt.expected.history_revision,
                run,
                &read_policy(h),
            ))
            .await
            .unwrap();
            facts.push(fact);
        }
    }
    facts
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
async fn mirrored_real_service_memory_trace_matches_physical_v2_source_and_historical_facts() {
    bounded(async {
        let mut p = Pair::new(false).await;
        trace(&mut p).await;
        let facts = p.verify().await;
        assert_eq!(facts.len(), 6);
        assert!(facts.iter().any(|f| f
            .run()
            .tools
            .iter()
            .any(|t| t.status == ToolStatus::Unknown)));
        let open = facts.iter().find(|f| f.run().id == "run-open").unwrap();
        assert_eq!(
            open.run().status,
            RunStatus::Open,
            "later terminal must not rewrite historical forget facts"
        );
        assert!(p
            .records
            .last()
            .unwrap()
            .execution_control
            .recovery_request
            .is_none());
        assert_eq!(
            p.records
                .last()
                .unwrap()
                .execution_control
                .forgotten_run_ids,
            vec!["run-a", "run-b", "run-open", "run-completed"]
        );
    })
    .await
}
#[tokio::test]
async fn mirrored_jsonl_trace_preserves_exact_bytes_audits_and_reopens_with_new_binding() {
    bounded(async {
        let mut p = Pair::new(true).await;
        trace(&mut p).await;
        p.left.session().storage().drain().await;
        p.right.session().storage().drain().await;
        let dir = p.dir.as_ref().unwrap();
        let left = std::fs::read(dir.path().join("left.jsonl")).unwrap();
        let right = std::fs::read(dir.path().join("right.jsonl")).unwrap();
        let facts = p.verify().await;
        let old_binding = facts[0].scope().binding_token.clone();
        let expected = p.records.last().unwrap().clone();
        let records = p.records.clone();
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
        let b = harness(Arc::new(
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
        let facts = mint(&b, &records).await;
        assert_ne!(facts[0].scope().binding_token, old_binding);
        let out =
            inspect_execution_transitions(&candidates(&records), &policy(&b), &facts).unwrap();
        assert_eq!(out.state(), &expected.state);
        assert_eq!(out.execution_control(), &expected.execution_control);
        let actual = DurableService::bind(&a, owner()).snapshot().await.unwrap();
        assert_eq!(out.state(), &actual.state);
        assert_eq!(out.execution_control(), &actual.execution_control);
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
async fn real_service_domain_rejections_preserve_both_histories() {
    bounded(async {
        let mut p = Pair::new(false).await;
        p.run("run", RunStatus::Open, false).await;
        assert!(matches!(
            p.reject(owner(), ControlAction::Recover, Some("run")).await,
            DurableError::Execution(ExecutionError::NotInterrupted)
        ));
        p.finish_open("run").await;
        assert!(matches!(
            p.reject(owner(), ControlAction::Recover, Some("run")).await,
            DurableError::Execution(ExecutionError::NotInterrupted)
        ));
        p.act(owner(), ControlAction::ForgetRun, Some("run")).await;
        assert!(matches!(
            p.reject(owner(), ControlAction::ForgetRun, Some("run"))
                .await,
            DurableError::Execution(ExecutionError::NoRun)
        ));
        p.run("interrupted", RunStatus::Interrupted, false).await;
        p.act(owner(), ControlAction::Recover, Some("interrupted"))
            .await;
        assert!(matches!(
            p.reject(owner(), ControlAction::Recover, Some("interrupted"))
                .await,
            DurableError::Execution(ExecutionError::AlreadyRequested)
        ));
        assert!(matches!(
            p.reject(
                DurableActor::child("child").unwrap(),
                ControlAction::ForgetRun,
                Some("interrupted")
            )
            .await,
            DurableError::PermissionDenied
        ));
        p.verify().await;
    })
    .await
}

fn recanonical(records: &[Record]) -> Vec<Record> {
    let mut tree = Tree::default();
    records
        .iter()
        .map(|r| {
            tree.push(
                r.receipt.clone(),
                r.state.clone(),
                r.execution_control.clone(),
            )
        })
        .collect()
}
#[tokio::test]
async fn native_capsules_do_not_make_open_recover_repeat_recover_or_forgotten_requests_valid() {
    let (h, f) = fixture().await;
    let earlier = native_fact(&h, f.as_of_revision() - 1).await;
    assert_eq!(earlier.run().status, RunStatus::Open);
    let mut open_record = recovered_records(&f).remove(0);
    open_record.receipt.expected.history_revision = earlier.as_of_revision();
    let input = candidates(&recanonical(&[open_record]));
    assert!(matches!(
        inspect_execution_transitions(&input, &policy(&h), &[earlier]),
        Err(ExecutionTransitionError::Execution(
            ExecutionError::NotInterrupted
        ))
    ));
    for forgotten in [false, true] {
        let mut records = recovered_records(&f);
        if !forgotten {
            records.truncate(1);
        }
        let last = records.last().unwrap();
        let mut next = last.clone();
        next.receipt = receipt(
            &f,
            "invalid-repeat",
            OWNER,
            ControlAction::Recover,
            Some("run"),
            &last.state,
            &last.state,
            false,
        );
        records.push(next);
        let input = candidates(&recanonical(&records));
        let facts = vec![f.clone(); records.len()];
        let err = inspect_execution_transitions(&input, &policy(&h), &facts).unwrap_err();
        assert_eq!(
            err,
            ExecutionTransitionError::Execution(if forgotten {
                ExecutionError::NoRun
            } else {
                ExecutionError::AlreadyRequested
            })
        );
    }
}
#[tokio::test]
async fn ordinary_reset_preserves_execution_and_forged_clearing_is_not_a_noop() {
    let (h, f) = fixture().await;
    let mut records = recovered_records(&f);
    records.truncate(1);
    let last = &records[0];
    let mut reset = last.clone();
    reset.receipt = receipt(
        &f,
        "reset",
        OWNER,
        ControlAction::Reset,
        None,
        &last.state,
        &last.state,
        false,
    );
    records.push(reset);
    let valid = recanonical(&records);
    let out =
        inspect_execution_transitions(&candidates(&valid), &policy(&h), &[f.clone()]).unwrap();
    assert_eq!(out.state().revision, 1);
    assert_eq!(
        out.execution_control()
            .recovery_request
            .as_ref()
            .unwrap()
            .run_id,
        "run"
    );
    records[1].execution_control = ExecutionControl::default();
    assert!(matches!(
        inspect_execution_transitions(&candidates(&recanonical(&records)), &policy(&h), &[f]),
        Err(ExecutionTransitionError::Control(
            TransitionError::Transition
        ))
    ));
}
#[tokio::test]
async fn complete_fact_binding_rejects_foreign_session_run_watermark_and_ordinal_order() {
    let (h, f) = fixture().await;
    let foreign_pair = Pair::new(false).await;
    foreign_pair.run("run", RunStatus::Interrupted, false).await;
    let foreign = inspect_native_run(
        &foreign_pair.right,
        "main",
        Some("run-prompt"),
        foreign_pair
            .right
            .session()
            .storage()
            .history_revision()
            .await
            .unwrap(),
        "run",
        &read_policy(&foreign_pair.right),
    )
    .await
    .unwrap();
    assert_ne!(foreign.scope().session_id, f.scope().session_id);
    let records = recovered_records(&f);
    assert!(matches!(
        inspect_execution_transitions(
            &candidates(&records),
            &policy(&h),
            &[foreign.clone(), foreign]
        ),
        Err(ExecutionTransitionError::Facts)
    ));
    for kind in ["run", "watermark"] {
        let mut bad = records[0].clone();
        if kind == "run" {
            bad.receipt.run_id = Some("other-run".into());
            bad.execution_control
                .recovery_request
                .as_mut()
                .unwrap()
                .run_id = "other-run".into();
        } else {
            bad.receipt.expected.history_revision += 1;
            bad.execution_control
                .recovery_request
                .as_mut()
                .unwrap()
                .observed_history_revision += 1;
        }
        assert!(matches!(
            inspect_execution_transitions(
                &candidates(&recanonical(&[bad])),
                &policy(&h),
                &[f.clone()]
            ),
            Err(ExecutionTransitionError::Facts)
        ));
    }
    let mut input = candidates(&records);
    input.swap(0, 1);
    assert!(matches!(
        inspect_execution_transitions(&input, &policy(&h), &[f.clone(), f]),
        Err(ExecutionTransitionError::Control(TransitionError::Chain(_)))
    ));
}
#[tokio::test]
async fn locally_valid_canonical_control_business_changed_and_revision_forgery_is_rejected() {
    let (h, f) = fixture().await;
    for kind in ["business", "false-change", "reset-workflow"] {
        let old = ControlState::default();
        let action = if kind == "business" {
            ControlAction::Enqueue {
                text: "real text".into(),
            }
        } else {
            ControlAction::Reset
        };
        let mut next = if kind == "reset-workflow" {
            old.apply(&serde_json::from_value(serde_json::json!({"action":"start","name":"invented","steps":[{"id":"s","title":"Verify"}]})).unwrap()).unwrap()
        } else {
            old.apply(&action).unwrap()
        };
        next.revision = 1;
        if kind == "business" {
            next.queue[0].text = "forged text".into();
        }
        let mut tree = Tree::default();
        let r = tree.push(
            receipt(&f, "forged", OWNER, action, None, &old, &next, true),
            next,
            ExecutionControl::default(),
        );
        assert!(
            matches!(
                inspect_execution_transitions(&candidates(&[r]), &policy(&h), &[]),
                Err(ExecutionTransitionError::Control(
                    TransitionError::Transition
                ))
            ),
            "{kind}"
        );
    }
}
#[tokio::test]
async fn inherited_canonical_256_257_record_and_native_capsule_count_bounds_are_hard() {
    use codec::chain::{ChainError, MAX_CHAIN_RECORDS};
    let (h, f) = fixture().await;
    let mut tree = Tree::default();
    let state = ControlState::default();
    let mut records = vec![];
    for i in 0..=MAX_CHAIN_RECORDS {
        records.push(tree.push(
            receipt(
                &f,
                &format!("noop-{i}"),
                OWNER,
                ControlAction::Reset,
                None,
                &state,
                &state,
                false,
            ),
            state.clone(),
            ExecutionControl::default(),
        ));
    }
    let input = candidates(&records);
    let valid =
        inspect_execution_transitions(&input[..MAX_CHAIN_RECORDS], &policy(&h), &[]).unwrap();
    assert_eq!(valid.ledger().receipts(), MAX_CHAIN_RECORDS as u64);
    assert_eq!(valid.state(), &state);
    assert_eq!(
        inspect_execution_transitions(&input, &policy(&h), &[]).unwrap_err(),
        ExecutionTransitionError::Control(TransitionError::Chain(ChainError::Limit))
    );
    assert_eq!(
        inspect_execution_transitions(&input, &policy(&h), &vec![f; MAX_CHAIN_RECORDS + 1])
            .unwrap_err(),
        ExecutionTransitionError::Facts
    );
}
