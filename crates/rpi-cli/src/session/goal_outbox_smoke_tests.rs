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
///
/// The goal is seeded with `append_custom_entry` instead of the `goal start`
/// action: since rpi-goal 0.3.0 `start` arms an automatic continuation, which
/// would need a live provider. Seeding keeps this fixture offline while still
/// covering what it is for — branch-local goal state, the deferred notice outbox,
/// turn accounting, and the terminal tool guards across the real ABI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "set RPI_GOAL_SMOKE_DLL to the matching rpi-goal cdylib"]
async fn native_goal_outbox_smoke() {
    use rpi_harness::agent_harness::AgentLane;
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};
    use rpi_plugin_sdk::EventTag;
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

    // The plugin registers the command tool plus the three explicit stopping
    // reasons. `goal start` would arm automatic work, so the state is seeded.
    let tools = harness.get_tools().await.unwrap();
    let tool_named = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.tool.schema().name == name)
            .map(|tool| tool.tool.clone())
            .unwrap_or_else(|| panic!("{name} DLL tool must load"))
    };
    let tool = tool_named("goal");
    for name in ["goal_complete", "goal_blocked", "goal_wait"] {
        tool_named(name);
    }
    let execute = |params| {
        tool.execute(
            "smoke",
            params,
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
    };
    let seed = |title: &str, max_turns: u32| {
        let harness = harness.clone();
        let title = title.to_string();
        async move {
            harness
                .session()
                .append_custom_entry(
                    "goal-state",
                    Some(json!({
                        "version": 2,
                        "active": {
                            "id": format!("seed-{title}"),
                            "title": title,
                            "status": "active",
                            "notes": "",
                            "startedAt": 1,
                            "updatedAt": 1,
                            "turnsUsed": 0,
                            "maxTurns": max_turns,
                        },
                        "archived": []
                    })),
                )
                .await
                .unwrap()
        }
    };

    seed("Branch A", 2).await;
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["title"],
        "Branch A"
    );
    let source = harness.view().get_leaf_id().await.unwrap().unwrap();
    assert!(harness
        .view()
        .find_entries_on_branch(
            &EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            },
            &BranchBounds::default()
        )
        .await
        .unwrap()
        .is_empty());
    // Neither branch navigation nor session switching dispatches a goal callback.
    harness
        .navigate_tree(None, false, None, None)
        .await
        .unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["active"],
        false
    );
    seed("Branch B", 2).await;
    harness
        .navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    assert_eq!(
        execute(json!({"action":"show"})).await.unwrap().details["goal"]["title"],
        "Branch A"
    );

    // A stale id never mutates the goal: the terminal tool rejects it.
    assert!(tool_named("goal_complete")
        .execute(
            "smoke",
            json!({"goal_id": "stale", "summary": "done"}),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .is_err());

    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    let shown = execute(json!({"action":"show"})).await.unwrap();
    // A maxed turn budget is its own stopped state since 0.3.0.
    assert_eq!(shown.details["goal"]["status"], "budget_limited");
    assert_eq!(shown.details["goal"]["turnsUsed"], 2);
    let entries = harness
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
    assert_eq!(
        entries.len(),
        2,
        "one goal notice and one budget-pause notice"
    );
    assert!(dispatch_session_event_async(&context, EventTag::TurnEnd)
        .await
        .is_none());
    assert_eq!(
        harness
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("message"),
                    ..Default::default()
                },
                &BranchBounds::default()
            )
            .await
            .unwrap()
            .len(),
        2
    );

    let original = harness.session().storage().metadata().id;
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
    let reloaded = harness
        .get_tools()
        .await
        .unwrap()
        .into_iter()
        .find(|tool| tool.tool.schema().name == "goal")
        .unwrap()
        .tool;
    assert_eq!(
        reloaded
            .execute(
                "reload",
                json!({"action":"show"}),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(|_| {})
            )
            .await
            .unwrap()
            .details["goal"]["status"],
        "budget_limited"
    );
}
