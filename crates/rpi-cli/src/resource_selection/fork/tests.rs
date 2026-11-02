use super::*;
use rpi_harness::session::jsonl::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
};
use rpi_harness::session::types::{
    BranchBounds, EntryOrder, EntryQuery, ForkOptions, ForkPosition,
};
use rpi_harness::session::{Session, SessionValues};
use std::sync::Arc;
#[tokio::test]
async fn native_fork_initialization_uses_current_binding_even_for_old_message_cut() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_string_lossy().into_owned();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into())),
        sessions_root: temp.path().join("sessions").to_string_lossy().into_owned(),
        clock: Arc::new(rpi_harness::session::memory::SystemClock),
        ids: Arc::new(rpi_harness::session::session::DefaultIdGenerator::new()),
    });
    let create = |id: &str| JsonlSessionCreateOptions {
        id: Some(id.into()),
        cwd: cwd.clone(),
        parent_session_id: None,
        metadata: None,
    };
    let source = repo.create_typed(&create("source")).await.unwrap();
    let metadata = source.jsonl_metadata();
    let session = Session::new(Arc::new(source), None);
    let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
        session: session.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    let mut selected = ResolvedSelection {
        package: None,
        selection: serde_json::from_value(json!({"version":1,"tools":{"mode":"none"}})).unwrap(),
        skills: vec![],
        extensions: vec![],
        instructions: vec![],
        transition: None,
        validated_skills: None,
    };
    let env: Arc<dyn rpi_tools::ExecutionEnv> =
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into()));
    selected.validate_skills(&env).await.unwrap();
    bind_inventory(&harness, &selected).await.unwrap();
    let message = session
        .append_message(rpi_agent::message::AgentMessage::User(
            rpi_ai::types::UserMessage::new("old message retained", 1),
        ))
        .await
        .unwrap();
    let later = session
        .append_message(rpi_agent::message::AgentMessage::User(
            rpi_ai::types::UserMessage::new("later message", 2),
        ))
        .await
        .unwrap();
    let initial = binding_state(&SessionValues::load(&session).await.unwrap());
    selected.selection.tools = ToolSelection::Only { names: vec![] };
    selected.transition = Some(ResourceTransition {
        version: 1,
        operation_id: "source-operation".into(),
        expected: serde_json::from_value(
            json!({"revision":initial["revision"],"fingerprint":initial["fingerprint"]}),
        )
        .unwrap(),
        target: selected.selection.clone(),
    });
    bind_inventory(&harness, &selected).await.unwrap();
    let source_values = SessionValues::load(&session).await.unwrap();
    let current = binding_state(&source_values);
    assert_eq!(current["revision"], 2);
    assert_eq!(current["operationId"], "source-operation");
    let leaf = session.get_leaf_id().await.unwrap().unwrap();
    let source_bytes = std::fs::read(&metadata.path).unwrap();
    for (id, options) in [
        ("clone", ForkOptions::default()),
        (
            "historical",
            ForkOptions::Branch {
                entry_id: Some(message.clone()),
                position: Some(ForkPosition::At),
            },
        ),
    ] {
        let fork = repo
            .fork_typed_with_value_initializer(&metadata, &create(id), &options, initial_values)
            .await
            .unwrap();
        let fork = Session::new(Arc::new(fork), None);
        let values = SessionValues::load(&fork).await.unwrap();
        let binding = binding_state(&values);
        assert_eq!(binding["revision"], 1);
        assert_eq!(binding["operationId"], Value::Null);
        assert_eq!(binding["fingerprint"], current["fingerprint"]);
        assert!(values.get(SESSION_KEY).unwrap().get("operations").is_none());
        assert_eq!(
            values.get(SESSION_KEY).unwrap()["declaration"],
            source_values.get(SESSION_KEY).unwrap()["declaration"]
        );
        let branch = fork
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap();
        assert!(branch.iter().any(|e| e.id() == message));
        assert_eq!(branch.iter().any(|e| e.id() == later), id == "clone");
        assert_eq!(branch.iter().any(|e| e.id() == leaf), id == "clone");
        // Source operation IDs are not destination authority: the SAME textual
        // ID can start a distinct new-session CAS chain from revision one.
        let mut next = selected.clone();
        next.selection.tools = ToolSelection::Default {};
        next.transition = Some(ResourceTransition {
            version: 1,
            operation_id: "source-operation".into(),
            expected: serde_json::from_value(
                json!({"revision":1,"fingerprint":binding["fingerprint"]}),
            )
            .unwrap(),
            target: next.selection.clone(),
        });
        let fork_harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
            session: fork.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        bind_inventory(&fork_harness, &next).await.unwrap();
        assert_eq!(
            binding_state(&SessionValues::load(&fork).await.unwrap())["revision"],
            2
        );
        assert_eq!(std::fs::read(&metadata.path).unwrap(), source_bytes);
    }
    let before = repo
        .list_typed(&JsonlSessionListOptions::default())
        .await
        .unwrap()
        .len();
    assert!(repo
        .fork_typed_with_value_initializer(
            &metadata,
            &create("bad-explicit"),
            &ForkOptions::Branch {
                entry_id: Some(leaf),
                position: Some(ForkPosition::At)
            },
            |_| panic!("explicit nonmessage validation precedes initializer")
        )
        .await
        .is_err());
    assert!(repo
        .fork_typed_with_value_initializer(
            &metadata,
            &create("bad-initializer"),
            &ForkOptions::default(),
            |_| Err(rpi_harness::error::SessionError::invalid_payload(
                "injected initialization rejection"
            ))
        )
        .await
        .is_err());
    assert_eq!(
        repo.list_typed(&JsonlSessionListOptions::default())
            .await
            .unwrap()
            .len(),
        before
    );
    assert_eq!(std::fs::read(&metadata.path).unwrap(), source_bytes);
}
