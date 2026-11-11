use super::*;

#[test]
fn host_durable_names_are_reserved_even_when_native_durable_is_disabled() {
    assert!(reserved_durable_name(
        ["read", "durable"].into_iter(),
        [].into_iter()
    ));
    assert!(reserved_durable_name(
        [].into_iter(),
        ["/durable"].into_iter()
    ));
    assert!(reserved_durable_name(
        [].into_iter(),
        ["durable"].into_iter()
    ));
    assert!(!reserved_durable_name(
        ["read"].into_iter(),
        ["durable-other"].into_iter()
    ));
    extern "C" fn command(
        _: rpi_plugin_sdk::StbStringRef,
        _: *mut rpi_plugin_sdk::StbString,
        _: *mut std::ffi::c_void,
    ) -> i32 {
        panic!("conflicting command must never execute")
    }
    let mut registry = rpi_extensions::ExtensionRegistry::new();
    registry.register_command(
        "durable".into(),
        "legacy".into(),
        command,
        std::ptr::null_mut(),
    );
    let session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        rpi_extensions::PluginKeepalive::empty(),
        vec![],
        None,
    );
    let default_error = validate_durable_extensions(&session).unwrap_err();
    assert!(default_error.contains("host-reserved"));
    assert!(validate_durable_extensions(&ExtensionSession::none()).is_ok());
}

#[test]
fn stdio_owning_adapters_can_coexist_with_native_durable() {
    let mut registry = rpi_extensions::ExtensionRegistry::new();
    assert!(registry.apply_declaration_json(r#"{"ownsStdio":true}"#));
    let session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        rpi_extensions::PluginKeepalive::empty(),
        vec![],
        None,
    );
    assert!(session.owns_stdio());
    assert!(validate_durable_extensions(&session).is_ok());
}

#[test]
fn native_durable_registration_is_default_and_never_replay_safe() {
    let args = crate::args::parse_args(&["--tools", "durable"].map(String::from));
    let env = Arc::new(rpi_tools::InMemoryExecutionEnv::new());
    let context = ExecutionToolContext::new(env.clone(), Some(env));
    let tools = build_tools(&context, &args);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool.schema().name, "durable");
    assert_eq!(tools[0].replay, ToolReplay::Never);
    let args =
        crate::args::parse_args(&["--no-native-durable", "--tools", "durable"].map(String::from));
    assert!(build_tools(&context, &args).is_empty());
}

#[tokio::test]
async fn native_durable_reload_refuses_before_preparing_or_publishing() {
    let _guard = crate::config::test_support::env_lock().lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let agent = temp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var(crate::config::CONFIG_DIR_ENV, v),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }
    let _restore = Restore(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
    let args = Args {
        native_durable: true,
        no_session: true,
        no_extensions: true,
        no_tools: true,
        no_skills: true,
        no_context_files: true,
        no_prompt_templates: true,
        system_prompt: Some("synthetic fixture".into()),
        ..Default::default()
    };
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("synthetic-key"),
        None,
    )
    .unwrap();
    let prepared = prepare_extensions(&args, temp.path(), false).unwrap();
    let (harness, _, context) = build_prepared(&resolved, &args, temp.path(), false, prepared)
        .await
        .unwrap();
    let service = rpi_harness::durable::DurableService::bind(
        &harness,
        rpi_harness::durable::DurableActor::owner("test").unwrap(),
    );
    let before = service.snapshot().await.unwrap();
    let bridge = context
        .action_bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
    let command_host = bridge.clone_host();
    let commands = command_host
        .list_commands(serde_json::json!({}))
        .await
        .unwrap();
    let durable: Vec<_> = commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|command| command["name"] == "/durable")
        .collect();
    assert_eq!(
        durable.len(),
        1,
        "build_prepared must configure the runtime host before publication"
    );
    assert_eq!(durable[0]["source"], "builtin");
    assert_eq!(
        durable[0]["description"],
        crate::native_durable::COMMAND_DESCRIPTION
    );
    assert!(
        harness.get_tools().await.unwrap().is_empty(),
        "no-tools must not disable discovery"
    );
    assert_eq!(service.snapshot().await.unwrap(), before);
    let foreign = ephemeral_session();
    foreign
        .append_custom_entry(
            "durable-state",
            Some(serde_json::json!({"synthetic":"legacy"})),
        )
        .await
        .unwrap();
    let target_revision = foreign.storage().history_revision().await.unwrap();
    let plugin_id = context.tool_context.session_id();
    let rejected = set_active_session(&harness, foreign.clone(), Some(&context.tool_context)).await;
    assert!(
        rejected
            .unwrap_err()
            .contains("Unsupported durable journal format"),
        "CLI opt-in must enforce core admission"
    );
    assert_eq!(service.snapshot().await.unwrap(), before);
    assert_eq!(context.tool_context.session_id(), plugin_id);
    assert_eq!(
        foreign.storage().history_revision().await.unwrap(),
        target_revision
    );
    let outcome = reload_extension_resources_inner(&harness, &context, || {
        panic!("reload prepared a replacement")
    })
    .await;
    assert!(outcome.had_errors);
    assert!(outcome.summary.contains("restart-required"));
    assert_eq!(
        command_host
            .list_commands(serde_json::json!({}))
            .await
            .unwrap(),
        commands
    );
    assert_eq!(service.snapshot().await.unwrap(), before);
    harness.close().await.unwrap();
}
