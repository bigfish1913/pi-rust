//! Synthetic L2 receipt-map contracts, not journal provenance, business replay,
//! actor authorization, ExecutionControl validation, or run-proof authority.
//! Raw Session appends below are fixture construction only, never writer admission.
use rpi_durable::{
    control::{ControlAction, ControlState},
    execution::ExecutionControl,
    ledger::{
        insert::{Insert, InsertStep, MAX_CREATED_NODES},
        Address, Node, NodeRef, ReceiptRef,
    },
};
use rpi_harness::{
    durable::{
        experimental_ledger::{
            self as codec,
            chain::{inspect_chain, ChainError, MAX_CHAIN_NODES, MAX_CHAIN_RECORDS},
            source::{read_branch, SourceCandidate},
            DecodedCandidate, Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE,
            FORMAT,
        },
        DurableReceipt, DurableScope,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

const SESSION: &str = "synthetic-chain";
const EPOCH: &str = "epoch-1";
type Nodes = HashMap<(String, u16), Node>;

fn policy(session: &str) -> ReadPolicy {
    ReadPolicy::new_epoch(session, "main", EPOCH).unwrap()
}
fn epoch(session: &str) -> Epoch {
    Epoch {
        session_id: session.into(),
        lane: "main".into(),
        epoch_id: EPOCH.into(),
    }
}
fn receipt(session: &str, key: &str, ordinal: u64) -> DurableReceipt {
    DurableReceipt {
        request_id: key.into(),
        actor: "synthetic-owner".into(),
        expected: DurableScope {
            session_id: session.into(),
            lane: "main".into(),
            binding_token: "synthetic-binding".into(),
            leaf_id: None,
            history_revision: ordinal * 7,
            history_generation: 0,
            entry_id: (ordinal > 1).then(|| format!("entry-{}", ordinal - 1)),
            control_revision: 0,
        },
        action: ControlAction::Reset,
        run_id: None,
        entry_id: format!("entry-{ordinal}"),
        control_revision: 0,
        changed: false,
    }
}

#[derive(Clone, Default)]
struct Prefix {
    nodes: Nodes,
    root: Option<NodeRef>,
}
impl Prefix {
    // Resolve only the borrowed, already constructed prefix; never future nodes
    // or an absent-node fallback. All Insert intermediates are retained verbatim.
    fn make(
        &mut self,
        session: &str,
        receipt: DurableReceipt,
        state: ControlState,
        execution_control: ExecutionControl,
        ordinal: u64,
    ) -> Record {
        let value = codec::receipt_reference(&receipt, ordinal).unwrap();
        let mut insert = Insert::new(
            &receipt.request_id,
            value,
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
                        .expect("requested prefix node must exist");
                    insert.provide(node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("unexpected insertion state: {other:?}"),
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
            epoch: epoch(session),
            ordinal,
            previous: receipt.expected.entry_id.clone().map(|entry_id| Previous {
                entry_id,
                ordinal: ordinal - 1,
            }),
            state,
            execution_control,
            receipt,
            nodes: batch.nodes,
            root: batch.root,
        }
    }
}
fn sequence(session: &str, keys: &[String]) -> Vec<Record> {
    let mut prefix = Prefix::default();
    keys.iter()
        .enumerate()
        .map(|(i, key)| {
            let ordinal = i as u64 + 1;
            prefix.make(
                session,
                receipt(session, key, ordinal),
                ControlState::default(),
                ExecutionControl::default(),
                ordinal,
            )
        })
        .collect()
}
fn keys(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).into()).collect()
}
fn decoded(records: &[Record], session: &str) -> Vec<DecodedCandidate> {
    records
        .iter()
        .map(|record| {
            let candidate = codec::decode(
                &serde_json::to_vec(record).unwrap(),
                &record.receipt.entry_id,
                CUSTOM_TYPE,
                &policy(session),
            )
            .expect("attack and normal fixtures must pass local decode before chain inspection");
            assert_eq!(candidate.record(), record);
            candidate
        })
        .collect()
}
fn reject(records: &[Record], error: ChainError) {
    let candidates = decoded(records, SESSION); // Explicitly establish the L1/L2 boundary.
    assert_eq!(
        inspect_chain(&candidates, &policy(SESSION)).unwrap_err(),
        error
    );
}
fn rehash(record: &mut Record) {
    let mut refs: Vec<NodeRef> = Vec::new();
    for node in &mut record.nodes {
        for child in [&mut node.left, &mut node.right].into_iter().flatten() {
            if child.address.entry_id == record.receipt.entry_id {
                *child = refs[usize::from(child.address.slot)].clone();
            }
        }
        refs.push(node.reference(&Sha256NodeHasher).unwrap());
    }
    record.root = refs[usize::from(record.root.address.slot)].clone();
}
fn refresh_receipt(record: &mut Record) {
    let value = codec::receipt_reference(&record.receipt, record.ordinal).unwrap();
    for node in &mut record.nodes {
        if node.value.entry_id == record.receipt.entry_id {
            node.value = value.clone();
        }
    }
    rehash(record);
}
fn all_nodes(records: &[Record]) -> Nodes {
    records
        .iter()
        .flat_map(|r| &r.nodes)
        .map(|n| ((n.address.entry_id.clone(), n.address.slot), n.clone()))
        .collect()
}

// Independent traversal/lookup oracle: no production Lookup or Insert is used
// to infer expected key membership, search order, AVL balance or reachability.
fn audit_tree(root: &NodeRef, nodes: &Nodes, expected: &BTreeMap<String, ReceiptRef>) {
    fn walk(
        reference: &NodeRef,
        nodes: &Nodes,
        low: Option<&str>,
        high: Option<&str>,
        seen: &mut HashSet<(String, u16)>,
        found: &mut BTreeMap<String, ReceiptRef>,
    ) -> (u8, u64) {
        let identity = (reference.address.entry_id.clone(), reference.address.slot);
        assert!(
            seen.insert(identity.clone()),
            "reachable tree is not cyclic or shared"
        );
        let node = nodes
            .get(&identity)
            .expect("every reachable reference resolves");
        assert_eq!(node.address, reference.address);
        let digest: [u8; 32] = Sha256::digest(node.canonical_bytes().unwrap()).into();
        assert_eq!(reference.digest, digest);
        assert!(low.is_none_or(|bound| bound < node.key.as_str()));
        assert!(high.is_none_or(|bound| node.key.as_str() < bound));
        let (lh, lc) = node.left.as_ref().map_or((0, 0), |r| {
            walk(r, nodes, low, Some(&node.key), seen, found)
        });
        let (rh, rc) = node.right.as_ref().map_or((0, 0), |r| {
            walk(r, nodes, Some(&node.key), high, seen, found)
        });
        assert!(lh.abs_diff(rh) <= 1);
        assert_eq!(reference.height, 1 + lh.max(rh));
        assert_eq!(reference.count, 1 + lc + rc);
        assert!(found.insert(node.key.clone(), node.value.clone()).is_none());
        (reference.height, reference.count)
    }
    let mut found = BTreeMap::new();
    walk(root, nodes, None, None, &mut HashSet::new(), &mut found);
    assert_eq!(&found, expected);
    for (key, value) in expected {
        let mut cursor = Some(root);
        loop {
            let r = cursor.expect("independent lookup finds every receipt");
            let n = &nodes[&(r.address.entry_id.clone(), r.address.slot)];
            match key.cmp(&n.key) {
                std::cmp::Ordering::Less => cursor = n.left.as_ref(),
                std::cmp::Ordering::Greater => cursor = n.right.as_ref(),
                std::cmp::Ordering::Equal => {
                    assert_eq!(&n.value, value);
                    break;
                }
            }
        }
    }
}
fn expected_map(records: &[Record]) -> BTreeMap<String, ReceiptRef> {
    records
        .iter()
        .map(|r| {
            (
                r.receipt.request_id.clone(),
                codec::receipt_reference(&r.receipt, r.ordinal).unwrap(),
            )
        })
        .collect()
}
fn inspect_and_audit(records: &[Record], session: &str) {
    let candidates = decoded(records, session);
    let result = inspect_chain(&candidates, &policy(session)).unwrap();
    assert_eq!(result.epoch(), &epoch(session));
    assert_eq!(result.receipts(), records.len() as u64);
    assert_eq!(result.root(), records.last().map(|r| &r.root));
    if let Some(root) = result.root() {
        audit_tree(root, &all_nodes(records), &expected_map(records));
    }
}

#[test]
fn empty_input_is_an_empty_relative_map_and_epoch_is_fixed() {
    let result = inspect_chain(&[], &policy(SESSION)).unwrap();
    assert_eq!(result.epoch(), &epoch(SESSION));
    assert_eq!(result.receipts(), 0);
    assert_eq!(result.root(), None);
    let records = sequence(SESSION, &keys(&["a"]));
    let candidates = decoded(&records, SESSION);
    for p in [
        policy("other-session"),
        ReadPolicy::new_epoch(SESSION, "side", EPOCH).unwrap(),
        ReadPolicy::new_epoch(SESSION, "main", "other-epoch").unwrap(),
    ] {
        assert_eq!(
            inspect_chain(&candidates, &p).unwrap_err(),
            ChainError::Epoch
        );
    }
    // Empty input is not evidence that any native history is empty.
}

#[test]
fn ascending_descending_and_fixed_shuffle_256_match_independent_map() {
    assert_eq!(MAX_CHAIN_RECORDS, 256);
    assert_eq!(MAX_CHAIN_NODES, 16_384);
    let ascending: Vec<String> = (0..256).map(|i| format!("request-{i:03}")).collect();
    let descending: Vec<String> = ascending.iter().rev().cloned().collect();
    let shuffled: Vec<String> = (0..256)
        .map(|i| ascending[(i * 73 + 19) % 256].clone())
        .collect();
    for order in [ascending, descending, shuffled] {
        let records = sequence(SESSION, &order);
        assert!(records.iter().map(|r| r.nodes.len()).sum::<usize>() <= MAX_CHAIN_NODES);
        inspect_and_audit(&records, SESSION);
    }
}

#[test]
fn all_four_rotations_preserve_old_roots_and_serde_restart() {
    for order in [
        ["c", "b", "a"],
        ["a", "b", "c"],
        ["c", "a", "b"],
        ["a", "c", "b"],
    ] {
        let records = sequence(SESSION, &keys(&order));
        inspect_and_audit(&records, SESSION);
        let nodes = all_nodes(&records);
        for i in 0..records.len() {
            audit_tree(&records[i].root, &nodes, &expected_map(&records[..=i]));
            inspect_and_audit(&records[..=i], SESSION);
        }
        let final_node = &nodes[&(
            records[2].root.address.entry_id.clone(),
            records[2].root.address.slot,
        )];
        assert_eq!(final_node.key, "b");
        assert_eq!((records[2].root.height, records[2].root.count), (2, 3));
        let restarted: Vec<Record> =
            serde_json::from_slice(&serde_json::to_vec(&records).unwrap()).unwrap();
        inspect_and_audit(&restarted, SESSION);
        assert_eq!(
            inspect_chain(&decoded(&records, SESSION), &policy(SESSION))
                .unwrap()
                .root(),
            inspect_chain(&decoded(&restarted, SESSION), &policy(SESSION))
                .unwrap()
                .root()
        );
    }
}

fn pad_nodes(record: &mut Record, target: usize) {
    let value = codec::receipt_reference(&record.receipt, record.ordinal).unwrap();
    while record.nodes.len() < target {
        let slot = u16::try_from(record.nodes.len()).unwrap();
        record.nodes.push(Node {
            address: Address {
                entry_id: record.receipt.entry_id.clone(),
                ordinal: record.ordinal,
                slot,
            },
            key: format!("unused-{slot}"),
            value: value.clone(),
            left: None,
            right: None,
        });
    }
}

#[test]
fn extra_unused_node_is_locally_accepted_but_not_a_canonical_batch() {
    let mut records = sequence(SESSION, &keys(&["a"]));
    let root = records[0].root.clone();
    pad_nodes(&mut records[0], 2);
    assert_eq!(
        records[0].root, root,
        "current lookup and root are untouched"
    );
    let candidates = decoded(&records, SESSION); // Must be decode Ok, not a codec rejection.
    assert_eq!(
        inspect_chain(&candidates, &policy(SESSION)).unwrap_err(),
        ChainError::Ledger
    );
}

#[test]
fn forged_old_receipt_and_off_lookup_child_digest_or_identity_are_rejected() {
    let records = sequence(SESSION, &keys(&["b", "a", "c"]));
    let root_slot = usize::from(records[2].root.address.slot);
    assert_eq!(records[2].nodes[root_slot].key, "b");
    assert_ne!(
        records[2].nodes[root_slot]
            .left
            .as_ref()
            .unwrap()
            .address
            .entry_id,
        "entry-3"
    );
    for attack in 0..3 {
        let mut changed = records.clone();
        let root = &mut changed[2].nodes[root_slot];
        match attack {
            0 => root.value.digest[0] ^= 1, // Recommitted forged historical receipt.
            1 => root.left.as_mut().unwrap().digest[0] ^= 1, // Off current c lookup.
            _ => root.left.as_mut().unwrap().address.entry_id = "missing-old-node".into(),
        }
        rehash(&mut changed[2]);
        assert_ne!(changed[2].root, records[2].root);
        reject(&changed, ChainError::Ledger);
    }
}

#[test]
fn same_epoch_sibling_splicing_cannot_substitute_the_prefix_root() {
    let left = sequence(SESSION, &keys(&["b", "a", "c"]));
    let right = sequence(SESSION, &keys(&["b", "z", "c"]));
    inspect_and_audit(&left, SESSION);
    inspect_and_audit(&right, SESSION);
    assert_eq!(left[2].previous, right[2].previous);
    assert_eq!(left[2].epoch, right[2].epoch);
    assert_ne!(left[2].root, right[2].root);
    let splice = vec![left[0].clone(), left[1].clone(), right[2].clone()];
    reject(&splice, ChainError::Ledger);
}

#[test]
fn omission_reorder_duplicate_request_and_duplicate_entry_fail_closed() {
    let records = sequence(SESSION, &keys(&["a", "b", "c"]));
    for invalid in [
        vec![records[1].clone()],
        vec![records[0].clone(), records[2].clone()],
        vec![records[1].clone(), records[0].clone()],
        vec![records[0].clone(), records[1].clone(), records[1].clone()],
    ] {
        reject(&invalid, ChainError::Chain);
    }
    // Locally valid second record from another map, but request a is already present.
    let other = sequence(SESSION, &keys(&["z", "a"]));
    reject(&[records[0].clone(), other[1].clone()], ChainError::Chain);
    // A locally shaped ordinal-3 tree with a reused non-adjacent entry ID.
    // The old child bodies are deliberately not proved by the local codec.
    let mut duplicate = records[2].clone();
    duplicate.receipt.entry_id = "entry-1".into();
    let value = codec::receipt_reference(&duplicate.receipt, 3).unwrap();
    let old = |id: &str, ordinal| NodeRef {
        address: Address {
            entry_id: id.into(),
            ordinal,
            slot: 0,
        },
        digest: [9; 32],
        height: 1,
        count: 1,
    };
    duplicate.nodes = vec![Node {
        address: Address {
            entry_id: "entry-1".into(),
            ordinal: 3,
            slot: 0,
        },
        key: "c".into(),
        value,
        left: Some(old("unproved-left", 1)),
        right: Some(old("unproved-right", 2)),
    }];
    duplicate.root = duplicate.nodes[0].reference(&Sha256NodeHasher).unwrap();
    reject(
        &[records[0].clone(), records[1].clone(), duplicate],
        ChainError::Chain,
    );
}

#[test]
fn expected_control_revision_must_link_but_noop_ordinal_is_not_native_history() {
    let mut prefix = Prefix::default();
    let mut records = Vec::new();
    for (i, (expected, changed, history)) in [(0, true, 13), (1, false, 57), (1, true, 91)]
        .into_iter()
        .enumerate()
    {
        let ordinal = i as u64 + 1;
        let mut r = receipt(SESSION, &format!("r-{ordinal}"), ordinal);
        r.expected.control_revision = expected;
        r.expected.history_revision = history;
        r.changed = changed;
        r.control_revision = expected + u64::from(changed);
        let mut state = ControlState::default();
        state.revision = r.control_revision;
        records.push(prefix.make(SESSION, r, state, ExecutionControl::default(), ordinal));
    }
    inspect_and_audit(&records, SESSION);
    assert_eq!(
        records.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        records.iter().map(|r| r.state.revision).collect::<Vec<_>>(),
        [1, 1, 2]
    );
    assert_eq!(
        records
            .iter()
            .map(|r| r.receipt.expected.history_revision)
            .collect::<Vec<_>>(),
        [13, 57, 91]
    );
    let mut divergent = records[..2].to_vec();
    divergent[1].receipt.expected.control_revision = 7;
    divergent[1].receipt.control_revision = 7;
    divergent[1].state.revision = 7;
    refresh_receipt(&mut divergent[1]);
    reject(&divergent, ChainError::Chain);
}

#[test]
fn record_and_node_limits_preflight_before_any_replay() {
    let order: Vec<String> = (0..257).map(|i| format!("r-{i:03}")).collect();
    let records = sequence(SESSION, &order);
    inspect_and_audit(&records[..256], SESSION);
    reject(&records, ChainError::Limit);
    let mut padded = records[..33].to_vec();
    for record in &mut padded {
        pad_nodes(record, MAX_CREATED_NODES);
    }
    assert_eq!(
        padded[..32].iter().map(|r| r.nodes.len()).sum::<usize>(),
        MAX_CHAIN_NODES
    );
    // Inclusive logical node bound: the first noncanonical batch is reached.
    reject(&padded[..32], ChainError::Ledger);
    assert!(padded.iter().map(|r| r.nodes.len()).sum::<usize>() > MAX_CHAIN_NODES);
    // Every record locally decodes, and the very first would otherwise fail Ledger.
    reject(&padded, ChainError::Limit);
    let oversized = decoded(&padded, SESSION);
    assert_eq!(
        inspect_chain(&oversized, &policy("wrong-epoch")).unwrap_err(),
        ChainError::Limit
    );
}

#[test]
fn canonical_map_intentionally_does_not_prove_business_actor_or_run_authority() {
    let mut prefix = Prefix::default();
    let first = prefix.make(
        SESSION,
        receipt(SESSION, "a", 1),
        ControlState::default(),
        ExecutionControl::default(),
        1,
    );
    let mut r = receipt(SESSION, "b", 2);
    r.actor = "not-authorized-by-this-api".into();
    r.expected.binding_token = "untrusted-binding".into();
    r.expected.leaf_id = Some("unproved-native-parent".into());
    r.expected.history_revision = 999;
    // A no-op Reset cannot establish this workflow through business replay.
    // Locally valid state and execution claims nevertheless remain out of L2 scope.
    let state: ControlState = serde_json::from_value(json!({
        "revision":0,"nextQueueId":0,"queue":[],
        "workflow":{"name":"forged-business-state","status":"running",
                    "steps":[{"id":"step","title":"Unproved","status":"pending"}]}
    }))
    .unwrap();
    let execution: ExecutionControl = serde_json::from_value(json!({
        "forgottenRunIds":["unproved-run"],"recoveryRequest":null
    }))
    .unwrap();
    let second = prefix.make(SESSION, r, state, execution, 2);
    let records = [first, second];
    inspect_and_audit(&records, SESSION);
    assert_ne!(records[0].state, records[1].state);
    assert!(!records[1].receipt.changed);
    // Acceptance is the honest negative boundary, NOT a full-journal proof.
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("bounded synthetic operation")
}
async fn fixture<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("bounded synthetic fixture")
}
type Audit = (Vec<Entry>, Vec<LaneRecord>, u64, Vec<LanePointer>);
async fn audit(session: &Session) -> Audit {
    bounded(async {
        let storage = session.storage();
        (
            storage.find_entries(&EntryQuery::default()).await.unwrap(),
            storage.find_records(&RecordQuery::default()).await.unwrap(),
            storage.history_revision().await.unwrap(),
            storage.get_lanes().await.unwrap(),
        )
    })
    .await
}
async fn append_native(session: &Session, id: &str) {
    bounded(session.append_entry(
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::ActiveTools {
                active_tool_names: vec![],
            },
        },
        "main",
    ))
    .await
    .unwrap();
}
async fn append_record(session: &Session, record: &Record) {
    decoded(
        std::slice::from_ref(record),
        &session.storage().metadata().id,
    );
    bounded(session.append_entry(
        ProvisionedEntry {
            id: record.receipt.entry_id.clone(),
            kind: ProvisionedKind::Custom {
                custom_type: CUSTOM_TYPE.into(),
                data: Some(serde_json::to_value(record).unwrap()),
            },
        },
        "main",
    ))
    .await
    .unwrap();
}
async fn source(session: &Session, leaf: &str) -> SourceCandidate {
    bounded(read_branch(
        session,
        "main",
        Some(leaf),
        &policy(&session.storage().metadata().id),
    ))
    .await
    .unwrap()
}
fn source_root(candidate: &SourceCandidate, session: &str) -> NodeRef {
    let records: Vec<_> = candidate
        .records
        .iter()
        .map(|r| r.candidate.clone())
        .collect();
    let result = inspect_chain(&records, &policy(session)).unwrap();
    let raw: Vec<_> = records.iter().map(|r| r.record().clone()).collect();
    audit_tree(
        result.root().unwrap(),
        &all_nodes(&raw),
        &expected_map(&raw),
    );
    result.root().unwrap().clone()
}
async fn seed(session: &Session) -> Vec<Record> {
    let id = session.storage().metadata().id;
    let mut prefix = Prefix::default();
    let mut records = Vec::new();
    for ordinal in 1..=3 {
        if ordinal > 1 {
            append_native(session, &format!("native-{ordinal}")).await;
        }
        let mut r = receipt(&id, &format!("request-{ordinal}"), ordinal);
        r.expected.leaf_id = bounded(session.view("main").get_leaf_id()).await.unwrap();
        r.expected.history_revision = bounded(session.storage().history_revision()).await.unwrap();
        let record = prefix.make(
            &id,
            r,
            ControlState::default(),
            ExecutionControl::default(),
            ordinal,
        );
        append_record(session, &record).await;
        records.push(record);
    }
    records
}

#[tokio::test]
async fn memory_source_composition_is_readonly_and_public_scope_grants_no_authority() {
    fixture(async {
        let options = AgentHarnessOptions::default();
        assert!(
            !options.require_native_durable,
            "fixtures must not enable production admission"
        );
        let session = Session::new(options.session.storage(), None);
        let records = seed(&session).await;
        let id = session.storage().metadata().id;
        // Build a real same-lane sibling from the exact genesis resolver snapshot.
        bounded(session.move_lane("main", Some("entry-1")))
            .await
            .unwrap();
        let mut sibling_prefix = Prefix {
            nodes: all_nodes(&records[..1]),
            root: Some(records[0].root.clone()),
        };
        let mut sibling_receipt = receipt(&id, "sibling-request", 2);
        sibling_receipt.entry_id = "sibling-record".into();
        sibling_receipt.expected.leaf_id =
            bounded(session.view("main").get_leaf_id()).await.unwrap();
        sibling_receipt.expected.history_revision =
            bounded(session.storage().history_revision()).await.unwrap();
        let sibling_record = sibling_prefix.make(
            &id,
            sibling_receipt,
            ControlState::default(),
            ExecutionControl::default(),
            2,
        );
        append_record(&session, &sibling_record).await;
        let before = audit(&session).await;
        let mut candidate = source(&session, "entry-3").await;
        assert_eq!(candidate.inspected_entries, 5);
        assert_eq!(
            candidate.scope.current_leaf_id.as_deref(),
            Some("sibling-record")
        );
        assert_eq!(source_root(&candidate, &id), records[2].root);
        let sibling = source(&session, "sibling-record").await;
        assert_eq!(sibling.records.len(), 2);
        assert_eq!(source_root(&sibling, &id), sibling_record.root);
        assert_ne!(source_root(&sibling, &id), records[2].root);
        // These public envelopes are deliberately not inputs to inspect_chain.
        candidate.scope.session_id = "forged-session".into();
        candidate.scope.lane = "forged-lane".into();
        candidate.scope.binding_token = "forged-binding".into();
        candidate.scope.selected_leaf_id = None;
        candidate.scope.history_revision = u64::MAX;
        candidate.inspected_entries = 0;
        candidate.inspected_custom_entries = 0;
        candidate.other_custom_entries = usize::MAX;
        candidate.encoded_bytes = 0;
        candidate.records[0].entry.base.id = "forged-source-entry".into();
        assert_eq!(source_root(&candidate, &id), records[2].root);
        let reordered: Vec<_> = candidate
            .records
            .iter()
            .rev()
            .map(|r| r.candidate.clone())
            .collect();
        assert_eq!(
            inspect_chain(&reordered, &policy(&id)).unwrap_err(),
            ChainError::Chain
        );
        assert_eq!(audit(&session).await, before);
    })
    .await
}

#[tokio::test]
async fn tempdir_jsonl_source_reopen_preserves_root_lanes_revision_entries_and_bytes() {
    fixture(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let storage = bounded(JsonlSessionStorage::create(
            fs.clone(),
            "chain.jsonl",
            JsonlV4Header::new("chain-jsonl".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ))
        .await
        .unwrap();
        let session = Session::new(Arc::new(storage), None);
        let records = seed(&session).await;
        bounded(session.drain()).await;
        let before = audit(&session).await;
        let path = dir.path().join("chain.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let first = source(&session, "entry-3").await;
        let root = source_root(&first, "chain-jsonl");
        assert_eq!(root, records[2].root);
        assert_eq!(audit(&session).await, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(session);
        let storage = bounded(JsonlSessionStorage::load(
            fs,
            "chain.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ))
        .await
        .unwrap();
        let reopened = Session::new(Arc::new(storage), None);
        assert_eq!(audit(&reopened).await, before);
        let second = source(&reopened, "entry-3").await;
        assert_eq!(source_root(&second, "chain-jsonl"), root);
        assert_eq!(first.encoded_bytes, second.encoded_bytes);
        assert_eq!(first.inspected_entries, second.inspected_entries);
        assert_eq!(audit(&reopened).await, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    })
    .await
}
