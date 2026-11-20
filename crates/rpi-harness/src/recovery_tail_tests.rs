//! Recovery tail selection: bounded payloads, pinned ancestry, canonical errors.
use super::*;
use crate::session::types::{provisioned_into_entry, SessionMetadata, SessionMutation};
use crate::session::{memory::InMemorySessionStorage, CounterIdGenerator, FakeClock};
use rpi_ai::providers::faux::{faux_assistant_message, faux_tool_call};

fn memory() -> (Session, Arc<InMemorySessionStorage>) {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "tail-test".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    (Session::new(storage.clone(), None), storage)
}
async fn append(session: &Session, id: &str, kind: ProvisionedKind) {
    session
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind,
            },
            "main",
        )
        .await
        .unwrap();
}
fn user(text: &str) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: UserMessage::new(text, 0).into(),
        terminate: None,
    }
}
fn custom() -> ProvisionedKind {
    ProvisionedKind::Custom {
        custom_type: "extension-state".into(),
        data: None,
    }
}
fn tool_result() -> ProvisionedKind {
    ProvisionedKind::Message {
        message: AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
            role: rpi_ai::types::ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "effect".into(),
            content: vec![Content::text("selected-body")],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        })),
        terminate: None,
    }
}
fn assistant() -> AssistantMessage {
    let mut message = faux_assistant_message("", StopReason::ToolUse);
    let mut call = faux_tool_call("effect", serde_json::json!({}));
    if let Content::ToolCall(call) = &mut call {
        call.id = "call".into();
    }
    message.content = vec![call];
    message
}
fn skip_batch_tail(entry: &Entry) -> bool {
    match entry {
        Entry::Custom(_) => true,
        Entry::Message(message) => matches!(message.message, AgentMessage::ToolResult(_)),
        _ => false,
    }
}
async fn frozen_tail(session: &Session, skip: impl Fn(&Entry) -> bool) -> Option<Entry> {
    session
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                exclude_tool_program_bodies: true,
                order: Some(EntryOrder::NewestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap()
        .into_iter()
        .find(|entry| !skip(entry))
}
fn move_leaf(storage: &InMemorySessionStorage, id: &str) {
    let mut state = storage.snapshot_state();
    let seq = state.next_sequence();
    state
        .apply_mutation(SessionMutation::Lane {
            seq,
            lane: "main".into(),
            leaf_id: Some(id.into()),
        })
        .unwrap();
    storage.replace_state(state);
}

#[tokio::test]
async fn tail_pages_match_full_scan_across_custom_results_and_metadata_barriers() {
    let (session, _) = memory();
    for n in 0..200 {
        append(&session, &format!("old-{n}"), user("old")).await;
    }
    append(
        &session,
        "assistant",
        ProvisionedKind::Message {
            message: AgentMessage::Assistant(Box::new(assistant())),
            terminate: None,
        },
    )
    .await;
    for n in 0..3 {
        append(&session, &format!("result-{n}"), tool_result()).await;
    }
    append(&session, "custom", custom()).await;
    let (actual, hydrated) = AgentHarness::newest_recovery_entry(&session, "main", skip_batch_tail)
        .await
        .unwrap();
    assert_eq!(actual, frozen_tail(&session, skip_batch_tail).await);
    assert_eq!(actual.unwrap().id(), "assistant");
    assert_eq!(hydrated, 16, "do not hydrate the whole older branch");
    assert!(!AgentHarness::tip_is_user_message(&session, "main").await);
    append(
        &session,
        "barrier",
        ProvisionedKind::ThinkingLevel {
            thinking_level: "off".into(),
        },
    )
    .await;
    for n in 0..33 {
        append(&session, &format!("tail-{n}"), custom()).await;
    }
    let (actual, hydrated) = AgentHarness::newest_recovery_entry(&session, "main", skip_batch_tail)
        .await
        .unwrap();
    assert_eq!(actual, frozen_tail(&session, skip_batch_tail).await);
    assert_eq!(actual.unwrap().id(), "barrier");
    assert_eq!(hydrated, 48);
    assert!(!AgentHarness::tip_is_user_message(&session, "main").await);
}

#[tokio::test]
async fn user_tip_crosses_pages_but_does_not_skip_an_assistant_or_context_marker() {
    let (session, _) = memory();
    append(&session, "user", user("prompt")).await;
    for n in 0..33 {
        append(&session, &format!("custom-{n}"), custom()).await;
    }
    assert!(AgentHarness::tip_is_user_message(&session, "main").await);
    session
        .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
            base: RecordBase {
                id: "context".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            queue: QueueKind::Context,
            run_id: None,
            target: serde_json::json!({"id":"user"}),
        }))
        .await
        .unwrap();
    assert!(!AgentHarness::tip_is_user_message(&session, "main").await);
    append(
        &session,
        "answer",
        ProvisionedKind::Message {
            message: AgentMessage::Assistant(Box::new(assistant())),
            terminate: None,
        },
    )
    .await;
    for n in 0..17 {
        append(&session, &format!("later-{n}"), custom()).await;
    }
    assert!(!AgentHarness::tip_is_user_message(&session, "main").await);
}

#[tokio::test]
async fn recovery_tail_pins_the_leaf_across_page_reads() {
    let (session, storage) = memory();
    append(&session, "origin", user("origin")).await;
    append(&session, "other", user("other branch")).await;
    move_leaf(&storage, "origin");
    for n in 0..33 {
        append(&session, &format!("custom-{n}"), custom()).await;
    }
    let switched = std::cell::Cell::new(false);
    let (entry, hydrated) = AgentHarness::newest_recovery_entry(&session, "main", |entry| {
        if !switched.replace(true) {
            move_leaf(&storage, "other");
        }
        matches!(entry, Entry::Custom(_))
    })
    .await
    .unwrap();
    assert_eq!(
        session.get_leaf_id().await.unwrap().as_deref(),
        Some("other")
    );
    assert_eq!(entry.unwrap().id(), "origin");
    assert_eq!(hydrated, 34);
}

#[tokio::test]
async fn invalid_ancestry_is_rejected_before_recovery_selection() {
    let (session, storage) = memory();
    let mut state = storage.snapshot_state();
    let seq = state.next_sequence();
    let entry = provisioned_into_entry(
        ProvisionedEntry {
            id: "broken".into(),
            kind: user("broken ancestry"),
        },
        seq,
        Some("missing".into()),
        0,
    );
    let error = state
        .apply_mutation(SessionMutation::Entry {
            seq,
            timestamp: 0,
            lane: None,
            entry,
        })
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("references missing parent missing"));
    assert_eq!(
        AgentHarness::newest_recovery_entry(&session, "main", |_| false)
            .await
            .unwrap(),
        (None, 0)
    );
}

#[tokio::test]
async fn cold_recovery_rereads_selected_payloads_and_propagates_corruption_and_short_reads() {
    use crate::session::jsonl::{
        encode_header, encode_mutation, JsonlSessionStorage, JsonlV4Header,
    };
    use rpi_tools::OsExecutionEnv;
    let (source, _) = memory();
    for n in 0..100 {
        append(&source, &format!("old-{n}"), user("old")).await;
    }
    append(
        &source,
        "assistant",
        ProvisionedKind::Message {
            message: AgentMessage::Assistant(Box::new(assistant())),
            terminate: None,
        },
    )
    .await;
    append(&source, "result", tool_result()).await;
    let entries = source
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut bytes = encode_header(&JsonlV4Header::new(
        "cold-tail".into(),
        0,
        "/".into(),
        None,
        None,
        None,
    ));
    for entry in entries {
        bytes.push_str(&encode_mutation(&SessionMutation::Entry {
            seq: entry.base().seq,
            timestamp: entry.base().timestamp,
            lane: Some("main".into()),
            entry,
        }));
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    let storage = Arc::new(
        JsonlSessionStorage::load(
            Arc::new(OsExecutionEnv::with_cwd(root.path().into())),
            &path.to_string_lossy(),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap(),
    );
    let session = Session::new(storage.clone(), None);
    let started = vec![ToolStartedRecord {
        base: RecordBase {
            id: "started".into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        },
        run_id: "run".into(),
        assistant_entry_id: "assistant".into(),
        tool_index: 0,
        tool_call_id: "call".into(),
        tool_name: "effect".into(),
        effective_args: serde_json::json!({}),
        result_entry_id: "result".into(),
        replay: ToolReplay::Never,
    }];
    assert!(crate::frame_progress::unknown_tool_outcomes_with_started(
        &session,
        "main",
        Some("assistant"),
        &assistant(),
        &started
    )
    .await
    .unwrap()
    .is_empty());
    assert_eq!(
        AgentHarness::newest_recovery_entry(&session, "main", |_| false)
            .await
            .unwrap()
            .0
            .unwrap()
            .id(),
        "result"
    );
    let corrupt = bytes.replace("selected-body", "corrupt!-body");
    assert_ne!(bytes, corrupt);
    assert_eq!(bytes.len(), corrupt.len());
    std::fs::write(&path, &corrupt).unwrap();
    for _ in 0..2 {
        assert!(crate::frame_progress::unknown_tool_outcomes_with_started(
            &session,
            "main",
            Some("assistant"),
            &assistant(),
            &started
        )
        .await
        .is_err());
        assert!(
            AgentHarness::newest_recovery_entry(&session, "main", skip_batch_tail)
                .await
                .is_err()
        );
    }
    assert!(AgentHarness::repair_interrupted_run(
        &session,
        "main",
        "run",
        &[],
        &CancellationToken::new(),
        false
    )
    .await
    .is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        corrupt,
        "failed selected read must not write a repair"
    );
    std::fs::write(&path, "").unwrap();
    assert!(crate::frame_progress::unknown_tool_outcomes_with_started(
        &session,
        "main",
        Some("assistant"),
        &assistant(),
        &started
    )
    .await
    .is_err());
    storage.release_writer_lease();
}

#[tokio::test]
async fn custom_only_and_empty_tails_terminate_and_unknown_lanes_fail() {
    let (session, _) = memory();
    assert_eq!(
        AgentHarness::newest_recovery_entry(&session, "main", |_| false)
            .await
            .unwrap(),
        (None, 0)
    );
    for n in 0..33 {
        append(&session, &format!("custom-{n}"), custom()).await;
    }
    assert_eq!(
        AgentHarness::newest_recovery_entry(&session, "main", |entry| matches!(
            entry,
            Entry::Custom(_)
        ))
        .await
        .unwrap(),
        (None, 33)
    );
    assert!(
        AgentHarness::newest_recovery_entry(&session, "unknown", |_| false)
            .await
            .is_err()
    );
    assert!(!AgentHarness::tip_is_user_message(&session, "unknown").await);
}

#[tokio::test]
async fn batch_ranges_preserve_anchor_membership_boundaries_and_reused_call_ids() {
    let (session, storage) = memory();
    append(&session, "origin", user("prompt")).await;
    let partial = assistant();
    for id in ["first", "second"] {
        append(
            &session,
            id,
            ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(partial.clone())),
                terminate: None,
            },
        )
        .await;
        if id == "first" {
            append(&session, "old-result", tool_result()).await;
            append(&session, "boundary", user("next batch")).await;
        }
    }
    for n in 0..17 {
        append(&session, &format!("custom-{n}"), custom()).await;
    }
    let outcomes = crate::frame_progress::unknown_tool_outcomes_with_started;
    assert!(outcomes(&session, "main", Some("first"), &partial, &[])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        outcomes(&session, "main", Some("second"), &partial, &[])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        outcomes(&session, "main", None, &partial, &[])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(outcomes(&session, "main", Some("missing"), &partial, &[])
        .await
        .unwrap_err()
        .to_string()
        .contains("not on the current lane branch"));
    move_leaf(&storage, "origin");
    assert!(outcomes(&session, "main", Some("second"), &partial, &[])
        .await
        .unwrap_err()
        .to_string()
        .contains("not on the current lane branch"));
}

#[tokio::test]
async fn very_long_skipped_tails_use_the_legacy_fallback_instead_of_unbounded_page_walks() {
    let (session, _) = memory();
    append(&session, "user", user("prompt")).await;
    for n in 0..130 {
        append(&session, &format!("custom-{n}"), custom()).await;
    }
    let (entry, hydrated) = AgentHarness::newest_recovery_entry(&session, "main", |entry| {
        matches!(entry, Entry::Custom(_))
    })
    .await
    .unwrap();
    assert_eq!(
        entry,
        frozen_tail(&session, |entry| matches!(entry, Entry::Custom(_))).await
    );
    assert_eq!(hydrated, 128 + 131);
    assert!(AgentHarness::tip_is_user_message(&session, "main").await);
}
