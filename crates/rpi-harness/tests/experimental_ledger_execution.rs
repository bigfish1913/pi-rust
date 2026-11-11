//! Conditional replay only: synthetic in-memory canonical records, real #45
//! native capsules, no source-bound journal/admission/restore/writer authority.
#[path = "experimental_ledger_execution/differential.rs"]
mod differential;
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
                execution::{inspect_execution_transitions, ExecutionTransitionError},
                TransitionError, TransitionPolicy,
            },
            native_run::{inspect_native_run, NativeRunInspection},
            DecodedCandidate, Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE,
            FORMAT,
        },
        DurableActor, DurableReceipt, DurableScope,
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
async fn native_fact(h: &AgentHarness, r: u64) -> NativeRunInspection {
    bounded(inspect_native_run(
        h,
        "main",
        Some("prompt"),
        r,
        "run",
        &read_policy(h),
    ))
    .await
    .unwrap()
}
async fn fixture() -> (AgentHarness, NativeRunInspection) {
    let h = AgentHarness::create(AgentHarnessOptions::default())
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
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
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: "prompt".into(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(UserMessage::new("private prompt", 0)),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("finished"),
            run_id: "run".into(),
            outcome: OperationOutcome::Aborted,
            error: Some(OperationError {
                code: "interrupted".into(),
                message: "private error".into(),
            }),
            result: Some(RunResultIdentity {
                leaf_id: "prompt".into(),
                final_entry_id: None,
                assistant_entry_id: None,
            }),
        }))
        .await
        .unwrap();
    let r = h.session().storage().history_revision().await.unwrap();
    let fact = native_fact(&h, r).await;
    (h, fact)
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
fn receipt(
    f: &NativeRunInspection,
    id: &str,
    actor: &str,
    action: ControlAction,
    run: Option<&str>,
    old: &ControlState,
    next: &ControlState,
    changed: bool,
) -> DurableReceipt {
    DurableReceipt {
        request_id: format!("request-{id}"),
        actor: actor.into(),
        expected: DurableScope {
            session_id: f.scope().session_id.clone(),
            lane: "main".into(),
            binding_token: f.scope().binding_token.clone(),
            leaf_id: f.scope().selected_leaf_id.clone(),
            history_revision: f.as_of_revision(),
            history_generation: 0,
            entry_id: None,
            control_revision: old.revision,
        },
        action,
        run_id: run.map(str::to_owned),
        entry_id: id.into(),
        control_revision: next.revision,
        changed,
    }
}
fn candidates(records: &[Record]) -> Vec<DecodedCandidate> {
    records
        .iter()
        .map(|r| {
            codec::decode(
                &serde_json::to_vec(r).unwrap(),
                &r.receipt.entry_id,
                CUSTOM_TYPE,
                &ReadPolicy::new_epoch(r.epoch.session_id.clone(), "main", EPOCH).unwrap(),
            )
            .expect("locally valid fixture isolating replay")
        })
        .collect()
}
fn recovered_records(f: &NativeRunInspection) -> Vec<Record> {
    let mut tree = Tree::default();
    let old = ControlState::default();
    let mut next = old.clone();
    next.revision = 1;
    let exec = ExecutionControl::default()
        .apply(&ControlAction::Recover, Some(f.run()), f.as_of_revision())
        .unwrap();
    let first = tree.push(
        receipt(
            f,
            "recover",
            OWNER,
            ControlAction::Recover,
            Some("run"),
            &old,
            &next,
            true,
        ),
        next.clone(),
        exec.clone(),
    );
    let mut final_state = next.clone();
    final_state.revision = 2;
    let final_exec = exec
        .apply(&ControlAction::ForgetRun, Some(f.run()), f.as_of_revision())
        .unwrap();
    let second = tree.push(
        receipt(
            f,
            "forget",
            OWNER,
            ControlAction::ForgetRun,
            Some("run"),
            &next,
            &final_state,
            true,
        ),
        final_state,
        final_exec,
    );
    vec![first, second]
}
#[tokio::test]
async fn real_capsules_conditionally_replay_recover_forget_and_exact_composite_revision() {
    let (h, f) = fixture().await;
    assert_eq!(f.run().status, RunStatus::Interrupted);
    let before = h
        .session()
        .storage()
        .get_log(&LogOptions::default())
        .await
        .unwrap();
    let records = recovered_records(&f);
    let input = candidates(&records);
    let out = inspect_execution_transitions(&input, &policy(&h), &[f.clone(), f.clone()]).unwrap();
    assert_eq!(out.state(), &records[1].state);
    assert_eq!(out.state().revision, 2);
    assert_eq!(out.execution_control().forgotten_run_ids, vec!["run"]);
    assert!(out.execution_control().recovery_request.is_none());
    assert_eq!(out.ledger().receipts(), 2);
    assert_eq!(out.ledger().root(), Some(&records[1].root));
    assert_eq!(
        h.session()
            .storage()
            .get_log(&LogOptions::default())
            .await
            .unwrap(),
        before
    );
}
#[tokio::test]
async fn missing_extra_swapped_historical_facts_and_wrong_leaf_fail_without_prefix() {
    let (h, f) = fixture().await;
    let records = recovered_records(&f);
    let input = candidates(&records);
    for facts in [
        vec![],
        vec![f.clone()],
        vec![f.clone(), f.clone(), f.clone()],
    ] {
        assert!(matches!(
            inspect_execution_transitions(&input, &policy(&h), &facts),
            Err(ExecutionTransitionError::Facts)
        ));
    }
    let mut bad = records[0].clone();
    bad.receipt.expected.leaf_id = Some("other-leaf".into());
    let mut t = Tree::default();
    let record = t.push(bad.receipt, bad.state, bad.execution_control);
    assert!(matches!(
        inspect_execution_transitions(&candidates(&[record]), &policy(&h), &[f.clone()]),
        Err(ExecutionTransitionError::Facts)
    ));
    let earlier = native_fact(&h, f.as_of_revision() - 1).await;
    assert_eq!(earlier.run().status, RunStatus::Open);
    assert!(matches!(
        inspect_execution_transitions(&input, &policy(&h), &[earlier, f.clone()]),
        Err(ExecutionTransitionError::Facts)
    ));
    assert!(inspect_execution_transitions(&input, &policy(&h), &[f.clone(), f]).is_ok());
}
#[tokio::test]
async fn mixed_real_current_revision_or_binding_observations_are_not_one_proof_set() {
    for rebind in [false, true] {
        let (h, f) = fixture().await;
        let input = candidates(&recovered_records(&f));
        if rebind {
            h.session().set_storage(h.session().storage());
        } else {
            h.session()
                .storage()
                .set_name(Some("synthetic fact"))
                .await
                .unwrap();
        }
        let newer = native_fact(&h, f.as_of_revision()).await;
        assert_ne!(f.scope(), newer.scope());
        assert!(matches!(
            inspect_execution_transitions(&input, &policy(&h), &[f, newer]),
            Err(ExecutionTransitionError::Facts)
        ));
    }
}
#[tokio::test]
async fn fixed_roles_and_existing_control_only_execution_rejection_are_preserved() {
    let (h, f) = fixture().await;
    let mut bad = recovered_records(&f).remove(0);
    bad.receipt.actor = "child".into();
    let mut t = Tree::default();
    let r = t.push(bad.receipt, bad.state, bad.execution_control);
    let input = candidates(&[r]);
    assert!(matches!(
        inspect_execution_transitions(&input, &policy(&h), &[f]),
        Err(ExecutionTransitionError::Control(
            TransitionError::Permission
        ))
    ));
    assert!(matches!(
        codec::control::inspect_control_transitions(&input, &policy(&h)),
        Err(TransitionError::ExecutionFactsRequired)
    ));
}
#[tokio::test]
async fn locally_valid_canonical_execution_tampering_is_rejected_before_returning_state() {
    let (h, f) = fixture().await;
    let mut records = recovered_records(&f);
    records.truncate(1);
    let mut bad = records.remove(0);
    bad.execution_control
        .recovery_request
        .as_mut()
        .unwrap()
        .unknown_invocation_ids
        .push("invented-invocation".into());
    let mut t = Tree::default();
    let bad = t.push(bad.receipt, bad.state, bad.execution_control);
    let input = candidates(&[bad]);
    assert!(matches!(
        inspect_execution_transitions(&input, &policy(&h), &[f.clone()]),
        Err(ExecutionTransitionError::Control(
            TransitionError::Transition
        ))
    ));
    assert!(inspect_execution_transitions(
        &candidates(&recovered_records(&f)),
        &policy(&h),
        &[f.clone(), f]
    )
    .is_ok());
}
#[tokio::test]
async fn empty_input_means_only_relative_default_and_extra_facts_are_rejected() {
    let (h, f) = fixture().await;
    let out = inspect_execution_transitions(&[], &policy(&h), &[]).unwrap();
    assert_eq!(out.state(), &ControlState::default());
    assert_eq!(out.execution_control(), &ExecutionControl::default());
    assert_eq!(out.ledger().receipts(), 0);
    assert!(matches!(
        inspect_execution_transitions(&[], &policy(&h), &[f]),
        Err(ExecutionTransitionError::Facts)
    ));
}
