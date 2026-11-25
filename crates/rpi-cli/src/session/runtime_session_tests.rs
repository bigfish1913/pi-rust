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
            &crate::config::AgentHome::process(),
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
        let prepared =
            prepare_extensions(&crate::config::AgentHome::process(), &args, &cwd, false).unwrap();
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
        let (harness, _events, context) = build_prepared(
            &crate::config::AgentHome::process(),
            &resolved,
            &args,
            &cwd,
            false,
            prepared,
        )
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
        for meta in metadata {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(format!("{}.lock", meta.path))
                .unwrap();
            if meta.id == fork_id {
                assert!(fs2::FileExt::try_lock_exclusive(&lock).is_err());
            } else {
                assert!(fs2::FileExt::try_lock_exclusive(&lock).is_ok());
            }
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
    // Import opens a fresh writer binding for the copied artifact; release the
    // fixture's original storage lease first.
    drop(session);
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
async fn fork_entry_cuts_the_new_branch_at_the_selected_message() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy();
    let source_root = temporary.path().join("source");
    let source = create_jsonl_session(&source_root, &cwd).await.unwrap();
    let first = source
        .append_message(rpi_agent::message::AgentMessage::User(
            rpi_ai::types::UserMessage::new("first", 1),
        ))
        .await
        .unwrap();
    let second = source
        .append_message(rpi_agent::message::AgentMessage::User(
            rpi_ai::types::UserMessage::new("second", 2),
        ))
        .await
        .unwrap();
    let source_file = list_session_files_sync(&source_root).remove(0);
    let root = temporary.path().join("sessions");
    let forked = fork_session_at_launch(
        &source_file.to_string_lossy(),
        &root,
        &cwd,
        None,
        Some(&first),
        Some(rpi_harness::session::types::ForkPosition::At),
    )
    .await
    .unwrap();
    let ids: Vec<String> = forked
        .find_entries(&rpi_harness::session::types::EntryQuery {
            order: Some(rpi_harness::session::types::EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.id().to_owned())
        .collect();
    assert!(
        ids.contains(&first),
        "inclusive fork must keep the selected entry"
    );
    assert!(
        !ids.contains(&second),
        "entries after the fork point must be absent"
    );
}

/// `before` drops the selected entry itself, so the branch ends at its parent.
/// This is also the position the harness picks when `--fork-position` is
/// omitted, which is why the CLI must not substitute its own default.
#[tokio::test]
async fn fork_entry_before_excludes_the_selected_message() {
    use rpi_harness::session::types::{EntryOrder, EntryQuery, ForkPosition};

    for position in [Some(ForkPosition::Before), None] {
        let temporary = tempfile::tempdir().unwrap();
        let cwd = temporary.path().to_string_lossy();
        let source_root = temporary.path().join("source");
        let source = create_jsonl_session(&source_root, &cwd).await.unwrap();
        let first = source
            .append_message(rpi_agent::message::AgentMessage::User(
                rpi_ai::types::UserMessage::new("first", 1),
            ))
            .await
            .unwrap();
        let second = source
            .append_message(rpi_agent::message::AgentMessage::User(
                rpi_ai::types::UserMessage::new("second", 2),
            ))
            .await
            .unwrap();
        let source_file = list_session_files_sync(&source_root).remove(0);
        let root = temporary.path().join("sessions");
        let forked = fork_session_at_launch(
            &source_file.to_string_lossy(),
            &root,
            &cwd,
            None,
            Some(&second),
            position,
        )
        .await
        .unwrap();
        let ids: Vec<String> = forked
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert!(
            ids.contains(&first),
            "{position:?}: the parent must survive an exclusive cut"
        );
        assert!(
            !ids.contains(&second),
            "{position:?}: the selected entry must be dropped"
        );
    }
}

/// `--fork-entry` is an exact entry id, unlike `--fork`'s partial session id.
/// An unknown id must fail the launch instead of silently forking at the leaf.
#[tokio::test]
async fn fork_entry_rejects_an_unknown_entry_id() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy();
    let source_root = temporary.path().join("source");
    let source = create_jsonl_session(&source_root, &cwd).await.unwrap();
    let first = source
        .append_message(rpi_agent::message::AgentMessage::User(
            rpi_ai::types::UserMessage::new("first", 1),
        ))
        .await
        .unwrap();
    let source_file = list_session_files_sync(&source_root).remove(0);
    let root = temporary.path().join("sessions");
    // A prefix of a real id must not match.
    let prefix = &first[..first.len() - 1];
    for unknown in ["no-such-entry", prefix] {
        let error = match fork_session_at_launch(
            &source_file.to_string_lossy(),
            &root,
            &cwd,
            None,
            Some(unknown),
            None,
        )
        .await
        {
            Ok(_) => panic!("an unknown --fork-entry must not fall back to the lane leaf"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(unknown),
            "{unknown}: the error must name the entry it could not find, got {error}"
        );
    }
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
    let forked = fork_session_at_launch(&copy.to_string_lossy(), &root, &cwd, None, None, None)
        .await
        .unwrap_or_else(|error| panic!("fork from a file path failed: {error}"));
    let forked_meta = forked.get_metadata().await.unwrap();
    assert_ne!(forked_meta.id, parent_id);
    assert_eq!(forked.get_name().await.unwrap().as_deref(), Some("parent"));
    assert_eq!(list_session_files_sync(&root).len(), 1);
}

#[tokio::test]
async fn launch_fork_cuts_at_or_before_a_historical_message() {
    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().to_string_lossy();
    let root = temporary.path().join("sessions");
    let source = create_jsonl_session(&root, &cwd).await.unwrap();
    let mut entries = Vec::new();
    for content in ["first", "target", "later"] {
        let message = rpi_ai::types::UserMessage::new(content, 42);
        entries.push(source.append_message(message.into()).await.unwrap());
    }
    let custom = source.append_custom_entry("test", None).await.unwrap();
    let path = list_session_files_sync(&root).remove(0);
    let original = std::fs::read(&path).unwrap();
    let launch = |entry: &str, position: &str| {
        let args = crate::args::parse_args(&[
            "--fork".into(),
            path.to_string_lossy().into_owned(),
            format!("--fork-entry={entry}"),
            format!("--fork-position={position}"),
        ]);
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        args
    };
    for (position, leaf) in [("at", &entries[1]), ("before", &entries[0])] {
        let args = launch(&entries[1], position);
        let selection = select_session_in_dir(&args, root.clone());
        let forked = build_session(&selection, &cwd, None, None).await.unwrap();
        assert_eq!(forked.get_leaf_id().await.unwrap().as_ref(), Some(leaf));
        assert!(forked.get_entry(&entries[2]).await.unwrap().is_none());
    }
    for entry in ["missing", &custom] {
        let args = launch(entry, "at");
        let selection = select_session_in_dir(&args, root.clone());
        assert!(build_session(&selection, &cwd, None, None).await.is_err());
    }
    assert_eq!(list_session_files_sync(&root).len(), 3);
    assert_eq!(std::fs::read(path).unwrap(), original);
}
