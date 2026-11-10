//! Pure synthetic proofs only: no journal, I/O, authenticated root, or real receipts.
//! Validation covers the visited path and child summaries, NOT unvisited subtrees,
//! global BST completeness, session/epoch identity, or receipt/root provenance.
use rpi_durable::ledger::{
    Address, LedgerError, Lookup, LookupStep, Node, NodeHasher, NodeRef, ReceiptRef, MAX_ID_BYTES,
    MAX_NODE_BYTES, MAX_PATH_NODES,
};
use serde::Deserialize;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};

// Deliberately noncryptographic. This is NOT SHA-256, authentication, or security.
#[derive(Default)]
struct TestHash(Cell<usize>);
impl NodeHasher for TestHash {
    fn digest(&self, bytes: &[u8]) -> [u8; 32] {
        self.0.set(self.0.get() + 1);
        let mut out = [0u8; 32];
        for (i, byte) in bytes.iter().enumerate() {
            let lane = i % 32;
            out[lane] = out[lane].wrapping_mul(31).wrapping_add(*byte);
            out[(lane + 13) % 32] ^= (i as u8).wrapping_add(*byte);
        }
        out
    }
}

fn leaf(key: &str, ordinal: u64) -> Node {
    let entry_id = format!("entry-{ordinal}");
    Node {
        address: Address {
            entry_id: entry_id.clone(),
            ordinal,
            slot: 0,
        },
        key: key.into(),
        value: ReceiptRef {
            entry_id,
            ordinal,
            digest: [ordinal as u8; 32],
        },
        left: None,
        right: None,
    }
}

// Independent recursive median construction, NOT a production insert algorithm.
// Unique entries, slot zero, and strictly increasing postorder ordinals.
fn balanced(keys: &[String], nodes: &mut Vec<Node>, hash: &TestHash) -> Option<NodeRef> {
    if keys.is_empty() {
        return None;
    }
    let middle = keys.len() / 2;
    let left = balanced(&keys[..middle], nodes, hash);
    let right = balanced(&keys[middle + 1..], nodes, hash);
    let mut node = leaf(&keys[middle], nodes.len() as u64 + 1);
    node.left = left;
    node.right = right;
    let reference = node.reference(hash).unwrap();
    nodes.push(node);
    Some(reference)
}

fn resolve(
    key: &str,
    root: Option<NodeRef>,
    nodes: &[Node],
    hash: &TestHash,
) -> (Option<ReceiptRef>, usize) {
    let mut lookup = Lookup::new(key, root).unwrap();
    loop {
        match lookup.step() {
            LookupStep::NeedNode(reference) => {
                let node = &nodes[reference.address.ordinal as usize - 1];
                assert_eq!(node.address, reference.address);
                lookup.provide(node, hash).unwrap();
            }
            LookupStep::Found(value) => return (Some(value.clone()), lookup.visited_nodes()),
            LookupStep::Absent => return (None, lookup.visited_nodes()),
            LookupStep::Poisoned => panic!("valid synthetic tree poisoned"),
        }
        assert!(lookup.visited_nodes() <= MAX_PATH_NODES);
    }
}

fn poisoned(lookup: &mut Lookup, node: &Node, hash: &TestHash, error: LedgerError) {
    assert!(matches!(lookup.step(), LookupStep::NeedNode(_)));
    assert_eq!(lookup.provide(node, hash), Err(error));
    assert_eq!(lookup.step(), LookupStep::Poisoned);
    let visits = lookup.visited_nodes();
    let calls = hash.0.get();
    assert_eq!(lookup.provide(node, hash), Err(LedgerError::Terminal));
    assert_eq!(lookup.unavailable(), Err(LedgerError::Terminal));
    assert_eq!(lookup.step(), LookupStep::Poisoned);
    assert_eq!(lookup.visited_nodes(), visits);
    assert_eq!(hash.0.get(), calls);
}

#[test]
fn balanced_4096_matches_btree_oracle_before_and_after_json_roundtrip() {
    let hash = TestHash::default();
    let keys: Vec<_> = (0..4096).map(|i| format!("k{:05}", i * 2)).collect();
    // Separate iterative oracle: never copy expected receipts from fixture nodes
    // or derive expected answers by traversing production lookup/reference code.
    let mut oracle = BTreeMap::new();
    let mut stack = vec![(0, keys.len(), false)];
    let mut ordinal = 0u64;
    while let Some((start, end, emit)) = stack.pop() {
        if start == end {
            continue;
        }
        let middle = start + (end - start) / 2;
        if emit {
            ordinal += 1;
            oracle.insert(
                keys[middle].clone(),
                ReceiptRef {
                    entry_id: format!("entry-{ordinal}"),
                    ordinal,
                    digest: [ordinal as u8; 32],
                },
            );
        } else {
            stack.extend([
                (start, end, true),
                (middle + 1, end, false),
                (start, middle, false),
            ]);
        }
    }
    let mut nodes = Vec::new();
    let root = balanced(&keys, &mut nodes, &hash);
    assert_eq!(nodes.len(), 4096);
    assert_eq!(root.as_ref().unwrap().count, 4096);
    let decoded_nodes: Vec<Node> =
        serde_json::from_str(&serde_json::to_string(&nodes).unwrap()).unwrap();
    let decoded_root: Option<NodeRef> =
        serde_json::from_str(&serde_json::to_string(&root).unwrap()).unwrap();
    assert_eq!(decoded_nodes, nodes);
    assert_eq!(decoded_root, root);
    for (tree, root) in [(&nodes, &root), (&decoded_nodes, &decoded_root)] {
        // All present keys plus all interior gaps and both exterior boundaries.
        for key in keys
            .iter()
            .cloned()
            .chain((0..4096).map(|i| format!("k{:05}", i * 2 + 1)))
            .chain(["a".to_owned(), "z".to_owned()])
        {
            let (actual, visits) = resolve(&key, root.clone(), tree, &hash);
            assert_eq!(actual.as_ref(), oracle.get(&key), "key {key}");
            assert!((1..=MAX_PATH_NODES).contains(&visits));
        }
    }
}

#[test]
fn empty_bounds_decoder_and_borrowed_node_lifetime() {
    let hash = TestHash::default();
    let mut empty = Lookup::new("key", None).unwrap();
    assert_eq!(empty.step(), LookupStep::Absent);
    assert_eq!(empty.visited_nodes(), 0);
    assert_eq!(empty.unavailable(), Err(LedgerError::Terminal));
    assert_eq!(
        empty.provide(&leaf("key", 1), &hash),
        Err(LedgerError::Terminal)
    );
    assert_eq!(hash.0.get(), 0);
    for key in [
        "".to_owned(),
        "bad/key".into(),
        "é".into(),
        "x".repeat(MAX_ID_BYTES + 1),
    ] {
        assert_eq!(Lookup::new(&key, None).unwrap_err(), LedgerError::Identity);
    }
    let mut huge = leaf("key", 1);
    huge.key = "x".repeat(100_000);
    assert_eq!(huge.reference(&hash), Err(LedgerError::Identity));
    huge = leaf("key", 1);
    huge.address.entry_id = "x".repeat(100_000);
    assert_eq!(huge.reference(&hash), Err(LedgerError::Identity));
    assert_eq!(
        hash.0.get(),
        0,
        "bounds must reject before invoking trusted hash"
    );

    let mut maximum = leaf(&"k".repeat(MAX_ID_BYTES), 3);
    maximum.address.entry_id = "p".repeat(MAX_ID_BYTES);
    maximum.value.entry_id = maximum.address.entry_id.clone();
    for (side, id, ordinal) in [(&mut maximum.left, 'l', 1), (&mut maximum.right, 'r', 2)] {
        *side = Some(NodeRef {
            address: Address {
                entry_id: id.to_string().repeat(MAX_ID_BYTES),
                ordinal,
                slot: 0,
            },
            digest: [0; 32],
            height: 1,
            count: 1,
        });
    }
    assert!(maximum.canonical_bytes().unwrap().len() <= MAX_NODE_BYTES);
    maximum.reference(&hash).unwrap();
    Lookup::new(&"k".repeat(MAX_ID_BYTES), None).unwrap();

    let node = leaf("key", 1);
    let mut json = serde_json::to_value(&node).unwrap();
    json["unknown"] = true.into();
    assert!(serde_json::from_value::<Node>(json).is_err());
    let mut json = serde_json::to_value(&node).unwrap();
    json["address"]["unknown"] = true.into();
    assert!(serde_json::from_value::<Node>(json).is_err());
    let mut json = serde_json::to_value(&node).unwrap();
    json["value"]["unknown"] = true.into();
    assert!(serde_json::from_value::<Node>(json).is_err());
    let mut json = serde_json::to_value(node.reference(&hash).unwrap()).unwrap();
    json["unknown"] = true.into();
    assert!(serde_json::from_value::<NodeRef>(json).is_err());

    let child = leaf("a", 1);
    let mut parent = leaf("m", 3);
    parent.value.entry_id = "receipt-only-marker".into();
    parent.value.ordinal = 2;
    parent.left = Some(child.reference(&hash).unwrap());
    let mut lookup = Lookup::new("a", Some(parent.reference(&hash).unwrap())).unwrap();
    let supplied_clone = Rc::new(parent.clone());
    drop(parent);
    let weak = Rc::downgrade(&supplied_clone);
    lookup.provide(&supplied_clone, &hash).unwrap();
    assert_eq!(Rc::strong_count(&supplied_clone), 1);
    drop(supplied_clone);
    assert!(weak.upgrade().is_none());
    assert!(!format!("{lookup:?}").contains("receipt-only-marker"));
    // Structural probes only: Rc detects retention of this allocation, not arbitrary
    // Node::clone calls; size/debug are NOT a total heap/RSS measurement.
    assert!(std::mem::size_of_val(&lookup) <= 1024);
    lookup.provide(&child, &hash).unwrap();
    assert_eq!(lookup.step(), LookupStep::Found(&child.value));
}

#[test]
fn tampered_proofs_poison_and_terminal_states_do_not_consume() {
    let hash = TestHash::default();
    let node = leaf("key", 1);
    let reference = node.reference(&hash).unwrap();
    for mutation in 0..5 {
        let mut expected = reference.clone();
        let mut supplied = node.clone();
        match mutation {
            0 => expected.digest[0] ^= 1,
            1 => expected.address.entry_id = "other-location".into(),
            2 => {
                expected.height = 2;
                expected.count = 2;
            }
            3 => supplied.value.digest[0] ^= 1,
            _ => supplied.key = "another-key".into(),
        }
        let mut lookup = Lookup::new("key", Some(expected)).unwrap();
        poisoned(&mut lookup, &supplied, &hash, LedgerError::Proof);
        assert_eq!(lookup.visited_nodes(), 0);
    }
    // Count alone can differ while both summaries remain locally admissible.
    let mut parent = leaf("m", 4);
    parent.left = Some(node.reference(&hash).unwrap());
    parent.right = Some(leaf("z", 2).reference(&hash).unwrap());
    let mut expected = parent.reference(&hash).unwrap();
    expected.count = 2;
    poisoned(
        &mut Lookup::new("m", Some(expected)).unwrap(),
        &parent,
        &hash,
        LedgerError::Proof,
    );
    let mut invalid = node.clone();
    invalid.key.clear();
    poisoned(
        &mut Lookup::new("key", Some(reference.clone())).unwrap(),
        &invalid,
        &hash,
        LedgerError::Identity,
    );

    let mut missing = Lookup::new("key", Some(reference.clone())).unwrap();
    assert!(matches!(missing.step(), LookupStep::NeedNode(_)));
    assert_eq!(missing.unavailable(), Err(LedgerError::MissingNode));
    assert_eq!(missing.step(), LookupStep::Poisoned);
    assert_eq!(missing.provide(&node, &hash), Err(LedgerError::Terminal));
    assert_eq!(missing.visited_nodes(), 0);
    let mut found = Lookup::new("key", Some(reference)).unwrap();
    found.provide(&node, &hash).unwrap();
    let calls = hash.0.get();
    assert_eq!(found.provide(&invalid, &hash), Err(LedgerError::Terminal));
    assert_eq!(found.unavailable(), Err(LedgerError::Terminal));
    assert_eq!(found.step(), LookupStep::Found(&node.value));
    assert_eq!(found.visited_nodes(), 1);
    assert_eq!(hash.0.get(), calls);
}

#[test]
fn invalid_addresses_avl_summaries_receipts_and_overflow_reject_before_hash() {
    let hash = TestHash::default();
    let base = leaf("m", 4);
    let child = leaf("a", 1).reference(&hash).unwrap();
    let valid_root = base.reference(&hash).unwrap();
    let calls = hash.0.get();
    for case in 0..15 {
        let mut node = base.clone();
        node.left = Some(child.clone());
        let c = node.left.as_mut().unwrap();
        let error = match case {
            0 => {
                c.address = node.address.clone();
                LedgerError::ReferenceOrder
            }
            1 => {
                c.address.ordinal = 5;
                LedgerError::ReferenceOrder
            }
            2 => {
                c.address.ordinal = 4;
                LedgerError::ReferenceOrder
            }
            3 => {
                c.height = 0;
                LedgerError::Shape
            }
            4 => {
                c.count = 0;
                LedgerError::Shape
            }
            5 => {
                c.height = 3;
                c.count = 3;
                LedgerError::Shape
            }
            6 => {
                c.count = 2;
                LedgerError::Shape
            }
            7 => {
                c.height = 129;
                LedgerError::Limit
            }
            8 => {
                c.height = 92;
                c.count = u64::MAX;
                LedgerError::Overflow
            }
            9 => {
                c.height = 2;
                c.count = 2;
                LedgerError::Shape
            }
            10 => {
                node.right = node.left.clone();
                LedgerError::Shape
            }
            11 => {
                node.value.ordinal = 3;
                LedgerError::ReferenceOrder
            }
            12 => {
                node.value.entry_id = "other".into();
                node.value.ordinal = 4;
                LedgerError::ReferenceOrder
            }
            13 => {
                c.address.ordinal = 0;
                LedgerError::ReferenceOrder
            }
            _ => {
                c.height = 91;
                c.count = u64::MAX;
                let mut right = c.clone();
                right.address.entry_id = "another-child".into();
                node.right = Some(right);
                LedgerError::Overflow
            }
        };
        assert_eq!(node.reference(&hash), Err(error.clone()), "case {case}");
        poisoned(
            &mut Lookup::new("m", Some(valid_root.clone())).unwrap(),
            &node,
            &hash,
            error,
        );
        assert_eq!(hash.0.get(), calls, "invalid shape hashed in case {case}");
    }
    for (height, count, error) in [
        (0, 1, LedgerError::Shape),
        (3, 3, LedgerError::Shape),
        (129, 1, LedgerError::Limit),
    ] {
        let mut root = child.clone();
        root.height = height;
        root.count = count;
        assert_eq!(Lookup::new("m", Some(root)).unwrap_err(), error);
    }
    // Same-entry children require same ordinal AND smaller slot; earlier ordinal
    // alone is not sufficient. Foreign receipts, conversely, must be older.
    let mut same = base.clone();
    same.address.slot = 2;
    let mut reference = child;
    reference.address = same.address.clone();
    reference.address.slot = 1;
    same.left = Some(reference);
    same.reference(&hash).unwrap();
    same.left.as_mut().unwrap().address.ordinal -= 1;
    assert_eq!(same.reference(&hash), Err(LedgerError::ReferenceOrder));

    // Exercise actual cursor traversal across two slots in one entry, not just
    // reference construction. The test hash remains deliberately nonsecure.
    let mut shared_leaf = leaf("a", 1);
    shared_leaf.address.entry_id = "shared".into();
    shared_leaf.value.entry_id = "shared".into();
    let leaf_ref = shared_leaf.reference(&hash).unwrap();
    let mut shared_parent = shared_leaf.clone();
    shared_parent.address.slot = 1;
    shared_parent.key = "m".into();
    shared_parent.left = Some(leaf_ref.clone());
    let parent_ref = shared_parent.reference(&hash).unwrap();
    for key in ["a", "b"] {
        let mut lookup = Lookup::new(key, Some(parent_ref.clone())).unwrap();
        assert_eq!(lookup.step(), LookupStep::NeedNode(&parent_ref));
        lookup.provide(&shared_parent, &hash).unwrap();
        assert_eq!(lookup.step(), LookupStep::NeedNode(&leaf_ref));
        lookup.provide(&shared_leaf, &hash).unwrap();
        assert_eq!(
            lookup.step(),
            if key == "a" {
                LookupStep::Found(&shared_leaf.value)
            } else {
                LookupStep::Absent
            }
        );
        assert_eq!(lookup.visited_nodes(), 2);
    }
    for slot in [1, 2] {
        let mut invalid = shared_parent.clone();
        invalid.left.as_mut().unwrap().address.slot = slot;
        let calls = hash.0.get();
        assert_eq!(invalid.reference(&hash), Err(LedgerError::ReferenceOrder));
        poisoned(
            &mut Lookup::new("a", Some(parent_ref.clone())).unwrap(),
            &invalid,
            &hash,
            LedgerError::ReferenceOrder,
        );
        assert_eq!(hash.0.get(), calls, "equal/higher slot must not hash");
    }
    let mut older_receipt = base;
    older_receipt.value.entry_id = "older-receipt".into();
    older_receipt.value.ordinal = 3;
    older_receipt.reference(&hash).unwrap();
}

#[test]
fn ancestor_bounds_in_both_directions_and_cross_node_cycles_poison() {
    let hash = TestHash::default();
    for mirrored in [false, true] {
        // Third node satisfies its immediate parent's bound, but violates the
        // ancestor's other bound. Every body/reference is locally hash-valid.
        let offender = leaf(if mirrored { "a" } else { "z" }, 1);
        let mut middle = leaf(if mirrored { "z" } else { "a" }, 2);
        let support = leaf(if mirrored { "b" } else { "y" }, 3);
        if mirrored {
            middle.left = Some(offender.reference(&hash).unwrap());
        } else {
            middle.right = Some(offender.reference(&hash).unwrap());
        }
        let mut root = leaf("m", 4);
        if mirrored {
            root.left = Some(support.reference(&hash).unwrap());
            root.right = Some(middle.reference(&hash).unwrap());
        } else {
            root.left = Some(middle.reference(&hash).unwrap());
            root.right = Some(support.reference(&hash).unwrap());
        }
        let mut lookup = Lookup::new(
            if mirrored { "n" } else { "b" },
            Some(root.reference(&hash).unwrap()),
        )
        .unwrap();
        lookup.provide(&root, &hash).unwrap();
        lookup.provide(&middle, &hash).unwrap();
        poisoned(&mut lookup, &offender, &hash, LedgerError::KeyOrder);
        assert_eq!(lookup.visited_nodes(), 2);
    }
    // An address cycle cannot have every edge strictly precede its parent.
    // Only the older endpoint's summary is trusted here; no cyclic hash fixed
    // point is assumed or claimed to be an authenticated valid proof.
    let mut root = leaf("m", 3);
    let mut back_edge = leaf("a", 2);
    root.left = Some(back_edge.reference(&hash).unwrap());
    let root_ref = root.reference(&hash).unwrap();
    back_edge.right = Some(root_ref.clone());
    let mut lookup = Lookup::new("a", Some(root_ref)).unwrap();
    lookup.provide(&root, &hash).unwrap();
    poisoned(&mut lookup, &back_edge, &hash, LedgerError::ReferenceOrder);
    assert_eq!(lookup.visited_nodes(), 1);
}

#[test]
fn independent_canonical_v1_golden_and_json_order_independence() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Golden {
        scope: String,
        node: Node,
        canonical_hex: String,
        canonical_bytes: usize,
    }
    for (fixture, expected_bytes) in [
        (
            include_str!("fixtures/ledger-lookup/node-canonical-v1.json"),
            120,
        ),
        (
            include_str!("fixtures/ledger-lookup/parent-canonical-v1.json"),
            240,
        ),
    ] {
        let golden: Golden = serde_json::from_str(fixture).unwrap();
        assert!(golden.scope.contains("canonical encoding only"));
        assert_eq!(golden.canonical_bytes, expected_bytes);
        if expected_bytes == 240 {
            let child = |ordinal, slot, digest| NodeRef {
                address: Address {
                    entry_id: format!("entry-{ordinal}"),
                    ordinal,
                    slot,
                },
                digest: [digest; 32],
                height: 1,
                count: 1,
            };
            assert_eq!(
                golden.node,
                Node {
                    address: Address {
                        entry_id: "entry-3".into(),
                        ordinal: 3,
                        slot: 2,
                    },
                    key: "request-3".into(),
                    value: ReceiptRef {
                        entry_id: "entry-2".into(),
                        ordinal: 2,
                        digest: [0x33; 32],
                    },
                    left: Some(child(1, 7, 0x11)),
                    right: Some(child(2, 9, 0x22)),
                }
            );
        }
        let bytes = golden.node.canonical_bytes().unwrap();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(bytes.len(), golden.canonical_bytes);
        // Both fixtures' expected bytes are independently encoded, never
        // generated by production code; this compares the complete encoding.
        assert_eq!(hex, golden.canonical_hex);
        let decoded: Node =
            serde_json::from_str(&serde_json::to_string(&golden.node).unwrap()).unwrap();
        assert_eq!(decoded, golden.node);
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        let reordered = format!(
            "{{\"right\":{},\"value\":{},\"key\":{},\"left\":{},\"address\":{{\"slot\":{},\"ordinal\":{},\"entryId\":{}}}}}",
            serde_json::to_string(&golden.node.right).unwrap(),
            serde_json::to_string(&golden.node.value).unwrap(),
            serde_json::to_string(&golden.node.key).unwrap(),
            serde_json::to_string(&golden.node.left).unwrap(),
            golden.node.address.slot,
            golden.node.address.ordinal,
            serde_json::to_string(&golden.node.address.entry_id).unwrap(),
        );
        let reordered: Node = serde_json::from_str(&reordered).unwrap();
        assert_eq!(reordered, golden.node);
        assert_eq!(reordered.canonical_bytes().unwrap(), bytes);
        let mut changed = golden.node;
        changed.value.digest[0] ^= 1;
        assert_ne!(changed.canonical_bytes().unwrap(), bytes);
    }
    let hash = TestHash::default();
    let mut parent = leaf("m", 3);
    parent.left = Some(leaf("a", 1).reference(&hash).unwrap());
    parent.right = Some(leaf("z", 2).reference(&hash).unwrap());
    let before = parent.canonical_bytes().unwrap();
    for left in [true, false] {
        let mut changed = parent.clone();
        let child = if left {
            &mut changed.left
        } else {
            &mut changed.right
        };
        child.as_mut().unwrap().digest[0] ^= 1;
        assert_ne!(changed.canonical_bytes().unwrap(), before);
    }
}
