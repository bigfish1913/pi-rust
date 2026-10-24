use super::*;

fn state_with_root(root: &std::path::Path) -> Arc<TuiState> {
    let mut state = test_tui_state();
    Arc::get_mut(&mut state).unwrap().session_dir = root.to_path_buf();
    state
}

fn assert_session_locked(path: &str) {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{path}.lock"))
        .unwrap();
    assert!(
        fs2::FileExt::try_lock_exclusive(&lock).is_err(),
        "{path} must remain exclusively locked"
    );
}

#[tokio::test]
async fn tui_fork_locks_the_copy_and_keeps_the_selected_root_for_switch_and_picker() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    source
        .append_message(user_message_with_images("custom-root history", vec![]))
        .await
        .unwrap();
    let initial_id = source.storage().metadata().id.clone();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    fork_session(&harness, &cwd, &state.chat_container, &state, &context).await;
    let fork_id = harness.session().storage().metadata().id.clone();
    assert_ne!(fork_id, initial_id);
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 2);
    for row in &metadata {
        assert_session_locked(&row.path);
    }
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    assert!(
        switch_to_session(
            &harness,
            &lane,
            &initial_id,
            &cwd,
            &state.chat_container,
            &state,
            &context
        )
        .await
    );
    assert_eq!(harness.session().storage().metadata().id, initial_id);
    let rendered = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(rendered.contains("custom-root history"));

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let editor_container = Arc::new(Container::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    open_session_selector(&state, &editor_container, &state.editor, &tui, &tx);
    assert!(
        state.selector_open(),
        "picker must find sessions in the selected root"
    );
    let router = state
        .active_selector
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .0
        .clone();
    router.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(TuiMessage::SwitchSession(selected)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
    else {
        panic!("session selection expected")
    };
    assert!(metadata.iter().any(|row| row.path.contains(&selected)));
    assert!(!cwd.join(".rpi/sessions").exists());
}

#[tokio::test]
async fn tui_import_preserves_identity_history_and_source_across_cwds() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("project");
    let old_cwd = temporary.path().join("original project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&old_cwd).unwrap();
    let root = temporary.path().join("custom sessions");
    let source_root = temporary.path().join("source");
    let source = crate::session::create_jsonl_session(&source_root, &old_cwd.to_string_lossy())
        .await
        .unwrap();
    source.set_name(Some("Imported history")).await.unwrap();
    source
        .append_message(user_message_with_images(
            "preserved imported prompt",
            vec![],
        ))
        .await
        .unwrap();
    let expected = source.storage().metadata().id.clone();
    let source_path = crate::session::list_session_files_sync(&source_root).remove(0);
    let original = std::fs::read(&source_path).unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &source_path.to_string_lossy(),
        &cwd,
        &state.chat_container,
        &state,
        &context,
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, expected);
    assert_eq!(
        harness.session().get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    assert!(strip_ansi(&state.chat_container.render(160).join("\n"))
        .contains("preserved imported prompt"));
    let metadata = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].cwd, old_cwd.to_string_lossy());
    assert_session_locked(&metadata[0].path);
    let restored =
        crate::session::open_session_by_id_in_dir(&expected, &cwd.to_string_lossy(), &root)
            .await
            .map_err(|error| error.to_string())
            .unwrap();
    assert_eq!(
        restored.get_name().await.unwrap().as_deref(),
        Some("Imported history")
    );
    let imported = std::fs::read(&metadata[0].path).unwrap();
    assert_eq!(imported, original);
    // Both repeated imports and importing a repository's own log must fail
    // without touching either saved history or the original source.
    for path in [&source_path, &std::path::PathBuf::from(&metadata[0].path)] {
        assert!(
            crate::session::import_session_file(path, &cwd.to_string_lossy(), &root)
                .await
                .err()
                .unwrap()
                .contains("already exists")
        );
    }
    assert_eq!(std::fs::read(&source_path).unwrap(), original);
    assert_eq!(std::fs::read(&metadata[0].path).unwrap(), imported);
    assert_eq!(crate::session::list_session_files_sync(&root).len(), 1);
}

#[tokio::test]
async fn invalid_import_leaves_the_active_session_and_repository_untouched() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("selected");
    let invalid = cwd.join("invalid.jsonl");
    std::fs::write(&invalid, "not a session\n").unwrap();
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let initial = harness.session().storage().metadata().id.clone();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    import_session(
        &harness,
        &lane,
        &invalid.to_string_lossy(),
        cwd,
        &state.chat_container,
        &state,
        &context,
    )
    .await;
    assert_eq!(harness.session().storage().metadata().id, initial);
    assert!(crate::session::list_session_files_sync(&root).is_empty());
    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "staging files must be cleaned up"
    );
    assert_eq!(std::fs::read_to_string(invalid).unwrap(), "not a session\n");
}

/// Every path that rebuilds the transcript for another session must advertise
/// the session's REAL capability snapshot.
///
/// The regression this locks: those paths used to call `add_welcome_message`,
/// which passes empty slices, so a session with tools and skills still showed
/// `Tools (0) none` / `Skills (0) none` right after `/session`, `/import`,
/// `/fork` or tree navigation.
#[tokio::test]
async fn session_switch_banner_reports_real_capabilities() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let first = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let first_id = first.storage().metadata().id.clone();
    first
        .append_message(user_message_with_images("first session history", vec![]))
        .await
        .unwrap();
    let second = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let second_id = second.storage().metadata().id.clone();
    assert_ne!(first_id, second_id);

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(second).await.unwrap();
    // One real tool so `Tools (N)` has something to count; the empty-sentinel
    // fallback would otherwise mask a broken banner.
    harness
        .set_tools(
            vec![rpi_harness::types::HarnessTool::new(Arc::new(
                BannerProbeTool::new("probe-tool"),
            ))],
            None,
        )
        .await
        .unwrap();
    harness
        .set_resources(rpi_harness::types::AgentHarnessResources {
            skills: Some(vec![rpi_harness::types::Skill {
                name: "demo-skill".into(),
                description: "a skill the banner must advertise".into(),
                content: String::new(),
                file_path: "/skills/demo-skill/SKILL.md".into(),
                disable_model_invocation: None,
            }]),
            ..Default::default()
        })
        .await
        .unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());

    assert!(
        switch_to_session(
            &harness,
            &lane,
            &first_id,
            cwd,
            &state.chat_container,
            &state,
            &context
        )
        .await
    );

    let banner = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(
        banner.contains("Skills (1)"),
        "the banner must count the session's skill: {banner}"
    );
    assert!(
        banner.contains("demo-skill"),
        "the banner must name the session's skill: {banner}"
    );
    // Reverse assertion: the empty-capability rendering must be gone. Without
    // it, a partially-empty implementation could still pass on unrelated text.
    assert!(
        !banner.contains("Skills (0)"),
        "the banner must not fall back to the empty capability state: {banner}"
    );
    assert!(
        !banner.contains("Tools (0) none"),
        "the banner must not report zero tools: {banner}"
    );
    assert!(
        banner.contains("Tools (1)") && banner.contains("probe-tool"),
        "the banner must count the session's tools: {banner}"
    );
}

/// `/new` must start a NEW session file, not keep appending to the old one, and
/// must reset the per-session footer counters while keeping the environment.
#[tokio::test]
async fn new_session_creates_a_file_and_resets_footer() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path();
    let root = cwd.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let previous_id = source.storage().metadata().id.clone();
    source
        .append_message(user_message_with_images(
            "history that stays behind",
            vec![],
        ))
        .await
        .unwrap();

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    harness.set_session(source).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = state_with_root(&root);
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());

    // Simulate a session that already accumulated usage before `/new`.
    state.footer.set_model("(prov) model");
    state.footer.set_context_window(200_000);
    state.footer.set_session_name(Some("old session"));
    state.footer.add_usage(12_000, 3_400, 40_000, 1_200, 0.421);
    state.footer.set_context_tokens(Some(126_400));

    assert!(
        start_new_session(
            &harness,
            &lane,
            cwd,
            &state.chat_container,
            &state,
            &context
        )
        .await
    );

    // (a) the harness serves a different session
    let new_id = harness.session().storage().metadata().id.clone();
    assert_ne!(new_id, previous_id, "`/new` must swap the session id");
    assert_eq!(
        std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).unwrap_or_default(),
        new_id,
        "the plugin env channel must follow the swap"
    );
    assert_eq!(
        context.session_id().as_deref(),
        Some(new_id.as_str()),
        "the plugin tool context must follow the swap"
    );

    // (b) the new session file really exists under the session root. The JSONL
    // repo nests files under a cwd-derived directory, so walk the tree instead
    // of assuming a flat layout.
    let files = jsonl_files_under(&root);
    assert!(
        files.iter().any(|path| path.contains(&new_id)),
        "new session file missing under {}: {files:?}",
        root.display()
    );
    assert_eq!(
        files.iter().filter(|path| path.contains(&new_id)).count(),
        1,
        "exactly one file may back the new session: {files:?}"
    );
    // The previous session is preserved, not deleted: `/new` must not destroy
    // user data.
    assert!(
        files.iter().any(|path| path.contains(&previous_id)),
        "the previous session file must survive: {files:?}"
    );

    // (c) the footer's session-scoped numbers are gone, the environment stays.
    let stats = strip_ansi(&state.footer.render(120).join("\n"));
    assert!(!stats.contains("↑12k"), "usage must reset: {stats}");
    assert!(!stats.contains("$0.421"), "cost must reset: {stats}");
    assert!(!stats.contains("old session"), "name must reset: {stats}");
    assert!(
        !stats.contains("63.2%"),
        "context badge must reset: {stats}"
    );
    assert!(stats.contains("(prov) model"), "model kept: {stats}");
    assert!(stats.contains("200k"), "window kept: {stats}");

    // The transcript is the new (empty) session, not the old history.
    let banner = strip_ansi(&state.chat_container.render(160).join("\n"));
    assert!(
        !banner.contains("history that stays behind"),
        "the old transcript must not follow into the new session: {banner}"
    );
    assert!(
        banner.contains(&format!("Started new session {new_id}.")),
        "the note must name the new session: {banner}"
    );
}

/// Every `.jsonl` file under `root`, at any depth.
fn jsonl_files_under(root: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found
}

/// A tool that exists only to give the banner something to count.
struct BannerProbeTool {
    schema: rpi_ai::types::Tool,
}

impl BannerProbeTool {
    fn new(name: &str) -> Self {
        Self {
            schema: rpi_ai::types::Tool {
                name: name.to_string(),
                description: "probe tool".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({
                    "type": "object",
                    "properties": {},
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for BannerProbeTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    fn execution_mode(&self) -> rpi_agent::types::ToolExecutionMode {
        rpi_agent::types::ToolExecutionMode::Parallel
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::AgentError> {
        Ok(rpi_agent::types::AgentToolResult::text("probe"))
    }
}
