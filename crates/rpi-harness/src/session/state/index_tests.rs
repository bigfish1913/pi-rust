//! The same reducer must work with payloads that cannot masquerade as Entries.
use super::*;
use crate::session::types::{
    AssistantFrameOp, AssistantFrameRecord, CustomEntry, EntryBase, MessageEntry,
    MessageSummaryQuery, MessageWindowQuery, RecordBase, TranscriptPageQuery,
};

#[derive(Clone, Debug, PartialEq)]
struct ColdRef(u64); // Deliberately has neither Entry methods nor serde support.

#[test]
fn operation_counts_are_lane_scoped_and_rebuilt_from_both_snapshot_formats() {
    use serde_json::json;

    let mut state = SessionState::new();
    state
        .apply_mutation(SessionMutation::Lane {
            seq: 1,
            lane: "other".into(),
            leaf_id: None,
        })
        .unwrap();
    for (lane, mut record) in [
        (
            "main",
            json!({"type":"operation_started","id":"run","intent":{
                "kind":"run","originalPrompt":[],"initialMessages":[],"resumeData":{}
            }}),
        ),
        (
            "main",
            json!({"type":"retry_pending","id":"retry","runId":"run",
            "attempt":2,"resultEntryId":"result"}),
        ),
        (
            "main",
            json!({"type":"operation_finished","id":"finished","runId":"run","outcome":"completed"}),
        ),
        (
            "other",
            json!({"type":"operation_started","id":"compact","intent":{
                "kind":"compaction","resultEntryId":"compaction-result"
            }}),
        ),
        (
            "other",
            json!({"type":"operation_started","id":"navigate","intent":{
                "kind":"navigation","targetId":null,"summarize":false
            }}),
        ),
        (
            "other",
            json!({"type":"usage","id":"usage","cause":"adjustment",
            "usage":rpi_ai::types::Usage::zero()}),
        ),
    ] {
        record["lane"] = json!(lane);
        record["timestamp"] = json!(42);
        record["seq"] = json!(state.next_sequence());
        state
            .apply_mutation(SessionMutation::Record {
                record: serde_json::from_value(record).unwrap(),
            })
            .unwrap();
    }
    let checkpoint: SessionState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    let startup = state
        .index
        .startup_image(|entry| Some(entry.clone()))
        .unwrap()
        .restore(|entry, _| Some(entry))
        .unwrap();
    for index in [&state.index, &checkpoint.index, &startup] {
        assert_eq!(
            index.operation_counts("main"),
            BTreeMap::from([
                ("operation_finished", 1),
                ("resume", 1),
                ("retry_pending", 1),
                ("run", 1),
            ])
        );
        assert_eq!(
            index.operation_counts("other"),
            BTreeMap::from([("compaction", 1), ("navigation", 1), ("usage", 1),])
        );
        assert!(index.operation_counts("unrecorded").is_empty());
    }
}

fn user(seq: u64, id: &str, parent: Option<&str>) -> SessionMutation {
    SessionMutation::Entry {
        seq,
        timestamp: 42,
        lane: Some("main".into()),
        entry: Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: id.into(),
                seq,
                parent_id: parent.map(str::to_owned),
                timestamp: 42,
            },
            message: rpi_ai::UserMessage::new("private payload 🐈".repeat(100), 43).into(),
            terminate: None,
        }),
    }
}

fn mutations() -> Vec<SessionMutation> {
    vec![
        user(1, "a", None),
        user(2, "b", Some("a")),
        SessionMutation::Record {
            record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id: "frame".into(),
                    seq: 3,
                    lane: "main".into(),
                    timestamp: 42,
                },
                run_id: "run".into(),
                stream_index: 0,
                op: AssistantFrameOp::ClearRun,
                frame: None,
            }),
        },
        SessionMutation::FactName {
            seq: 4,
            name: Some("shared".into()),
        },
        SessionMutation::FactLabel {
            seq: 5,
            target_id: "a".into(),
            label: Some("label".into()),
        },
        SessionMutation::Lane {
            seq: 6,
            lane: "sibling".into(),
            leaf_id: Some("a".into()),
        },
        user(7, "c", Some("b")),
    ]
}

#[test]
fn branch_query_ancestry_survives_checkpoint_and_startup_restore() {
    let mut state = SessionState::new();
    for mutation in mutations() {
        state.apply_mutation(mutation).unwrap();
    }
    state
        .apply_mutation(SessionMutation::Entry {
            seq: 8,
            timestamp: 42,
            lane: Some("main".into()),
            entry: Entry::Custom(CustomEntry {
                base: EntryBase {
                    entry_type: "custom".into(),
                    id: "custom".into(),
                    parent_id: Some("c".into()),
                    seq: 8,
                    timestamp: 42,
                },
                custom_type: "fixture".into(),
                data: None,
            }),
        })
        .unwrap();
    let checkpoint: SessionState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    let startup = state
        .index
        .startup_image(|entry| Some(entry.clone()))
        .unwrap()
        .restore(|entry, _| Some(entry))
        .unwrap();
    for index in [&state.index, &checkpoint.index, &startup] {
        let query = TranscriptPageQuery {
            limit: 1,
            before_id: Some("c".into()),
        };
        assert_eq!(
            index.transcript_indices(Some("custom"), &query).unwrap(),
            (vec![1], 3, true)
        );
        assert!(index.transcript_indices(Some("a"), &query).is_err());
        assert_eq!(
            index.context_usage_indices(Some("custom")).unwrap(),
            (vec![0, 1, 2], 0)
        );
    }
}

#[test]
fn cold_and_eager_replay_have_identical_metadata_and_selections() {
    let mut cold = IndexedState::<ColdRef>::new();
    let mut eager = SessionState::new();
    for mutation in mutations() {
        let eager_outcome = eager.apply_mutation(mutation.clone()).unwrap();
        let cold_outcome = cold
            .apply_mutation(mutation, |entry| ColdRef(entry.seq()))
            .unwrap();
        assert_eq!(
            std::mem::discriminant(&eager_outcome),
            std::mem::discriminant(&cold_outcome)
        );
        assert_eq!(cold.metadata(), eager.index.metadata());
        assert_eq!(cold.get_lanes(), eager.get_lanes());
        assert_eq!(cold.get_stats(), eager.get_stats());
        assert_eq!(cold.sequence(), eager.sequence());
        assert_eq!(cold.get_name(), eager.get_name());
        assert_eq!(cold.get_label("a"), eager.get_label("a"));
        assert_eq!(cold.records(), eager.index.records());
    }
    let query = MessageSummaryQuery::default();
    let selected = cold.select_message_summaries("main", &query).unwrap();
    let page = eager.message_summaries("main", &query).unwrap();
    let ids: Vec<_> = selected
        .indices
        .iter()
        .map(|&i| cold.entry_meta(i).unwrap().id())
        .collect();
    assert_eq!(
        ids,
        page.messages
            .iter()
            .map(|message| message.entry_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(selected.has_more, page.has_more);
    let query = MessageWindowQuery {
        anchor_id: Some("b".into()),
        limit: 2,
        ..Default::default()
    };
    let selected = cold.select_message_window("main", &query).unwrap();
    let page = eager.message_window("main", &query).unwrap();
    assert_eq!(
        selected
            .indices
            .iter()
            .map(|&i| cold.entry_meta(i).unwrap().id())
            .collect::<Vec<_>>(),
        page.entries.iter().map(Entry::id).collect::<Vec<_>>()
    );
    assert_eq!(
        (selected.has_more_before, selected.has_more_after),
        (page.has_more_before, page.has_more_after)
    );
    assert_eq!(
        cold.walk_to_root(Some("c"), &BranchBounds::default())
            .unwrap(),
        vec![2, 1, 0]
    );
    assert_eq!(cold.payload(0), Some(&ColdRef(1)));
    assert_eq!(cold.metadata()[0].message_timestamp, Some(43));
    assert_eq!(
        cold.log_items(&LogOptions::default()).unwrap().len(),
        eager.get_log(&LogOptions::default()).unwrap().len()
    );
}

#[test]
fn rejected_mutations_never_construct_payload_or_change_metadata() {
    let mut cold = IndexedState::<ColdRef>::new();
    cold.apply_mutation(user(1, "a", None), |_| ColdRef(1))
        .unwrap();
    let mut missing_lane = user(2, "b", Some("a"));
    if let SessionMutation::Entry { lane, .. } = &mut missing_lane {
        *lane = Some("missing".into());
    }
    let mut missing_parent = user(2, "b", Some("missing"));
    if let SessionMutation::Entry { lane, .. } = &mut missing_parent {
        *lane = None;
    }
    for mutation in [
        user(9, "b", Some("a")),
        user(2, "a", Some("a")),
        user(2, "b", None),
        missing_lane,
        missing_parent,
        SessionMutation::Lane {
            seq: 2,
            lane: "other".into(),
            leaf_id: Some("missing".into()),
        },
        SessionMutation::FactLabel {
            seq: 2,
            target_id: "missing".into(),
            label: None,
        },
    ] {
        let before = cold.metadata().to_vec();
        assert!(cold
            .apply_mutation(mutation, |_| panic!(
                "invalid mutation invoked payload factory"
            ))
            .is_err());
        assert_eq!(cold.metadata(), before);
        assert_eq!(cold.sequence(), 1);
        assert_eq!(cold.require_lane("main").unwrap().as_deref(), Some("a"));
    }
    let before = cold.metadata().to_vec();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = cold.apply_mutation(user(2, "b", Some("a")), |_| {
            panic!("payload construction failed")
        });
    }));
    assert!(panic.is_err());
    assert_eq!(cold.metadata(), before);
    assert_eq!(cold.sequence(), 1);
}

#[test]
fn metadata_is_not_a_fabricated_entry_and_payload_filters_are_deferred() {
    let mut cold = IndexedState::<ColdRef>::new();
    let entry = Entry::Custom(CustomEntry {
        base: EntryBase {
            entry_type: "custom".into(),
            id: "custom".into(),
            seq: 1,
            parent_id: None,
            timestamp: 42,
        },
        custom_type: "opaque".into(),
        data: Some(serde_json::json!({"secret": "not retained in metadata"})),
    });
    cold.apply_mutation(
        SessionMutation::Entry {
            seq: 1,
            timestamp: 42,
            lane: Some("main".into()),
            entry: entry.clone(),
        },
        |_| ColdRef(100),
    )
    .unwrap();
    let query = EntryQuery {
        tool_call_id: Some("unknown".into()),
        outbox_key: Some("unknown".into()),
        limit: Some(1),
        ..Default::default()
    };
    assert_eq!(cold.select_entry_indices(&query, None).unwrap(), vec![0]);
    assert!(!matches_entry_query(&entry, &query));
    assert_eq!(
        cold.entry_meta(0).unwrap().custom_type.as_deref(),
        Some("opaque")
    );
    // A cold payload has no Entry API; metadata exposes no as_message/base_mut
    // or conversion to Entry. Even diagnostics contain no custom JSON payload.
    assert!(!format!("{:?}", cold.metadata()).contains("not retained"));
    *cold.payload_mut(0).unwrap() = ColdRef(101);
    assert_eq!(cold.entry_meta(0).unwrap().id(), "custom");
}
