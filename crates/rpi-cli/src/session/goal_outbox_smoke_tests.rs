use super::*;

struct RestoreSmokeConfig(Option<std::ffi::OsString>);
impl Drop for RestoreSmokeConfig {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}

/// A real separately-built plugin crosses the SDK ABI and runtime action bridge.
/// The worker is paused immediately, so this remains provider/network-free while
/// checking the current durable goal notice and session semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "set RPI_GOAL_SMOKE_DLL to the matching rpi-goal cdylib"]
async fn native_goal_outbox_smoke() {
    use rpi_harness::agent_harness::AgentLane;
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};
    use serde_json::json;
    let dll = std::env::var_os("RPI_GOAL_SMOKE_DLL").expect("RPI_GOAL_SMOKE_DLL is required");
    let temporary = tempfile::tempdir().unwrap();
    let _config = RestoreSmokeConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let agent = temporary.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let args = Args {
        extension: vec![dll.into()],
        dev_local_only: true,
        no_builtin_tools: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test".into()),
        session_dir: Some(temporary.path().join("sessions")),
        ..Default::default()
    };
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let (harness, _events, context) = build(&resolved, &args, temporary.path(), false)
        .await
        .unwrap();
    let tool = harness
        .get_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.tool.schema().name == "goal")
        .expect("goal DLL must load")
        .tool;
    let execute = |params| {
        tool.execute(
            "smoke",
            params,
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
    };
    let started = execute(json!({"action":"start","title":"Branch A","maxTurns":2}))
        .await
        .unwrap();
    assert_eq!(started.details["goal"]["title"], "Branch A");
    // The current goal worker uses triggerTurn=true. Pause immediately so this
    // smoke test remains provider-free while exercising the durable notice path.
    execute(json!({"action":"pause"})).await.unwrap();
    harness.abort_and_reconcile().await.unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["status"],
        "paused"
    );
    let original = harness.session().storage().metadata().id;
    let initial_entries = harness
        .view()
        .find_entries_on_branch(
            &EntryQuery {
                entry_type: Some("message"),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    assert!(
        initial_entries.len() >= 2,
        "start and pause persist goal notices even if kickoff began: {initial_entries:?}"
    );
    let goal_notices = initial_entries
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                rpi_harness::session::types::Entry::Message(message)
                    if matches!(&message.message, rpi_agent::AgentMessage::Custom(custom)
                        if custom.data.get("customType") == Some(&json!("goal")))
            )
        })
        .count();
    assert!(
        goal_notices >= 2,
        "start and pause must each persist a goal notice"
    );

    let host = context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_host();
    host.new_session(json!({})).await.unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["active"],
        false
    );
    host.switch_session(json!({"id":original})).await.unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["title"],
        "Branch A"
    );
    assert!(
        !reload_extension_resources(&harness, &context)
            .await
            .had_errors
    );
    let tool = harness
        .get_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.tool.schema().name == "goal")
        .unwrap()
        .tool;
    assert_eq!(
        tool.execute(
            "reload",
            json!({"action":"show"}),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {})
        )
        .await
        .unwrap()
        .details["goal"]["status"],
        "paused"
    );
}
