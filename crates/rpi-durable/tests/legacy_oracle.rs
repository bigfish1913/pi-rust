//! Test-only P0/P8 differential oracle. All entries are synthetic, branch-selected
//! inputs, not sessions. This is neither a production reader nor an importer.
//! The pinned decoder is compiled directly, including its own unit tests.

#[rustfmt::skip]
#[allow(dead_code)]
#[path = "fixtures/legacy-oracle/reference.rs.txt"]
mod legacy;

use rpi_durable::control::{ControlAction, ControlError, ControlState};
use rpi_durable::execution;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
struct Frame {
    name: String,
    entry: Value,
    action: Option<ControlAction>,
    expected: Value,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/legacy-oracle/control-trace.json")).unwrap()
}

fn replay(entries: &[Value]) -> Result<legacy::DurableState, String> {
    let before = entries.to_vec();
    let result = legacy::DurableState::try_from_branch(entries);
    assert_eq!(
        entries,
        before.as_slice(),
        "oracle must not mutate its input"
    );
    result
}

// This helper models ONLY pure candidate preparation and a successful synthetic
// host revision commit. It does not assert persistence, authorization or actor
// assignment. In production the host assigns/enforces claimed_by; here None
// matches the legacy schema's lack of ownership knowledge, not proof of ownership.
fn commit(state: &mut ControlState, action: &ControlAction) {
    let before = state.clone();
    let mut candidate = state.apply(action).unwrap();
    assert_eq!(*state, before, "apply must leave the published state alone");
    assert_eq!(candidate.revision, before.revision, "host owns revision");
    if candidate != before {
        candidate.revision = before.revision.checked_add(1).unwrap();
    }
    candidate.validate().unwrap();
    *state = candidate;
}

// Explicit control-field whitelist; run and nextRunId are NOT control fields.
// Unlike a business-only comparison, this includes revision, queue counter,
// stamps AND native claimedBy. Only omitted serde defaults are expanded. No
// revision/counter rewriting or legacy-to-native state conversion happens here.
fn control_projection(state: Value) -> Value {
    let queue: Vec<Value> = state["queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            json!({
                "id": item["id"], "text": item["text"], "status": item["status"],
                "createdRevision": item["createdRevision"],
                "updatedRevision": item["updatedRevision"],
                "note": item.get("note").cloned().unwrap_or(json!("")),
                "claimedBy": item.get("claimedBy").cloned().unwrap_or(Value::Null)
            })
        })
        .collect();
    json!({
        "revision": state["revision"], "nextQueueId": state["nextQueueId"],
        "workflow": state.get("workflow").cloned().unwrap_or(Value::Null),
        "queue": queue
    })
}

// Revisions are different domains once legacy execution-only records intervene.
// This intentionally narrower business whitelist is used ONLY in the explicit
// revision-divergence test, which also asserts the raw revisions/stamps below.
fn business_projection(state: &Value) -> Value {
    let queue: Vec<Value> = state["queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            json!({
                "id": item["id"], "text": item["text"], "status": item["status"],
                "note": item["note"], "claimedBy": item["claimedBy"]
            })
        })
        .collect();
    json!({"workflow":state["workflow"], "nextQueueId":state["nextQueueId"], "queue":queue})
}

#[test]
fn literal_paired_trace_compares_every_prefix_three_ways() {
    let fixture = fixture();
    assert!(fixture.name.contains("synthetic"));
    assert!(fixture.name.contains("no real sessions"));
    assert_eq!(fixture.frames.len(), 17);
    let mut entries = Vec::new();
    let mut native = ControlState::default();
    for frame in fixture.frames {
        entries.push(frame.entry);
        let old = replay(&entries).unwrap();
        if let Some(action) = frame.action {
            commit(&mut native, &action);
        }
        native.validate().unwrap();
        let old_projection = control_projection(serde_json::to_value(&old).unwrap());
        let native_projection = control_projection(serde_json::to_value(&native).unwrap());
        assert_eq!(old_projection, frame.expected, "legacy: {}", frame.name);
        assert_eq!(native_projection, frame.expected, "native: {}", frame.name);
        assert_eq!(old_projection, native_projection, "paired: {}", frame.name);

        // Raw numeric and ownership assertions are independent of projection:
        // the control-only fixture aligns domains, never erases their values.
        let revision = frame.expected["revision"].as_u64().unwrap();
        let counter = frame.expected["nextQueueId"].as_u64().unwrap();
        assert_eq!(old.revision, revision);
        assert_eq!(native.revision, revision);
        assert_eq!(old.next_queue_id, counter);
        assert_eq!(native.next_queue_id, counter);
        assert_eq!(old.next_run_id, 0);
        assert!(old.run.is_none());
        for ((old_item, native_item), expected) in old
            .queue
            .iter()
            .zip(&native.queue)
            .zip(frame.expected["queue"].as_array().unwrap())
        {
            let created = expected["createdRevision"].as_u64().unwrap();
            let updated = expected["updatedRevision"].as_u64().unwrap();
            assert_eq!(old_item.created_revision, created);
            assert_eq!(native_item.created_revision, created);
            assert_eq!(old_item.updated_revision, updated);
            assert_eq!(native_item.updated_revision, updated);
            assert!(native_item.claimed_by.is_none());
        }
    }
}

#[test]
fn legacy_acceptance_does_not_override_native_validation_or_ownership() {
    let valid = json!({
        "id":"drift-base", "type":"custom", "customType":"durable-state",
        "data":{"format":"rpi-durable-journal","version":1,"kind":"snapshot",
            "baseRevision":0,"revision":1,"baseEntryId":null,
            "state":{"revision":1,"nextQueueId":1,"queue":[{
                "id":"item-1","text":"work","status":"queued",
                "createdRevision":1,"updatedRevision":1
            }]}}
    });
    let mut cases = Vec::new();
    let mut zero_stamp = valid.clone();
    zero_stamp["data"]["state"]["queue"][0]["createdRevision"] = json!(0);
    cases.push(("zero stamp", zero_stamp));
    let mut future_stamp = valid.clone();
    future_stamp["data"]["state"]["queue"][0]["updatedRevision"] = json!(2);
    cases.push(("future stamp", future_stamp));
    let mut counter = valid.clone();
    counter["data"]["state"]["nextQueueId"] = json!(0);
    cases.push(("counter behind", counter));
    let mut noncanonical = valid.clone();
    noncanonical["data"]["state"]["queue"][0]["id"] = json!("item-01");
    cases.push(("noncanonical id", noncanonical));
    let mut untrimmed = valid.clone();
    untrimmed["data"]["state"]["queue"][0]["text"] = json!(" work ");
    cases.push(("untrimmed text", untrimmed));
    let mut invalid_completion = fixture().frames[1].entry.clone();
    invalid_completion["id"] = json!("drift-workflow");
    invalid_completion["data"] = json!({
        "format":"rpi-durable-journal","version":1,"kind":"snapshot",
        "baseRevision":0,"revision":1,"state":{"revision":1,"queue":[],
            "workflow":{"name":"Premature","status":"completed","steps":[
                {"id":"a","title":"Not done","status":"pending"}
            ]}}
    });
    cases.push(("completed with pending step", invalid_completion));
    for (name, entry) in cases {
        let old = replay(&[entry]).unwrap();
        let native: ControlState =
            serde_json::from_value(serde_json::to_value(old).unwrap()).unwrap();
        assert!(native.validate().is_err(), "native must reject {name}");
    }

    // A whitelist must not hide actual host ownership when it is present.
    let old = replay(&[valid]).unwrap();
    let mut native: ControlState =
        serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    native.validate().unwrap();
    commit(
        &mut native,
        &ControlAction::Claim {
            item: "item-1".into(),
        },
    );
    native.queue[0].claimed_by = Some("synthetic-host-owner".into());
    native.validate().unwrap();
    let projected = control_projection(serde_json::to_value(&native).unwrap());
    assert_eq!(projected["queue"][0]["claimedBy"], "synthetic-host-owner");
    assert_ne!(projected["queue"][0]["claimedBy"], Value::Null);
    assert_eq!(old.queue[0].created_revision, 1);
}

#[test]
fn execution_only_delta_separates_revision_domains_without_erasing_stamps() {
    let fixture = fixture();
    let mut entries: Vec<Value> = fixture.frames[..=5]
        .iter()
        .map(|frame| frame.entry.clone())
        .collect();
    let mut native = ControlState::default();
    for frame in &fixture.frames[..=5] {
        if let Some(action) = &frame.action {
            commit(&mut native, action);
        }
    }
    entries.push(json!({
        "id":"execution-only-6","type":"custom","customType":"durable-state",
        "data":{"format":"rpi-durable-journal","version":1,"kind":"delta",
            "baseRevision":5,"revision":6,"baseEntryId":"oracle-5",
            "changes":[{"op":"setRun","run":{"id":"legacy-run-1","status":"running",
                "startedRevision":6,"updatedRevision":6,"tools":[]}},
                {"op":"setCounters","next_run_id":1,"next_queue_id":1}]}
    }));
    let old = replay(&entries).unwrap();
    commit(&mut native, &ControlAction::Status); // a read is not a control commit
    let old_projection = control_projection(serde_json::to_value(&old).unwrap());
    let native_projection = control_projection(serde_json::to_value(&native).unwrap());
    assert_ne!(old_projection, native_projection);
    assert_eq!(
        business_projection(&old_projection),
        business_projection(&native_projection)
    );
    assert_eq!(old.revision, 6);
    assert_eq!(native.revision, 5);
    assert_eq!(old.next_run_id, 1);
    assert_eq!(old.next_queue_id, 1);
    assert_eq!(native.next_queue_id, 1);
    assert_eq!(old.queue[0].created_revision, 5);
    assert_eq!(old.queue[0].updated_revision, 5);
    assert_eq!(native.queue[0].created_revision, 5);
    assert_eq!(native.queue[0].updated_revision, 5);
    assert_eq!(old.run.as_ref().unwrap().started_revision, 6);
    // Native ownership is not silently discarded by the business whitelist either.
    let mut owned = native_projection.clone();
    owned["queue"][0]["claimedBy"] = json!("host-only");
    assert_ne!(
        business_projection(&owned),
        business_projection(&native_projection)
    );
}

#[test]
fn selected_branch_forks_replay_independently_and_reject_cross_leaf_splicing() {
    // Branch selection is synthetic here; this is NOT a Session graph walker.
    let fixture = fixture();
    let shared: Vec<Value> = fixture.frames[..=5]
        .iter()
        .map(|frame| frame.entry.clone())
        .collect();
    let left = json!({
        "id":"fork-left-6","type":"custom","customType":"durable-state",
        "data":{"format":"rpi-durable-journal","version":1,"kind":"delta",
            "baseRevision":5,"revision":6,"baseEntryId":"oracle-5","changes":[
                {"op":"updateQueue","item":{"id":"item-1","text":"left work",
                    "status":"claimed","createdRevision":5,"updatedRevision":6}},
                {"op":"setWorkflowMeta","name":"Left","status":"paused","reason":"wait"}]}
    });
    let right = json!({
        "id":"fork-right-6","type":"custom","customType":"durable-state",
        "data":{"format":"rpi-durable-journal","version":1,"kind":"delta",
            "baseRevision":5,"revision":6,"baseEntryId":"oracle-5","changes":[
                {"op":"updateQueue","item":{"id":"item-1","text":"right work",
                    "status":"cancelled","createdRevision":5,"updatedRevision":6,"note":"cancelled"}},
                {"op":"setWorkflowMeta","name":"Right","status":"failed","reason":"stop"}]}
    });
    let mut left_branch = shared.clone();
    left_branch.push(left.clone());
    let mut right_branch = shared.clone();
    right_branch.push(right.clone());
    let left_state = replay(&left_branch).unwrap();
    let right_state = replay(&right_branch).unwrap();
    assert_eq!(left_state.queue[0].text, "left work");
    assert_eq!(left_state.queue[0].status, legacy::QueueStatus::Claimed);
    assert_eq!(
        left_state.workflow.as_ref().unwrap().status,
        legacy::WorkflowStatus::Paused
    );
    assert_eq!(right_state.queue[0].text, "right work");
    assert_eq!(right_state.queue[0].status, legacy::QueueStatus::Cancelled);
    assert_eq!(
        right_state.workflow.as_ref().unwrap().status,
        legacy::WorkflowStatus::Failed
    );
    assert_ne!(left_state, right_state);
    assert_eq!(replay(&shared).unwrap().queue[0].text, "compile");

    // Make numeric continuity valid so failure specifically exercises the native
    // entry anchor, rather than merely the duplicated revision of the two leaves.
    let mut crossed = right;
    crossed["id"] = json!("crossed-7");
    crossed["data"]["baseRevision"] = json!(6);
    crossed["data"]["revision"] = json!(7);
    crossed["data"]["baseEntryId"] = json!("fork-right-6");
    left_branch.push(crossed);
    assert!(replay(&left_branch).is_err());
    let mut wrong_leaf = left;
    wrong_leaf["data"]["baseEntryId"] = json!("fork-right-6");
    let mut wrong_branch = shared;
    wrong_branch.push(wrong_leaf);
    assert!(replay(&wrong_branch).is_err());
}

#[test]
fn malformed_suffix_never_publishes_a_successful_fixture_prefix() {
    let fixture = fixture();
    let good: Vec<Value> = fixture.frames[..=5]
        .iter()
        .map(|frame| frame.entry.clone())
        .collect();
    let published = replay(&good).unwrap();
    assert_eq!(
        control_projection(serde_json::to_value(&published).unwrap()),
        fixture.frames[5].expected
    );
    let suffix = fixture.frames[6].entry.clone();
    let mut variants = Vec::new();
    let mut version = suffix.clone();
    version["data"]["version"] = json!(2);
    variants.push(version);
    let mut gap = suffix.clone();
    gap["data"]["revision"] = json!(8);
    variants.push(gap);
    let mut missing_id = suffix.clone();
    missing_id.as_object_mut().unwrap().remove("id");
    variants.push(missing_id);
    let mut wrong_type = suffix.clone();
    wrong_type["type"] = json!("message");
    variants.push(wrong_type);
    let mut wrong_reference = suffix.clone();
    wrong_reference["data"]["baseEntryId"] = json!("unrelated-native-id");
    variants.push(wrong_reference);
    let mut bad_path = suffix;
    bad_path["data"]["changes"][0]["item"]["id"] = json!("item-99");
    variants.push(bad_path);
    for bad in variants {
        let mut candidate = good.clone();
        candidate.push(bad);
        assert!(replay(&candidate).is_err());
        // Failure has no replacement state: the previously decoded value and
        // immutable source prefix remain usable, never partial suffix success.
        assert_eq!(replay(&good).unwrap(), published);
    }

    let malformed_payload = json!({
        "id":"obsolete-bad-payload","type":"custom","customType":"durable-state",
        "data":{"not":"a journal record"}
    });
    let mut recoverable = vec![malformed_payload.clone()];
    recoverable.extend(good.clone());
    assert_eq!(replay(&recoverable).unwrap(), published);
    let mut malformed_envelope = malformed_payload;
    malformed_envelope.as_object_mut().unwrap().remove("id");
    let mut not_recoverable = vec![malformed_envelope];
    not_recoverable.extend(good);
    assert!(replay(&not_recoverable).is_err());
    // Even a syntactically valid linked delta cannot replace a complete base.
    assert!(replay(&[fixture.frames[6].entry.clone()]).is_err());
}

#[test]
fn legacy_execution_summaries_are_not_native_execution_or_replay_proof() {
    let entry = json!({
        "id":"synthetic-execution-snapshot","type":"custom","customType":"durable-state",
        "data":{"format":"rpi-durable-journal","version":1,"kind":"snapshot",
            "baseRevision":0,"revision":9,"baseEntryId":null,"state":{
                "revision":9,"nextRunId":3,"nextQueueId":0,"queue":[],
                "run":{"id":"legacy-run-3","status":"settled","startedRevision":7,
                    "updatedRevision":9,"outcome":"reported success","tools":[{
                        "callId":"model-call-1","name":"write","params":"summary only",
                        "status":"completed","result":"reported success"}]}
            }}
    });
    let old = replay(&[entry]).unwrap();
    let run = old.run.as_ref().unwrap();
    assert_eq!(run.status, legacy::RunStatus::Settled);
    assert_eq!(run.tools[0].status, legacy::ToolStatus::Completed);
    assert_eq!(run.tools[0].result, "reported success");
    assert_eq!(old.revision, 9);
    assert_eq!(old.next_run_id, 3);
    let raw_run = serde_json::to_value(run).unwrap();
    assert!(raw_run.get("startedSeq").is_none());
    assert!(raw_run.get("finishedRecordId").is_none());
    let tool = &raw_run["tools"][0];
    for absent in [
        "invocationId",
        "assistantEntryId",
        "reservedResultEntryId",
        "resultEntryId",
        "replay",
        "contentHash",
    ] {
        assert!(tool.get(absent).is_none(), "no native proof for {absent}");
    }
    assert!(serde_json::from_value::<execution::RunCheckpoint>(raw_run.clone()).is_err());
    assert!(serde_json::from_value::<execution::ToolCheckpoint>(tool.clone()).is_err());
    assert!(serde_json::from_value::<execution::ReplayPolicy>(Value::Null).is_err());

    // Deliberately NO summary-to-checkpoint or summary-to-Safe mapping. Native
    // control starts independently and cannot recover from legacy execution text.
    // Neither this oracle nor structural native validation proves storage IDs,
    // content hashes, branch membership, tool outcomes or replay authorization.
    let native = ControlState::default();
    native.validate().unwrap();
    assert_eq!(native.revision, 0);
    assert!(native.workflow.is_none());
    assert!(native.queue.is_empty());
    assert_eq!(
        native.apply(&ControlAction::Recover),
        Err(ControlError::ExecutionFactsRequired)
    );
    assert_eq!(
        native.apply(&ControlAction::ForgetRun),
        Err(ControlError::ExecutionFactsRequired)
    );
}
