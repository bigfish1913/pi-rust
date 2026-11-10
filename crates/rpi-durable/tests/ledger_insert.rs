//! Synthetic in-memory insertion tests, not a journal or authenticated history.
//! TestHash is deliberately noncryptographic; it provides no security guarantee.
use rpi_durable::ledger::{
    insert::{Batch, Insert, InsertError, InsertStep, MAX_CREATED_NODES},
    Address, LedgerError, Lookup, LookupStep, Node, NodeHasher, NodeRef, ReceiptRef, MAX_ID_BYTES,
    MAX_PATH_NODES,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};

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

type Location = (String, u64, u16);
type Store = BTreeMap<Location, Node>;
type Oracle = BTreeMap<String, ReceiptRef>;
fn location(a: &Address) -> Location {
    (a.entry_id.clone(), a.ordinal, a.slot)
}
fn key(n: usize) -> String {
    format!("k{n:05}")
}
fn receipt(n: u64) -> ReceiptRef {
    ReceiptRef {
        entry_id: format!("entry-{n}"),
        ordinal: n,
        digest: [n as u8; 32],
    }
}
fn leaf(n: usize, ordinal: u64) -> Node {
    let value = receipt(ordinal);
    Node {
        address: Address {
            entry_id: value.entry_id.clone(),
            ordinal,
            slot: 0,
        },
        key: key(n),
        value,
        left: None,
        right: None,
    }
}
fn body<'a>(store: &'a Store, reference: &NodeRef) -> &'a Node {
    let node = store
        .get(&location(&reference.address))
        .expect("exact requested address");
    assert_eq!(node.address, reference.address);
    node
}

// Independent traversal: compute heights/counts from bodies, not summary fields,
// enforce inherited global bounds, and compare every value with a separate map.
fn audit(root: &NodeRef, store: &Store, expected: &Oracle, hash: &TestHash) -> Store {
    fn walk(
        r: &NodeRef,
        store: &Store,
        lo: Option<&str>,
        hi: Option<&str>,
        seen: &mut BTreeSet<Location>,
        found: &mut Oracle,
        saved: &mut Store,
        hash: &TestHash,
    ) -> (u64, u8) {
        assert!(
            seen.insert(location(&r.address)),
            "duplicate reachable address"
        );
        let n = body(store, r);
        assert!(lo.is_none_or(|v| v < n.key.as_str()));
        assert!(hi.is_none_or(|v| n.key.as_str() < v));
        assert!(found.insert(n.key.clone(), n.value.clone()).is_none());
        saved.insert(location(&n.address), n.clone());
        let (lc, lh) = n.left.as_ref().map_or((0, 0), |c| {
            walk(c, store, lo, Some(&n.key), seen, found, saved, hash)
        });
        let (rc, rh) = n.right.as_ref().map_or((0, 0), |c| {
            walk(c, store, Some(&n.key), hi, seen, found, saved, hash)
        });
        assert!(lh.abs_diff(rh) <= 1);
        let count = 1 + lc + rc;
        let height = 1 + lh.max(rh);
        assert_eq!((r.count, r.height), (count, height));
        assert_eq!(r, &n.reference(hash).unwrap()); // includes digest and address
        (count, height)
    }
    let (mut seen, mut found, mut saved) = (BTreeSet::new(), Oracle::new(), Store::new());
    let (count, _) = walk(
        root, store, None, None, &mut seen, &mut found, &mut saved, hash,
    );
    assert_eq!(count as usize, expected.len());
    assert_eq!(&found, expected);
    saved
}

// No publication while the cursor is searching. Even temporary/unreachable
// rotation nodes must have valid references and monotonically assigned slots.
fn build(
    k: &str,
    value: ReceiptRef,
    root: Option<NodeRef>,
    store: &Store,
    hash: &TestHash,
) -> Batch {
    assert_eq!(MAX_CREATED_NODES, 512);
    assert_eq!(MAX_PATH_NODES, 128);
    let count = root.as_ref().map_or(0, |r| r.count);
    let mut insert = Insert::new(k, value.clone(), root, hash).unwrap();
    let mut supplied = 0;
    loop {
        assert!(insert.retained_path_nodes() <= MAX_PATH_NODES);
        match insert.step() {
            InsertStep::NeedNode(r) => {
                let n = body(store, r).clone();
                assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
                insert.provide(&n, hash).unwrap();
                supplied += 1;
            }
            InsertStep::Ready(_) => break,
            state => panic!("unexpected {state:?}"),
        }
    }
    let batch = insert.take_batch().unwrap();
    assert_eq!(insert.step(), InsertStep::Taken);
    assert_eq!(batch.root.count, count + 1);
    assert!(!batch.nodes.is_empty());
    assert!(batch.nodes.len() <= MAX_CREATED_NODES);
    assert!(batch.nodes.len() <= 1 + 3 * supplied);
    assert_eq!(insert.retained_path_nodes(), 0);
    let mut staged = Store::new();
    for (slot, n) in batch.nodes.iter().enumerate() {
        assert_eq!(n.address.slot as usize, slot);
        assert_eq!(n.address.entry_id, value.entry_id);
        assert_eq!(n.address.ordinal, value.ordinal);
        let reference = n.reference(hash).unwrap();
        for child in [&n.left, &n.right].into_iter().flatten() {
            let child_body = staged
                .get(&location(&child.address))
                .or_else(|| store.get(&location(&child.address)))
                .expect("child precedes parent");
            assert_eq!(child, &child_body.reference(hash).unwrap());
        }
        assert!(staged.insert(location(&n.address), n.clone()).is_none());
        if n.address == batch.root.address {
            assert_eq!(reference, batch.root);
        }
    }
    assert_eq!(
        body(&staged, &batch.root).reference(hash).unwrap(),
        batch.root
    );
    batch
}
fn publish(store: &mut Store, batch: Batch) -> NodeRef {
    for n in batch.nodes {
        assert!(store.insert(location(&n.address), n).is_none());
    }
    batch.root
}
fn lookup(k: &str, root: NodeRef, store: &Store, hash: &TestHash) -> Option<ReceiptRef> {
    let mut cursor = Lookup::new(k, Some(root)).unwrap();
    loop {
        match cursor.step() {
            LookupStep::NeedNode(r) => cursor.provide(body(store, r), hash).unwrap(),
            LookupStep::Found(v) => return Some(v.clone()),
            LookupStep::Absent => return None,
            LookupStep::Poisoned => panic!("valid tree lookup poisoned"),
        }
        assert!(cursor.visited_nodes() <= MAX_PATH_NODES);
    }
}

#[test]
fn minimal_ll_lr_rl_rr_rotations() {
    for order in [[30, 20, 10], [30, 10, 20], [10, 30, 20], [10, 20, 30]] {
        let (hash, mut store, mut expected) = (TestHash::default(), Store::new(), Oracle::new());
        let mut root = None;
        for (i, n) in order.into_iter().enumerate() {
            let value = receipt(i as u64 + 1);
            let batch = build(&key(n), value.clone(), root, &store, &hash);
            root = Some(publish(&mut store, batch));
            expected.insert(key(n), value);
        }
        let root = root.unwrap();
        audit(&root, &store, &expected, &hash);
        let n = body(&store, &root);
        assert_eq!(n.key, key(20));
        for (r, k) in [
            (n.left.as_ref().unwrap(), 10),
            (n.right.as_ref().unwrap(), 30),
        ] {
            let child = body(&store, r);
            assert_eq!(child.key, key(k));
            assert!(child.left.is_none() && child.right.is_none());
            assert_eq!((r.count, r.height), (1, 1));
        }
    }
}

fn exercise(order: Vec<usize>) {
    let hash = TestHash::default();
    let (mut store, mut expected, mut root) = (Store::new(), Oracle::new(), None);
    let mut snapshots = Vec::new();
    for (i, index) in order.into_iter().enumerate() {
        let k = key(2 * index + 2); // Every odd key is an absent gap.
        let v = receipt(i as u64 + 1);
        let batch = build(&k, v.clone(), root, &store, &hash);
        root = Some(publish(&mut store, batch));
        assert!(expected.insert(k, v).is_none());
        if (i + 1) % 256 == 0 {
            let r = root.as_ref().unwrap();
            let saved = audit(r, &store, &expected, &hash);
            snapshots.push((r.clone(), expected.clone(), saved));
        }
    }
    let root = root.unwrap();
    assert_eq!(expected.len(), 4096); // Actual insertions, well beyond 64.
    audit(&root, &store, &expected, &hash);
    for (old_root, old_expected, old_bodies) in &snapshots {
        assert_eq!(&audit(old_root, &store, old_expected, &hash), old_bodies);
        for (address, old) in old_bodies {
            assert_eq!(store.get(address), Some(old));
        }
    }
    // Serialize all published bodies (including unreachable intermediate nodes),
    // then reconstruct a fresh address resolver; JSON object keys aren't addresses.
    let encoded = serde_json::to_string(&store.values().collect::<Vec<_>>()).unwrap();
    let decoded: Vec<Node> = serde_json::from_str(&encoded).unwrap();
    let mut restarted = Store::new();
    for n in decoded {
        assert!(restarted.insert(location(&n.address), n).is_none());
    }
    assert_eq!(restarted, store);
    let root: NodeRef = serde_json::from_str(&serde_json::to_string(&root).unwrap()).unwrap();
    audit(&root, &restarted, &expected, &hash);
    for i in 0..4096 {
        let k = key(2 * i + 2);
        assert_eq!(
            lookup(&k, root.clone(), &restarted, &hash).as_ref(),
            expected.get(&k)
        );
    }
    for i in 0..=4096 {
        assert_eq!(
            lookup(&key(2 * i + 1), root.clone(), &restarted, &hash),
            None
        );
    }
    // A fresh insertion after reload also works and leaves the old root intact.
    let v = receipt(4097);
    let batch = build(&key(0), v.clone(), Some(root.clone()), &restarted, &hash);
    let new_root = publish(&mut restarted, batch);
    audit(&root, &restarted, &expected, &hash);
    expected.insert(key(0), v);
    audit(&new_root, &restarted, &expected, &hash);
}
#[test]
fn ascending_4096() {
    exercise((0..4096).collect());
}
#[test]
fn descending_4096() {
    exercise((0..4096).rev().collect());
}
#[test]
fn deterministic_fisher_yates_4096() {
    let mut order: Vec<_> = (0..4096).collect();
    let mut state = 0x4d595df4d0f33173u64;
    for i in (1..order.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    exercise(order);
}

fn terminal_poison(insert: &mut Insert, node: &Node, hash: &TestHash) {
    assert_eq!(insert.step(), InsertStep::Poisoned);
    let calls = hash.0.get();
    let retained = insert.retained_path_nodes();
    assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
    assert_eq!(insert.provide(node, hash), Err(InsertError::Terminal));
    assert_eq!(insert.unavailable(), Err(InsertError::Terminal));
    assert_eq!(insert.step(), InsertStep::Poisoned);
    assert_eq!(insert.retained_path_nodes(), retained);
    assert_eq!(hash.0.get(), calls);
}
fn reject(insert: &mut Insert, node: &Node, hash: &TestHash, error: LedgerError) {
    assert_eq!(insert.provide(node, hash), Err(InsertError::Ledger(error)));
    terminal_poison(insert, node, hash);
}

#[test]
fn duplicate_keys_poison_and_exact_receipt_reuse_is_rejected_at_construction() {
    let hash = TestHash::default();
    let mut store = Store::new();
    let mut root = None;
    let mut expected = Oracle::new();
    for (n, ordinal) in [(20, 1), (10, 2), (30, 3)] {
        let v = receipt(ordinal);
        let batch = build(&key(n), v.clone(), root, &store, &hash);
        root = Some(publish(&mut store, batch));
        expected.insert(key(n), v);
    }
    let before = store.clone();
    for n in [20, 10, 30] {
        let k = key(n);
        // Exact ReceiptRef reuse cannot reach Duplicate: every valid stored
        // value.ordinal <= root.ordinal, but new requires value.ordinal > root.
        // Assert the actual public contract rather than forging invalid history.
        invalid(
            &k,
            expected[&k].clone(),
            root.clone(),
            LedgerError::ReferenceOrder,
            &hash,
        );
        for digest in [expected[&k].digest, [99; 32]] {
            let mut value = receipt(10);
            value.digest = digest; // Both matching and different receipt digests.
            let mut insert = Insert::new(&k, value, root.clone(), &hash).unwrap();
            loop {
                let node = match insert.step() {
                    InsertStep::NeedNode(r) => body(&store, r).clone(),
                    state => panic!("duplicate unexpectedly reached {state:?}"),
                };
                assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
                let result = insert.provide(&node, &hash);
                if node.key == k {
                    assert_eq!(result, Err(InsertError::Duplicate));
                    terminal_poison(&mut insert, &node, &hash);
                    break;
                }
                assert_eq!(result, Ok(()));
                assert!(insert.retained_path_nodes() <= MAX_PATH_NODES);
            }
        }
    }
    assert_eq!(store, before);
    audit(root.as_ref().unwrap(), &store, &expected, &hash);
}

#[test]
fn proof_failures_and_second_node_bounds_are_terminal() {
    let hash = TestHash::default();
    let n = leaf(20, 1);
    let r = n.reference(&hash).unwrap();
    let mut missing = Insert::new(&key(10), receipt(3), Some(r.clone()), &hash).unwrap();
    assert_eq!(
        missing.unavailable(),
        Err(InsertError::Ledger(LedgerError::MissingNode))
    );
    terminal_poison(&mut missing, &n, &hash);
    let mut wrong = n.clone();
    wrong.value.digest[0] ^= 1;
    let mut insert = Insert::new(&key(10), receipt(3), Some(r.clone()), &hash).unwrap();
    reject(&mut insert, &wrong, &hash, LedgerError::Proof);
    let mut malformed = n.clone();
    let mut child = r.clone();
    child.height = 0;
    malformed.left = Some(child);
    let mut insert = Insert::new(&key(10), receipt(3), Some(r.clone()), &hash).unwrap();
    reject(&mut insert, &malformed, &hash, LedgerError::Shape);
    let mut unordered = n.clone();
    unordered.left = Some(r.clone()); // Child does not precede its own parent.
    let mut insert = Insert::new(&key(10), receipt(3), Some(r), &hash).unwrap();
    reject(&mut insert, &unordered, &hash, LedgerError::ReferenceOrder);

    let bad_child = leaf(40, 1); // Locally valid, globally wrong on the left of 30.
    let mut parent = leaf(30, 2);
    parent.left = Some(bad_child.reference(&hash).unwrap());
    let r = parent.reference(&hash).unwrap();
    let mut insert = Insert::new(&key(20), receipt(3), Some(r), &hash).unwrap();
    insert.provide(&parent, &hash).unwrap();
    assert_eq!(insert.retained_path_nodes(), 1);
    assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
    reject(&mut insert, &bad_child, &hash, LedgerError::KeyOrder);
    let bad_child = leaf(20, 1);
    let mut parent = leaf(30, 2);
    parent.right = Some(bad_child.reference(&hash).unwrap());
    let mut insert = Insert::new(
        &key(40),
        receipt(3),
        Some(parent.reference(&hash).unwrap()),
        &hash,
    )
    .unwrap();
    insert.provide(&parent, &hash).unwrap();
    assert_eq!(insert.retained_path_nodes(), 1);
    reject(&mut insert, &bad_child, &hash, LedgerError::KeyOrder);
}

#[test]
fn adversarial_second_node_never_exposes_a_partial_batch() {
    let hash = TestHash::default();
    let child = leaf(10, 1);
    let mut parent = leaf(30, 2);
    parent.left = Some(child.reference(&hash).unwrap());
    let root = parent.reference(&hash).unwrap();
    let store: Store = [child.clone(), parent.clone()]
        .into_iter()
        .map(|n| (location(&n.address), n))
        .collect();
    let before = store.clone();
    for error in [
        LedgerError::Proof,
        LedgerError::Shape,
        LedgerError::ReferenceOrder,
        LedgerError::MissingNode,
    ] {
        let mut insert = Insert::new(&key(5), receipt(3), Some(root.clone()), &hash).unwrap();
        insert.provide(body(&store, &root), &hash).unwrap();
        assert_eq!(insert.retained_path_nodes(), 1);
        assert_eq!(
            insert.step(),
            InsertStep::NeedNode(parent.left.as_ref().unwrap())
        );
        assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
        let mut bad = child.clone();
        match error {
            LedgerError::Proof => bad.value.digest[0] ^= 1,
            LedgerError::Shape => {
                let mut r = child.reference(&hash).unwrap();
                r.height = 0;
                bad.left = Some(r);
            }
            LedgerError::ReferenceOrder => bad.value.ordinal = 0,
            LedgerError::MissingNode => {
                assert_eq!(insert.unavailable(), Err(InsertError::Ledger(error)));
                terminal_poison(&mut insert, &bad, &hash);
                assert_eq!(store, before);
                continue;
            }
            _ => unreachable!(),
        }
        reject(&mut insert, &bad, &hash, error);
        assert_eq!(store, before);
    }
}

#[test]
fn ready_wrong_calls_preserve_batch_and_taken_is_terminal() {
    let hash = TestHash::default();
    let n = leaf(10, 1);
    let mut insert = Insert::new(&key(20), receipt(2), None, &hash).unwrap();
    let batch = match insert.step() {
        InsertStep::Ready(b) => b.clone(),
        s => panic!("{s:?}"),
    };
    let calls = hash.0.get();
    assert_eq!(insert.provide(&n, &hash), Err(InsertError::Terminal));
    assert_eq!(insert.unavailable(), Err(InsertError::Terminal));
    assert_eq!(insert.step(), InsertStep::Ready(&batch));
    assert_eq!(insert.take_batch(), Ok(batch));
    assert_eq!(insert.take_batch(), Err(InsertError::Terminal));
    assert_eq!(insert.provide(&n, &hash), Err(InsertError::Terminal));
    assert_eq!(insert.unavailable(), Err(InsertError::Terminal));
    assert_eq!(insert.step(), InsertStep::Taken);
    assert_eq!(hash.0.get(), calls);
}

fn invalid(k: &str, v: ReceiptRef, r: Option<NodeRef>, error: LedgerError, hash: &TestHash) {
    let calls = hash.0.get();
    assert_eq!(
        Insert::new(k, v, r, hash).unwrap_err(),
        InsertError::Ledger(error)
    );
    assert_eq!(hash.0.get(), calls, "invalid constructor must not hash");
}
#[test]
fn constructors_reject_invalid_inputs_before_hashing() {
    let hash = TestHash::default();
    let root = leaf(10, 1).reference(&hash).unwrap();
    for bad in [
        "".to_string(),
        "bad/key".into(),
        "é".into(),
        "x".repeat(MAX_ID_BYTES + 1),
    ] {
        invalid(&bad, receipt(2), None, LedgerError::Identity, &hash);
        let mut v = receipt(2);
        v.entry_id = bad.clone();
        invalid(&key(20), v, None, LedgerError::Identity, &hash);
        let mut r = root.clone();
        r.address.entry_id = bad;
        invalid(&key(20), receipt(2), Some(r), LedgerError::Identity, &hash);
    }
    invalid(
        &key(20),
        receipt(0),
        None,
        LedgerError::ReferenceOrder,
        &hash,
    );
    for ordinal in [2, 3] {
        let mut r = root.clone();
        r.address.ordinal = ordinal;
        invalid(
            &key(20),
            receipt(2),
            Some(r),
            LedgerError::ReferenceOrder,
            &hash,
        );
    }
    let mut r = root.clone();
    r.address.entry_id = receipt(2).entry_id;
    invalid(
        &key(20),
        receipt(2),
        Some(r),
        LedgerError::ReferenceOrder,
        &hash,
    );
    let mut r = root.clone();
    r.address.ordinal = 0;
    invalid(
        &key(20),
        receipt(2),
        Some(r),
        LedgerError::ReferenceOrder,
        &hash,
    );
    for (height, count, error) in [
        (0, 1, LedgerError::Shape),
        (1, 0, LedgerError::Shape),
        (1, 2, LedgerError::Shape),
        (129, 1, LedgerError::Limit),
        (93, u64::MAX, LedgerError::Overflow),
        (64, u64::MAX, LedgerError::Overflow),
    ] {
        let mut r = root.clone();
        r.height = height;
        r.count = count;
        invalid(&key(20), receipt(2), Some(r), error, &hash);
    }
    // Plausible huge summary is not a proof of its bodies. No allocation or
    // hashing is needed until one is supplied. u64 AVL paths cannot reach 128.
    let mut huge = root;
    huge.height = 64;
    huge.count = u64::MAX - 1;
    let calls = hash.0.get();
    let mut insert = Insert::new(&key(20), receipt(2), Some(huge.clone()), &hash).unwrap();
    assert_eq!(insert.step(), InsertStep::NeedNode(&huge));
    assert_eq!(insert.retained_path_nodes(), 0);
    assert_eq!(hash.0.get(), calls);
    assert_eq!(
        insert.unavailable(),
        Err(InsertError::Ledger(LedgerError::MissingNode))
    );
}

#[test]
fn rotations_preserve_populated_intervening_subtrees_and_old_roots() {
    for (order, middle) in [
        (vec![50, 20, 70, 10, 30, 5], 20),
        (vec![50, 80, 30, 90, 70, 95], 80),
        (vec![50, 20, 70, 10, 30, 60, 80, 5, 15, 25, 35, 40], 30),
        (vec![50, 80, 30, 90, 70, 40, 20, 95, 85, 75, 65, 60], 70),
    ] {
        let (hash, mut store, mut expected) = (TestHash::default(), Store::new(), Oracle::new());
        let mut root = None;
        for (i, n) in order.into_iter().enumerate() {
            let before_root = root.clone();
            let before_expected = expected.clone();
            let before_nodes = store.clone();
            let value = receipt(i as u64 + 1);
            let batch = build(&key(n), value.clone(), root, &store, &hash);
            root = Some(publish(&mut store, batch));
            expected.insert(key(n), value);
            audit(root.as_ref().unwrap(), &store, &expected, &hash);
            if let Some(old) = before_root {
                audit(&old, &store, &before_expected, &hash);
            }
            for (location, node) in before_nodes {
                assert_eq!(store.get(&location), Some(&node));
            }
        }
        assert_eq!(body(&store, root.as_ref().unwrap()).key, key(middle));
    }
}

#[test]
fn reserved_identity_collision_during_rebuild_poison_has_no_partial_batch() {
    let hash = TestHash::default();
    // Caller violates globally unique reservation: old unvisited child uses
    // the new transaction's entry ID but a different ordinal. Constructor and
    // root proof are valid locally; copying the parent must fail, not publish
    // the already staged new leaf. This is not a global uniqueness validator.
    let mut child = leaf(10, 1);
    child.address.entry_id = receipt(3).entry_id;
    child.value.entry_id = child.address.entry_id.clone();
    let mut parent = leaf(20, 2);
    parent.left = Some(child.reference(&hash).unwrap());
    let root = parent.reference(&hash).unwrap();
    let mut insert = Insert::new(&key(30), receipt(3), Some(root), &hash).unwrap();
    let calls = hash.0.get();
    assert_eq!(
        insert.provide(&parent, &hash),
        Err(InsertError::Ledger(LedgerError::ReferenceOrder))
    );
    assert_eq!(
        hash.0.get(),
        calls + 2,
        "validated root then staged leaf only"
    );
    terminal_poison(&mut insert, &parent, &hash);
}
