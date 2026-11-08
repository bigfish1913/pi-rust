//! Explicit offline test bridge, never a production importer. The ignored test
//! requires a freshly executed old-plugin artifact from the isolated runner.
use rpi_durable::control::{ControlAction, ControlState};
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{DurableActor, DurableError, DurableMutation, DurableService},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::{EntryQuery, RecordQuery},
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc};

const OWNER: &str = "synthetic-differential-owner";
const TRACE: &str =
    include_str!("../../rpi-durable/tests/fixtures/legacy-oracle/control-trace.json");
const COMMIT: &str = "041c981b21ac42348feee371757f5ef26ecbdcab";
const PLUGIN_SHA: &str = "0f4be632b1f5dab5dfa0ac9cf55bb9119b1a37ec3ac195f08161dca9480fdaac";
const TRACE_SHA: &str = "3aa91d01e3e92a10b34b4e89a923802537437f6076b359edb2d2b4bfad050343";
const MAX_ARTIFACT: usize = 2 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Artifact {
    schema: String,
    legacy_commit: String,
    plugin_sha256: String,
    trace_sha256: String,
    frames: Vec<Observed>,
    entries: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Observed {
    index: usize,
    action: Option<Value>,
    state: Value,
    entry_count: usize,
    entry_id: Option<String>,
}
fn closed(value: &Value, fields: &[&str]) -> Result<(), String> {
    let map = value.as_object().ok_or("object required")?;
    if map.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err("unknown field".into());
    }
    Ok(())
}
fn literal() -> Vec<Value> {
    serde_json::from_str::<Value>(TRACE).unwrap()["frames"]
        .as_array()
        .unwrap()
        .clone()
}
fn old_control(value: &Value) -> Result<ControlState, String> {
    closed(
        value,
        &[
            "revision",
            "nextRunId",
            "nextQueueId",
            "workflow",
            "run",
            "queue",
        ],
    )?;
    if value["nextRunId"].as_u64() != Some(0) || value.get("run").is_some_and(|run| !run.is_null())
    {
        return Err("execution is outside this comparison".into());
    }
    if let Some(workflow) = value.get("workflow").filter(|v| !v.is_null()) {
        closed(workflow, &["name", "status", "steps", "reason"])?;
        for step in workflow["steps"].as_array().ok_or("steps required")? {
            closed(step, &["id", "title", "status", "note"])?;
        }
    }
    for item in value["queue"].as_array().ok_or("queue required")? {
        // Even a null invented owner is rejected rather than erased by serde.
        closed(
            item,
            &[
                "id",
                "text",
                "status",
                "createdRevision",
                "updatedRevision",
                "note",
            ],
        )?;
    }
    let state:ControlState=serde_json::from_value(json!({"revision":value["revision"],
        "nextQueueId":value["nextQueueId"],"workflow":value.get("workflow"),"queue":value["queue"]}))
        .map_err(|_|"invalid control shape")?;
    state.validate().map_err(|_| "invalid control values")?;
    Ok(state)
}
fn validate_artifact(bytes: &[u8]) -> Result<Vec<ControlState>, String> {
    if bytes.len() > MAX_ARTIFACT {
        return Err("artifact byte limit".into());
    }
    let artifact: Artifact = serde_json::from_slice(bytes).map_err(|_| "artifact schema")?;
    if artifact.schema != "rpi-synthetic-legacy-control-differential-v1"
        || artifact.legacy_commit != COMMIT
        || artifact.plugin_sha256 != PLUGIN_SHA
        || artifact.trace_sha256 != TRACE_SHA
    {
        return Err("artifact version/provenance labels".into());
    }
    if artifact.frames.len() != 17 || artifact.entries.len() != 16 {
        return Err("artifact count".into());
    }
    let frames = literal();
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    for (index, (observed, expected)) in artifact.frames.iter().zip(&frames).enumerate() {
        let expected_id = (index > 0).then(|| format!("entry-{index}"));
        if observed.index != index
            || observed.action.as_ref() != expected.get("action")
            || observed.entry_count != index
            || observed.entry_id != expected_id
        {
            return Err("frame identity/action/order".into());
        }
        let old = old_control(&observed.state)?;
        let expected: ControlState =
            serde_json::from_value(expected["expected"].clone()).map_err(|_| "literal invalid")?;
        if old != expected {
            return Err("old state differs from complete literal control projection".into());
        }
        out.push(old);
    }
    for (index, entry) in artifact.entries.iter().enumerate() {
        let revision = index + 1;
        closed(entry, &["id", "type", "customType", "data"])?;
        let id = entry["id"].as_str().ok_or("entry id")?;
        if id != format!("entry-{revision}")
            || !ids.insert(id)
            || entry["type"] != "custom"
            || entry["customType"] != "durable-state"
        {
            return Err("entry identity/type".into());
        }
        let data = &entry["data"];
        closed(
            data,
            &[
                "format",
                "version",
                "kind",
                "baseRevision",
                "revision",
                "baseEntryId",
                "state",
                "changes",
            ],
        )?;
        if data["format"] != "rpi-durable-journal"
            || data["version"] != 1
            || data["revision"] != revision
            || data["baseRevision"] != revision - 1
        {
            return Err("journal identity/revision".into());
        }
        if index == 0 {
            if data["kind"] != "snapshot"
                || !data["baseEntryId"].is_null()
                || !data["changes"].is_null()
                || old_control(&data["state"])? != out[1]
            {
                return Err("revision-one origin snapshot".into());
            }
        } else if data["kind"] != "delta"
            || data["baseEntryId"] != format!("entry-{index}")
            || !data["state"].is_null()
            || data["changes"].as_array().is_none_or(Vec::is_empty)
            || data["changes"] != expected_emitted_changes(revision)
        {
            return Err("journal delta/anchor/literal changes".into());
        }
    }
    // These labels/shape checks do NOT authenticate the file. Only the runner's
    // checked execution/copy hashes supply the surrounding test provenance.
    Ok(out)
}
// The hand-authored decoder trace used appendQueue then setCounters. Actual
// plugin output emits counters FIRST at these three enqueue frames. Assert this
// concrete difference; do not sort arbitrary changes or rewrite captured data.
fn expected_emitted_changes(revision: usize) -> Value {
    let mut changes = literal()[revision]["entry"]["data"]["changes"].clone();
    if matches!(revision, 5 | 8 | 13) {
        assert_eq!(changes[0]["op"], "appendQueue");
        assert_eq!(changes[1]["op"], "setCounters");
        assert_eq!(changes.as_array().unwrap().len(), 2);
        changes.as_array_mut().unwrap().swap(0, 1);
    }
    changes
}
fn expected_native(index: usize) -> ControlState {
    let mut expected: ControlState =
        serde_json::from_value(literal()[index]["expected"].clone()).unwrap();
    // Explicit, asserted semantic difference. Ownership is NOT imported from old
    // data: the real service assigns it from this independent native test actor.
    if index >= 6 {
        expected.queue[0].claimed_by = Some(OWNER.into());
    }
    expected
}
async fn open(session: Session) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session,
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    })
    .await
    .unwrap()
}
fn service(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner(OWNER).unwrap())
}
async fn audit(h: &AgentHarness) -> Value {
    let store = h.session().storage();
    json!({"entries":store.find_entries(&EntryQuery::default()).await.unwrap(),
        "records":store.find_records(&RecordQuery::default()).await.unwrap()})
}
async fn exercise(h: &AgentHarness, old: Option<&[ControlState]>) -> (ControlState, Value) {
    let s = service(h);
    let frames = literal();
    let mut receipts = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        if let Some(action) = frame.get("action") {
            let request = DurableMutation {
                request_id: format!("paired-{index}"),
                expected: s.snapshot().await.unwrap().scope,
                action: serde_json::from_value::<ControlAction>(action.clone()).unwrap(),
                run_id: None,
            };
            let result = s.mutate(request).await.unwrap();
            assert!(result.receipt.changed);
            assert!(!result.replayed);
            assert_eq!(result.receipt.actor, OWNER);
            assert_eq!(result.receipt.control_revision, index as u64);
            assert_eq!(
                result.receipt.action,
                serde_json::from_value::<ControlAction>(action.clone()).unwrap()
            );
            assert_eq!(
                s.request_outcome(&format!("paired-{index}")).await.unwrap(),
                Some(result.receipt.clone())
            );
            receipts.push(result.receipt);
        }
        let before = audit(h).await;
        let actual = s.snapshot().await.unwrap();
        actual.state.validate().unwrap();
        assert_eq!(
            actual.state,
            expected_native(index),
            "complete native literal at frame {index}"
        );
        assert_eq!(actual.scope.control_revision, index as u64);
        if let Some(old) = old {
            let mut explicit_ownerless = expected_native(index);
            if index >= 6 {
                assert_eq!(actual.state.queue[0].claimed_by.as_deref(), Some(OWNER));
                assert!(old[index].queue[0].claimed_by.is_none());
                assert_ne!(
                    actual.state, old[index],
                    "trusted ownership difference must remain observable"
                );
                explicit_ownerless.queue[0].claimed_by = None;
            }
            assert_eq!(
                old[index], explicit_ownerless,
                "all old fields, including raw revisions and stamps"
            );
            assert_eq!(actual.state.revision, old[index].revision);
            assert_eq!(actual.state.next_queue_id, old[index].next_queue_id);
            for (native, legacy) in actual.state.queue.iter().zip(&old[index].queue) {
                assert_eq!(native.created_revision, legacy.created_revision);
                assert_eq!(native.updated_revision, legacy.updated_revision);
            }
        }
        if index == 6 {
            let foreign =
                DurableService::bind(h, DurableActor::child("synthetic-foreign-child").unwrap());
            let request = DurableMutation {
                request_id: "foreign-finish".into(),
                expected: actual.scope.clone(),
                action: serde_json::from_value(
                    json!({"action":"finish","item":"item-1","status":"completed"}),
                )
                .unwrap(),
                run_id: None,
            };
            assert!(matches!(
                foreign.mutate(request).await,
                Err(DurableError::PermissionDenied)
            ));
            assert!(s.request_outcome("foreign-finish").await.unwrap().is_none());
            assert_eq!(s.snapshot().await.unwrap(), actual);
        }
        assert_eq!(
            audit(h).await,
            before,
            "reads/rejected foreign claim must not append"
        );
    }
    let before = audit(h).await;
    for receipt in &receipts {
        assert_eq!(
            s.request_outcome(&receipt.request_id)
                .await
                .unwrap()
                .as_ref(),
            Some(receipt)
        );
    }
    assert_eq!(before["entries"].as_array().unwrap().len(), 16);
    assert_eq!(audit(h).await, before);
    s.drain().await.unwrap();
    (s.snapshot().await.unwrap().state, json!(receipts))
}
async fn compare_both(old: Option<&[ControlState]>) {
    let h = open(AgentHarnessOptions::default().session).await;
    let (state, receipts) = exercise(&h, old).await;
    let before = audit(&h).await;
    let store = h.session().storage();
    service(&h).drain().await.unwrap();
    h.close().await.unwrap();
    drop(h);
    let reopened = open(Session::new(store, None)).await;
    assert_eq!(service(&reopened).snapshot().await.unwrap().state, state);
    for receipt in receipts.as_array().unwrap() {
        assert_eq!(
            serde_json::to_value(
                service(&reopened)
                    .request_outcome(receipt["requestId"].as_str().unwrap())
                    .await
                    .unwrap()
            )
            .unwrap(),
            *receipt
        );
    }
    assert_eq!(audit(&reopened).await, before);
    reopened.close().await.unwrap();
    drop(reopened);

    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs.clone(),
        "native.jsonl",
        JsonlV4Header::new(
            "synthetic-action-differential".into(),
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
    let h = open(Session::new(Arc::new(store), None)).await;
    let (state, receipts) = exercise(&h, old).await;
    let before = audit(&h).await;
    service(&h).drain().await.unwrap();
    let bytes = std::fs::read(dir.path().join("native.jsonl")).unwrap();
    h.close().await.unwrap();
    drop(h);
    let store = JsonlSessionStorage::load(
        fs,
        "native.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let reopened = open(Session::new(Arc::new(store), None)).await;
    assert_eq!(service(&reopened).snapshot().await.unwrap().state, state);
    for receipt in receipts.as_array().unwrap() {
        assert_eq!(
            serde_json::to_value(
                service(&reopened)
                    .request_outcome(receipt["requestId"].as_str().unwrap())
                    .await
                    .unwrap()
            )
            .unwrap(),
            *receipt
        );
    }
    assert_eq!(audit(&reopened).await, before);
    assert_eq!(
        std::fs::read(dir.path().join("native.jsonl")).unwrap(),
        bytes
    );
    reopened.close().await.unwrap();
    drop(reopened);
    assert_eq!(
        std::fs::read(dir.path().join("native.jsonl")).unwrap(),
        bytes
    );
}
#[tokio::test]
async fn literal_control_actions_use_real_native_service_ownership_and_receipts() {
    tokio::time::timeout(std::time::Duration::from_secs(20), compare_both(None))
        .await
        .unwrap();
}
#[tokio::test]
#[ignore = "Requires freshly executed pinned plugin artifact; use legacy-differential/run.py"]
async fn actual_plugin_control_trace_matches_native_services() {
    let path = std::env::var_os("RPI_SYNTHETIC_LEGACY_CONTROL_EXPORT")
        .expect("explicit synthetic runner artifact");
    let mut file = std::fs::File::open(&path).unwrap();
    use std::io::Read;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_ARTIFACT + 1) as u64)
        .read_to_end(&mut bytes)
        .unwrap();
    let old = validate_artifact(&bytes).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), compare_both(Some(&old)))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "native bridge never rewrites legacy artifact"
    );
}
#[test]
fn test_bridge_rejects_untrusted_artifact_shape_instead_of_faking_a_comparison() {
    assert!(validate_artifact(&vec![b' '; MAX_ARTIFACT + 1]).is_err());
    assert!(validate_artifact(b"{}").is_err());
    // Build a structurally valid *synthetic copy* for rejection probes only.
    // This does not stand in for actual old-plugin execution in the ignored test.
    let frames = literal();
    let mut entries = Vec::new();
    let mut observed = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let mut raw = frame["expected"].clone();
        raw["nextRunId"] = json!(0);
        for item in raw["queue"].as_array_mut().unwrap() {
            item.as_object_mut().unwrap().remove("claimedBy");
        }
        observed.push(
            json!({"index":index,"action":frame.get("action"),"state":raw,
            "entryCount":index,"entryId":(index>0).then(||format!("entry-{index}"))}),
        );
        if index > 0 {
            let mut entry = frame["entry"].clone();
            entry["id"] = json!(format!("entry-{index}"));
            entry["data"]["baseEntryId"] = if index == 1 {
                Value::Null
            } else {
                json!(format!("entry-{}", index - 1))
            };
            if index == 1 {
                entry["data"] = json!({"format":"rpi-durable-journal","version":1,"kind":"snapshot",
                "baseRevision":0,"revision":1,"state":raw});
            } else {
                entry["data"]["changes"] = expected_emitted_changes(index);
            }
            entries.push(entry);
        }
    }
    let mut probe = json!({"schema":"rpi-synthetic-legacy-control-differential-v1","legacyCommit":COMMIT,
        "pluginSha256":PLUGIN_SHA,"traceSha256":TRACE_SHA,"frames":observed,"entries":entries});
    assert!(validate_artifact(&serde_json::to_vec(&probe).unwrap()).is_ok());
    let original = probe.clone();
    for path in [
        "/schema",
        "/legacyCommit",
        "/pluginSha256",
        "/traceSha256",
        "/frames/2/index",
        "/frames/2/entryCount",
        "/frames/2/entryId",
        "/frames/2/action/action",
        "/frames/2/state/revision",
        "/frames/6/state/queue/0/updatedRevision",
        "/entries/1/data/baseEntryId",
        "/entries/4/data/changes/0/op",
    ] {
        probe = original.clone();
        *probe.pointer_mut(path).unwrap() = json!("forged");
        assert!(
            validate_artifact(&serde_json::to_vec(&probe).unwrap()).is_err(),
            "{path}"
        );
    }
    for (parent, key) in [
        ("", "future"),
        ("/frames/0", "future"),
        ("/frames/0/state", "run"),
        ("/frames/5/state/queue/0", "claimedBy"),
        ("/frames/1/state/workflow/steps/0", "future"),
    ] {
        probe = original.clone();
        probe
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), json!({"unknown":true}));
        assert!(
            validate_artifact(&serde_json::to_vec(&probe).unwrap()).is_err(),
            "{parent}/{key}"
        );
    }
    probe = original;
    probe["frames"].as_array_mut().unwrap().pop();
    assert!(validate_artifact(&serde_json::to_vec(&probe).unwrap()).is_err());
}
