//! Inspection candidates are comparisons, never import permits or execution facts.
use super::*;
use rpi_durable::control::ControlState;
#[path = "control_inspection_bounds.rs"]
mod bounds;
use rpi_harness::durable::legacy::{
    inspect_legacy_control, LegacyControlInspectionError as Error, LEGACY_CONTROL_POLICY,
};

fn frames() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rpi-durable/tests/fixtures/legacy-oracle/control-trace.json"
    )))
    .unwrap();
    let frames = fixture["frames"].as_array().unwrap().clone();
    assert_eq!(frames.len(), 17);
    frames
}

async fn append_frame(s: &Session, frame: &Value) {
    append(
        s,
        frame["entry"]["id"].as_str().unwrap(),
        "durable-state",
        Some(frame["entry"]["data"].clone()),
    )
    .await;
}

async fn trace(s: &Session, last: usize) -> LegacyBranchSource {
    let frames = frames();
    for frame in &frames[..=last] {
        append_frame(s, frame).await;
    }
    source(s, frames[last]["entry"]["id"].as_str().unwrap()).await
}

fn reject(src: &LegacyBranchSource, expected: Error) {
    let before = serde_json::to_value(src).unwrap();
    assert_eq!(inspect_legacy_control(src).unwrap_err(), expected);
    assert_eq!(serde_json::to_value(src).unwrap(), before);
}

fn check(src: &LegacyBranchSource, expected: &Value) -> ControlState {
    let before = serde_json::to_value(src).unwrap();
    let result = inspect_legacy_control(src).unwrap();
    assert_eq!(result.policy, "anchored_control_only_v1");
    assert_eq!(result.policy, LEGACY_CONTROL_POLICY);
    assert_eq!(result.scope, src.scope);
    assert_eq!(
        result.canonical_entry_ids,
        src.canonical_entries
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        result.source_anchor_id,
        src.canonical_entries.last().unwrap()["id"]
            .as_str()
            .unwrap()
    );
    assert_eq!(
        result.projection,
        serde_json::from_value::<ControlState>(expected.clone()).unwrap()
    );
    assert_eq!(serde_json::to_value(src).unwrap(), before);
    result.projection
}

// Patch only cloned, actually extracted envelopes; never fabricate native apply output.
fn patch(src: &LegacyBranchSource, index: usize, path: &str, value: Value) -> LegacyBranchSource {
    let mut out = src.clone();
    let (parent, key) = path.rsplit_once('/').unwrap();
    out.canonical_entries[index]
        .pointer_mut(parent)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(key.into(), value);
    out
}

fn complete_snapshot(revision: u64, base: u64, anchor: Option<&str>, state: Value) -> Value {
    json!({"format":"rpi-durable-journal","version":1,"kind":"snapshot",
        "baseRevision":base,"revision":revision,"baseEntryId":anchor,"state":state})
}

#[tokio::test]
async fn every_literal_control_prefix_preserves_revision_stamps_and_storage() {
    bounded(async {
        let s = memory();
        for (index, frame) in frames().iter().enumerate() {
            append_frame(&s, frame).await;
            let before = audit(&s).await;
            let src = source(&s, frame["entry"]["id"].as_str().unwrap()).await;
            let old = legacy::DurableState::try_from_branch(&src.canonical_entries).unwrap();
            let expected =
                serde_json::from_value::<ControlState>(frame["expected"].clone()).unwrap();
            assert_eq!(old.revision, expected.revision);
            assert_eq!(old.revision, index as u64);
            assert_eq!(old.next_queue_id, expected.next_queue_id);
            assert_eq!(old.next_run_id, 0);
            assert!(old.run.is_none());
            assert_eq!(
                serde_json::from_value::<ControlState>(json!({
                    "revision":old.revision,"nextQueueId":old.next_queue_id,
                    "workflow":&old.workflow,"queue":&old.queue,
                }))
                .unwrap(),
                expected,
                "full raw reference control projection at frame {index}"
            );
            assert_eq!(old.queue.len(), expected.queue.len());
            for (raw, literal) in old.queue.iter().zip(&expected.queue) {
                assert_eq!(raw.id, literal.id);
                assert_eq!(raw.created_revision, literal.created_revision);
                assert_eq!(raw.updated_revision, literal.updated_revision);
            }
            // The independent full reference supplies the raw replay, not native apply.
            assert_eq!(
                serde_json::to_value(&old.workflow).unwrap(),
                serde_json::to_value(&expected.workflow).unwrap()
            );
            if index == 6 {
                reject(&src, Error::UnownedClaim);
            } else {
                check(&src, &frame["expected"]);
            }
            assert_eq!(audit(&s).await, before, "frame {index}");
        }
    })
    .await;
}

#[tokio::test]
async fn closed_control_policy_rejects_unknown_execution_and_mixed_sources() {
    bounded(async {
        let s = memory();
        let src = trace(&s, 15).await;
        let before = audit(&s).await;
        check(&src, &frames()[15]["expected"]);
        let mut cases = vec![
            (
                patch(&src, 0, "/data/future", json!(true)),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 0, "/data/state/future", json!(true)),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 14, "/data/changes/0/workflow/future", json!(true)),
                Error::UnsupportedRecord,
            ),
            (
                patch(
                    &src,
                    14,
                    "/data/changes/0/workflow/steps/0/future",
                    json!(true),
                ),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 15, "/data/changes/0/future", json!(true)),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 0, "/data/state/nextRunId", json!(1)),
                Error::ExecutionHistory,
            ),
            (
                patch(&src, 0, "/data/state/run", json!({"id":"run-1"})),
                Error::ExecutionHistory,
            ),
            (
                patch(
                    &src,
                    15,
                    "/data/changes",
                    json!([{"op":"setRun","run":null}]),
                ),
                Error::ExecutionHistory,
            ),
            (
                patch(
                    &src,
                    15,
                    "/data/changes",
                    json!([{"op":"setCounters","next_run_id":1,"next_queue_id":3}]),
                ),
                Error::ExecutionHistory,
            ),
            (
                patch(&src, 15, "/data/changes/0/op", json!("set_workflow_meta")),
                Error::UnsupportedRecord,
            ),
        ];
        for owner in [Value::Null, json!("untrusted-owner")] {
            cases.push((
                patch(&src, 5, "/data/changes/0/item/claimedBy", owner),
                Error::UnsupportedRecord,
            ));
        }
        // Unknown fields and even setRun(null) remain accepted by the pinned old
        // decoder. Inspection deliberately excludes them rather than relaxing policy.
        assert!(legacy::DurableState::try_from_branch(&cases[0].0.canonical_entries).is_err());
        for index in [1, 2, 3, 4, 7, 10, 11] {
            assert!(
                legacy::DurableState::try_from_branch(&cases[index].0.canonical_entries).is_ok()
            );
        }
        let mut mixed = src.clone();
        mixed.runtime_entries.push(json!({"opaque":"runtime"}));
        cases.push((mixed, Error::MixedHistory));
        let mut mixed = src.clone();
        mixed
            .native_control_entries
            .push(json!({"opaque":"native"}));
        cases.push((mixed, Error::MixedHistory));
        let mut opaque = src.clone();
        opaque.other_custom_entries = 1;
        cases.push((opaque, Error::OpaqueHistory));
        let mut empty = src.clone();
        empty.canonical_entries.clear();
        empty.inspected_custom_entries = 0;
        cases.push((empty, Error::NoLegacyHistory));
        let mut wrong_count = src.clone();
        wrong_count.inspected_custom_entries -= 1;
        cases.push((wrong_count, Error::InvalidSource));
        for (candidate, error) in cases {
            reject(&candidate, error);
        }
        assert_eq!(audit(&s).await, before);
    })
    .await;
}

#[tokio::test]
async fn every_prefix_must_be_zero_rooted_anchored_and_native_valid() {
    bounded(async {
        let s = memory();
        trace(&s, 16).await;
        let old = legacy::DurableState::try_from_branch(&source(&s, "oracle-16").await.canonical_entries).unwrap();
        let mut final_state = serde_json::to_value(old).unwrap();
        final_state["revision"] = json!(17);
        append(&s, "clean-17", "durable-state", Some(complete_snapshot(17, 16, Some("oracle-16"), final_state))).await;
        let src = source(&s, "clean-17").await;
        let before = audit(&s).await;
        assert!(inspect_legacy_control(&src).is_ok());
        let cases = [
            (patch(&src, 2, "/data/baseEntryId", json!("wrong")), Error::UnanchoredHistory),
            (patch(&src, 2, "/data/baseRevision", json!(0)), Error::UnanchoredHistory),
            (patch(&src, 2, "/data/revision", json!(3)), Error::UnanchoredHistory),
            // Snapshots are subject to exactly the same next-revision and anchor checks.
            (patch(&src, 17, "/data/baseEntryId", Value::Null), Error::UnanchoredHistory),
            (patch(&src, 17, "/data/baseRevision", json!(0)), Error::UnanchoredHistory),
            (patch(&patch(&src, 17, "/data/revision", json!(18)), 17, "/data/state/revision", json!(18)), Error::UnanchoredHistory),
            (patch(&src, 1, "/data", json!({"obsolete":"bad payload"})), Error::UnsupportedRecord),
            (patch(&src, 1, "/data/unknown", json!(true)), Error::UnsupportedRecord),
            (patch(&src, 2, "/data/changes/0/step_id", json!("missing")), Error::InvalidLegacyHistory),
        ];
        for (candidate, error) in cases {
            // A later self-contained snapshot hides these errors in legacy replay.
            let decoded = legacy::DurableState::try_from_branch(&candidate.canonical_entries).unwrap();
            assert_eq!(decoded.revision, candidate.canonical_entries[17]["data"]["revision"].as_u64().unwrap());
            reject(&candidate, error);
        }
        s.move_lane("main", None).await.unwrap();
        append(&s, "nonzero-root", "durable-state", Some(complete_snapshot(40, 39, None,
            json!({"revision":40,"nextRunId":0,"nextQueueId":0,"queue":[]})))).await;
        let truncated = source(&s, "nonzero-root").await;
        assert_eq!(legacy::DurableState::try_from_branch(&truncated.canonical_entries).unwrap().revision, 40);
        reject(&truncated, Error::UnanchoredHistory);
        // The first branch itself was untouched; the extra append above was explicit.
        let observed = source(&s, "clean-17").await;
        assert_eq!(observed.canonical_entries, src.canonical_entries);
        assert!(observed.scope.history_revision > src.scope.history_revision);
        assert_eq!(observed.scope.current_leaf_id.as_deref(), Some("nonzero-root"));
        assert_eq!(before["entries"].as_array().unwrap().len() + 1,
            audit(&s).await["entries"].as_array().unwrap().len());

        // Begin with a valid empty revision-zero root, NOT queue stamps from rev 1.
        let native = memory();
        append(&native, "zero", "durable-state", Some(frames()[0]["entry"]["data"].clone())).await;
        let valid = json!({"revision":1,"nextRunId":0,"nextQueueId":1,
            "workflow":{"name":"Valid","status":"running","steps":[{"id":"step","title":"Check","status":"pending"}]},
            "queue":[{"id":"item-1","text":"valid","status":"queued","createdRevision":1,"updatedRevision":1}]});
        append(&native, "one", "durable-state", Some(complete_snapshot(1, 0, Some("zero"), valid))).await;
        let pair = source(&native, "one").await;
        let before = audit(&native).await;
        assert!(inspect_legacy_control(&pair).is_ok());
        let mutations = [
            ("/data/state/queue/0/id", json!("arbitrary")),
            ("/data/state/queue/0/id", json!("item-01")),
            ("/data/state/queue/0/createdRevision", json!(0)),
            ("/data/state/queue/0/updatedRevision", json!(2)),
            ("/data/state/nextQueueId", json!(0)),
            ("/data/state/queue/0/text", json!(" untrimmed ")),
            ("/data/state/workflow/name", json!(" untrimmed ")),
            ("/data/state/workflow/status", json!("completed")),
        ];
        for (path, value) in mutations {
            let candidate = patch(&pair, 1, path, value);
            assert!(legacy::DurableState::try_from_branch(&candidate.canonical_entries).is_ok(), "{path}");
            reject(&candidate, Error::NativeStateRejected);
        }
        assert_eq!(audit(&native).await, before);
    })
    .await;
}

fn sibling_delta(label: &str) -> Value {
    json!({"format":"rpi-durable-journal","version":1,"kind":"delta",
        "baseRevision":0,"revision":1,"baseEntryId":"zero",
        "changes":[{"op":"appendQueue","item":{"id":"item-1","text":label,
            "status":"queued","createdRevision":1,"updatedRevision":1}},
            {"op":"setCounters","next_run_id":0,"next_queue_id":1}]})
}

fn sibling_literal(label: &str) -> Value {
    json!({"revision":1,"nextQueueId":1,"workflow":null,
        "queue":[{"id":"item-1","text":label,"status":"queued",
            "createdRevision":1,"updatedRevision":1}]})
}

#[tokio::test]
async fn jsonl_sibling_inspections_and_provider_free_reopen_never_write() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "inspection.jsonl",
            JsonlV4Header::new(
                "control-inspection".into(),
                1,
                "/synthetic".into(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let s = Session::new(Arc::new(store), None);
        append(
            &s,
            "zero",
            "durable-state",
            Some(frames()[0]["entry"]["data"].clone()),
        )
        .await;
        append(&s, "A", "durable-state", Some(sibling_delta("branch A"))).await;
        s.move_lane("main", Some("zero")).await.unwrap();
        append(&s, "B", "durable-state", Some(sibling_delta("branch B"))).await;
        s.drain().await;
        let path = dir.path().join("inspection.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let before = audit(&s).await;
        let a = source(&s, "A").await;
        let b = source(&s, "B").await;
        assert_eq!(a.scope.selected_leaf_id.as_deref(), Some("A"));
        assert_eq!(a.scope.current_leaf_id.as_deref(), Some("B"));
        assert_eq!(b.scope.selected_leaf_id.as_deref(), Some("B"));
        let pa = check(&a, &sibling_literal("branch A"));
        let pb = check(&b, &sibling_literal("branch B"));
        assert_ne!(pa, pb);
        assert_eq!(audit(&s).await, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        s.drain().await;
        drop(s);
        let store = JsonlSessionStorage::load(
            fs,
            "inspection.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = Session::new(Arc::new(store), None);
        let before = audit(&reopened).await;
        for (leaf, old, expected, projection) in [
            ("A", &a, sibling_literal("branch A"), pa),
            ("B", &b, sibling_literal("branch B"), pb),
        ] {
            let new = source(&reopened, leaf).await;
            assert_eq!(check(&new, &expected), projection);
            assert_ne!(new.scope.binding_token, old.scope.binding_token);
            assert_eq!(new.scope.session_id, old.scope.session_id);
            assert_eq!(new.scope.history_revision, old.scope.history_revision);
            assert_eq!(new.scope.current_leaf_id, old.scope.current_leaf_id);
            assert_eq!(new.scope.selected_leaf_id, old.scope.selected_leaf_id);
            assert_eq!(new.canonical_entries, old.canonical_entries);
        }
        assert_eq!(audit(&reopened).await, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        reopened.drain().await;
        drop(reopened);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    })
    .await;
}
