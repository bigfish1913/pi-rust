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
