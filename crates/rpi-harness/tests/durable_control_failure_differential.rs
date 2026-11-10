//! Synthetic old-runtime comparison, not an importer, store selector or ABI-error equivalence.
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
use std::{
    path::Path,
    sync::{atomic::Ordering, Arc},
};
#[allow(dead_code)]
#[path = "support/durable_control_probe.rs"]
mod probe;

const OWNER: &str = "synthetic-control-failure-owner";
const MAX_ARTIFACT: usize = 2 * 1024 * 1024;
const CASES: &str =
    include_str!("../../rpi-durable/tests/fixtures/legacy-control-failure/cases.json");
// Retained old-only probe, solely a fixed wire-shape/negative-probe oracle.
// The ignored paired test NEVER falls back to these bytes as its input.
const CAPTURE: &[u8] =
    include_bytes!("../../rpi-durable/tests/fixtures/legacy-control-failure/captured-probe.json");
const CASES_SHA: &str = "4eace46c0dd0353b8c91e4e10b60beb5b5afa880829592e82b031c98ab25b6f5";
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Artifact {
    schema: String,
    legacy_commit: String,
    plugin_sha256: String,
    cases_sha256: String,
    frames: Vec<Observed>,
    entries: Vec<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Observed {
    index: usize,
    label: String,
    action: Value,
    kind: String,
    legacy_channel: String,
    legacy_result: Option<Value>,
    legacy_error: Option<String>,
    state: Value,
    entry_count: usize,
    entry_id: Option<String>,
}
fn cases() -> Vec<Value> {
    let value: Value = serde_json::from_str(CASES).unwrap();
    assert_eq!(value["schema"], "rpi-synthetic-control-failure-cases-v1");
    let frames = value["frames"].as_array().unwrap().clone();
    assert_eq!(frames.len(), 20);
    frames
}
fn closed(value: &Value, fields: &[&str]) -> Result<(), String> {
    let map = value.as_object().ok_or("object required")?;
    if map.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err("unknown control field".into());
    }
    Ok(())
}
fn control(value: &Value) -> Result<ControlState, String> {
    closed(
        value,
        &["revision", "nextRunId", "nextQueueId", "workflow", "queue"],
    )?;
    if value["nextRunId"].as_u64() != Some(0) {
        return Err("execution is outside this bridge".into());
    }
    if let Some(workflow) = value.get("workflow") {
        closed(workflow, &["name", "status", "steps", "reason"])?;
        for step in workflow["steps"].as_array().ok_or("steps required")? {
            closed(step, &["id", "title", "status", "note"])?;
        }
    }
    for item in value["queue"].as_array().ok_or("queue required")? {
        // No null owner allowance: this matrix never claims an item.
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
    let state: ControlState = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    state.validate().map_err(|e| e.to_string())?;
    Ok(state)
}
fn validate(bytes: &[u8]) -> Result<Vec<ControlState>, String> {
    if bytes.len() > MAX_ARTIFACT {
        return Err("artifact exceeds bound".into());
    }
    let artifact: Artifact = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if artifact.schema != "rpi-synthetic-legacy-control-failure-export-v1"
        || artifact.legacy_commit != "041c981b21ac42348feee371757f5ef26ecbdcab"
        || artifact.plugin_sha256
            != "0f4be632b1f5dab5dfa0ac9cf55bb9119b1a37ec3ac195f08161dca9480fdaac"
        || artifact.cases_sha256 != CASES_SHA
        || artifact.frames.len() != 20
        || artifact.entries.len() != 7
    {
        return Err("fixed synthetic provenance/count mismatch".into());
    }
    // Exact retained seven wire records, including ordering, all fields, anchors,
    // snapshot origin and changes. This is NOT another journal reducer. The
    // fresh exporter separately replays every prefix with the actual old core.
    let reference: Value = serde_json::from_slice(CAPTURE).unwrap();
    if json!(artifact.entries) != reference["entries"] {
        return Err("old wire records differ from explicit retained probe".into());
    }
    let mut states = Vec::new();
    for (index, (observed, case)) in artifact.frames.iter().zip(cases()).enumerate() {
        if observed.index != index
            || observed.label != case["label"].as_str().unwrap()
            || observed.action != case["action"]
            || observed.kind != case["kind"].as_str().unwrap()
            || observed.entry_count != case["oldEntries"].as_u64().unwrap() as usize
        {
            return Err("action/order/count mismatch".into());
        }
        let expected_id =
            (observed.entry_count != 0).then(|| format!("entry-{}", observed.entry_count));
        if observed.entry_id != expected_id {
            return Err("frame anchor mismatch".into());
        }
        let expected_channel = match observed.kind.as_str() {
            "reject" => "details-error",
            "appendFailure" => "tool-error",
            _ => "done",
        };
        if observed.legacy_channel != expected_channel {
            return Err("legacy error channel mismatch".into());
        }
        match expected_channel {
            "tool-error" => {
                if observed.legacy_result.is_some()
                    || !observed
                        .legacy_error
                        .as_ref()
                        .is_some_and(|v| !v.is_empty())
                {
                    return Err("old transport error shape".into());
                }
            }
            "details-error" => {
                if !observed
                    .legacy_error
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                    || observed
                        .legacy_result
                        .as_ref()
                        .and_then(|v| v["details"]["error"].as_str())
                        != observed.legacy_error.as_deref()
                {
                    return Err("old details error shape".into());
                }
            }
            _ => {
                if observed.legacy_error.is_some()
                    || observed.legacy_result.is_none()
                    || observed.legacy_result.as_ref().unwrap()["details"]
                        .get("error")
                        .is_some()
                {
                    return Err("old success envelope shape".into());
                }
            }
        }
        // Raw old presentation content is opaque, bounded by the whole artifact;
        // it is never executed or treated as authority/compatible native ABI.
        if observed.state != case["expected"] {
            return Err("whole literal state mismatch".into());
        }
        let state = control(&observed.state)?;
        if state != control(&case["expected"])? {
            return Err("typed control mismatch".into());
        }
        states.push(state);
    }
    Ok(states)
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
    let storage = h.session().storage();
    json!({"entries":storage.find_entries(&EntryQuery::default()).await.unwrap(),
        "records":storage.find_records(&RecordQuery::default()).await.unwrap()})
}
fn disk(path: Option<&Path>) -> Option<Vec<u8>> {
    path.map(|p| std::fs::read(p).unwrap())
}
async fn exercise(
    h: &AgentHarness,
    probe: &probe::Probe,
    old: Option<&[ControlState]>,
    path: Option<&Path>,
) -> (ControlState, Value) {
    let s = service(h);
    let mut receipts = Vec::new();
    let mut failed: Option<DurableMutation> = None;
    for (index, case) in cases().iter().enumerate() {
        let before = s.snapshot().await.unwrap();
        let before_audit = audit(h).await;
        let before_disk = disk(path);
        let attempts = probe.attempts.load(Ordering::SeqCst);
        let kind = case["kind"].as_str().unwrap();
        if kind != "read" {
            let request = DurableMutation {
                request_id: if index == 9 || index == 10 {
                    "enqueue-retry".into()
                } else {
                    format!("control-failure-{index}")
                },
                expected: before.scope.clone(),
                action: serde_json::from_value::<ControlAction>(case["action"].clone()).unwrap(),
                run_id: None,
            };
            if index == 10 {
                let failed = failed.as_ref().unwrap();
                assert_eq!(request.request_id, failed.request_id);
                assert_eq!(request.action, failed.action);
                assert_eq!(request.expected, failed.expected);
                assert_eq!(request.run_id, failed.run_id);
            }
            if kind == "appendFailure" {
                probe.mode.store(1, Ordering::SeqCst);
                failed = Some(request.clone());
            }
            let answer = s.mutate(request.clone()).await;
            match kind {
                "changed" | "noop" => {
                    let result = answer.unwrap();
                    assert!(!result.replayed);
                    assert_eq!(result.receipt.changed, kind == "changed");
                    assert_eq!(result.receipt.request_id, request.request_id);
                    assert_eq!(result.receipt.actor, OWNER);
                    assert_eq!(result.receipt.expected, request.expected);
                    assert_eq!(result.receipt.action, request.action);
                    assert_eq!(result.receipt.run_id, None);
                    assert_eq!(
                        result.receipt.control_revision,
                        case["expected"]["revision"].as_u64().unwrap()
                    );
                    assert_eq!(
                        s.request_outcome(&request.request_id).await.unwrap(),
                        Some(result.receipt.clone())
                    );
                    receipts.push(result.receipt);
                    assert_eq!(probe.attempts.load(Ordering::SeqCst), attempts + 1);
                    assert_ne!(audit(h).await, before_audit);
                    if path.is_some() {
                        assert_ne!(disk(path), before_disk);
                    }
                }
                "reject" => {
                    assert!(
                        matches!(answer, Err(DurableError::Control(_))),
                        "case {index}: {answer:?}"
                    );
                    assert!(s
                        .request_outcome(&request.request_id)
                        .await
                        .unwrap()
                        .is_none());
                    assert_eq!(probe.attempts.load(Ordering::SeqCst), attempts);
                }
                "appendFailure" => {
                    // Probe knows this particular failure happened before append.
                    // The public service correctly retains uncertain semantics.
                    assert!(
                        matches!(answer, Err(DurableError::CommitUncertain(_))),
                        "{answer:?}"
                    );
                    assert_eq!(probe.mode.load(Ordering::SeqCst), 0);
                    assert_eq!(probe.attempts.load(Ordering::SeqCst), attempts + 1);
                    assert!(s
                        .request_outcome(&request.request_id)
                        .await
                        .unwrap()
                        .is_none());
                }
                _ => panic!("unknown case"),
            }
        }
        let actual = s.snapshot().await.unwrap();
        actual.state.validate().unwrap();
        assert_eq!(
            actual.state,
            control(&case["expected"]).unwrap(),
            "native whole literal case {index}"
        );
        assert_eq!(actual.scope.control_revision, actual.state.revision);
        if let Some(old) = old {
            assert_eq!(
                actual.state, old[index],
                "actual old/native whole state {index}"
            );
        }
        if matches!(kind, "reject" | "appendFailure" | "read") {
            assert_eq!(actual, before);
            assert_eq!(audit(h).await, before_audit);
            assert_eq!(disk(path), before_disk);
        }
        if kind == "noop" {
            assert_eq!(actual.state, before.state);
            assert_eq!(actual.scope.control_revision, before.scope.control_revision);
            assert_ne!(
                actual.scope, before.scope,
                "native no-op still durably records its receipt"
            );
        }
        assert_eq!(
            receipts.len(),
            case["nativeReceipts"].as_u64().unwrap() as usize
        );
        assert_eq!(
            audit(h).await["entries"].as_array().unwrap().len(),
            receipts.len()
        );
    }
    assert_eq!(receipts.len(), 10);
    assert_eq!(probe.commits.load(Ordering::SeqCst), 10);
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 11);
    let before = audit(h).await;
    let bytes = disk(path);
    for receipt in &receipts {
        assert_eq!(
            s.request_outcome(&receipt.request_id)
                .await
                .unwrap()
                .as_ref(),
            Some(receipt)
        );
    }
    s.drain().await.unwrap();
    assert_eq!(audit(h).await, before);
    assert_eq!(disk(path), bytes);
    (s.snapshot().await.unwrap().state, json!(receipts))
}
async fn verify_reopened(h: &AgentHarness, state: &ControlState, receipts: &Value, before: &Value) {
    assert_eq!(&service(h).snapshot().await.unwrap().state, state);
    for receipt in receipts.as_array().unwrap() {
        assert_eq!(
            serde_json::to_value(
                service(h)
                    .request_outcome(receipt["requestId"].as_str().unwrap())
                    .await
                    .unwrap()
            )
            .unwrap(),
            *receipt
        );
    }
    assert_eq!(&audit(h).await, before);
}
async fn compare_both(old: Option<&[ControlState]>) {
    let inner = AgentHarnessOptions::default().session.storage();
    let probe = probe::Probe::new(inner.clone());
    let h = open(Session::new(probe.clone(), None)).await;
    let (state, receipts) = exercise(&h, &probe, old, None).await;
    let before = audit(&h).await;
    h.close().await.unwrap();
    drop(h);
    let reopened = open(Session::new(inner, None)).await;
    verify_reopened(&reopened, &state, &receipts, &before).await;
    reopened.close().await.unwrap();
    drop(reopened);

    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs.clone(),
        "native.jsonl",
        JsonlV4Header::new(
            "synthetic-control-failure".into(),
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
    let probe = probe::Probe::new(Arc::new(store));
    let h = open(Session::new(probe.clone(), None)).await;
    let path = dir.path().join("native.jsonl");
    let (state, receipts) = exercise(&h, &probe, old, Some(&path)).await;
    let before = audit(&h).await;
    let bytes = std::fs::read(&path).unwrap();
    h.close().await.unwrap();
    drop(h);
    drop(probe);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let store = JsonlSessionStorage::load(
        fs,
        "native.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let reopened = open(Session::new(Arc::new(store), None)).await;
    verify_reopened(&reopened, &state, &receipts, &before).await;
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    reopened.close().await.unwrap();
    drop(reopened);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[tokio::test]
async fn literal_failures_and_noops_use_real_service_receipts_and_explicit_retry() {
    tokio::time::timeout(std::time::Duration::from_secs(20), compare_both(None))
        .await
        .unwrap();
}
#[tokio::test]
#[ignore = "Requires fresh old-plugin execution; use legacy-control-failure/run.py without --old-only"]
async fn actual_plugin_control_failures_match_native_services() {
    use std::io::Read;
    let path = std::env::var_os("RPI_SYNTHETIC_LEGACY_CONTROL_FAILURE_EXPORT")
        .expect("explicit runner-owned synthetic artifact required");
    let mut file = std::fs::File::open(&path).unwrap();
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_ARTIFACT + 1) as u64)
        .read_to_end(&mut bytes)
        .unwrap();
    let states = validate(&bytes).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        compare_both(Some(&states)),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "never rewrite old artifact"
    );
}
#[test]
fn captured_probe_is_only_a_strict_wire_and_negative_artifact_oracle() {
    assert_eq!(validate(CAPTURE).unwrap().len(), 20);
    assert!(validate(b"{}").is_err());
    assert!(validate(&vec![b' '; MAX_ARTIFACT + 1]).is_err());
    let original: Value = serde_json::from_slice(CAPTURE).unwrap();
    let mut probes = Vec::new();
    let mut v = original.clone();
    v["frames"][0]["unexpected"] = json!(true);
    probes.push(v);
    let mut v = original.clone();
    v["unexpected"] = json!(true);
    probes.push(v);
    let mut v = original.clone();
    v["casesSha256"] = json!("forged");
    probes.push(v);
    let mut v = original.clone();
    v["frames"].as_array_mut().unwrap().pop();
    probes.push(v);
    let mut v = original.clone();
    v["frames"][9]["legacyChannel"] = json!("done");
    probes.push(v);
    let mut v = original.clone();
    v["frames"][4]["legacyError"] = Value::Null;
    probes.push(v);
    let mut v = original.clone();
    v["frames"][0]["entryCount"] = json!(1);
    probes.push(v);
    let mut v = original.clone();
    v["frames"][10]["action"]["text"] = json!("different");
    probes.push(v);
    let mut v = original.clone();
    v["frames"][10]["state"]["queue"][0]["claimedBy"] = Value::Null;
    probes.push(v);
    let mut v = original.clone();
    v["frames"][10]["state"]["queue"][0]["updatedRevision"] = json!(5);
    probes.push(v);
    let mut v = original.clone();
    v["entries"][0]["data"]["revision"] = json!(0);
    probes.push(v);
    let mut v = original.clone();
    v["entries"][3]["data"]["changes"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    probes.push(v);
    let mut v = original.clone();
    v["entries"][6]["data"]["baseEntryId"] = Value::Null;
    probes.push(v);
    for probe in probes {
        assert!(validate(&serde_json::to_vec(&probe).unwrap()).is_err());
    }
    let mut null_owner = cases()[10]["expected"].clone();
    null_owner["queue"][0]["claimedBy"] = Value::Null;
    assert!(control(&null_owner).is_err());
    let mut execution = cases()[0]["expected"].clone();
    execution["run"] = Value::Null;
    assert!(control(&execution).is_err());
    let mut workflow = cases()[1]["expected"].clone();
    workflow["workflow"]["future"] = Value::Null;
    assert!(control(&workflow).is_err());
    let mut step = cases()[1]["expected"].clone();
    step["workflow"]["steps"][0]["future"] = json!(true);
    assert!(control(&step).is_err());
    let mut state = cases()[0]["expected"].clone();
    state["future"] = Value::Null;
    assert!(control(&state).is_err());
}
