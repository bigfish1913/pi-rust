use super::*;

struct RestoreConfig(Option<std::ffi::OsString>);

impl Drop for RestoreConfig {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}

#[tokio::test]
async fn selected_root_survives_build_runtime_actions_and_both_reload_host_paths() {
    let temporary = tempfile::tempdir().unwrap();
    let env_root = temporary.path().join("env sessions");
    let _env = crate::config::test_support::SessionDirEnv::new(Some(&env_root));
    let _config = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let agent = temporary.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let settings_path = agent.join("settings.json");

    for mode in ["flag", "settings", "environment"] {
        let cwd = temporary.path().join(mode);
        std::fs::create_dir_all(&cwd).unwrap();
        let root = if mode == "flag" {
            cwd.join("../flag selected sessions")
        } else {
            temporary.path().join(format!("{mode} selected sessions"))
        };
        let settings = match mode {
            "environment" => serde_json::json!({}),
            "flag" => {
                serde_json::json!({"sessionDir": temporary.path().join("ignored settings root")})
            }
            _ => serde_json::json!({"sessionDir": root}),
        };
        std::fs::write(&settings_path, settings.to_string()).unwrap();
        std::env::set_var(
            "RPI_CODING_AGENT_SESSION_DIR",
            if mode == "environment" {
                &root
            } else {
                &env_root
            },
        );
        let resolved = crate::provider::resolve(
            Some("anthropic"),
            Some(crate::provider::DEFAULT_MODEL_ID),
            None,
            Some("test-key"),
            None,
        )
        .unwrap();
        let args = Args {
            session_dir: (mode == "flag")
                .then(|| std::path::PathBuf::from("../flag selected sessions")),
            no_extensions: true,
            no_tools: true,
            no_skills: true,
            no_prompt_templates: true,
            no_context_files: true,
            system_prompt: Some("test".into()),
            ..Default::default()
        };
        let prepared = prepare_extensions(&args, &cwd, false).unwrap();
        assert_eq!(prepared.session_dir, root);
        // Extension discovery must not make later session setup re-resolve the
        // root from changed settings or process environment.
        let changed_root = temporary.path().join(format!("{mode} changed sessions"));
        std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", &changed_root);
        std::fs::write(
            &settings_path,
            serde_json::json!({"sessionDir": changed_root}).to_string(),
        )
        .unwrap();
        let (harness, _events, context) = build_prepared(&resolved, &args, &cwd, false, prepared)
            .await
            .unwrap();
        assert_eq!(context.session_dir(), root);
        let initial_id = harness.session().storage().metadata().id.clone();
        harness.session().set_name(Some("original")).await.unwrap();
        let original_host = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_host();
        let fork_id = original_host.fork(serde_json::json!({})).await.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(fork_id, initial_id);
        assert!(
            !reload_extension_resources(&harness, &context)
                .await
                .had_errors
        );
        let host = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_host();
        host.switch_session(serde_json::json!({"id":initial_id}))
            .await
            .unwrap();
        assert_eq!(
            harness.session().get_name().await.unwrap().as_deref(),
            Some("original")
        );
        // Exercise reload's fallback constructor as well as bridge reuse.
        context.action_bridge.lock().unwrap().take();
        assert!(
            !reload_extension_resources(&harness, &context)
                .await
                .had_errors
        );
        let host = context
            .action_bridge
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_host();
        let new_id = host.new_session(serde_json::json!({})).await.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(harness.session().storage().metadata().id, new_id);
        host.switch_session(serde_json::json!({"id":fork_id}))
            .await
            .unwrap();
        let metadata = list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
            .await
            .unwrap();
        assert_eq!(metadata.len(), 3, "{mode}");
        // The active session (the fork being served) keeps its writer lock;
        // the sessions this process switched away from are released for other
        // processes to open again.
        let active_id = harness.session().storage().metadata().id.clone();
        for meta in metadata {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(format!("{}.lock", meta.path))
                .unwrap();
            assert_eq!(
                fs2::FileExt::try_lock_exclusive(&lock).is_err(),
                meta.id == active_id,
                "session {} lock held = {} but it is {}active",
                meta.id,
                meta.id == active_id,
                if meta.id == active_id { "" } else { "not " }
            );
        }
        assert!(
            !changed_root.exists(),
            "{mode}: runtime must not drift to the changed root"
        );
        assert!(!cwd.join(".rpi/sessions").exists());
    }
}

#[tokio::test]
async fn resume_picker_reports_the_selected_root_when_it_has_no_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("custom empty sessions");
    let error = crate::resume_picker::select_in_dir(temporary.path(), &root)
        .await
        .unwrap_err();
    assert!(error.contains(&root.display().to_string()));
    create_jsonl_session(&root, &temporary.path().to_string_lossy())
        .await
        .unwrap();
    let error = crate::resume_picker::select_in_dir(temporary.path(), &root)
        .await
        .unwrap_err();
    assert!(error.contains("no resumable sessions"));
    assert!(error.contains(&root.display().to_string()));
}

#[tokio::test]
async fn importing_a_torn_tail_repairs_only_the_private_copy() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy();
    let source_root = temporary.path().join("source");
    let session = create_jsonl_session(&source_root, &cwd).await.unwrap();
    session.set_name(Some("preserved")).await.unwrap();
    let path = list_session_files_sync(&source_root).remove(0);
    let mut content = std::fs::read(&path).unwrap();
    content.extend_from_slice(b"{\"kind\":");
    std::fs::write(&path, &content).unwrap();
    let root = temporary.path().join("destination");
    let imported = import_session_file(&path, &cwd, &root).await.unwrap();
    assert_eq!(
        imported.get_name().await.unwrap().as_deref(),
        Some("preserved")
    );
    assert_eq!(std::fs::read(&path).unwrap(), content);
    let saved = list_session_files_sync(&root).remove(0);
    assert!(!std::fs::read_to_string(saved)
        .unwrap()
        .ends_with("{\"kind\":"));
}

#[tokio::test]
async fn fork_takes_a_session_file_outside_the_session_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy();
    let source_root = temporary.path().join("source");
    let source = create_jsonl_session(&source_root, &cwd).await.unwrap();
    source.set_name(Some("parent")).await.unwrap();
    let parent_id = source.get_metadata().await.unwrap().id;
    // A caller copies the parent file elsewhere and forks from that copy.
    let copy = temporary.path().join("elsewhere").join("parent.jsonl");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    // Move the complete recovery pair, not only the history manifest.
    let original = list_session_files_sync(&source_root).remove(0);
    std::fs::copy(&original, &copy).unwrap();
    std::fs::copy(
        format!("{}.progress.jsonl", original.display()),
        format!("{}.progress.jsonl", copy.display()),
    )
    .unwrap();
    let root = temporary.path().join("sessions");
    let forked = fork_session_at_launch(&copy.to_string_lossy(), &root, &cwd, None)
        .await
        .unwrap_or_else(|error| panic!("fork from a file path failed: {error}"));
    let forked_meta = forked.get_metadata().await.unwrap();
    assert_ne!(forked_meta.id, parent_id);
    assert_eq!(forked.get_name().await.unwrap().as_deref(), Some("parent"));
    assert_eq!(list_session_files_sync(&root).len(), 1);
}

/// Another process's probe handle: flock the log's lock file and report whether
/// the exclusive lock could be taken. Mirrors exactly what a second rpi does
/// in `claim_session_lock` (`try_lock_exclusive` on `<session>.jsonl.lock`).
fn other_process_lock(log_path: &Path) -> Result<std::fs::File, &'static str> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(format!("{}.lock", log_path.display()))
        .map_err(|_| "open lock file")?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| "session is already locked")?;
    Ok(file)
}

async fn harness_over(session: Session) -> rpi_harness::agent_harness::AgentHarness {
    use rpi_ai::Api;
    use rpi_harness::agent_harness::AgentHarness;
    use rpi_harness::types::{
        AgentHarnessOptions, AgentHarnessResources, DrivingMode, HarnessToolExecution, RetryPolicy,
    };
    AgentHarness::create(AgentHarnessOptions {
        model: rpi_ai::Model::new("m", "m", Api::Faux, "faux", "http://x"),
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: AgentHarnessResources::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: HarnessToolExecution::default(),
        drive: DrivingMode::default(),
        session,
        allow_existing_session: true,
        models: Vec::new(),
        to_provider_messages: None,
        entry_projectors: Default::default(),
        entry_transforms: Vec::new(),
        agent_emitter: None,
        provider_hooks: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
    })
    .await
    .unwrap()
}

// Axis A: a session the process switched away from must be openable by another
// process. This is the user-visible defect: `/new` left the old log locked.
#[tokio::test]
async fn hot_switch_releases_the_outgoing_session_lock_to_other_processes() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy().to_string();
    let root = temporary.path().join("sessions");
    let first = create_jsonl_session(&root, &cwd).await.unwrap();
    let first_path = first.storage().session_log_path().unwrap();
    let harness = harness_over(first).await;
    // Re-claiming the active session's own lock is the hot-switch no-op path.
    let second = create_jsonl_session(&root, &cwd).await.unwrap();
    set_active_session(&harness, second, None).await.unwrap();
    // A second rpi process can now open the switched-away session.
    let _probe = other_process_lock(Path::new(&first_path))
        .expect("outgoing session log must be unlocked after set_active_session swapped it out");
}

// Axis B: switching back re-claims the released lock through the real flock,
// not a stale claim record — after release, re-claim runs try_lock_exclusive.
#[tokio::test]
async fn switch_back_reclaims_a_released_session_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy().to_string();
    let root = temporary.path().join("sessions");
    let first = create_jsonl_session(&root, &cwd).await.unwrap();
    let first_path = first.storage().session_log_path().unwrap();
    let first_id = first.storage().metadata().id.clone();
    let harness = harness_over(first).await;
    let second = create_jsonl_session(&root, &cwd).await.unwrap();
    set_active_session(&harness, second, None).await.unwrap();
    // While unlocked, a foreign process takes the lock and then lets go.
    drop(other_process_lock(Path::new(&first_path)).unwrap());
    let restored = open_session_by_id_in_dir(&first_id, &cwd, &root)
        .await
        .unwrap_or_else(|e| panic!("switch back after release must re-claim: {e}"));
    assert_eq!(restored.session.storage().metadata().id, first_id);
}

// Axis C: re-activating the session the harness already serves (the
// same-session hot-switch reentry) must neither release nor deadlock.
#[tokio::test]
async fn same_session_swap_keeps_its_writer_lock_held() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy().to_string();
    let root = temporary.path().join("sessions");
    let session = create_jsonl_session(&root, &cwd).await.unwrap();
    let path = session.storage().session_log_path().unwrap();
    let id = session.storage().metadata().id.clone();
    let harness = harness_over(session).await;
    // Same id: the reentry no-op. The lock must stay held against others.
    let reopened = open_session_by_id_in_dir(&id, &cwd, &root)
        .await
        .unwrap_or_else(|e| panic!("same-session re-open must succeed: {e}"));
    set_active_session(&harness, reopened.session, None)
        .await
        .unwrap();
    let probe = other_process_lock(Path::new(&path));
    assert!(
        probe.is_err(),
        "the active session's writer lock must stay held"
    );
}

// Axis D: a durable → ephemeral swap releases exactly the durable log, while
// an ephemeral → durable swap must not touch another session's lock. Lock
// identity is the log path, never the session id.
#[tokio::test]
async fn ephemeral_swaps_release_only_the_outgoing_durable_log() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy().to_string();
    let root = temporary.path().join("sessions");
    let durable = create_jsonl_session(&root, &cwd).await.unwrap();
    let path = durable.storage().session_log_path().unwrap();
    let harness = harness_over(durable).await;
    set_active_session(&harness, ephemeral_session(), None)
        .await
        .unwrap();
    let _probe = other_process_lock(Path::new(&path))
        .expect("durable log must be released when switching to an ephemeral session");
    // Active session is ephemeral (no log). Switching to another durable one
    // must not release anything unexpectedly; the new log ends up claimed.
    let fresh = create_jsonl_session(&root, &cwd).await.unwrap();
    let fresh_path = fresh.storage().session_log_path().unwrap();
    set_active_session(&harness, fresh, None).await.unwrap();
    assert!(
        other_process_lock(Path::new(&fresh_path)).is_err(),
        "the newly active durable session's lock must be held"
    );
}
