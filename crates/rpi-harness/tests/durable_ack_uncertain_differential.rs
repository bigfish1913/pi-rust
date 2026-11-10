//! Post-append ACK uncertainty: fixed synthetic old restore gate vs native receipts.
//! Not an importer, generic ABI equivalence or cross-process exactly-once proof.
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
const OWNER: &str = "synthetic-ack-uncertain-owner";
const MAX_ARTIFACT: usize = 2 * 1024 * 1024;
const CASES: &str =
    include_str!("../../rpi-durable/tests/fixtures/legacy-ack-uncertain/cases.json");
// Retained wire/negative oracle only; never a fallback fresh comparison input.
const CAPTURE: &[u8] =
    include_bytes!("../../rpi-durable/tests/fixtures/legacy-ack-uncertain/captured-probe.json");
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
    kind: String,
    action: Value,
    old_channel: String,
    old_result: Option<Value>,
    old_error: Option<String>,
    old_blocked: bool,
    old_visible: Option<Value>,
    persisted: Value,
    entry_count: usize,
    entry_id: String,
    old_reads: usize,
}
fn cases() -> Vec<Value> {
    let value: Value = serde_json::from_str(CASES).unwrap();
    assert_eq!(value["schema"], "rpi-synthetic-ack-uncertain-cases-v1");
    let frames = value["frames"].as_array().unwrap().clone();
    assert_eq!(frames.len(), 10);
    frames
}
fn closed(value: &Value, fields: &[&str]) -> Result<(), String> {
    let map = value.as_object().ok_or("object required")?;
    if map.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err("unknown field".into());
    }
    Ok(())
}
fn control(value: &Value) -> Result<ControlState, String> {
    closed(
        value,
        &["revision", "nextRunId", "nextQueueId", "workflow", "queue"],
    )?;
    if value["nextRunId"].as_u64() != Some(0) {
        return Err("execution outside matrix".into());
    }
    closed(&value["workflow"], &["name", "status", "steps", "reason"])?;
    for step in value["workflow"]["steps"]
        .as_array()
        .ok_or("steps required")?
    {
        closed(step, &["id", "title", "status", "note"])?;
    }
    for item in value["queue"].as_array().ok_or("queue required")? {
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
        return Err("oversize artifact".into());
    }
    let artifact: Artifact = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if artifact.schema != "rpi-synthetic-legacy-ack-uncertain-export-v1"
        || artifact.legacy_commit != "041c981b21ac42348feee371757f5ef26ecbdcab"
        || artifact.plugin_sha256
            != "0f4be632b1f5dab5dfa0ac9cf55bb9119b1a37ec3ac195f08161dca9480fdaac"
        || artifact.cases_sha256
            != "2db451f05628e51f75b08795c91cc9d715f866064623846f1da9a2f25ef2c791"
        || artifact.frames.len() != 10
        || artifact.entries.len() != 4
    {
        return Err("fixed provenance/count mismatch".into());
    }
    // Exact retained four records; no reducer, anchor rewrite or sorting. The
    // fresh exporter separately uses the actual old core on every prefix.
    let reference: Value = serde_json::from_slice(CAPTURE).unwrap();
    if json!(artifact.entries) != reference["entries"] {
        return Err("wire record mismatch".into());
    }
    let mut states = Vec::new();
    for (index, (frame, case)) in artifact.frames.iter().zip(cases()).enumerate() {
        let blocked = case["oldBlocked"].as_bool().unwrap();
        if frame.index != index
            || frame.label != case["label"].as_str().unwrap()
            || frame.kind != case["kind"].as_str().unwrap()
            || frame.action != case["action"]
            || frame.old_blocked != blocked
            || frame.entry_count != case["oldEntries"].as_u64().unwrap() as usize
            || frame.entry_id != format!("entry-{}", frame.entry_count)
            || frame.old_reads != case["oldReads"].as_u64().unwrap() as usize
            || frame.persisted != case["expected"]
        {
            return Err("frame literal/order/count mismatch".into());
        }
        let expected_channel = match frame.kind.as_str() {
            "reload" => "event-ok",
            "retry" => "details-error",
            "missingAck" | "malformedAck" => "tool-error",
            _ => "done",
        };
        if frame.old_channel != expected_channel {
            return Err("old channel mismatch".into());
        }
        match expected_channel {
            "event-ok" => {
                if frame.old_result.is_some() || frame.old_error.is_some() {
                    return Err("reload shape".into());
                }
            }
            "tool-error" => {
                if frame.old_result.is_some()
                    || !frame.old_error.as_ref().is_some_and(|e| {
                        e.starts_with("durable append acknowledgement is unknown:")
                    })
                {
                    return Err("uncertain tool error shape".into());
                }
            }
            "details-error" => {
                if !frame
                    .old_error
                    .as_ref()
                    .is_some_and(|e| e.contains("durable append acknowledgement is unknown:"))
                    || frame
                        .old_result
                        .as_ref()
                        .and_then(|v| v["details"]["error"].as_str())
                        != frame.old_error.as_deref()
                {
                    return Err("blocked retry shape".into());
                }
            }
            _ => {
                if frame.old_error.is_some()
                    || frame.old_result.is_none()
                    || frame.old_result.as_ref().unwrap()["details"]
                        .get("error")
                        .is_some()
                {
                    return Err("success shape".into());
                }
            }
        }
        // Presentation is opaque; raw errors/result strings are not native ABI
        // equivalence or authority. Labels likewise are forgeable, not auth.
        if blocked {
            if frame.old_visible.is_some() {
                return Err("blocked status must not expose stale state as usable".into());
            }
        } else if frame.old_visible.as_ref() != Some(&frame.persisted) {
            return Err("visible/persisted mismatch".into());
        }
        let state = control(&frame.persisted)?;
        assert_eq!(state, control(&case["expected"])?);
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
    let store = h.session().storage();
    json!({"entries":store.find_entries(&EntryQuery::default()).await.unwrap(),"records":store.find_records(&RecordQuery::default()).await.unwrap()})
}
fn disk(path: Option<&Path>) -> Option<Vec<u8>> {
    path.map(|p| std::fs::read(p).unwrap())
}
async fn exercise(
    h: &AgentHarness,
    p: &probe::Probe,
    old: Option<&[ControlState]>,
    path: Option<&Path>,
) -> (ControlState, Value, Vec<DurableMutation>) {
    let s = service(h);
    let mut receipts = Vec::new();
    let mut requests: Vec<DurableMutation> = Vec::new();
    for (index, case) in cases().iter().enumerate() {
        let before = s.snapshot().await.unwrap();
        let before_audit = audit(h).await;
        let bytes = disk(path);
        let attempts = p.attempts.load(Ordering::SeqCst);
        let kind = case["kind"].as_str().unwrap();
        match kind {
            "commit" | "missingAck" | "malformedAck" => {
                let request = DurableMutation {
                    request_id: format!("ack-{index}"),
                    expected: before.scope.clone(),
                    action: serde_json::from_value::<ControlAction>(case["action"].clone())
                        .unwrap(),
                    run_id: None,
                };
                if kind != "commit" {
                    p.mode.store(2, Ordering::SeqCst);
                }
                let result = s.mutate(request.clone()).await;
                let receipt = if kind == "commit" {
                    let result = result.unwrap();
                    assert!(!result.replayed);
                    result.receipt
                } else {
                    // Same abstract post-commit uncertainty, NOT the old ABI's
                    // missing-entryId vs malformed-JSON distinction.
                    assert!(
                        matches!(result, Err(DurableError::CommitUncertain(_))),
                        "{result:?}"
                    );
                    assert_eq!(p.mode.load(Ordering::SeqCst), 0);
                    s.request_outcome(&request.request_id)
                        .await
                        .unwrap()
                        .expect("committed receipt despite unusable ACK")
                };
                assert!(receipt.changed);
                assert_eq!(receipt.actor, OWNER);
                assert_eq!(receipt.request_id, request.request_id);
                assert_eq!(receipt.action, request.action);
                assert_eq!(receipt.expected, request.expected);
                assert_eq!(receipt.run_id, None);
                assert_eq!(
                    receipt.control_revision,
                    case["expected"]["revision"].as_u64().unwrap()
                );
                assert_eq!(
                    s.request_outcome(&request.request_id).await.unwrap(),
                    Some(receipt.clone())
                );
                assert_eq!(p.attempts.load(Ordering::SeqCst), attempts + 1);
                assert_eq!(p.commits.load(Ordering::SeqCst), receipts.len() + 1);
                assert_ne!(audit(h).await, before_audit);
                if path.is_some() {
                    assert_ne!(disk(path), bytes);
                }
                receipts.push(receipt);
                requests.push(request);
            }
            "retry" => {
                let source = case["retryOf"].as_u64().unwrap() as usize;
                let request = requests
                    .iter()
                    .find(|r| r.request_id == format!("ack-{source}"))
                    .unwrap()
                    .clone();
                assert_eq!(
                    serde_json::to_value(&request.action).unwrap(),
                    case["action"]
                );
                assert_ne!(
                    request.expected, before.scope,
                    "retry retains ORIGINAL expected scope"
                );
                let receipt = s
                    .request_outcome(&request.request_id)
                    .await
                    .unwrap()
                    .unwrap();
                let result = s.mutate(request.clone()).await.unwrap();
                assert!(result.replayed);
                assert_eq!(result.receipt, receipt);
                // Same ID with a different action is not an authorized retry.
                let mut changed = request.clone();
                changed.action =
                    serde_json::from_value(json!({"action":"enqueue","text":"not-identical"}))
                        .unwrap();
                assert!(matches!(
                    s.mutate(changed).await,
                    Err(DurableError::Conflict)
                ));
                assert_eq!(
                    s.request_outcome(&request.request_id).await.unwrap(),
                    Some(receipt)
                );
            }
            "reload" => {
                // Native has no legacy restore-error gate or SessionTree event.
                // Rebinding the same trusted service is read-only, not pretending
                // to perform old reload; real storage reopen is tested below.
                assert_eq!(service(h).snapshot().await.unwrap(), before);
            }
            "read" => {}
            _ => panic!("unexpected fixed case"),
        }
        let actual = s.snapshot().await.unwrap();
        actual.state.validate().unwrap();
        assert_eq!(
            actual.state,
            control(&case["expected"]).unwrap(),
            "native literal frame {index}"
        );
        assert_eq!(actual.scope.control_revision, actual.state.revision);
        if let Some(old) = old {
            assert_eq!(
                actual.state, old[index],
                "whole persisted old/native frame {index}"
            );
        }
        if matches!(kind, "retry" | "reload" | "read") {
            assert_eq!(actual, before);
            assert_eq!(audit(h).await, before_audit);
            assert_eq!(disk(path), bytes);
            assert_eq!(p.attempts.load(Ordering::SeqCst), attempts);
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
    assert_eq!(p.attempts.load(Ordering::SeqCst), 4);
    assert_eq!(p.commits.load(Ordering::SeqCst), 4);
    s.drain().await.unwrap();
    (s.snapshot().await.unwrap().state, json!(receipts), requests)
}
async fn verify_reopened(
    h: &AgentHarness,
    state: &ControlState,
    receipts: &Value,
    requests: &[DurableMutation],
    before: &Value,
    path: Option<&Path>,
) {
    let s = service(h);
    let snapshot = s.snapshot().await.unwrap();
    assert_eq!(&snapshot.state, state);
    let bytes = disk(path);
    for (request, receipt) in requests.iter().zip(receipts.as_array().unwrap()) {
        assert_eq!(
            serde_json::to_value(s.request_outcome(&request.request_id).await.unwrap()).unwrap(),
            *receipt
        );
        // Original expected scopes are never rebased. Existing native policy
        // permits replay only while the receipt entry is still the current
        // control anchor. Older receipts remain queryable, but their requests
        // conflict after later commits; do not claim arbitrary historical retry.
        let result = s.mutate(request.clone()).await;
        if receipt["entryId"].as_str() == snapshot.scope.entry_id.as_deref() {
            let result = result.unwrap();
            assert!(result.replayed);
            assert_eq!(serde_json::to_value(result.receipt).unwrap(), *receipt);
        } else {
            assert!(matches!(result, Err(DurableError::Conflict)));
        }
    }
    assert_eq!(s.snapshot().await.unwrap(), snapshot);
    s.drain().await.unwrap();
    assert_eq!(&audit(h).await, before);
    assert_eq!(disk(path), bytes);
}
async fn compare_both(old: Option<&[ControlState]>) {
    let inner = AgentHarnessOptions::default().session.storage();
    let p = probe::Probe::new(inner.clone());
    let h = open(Session::new(p.clone(), None)).await;
    let (state, receipts, requests) = exercise(&h, &p, old, None).await;
    let before = audit(&h).await;
    h.close().await.unwrap();
    drop(h);
    drop(p);
    let h = open(Session::new(inner, None)).await;
    verify_reopened(&h, &state, &receipts, &requests, &before, None).await;
    h.close().await.unwrap();
    drop(h);
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs.clone(),
        "native.jsonl",
        JsonlV4Header::new(
            "synthetic-ack-uncertain".into(),
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
    let p = probe::Probe::new(Arc::new(store));
    let h = open(Session::new(p.clone(), None)).await;
    let path = dir.path().join("native.jsonl");
    let (state, receipts, requests) = exercise(&h, &p, old, Some(&path)).await;
    let before = audit(&h).await;
    let bytes = std::fs::read(&path).unwrap();
    h.close().await.unwrap();
    drop(h);
    drop(p);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let store = JsonlSessionStorage::load(
        fs,
        "native.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = open(Session::new(Arc::new(store), None)).await;
    verify_reopened(&h, &state, &receipts, &requests, &before, Some(&path)).await;
    h.close().await.unwrap();
    drop(h);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
#[tokio::test]
async fn literal_post_commit_ack_failures_keep_receipts_and_identical_retries_read_only() {
    tokio::time::timeout(std::time::Duration::from_secs(20), compare_both(None))
        .await
        .unwrap();
}
#[tokio::test]
#[ignore = "Requires fresh actual old plugin artifact from legacy-ack-uncertain/run.py without --old-only"]
async fn actual_plugin_ack_uncertainty_matches_native_services() {
    use std::io::Read;
    let path = std::env::var_os("RPI_SYNTHETIC_LEGACY_ACK_UNCERTAIN_EXPORT")
        .expect("explicit runner-owned synthetic artifact required");
    let mut file = std::fs::File::open(&path).unwrap();
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_ARTIFACT + 1) as u64)
        .read_to_end(&mut bytes)
        .unwrap();
    let old = validate(&bytes).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), compare_both(Some(&old)))
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
#[test]
fn retained_ack_probe_bounds_wire_visibility_and_closed_state_without_fresh_fallback() {
    assert_eq!(validate(CAPTURE).unwrap().len(), 10);
    assert!(validate(b"{}").is_err());
    assert!(validate(&vec![b' '; MAX_ARTIFACT + 1]).is_err());
    let original: Value = serde_json::from_slice(CAPTURE).unwrap();
    let mut probes = Vec::new();
    for pointer in [
        "/extra",
        "/frames/0/extra",
        "/casesSha256",
        "/frames/1/oldBlocked",
        "/frames/1/oldChannel",
        "/frames/1/oldError",
        "/frames/1/oldVisible",
        "/frames/3/oldVisible",
        "/frames/3/oldReads",
        "/frames/2/action",
        "/frames/2/entryId",
        "/frames/4/persisted",
        "/entries/1/data/changes",
        "/entries/2/data/baseEntryId",
        "/entries/0/data/revision",
    ] {
        let mut v = original.clone();
        match pointer {
            "/extra" => v["extra"] = json!(true),
            "/frames/0/extra" => v["frames"][0]["extra"] = json!(true),
            _ => *v.pointer_mut(pointer).unwrap() = json!("wrong"),
        };
        probes.push(v);
    }
    let mut v = original.clone();
    v["frames"].as_array_mut().unwrap().pop();
    probes.push(v);
    let mut v = original.clone();
    v["entries"][1]["data"]["changes"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    probes.push(v);
    for probe in probes {
        assert!(validate(&serde_json::to_vec(&probe).unwrap()).is_err());
    }
    let mut state = cases()[1]["expected"].clone();
    state["queue"][0]["claimedBy"] = Value::Null;
    assert!(control(&state).is_err());
    let mut state = cases()[0]["expected"].clone();
    state["run"] = Value::Null;
    assert!(control(&state).is_err());
    let mut state = cases()[0]["expected"].clone();
    state["workflow"]["extra"] = Value::Null;
    assert!(control(&state).is_err());
    let mut state = cases()[0]["expected"].clone();
    state["workflow"]["steps"][0]["extra"] = Value::Null;
    assert!(control(&state).is_err());
}
