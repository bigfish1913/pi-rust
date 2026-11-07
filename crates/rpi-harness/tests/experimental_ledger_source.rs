//! Explicit branch-source contracts, not admission/authorization or transition replay.
//! Reserved entries are seeded through raw Session::append_entry only in these tests.
#[path = "experimental_ledger_source/probe.rs"]
mod probe;
use probe::Probe;
use rpi_durable::{
    control::{ControlAction, ControlState},
    execution::ExecutionControl,
    ledger::{
        insert::{Insert, InsertStep},
        Node, NodeRef,
    },
};
use rpi_harness::{
    durable::{
        experimental_ledger::{
            self as codec,
            source::{
                read_branch, SourceCandidate, SourceError, MAX_BRANCH_BYTES, MAX_CUSTOM_ENTRIES,
                MAX_PATH_ENTRIES,
            },
            Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE, FORMAT,
        },
        DurableReceipt, DurableScope,
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
use std::{
    collections::HashMap,
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("bounded source operation")
}
async fn fixture<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("bounded fixture")
}
fn memory() -> (Session, Arc<Probe>) {
    let probe = Probe::new(AgentHarnessOptions::default().session.storage());
    (Session::new(probe.clone(), None), probe)
}
fn policy(s: &Session, lane: &str) -> ReadPolicy {
    ReadPolicy::new_epoch(s.storage().metadata().id, lane, "synthetic-epoch").unwrap()
}
async fn read(s: &Session, lane: &str, leaf: Option<&str>) -> Result<SourceCandidate, SourceError> {
    bounded(read_branch(s, lane, leaf, &policy(s, lane))).await
}
async fn append(s: &Session, lane: &str, id: &str, kind: ProvisionedKind) -> Entry {
    bounded(s.append_entry(
        ProvisionedEntry {
            id: id.into(),
            kind,
        },
        lane,
    ))
    .await
    .unwrap()
}
async fn custom(s: &Session, lane: &str, id: &str, ty: &str, data: Option<Value>) -> Entry {
    append(
        s,
        lane,
        id,
        ProvisionedKind::Custom {
            custom_type: ty.into(),
            data,
        },
    )
    .await
}
async fn tools(s: &Session, lane: &str, id: &str) -> Entry {
    append(
        s,
        lane,
        id,
        ProvisionedKind::ActiveTools {
            active_tool_names: vec![],
        },
    )
    .await
}
#[derive(Clone, Default)]
struct Tree {
    nodes: HashMap<(String, u16), Node>,
    root: Option<NodeRef>,
    previous: Option<Previous>,
}
impl Tree {
    // Cloning Tree is a snapshot of the exact replay prefix for sibling forks.
    fn stage(&mut self, record: &mut Record) {
        let reference = codec::receipt_reference(&record.receipt, record.ordinal).unwrap();
        let mut insert = Insert::new(
            &record.receipt.request_id,
            reference,
            self.root.clone(),
            &Sha256NodeHasher,
        )
        .unwrap();
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    let node = self
                        .nodes
                        .get(&(reference.address.entry_id.clone(), reference.address.slot))
                        .unwrap()
                        .clone();
                    insert.provide(&node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("unexpected pure insert: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        for node in &batch.nodes {
            self.nodes.insert(
                (node.address.entry_id.clone(), node.address.slot),
                node.clone(),
            );
        }
        record.nodes = batch.nodes;
        record.root = batch.root.clone();
        self.root = Some(batch.root);
        self.previous = Some(Previous {
            entry_id: record.receipt.entry_id.clone(),
            ordinal: record.ordinal,
        });
    }
    async fn make(&self, s: &Session, lane: &str, id: &str, request: &str) -> Record {
        let ordinal = self.previous.as_ref().map_or(1, |p| p.ordinal + 1);
        // Construct the root only after the receipt has captured actual storage scope.
        let receipt = DurableReceipt {
            request_id: request.into(),
            actor: "synthetic-owner".into(),
            expected: DurableScope {
                session_id: s.storage().metadata().id,
                lane: lane.into(),
                binding_token: "synthetic-bind".into(),
                leaf_id: bounded(s.view(lane).get_leaf_id()).await.unwrap(),
                history_revision: bounded(s.storage().history_revision()).await.unwrap(),
                history_generation: 0,
                entry_id: self.previous.as_ref().map(|p| p.entry_id.clone()),
                control_revision: 0,
            },
            action: ControlAction::Reset,
            run_id: None,
            entry_id: id.into(),
            control_revision: 0,
            changed: false,
        };
        // Insert once to obtain the non-optional root; caller may restage a mutation from this same prefix.
        let mut tree = self.clone();
        let reference = codec::receipt_reference(&receipt, ordinal).unwrap();
        let mut insert =
            Insert::new(request, reference, tree.root.clone(), &Sha256NodeHasher).unwrap();
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    let node = tree
                        .nodes
                        .get(&(reference.address.entry_id.clone(), reference.address.slot))
                        .unwrap()
                        .clone();
                    insert.provide(&node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("unexpected pure insert: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        tree.root = Some(batch.root.clone());
        Record {
            format: FORMAT.into(),
            version: 2,
            epoch: Epoch {
                session_id: s.storage().metadata().id,
                lane: lane.into(),
                epoch_id: "synthetic-epoch".into(),
            },
            ordinal,
            previous: self.previous.clone(),
            state: ControlState::default(),
            execution_control: ExecutionControl::default(),
            receipt,
            nodes: batch.nodes,
            root: batch.root,
        }
    }
    async fn add(&mut self, s: &Session, lane: &str, id: &str, request: &str) -> Entry {
        let mut record = self.make(s, lane, id, request).await;
        self.stage(&mut record);
        codec::decode(
            &serde_json::to_vec(&record).unwrap(),
            id,
            CUSTOM_TYPE,
            &policy(s, lane),
        )
        .unwrap();
        let entry = custom(
            s,
            lane,
            id,
            CUSTOM_TYPE,
            Some(serde_json::to_value(record).unwrap()),
        )
        .await;
        assert_eq!(
            entry.parent_id(),
            s.storage()
                .get_entry(id)
                .await
                .unwrap()
                .unwrap()
                .parent_id()
        );
        entry
    }
}
type Audit = (Vec<Entry>, Vec<LaneRecord>, u64, Vec<LanePointer>);
async fn audit(storage: &Arc<dyn SessionStorage>) -> Audit {
    bounded(async {
        (
            storage.find_entries(&EntryQuery::default()).await.unwrap(),
            storage.find_records(&RecordQuery::default()).await.unwrap(),
            storage.history_revision().await.unwrap(),
            storage.get_lanes().await.unwrap(),
        )
    })
    .await
}
async fn unchanged(probe: &Probe, before: &Audit) {
    assert_eq!(&audit(&probe.inner).await, before);
    assert_eq!(
        probe.writes.load(Ordering::SeqCst),
        0,
        "source must never mutate or drain storage"
    );
}
fn candidate_ids(c: &SourceCandidate) -> Vec<&str> {
    c.records.iter().map(|r| r.entry.base.id.as_str()).collect()
}

#[tokio::test]
async fn memory_explicit_selection_siblings_shared_genesis_and_read_only_audit() {
    fixture(async {
        let (s, p) = memory();
        let mut root = Tree::default();
        root.add(&s, "main", "genesis", "req-root").await;
        s.create_lane("side", Some("genesis")).await.unwrap();
        s.create_lane("empty", None).await.unwrap();
        let mut sibling = root.clone();
        tools(&s, "main", "native").await;
        custom(&s, "main", "unknown", "plugin.other", Some(json!({"x":1}))).await;
        root.add(&s, "main", "main-record", "req-main").await;
        // Restore both the native parent and the exact pure resolver snapshot.
        s.move_lane("main", Some("genesis")).await.unwrap();
        sibling
            .add(&s, "main", "sibling-record", "req-sibling")
            .await;
        tools(&s, "main", "sibling-tip").await;
        p.reset();
        let before = audit(&p.inner).await;
        let empty = read(&s, "main", None).await.unwrap();
        assert_eq!(
            (
                empty.inspected_entries,
                empty.inspected_custom_entries,
                empty.encoded_bytes
            ),
            (0, 0, 0)
        );
        assert_eq!(empty.scope.selected_leaf_id, None);
        assert_eq!(empty.scope.current_leaf_id.as_deref(), Some("sibling-tip"));
        let main = read(&s, "main", Some("main-record")).await.unwrap();
        assert_eq!(candidate_ids(&main), ["genesis", "main-record"]);
        assert_eq!(
            (
                main.inspected_entries,
                main.inspected_custom_entries,
                main.other_custom_entries
            ),
            (4, 3, 1)
        );
        assert_eq!(
            main.records[1].entry.base.parent_id.as_deref(),
            Some("unknown")
        );
        let selected = read(&s, "main", Some("genesis")).await.unwrap();
        assert_eq!(candidate_ids(&selected), ["genesis"]);
        let sibling = read(&s, "main", Some("sibling-tip")).await.unwrap();
        assert_eq!(candidate_ids(&sibling), ["genesis", "sibling-record"]);
        assert_eq!(sibling.inspected_entries, 3);
        assert!(read(&s, "empty", None).await.unwrap().records.is_empty());
        assert_eq!(
            read(&s, "missing-lane", None).await.unwrap_err(),
            SourceError::Read
        );
        unchanged(&p, &before).await;
    })
    .await
}

#[tokio::test]
async fn jsonl_reopen_preserves_exact_entries_records_and_file_bytes() {
    fixture(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            JsonlV4Header::new("source-jsonl".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(storage));
        let s = Session::new(p.clone(), None);
        let mut tree = Tree::default();
        tree.add(&s, "main", "first", "req-a").await;
        tools(&s, "main", "native").await;
        tree.add(&s, "main", "last", "req-b").await;
        s.drain().await;
        let before = audit(&p.inner).await;
        let bytes = std::fs::read(dir.path().join("session.jsonl")).unwrap();
        p.reset();
        let first = read(&s, "main", Some("last")).await.unwrap();
        unchanged(&p, &before).await;
        drop(s);
        drop(p);
        let storage = JsonlSessionStorage::load(
            fs,
            "session.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(storage));
        let s = Session::new(p.clone(), None);
        assert_eq!(audit(&p.inner).await, before);
        let reopened = read(&s, "main", Some("last")).await.unwrap();
        assert_eq!(candidate_ids(&first), candidate_ids(&reopened));
        assert_eq!(
            (first.inspected_entries, first.encoded_bytes),
            (reopened.inspected_entries, reopened.encoded_bytes)
        );
        for (a, b) in first.records.iter().zip(&reopened.records) {
            assert_eq!(a.entry, b.entry);
            assert_eq!(a.candidate.record(), b.candidate.record());
        }
        unchanged(&p, &before).await;
        assert_eq!(
            std::fs::read(dir.path().join("session.jsonl")).unwrap(),
            bytes
        );
    })
    .await
}

#[tokio::test]
async fn mixed_selected_history_fails_even_below_new_records_but_siblings_do_not() {
    fixture(async {
        for ty in [
            "durable-state",
            "rpi-durable-runtime",
            "rpi.durable.control.v1",
            "rpi.durable.future.v99",
        ] {
            let (s, p) = memory();
            custom(&s, "main", "legacy", ty, Some(json!({}))).await;
            let mut tree = Tree::default();
            tree.add(&s, "main", "new-record", "req-new").await;
            p.reset();
            let before = audit(&p.inner).await;
            assert_eq!(
                read(&s, "main", Some("legacy")).await.unwrap_err(),
                SourceError::Mixed
            );
            assert_eq!(
                read(&s, "main", Some("new-record")).await.unwrap_err(),
                SourceError::Mixed
            );
            unchanged(&p, &before).await;
            s.move_lane("main", None).await.unwrap();
            Tree::default().add(&s, "main", "clean", "req-clean").await;
            p.reset();
            let before = audit(&p.inner).await;
            assert_eq!(
                candidate_ids(&read(&s, "main", Some("clean")).await.unwrap()),
                ["clean"]
            );
            unchanged(&p, &before).await;
        }
    })
    .await
}

#[tokio::test]
async fn malformed_codec_and_chain_scope_never_return_a_successful_prefix() {
    fixture(async {
        for case in [
            "codec",
            "epoch",
            "previous",
            "gap",
            "duplicate",
            "future-history",
            "old-history",
            "control",
            "leaf",
        ] {
            let (s, p) = memory();
            let mut tree = Tree::default();
            tree.add(&s, "main", "first", "req-a").await;
            tools(&s, "main", "bridge").await;
            let mut second = tree.make(&s, "main", "second", "req-b").await;
            match case {
                "epoch" => second.epoch.epoch_id = "different-epoch".into(),
                "previous" => {
                    second.previous.as_mut().unwrap().entry_id = "wrong-previous".into();
                    second.receipt.expected.entry_id = Some("wrong-previous".into());
                }
                "gap" => {}
                // Mutate the serialized ordinal only after pure insertion, so a
                // fixture precondition cannot mask the source's rejection.
                "duplicate" => {
                    // Supply a locally valid alternate predecessor tree. The source, not the
                    // local codec, must reject the duplicate request in actual ancestry.
                    let mut alternate = Tree::default();
                    let mut first = Tree::default()
                        .make(&s, "main", "first", "placeholder")
                        .await;
                    first.receipt.expected.leaf_id = None;
                    first.receipt.expected.history_revision = 0;
                    alternate.stage(&mut first);
                    second = alternate.make(&s, "main", "second", "req-a").await;
                    tree = alternate;
                }
                "future-history" => second.receipt.expected.history_revision = u64::MAX,
                "old-history" => second.receipt.expected.history_revision = 0,
                "control" => {
                    second.receipt.expected.control_revision = 1;
                    second.receipt.control_revision = 1;
                    second.state.revision = 1;
                }
                "leaf" => second.receipt.expected.leaf_id = Some("first".into()),
                _ => {}
            }
            tree.stage(&mut second);
            if case == "gap" {
                second.ordinal = 3;
            }
            let data = if case == "codec" {
                json!({"version":2})
            } else {
                serde_json::to_value(&second).unwrap()
            };
            if matches!(
                case,
                "previous" | "duplicate" | "future-history" | "old-history" | "control" | "leaf"
            ) {
                codec::decode(
                    &serde_json::to_vec(&data).unwrap(),
                    "second",
                    CUSTOM_TYPE,
                    &policy(&s, "main"),
                )
                .expect("isolate source-chain rejection from local codec");
            }
            custom(&s, "main", "second", CUSTOM_TYPE, Some(data)).await;
            p.reset();
            let before = audit(&p.inner).await;
            let error = read(&s, "main", Some("second")).await.unwrap_err();
            if matches!(
                case,
                "previous" | "duplicate" | "future-history" | "old-history" | "control" | "leaf"
            ) {
                assert_eq!(error, SourceError::Chain, "{case}");
            }
            unchanged(&p, &before).await;
        }
    })
    .await
}

#[tokio::test]
async fn policy_identities_empty_selection_and_missing_selected_entry() {
    fixture(async {
        let (s, p) = memory();
        tools(&s, "main", "tip").await;
        p.reset();
        let before = audit(&p.inner).await;
        for bad in [
            ReadPolicy::new_epoch("wrong-session", "main", "synthetic-epoch").unwrap(),
            ReadPolicy::new_epoch(s.storage().metadata().id, "wrong-lane", "synthetic-epoch")
                .unwrap(),
        ] {
            assert_eq!(
                bounded(read_branch(&s, "main", None, &bad))
                    .await
                    .unwrap_err(),
                SourceError::Policy
            );
        }
        for id in ["", "bad\nidentity"] {
            assert_eq!(
                read(&s, "main", Some(id)).await.unwrap_err(),
                SourceError::Identity
            );
            assert_eq!(
                bounded(read_branch(&s, id, None, &policy(&s, "main")))
                    .await
                    .unwrap_err(),
                SourceError::Identity
            );
        }
        assert_eq!(
            read(&s, "main", Some("missing")).await.unwrap_err(),
            SourceError::Metadata
        );
        assert!(read(&s, "main", None).await.unwrap().records.is_empty());
        unchanged(&p, &before).await;
    })
    .await
}

#[tokio::test]
async fn malformed_entry_metadata_and_non_older_parent_paths_fail_closed() {
    fixture(async {
        for case in [
            "id",
            "tag",
            "zero",
            "future",
            "missing",
            "self",
            "cycle",
            "equal",
            "newer",
            "invalid-parent",
        ] {
            let (s, p) = memory();
            let parent = tools(&s, "main", "parent").await;
            let tip = tools(&s, "main", "tip").await;
            let mut bad = tip.clone();
            match case {
                "id" => bad.base_mut().id = "different-id".into(),
                "tag" => bad.base_mut().entry_type = "custom".into(),
                "zero" => bad.base_mut().seq = 0,
                "future" => bad.base_mut().seq = u64::MAX,
                "missing" => bad.base_mut().parent_id = Some("absent".into()),
                "self" => bad.base_mut().parent_id = Some("tip".into()),
                "invalid-parent" => bad.base_mut().parent_id = Some("".into()),
                "cycle" | "equal" | "newer" => {
                    let mut parent = parent.clone();
                    if case == "cycle" {
                        parent.base_mut().parent_id = Some("tip".into());
                    } else {
                        parent.base_mut().seq = tip.seq() + u64::from(case == "newer");
                    }
                    p.entries
                        .lock()
                        .unwrap()
                        .insert("parent".into(), Some(parent));
                }
                _ => unreachable!(),
            }
            p.entries.lock().unwrap().insert("tip".into(), Some(bad));
            p.reset();
            let before = audit(&p.inner).await;
            assert_eq!(
                read(&s, "main", Some("tip")).await.unwrap_err(),
                if case == "invalid-parent" {
                    SourceError::Identity
                } else {
                    SourceError::Metadata
                },
                "{case}"
            );
            unchanged(&p, &before).await;
        }
    })
    .await
}

#[tokio::test]
async fn entry_read_failure_is_not_an_empty_branch() {
    fixture(async {
        let (s, p) = memory();
        tools(&s, "main", "tip").await;
        p.reset();
        let before = audit(&p.inner).await;
        p.fail_reads.store(true, Ordering::SeqCst);
        assert_eq!(
            read(&s, "main", Some("tip")).await.unwrap_err(),
            SourceError::Read
        );
        assert!(read(&s, "main", None).await.unwrap().records.is_empty());
        unchanged(&p, &before).await;
    })
    .await
}

#[tokio::test]
async fn actual_captured_get_entry_is_fenced_against_external_changes_and_final_read_faults() {
    fixture(async {
        for change in [
            "append",
            "navigate",
            "rebind",
            "revision-fault",
            "lane-fault",
            "error-plus-append",
        ] {
            let (s, p) = memory();
            Tree::default().add(&s, "main", "tip", "req-a").await;
            p.reset();
            p.pause.store(true, Ordering::SeqCst);
            let reader_session = s.clone();
            let task =
                tokio::spawn(async move { read(&reader_session, "main", Some("tip")).await });
            bounded(p.captured.notified()).await;
            let external = Session::new(p.inner.clone(), None);
            match change {
                "append" | "error-plus-append" => {
                    tools(&external, "main", "external").await;
                }
                "navigate" => external.move_lane("main", None).await.unwrap(),
                "rebind" => s.set_storage(p.clone()),
                "revision-fault" => p.fail_revision.store(true, Ordering::SeqCst),
                "lane-fault" => p.fail_lanes.store(true, Ordering::SeqCst),
                _ => unreachable!(),
            }
            if change == "error-plus-append" {
                p.fail_reads.store(true, Ordering::SeqCst);
            }
            let before = audit(&p.inner).await;
            p.release.notify_one();
            let error = bounded(task).await.unwrap().unwrap_err();
            assert_eq!(
                error,
                if change.ends_with("fault") {
                    SourceError::Read
                } else {
                    SourceError::Conflict
                },
                "{change}"
            );
            unchanged(&p, &before).await;
        }
    })
    .await
}

#[tokio::test]
async fn cancellation_drops_the_reader_without_late_reads_or_mutations() {
    fixture(async {
        let (s, p) = memory();
        Tree::default().add(&s, "main", "tip", "req-a").await;
        p.reset();
        let before = audit(&p.inner).await;
        p.pause.store(true, Ordering::SeqCst);
        let task = tokio::spawn(async move { read(&s, "main", Some("tip")).await });
        bounded(p.captured.notified()).await;
        task.abort();
        assert!(bounded(task).await.unwrap_err().is_cancelled());
        let reads = p.reads.load(Ordering::SeqCst);
        p.release.notify_one();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(p.reads.load(Ordering::SeqCst), reads);
        unchanged(&p, &before).await;
    })
    .await
}

fn mock_entry(n: usize, custom_entry: bool) -> Entry {
    let base = EntryBase {
        entry_type: if custom_entry {
            "custom"
        } else {
            "active_tools_change"
        }
        .into(),
        id: format!("mock-{n}"),
        seq: n as u64,
        parent_id: (n > 1).then(|| format!("mock-{}", n - 1)),
        timestamp: 0,
    };
    if custom_entry {
        Entry::Custom(CustomEntry {
            base,
            custom_type: "plugin.unknown".into(),
            data: Some(json!("")),
        })
    } else {
        Entry::ActiveTools(ActiveToolsEntry {
            base,
            active_tool_names: vec![],
        })
    }
}
fn mock_path(p: &Probe, count: usize, custom_entry: bool) {
    let mut map = p.entries.lock().unwrap();
    map.clear();
    for n in 1..=count {
        map.insert(format!("mock-{n}"), Some(mock_entry(n, custom_entry)));
    }
    p.revision.store(count as u64, Ordering::SeqCst);
}
#[tokio::test]
async fn custom_entry_budget_is_exact_and_counts_unknown_types() {
    fixture(async {
        let (s, p) = memory();
        tools(&s, "main", "actual-tip").await;
        for count in [MAX_CUSTOM_ENTRIES, MAX_CUSTOM_ENTRIES + 1] {
            mock_path(&p, count, true);
            p.reset();
            let before = audit(&p.inner).await;
            let result = read(&s, "main", Some(&format!("mock-{count}"))).await;
            if count == MAX_CUSTOM_ENTRIES {
                let c = result.unwrap();
                assert_eq!(
                    (
                        c.inspected_entries,
                        c.inspected_custom_entries,
                        c.other_custom_entries
                    ),
                    (count, count, count)
                );
                assert!(c.records.is_empty());
            } else {
                assert_eq!(result.unwrap_err(), SourceError::Limit);
            }
            unchanged(&p, &before).await;
        }
    })
    .await
}
#[tokio::test]
async fn path_entry_budget_is_exact_and_includes_every_non_custom_entry() {
    fixture(async {
        let (s, p) = memory();
        tools(&s, "main", "actual-tip").await;
        for count in [MAX_PATH_ENTRIES, MAX_PATH_ENTRIES + 1] {
            // Mock only the resolver metadata and revision; avoid 4097 real quadratic appends.
            mock_path(&p, count, false);
            p.reset();
            let before = audit(&p.inner).await;
            let result = read(&s, "main", Some(&format!("mock-{count}"))).await;
            if count == MAX_PATH_ENTRIES {
                let c = result.unwrap();
                assert_eq!(c.inspected_entries, count);
                assert_eq!(c.inspected_custom_entries, 0);
                assert!(c.records.is_empty());
            } else {
                assert_eq!(result.unwrap_err(), SourceError::Limit);
            }
            unchanged(&p, &before).await;
        }
    })
    .await
}
#[tokio::test]
async fn aggregate_bytes_exact_eight_mib_and_one_byte_over_include_all_entry_kinds() {
    fixture(async {
        let (s, p) = memory();
        tools(&s, "main", "actual-tip").await;
        let native = mock_entry(1, false);
        let mut large = mock_entry(2, true);
        // Independent serde measurement, not the source's counting implementation.
        let overhead =
            serde_json::to_vec(&native).unwrap().len() + serde_json::to_vec(&large).unwrap().len();
        let padding = MAX_BRANCH_BYTES.checked_sub(overhead).unwrap();
        if let Entry::Custom(c) = &mut large {
            c.data = Some(json!("x".repeat(padding)));
        }
        assert_eq!(
            serde_json::to_vec(&native).unwrap().len() + serde_json::to_vec(&large).unwrap().len(),
            MAX_BRANCH_BYTES
        );
        p.revision.store(2, Ordering::SeqCst);
        p.entries
            .lock()
            .unwrap()
            .insert("mock-1".into(), Some(native));
        p.entries
            .lock()
            .unwrap()
            .insert("mock-2".into(), Some(large.clone()));
        p.reset();
        let before = audit(&p.inner).await;
        let c = read(&s, "main", Some("mock-2")).await.unwrap();
        assert_eq!(
            (c.encoded_bytes, c.inspected_entries, c.other_custom_entries),
            (MAX_BRANCH_BYTES, 2, 1)
        );
        if let Entry::Custom(c) = &mut large {
            c.data = Some(json!("x".repeat(padding + 1)));
        }
        p.entries
            .lock()
            .unwrap()
            .insert("mock-2".into(), Some(large));
        assert_eq!(
            read(&s, "main", Some("mock-2")).await.unwrap_err(),
            SourceError::Limit
        );
        unchanged(&p, &before).await;
    })
    .await
}

#[tokio::test]
async fn source_scope_reports_generation_after_same_revision_replacement() {
    let s = Session::new(AgentHarnessOptions::default().session.storage(), None);
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
    let candidate = read(&s, "main", None).await.unwrap();
    assert_eq!(candidate.scope.history_revision, 0);
    assert_eq!(candidate.scope.history_generation, 1);
}
