//! Pure synthetic codec tests: no Session, adapter, persistence, or history authentication.
use rpi_durable::{
    control::{ControlAction, ControlState},
    execution::ExecutionControl,
    ledger::{
        insert::{Insert, InsertStep},
        Node, NodeRef,
    },
};
use rpi_harness::durable::{
    experimental_ledger::{
        self as codec, CodecError, Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher,
        CUSTOM_TYPE, FORMAT, MAX_RECORD_BYTES,
    },
    DurableReceipt, DurableScope,
};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;

fn policy() -> ReadPolicy {
    ReadPolicy::new_epoch("synthetic-ledger", "main", "epoch-1").unwrap()
}

fn receipt(key: &str, ordinal: u64) -> DurableReceipt {
    DurableReceipt {
        request_id: key.into(),
        actor: "owner-1".into(),
        expected: DurableScope {
            session_id: "synthetic-ledger".into(),
            lane: "main".into(),
            binding_token: "bind-1".into(),
            leaf_id: None,
            history_revision: 0,
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

// The resolver is just an in-memory map of pure Insert outputs, not a Session store.
fn sequence(keys: &[&str]) -> Vec<Record> {
    let mut store: HashMap<(String, u16), Node> = HashMap::new();
    let mut root = None;
    let mut records = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let ordinal = index as u64 + 1;
        let mut receipt = receipt(key, ordinal);
        if ordinal > 1 {
            receipt.actor = "child-different".into();
            receipt.expected.history_revision = 7; // Not ledger ordinal or control revision.
        }
        let reference = codec::receipt_reference(&receipt, ordinal).unwrap();
        let mut insert = Insert::new(key, reference, root.clone(), &Sha256NodeHasher).unwrap();
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    let node = store
                        .get(&(reference.address.entry_id.clone(), reference.address.slot))
                        .expect("synthetic predecessor node")
                        .clone();
                    insert.provide(&node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("unexpected insertion state: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        for node in &batch.nodes {
            store.insert(
                (node.address.entry_id.clone(), node.address.slot),
                node.clone(),
            );
        }
        root = Some(batch.root.clone());
        records.push(Record {
            format: FORMAT.into(),
            version: 2,
            epoch: Epoch {
                session_id: "synthetic-ledger".into(),
                lane: "main".into(),
                epoch_id: "epoch-1".into(),
            },
            ordinal,
            previous: (ordinal > 1).then(|| Previous {
                entry_id: format!("entry-{}", ordinal - 1),
                ordinal: ordinal - 1,
            }),
            state: ControlState::default(),
            execution_control: ExecutionControl::default(),
            receipt,
            nodes: batch.nodes,
            root: batch.root,
        });
    }
    records
}

fn first() -> Record {
    sequence(&["req-1"]).pop().unwrap()
}
fn raw(record: &Record) -> Vec<u8> {
    serde_json::to_vec(record).unwrap()
}
fn accept(record: &Record) {
    let candidate = codec::decode(
        &raw(record),
        &record.receipt.entry_id,
        CUSTOM_TYPE,
        &policy(),
    )
    .expect("locally valid synthetic candidate");
    assert_eq!(candidate.record(), record);
}
fn reject(value: &Value, entry: &str, error: CodecError) {
    let result = codec::decode(
        &serde_json::to_vec(value).unwrap(),
        entry,
        CUSTOM_TYPE,
        &policy(),
    );
    assert_eq!(result.unwrap_err(), error, "mutation: {value}");
}
fn mutate(base: &Record, pointer: &str, replacement: Value, error: CodecError) {
    let mut value = serde_json::to_value(base).unwrap();
    *value.pointer_mut(pointer).expect(pointer) = replacement;
    reject(&value, &base.receipt.entry_id, error);
}

// Recommit a deliberate locally shaped mutation; do not confuse a hash mismatch
// with the decoder's later current-request path checks.
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

#[test]
fn first_reset_noop_is_a_typed_local_candidate() {
    let record = first();
    assert_eq!(record.ordinal, 1);
    assert_eq!(record.state.revision, 0);
    assert_eq!(record.execution_control, ExecutionControl::default());
    assert_eq!(record.previous, None);
    assert_eq!(record.receipt.expected.entry_id, None);
    assert_eq!(record.receipt.expected.leaf_id, None);
    assert_eq!(record.receipt.expected.history_revision, 0);
    assert_eq!(record.receipt.expected.control_revision, 0);
    assert!(!record.receipt.changed);
    assert_eq!(record.nodes.len(), 1);
    assert_eq!(
        record.nodes[0].value,
        codec::receipt_reference(&record.receipt, 1).unwrap()
    );
    assert_eq!(record.root.address.entry_id, "entry-1");
    accept(&record);
}

#[test]
fn sequential_insert_batches_cover_all_four_avl_rotations() {
    for keys in [
        ["c", "b", "a"],
        ["a", "b", "c"],
        ["c", "a", "b"],
        ["a", "c", "b"],
    ] {
        let records = sequence(&keys);
        for record in &records {
            accept(record); // Decoder must find the current request using this batch alone.
            for node in &record.nodes {
                let independent: [u8; 32] = Sha256::digest(node.canonical_bytes().unwrap()).into();
                assert_eq!(
                    node.reference(&Sha256NodeHasher).unwrap().digest,
                    independent
                );
            }
        }
        let second = &records[1];
        assert_eq!(
            (
                second.ordinal,
                second.state.revision,
                second.receipt.expected.history_revision
            ),
            (2, 0, 7)
        );
        assert_eq!(second.previous.as_ref().unwrap().entry_id, "entry-1");
        assert_ne!(second.receipt.actor, records[0].receipt.actor);
        let third = &records[2];
        assert_eq!(third.nodes[usize::from(third.root.address.slot)].key, "b");
        assert_eq!((third.root.height, third.root.count), (2, 3));
    }
}

#[test]
fn raw_json_is_strict_and_the_one_mib_boundary_is_inclusive() {
    let bytes = raw(&first());
    let text = String::from_utf8(bytes.clone()).unwrap();
    let cases = [
        format!("{{\"version\":2,{}", &text[1..]),
        text.replacen("\"state\":{", "\"state\":{\"revision\":0,", 1),
        text.replacen("\"state\":{", "\"state\":{\"revi\\u0073ion\":0,", 1),
        format!("{{\"ver\\u0073ion\":2,{}", &text[1..]),
        format!("{text} null"),
        format!("{}0{}", "[".repeat(160), "]".repeat(160)),
    ];
    for invalid in cases {
        assert_eq!(
            codec::decode(invalid.as_bytes(), "entry-1", CUSTOM_TYPE, &policy()).unwrap_err(),
            CodecError::Json
        );
    }
    assert_eq!(
        codec::decode(&[0xff], "entry-1", CUSTOM_TYPE, &policy()).unwrap_err(),
        CodecError::Json
    );
    let mut padded = bytes;
    padded.resize(MAX_RECORD_BYTES, b' ');
    assert!(codec::decode(&padded, "entry-1", CUSTOM_TYPE, &policy()).is_ok());
    padded.push(b' ');
    assert_eq!(
        codec::decode(&padded, "entry-1", CUSTOM_TYPE, &policy()).unwrap_err(),
        CodecError::Limit
    );
}

#[test]
fn normalized_field_form_rejects_unknowns_defaults_and_skipped_nulls() {
    let mut base = first();
    base.state = serde_json::from_value(json!({"revision":0,"nextQueueId":0,"queue":[],
        "workflow":{"name":"work","status":"running","steps":[{"id":"s","title":"step","status":"pending"}]}}))
    .unwrap();
    accept(&base);
    for pointer in [
        "",
        "/state",
        "/state/workflow",
        "/state/workflow/steps/0",
        "/receipt/action",
        "/receipt/expected",
        "/executionControl",
        "/nodes/0",
        "/nodes/0/address",
        "/nodes/0/value",
        "/root",
        "/root/address",
        "/receipt",
    ] {
        let mut value = serde_json::to_value(&base).unwrap();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), json!(1));
        reject(&value, "entry-1", CodecError::Schema);
    }
    let mut queue = base.clone();
    queue.state = queued_state();
    queue.receipt.changed = true;
    queue.receipt.control_revision = 1;
    refresh_receipt(&mut queue);
    accept(&queue);
    let mut value = serde_json::to_value(&queue).unwrap();
    value["state"]["queue"][0]["unknown"] = json!(true);
    reject(&value, "entry-1", CodecError::Schema);
    for (parent, field) in [
        ("/state", "revision"),
        ("/state", "nextQueueId"),
        ("/state", "queue"),
        ("/state", "workflow"),
        ("/executionControl", "forgottenRunIds"),
        ("/executionControl", "recoveryRequest"),
    ] {
        let mut value = serde_json::to_value(first()).unwrap();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(field);
        reject(&value, "entry-1", CodecError::Schema);
    }
    let mut value = serde_json::to_value(first()).unwrap();
    value["receipt"]["runId"] = Value::Null;
    reject(&value, "entry-1", CodecError::Schema);
    // ControlAction itself has serialized defaults, too.
    let mut value = serde_json::to_value(first()).unwrap();
    value["receipt"]["action"] = json!({"action":"checkpoint","step":"s"});
    reject(&value, "entry-1", CodecError::Schema);
}

#[test]
fn format_epoch_outer_identity_and_policy_errors_are_sanitized() {
    let record = first();
    let bytes = raw(&record);
    assert_eq!(
        codec::decode(&bytes, "entry-1", "secret-custom-type", &policy()).unwrap_err(),
        CodecError::Format
    );
    mutate(
        &record,
        "/format",
        json!("secret-format"),
        CodecError::Format,
    );
    mutate(&record, "/version", json!(3), CodecError::Format);
    for field in ["sessionId", "lane", "epochId"] {
        mutate(
            &record,
            &format!("/epoch/{field}"),
            json!("another"),
            CodecError::Epoch,
        );
        mutate(
            &record,
            &format!("/epoch/{field}"),
            json!("secret/invalid"),
            CodecError::Identity,
        );
    }
    for args in [
        ("bad/id", "main", "epoch"),
        ("session", "", "epoch"),
        ("session", "main", "bad epoch"),
    ] {
        assert_eq!(
            ReadPolicy::new_epoch(args.0, args.1, args.2).unwrap_err(),
            CodecError::Identity
        );
    }
    for outer in ["", "bad/id", &"a".repeat(129)] {
        assert_eq!(
            codec::decode(&bytes, outer, CUSTOM_TYPE, &policy()).unwrap_err(),
            CodecError::Identity
        );
    }
    assert_eq!(
        codec::decode(&bytes, "other-entry", CUSTOM_TYPE, &policy()).unwrap_err(),
        CodecError::Receipt
    );
    assert_eq!(
        CodecError::Identity.to_string(),
        "invalid experimental ledger identity"
    );
    assert_eq!(
        CodecError::Format.to_string(),
        "unsupported experimental ledger format"
    );
    assert_eq!(
        CodecError::Receipt.to_string(),
        "invalid experimental ledger receipt"
    );
}

#[test]
fn receipt_scope_targets_revisions_and_predecessor_guards() {
    let first = first();
    for pointer in [
        "/receipt/requestId",
        "/receipt/actor",
        "/receipt/entryId",
        "/receipt/expected/sessionId",
        "/receipt/expected/lane",
        "/receipt/expected/bindingToken",
        "/receipt/expected/leafId",
        "/receipt/expected/entryId",
    ] {
        mutate(&first, pointer, json!("bad/secret"), CodecError::Identity);
    }
    for pointer in [
        "/receipt/entryId",
        "/receipt/expected/sessionId",
        "/receipt/expected/lane",
    ] {
        mutate(&first, pointer, json!("different"), CodecError::Receipt);
    }
    for action in ["status", "queue", "recover", "forget_run"] {
        mutate(
            &first,
            "/receipt/action",
            json!({"action":action}),
            CodecError::Receipt,
        );
    }
    let mut value = serde_json::to_value(&first).unwrap();
    value["receipt"]["runId"] = json!("run-1");
    reject(&value, "entry-1", CodecError::Receipt);
    value["receipt"]["action"] = json!({"action":"recover"});
    for run in ["", " untrimmed", "bad\nrun", &"r".repeat(513)] {
        value["receipt"]["runId"] = json!(run);
        reject(&value, "entry-1", CodecError::Receipt);
    }
    for (pointer, replacement) in [
        ("/receipt/controlRevision", json!(1)),
        ("/receipt/changed", json!(true)),
        ("/receipt/expected/controlRevision", json!(1)),
        ("/receipt/expected/entryId", json!("anchor")),
        ("/ordinal", json!(0)),
        ("/ordinal", json!(2)),
        ("/previous", json!({"entryId":"anchor","ordinal":0})),
    ] {
        mutate(&first, pointer, replacement, CodecError::Receipt);
    }
    let second = sequence(&["a", "b"]).pop().unwrap();
    for (pointer, replacement, error) in [
        ("/previous", Value::Null, CodecError::Receipt),
        ("/previous/entryId", json!("entry-2"), CodecError::Receipt),
        ("/previous/entryId", json!("bad/id"), CodecError::Identity),
        ("/previous/ordinal", json!(0), CodecError::Receipt),
        (
            "/receipt/expected/entryId",
            json!("not-previous"),
            CodecError::Receipt,
        ),
    ] {
        mutate(&second, pointer, replacement, error);
    }
    let mut overflow = second;
    overflow.state.revision = u64::MAX;
    overflow.receipt.control_revision = u64::MAX;
    overflow.receipt.expected.control_revision = u64::MAX;
    overflow.receipt.changed = true;
    reject(
        &serde_json::to_value(overflow).unwrap(),
        "entry-2",
        CodecError::Receipt,
    );
    assert_eq!(
        codec::receipt_reference(&first.receipt, 0).unwrap_err(),
        CodecError::Receipt
    );
}

#[test]
fn node_batch_shape_order_references_and_root_guards() {
    let base = first();
    for (pointer, replacement) in [
        ("/nodes", json!([])),
        (
            "/nodes",
            serde_json::to_value(vec![base.nodes[0].clone(); 513]).unwrap(),
        ),
        ("/nodes/0/address/slot", json!(1)),
        ("/nodes/0/address/ordinal", json!(2)),
        ("/nodes/0/address/entryId", json!("other")),
        ("/root/count", json!(2)),
        ("/root/address/entryId", json!("other")),
        ("/root/address/ordinal", json!(2)),
        ("/root/address/slot", json!(99)),
        ("/root/digest/0", json!(base.root.digest[0] ^ 1)),
        (
            "/nodes/0/value/digest/0",
            json!(base.nodes[0].value.digest[0] ^ 1),
        ),
        ("/nodes/0/value/ordinal", json!(0)),
    ] {
        mutate(&base, pointer, replacement, CodecError::Ledger);
    }
    let pair = sequence(&["a", "b"]).pop().unwrap();
    let slot = usize::from(pair.root.address.slot);
    for (suffix, replacement) in [
        (
            "digest/0",
            json!(pair.nodes[slot].right.as_ref().unwrap().digest[0] ^ 1),
        ),
        ("address/slot", json!(slot)),
        ("address/ordinal", json!(1)),
        ("count", json!(2)),
    ] {
        mutate(
            &pair,
            &format!("/nodes/{slot}/right/{suffix}"),
            replacement,
            CodecError::Ledger,
        );
    }
    let mut swapped = pair.clone();
    swapped.nodes.swap(0, 1);
    reject(
        &serde_json::to_value(swapped).unwrap(),
        "entry-2",
        CodecError::Ledger,
    );
    let triple = sequence(&["b", "a", "c"]).pop().unwrap();
    let slot = usize::from(triple.root.address.slot);
    for (suffix, replacement) in [
        ("address/ordinal", json!(3)),
        ("count", json!(3)),
        ("count", json!(0)),
        ("height", json!(0)),
    ] {
        mutate(
            &triple,
            &format!("/nodes/{slot}/left/{suffix}"),
            replacement,
            CodecError::Ledger,
        );
    }
    mutate(
        &triple,
        &format!("/nodes/{slot}/value/ordinal"),
        json!(3),
        CodecError::Ledger,
    );
}

#[test]
fn rehashed_corruption_reaches_local_proof_rejections() {
    for root_key in ["z", "b"] {
        let mut record = sequence(&["a", "b"]).pop().unwrap();
        let slot = usize::from(record.root.address.slot);
        record.nodes[slot].key = root_key.into();
        rehash(&mut record);
        // z makes current b absent; b finds the predecessor's receipt, not current.
        reject(
            &serde_json::to_value(record).unwrap(),
            "entry-2",
            CodecError::Proof,
        );
    }
    let mut record = sequence(&["c", "b", "a"]).pop().unwrap();
    record.nodes[0].key = "z".into(); // Valid local shape, violates the lookup upper bound.
    rehash(&mut record);
    reject(
        &serde_json::to_value(record).unwrap(),
        "entry-3",
        CodecError::Proof,
    );
    let mut record = sequence(&["b", "a", "c"]).pop().unwrap();
    record.receipt.request_id = "a".into(); // Requested path would need an older body.
    refresh_receipt(&mut record);
    reject(
        &serde_json::to_value(record).unwrap(),
        "entry-3",
        CodecError::Proof,
    );
}

#[test]
fn existing_control_and_execution_validators_are_not_bypassed_by_serde() {
    let base = first();
    for state in [
        json!({"revision":0,"nextQueueId":0,"workflow":{"name":"w","status":"running","steps":[]},"queue":[]}),
        json!({"revision":0,"nextQueueId":0,"workflow":{"name":"w","status":"completed","steps":[{"id":"s","title":"s","status":"pending"}]},"queue":[]}),
    ] {
        let typed: ControlState = serde_json::from_value(state.clone()).unwrap();
        assert!(typed.validate().is_err());
        mutate(&base, "/state", state, CodecError::Schema);
    }
    let mut bad_queue = queued_state();
    bad_queue.queue[0].updated_revision = 2;
    assert!(bad_queue.validate().is_err());
    mutate(
        &base,
        "/state",
        serde_json::to_value(bad_queue).unwrap(),
        CodecError::Schema,
    );
    for execution in [
        json!({"forgottenRunIds":["same","same"],"recoveryRequest":null}),
        json!({"forgottenRunIds":[],"recoveryRequest":{"runId":"r","startedSeq":0,"observedHistoryRevision":0,"unknownInvocationIds":[]}}),
        json!({"forgottenRunIds":["r"],"recoveryRequest":{"runId":"r","startedSeq":1,"observedHistoryRevision":1,"unknownInvocationIds":[]}}),
    ] {
        let typed: ExecutionControl = serde_json::from_value(execution.clone()).unwrap();
        assert!(typed.validate().is_err());
        mutate(&base, "/executionControl", execution, CodecError::Schema);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn reverse_objects(value: &Value) -> String {
    match value {
        Value::Object(map) => format!(
            "{{ {} }}",
            map.iter()
                .rev()
                .map(|(key, value)| format!(
                    "{} : {}",
                    serde_json::to_string(key).unwrap(),
                    reverse_objects(value)
                ))
                .collect::<Vec<_>>()
                .join(", \n")
        ),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(reverse_objects)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}

#[test]
fn canonical_receipt_matches_independent_golden_and_has_a_logical_budget() {
    // Parent supplies this independently generated fixture; this test never writes it.
    let golden: Value = serde_json::from_str(include_str!(
        "fixtures/experimental-ledger-v2/receipt-reset-canonical.json"
    ))
    .unwrap();
    let typed: DurableReceipt = serde_json::from_value(golden["receipt"].clone()).unwrap();
    assert_eq!(typed, first().receipt);
    let bytes = codec::receipt_canonical_bytes(&typed).unwrap();
    let body = serde_json::to_vec(&typed).unwrap();
    let prefix = b"rpi-durable-ledger-receipt-experimental-2\0";
    assert_eq!(&bytes[..prefix.len()], prefix);
    assert_eq!(
        &bytes[prefix.len()..prefix.len() + 4],
        &(body.len() as u32).to_be_bytes()
    );
    assert_eq!(&bytes[prefix.len() + 4..], body);
    assert_eq!(hex(&bytes), golden["canonicalHex"].as_str().unwrap());
    assert_eq!(
        bytes.len() as u64,
        golden["canonicalBytes"].as_u64().unwrap()
    );
    let independent: [u8; 32] = Sha256::digest(&bytes).into();
    assert_eq!(hex(&independent), golden["sha256Hex"].as_str().unwrap());
    assert_eq!(
        codec::receipt_reference(&typed, 1).unwrap().digest,
        independent
    );
    let reordered: DurableReceipt =
        serde_json::from_str(&reverse_objects(&golden["receipt"])).unwrap();
    assert_eq!(codec::receipt_canonical_bytes(&reordered).unwrap(), bytes);
    let record = first();
    let reordered = reverse_objects(&serde_json::to_value(&record).unwrap());
    let candidate = codec::decode(reordered.as_bytes(), "entry-1", CUSTOM_TYPE, &policy()).unwrap();
    assert_eq!(candidate.record(), &record);
    assert_eq!(candidate.record().root.digest, record.root.digest);
    let mut oversized = typed;
    oversized.action = ControlAction::Enqueue {
        text: "x".repeat(64 * 1024 + 1),
    };
    assert_eq!(
        codec::receipt_canonical_bytes(&oversized).unwrap_err(),
        CodecError::Limit
    );
    assert_eq!(
        codec::receipt_reference(&oversized, 1).unwrap_err(),
        CodecError::Limit
    );
}

fn queued_state() -> ControlState {
    serde_json::from_value(
        json!({"revision":1,"nextQueueId":1,"workflow":null,"queue":[{
        "id":"item-1","text":"synthetic queue item","status":"queued",
        "createdRevision":1,"updatedRevision":1}]}),
    )
    .unwrap()
}
fn refresh_receipt(record: &mut Record) {
    let reference = codec::receipt_reference(&record.receipt, record.ordinal).unwrap();
    for node in &mut record.nodes {
        if node.value.entry_id == record.receipt.entry_id {
            node.value = reference.clone();
        }
    }
    rehash(record);
}

#[test]
fn acceptance_is_only_local_not_prior_history_or_transition_authentication() {
    let mut record = sequence(&["b", "a", "c"]).pop().unwrap();
    record.previous.as_mut().unwrap().entry_id = "unresolved-predecessor".into();
    record.receipt.expected.entry_id = Some("unresolved-predecessor".into());
    record.receipt.expected.history_revision = 999;
    record.receipt.expected.binding_token = "unverified-binding".into();
    record.receipt.expected.leaf_id = Some("unverified-leaf".into());
    let slot = usize::from(record.root.address.slot);
    let old_child = record.nodes[slot].left.as_mut().unwrap();
    assert_ne!(old_child.address.entry_id, record.receipt.entry_id);
    old_child.address.entry_id = "unresolved-old-node".into();
    old_child.digest = [42; 32]; // Off-path older references are not authenticated.
    refresh_receipt(&mut record);
    accept(&record);
    let mut transition = first();
    transition.state = queued_state();
    transition.receipt.changed = true;
    transition.receipt.control_revision = 1;
    // A real Reset cannot enqueue this state. The codec intentionally does not replay it.
    assert_eq!(transition.receipt.action, ControlAction::Reset);
    transition.state.validate().unwrap();
    refresh_receipt(&mut transition);
    accept(&transition);
}
