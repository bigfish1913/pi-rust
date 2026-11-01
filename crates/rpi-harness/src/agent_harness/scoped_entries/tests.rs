use super::*;

#[tokio::test]
async fn native_commits_share_the_guard_gate_and_recovery_refuses_scoped_mutations() {
    let h = harness().await;
    let initial = guard(&h).await;
    let gate = h.message_writes.lock().await;
    let entry = ProvisionedEntry {
        id: "native".into(),
        kind: ProvisionedKind::Message {
            message: notice(),
            terminate: None,
        },
    };
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(10),
        h.append_provisioned(entry.clone())
    )
    .await
    .is_err());
    assert!(h.session.get_entry("native").await.unwrap().is_none());
    drop(gate);
    h.append_provisioned(entry).await.unwrap();
    assert!(h
        .append_scoped_entry(
            &initial,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .is_err());
    let current = guard(&h).await;
    let (run_id, _, _) = h.acquire_run(OperationKind::Run).await.unwrap();
    assert!(h
        .append_scoped_entry(
            &current,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("recovery in progress"));
    assert!(h.navigate_tree(None, false, None, None).await.is_err());
    h.release_run(&run_id);
    assert!(h
        .append_scoped_entry(
            &current,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn durable_jsonl_outbox_reopens_without_reaccepting_a_delivered_notice() {
    use crate::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use crate::session::memory::SystemClock;
    use crate::session::session::DefaultIdGenerator;
    let temporary = tempfile::tempdir().unwrap();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temporary.path().to_owned(),
        )),
        sessions_root: temporary.path().to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let storage = repo
        .create_typed(&JsonlSessionCreateOptions {
            id: Some("outbox-reopen".into()),
            cwd: temporary.path().to_string_lossy().into_owned(),
            metadata: None,
            parent_session_id: None,
        })
        .await
        .unwrap();
    let metadata = storage.jsonl_metadata();
    let session = Session::new(Arc::new(storage), None);
    let h = AgentHarness::create(AgentHarnessOptions {
        session,
        ..Default::default()
    })
    .await
    .unwrap();
    let source = state(&h).await;
    h.session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "interrupted".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: Some(source),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let accepted = deliver(&h).await;
    assert!(
        h.session.get_entry(&accepted).await.unwrap().is_none(),
        "accepted outbox target must still be queued at crash"
    );
    drop(h);
    for _ in 0..2 {
        let session = Session::new(
            Arc::new(repo.open_by_jsonl_metadata(&metadata).await.unwrap()),
            None,
        );
        let h = AgentHarness::create(AgentHarnessOptions {
            session,
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(deliver(&h).await, accepted);
        let messages = h
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    outbox_key: Some("revision-1".into()),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
    }
}

#[tokio::test]
async fn compaction_reissues_current_notice_instead_of_deduping_against_pruned_context() {
    let h = harness().await;
    state(&h).await;
    let before = deliver(&h).await;
    h.session
        .append_entry(
            ProvisionedEntry {
                id: "compact".into(),
                kind: ProvisionedKind::Compaction {
                    summary: "earlier discussion".into(),
                    retained_tail: vec![],
                    tokens_before: 1,
                    details: None,
                    usage: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    let after = deliver(&h).await;
    assert_ne!(before, after);
    assert_eq!(deliver(&h).await, after);
}

async fn harness() -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions::default())
        .await
        .unwrap()
}
async fn guard(h: &AgentHarness) -> BranchWriteGuard {
    BranchWriteGuard {
        session_id: h.session.storage().metadata().id,
        leaf_id: h.view().get_leaf_id().await.unwrap(),
        custom_type: "goal-state".into(),
        entry_id: h
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    custom_type: Some("goal-state".into()),
                    order: Some(EntryOrder::NewestFirst),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap()
            .first()
            .map(|entry| entry.id().to_owned()),
    }
}
async fn state(h: &AgentHarness) -> String {
    h.append_scoped_entry(
        &guard(h).await,
        ScopedEntry::Custom {
            custom_type: "goal-state".into(),
            data: Some(serde_json::json!({"title":"A"})),
        },
    )
    .await
    .unwrap()
}
fn notice() -> AgentMessage {
    crate::messages::create_custom_message(
        "goal",
        UserContent::Text("Goal A".into()),
        true,
        None,
        1,
    )
}
async fn deliver(h: &AgentHarness) -> String {
    h.append_scoped_entry(
        &guard(h).await,
        ScopedEntry::Notice {
            message: notice(),
            key: "revision-1".into(),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn scoped_writes_reject_stale_session_leaf_and_state() {
    let h = harness().await;
    let initial = guard(&h).await;
    state(&h).await;
    let change = || ScopedEntry::Custom {
        custom_type: "goal-state".into(),
        data: None,
    };
    assert!(h.append_scoped_entry(&initial, change()).await.is_err());
    let mut current = guard(&h).await;
    current.session_id.push_str("-different");
    assert!(h.append_scoped_entry(&current, change()).await.is_err());
    current = guard(&h).await;
    current.entry_id = None;
    assert!(h.append_scoped_entry(&current, change()).await.is_err());
    assert!(h
        .append_scoped_entry(&guard(&h).await, change())
        .await
        .is_ok());
}

#[tokio::test]
async fn outbox_deduplication_is_branch_local_and_rejects_payload_collisions() {
    let h = harness().await;
    let source = state(&h).await;
    let first = deliver(&h).await;
    assert_eq!(deliver(&h).await, first);
    assert!(h
        .append_scoped_entry(
            &guard(&h).await,
            ScopedEntry::Notice {
                message: crate::messages::create_custom_message(
                    "goal",
                    UserContent::Text("Changed".into()),
                    true,
                    None,
                    1
                ),
                key: "revision-1".into(),
            }
        )
        .await
        .is_err());
    h.navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    let other = deliver(&h).await;
    assert_ne!(
        other, first,
        "a notice consumed on another branch cannot suppress this branch"
    );
    h.navigate_tree(Some(&first), false, None, None)
        .await
        .unwrap();
    assert_eq!(deliver(&h).await, first);
}

#[tokio::test]
async fn queued_outbox_survives_reconstruction_and_does_not_drain_on_another_branch() {
    let h = harness().await;
    let source = state(&h).await;
    h.session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "run".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: Some(source.clone()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let accepted = deliver(&h).await;
    assert!(h.session.get_entry(&accepted).await.unwrap().is_none());
    assert_eq!(deliver(&h).await, accepted);
    h.navigate_tree(None, false, None, None).await.unwrap();
    assert!(h.pending_context_entries().await.unwrap().is_empty());
    h.navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    assert_eq!(h.pending_context_entries().await.unwrap()[0].id, accepted);
    let session = h.session.clone();
    drop(h);
    let reopened = AgentHarness::create(AgentHarnessOptions {
        session,
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(deliver(&reopened).await, accepted);
    assert!(reopened
        .session
        .get_entry(&accepted)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn competing_guarded_writers_accept_only_one_mutation() {
    let h = harness().await;
    let initial = guard(&h).await;
    let write = || {
        h.append_scoped_entry(
            &initial,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None,
            },
        )
    };
    let (a, b) = tokio::join!(write(), write());
    assert_ne!(a.is_ok(), b.is_ok());
}
