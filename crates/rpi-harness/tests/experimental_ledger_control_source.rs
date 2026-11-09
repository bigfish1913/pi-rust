//! Native-source control inspection contracts, not admission, VerifiedJournal,
//! run proof, backend honesty, policy-origin authentication or atomic ABA fencing.
//! Raw reserved Session appends below construct synthetic fixtures ONLY.
#[path = "experimental_ledger_control_source/probe.rs"]
mod probe;
use probe::Probe;
use rpi_durable::{
    control::{ControlAction, ControlState, QueueStatus},
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
            chain::ChainError,
            control::{
                source::{inspect_control_branch, BranchInspectionError, ControlBranchInspection},
                TransitionError, TransitionPolicy,
            },
            source::{SourceError, MAX_CUSTOM_ENTRIES, MAX_PATH_ENTRIES},
            Epoch, Previous, ReadPolicy, Record, Sha256NodeHasher, CUSTOM_TYPE, FORMAT,
        },
        DurableActor, DurableReceipt, DurableScope,
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
use std::{
    collections::HashMap,
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

const OWNER: &str = "synthetic-owner";
const CHILD: &str = "synthetic-child";
const EPOCH: &str = "synthetic-control-source";
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("bounded inspection")
}
async fn fixture<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("bounded fixture")
}
fn memory() -> (Arc<Session>, Arc<Probe>) {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    (Arc::new(Session::new(p.clone(), None)), p)
}
fn read_policy(s: &Session, lane: &str) -> ReadPolicy {
    ReadPolicy::new_epoch(s.storage().metadata().id, lane, EPOCH).unwrap()
}
fn policy(s: &Session, lane: &str) -> TransitionPolicy {
    TransitionPolicy::new(
        read_policy(s, lane),
        vec![
            DurableActor::owner(OWNER).unwrap(),
            DurableActor::child(CHILD).unwrap(),
        ],
    )
    .unwrap()
}
async fn inspect(
    s: &Session,
    leaf: Option<&str>,
) -> Result<ControlBranchInspection, BranchInspectionError> {
    bounded(inspect_control_branch(s, "main", leaf, &policy(s, "main"))).await
}
async fn append(s: &Session, id: &str, kind: ProvisionedKind) -> Entry {
    bounded(s.append_entry(
        ProvisionedEntry {
            id: id.into(),
            kind,
        },
        "main",
    ))
    .await
    .unwrap()
}
async fn custom(s: &Session, id: &str, ty: &str, data: Value) -> Entry {
    append(
        s,
        id,
        ProvisionedKind::Custom {
            custom_type: ty.into(),
            data: Some(data),
        },
    )
    .await
}
async fn native(s: &Session, id: &str) -> Entry {
    append(
        s,
        id,
        ProvisionedKind::ActiveTools {
            active_tool_names: vec![],
        },
    )
    .await
}
async fn publish(s: &Session, r: &Record) {
    custom(
        s,
        &r.receipt.entry_id,
        CUSTOM_TYPE,
        serde_json::to_value(r).unwrap(),
    )
    .await;
}
#[derive(Clone, Default)]
struct Tree {
    nodes: HashMap<(String, u16), Node>,
    root: Option<NodeRef>,
    previous: Option<Previous>,
    state: ControlState,
}
impl Tree {
    fn record(
        &self,
        receipt: DurableReceipt,
        state: ControlState,
        execution_control: ExecutionControl,
    ) -> Record {
        let ordinal = self.previous.as_ref().map_or(1, |p| p.ordinal + 1);
        let mut insert = Insert::new(
            &receipt.request_id,
            codec::receipt_reference(&receipt, ordinal).unwrap(),
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
                        .unwrap();
                    insert.provide(node, &Sha256NodeHasher).unwrap();
                }
                InsertStep::Ready(_) => break,
                other => panic!("fixture insertion: {other:?}"),
            }
        }
        let batch = insert.take_batch().unwrap();
        Record {
            format: FORMAT.into(),
            version: 2,
            epoch: Epoch {
                session_id: receipt.expected.session_id.clone(),
                lane: "main".into(),
                epoch_id: EPOCH.into(),
            },
            ordinal,
            previous: self.previous.clone(),
            state,
            execution_control,
            receipt,
            nodes: batch.nodes,
            root: batch.root,
        }
    }
    async fn make(&self, s: &Session, id: &str, actor: &str, action: ControlAction) -> Record {
        let mut next = self.state.apply(&action).unwrap();
        if let ControlAction::Claim { item } = &action {
            next.queue
                .iter_mut()
                .find(|q| q.id == *item)
                .unwrap()
                .claimed_by = Some(actor.into());
        }
        let changed = next != self.state;
        if changed {
            next.revision += 1;
        }
        let receipt = DurableReceipt {
            request_id: format!("request-{id}"),
            actor: actor.into(),
            expected: DurableScope {
                session_id: s.storage().metadata().id,
                lane: "main".into(),
                binding_token: "synthetic-binding".into(),
                leaf_id: bounded(s.view("main").get_leaf_id()).await.unwrap(),
                history_revision: bounded(s.storage().history_revision()).await.unwrap(),
                history_generation: 0,
                entry_id: self.previous.as_ref().map(|p| p.entry_id.clone()),
                control_revision: self.state.revision,
            },
            action,
            run_id: None,
            entry_id: id.into(),
            control_revision: next.revision,
            changed,
        };
        self.record(receipt, next, ExecutionControl::default())
    }
    async fn add(&mut self, s: &Session, id: &str, actor: &str, action: ControlAction) -> Record {
        let record = self.make(s, id, actor, action).await;
        local(s, &record);
        publish(s, &record).await;
        for node in &record.nodes {
            self.nodes.insert(
                (node.address.entry_id.clone(), node.address.slot),
                node.clone(),
            );
        }
        self.root = Some(record.root.clone());
        self.previous = Some(Previous {
            entry_id: id.into(),
            ordinal: record.ordinal,
        });
        self.state = record.state.clone();
        record
    }
}
fn local(s: &Session, r: &Record) {
    codec::decode(
        &serde_json::to_vec(r).unwrap(),
        &r.receipt.entry_id,
        CUSTOM_TYPE,
        &read_policy(s, "main"),
    )
    .expect("fixture passes local codec, isolating later validation");
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
async fn unchanged(p: &Probe, before: &Audit) {
    assert_eq!(&audit(&p.inner).await, before);
    assert_eq!(
        p.writes.load(Ordering::SeqCst),
        0,
        "inspection must not write or drain"
    );
}
fn source(error: SourceError) -> BranchInspectionError {
    BranchInspectionError::Source(error)
}
fn transition(error: TransitionError) -> BranchInspectionError {
    BranchInspectionError::Transition(error)
}
fn fences(p: &Probe) {
    assert_eq!(
        p.revision_calls.load(Ordering::SeqCst),
        4,
        "outer1, inner2/3, outer4"
    );
    assert_eq!(
        p.lane_calls.load(Ordering::SeqCst),
        4,
        "both final observations must run"
    );
}

#[tokio::test]
async fn memory_explicit_siblings_shared_prefix_noncustom_tip_and_scope_state_root() {
    fixture(async {
        let (s, p) = memory();
        let mut tree = Tree::default();
        tree.add(&s, "genesis", OWNER, ControlAction::Reset).await;
        s.create_lane("side", Some("genesis")).await.unwrap();
        s.create_lane("empty", None).await.unwrap();
        let mut sibling = tree.clone();
        native(&s, "bridge").await;
        custom(
            &s,
            "plugin",
            "plugin.other",
            json!({"payload":"x".repeat(16 * 1024)}),
        )
        .await;
        let queued = tree
            .add(
                &s,
                "queued",
                OWNER,
                ControlAction::Enqueue {
                    text: "Verify".into(),
                },
            )
            .await;
        let item = queued.state.queue[0].id.clone();
        let claimed = tree
            .add(&s, "claimed", CHILD, ControlAction::Claim { item })
            .await;
        s.move_lane("main", Some("genesis")).await.unwrap();
        let other = sibling
            .add(
                &s,
                "sibling",
                OWNER,
                ControlAction::Enqueue {
                    text: "Other".into(),
                },
            )
            .await;
        native(&s, "sibling-tip").await;
        p.reset();
        let before = audit(&p.inner).await;
        let selected = inspect(&s, Some("claimed")).await.unwrap();
        assert_eq!(selected.control().state(), &claimed.state);
        assert_eq!(
            selected.control().state().queue[0].status,
            QueueStatus::Claimed
        );
        assert_eq!(
            selected.control().state().queue[0].claimed_by.as_deref(),
            Some(CHILD)
        );
        assert_eq!(selected.control().ledger().receipts(), 3);
        assert_eq!(selected.control().ledger().root(), Some(&claimed.root));
        let scope = selected.scope();
        assert_eq!(scope.session_id, s.storage().metadata().id);
        assert_eq!(scope.lane, "main");
        let source_scope = bounded(codec::source::read_branch(
            &s,
            "main",
            Some("claimed"),
            &read_policy(&s, "main"),
        ))
        .await
        .unwrap()
        .scope;
        assert_eq!(scope, &source_scope);
        assert_eq!(scope.selected_leaf_id.as_deref(), Some("claimed"));
        assert_eq!(scope.current_leaf_id.as_deref(), Some("sibling-tip"));
        assert_eq!(scope.history_revision, before.2);
        assert_eq!(selected.control().ledger().epoch(), &claimed.epoch);
        let other_result = inspect(&s, Some("sibling-tip")).await.unwrap();
        assert_eq!(other_result.control().state(), &other.state);
        assert_eq!(other_result.control().ledger().root(), Some(&other.root));
        assert_eq!(other_result.control().ledger().receipts(), 2);
        let prefix = inspect(&s, Some("genesis")).await.unwrap();
        assert_eq!(prefix.control().state(), &ControlState::default());
        assert_eq!(prefix.control().ledger().receipts(), 1);
        unchanged(&p, &before).await;
    })
    .await;
}

#[tokio::test]
async fn none_is_empty_not_current_latest_or_other_lane_fallback() {
    fixture(async {
        let (s, p) = memory();
        Tree::default()
            .add(
                &s,
                "tip",
                OWNER,
                ControlAction::Enqueue {
                    text: "Not empty".into(),
                },
            )
            .await;
        s.create_lane("empty", None).await.unwrap();
        p.reset();
        let before = audit(&p.inner).await;
        for lane in ["main", "empty"] {
            let result = bounded(inspect_control_branch(&s, lane, None, &policy(&s, lane)))
                .await
                .unwrap();
            assert_eq!(result.scope().selected_leaf_id, None);
            assert_eq!(result.control().state(), &ControlState::default());
            assert_eq!(result.control().ledger().receipts(), 0);
            assert_eq!(result.control().ledger().root(), None);
            assert_eq!(
                result.scope().current_leaf_id.as_deref(),
                if lane == "main" { Some("tip") } else { None }
            );
        }
        unchanged(&p, &before).await;
    })
    .await;
}

#[tokio::test]
async fn jsonl_reopen_full_audit_and_exact_file_bytes_are_unchanged() {
    fixture(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "session.jsonl",
            JsonlV4Header::new(
                "control-source-jsonl".into(),
                1,
                "/".into(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(storage));
        let s = Session::new(p.clone(), None);
        let mut tree = Tree::default();
        tree.add(&s, "first", OWNER, ControlAction::Reset).await;
        native(&s, "native").await;
        let last = tree
            .add(
                &s,
                "last",
                OWNER,
                ControlAction::Enqueue {
                    text: "persist".into(),
                },
            )
            .await;
        s.create_lane("side", Some("first")).await.unwrap();
        s.drain().await;
        let before = audit(&p.inner).await;
        let bytes = std::fs::read(dir.path().join("session.jsonl")).unwrap();
        p.reset();
        let first = inspect(&s, Some("last")).await.unwrap();
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
        let reopened = inspect(&s, Some("last")).await.unwrap();
        assert_eq!(reopened.control().state(), &last.state);
        assert_eq!(reopened.control().state(), first.control().state());
        assert_eq!(
            reopened.control().ledger().root(),
            first.control().ledger().root()
        );
        assert_eq!(reopened.control().ledger().receipts(), 2);
        assert_eq!(
            reopened.scope().history_revision,
            first.scope().history_revision
        );
        unchanged(&p, &before).await;
        assert_eq!(
            std::fs::read(dir.path().join("session.jsonl")).unwrap(),
            bytes
        );
    })
    .await;
}

#[tokio::test]
async fn initial_identity_policy_missing_lane_and_capture_read_faults_fail_closed() {
    fixture(async {
        let (s, p) = memory();
        native(&s, "tip").await;
        let before = audit(&p.inner).await;
        let good = policy(&s, "main");
        for (lane, leaf) in [
            ("", None),
            ("bad\nlane", None),
            ("main", Some("")),
            ("main", Some("bad\nleaf")),
        ] {
            p.reset();
            assert_eq!(
                bounded(inspect_control_branch(&s, lane, leaf, &good))
                    .await
                    .unwrap_err(),
                source(SourceError::Identity)
            );
            assert_eq!(p.reads.load(Ordering::SeqCst), 0);
        }
        for read in [
            ReadPolicy::new_epoch("wrong-session", "main", EPOCH).unwrap(),
            read_policy(&s, "other"),
        ] {
            let bad =
                TransitionPolicy::new(read, vec![DurableActor::owner(OWNER).unwrap()]).unwrap();
            p.reset();
            assert_eq!(
                bounded(inspect_control_branch(&s, "main", None, &bad))
                    .await
                    .unwrap_err(),
                source(SourceError::Policy)
            );
            assert_eq!(p.reads.load(Ordering::SeqCst), 0);
        }
        p.reset();
        assert_eq!(
            bounded(inspect_control_branch(
                &s,
                "missing",
                None,
                &policy(&s, "missing")
            ))
            .await
            .unwrap_err(),
            source(SourceError::Read)
        );
        assert_eq!(p.revision_calls.load(Ordering::SeqCst), 1);
        assert_eq!(p.lane_calls.load(Ordering::SeqCst), 1);
        for fault in ["revision", "lane"] {
            p.reset();
            if fault == "revision" {
                p.fail_revision_at.store(1, Ordering::SeqCst);
            } else {
                p.fail_lane_at.store(1, Ordering::SeqCst);
            }
            assert_eq!(
                inspect(&s, None).await.unwrap_err(),
                source(SourceError::Read)
            );
            assert_eq!(p.revision_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                p.lane_calls.load(Ordering::SeqCst),
                usize::from(fault == "lane")
            );
            p.fail_revision_at.store(0, Ordering::SeqCst);
            p.fail_lane_at.store(0, Ordering::SeqCst);
        }
        p.reset();
        p.fail_reads.store(true, Ordering::SeqCst);
        assert_eq!(
            inspect(&s, Some("tip")).await.unwrap_err(),
            source(SourceError::Read)
        );
        fences(&p);
        p.fail_reads.store(false, Ordering::SeqCst);
        p.reset();
        assert_eq!(
            inspect(&s, Some("absent")).await.unwrap_err(),
            source(SourceError::Metadata)
        );
        fences(&p);
        unchanged(&p, &before).await;
    })
    .await;
}

#[tokio::test]
async fn mixed_legacy_v1_future_selected_prefixes_reject_but_clean_siblings_pass() {
    fixture(async {
        for ty in [
            "durable-state",
            "rpi-durable-runtime",
            "rpi.durable.control.v1",
            "rpi.durable.future.v99",
        ] {
            let (s, p) = memory();
            custom(&s, "legacy", ty, json!({})).await;
            Tree::default()
                .add(&s, "v2", OWNER, ControlAction::Reset)
                .await;
            p.reset();
            let before = audit(&p.inner).await;
            for leaf in ["legacy", "v2"] {
                assert_eq!(
                    inspect(&s, Some(leaf)).await.unwrap_err(),
                    source(SourceError::Mixed),
                    "{ty}/{leaf}"
                );
            }
            unchanged(&p, &before).await;
            s.move_lane("main", None).await.unwrap();
            Tree::default()
                .add(&s, "clean", OWNER, ControlAction::Reset)
                .await;
            p.reset();
            let before = audit(&p.inner).await;
            assert_eq!(
                inspect(&s, Some("clean"))
                    .await
                    .unwrap()
                    .control()
                    .ledger()
                    .receipts(),
                1
            );
            unchanged(&p, &before).await;
        }
    })
    .await;
}

#[tokio::test]
async fn malformed_codec_source_chain_and_noncanonical_extra_nodes_reject_whole_input() {
    fixture(async {
        for case in ["codec", "source-chain", "noncanonical"] {
            let (s, p) = memory();
            let mut tree = Tree::default();
            tree.add(&s, "first", OWNER, ControlAction::Reset).await;
            native(&s, "bridge").await;
            let mut bad = tree.make(&s, "bad", OWNER, ControlAction::Reset).await;
            if case == "source-chain" {
                bad.receipt.expected.leaf_id = Some("first".into());
                bad = tree.record(bad.receipt, bad.state, bad.execution_control);
            } else if case == "noncanonical" {
                let mut extra = bad.nodes[0].clone();
                extra.address.slot = bad.nodes.len() as u16;
                bad.nodes.push(extra);
            }
            if case == "codec" {
                custom(&s, "bad", CUSTOM_TYPE, json!({"version":2})).await;
            } else {
                local(&s, &bad);
                publish(&s, &bad).await;
            }
            p.reset();
            let before = audit(&p.inner).await;
            for _ in 0..2 {
                p.reset();
                let error = inspect(&s, Some("bad")).await.unwrap_err();
                match case {
                    "codec" => assert!(matches!(
                        error,
                        BranchInspectionError::Source(SourceError::Codec(_))
                    )),
                    "source-chain" => assert_eq!(error, source(SourceError::Chain)),
                    _ => assert_eq!(
                        error,
                        transition(TransitionError::Chain(ChainError::Ledger))
                    ),
                }
                fences(&p);
                unchanged(&p, &before).await;
            }
            assert_eq!(
                inspect(&s, Some("first"))
                    .await
                    .unwrap()
                    .control()
                    .ledger()
                    .receipts(),
                1
            );
            unchanged(&p, &before).await;
        }
    })
    .await;
}

#[tokio::test]
async fn canonical_business_actor_and_execution_errors_repeat_without_prefix_or_poison() {
    fixture(async {
        for case in [
            "business",
            "actor",
            "child-reset",
            "recover",
            "forget",
            "execution",
        ] {
            let (s, p) = memory();
            let mut tree = Tree::default();
            tree.add(&s, "first", OWNER, ControlAction::Reset).await;
            let mut bad = tree.make(&s, "bad", OWNER, ControlAction::Reset).await;
            let expected = match case {
                "business" => {
                    bad.receipt.action = ControlAction::Claim {
                        item: "missing".into(),
                    };
                    TransitionError::Transition
                }
                "actor" => {
                    bad.receipt.actor = "unknown".into();
                    TransitionError::Permission
                }
                "child-reset" => {
                    bad.receipt.actor = CHILD.into();
                    TransitionError::Permission
                }
                "recover" | "forget" => {
                    bad.receipt.action = if case == "recover" {
                        ControlAction::Recover
                    } else {
                        ControlAction::ForgetRun
                    };
                    bad.receipt.run_id = Some("run-1".into());
                    TransitionError::ExecutionFactsRequired
                }
                _ => {
                    bad.execution_control.forgotten_run_ids.push("run-1".into());
                    TransitionError::ExecutionFactsRequired
                }
            };
            let bad = tree.record(bad.receipt, bad.state, bad.execution_control);
            local(&s, &bad);
            publish(&s, &bad).await;
            let encoded = serde_json::to_vec(&bad).unwrap();
            p.reset();
            let before = audit(&p.inner).await;
            for _ in 0..2 {
                p.reset();
                assert_eq!(
                    inspect(&s, Some("bad")).await.unwrap_err(),
                    transition(expected),
                    "{case}"
                );
                fences(&p);
                assert_eq!(serde_json::to_vec(&bad).unwrap(), encoded);
                unchanged(&p, &before).await;
            }
            assert_eq!(
                inspect(&s, Some("first"))
                    .await
                    .unwrap()
                    .control()
                    .ledger()
                    .receipts(),
                1
            );
            unchanged(&p, &before).await;
        }
    })
    .await;
}

// The hook is AFTER actual inner-final revision result #3, not get_entry or
// query start. The inner fence can pass its stale result; only outer #4 sees it.
#[tokio::test]
async fn captured_inner_final_revision_append_navigation_and_same_facade_rebind_conflict() {
    fixture(async {
        for change in [
            "append",
            "navigate",
            "rebind",
            "business-error-plus-append",
            "source-error-plus-append",
        ] {
            let (s, p) = memory();
            let tree = Tree::default();
            let mut r = tree.make(&s, "tip", OWNER, ControlAction::Reset).await;
            if change == "business-error-plus-append" {
                r.receipt.action = ControlAction::Claim {
                    item: "missing".into(),
                };
                r = tree.record(r.receipt, r.state, r.execution_control);
            }
            if change == "source-error-plus-append" {
                custom(&s, "tip", "durable-state", json!({})).await;
            } else {
                local(&s, &r);
                publish(&s, &r).await;
            }
            p.reset();
            p.pause_revision_at.store(3, Ordering::SeqCst);
            let fixed = policy(&s, "main");
            let shared = Arc::clone(&s); // Arc retains the exact same Session facade.
            let mut future = Box::pin(inspect_control_branch(&shared, "main", Some("tip"), &fixed));
            bounded(async {
                tokio::select! {
                    result = &mut future => panic!("completed before hook: {result:?}"),
                    _ = p.captured.notified() => {}
                }
            })
            .await;
            assert_eq!(p.revision_calls.load(Ordering::SeqCst), 3);
            assert_eq!(p.lane_calls.load(Ordering::SeqCst), 3);
            let external = Session::new(p.inner.clone(), None);
            match change {
                "append" | "business-error-plus-append" | "source-error-plus-append" => {
                    native(&external, "external").await;
                }
                "navigate" => external.move_lane("main", None).await.unwrap(),
                "rebind" => s.set_storage(p.clone()),
                _ => unreachable!(),
            }
            let before = audit(&p.inner).await;
            p.release.notify_one();
            assert_eq!(
                bounded(future).await.unwrap_err(),
                source(SourceError::Conflict),
                "{change}"
            );
            fences(&p);
            unchanged(&p, &before).await;
        }
    })
    .await;
}

#[tokio::test]
async fn outer_call_four_read_faults_override_conflict_and_collected_errors_and_read_both_fences() {
    fixture(async {
        for fault in ["revision", "lane"] {
            for kind in ["success", "transition", "source"] {
                let (s, p) = memory();
                let tree = Tree::default();
                let mut r = tree.make(&s, "tip", OWNER, ControlAction::Reset).await;
                if kind == "transition" {
                    r.receipt.action = ControlAction::Claim {
                        item: "missing".into(),
                    };
                    r = tree.record(r.receipt, r.state, r.execution_control);
                }
                if kind == "source" {
                    custom(&s, "tip", "durable-state", json!({})).await;
                } else {
                    local(&s, &r);
                    publish(&s, &r).await;
                }
                p.reset();
                p.pause_revision_at.store(3, Ordering::SeqCst);
                if fault == "revision" {
                    p.fail_revision_at.store(4, Ordering::SeqCst);
                } else {
                    p.fail_lane_at.store(4, Ordering::SeqCst);
                }
                let fixed = policy(&s, "main");
                let mut future = Box::pin(inspect_control_branch(&s, "main", Some("tip"), &fixed));
                bounded(async {
                    tokio::select! {
                        result = &mut future => panic!("completed before hook: {result:?}"),
                        _ = p.captured.notified() => {}
                    }
                })
                .await;
                let external = Session::new(p.inner.clone(), None);
                native(&external, "external").await; // additionally force conflict
                let before = audit(&p.inner).await;
                p.release.notify_one();
                assert_eq!(
                    bounded(future).await.unwrap_err(),
                    source(SourceError::Read),
                    "{fault}/{kind}"
                );
                fences(&p);
                unchanged(&p, &before).await;
            }
        }
    })
    .await;
}

#[tokio::test]
async fn cancellation_at_captured_inner_final_revision_has_no_worker_or_late_effects() {
    fixture(async {
        let (s, p) = memory();
        Tree::default()
            .add(&s, "tip", OWNER, ControlAction::Reset)
            .await;
        p.reset();
        let before = audit(&p.inner).await;
        p.pause_revision_at.store(3, Ordering::SeqCst);
        let fixed = policy(&s, "main");
        let mut future = Box::pin(inspect_control_branch(&s, "main", Some("tip"), &fixed));
        bounded(async {
            tokio::select! {
                result = &mut future => panic!("completed before hook: {result:?}"),
                _ = p.captured.notified() => {}
            }
        })
        .await;
        assert_eq!(p.revision_calls.load(Ordering::SeqCst), 3);
        drop(future); // no spawned task; cancellation drops the root future itself
        let reads = p.reads.load(Ordering::SeqCst);
        p.release.notify_one();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(p.reads.load(Ordering::SeqCst), reads);
        assert_eq!(p.revision_calls.load(Ordering::SeqCst), 3);
        assert_eq!(p.lane_calls.load(Ordering::SeqCst), 3);
        unchanged(&p, &before).await;
    })
    .await;
}

#[tokio::test]
async fn inherited_custom_256_257_and_path_4096_4097_limits_use_synthetic_resolver() {
    fixture(async {
        let (s, p) = memory();
        native(&s, "actual-tip").await;
        for is_custom in [true, false] {
            let limit = if is_custom {
                MAX_CUSTOM_ENTRIES
            } else {
                MAX_PATH_ENTRIES
            };
            for count in [limit, limit + 1] {
                // Resolver-only fixture: no quadratic actual appends, no heap/RSS claim.
                let mut entries = p.entries.lock().unwrap();
                entries.clear();
                for n in 1..=count {
                    let base = EntryBase {
                        entry_type: if is_custom {
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
                    let entry = if is_custom {
                        Entry::Custom(CustomEntry {
                            base,
                            custom_type: "plugin.unknown".into(),
                            data: Some(json!({})),
                        })
                    } else {
                        Entry::ActiveTools(ActiveToolsEntry {
                            base,
                            active_tool_names: vec![],
                        })
                    };
                    entries.insert(format!("mock-{n}"), Some(entry));
                }
                drop(entries);
                p.revision.store(count as u64, Ordering::SeqCst);
                p.reset();
                let before = audit(&p.inner).await;
                let result = inspect(&s, Some(&format!("mock-{count}"))).await;
                if count == limit {
                    let result = result.unwrap();
                    assert_eq!(result.control().state(), &ControlState::default());
                    assert_eq!(result.control().ledger().receipts(), 0);
                    assert_eq!(result.control().ledger().root(), None);
                } else {
                    assert_eq!(result.unwrap_err(), source(SourceError::Limit));
                }
                fences(&p);
                unchanged(&p, &before).await;
            }
        }
    })
    .await;
}
