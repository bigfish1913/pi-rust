//! Actual v2 experimental-ledger writer contracts.
//!
//! These tests deliberately use the public harness/service writer for every
//! positive v2 path.  The older experimental ledger tests are pure codec,
//! source, and synthetic-replay contracts; this file is the live admission and
//! append boundary for the planned `ExperimentalDurablePolicy` mode.
#![allow(dead_code)]

#[path = "experimental_ledger_writer/admission.rs"]
mod admission;
#[path = "experimental_ledger_writer/budget.rs"]
mod budget;
#[path = "experimental_ledger_writer/differential.rs"]
mod differential;
#[path = "experimental_ledger_writer/execution.rs"]
mod execution;
#[path = "experimental_ledger_writer/fixture.rs"]
mod fixture;
#[path = "experimental_ledger_writer/jsonl.rs"]
mod jsonl;
#[path = "experimental_ledger_writer/memory.rs"]
mod memory;
#[path = "support/durable_control_probe.rs"]
mod probe;
#[path = "experimental_ledger_writer/settlement.rs"]
mod settlement;

use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{
        experimental_ledger::{
            self,
            archive::{ArchivedReceipt, ReceiptArchive},
            CUSTOM_TYPE,
        },
        DurableActor, DurableError, DurableMutation, DurableOutcome, DurableReceipt, DurableScope,
        DurableService, ExperimentalDurablePolicy,
    },
    session::{
        memory::{InMemorySessionStorage, SystemClock},
        types::{
            Entry, EntryOrder, EntryQuery, HistoryCheckpoint, HistoryIdentity, HistoryReplacement,
            LanePointer, SessionMetadata, SessionStorage,
        },
        DefaultIdGenerator, Session, SessionState,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::{future::Future, sync::Arc, time::Duration};

pub(crate) const OWNER: &str = "owner";
pub(crate) const CHILD: &str = "child";
pub(crate) const EPOCH: &str = "writer-epoch";

pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("experimental writer operation timed out")
}

pub(crate) fn policy(epoch: &str) -> ExperimentalDurablePolicy {
    ExperimentalDurablePolicy::new(epoch, vec![OWNER.into()]).unwrap()
}

pub(crate) fn memory_session(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}

pub(crate) async fn memory_v1(id: &str) -> AgentHarness {
    open(memory_session(id), None).await
}

pub(crate) async fn memory_v2(id: &str) -> AgentHarness {
    open(memory_session(id), Some(policy(EPOCH))).await
}

pub(crate) fn archived_checkpoint(session_id: &str) -> (HistoryCheckpoint, DurableReceipt) {
    let history = HistoryIdentity {
        revision: 0,
        generation: 0,
    };
    let receipt = DurableReceipt {
        request_id: "archived-request".into(),
        actor: OWNER.into(),
        expected: DurableScope {
            session_id: session_id.into(),
            lane: "main".into(),
            binding_token: "old-binding".into(),
            leaf_id: None,
            history_revision: history.revision,
            history_generation: history.generation,
            entry_id: None,
            control_revision: 0,
        },
        action: serde_json::from_value(start()).unwrap(),
        run_id: None,
        entry_id: "archived-entry".into(),
        control_revision: 1,
        changed: true,
    };
    let entry = ArchivedReceipt::new(receipt.clone(), history, 1).unwrap();
    let archive = ReceiptArchive::from_entries(vec![entry]).unwrap();
    let checkpoint = SessionState::new()
        .history_checkpoint(history)
        .unwrap()
        .with_receipt_archive(archive)
        .unwrap();
    (checkpoint, receipt)
}

pub(crate) async fn open(
    session: Session,
    experimental_durable: Option<ExperimentalDurablePolicy>,
) -> AgentHarness {
    try_open(session, experimental_durable).await.unwrap()
}

pub(crate) async fn try_open(
    session: Session,
    policy: Option<ExperimentalDurablePolicy>,
) -> rpi_harness::result::HarnessResult<AgentHarness> {
    let options = AgentHarnessOptions {
        session,
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    };
    match policy {
        Some(policy) => AgentHarness::create_experimental(options, policy).await,
        None => AgentHarness::create(options).await,
    }
}

pub(crate) fn owner(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner(OWNER).unwrap())
}

pub(crate) fn child(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::child(CHILD).unwrap())
}

pub(crate) async fn request(
    service: &DurableService,
    request_id: &str,
    action: Value,
    run_id: Option<&str>,
) -> DurableMutation {
    DurableMutation {
        request_id: request_id.into(),
        expected: service.snapshot().await.unwrap().scope,
        action: serde_json::from_value(action).unwrap(),
        run_id: run_id.map(str::to_owned),
    }
}

pub(crate) async fn apply(
    service: &DurableService,
    request_id: &str,
    action: Value,
) -> DurableOutcome {
    bounded(service.mutate(request(service, request_id, action, None).await))
        .await
        .unwrap()
}

pub(crate) async fn apply_request(
    service: &DurableService,
    request: DurableMutation,
) -> Result<DurableOutcome, DurableError> {
    bounded(service.mutate(request)).await
}

pub(crate) async fn custom_entries(h: &AgentHarness, custom_type: &str) -> Vec<Entry> {
    h.session()
        .storage()
        .find_entries(&EntryQuery {
            entry_type: Some("custom"),
            custom_type: Some(custom_type.into()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap()
}

pub(crate) async fn audit(
    h: &AgentHarness,
    custom_type: &str,
) -> (Vec<Entry>, u64, Vec<LanePointer>) {
    let storage = h.session().storage();
    (
        custom_entries(h, custom_type).await,
        storage.history_revision().await.unwrap(),
        storage.get_lanes().await.unwrap(),
    )
}

pub(crate) fn assert_canonical_v2(entries: &[Entry]) {
    let mut previous = None;
    for (index, entry) in entries.iter().enumerate() {
        let Entry::Custom(custom) = entry else {
            panic!("experimental writer emitted a non-custom entry")
        };
        assert_eq!(custom.custom_type, CUSTOM_TYPE);
        let data = custom.data.clone().expect("writer record payload");
        let record: experimental_ledger::Record = serde_json::from_value(data).unwrap();
        assert_eq!(record.version, 2);
        assert_eq!(record.receipt.entry_id, custom.base.id);
        assert!(!record.receipt.request_id.is_empty());
        assert_eq!(record.ordinal, index as u64 + 1);
        assert_eq!(record.receipt.entry_id, custom.base.id);
        assert_eq!(
            record.previous.as_ref().map(|p| &p.entry_id),
            previous.as_ref()
        );
        assert!(!record.nodes.is_empty());
        assert_eq!(record.root.count, record.ordinal);
        assert_eq!(record.receipt.expected.entry_id, previous.clone());
        previous = Some(custom.base.id.clone());
    }
}

pub(crate) fn start() -> Value {
    json!({"action":"start", "name":"work", "steps":[{"id":"step", "title":"Verify"}]})
}

pub(crate) fn checkpoint(status: &str) -> Value {
    json!({"action":"checkpoint", "step":"step", "status":status, "note":"checked"})
}
