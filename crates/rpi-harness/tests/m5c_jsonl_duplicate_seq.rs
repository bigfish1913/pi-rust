//! Duplicate-seq / divergent-writer recovery for `JsonlSessionStorage::load`.
//!
//! Two writers on one JSONL (e.g. a session resumed while still open in another
//! process) diverge their `sequence` counters: a mutation lands with an
//! already-consumed seq, and a later writer's seqs jump forward. The loader
//! heals both — dropping the duplicate, accepting the gap, re-chaining an entry
//! that no longer chains to the lane leaf — and republishes the file without the
//! dropped duplicate. Before this, the session could not be opened at all.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_harness::session::jsonl::{encode_mutation, JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::{
    provisioned_into_entry, ProvisionedEntry, ProvisionedKind, SessionMutation, SessionStorage,
};
use rpi_tools::env::{FileContent, FileSystem};

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
}

fn header() -> JsonlV4Header {
    JsonlV4Header::new(
        "sess-1".into(),
        1_700_000_000_000,
        "/cwd".into(),
        None,
        None,
        None,
    )
}

type Fixture = (Arc<dyn FileSystem>, Arc<FakeClock>, Arc<CounterIdGenerator>);

fn fixture() -> Fixture {
    let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
    let fs: Arc<dyn FileSystem> = Arc::new(env);
    (
        fs,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
}

fn message_provisioned(id: &str, text: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_string(),
        kind: ProvisionedKind::Message {
            message: user_msg(text),
            terminate: None,
        },
    }
}

/// A stamped `Entry` mutation on the `main` lane, chained to `parent`.
fn entry_mutation(seq: u64, id: &str, parent: Option<&str>) -> SessionMutation {
    let timestamp = 1_700_000_000_000i64 + seq as i64;
    SessionMutation::Entry {
        seq,
        timestamp,
        lane: Some("main".into()),
        entry: provisioned_into_entry(
            message_provisioned(id, id),
            seq,
            parent.map(str::to_string),
            timestamp,
        ),
    }
}

fn lane_mutation(seq: u64) -> SessionMutation {
    SessionMutation::Lane {
        seq,
        lane: "main".into(),
        leaf_id: None,
    }
}

fn line(mutation: &SessionMutation) -> String {
    encode_mutation(mutation)
}

#[tokio::test]
async fn duplicate_seq_is_dropped_gap_is_accepted_and_file_is_republished() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();

    // A valid prefix: lane, then e1 → e2.
    for mutation in [
        lane_mutation(1),
        entry_mutation(2, "e1", None),
        entry_mutation(3, "e2", Some("e1")),
        // Duplicate of seq 3 with a fresh id: what a second writer appends when
        // its counter lagged. Must be dropped, not applied (and not fatal).
        entry_mutation(3, "e2dup", Some("e1")),
        // Chains to e2, which is only the leaf if the duplicate was dropped.
        entry_mutation(4, "e3", Some("e2")),
        // Forward gap: the other writer's counter was ahead. Accepted.
        entry_mutation(10, "e4", Some("e3")),
    ] {
        fs.append_file(path, FileContent::Text(line(&mutation)), None)
            .await
            .unwrap();
    }

    let storage = JsonlSessionStorage::load(fs.clone(), path, clock.clone(), ids.clone())
        .await
        .expect("healed load");

    assert!(storage.get_entry("e1").await.unwrap().is_some());
    assert!(storage.get_entry("e2").await.unwrap().is_some());
    assert!(storage.get_entry("e3").await.unwrap().is_some());
    assert!(storage.get_entry("e4").await.unwrap().is_some());
    assert!(
        storage.get_entry("e2dup").await.unwrap().is_none(),
        "the duplicated append must be dropped"
    );

    // The healed file no longer carries the duplicate.
    let content = fs.read_text_file(path, None).await.unwrap();
    assert!(
        !content.contains("e2dup"),
        "duplicate should be republished away"
    );

    // Appends resume consistently after the highest surviving seq (10).
    let next = storage
        .append_entry(message_provisioned("e5", "e5"), "main")
        .await
        .unwrap();
    assert_eq!(
        next.seq(),
        11,
        "next append continues from the resynced seq"
    );
}

#[tokio::test]
async fn entry_that_does_not_chain_to_the_leaf_is_re_chained() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();
    for mutation in [
        lane_mutation(1),
        entry_mutation(2, "e1", None),
        // Parent is stale (another writer's leaf), not the current leaf `e1`.
        entry_mutation(3, "e2", None),
    ] {
        fs.append_file(path, FileContent::Text(line(&mutation)), None)
            .await
            .unwrap();
    }

    let storage = JsonlSessionStorage::load(fs.clone(), path, clock.clone(), ids)
        .await
        .expect("healed load");
    let e2 = storage.get_entry("e2").await.unwrap().unwrap();
    assert_eq!(
        e2.parent_id(),
        Some("e1"),
        "an unchained entry is re-parented onto the lane leaf"
    );
}
