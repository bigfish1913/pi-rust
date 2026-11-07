//! Read-only branch extraction versus the pinned, independent legacy decoder.
//! The eleven tests in `legacy::tests` are upstream reproduction, not new coverage.
#[rustfmt::skip]
#[allow(dead_code)]
#[path = "../../rpi-durable/tests/fixtures/legacy-oracle/reference.rs.txt"]
mod legacy;

use rpi_harness::{
    durable::{
        experimental_ledger::source::SourceScope,
        legacy::{
            read_legacy_branch, LegacyBranchScope, LegacyBranchSource, MAX_LEGACY_BRANCH_BYTES,
            MAX_LEGACY_BRANCH_CUSTOM_ENTRIES,
        },
        DurableError,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SessionState, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};
#[path = "durable_legacy/adversarial.rs"]
mod adversarial;
#[path = "durable_legacy/control_inspection.rs"]
mod control_inspection;

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("bounded fixture operation")
}
fn memory() -> Session {
    Session::new(AgentHarnessOptions::default().session.storage(), None)
}
async fn append(s: &Session, id: &str, ty: &str, data: Option<Value>) -> Entry {
    let entry = s
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Custom {
                    custom_type: ty.into(),
                    data,
                },
            },
            "main",
        )
        .await
        .unwrap();
    assert_eq!(entry.base().id, id);
    entry
}
async fn source(s: &Session, leaf: &str) -> LegacyBranchSource {
    read_legacy_branch(s, "main", Some(leaf)).await.unwrap()
}
// Binding is deliberately observed through the public source scope, not private
// Session internals. Capture every entry/record, not just recognized custom data.
async fn audit(s: &Session) -> Value {
    let storage = s.storage();
    json!({
        "scope": read_legacy_branch(s, "main", None).await.unwrap().scope,
        "metadata": storage.metadata().id,
        "revision": storage.history_revision().await.unwrap(),
        "entries": storage.find_entries(&EntryQuery::default()).await.unwrap(),
        "records": storage.find_records(&RecordQuery::default()).await.unwrap(),
    })
}
fn snapshot(revision: u64) -> Value {
    json!({"format":"rpi-durable-journal","version":1,"kind":"snapshot",
        "baseRevision":revision.saturating_sub(1),"revision":revision,
        "state":{"revision":revision,"queue":[]}})
}
fn queue(label: &str) -> Value {
    json!([{"id":format!("queue-{label}"),"text":format!("literal {label}"),
        "status":"queued","createdRevision":2,"updatedRevision":2}])
}
fn workflow(label: &str) -> Value {
    json!({"name":format!("workflow {label}"),"status":"running",
        "steps":[{"id":"verify","title":format!("verify {label}"),"status":"pending"}]})
}
fn delta(base: &Entry, label: &str) -> Value {
    json!({"format":"rpi-durable-journal","version":1,"kind":"delta",
        "baseRevision":1,"revision":2,"baseEntryId":base.base().id,
        "changes":[{"op":"setQueue","queue":queue(label)},
            {"op":"setWorkflow","workflow":workflow(label)}]})
}
fn assert_literal(src: &LegacyBranchSource, label: &str) {
    let decoded = legacy::DurableState::try_from_branch(&src.canonical_entries).unwrap();
    assert_eq!(decoded.revision, 2);
    assert_eq!(
        serde_json::to_value(decoded.queue).unwrap(),
        queue_with_default_note(label)
    );
    assert_eq!(
        serde_json::to_value(decoded.workflow.unwrap()).unwrap(),
        workflow(label)
    );
}
fn queue_with_default_note(label: &str) -> Value {
    // Empty note is omitted by the reference serializer, matching the literal.
    queue(label)
}

#[tokio::test]
async fn explicit_memory_siblings_and_empty_source_are_read_only() {
    bounded(async {
        let s = memory();
        let prefix = append(&s, "prefix", "durable-state", Some(snapshot(1))).await;
        let a = append(&s, "leaf-a", "durable-state", Some(delta(&prefix, "A"))).await;
        s.move_lane("main", Some(&prefix.base().id)).await.unwrap();
        let b = append(&s, "leaf-b", "durable-state", Some(delta(&prefix, "B"))).await;
        let before = audit(&s).await;
        let sa = source(&s, &a.base().id).await;
        let sb = source(&s, &b.base().id).await;
        assert_literal(&sa, "A");
        assert_literal(&sb, "B");
        assert_eq!(
            sa.canonical_entries[0],
            serde_json::to_value(&prefix).unwrap()
        );
        assert_eq!(sa.canonical_entries[1], serde_json::to_value(&a).unwrap());
        assert_eq!(sb.canonical_entries[1], serde_json::to_value(&b).unwrap());
        assert_eq!(sa.scope.current_leaf_id.as_deref(), Some("leaf-b"));
        assert_eq!(sa.scope.selected_leaf_id.as_deref(), Some("leaf-a"));
        assert_eq!(sa.scope.binding_token, sb.scope.binding_token);
        assert_eq!(sa.scope.history_revision, sb.scope.history_revision);
        let empty = read_legacy_branch(&s, "main", None).await.unwrap();
        assert_eq!(empty.inspected_custom_entries, 0);
        assert!(empty.canonical_entries.is_empty());
        assert!(empty.runtime_entries.is_empty());
        assert!(empty.native_control_entries.is_empty());
        assert_eq!(empty.other_custom_entries, 0);
        assert_eq!(empty.scope.selected_leaf_id, None);
        assert_eq!(empty.scope.current_leaf_id, sb.scope.current_leaf_id);
        for (lane, leaf) in [
            ("missing-lane", Some("leaf-a")),
            ("main", Some("missing-leaf")),
            ("main", Some("")),
            ("", Some("leaf-a")),
        ] {
            assert!(read_legacy_branch(&s, lane, leaf).await.is_err());
        }
        assert_eq!(audit(&s).await, before);
    })
    .await;
}

#[tokio::test]
async fn jsonl_reopen_preserves_opaque_classes_without_moving_pointer() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs.clone(),
            "legacy.jsonl",
            JsonlV4Header::new(
                "synthetic-legacy".into(),
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
        // No harness creation, provider, durable service or repair path.
        let s = Session::new(Arc::new(store), None);
        let base = append(&s, "base", "durable-state", Some(snapshot(1))).await;
        let a = append(&s, "branch-a", "durable-state", Some(delta(&base, "A"))).await;
        let runtime_data =
            json!({"opaque":[null,1,"\\n",{"nativeLooking":"rpi.durable.control.v1"}]});
        let runtime = append(
            &s,
            "runtime",
            "rpi-durable-runtime",
            Some(runtime_data.clone()),
        )
        .await;
        let control_data = json!({"not":"a validated native control payload"});
        let control = append(
            &s,
            "control",
            "rpi.durable.control.v1",
            Some(control_data.clone()),
        )
        .await;
        append(
            &s,
            "unknown",
            "durable-state.future",
            Some(json!({"opaque":true})),
        )
        .await;
        s.move_lane("main", Some(&base.base().id)).await.unwrap();
        append(&s, "branch-b", "durable-state", Some(delta(&base, "B"))).await;
        let before = audit(&s).await;
        let old = source(&s, "unknown").await;
        assert_literal(&old, "A");
        assert_eq!(old.inspected_custom_entries, 5);
        assert_eq!(old.other_custom_entries, 1);
        assert_eq!(
            old.runtime_entries,
            vec![serde_json::to_value(&runtime).unwrap()]
        );
        assert_eq!(
            old.native_control_entries,
            vec![serde_json::to_value(&control).unwrap()]
        );
        assert_eq!(old.runtime_entries[0]["data"], runtime_data);
        assert_eq!(old.native_control_entries[0]["data"], control_data);
        assert_eq!(old.canonical_entries[1], serde_json::to_value(&a).unwrap());
        assert_eq!(old.scope.current_leaf_id.as_deref(), Some("branch-b"));
        assert_eq!(audit(&s).await, before);
        let canonical_bytes = serde_json::to_vec(&old.canonical_entries).unwrap();
        s.drain().await;
        let path = dir.path().join("legacy.jsonl");
        let before_bytes = std::fs::read(&path).unwrap();
        assert_eq!(source(&s, "unknown").await, old);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        drop(s); // also releases the only storage owner before load
        let store = JsonlSessionStorage::load(
            fs,
            "legacy.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let reopened = Session::new(Arc::new(store), None);
        let reopen_before = audit(&reopened).await;
        let new = source(&reopened, "unknown").await;
        assert_ne!(new.scope.binding_token, old.scope.binding_token);
        assert_eq!(new.scope.session_id, old.scope.session_id);
        assert_eq!(new.scope.history_revision, old.scope.history_revision);
        assert_eq!(new.scope.current_leaf_id, old.scope.current_leaf_id);
        assert_eq!(new.scope.selected_leaf_id, old.scope.selected_leaf_id);
        assert_eq!(
            serde_json::to_vec(&new.canonical_entries).unwrap(),
            canonical_bytes
        );
        assert_eq!(new.runtime_entries, old.runtime_entries);
        assert_eq!(new.native_control_entries, old.native_control_entries);
        assert_eq!(new.other_custom_entries, old.other_custom_entries);
        assert_literal(&new, "A");
        assert_literal(&source(&reopened, "branch-b").await, "B");
        assert_eq!(audit(&reopened).await, reopen_before);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        reopened.drain().await;
    })
    .await;
}

#[tokio::test]
async fn extraction_retains_invalid_payloads_and_decoder_owns_rejection() {
    bounded(async {
        let s = memory();
        let prefix = append(&s, "prefix", "durable-state", Some(snapshot(1))).await;
        let bad = append(
            &s,
            "bad-suffix",
            "durable-state",
            Some(json!({"broken":"payload"})),
        )
        .await;
        s.move_lane("main", Some(&prefix.base().id)).await.unwrap();
        append(
            &s,
            "good-sibling",
            "durable-state",
            Some(delta(&prefix, "valid")),
        )
        .await;
        let before = audit(&s).await;
        let invalid = source(&s, "bad-suffix").await;
        assert_eq!(invalid.canonical_entries.len(), 2);
        assert_eq!(
            invalid.canonical_entries[1],
            serde_json::to_value(&bad).unwrap()
        );
        assert!(legacy::DurableState::try_from_branch(&invalid.canonical_entries).is_err());
        assert_literal(&source(&s, "good-sibling").await, "valid");
        assert_eq!(audit(&s).await, before);

        s.move_lane("main", Some(&bad.base().id)).await.unwrap();
        append(&s, "self-contained", "durable-state", Some(snapshot(40))).await;
        let before = audit(&s).await;
        let obsolete = source(&s, "self-contained").await;
        assert_eq!(
            obsolete.canonical_entries.len(),
            3,
            "extractor must not prune obsolete payloads"
        );
        assert_eq!(
            obsolete.canonical_entries[1],
            serde_json::to_value(&bad).unwrap()
        );
        assert_eq!(
            legacy::DurableState::try_from_branch(&obsolete.canonical_entries)
                .unwrap()
                .revision,
            40
        );
        assert_eq!(audit(&s).await, before);

        s.move_lane("main", None).await.unwrap();
        // A storage-legal CustomEntry with absent data is a malformed legacy
        // envelope, unlike an obsolete malformed *payload*. No raw file damage.
        let missing = append(&s, "missing-data", "durable-state", None).await;
        append(&s, "later-snapshot", "durable-state", Some(snapshot(50))).await;
        let before = audit(&s).await;
        let missing_source = source(&s, "later-snapshot").await;
        assert_eq!(missing_source.canonical_entries.len(), 2);
        assert_eq!(
            missing_source.canonical_entries[0],
            serde_json::to_value(missing).unwrap()
        );
        assert!(missing_source.canonical_entries[0].get("data").is_none());
        assert!(
            legacy::DurableState::try_from_branch(&missing_source.canonical_entries).is_err(),
            "a later snapshot must not hide a malformed native envelope"
        );
        assert_eq!(audit(&s).await, before);
    })
    .await;
}

#[tokio::test]
async fn every_custom_entry_counts_and_serialized_byte_boundary_is_inclusive() {
    bounded(async {
        assert_eq!(MAX_LEGACY_BRANCH_CUSTOM_ENTRIES, 256);
        assert_eq!(MAX_LEGACY_BRANCH_BYTES, 2 * 1024 * 1024);
        let s = memory();
        for i in 0..256 {
            append(
                &s,
                &format!("unknown-{i}"),
                "unrecognized",
                Some(json!({"i":i})),
            )
            .await;
        }
        let before = audit(&s).await;
        let full = source(&s, "unknown-255").await;
        assert_eq!(full.inspected_custom_entries, 256);
        assert_eq!(full.other_custom_entries, 256);
        assert!(full.canonical_entries.is_empty());
        assert_eq!(audit(&s).await, before);
        append(&s, "overflow", "durable-state", Some(snapshot(1))).await;
        let before = audit(&s).await;
        assert!(matches!(
            read_legacy_branch(&s, "main", Some("overflow")).await,
            Err(DurableError::LimitExceeded)
        ));
        assert_eq!(audit(&s).await, before);
        let detached = source(&s, "unknown-255").await;
        assert_eq!(detached.canonical_entries, full.canonical_entries);
        assert_eq!(
            detached.inspected_custom_entries,
            full.inspected_custom_entries
        );
        assert!(detached.scope.history_revision > full.scope.history_revision);
        assert_eq!(detached.scope.current_leaf_id.as_deref(), Some("overflow"));

        for excess in [0, 1] {
            let s = memory();
            // Same-length synthetic IDs, root parent and one-digit sequence
            // make the measured envelope overhead identical to the final entry.
            let probe = append(&s, "probe", "unrecognized", Some(json!(""))).await;
            let overhead = serde_json::to_vec(&probe).unwrap().len();
            s.move_lane("main", None).await.unwrap();
            let sized = append(
                &s,
                "sized",
                "unrecognized",
                Some(json!(
                    "x".repeat(MAX_LEGACY_BRANCH_BYTES + excess - overhead)
                )),
            )
            .await;
            assert_eq!(
                serde_json::to_vec(&sized).unwrap().len(),
                MAX_LEGACY_BRANCH_BYTES + excess,
                "fixture must measure the serialized Entry, not only its data"
            );
            let before = audit(&s).await;
            let result = read_legacy_branch(&s, "main", Some("sized")).await;
            if excess == 0 {
                let src = result.unwrap();
                assert_eq!(src.inspected_custom_entries, 1);
                assert_eq!(src.other_custom_entries, 1);
                assert!(src.canonical_entries.is_empty());
            } else {
                assert!(matches!(result, Err(DurableError::LimitExceeded)));
            }
            assert_eq!(audit(&s).await, before);
        }
    })
    .await;
}

#[test]
fn source_scope_history_generation_defaults_for_legacy_metadata() {
    let fields = json!({
        "sessionId": "session",
        "lane": "main",
        "bindingToken": "binding",
        "selectedLeafId": null,
        "currentLeafId": null,
        "historyRevision": 7
    });
    let source: SourceScope = serde_json::from_value(fields.clone()).unwrap();
    let legacy: LegacyBranchScope = serde_json::from_value(fields).unwrap();
    assert_eq!(source.history_generation, 0);
    assert_eq!(legacy.history_generation, 0);
    assert_eq!(
        serde_json::to_value(source).unwrap()["historyGeneration"],
        0
    );
    assert_eq!(
        serde_json::to_value(legacy).unwrap()["historyGeneration"],
        0
    );
}

#[tokio::test]
async fn legacy_scope_reports_generation_after_same_revision_replacement() {
    let s = memory();
    s.storage()
        .replace_history_if_revision_and_generation(
            0,
            0,
            HistoryReplacement {
                state: SessionState::new(),
                canonical_mutations: None,
            },
        )
        .await
        .unwrap();
    let source = read_legacy_branch(&s, "main", None).await.unwrap();
    assert_eq!(source.scope.history_revision, 0);
    assert_eq!(source.scope.history_generation, 1);
}
