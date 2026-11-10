//! Conditional, caller-attested control replay only: not source provenance,
//! authentication, a complete journal, execution facts, or a v2 writer.
//! Native v1 outcomes below are transformed into synthetic in-memory v2 fixtures;
//! the original Session records are never rewritten or imported.
use rpi_durable::{
    control::{
        ControlAction, ControlState, QueueItem, QueueStatus, Step, StepStatus, Workflow,
        WorkflowStatus,
    },
    execution::{ExecutionControl, RecoveryRequest},
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
            chain::{inspect_chain, ChainError, MAX_CHAIN_RECORDS},
            control::{
                inspect_control_transitions, TransitionError, TransitionPolicy, MAX_POLICY_ACTORS,
            },
            DecodedCandidate, Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE,
            FORMAT,
        },
        DurableActor, DurableMutation, DurableOutcome, DurableReceipt, DurableScope,
        DurableService,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::SessionStorage,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

const SESSION: &str = "synthetic-control";
const EPOCH: &str = "control-epoch";
const OWNER: &str = "owner";
const CHILD: &str = "child";
const OTHER: &str = "other-child";

fn read_policy(session: &str) -> ReadPolicy {
    ReadPolicy::new_epoch(session, "main", EPOCH).unwrap()
}
fn actors() -> Vec<DurableActor> {
    vec![
        DurableActor::owner(OWNER).unwrap(),
        DurableActor::child(CHILD).unwrap(),
        DurableActor::child(OTHER).unwrap(),
    ]
}
fn policy(session: &str) -> TransitionPolicy {
    TransitionPolicy::new(read_policy(session), actors()).unwrap()
}
fn action(value: Value) -> ControlAction {
    serde_json::from_value(value).unwrap()
}
fn start_action() -> ControlAction {
    action(json!({"action":"start","name":"work","steps":[{"id":"s","title":"Verify"}]}))
}
fn workflow_state() -> ControlState {
    ControlState {
        revision: 1,
        workflow: Some(Workflow {
            name: "work".into(),
            status: WorkflowStatus::Running,
            reason: None,
            steps: vec![Step {
                id: "s".into(),
                title: "Verify".into(),
                status: StepStatus::Pending,
                note: None,
            }],
        }),
        ..Default::default()
    }
}

#[derive(Default)]
struct Prefix {
    nodes: HashMap<(String, u16), Node>,
    root: Option<NodeRef>,
}
impl Prefix {
    // Fixture construction with the pure Insert; no Session append or v2 writer.
    fn make(
        &mut self,
        receipt: DurableReceipt,
        state: ControlState,
        execution_control: ExecutionControl,
        ordinal: u64,
    ) -> Record {
        let mut insert = Insert::new(
            &receipt.request_id,
            codec::receipt_reference(&receipt, ordinal).unwrap(),
            self.root.clone(),
            &Sha256NodeHasher,
        )
        .unwrap();
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    let node = self
                        .nodes
                        .get(&(reference.address.entry_id.clone(), reference.address.slot))
                        .expect("prefix node");
                    insert.provide(node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("unexpected fixture insertion: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        for node in &batch.nodes {
            assert!(self
                .nodes
                .insert(
                    (node.address.entry_id.clone(), node.address.slot),
                    node.clone()
                )
                .is_none());
        }
        self.root = Some(batch.root.clone());
        Record {
            format: FORMAT.into(),
            version: 2,
            epoch: Epoch {
                session_id: receipt.expected.session_id.clone(),
                lane: "main".into(),
                epoch_id: EPOCH.into(),
            },
            ordinal,
            previous: receipt.expected.entry_id.clone().map(|entry_id| Previous {
                entry_id,
                ordinal: ordinal - 1,
            }),
            receipt,
            state,
            execution_control,
            nodes: batch.nodes,
            root: batch.root,
        }
    }
}
fn rebuild(records: &[Record]) -> Vec<Record> {
    let mut prefix = Prefix::default();
    records
        .iter()
        .map(|r| {
            prefix.make(
                r.receipt.clone(),
                r.state.clone(),
                r.execution_control.clone(),
                r.ordinal,
            )
        })
        .collect()
}
fn synthetic_receipt(
    ordinal: u64,
    previous_revision: u64,
    actor: &str,
    action: ControlAction,
    revision: u64,
    changed: bool,
) -> DurableReceipt {
    DurableReceipt {
        request_id: format!("request-{ordinal:04}"),
        actor: actor.into(),
        expected: DurableScope {
            session_id: SESSION.into(),
            lane: "main".into(),
            binding_token: "synthetic-binding".into(),
            leaf_id: None,
            history_revision: ordinal * 7,
            history_generation: 0,
            entry_id: (ordinal > 1).then(|| format!("entry-{}", ordinal - 1)),
            control_revision: previous_revision,
        },
        action,
        run_id: None,
        entry_id: format!("entry-{ordinal}"),
        control_revision: revision,
        changed,
    }
}
fn append(
    records: &mut Vec<Record>,
    actor: &str,
    action: ControlAction,
    state: ControlState,
    changed: bool,
) {
    let ordinal = records.len() as u64 + 1;
    let previous_revision = records.last().map_or(0, |r| r.state.revision);
    let receipt = synthetic_receipt(
        ordinal,
        previous_revision,
        actor,
        action,
        state.revision,
        changed,
    );
    let mut prefix = Prefix::default();
    for old in records.iter() {
        prefix.make(
            old.receipt.clone(),
            old.state.clone(),
            old.execution_control.clone(),
            old.ordinal,
        );
    }
    records.push(prefix.make(receipt, state, ExecutionControl::default(), ordinal));
}
fn resets(count: usize) -> Vec<Record> {
    let mut prefix = Prefix::default();
    (1..=count)
        .map(|i| {
            prefix.make(
                synthetic_receipt(i as u64, 0, OWNER, ControlAction::Reset, 0, false),
                ControlState::default(),
                ExecutionControl::default(),
                i as u64,
            )
        })
        .collect()
}
fn decode(records: &[Record]) -> Vec<DecodedCandidate> {
    records
        .iter()
        .map(|r| {
            let candidate = codec::decode(
                &serde_json::to_vec(r).unwrap(),
                &r.receipt.entry_id,
                CUSTOM_TYPE,
                &read_policy(&r.epoch.session_id),
            )
            .expect("fixture must pass local codec before transition checks");
            assert_eq!(candidate.record(), r);
            candidate
        })
        .collect()
}
fn canonical(records: &[Record]) -> Vec<DecodedCandidate> {
    let candidates = decode(records);
    let session = records
        .first()
        .map_or(SESSION, |r| r.epoch.session_id.as_str());
    assert_eq!(
        inspect_chain(&candidates, &read_policy(session))
            .unwrap()
            .receipts(),
        records.len() as u64
    );
    candidates
}
fn reject(records: &[Record], expected: TransitionError) {
    let candidates = canonical(records); // A valid map is not a valid business transition.
    assert_eq!(
        inspect_control_transitions(&candidates, &policy(SESSION)).unwrap_err(),
        expected
    );
}
fn queue_state(revision: u64, status: QueueStatus, owner: Option<&str>) -> ControlState {
    ControlState {
        revision,
        next_queue_id: 1,
        workflow: None,
        queue: vec![QueueItem {
            id: "item-1".into(),
            text: "manual".into(),
            status,
            created_revision: 1,
            updated_revision: revision,
            note: String::new(),
            claimed_by: owner.map(str::to_owned),
        }],
    }
}
fn claimed_trace() -> Vec<Record> {
    let mut records = Vec::new();
    append(
        &mut records,
        OWNER,
        ControlAction::Enqueue {
            text: "manual".into(),
        },
        queue_state(1, QueueStatus::Queued, None),
        true,
    );
    append(
        &mut records,
        CHILD,
        ControlAction::Claim {
            item: " item-1 ".into(),
        },
        queue_state(2, QueueStatus::Claimed, Some(CHILD)),
        true,
    );
    records
}

#[test]
fn policy_requires_one_to_256_distinct_fixed_actor_ids() {
    assert_eq!(
        TransitionPolicy::new(read_policy(SESSION), vec![]).unwrap_err(),
        TransitionError::Policy
    );
    for duplicates in [
        vec![
            DurableActor::owner(OWNER).unwrap(),
            DurableActor::owner(OWNER).unwrap(),
        ],
        vec![
            DurableActor::owner(OWNER).unwrap(),
            DurableActor::child(OWNER).unwrap(),
        ],
        vec![
            DurableActor::child(CHILD).unwrap(),
            DurableActor::child(CHILD).unwrap(),
        ],
    ] {
        assert_eq!(
            TransitionPolicy::new(read_policy(SESSION), duplicates).unwrap_err(),
            TransitionError::Policy
        );
    }
    assert_eq!(MAX_POLICY_ACTORS, 256);
    let mut roster: Vec<_> = (0..256)
        .map(|i| DurableActor::child(format!("child-{i}")).unwrap())
        .collect();
    let fixed = TransitionPolicy::new(read_policy(SESSION), roster.clone()).unwrap();
    roster.push(DurableActor::owner(OWNER).unwrap());
    assert_eq!(
        TransitionPolicy::new(read_policy(SESSION), roster).unwrap_err(),
        TransitionError::Policy
    );
    let candidates = canonical(&resets(1));
    for p in [fixed.clone(), fixed] {
        assert_eq!(
            inspect_control_transitions(&candidates, &p).unwrap_err(),
            TransitionError::Permission
        );
    }
    assert!(TransitionPolicy::new(
        read_policy(SESSION),
        vec![DurableActor::owner(OWNER).unwrap()]
    )
    .is_ok());
}

#[test]
fn empty_noop_and_exact_chain_record_limit() {
    let p = policy(SESSION);
    let empty = inspect_control_transitions(&[], &p).unwrap();
    assert_eq!(empty.state(), &ControlState::default());
    assert_eq!(empty.ledger().receipts(), 0);
    assert!(empty.ledger().root().is_none());
    assert_eq!(empty.ledger().epoch().epoch_id, EPOCH);
    assert_eq!(MAX_CHAIN_RECORDS, 256);
    let records = resets(257);
    let candidates = decode(&records);
    let result = inspect_control_transitions(&candidates[..256], &p).unwrap();
    assert_eq!(result.state(), &ControlState::default());
    assert_eq!(result.ledger().receipts(), 256);
    assert_eq!(result.ledger().root(), Some(&records[255].root));
    assert_eq!(
        inspect_control_transitions(&candidates, &p).unwrap_err(),
        TransitionError::Chain(ChainError::Limit)
    );
}

#[test]
fn trusted_claim_stamp_finish_and_cancel_owner_rules_match_host() {
    let records = claimed_trace();
    let inspected = inspect_control_transitions(&canonical(&records), &policy(SESSION)).unwrap();
    assert_eq!(
        inspected.state(),
        &queue_state(2, QueueStatus::Claimed, Some(CHILD))
    );
    for (actor, terminal, permitted) in [
        (CHILD, QueueStatus::Completed, true),
        (CHILD, QueueStatus::Failed, true),
        (OWNER, QueueStatus::Completed, false),
        (OTHER, QueueStatus::Completed, false),
        (CHILD, QueueStatus::Cancelled, true),
        (OWNER, QueueStatus::Cancelled, true),
        (OTHER, QueueStatus::Cancelled, false),
    ] {
        let mut branch = records.clone();
        let mut state = queue_state(3, terminal, Some(CHILD));
        let action = if terminal == QueueStatus::Cancelled {
            state.queue[0].note = "cancelled".into();
            ControlAction::Cancel {
                item: " item-1 ".into(),
                note: None,
            }
        } else {
            ControlAction::Finish {
                item: "item-1".into(),
                status: terminal,
                note: None,
            }
        };
        append(&mut branch, actor, action, state.clone(), true);
        let candidates = canonical(&branch);
        if permitted {
            assert_eq!(
                inspect_control_transitions(&candidates, &policy(SESSION))
                    .unwrap()
                    .state(),
                &state
            );
        } else {
            assert_eq!(
                inspect_control_transitions(&candidates, &policy(SESSION)).unwrap_err(),
                TransitionError::Permission
            );
        }
    }
    // A child can cancel an unclaimed item; ownership restriction is claimed-only.
    let mut queued = records[..1].to_vec();
    let mut state = queue_state(2, QueueStatus::Cancelled, None);
    state.queue[0].note = "cancelled".into();
    append(
        &mut queued,
        OTHER,
        ControlAction::Cancel {
            item: "item-1".into(),
            note: None,
        },
        state.clone(),
        true,
    );
    assert_eq!(
        inspect_control_transitions(&canonical(&queued), &policy(SESSION))
            .unwrap()
            .state(),
        &state
    );
}

#[test]
fn fixed_policy_not_receipt_claims_controls_unknown_and_child_roles() {
    for (actor, action, state, changed) in [
        (
            "unknown",
            ControlAction::Reset,
            ControlState::default(),
            false,
        ),
        (CHILD, ControlAction::Reset, ControlState::default(), false),
        (CHILD, start_action(), workflow_state(), true),
    ] {
        let mut records = Vec::new();
        append(&mut records, actor, action, state, changed);
        reject(&records, TransitionError::Permission);
    }
    let mut records = Vec::new();
    append(&mut records, OWNER, start_action(), workflow_state(), true);
    let candidates = canonical(&records);
    let owner_policy = policy(SESSION);
    let changed_roles = TransitionPolicy::new(
        read_policy(SESSION),
        vec![DurableActor::child(OWNER).unwrap()],
    )
    .unwrap();
    assert!(inspect_control_transitions(&candidates, &owner_policy.clone()).is_ok());
    assert_eq!(
        inspect_control_transitions(&candidates, &changed_roles).unwrap_err(),
        TransitionError::Permission
    );
    assert!(inspect_control_transitions(&candidates, &owner_policy).is_ok());
}

#[test]
fn locally_valid_canonical_state_changed_revision_and_stamp_forgeries_fail() {
    let baseline = claimed_trace();
    for attack in 0..6 {
        let mut records = baseline.clone();
        let last = records.last_mut().unwrap();
        match attack {
            0 => last.state.queue[0].text = "forged text".into(),
            1 => last.state.next_queue_id = 9,
            2 => last.state.queue[0].created_revision = 2,
            3 => last.state.queue[0].updated_revision = 1,
            4 => last.state.queue[0].claimed_by = Some(OWNER.into()),
            5 => last.state.queue[0].claimed_by = None,
            _ => unreachable!(),
        }
        reject(&rebuild(&records), TransitionError::Transition);
    }
    // Inventing a change on Reset is locally consistent when all three fields agree.
    let mut phantom = resets(1);
    phantom[0].state.revision = 1;
    phantom[0].receipt.control_revision = 1;
    phantom[0].receipt.changed = true;
    reject(&rebuild(&phantom), TransitionError::Transition);
    // Suppressing a real workflow change also passes local receipt arithmetic.
    let mut suppressed = Vec::new();
    let mut state = workflow_state();
    state.revision = 0;
    append(&mut suppressed, OWNER, start_action(), state, false);
    reject(&suppressed, TransitionError::Transition);
    // An unchanged Reset cannot silently erase or introduce state either.
    let mut omitted = Vec::new();
    append(
        &mut omitted,
        OWNER,
        start_action(),
        ControlState::default(),
        false,
    );
    reject(&omitted, TransitionError::Transition);
}

#[test]
fn canonical_maps_do_not_validate_illegal_domain_transitions() {
    for action in [
        ControlAction::Complete,
        ControlAction::Resume,
        ControlAction::Pause { reason: None },
        ControlAction::Fail { reason: None },
        ControlAction::Claim {
            item: "item-1".into(),
        },
        ControlAction::Finish {
            item: "item-1".into(),
            status: QueueStatus::Completed,
            note: None,
        },
        ControlAction::Cancel {
            item: "item-1".into(),
            note: None,
        },
        ControlAction::Enqueue { text: " ".into() },
    ] {
        let mut records = Vec::new();
        append(&mut records, OWNER, action, ControlState::default(), false);
        reject(&records, TransitionError::Transition);
    }
    let mut incomplete = Vec::new();
    append(
        &mut incomplete,
        OWNER,
        start_action(),
        workflow_state(),
        true,
    );
    append(
        &mut incomplete,
        CHILD,
        ControlAction::Complete,
        workflow_state(),
        false,
    );
    reject(&incomplete, TransitionError::Transition);
    let mut unclaimed = claimed_trace()[..1].to_vec();
    append(
        &mut unclaimed,
        OWNER,
        ControlAction::Finish {
            item: "item-1".into(),
            status: QueueStatus::Completed,
            note: None,
        },
        queue_state(1, QueueStatus::Queued, None),
        false,
    );
    reject(&unclaimed, TransitionError::Transition);
}

#[test]
fn execution_actions_and_nondefault_execution_control_always_fail_closed() {
    for actor in [OWNER, CHILD, "unknown"] {
        for action in [ControlAction::Recover, ControlAction::ForgetRun] {
            let mut records = resets(1);
            records[0].receipt.action = action;
            // run-1 is a legal native run ID; no provider/native run is fabricated.
            records[0].receipt.run_id = Some("run-1".into());
            records[0].receipt.actor = actor.into();
            reject(&rebuild(&records), TransitionError::ExecutionFactsRequired);
        }
    }
    for execution in [
        ExecutionControl {
            forgotten_run_ids: vec!["run-1".into()],
            recovery_request: None,
        },
        ExecutionControl {
            forgotten_run_ids: vec![],
            recovery_request: Some(RecoveryRequest {
                run_id: "run-1".into(),
                started_seq: 1,
                observed_history_revision: 7,
                unknown_invocation_ids: vec![],
            }),
        },
    ] {
        let mut records = resets(1);
        records[0].execution_control = execution;
        reject(&rebuild(&records), TransitionError::ExecutionFactsRequired);
    }
    // A valid preceding control prefix does not authorize a later run-bearing record.
    let mut records = resets(2);
    records[1].receipt.action = ControlAction::Recover;
    records[1].receipt.run_id = Some("run-1".into());
    reject(&rebuild(&records), TransitionError::ExecutionFactsRequired);
}

#[test]
fn complete_chain_validation_precedes_actor_and_transition_replay() {
    let mut records = resets(2);
    records[0].receipt.actor = "unknown".into();
    let mut records = rebuild(&records);
    records[1].epoch.epoch_id = "sibling-epoch".into();
    let candidates: Vec<_> = records
        .iter()
        .map(|r| {
            let p = ReadPolicy::new_epoch(SESSION, "main", &r.epoch.epoch_id).unwrap();
            codec::decode(
                &serde_json::to_vec(r).unwrap(),
                &r.receipt.entry_id,
                CUSTOM_TYPE,
                &p,
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        inspect_control_transitions(&candidates, &policy(SESSION)).unwrap_err(),
        TransitionError::Chain(ChainError::Epoch)
    );
    let mut sibling = rebuild(&resets(2));
    sibling[1].receipt.expected.entry_id = Some("sibling-entry".into());
    sibling = rebuild(&sibling);
    assert_eq!(
        inspect_control_transitions(&decode(&sibling), &policy(SESSION)).unwrap_err(),
        TransitionError::Chain(ChainError::Chain)
    );
    // An extra well-shaped but unreachable node passes L1; exact batch equality rejects it.
    let mut bad = resets(2);
    bad[0].receipt.actor = "unknown".into();
    bad = rebuild(&bad);
    let last = &mut bad[1];
    let mut extra = last.nodes[0].clone();
    extra.address.slot = last.nodes.len() as u16;
    last.nodes.push(extra);
    let candidates = decode(&bad);
    assert_eq!(
        inspect_control_transitions(&candidates, &policy(SESSION)).unwrap_err(),
        TransitionError::Chain(ChainError::Ledger)
    );
}

#[test]
fn repeated_failures_do_not_mutate_input_or_poison_later_reads() {
    let good = claimed_trace();
    let good_candidates = canonical(&good);
    let mut bad = good.clone();
    bad[1].state.queue[0].claimed_by = Some(OTHER.into());
    let bad = rebuild(&bad);
    let candidates = canonical(&bad);
    let before: Vec<_> = candidates
        .iter()
        .map(|c| serde_json::to_vec(c.record()).unwrap())
        .collect();
    let p = policy(SESSION);
    for _ in 0..3 {
        assert_eq!(
            inspect_control_transitions(&candidates, &p.clone()).unwrap_err(),
            TransitionError::Transition
        );
        assert_eq!(
            inspect_control_transitions(&good_candidates, &p)
                .unwrap()
                .state(),
            &good[1].state
        );
    }
    let after: Vec<_> = candidates
        .iter()
        .map(|c| serde_json::to_vec(c.record()).unwrap())
        .collect();
    assert_eq!(before, after);
    assert_eq!(
        good_candidates
            .iter()
            .map(|c| c.record())
            .collect::<Vec<_>>(),
        good.iter().collect::<Vec<_>>()
    );
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("synthetic operation exceeded 15s")
}
async fn harness(storage: Arc<dyn SessionStorage>) -> AgentHarness {
    bounded(AgentHarness::create(AgentHarnessOptions {
        session: Session::new(storage, None),
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    }))
    .await
    .unwrap()
}
async fn native_trace(h: &AgentHarness) -> Vec<DurableOutcome> {
    let owner = DurableService::bind(h, DurableActor::owner(OWNER).unwrap());
    let child = DurableService::bind(h, DurableActor::child(CHILD).unwrap());
    let other = DurableService::bind(h, DurableActor::child(OTHER).unwrap());
    let steps = vec![
        (
            0,
            json!({"action":"start","name":"work","steps":[{"id":"s","title":"Verify"}]}),
        ),
        (
            1,
            json!({"action":"checkpoint","step":"s","status":"in_progress","note":"checked"}),
        ),
        (
            1,
            json!({"action":"checkpoint","step":"s","status":"in_progress","note":"checked"}),
        ),
        (2, json!({"action":"pause","reason":"wait"})),
        (0, json!({"action":"resume"})),
        (
            1,
            json!({"action":"checkpoint","step":"s","status":"completed"}),
        ),
        (2, json!({"action":"complete"})),
        (
            0,
            json!({"action":"start","name":"replacement","steps":[{"id":"t","title":"Next"}]}),
        ),
        (1, json!({"action":"fail"})),
        (0, json!({"action":"reset"})),
        (0, json!({"action":"reset"})),
        (1, json!({"action":"enqueue","text":"first"})),
        (1, json!({"action":"claim","item":" item-1 "})),
        (
            1,
            json!({"action":"finish","item":"item-1","status":"completed","note":"done"}),
        ),
        (2, json!({"action":"enqueue","text":"second"})),
        (2, json!({"action":"claim","item":"item-2"})),
        (0, json!({"action":"cancel","item":"item-2"})),
        (0, json!({"action":"enqueue","text":"third"})),
        (1, json!({"action":"cancel","item":"item-3"})),
        (1, json!({"action":"enqueue","text":"fourth"})),
        (0, json!({"action":"claim","item":"item-4"})),
        (
            0,
            json!({"action":"finish","item":"item-4","status":"failed"}),
        ),
    ];
    let services = [&owner, &child, &other];
    let mut outcomes = Vec::new();
    for (i, (role, value)) in steps.into_iter().enumerate() {
        let service = services[role];
        let before = bounded(service.snapshot()).await.unwrap();
        let outcome = bounded(service.mutate(DurableMutation {
            request_id: format!("native-{i:04}"),
            expected: before.scope.clone(),
            action: action(value),
            run_id: None,
        }))
        .await
        .unwrap();
        assert!(!outcome.replayed);
        assert_eq!(outcome.receipt.expected, before.scope);
        assert_eq!(
            outcome.receipt.changed,
            outcome.snapshot.state != before.state
        );
        assert_eq!(bounded(owner.snapshot()).await.unwrap(), outcome.snapshot);
        outcomes.push(outcome);
    }
    outcomes
}
fn audit_native_trace(outcomes: &[DurableOutcome]) -> Vec<Record> {
    assert_eq!(outcomes.len(), 22);
    assert!(!outcomes[2].receipt.changed);
    assert!(!outcomes[10].receipt.changed);
    let revisions = [
        1, 2, 2, 3, 4, 5, 6, 7, 8, 9, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ];
    let mut prefix = Prefix::default();
    let records: Vec<_> = outcomes
        .iter()
        .enumerate()
        .map(|(i, outcome)| {
            assert_eq!(outcome.receipt.control_revision, revisions[i]);
            assert_eq!(outcome.snapshot.state.revision, revisions[i]);
            assert_eq!(
                outcome.snapshot.scope.entry_id.as_deref(),
                Some(outcome.receipt.entry_id.as_str())
            );
            assert_eq!(
                outcome.snapshot.execution_control,
                ExecutionControl::default()
            );
            // Preserve exact native receipt, expected scope, entry ID and full state.
            prefix.make(
                outcome.receipt.clone(),
                outcome.snapshot.state.clone(),
                outcome.snapshot.execution_control.clone(),
                i as u64 + 1,
            )
        })
        .collect();
    let candidates = canonical(&records);
    let p = policy(&records[0].epoch.session_id);
    for (i, outcome) in outcomes.iter().enumerate() {
        let result = inspect_control_transitions(&candidates[..=i], &p).unwrap();
        assert_eq!(result.state(), &outcome.snapshot.state);
        assert_eq!(result.ledger().receipts(), i as u64 + 1);
        assert_eq!(result.ledger().root(), Some(&records[i].root));
    }
    let state = &outcomes.last().unwrap().snapshot.state;
    assert!(state.workflow.is_none());
    assert_eq!(state.next_queue_id, 4);
    let expected = [
        (QueueStatus::Completed, 10, 12, Some(CHILD), "done"),
        (QueueStatus::Cancelled, 13, 15, Some(OTHER), "cancelled"),
        (QueueStatus::Cancelled, 16, 17, None, "cancelled"),
        (QueueStatus::Failed, 18, 20, Some(OWNER), ""),
    ];
    assert_eq!(state.queue.len(), expected.len());
    for (item, (status, created, updated, owner, note)) in state.queue.iter().zip(expected) {
        assert_eq!(
            (
                item.status,
                item.created_revision,
                item.updated_revision,
                item.claimed_by.as_deref(),
                item.note.as_str()
            ),
            (status, created, updated, owner, note)
        );
    }
    records
}

#[tokio::test]
async fn memory_service_trace_is_an_independent_full_state_oracle() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let h = harness(AgentHarnessOptions::default().session.storage()).await;
        let outcomes = native_trace(&h).await;
        let before =
            bounded(DurableService::bind(&h, DurableActor::owner(OWNER).unwrap()).snapshot())
                .await
                .unwrap();
        audit_native_trace(&outcomes);
        assert_eq!(
            bounded(DurableService::bind(&h, DurableActor::owner(OWNER).unwrap()).snapshot())
                .await
                .unwrap(),
            before
        );
        bounded(h.drain_session()).await;
    })
    .await
    .expect("memory fixture exceeded 30s");
}

#[tokio::test]
async fn jsonl_service_reopen_and_readonly_inspection_preserve_original_v1_bytes() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = bounded(JsonlSessionStorage::create(
            fs.clone(),
            "control.jsonl",
            JsonlV4Header::new(SESSION.into(), 1, "/synthetic".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ))
        .await
        .unwrap();
        let h = harness(Arc::new(store)).await;
        let outcomes = native_trace(&h).await;
        bounded(h.drain_session()).await;
        drop(h);
        let path = dir.path().join("control.jsonl");
        let before = std::fs::read(&path).unwrap();
        let records = audit_native_trace(&outcomes);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let store = bounded(JsonlSessionStorage::load(
            fs,
            "control.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ))
        .await
        .unwrap();
        let h = harness(Arc::new(store)).await;
        let service = DurableService::bind(&h, DurableActor::owner(OWNER).unwrap());
        let reopened = bounded(service.snapshot()).await.unwrap();
        assert_eq!(reopened.state, outcomes.last().unwrap().snapshot.state);
        assert_eq!(
            reopened.scope.entry_id,
            outcomes.last().unwrap().snapshot.scope.entry_id
        );
        assert_ne!(
            reopened.scope.binding_token,
            outcomes.last().unwrap().snapshot.scope.binding_token
        );
        let result = inspect_control_transitions(&canonical(&records), &policy(SESSION)).unwrap();
        assert_eq!(result.state(), &reopened.state);
        assert_eq!(bounded(service.snapshot()).await.unwrap(), reopened);
        bounded(h.drain_session()).await;
        assert_eq!(std::fs::read(&path).unwrap(), before);
    })
    .await
    .expect("JSONL fixture exceeded 30s");
}
