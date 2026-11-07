//! Read-only inspection of a fixed *actual synthetic old-plugin capture*.
//! This is not fresh old execution, user data, an importer or a provenance grant.
use rpi_durable::control::ControlState;
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{
        legacy::{
            inspect_legacy_control, inspect_legacy_control_revision_one, read_legacy_branch,
            LegacyBranchSource, LegacyControlInspection, LegacyControlInspectionError as Error,
            LEGACY_CONTROL_POLICY, LEGACY_CONTROL_REVISION_ONE_POLICY, MAX_LEGACY_BRANCH_BYTES,
        },
        DurableActor, DurableError, DurableService,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};
const CAPTURE: &[u8] = include_bytes!("fixtures/legacy-revision-one/captured-control.json");
fn captured() -> Value {
    assert_eq!(CAPTURE.len(), 13278);
    let capture: Value = serde_json::from_slice(CAPTURE).unwrap();
    assert_eq!(
        capture["schema"],
        "rpi-synthetic-legacy-control-differential-v1"
    );
    assert_eq!(capture["entries"].as_array().unwrap().len(), 16);
    assert_eq!(capture["frames"].as_array().unwrap().len(), 17);
    assert_eq!(capture["entries"][0]["data"]["kind"], "snapshot");
    assert_eq!(capture["entries"][0]["data"]["revision"], 1);
    capture
}
fn expected(revision: usize) -> ControlState {
    let trace: Value = serde_json::from_str(include_str!(
        "../../rpi-durable/tests/fixtures/legacy-oracle/control-trace.json"
    ))
    .unwrap();
    serde_json::from_value(trace["frames"][revision]["expected"].clone()).unwrap()
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("bounded synthetic inspection")
}
fn memory() -> Session {
    Session::new(AgentHarnessOptions::default().session.storage(), None)
}
async fn append(s: &Session, id: &str, data: Value) {
    let entry = s
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "durable-state".into(),
                    data: Some(data),
                },
            },
            "main",
        )
        .await
        .unwrap();
    assert_eq!(entry.base().id, id);
}
async fn append_prefix(s: &Session, last: usize) {
    for entry in &captured()["entries"].as_array().unwrap()[..last] {
        append(s, entry["id"].as_str().unwrap(), entry["data"].clone()).await;
    }
}
async fn source(s: &Session, leaf: &str) -> LegacyBranchSource {
    read_legacy_branch(s, "main", Some(leaf)).await.unwrap()
}
async fn audit(s: &Session) -> Value {
    let store = s.storage();
    json!({"metadata":store.metadata().id,"revision":store.history_revision().await.unwrap(),
        "entries":store.find_entries(&EntryQuery::default()).await.unwrap(),
        "records":store.find_records(&RecordQuery::default()).await.unwrap()})
}
fn check(src: &LegacyBranchSource, expected: &ControlState) -> LegacyControlInspection {
    let before = serde_json::to_value(src).unwrap();
    assert_eq!(
        inspect_legacy_control(src).unwrap_err(),
        Error::UnanchoredHistory
    );
    let result = inspect_legacy_control_revision_one(src).unwrap();
    assert_eq!(result.policy, "anchored_revision_one_control_only_v1");
    assert_eq!(result.policy, LEGACY_CONTROL_REVISION_ONE_POLICY);
    assert_ne!(result.policy, LEGACY_CONTROL_POLICY);
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
        result.canonical_entry_ids.last().unwrap().as_str()
    );
    assert_eq!(&result.projection, expected);
    assert_eq!(serde_json::to_value(src).unwrap(), before);
    result
}
fn reject(src: &LegacyBranchSource, error: Error) {
    let before = serde_json::to_value(src).unwrap();
    assert_eq!(inspect_legacy_control_revision_one(src).unwrap_err(), error);
    assert_eq!(serde_json::to_value(src).unwrap(), before);
}
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
#[tokio::test]
async fn every_captured_prefix_requires_explicit_origin_and_preserves_revision_stamps() {
    bounded(async {
        let s = memory();
        for (index, entry) in captured()["entries"].as_array().unwrap().iter().enumerate() {
            let revision = index + 1;
            append(&s, entry["id"].as_str().unwrap(), entry["data"].clone()).await;
            let before = audit(&s).await;
            let src = source(&s, entry["id"].as_str().unwrap()).await;
            assert_eq!(src.canonical_entries.len(), revision);
            assert_eq!(
                inspect_legacy_control(&src).unwrap_err(),
                Error::UnanchoredHistory
            );
            if revision == 6 {
                reject(&src, Error::UnownedClaim);
            } else {
                let result = check(&src, &expected(revision));
                assert_eq!(result.projection.revision, revision as u64);
                assert_eq!(
                    result.projection.next_queue_id,
                    captured()["frames"][revision]["state"]["nextQueueId"]
                        .as_u64()
                        .unwrap()
                );
                for (item, old) in result.projection.queue.iter().zip(
                    captured()["frames"][revision]["state"]["queue"]
                        .as_array()
                        .unwrap(),
                ) {
                    assert!(item.claimed_by.is_none());
                    assert_eq!(
                        item.created_revision,
                        old["createdRevision"].as_u64().unwrap()
                    );
                    assert_eq!(
                        item.updated_revision,
                        old["updatedRevision"].as_u64().unwrap()
                    );
                }
            }
            assert_eq!(audit(&s).await, before);
        }
        let before = audit(&s).await;
        // Inspection is NOT adoption. Native publication and native service
        // decoding still reject the same actual legacy journal after Ok.
        let enabled = AgentHarness::create(AgentHarnessOptions {
            session: s.clone(),
            allow_existing_session: true,
            require_native_durable: true,
            ..Default::default()
        })
        .await;
        assert!(enabled.is_err());
        let h = AgentHarness::create(AgentHarnessOptions {
            session: s.clone(),
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let native = DurableService::bind(
            &h,
            DurableActor::owner("synthetic-inspector-owner").unwrap(),
        );
        assert!(matches!(
            native.snapshot().await,
            Err(DurableError::LegacyJournalPresent)
        ));
        h.close().await.unwrap();
        drop(h);
        assert_eq!(audit(&s).await, before);
        assert_eq!(CAPTURE.len(), 13278);
    })
    .await;
}
#[tokio::test]
async fn origin_and_every_snapshot_anchor_are_explicit_not_automatic_fallbacks() {
    bounded(async {
        let s=memory();append_prefix(&s,2).await;let src=source(&s,"entry-2").await;
        let before=audit(&s).await;check(&src,&expected(2));
        for candidate in [
            patch(&src,0,"/data/revision",json!(2)),
            patch(&src,0,"/data/baseRevision",json!(1)),
            patch(&src,0,"/data/baseEntryId",json!("omitted-prior")),
            patch(&src,0,"/data/kind",json!("delta")),
            patch(&src,1,"/data/baseRevision",json!(0)),
            patch(&src,1,"/data/revision",json!(3)),
            patch(&src,1,"/data/baseEntryId",json!("not-canonical")),
        ] {reject(&candidate,Error::UnanchoredHistory);}
        let mut zero=src.clone();zero.canonical_entries.truncate(1);zero.inspected_custom_entries=1;
        zero.scope.selected_leaf_id=Some("entry-1".into());
        zero.canonical_entries[0]["data"]["revision"]=json!(0);
        zero.canonical_entries[0]["data"]["state"]["revision"]=json!(0);
        reject(&zero,Error::UnanchoredHistory);
        let old_default=inspect_legacy_control(&zero).unwrap();
        assert_eq!(old_default.policy,LEGACY_CONTROL_POLICY);assert_eq!(old_default.projection.revision,0);
        // A later valid snapshot cannot hide an invalid earlier prefix.
        let mut snap=src.clone();let mut raw=captured()["frames"][2]["state"].clone();
        raw.as_object_mut().unwrap().remove("action");
        snap.canonical_entries[1]["data"]=json!({"format":"rpi-durable-journal","version":1,"kind":"snapshot",
            "baseRevision":1,"revision":2,"baseEntryId":"entry-1","state":raw});
        check(&snap,&expected(2));
        reject(&patch(&snap,1,"/data/baseEntryId",json!("wrong")),Error::UnanchoredHistory);
        reject(&patch(&snap,1,"/data/baseEntryId",Value::Null),Error::UnanchoredHistory);
        // Explicitly remove the first canonical record and present the later
        // snapshot as a standalone truncated source; origin policy cannot infer
        // the omitted history or invent revision one for it.
        let mut truncated=snap.clone();truncated.canonical_entries.remove(0);
        truncated.inspected_custom_entries=1;
        truncated.canonical_entries[0]["data"]["baseEntryId"]=Value::Null;
        reject(&truncated,Error::UnanchoredHistory);
        reject(&patch(&snap,0,"/data/state/future",json!(true)),Error::UnsupportedRecord);
        reject(&patch(&snap,0,"/data/state/workflow/name",json!("")),Error::InvalidLegacyHistory);
        reject(&patch(&snap,0,"/data/state/workflow/name",json!(" untrimmed ")),Error::NativeStateRejected);
        assert_eq!(audit(&s).await,before);
    }).await;
}
#[tokio::test]
async fn shared_closed_history_claim_metadata_and_budget_rejections_are_not_relaxed() {
    bounded(async {
        let s = memory();
        append_prefix(&s, 7).await;
        let src = source(&s, "entry-7").await;
        let before = audit(&s).await;
        check(&src, &expected(7));
        for (candidate, error) in [
            (
                patch(&src, 0, "/data/future", json!(true)),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 0, "/data/state/run", json!({"id":"invented"})),
                Error::ExecutionHistory,
            ),
            (
                patch(&src, 0, "/data/state/nextRunId", json!(1)),
                Error::ExecutionHistory,
            ),
            (
                patch(
                    &src,
                    1,
                    "/data/changes",
                    json!([{"op":"setRun","run":null}]),
                ),
                Error::ExecutionHistory,
            ),
            (
                patch(&src, 4, "/data/changes/1/item/claimedBy", json!(null)),
                Error::UnsupportedRecord,
            ),
            (
                patch(&src, 1, "/data/changes/0/step_id", json!("absent")),
                Error::InvalidLegacyHistory,
            ),
            (patch(&src, 0, "/seq", json!(0)), Error::InvalidSource),
            (patch(&src, 1, "/seq", json!(1)), Error::InvalidSource),
            (
                patch(&src, 1, "/id", json!("entry-1")),
                Error::InvalidSource,
            ),
        ] {
            reject(&candidate, error);
        }
        let mut changed = src.clone();
        changed.runtime_entries.push(json!({}));
        reject(&changed, Error::MixedHistory);
        changed = src.clone();
        changed.native_control_entries.push(json!({}));
        reject(&changed, Error::MixedHistory);
        changed = src.clone();
        changed.other_custom_entries = 1;
        reject(&changed, Error::OpaqueHistory);
        changed = src.clone();
        changed.inspected_custom_entries += 1;
        reject(&changed, Error::InvalidSource);
        changed = src.clone();
        changed.canonical_entries = vec![src.canonical_entries[0].clone(); 257];
        changed.inspected_custom_entries = 257;
        reject(&changed, Error::LimitExceeded);
        let oversized = patch(
            &src,
            0,
            "/data/state/workflow/reason",
            json!("x".repeat(MAX_LEGACY_BRANCH_BYTES)),
        );
        reject(&oversized, Error::LimitExceeded);
        assert_eq!(audit(&s).await, before);
    })
    .await;
}
#[tokio::test]
async fn jsonl_sibling_selection_and_cold_reopen_keep_original_payloads_and_bytes() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "origin.jsonl",
            JsonlV4Header::new(
                "synthetic-revision-one".into(),
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
        append_prefix(&s, 5).await;
        s.move_lane("main", Some("entry-3")).await.unwrap();
        append(&s, "sibling-4", captured()["entries"][3]["data"].clone()).await;
        s.drain().await;
        let path = dir.path().join("origin.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let before = audit(&s).await;
        let a = source(&s, "entry-5").await;
        let b = source(&s, "sibling-4").await;
        assert_eq!(a.scope.selected_leaf_id.as_deref(), Some("entry-5"));
        assert_eq!(a.scope.current_leaf_id.as_deref(), Some("sibling-4"));
        assert_eq!(b.canonical_entries.len(), 4);
        assert_eq!(a.canonical_entries.len(), 5);
        let pa = check(&a, &expected(5));
        let pb = check(&b, &expected(4));
        assert_ne!(pa.projection, pb.projection);
        for (native_envelope, old_host_envelope) in a
            .canonical_entries
            .iter()
            .zip(captured()["entries"].as_array().unwrap())
        {
            assert_eq!(
                native_envelope["data"], old_host_envelope["data"],
                "payloads are not rebased or decoded back into records"
            );
        }
        assert_eq!(audit(&s).await, before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        s.drain().await;
        drop(s);
        let store = JsonlSessionStorage::load(
            fs,
            "origin.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = Session::new(Arc::new(store), None);
        let before = audit(&reopened).await;
        assert_eq!(
            check(&source(&reopened, "entry-5").await, &expected(5)).projection,
            pa.projection
        );
        assert_eq!(
            check(&source(&reopened, "sibling-4").await, &expected(4)).projection,
            pb.projection
        );
        assert_eq!(audit(&reopened).await, before);
        reopened.drain().await;
        drop(reopened);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    })
    .await;
}
