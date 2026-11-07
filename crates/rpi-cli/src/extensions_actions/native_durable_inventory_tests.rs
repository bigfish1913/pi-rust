//! Inventory-only coverage: no prompts, providers, disk sessions, or config writes.
use super::*;
use rpi_harness::session::{
    memory::{InMemorySessionStorage, SystemClock},
    session::DefaultIdGenerator,
    types::{EntryQuery, SessionMetadata},
};
use rpi_harness::types::{AgentHarnessOptions, AgentHarnessResources, PromptTemplate};
use serde_json::{json, Value};

fn empty_host(
    cwd: &std::path::Path,
    flags: BTreeMap<String, Value>,
) -> (HarnessActionHost, Arc<OnceLock<WeakAgentHarness>>) {
    HarnessActionHost::new_empty(
        vec![],
        cwd.to_path_buf(),
        Handle::current(),
        flags,
        rpi_extensions::ToolCallContext::default(),
    )
}

async fn inventory(host: &HarnessActionHost) -> Vec<Value> {
    host.list_commands(json!({})).await.unwrap()["commands"]
        .as_array()
        .expect("ListCommands must contain an array")
        .clone()
}

fn named<'a>(commands: &'a [Value], name: &str) -> Vec<&'a Value> {
    commands.iter().filter(|c| c["name"] == name).collect()
}

fn assert_native(commands: &[Value], enabled: bool) {
    let durable = named(commands, "/durable");
    assert_eq!(durable.len(), usize::from(enabled));
    if enabled {
        assert_eq!(durable[0]["source"], "builtin");
        assert!(durable[0]["description"]
            .as_str()
            .unwrap()
            .contains("Native durable"));
    }
}

fn static_inventory(enabled: bool) -> Vec<Value> {
    merge_command_list_reserved(
        crate::interactive_tui::build_builtin_commands_for_abi(enabled),
        vec![],
        vec![],
        crate::interactive_tui::builtin_reserved_names(enabled),
    )
}

async fn template_harness(cwd: &std::path::Path) -> AgentHarness {
    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: format!("inventory-{}", cwd.file_name().unwrap().to_string_lossy()),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    AgentHarness::create(AgentHarnessOptions {
        session,
        tools: vec![],
        active_tool_names: vec![],
        models: vec![],
        resources: AgentHarnessResources {
            prompt_templates: Some(vec![
                PromptTemplate {
                    name: "durable".into(),
                    description: Some("durable template".into()),
                    content: "synthetic durable template".into(),
                },
                PromptTemplate {
                    name: "review".into(),
                    description: Some("review template".into()),
                    content: "synthetic review template".into(),
                },
            ]),
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap()
}

extern "C" fn unreachable_legacy_command(
    _: rpi_plugin_sdk::StbStringRef,
    _: *mut rpi_plugin_sdk::StbString,
    _: *mut std::ffi::c_void,
) -> i32 {
    panic!("listing commands must never execute a legacy ABI handler")
}

#[tokio::test]
async fn empty_host_inventory_matches_the_tui_and_requires_explicit_opt_in() {
    let cwd = tempfile::tempdir().unwrap();
    // In particular, new_empty's default must remain disabled.
    let (default_host, _) = empty_host(cwd.path(), BTreeMap::new());
    let baseline = inventory(&default_host).await;
    assert_native(&baseline, false);
    assert_eq!(baseline, static_inventory(false));

    for enabled in [false, true] {
        let (host, _) = empty_host(cwd.path(), BTreeMap::new());
        let host = host.with_native_durable(enabled);
        let commands = inventory(&host).await;
        assert_native(&commands, enabled);
        assert_eq!(commands, static_inventory(enabled));
        assert!(commands.iter().all(|c| c["source"] == "builtin"));
    }
}

#[tokio::test]
async fn native_builtin_wins_legacy_and_template_collisions_but_legacy_keeps_old_precedence() {
    let cwd = tempfile::tempdir().unwrap();
    let mut registry = rpi_extensions::ExtensionRegistry::new();
    assert!(!registry.register_command(
        "durable".into(),
        "legacy durable command".into(),
        unreachable_legacy_command,
        std::ptr::null_mut(),
    ));
    let registry = Arc::new(registry.snapshot());
    let harness = template_harness(cwd.path()).await;
    for enabled in [false, true] {
        let (host, cell) = empty_host(cwd.path(), BTreeMap::new());
        let host = host.with_native_durable(enabled);
        host.publish_registry_impl(Some(Arc::clone(&registry)));
        HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        let commands = inventory(&host).await;
        let durable = named(&commands, "/durable");
        assert_eq!(durable.len(), 1, "a collision must not produce duplicates");
        if enabled {
            assert_native(&commands, true);
        } else {
            assert_eq!(durable[0]["source"], "extension");
            assert_eq!(durable[0]["description"], "legacy durable command");
        }
        let review = named(&commands, "/review");
        assert_eq!(review.len(), 1);
        assert_eq!(review[0]["source"], "prompt");
    }
    harness.close().await.unwrap();
}

#[tokio::test]
async fn unknown_flag_cannot_enable_native_inventory_and_empty_tools_cannot_disable_it() {
    let cwd = tempfile::tempdir().unwrap();
    let flags = BTreeMap::from([("native-durable".into(), json!(true))]);
    let (disabled, disabled_cell) = empty_host(cwd.path(), flags.clone());
    assert_native(&inventory(&disabled).await, false);
    let (enabled, enabled_cell) = empty_host(cwd.path(), flags);
    let enabled = enabled.with_native_durable(true);
    let harness = template_harness(cwd.path()).await;
    let storage = harness.session().storage();
    let entries_before =
        serde_json::to_value(storage.find_entries(&EntryQuery::default()).await.unwrap()).unwrap();
    let revision_before = storage.history_revision().await.unwrap();
    let snapshot_before = harness.snapshot().await.unwrap();

    HarnessActionHost::set_harness(&disabled_cell, Arc::new(harness.clone()));
    HarnessActionHost::set_harness(&enabled_cell, Arc::new(harness.clone()));
    let disabled_commands = inventory(&disabled).await;
    let durable_template = named(&disabled_commands, "/durable");
    assert_eq!(durable_template.len(), 1);
    assert_eq!(durable_template[0]["source"], "prompt");
    assert_eq!(durable_template[0]["description"], "durable template");
    let commands = inventory(&enabled).await;
    assert_native(&commands, true);
    let review = named(&commands, "/review");
    assert_eq!(review.len(), 1);
    assert_eq!(review[0]["source"], "prompt");
    assert_eq!(review[0]["description"], "review template");

    let snapshot_after = harness.snapshot().await.unwrap();
    assert_eq!(snapshot_after.lane, snapshot_before.lane);
    assert_eq!(snapshot_after.leaf_id, snapshot_before.leaf_id);
    assert_eq!(snapshot_after.name, snapshot_before.name);
    assert_eq!(snapshot_after.labels, snapshot_before.labels);
    assert!(snapshot_before.active.is_none());
    assert!(snapshot_after.active.is_none());
    assert_eq!(storage.history_revision().await.unwrap(), revision_before);
    assert_eq!(
        serde_json::to_value(storage.find_entries(&EntryQuery::default()).await.unwrap()).unwrap(),
        entries_before
    );

    harness.close().await.unwrap();
    for (host, native) in [(&disabled, false), (&enabled, true)] {
        let commands = inventory(host).await;
        assert_native(&commands, native);
        assert_eq!(commands, static_inventory(native));
        assert!(named(&commands, "/review").is_empty());
        assert!(commands.iter().all(|c| c["source"] != "prompt"));
    }
    assert_eq!(storage.history_revision().await.unwrap(), revision_before);
    assert_eq!(
        serde_json::to_value(storage.find_entries(&EntryQuery::default()).await.unwrap()).unwrap(),
        entries_before
    );
}

#[tokio::test]
async fn reservation_only_adds_durable_when_enabled_and_preserves_hidden_names_and_aliases() {
    let cwd = tempfile::tempdir().unwrap();
    for enabled in [false, true] {
        let reserved = crate::interactive_tui::builtin_reserved_names(enabled);
        assert_eq!(
            reserved.iter().filter(|n| *n == "/durable").count(),
            usize::from(enabled)
        );
        assert!(reserved.iter().any(|n| n == "/q"));
        assert!(reserved.iter().any(|n| n == "/context"));
        let mut registry = rpi_extensions::ExtensionRegistry::new();
        for name in ["durable", "q", "context", "inventory-only"] {
            registry.register_command(
                name.into(),
                format!("legacy {name}"),
                unreachable_legacy_command,
                std::ptr::null_mut(),
            );
        }
        let (host, _) = empty_host(cwd.path(), BTreeMap::new());
        let host = host.with_native_durable(enabled);
        host.publish_registry_impl(Some(Arc::new(registry.snapshot())));
        let commands = inventory(&host).await;
        assert!(named(&commands, "/q").is_empty());
        assert!(named(&commands, "/context").is_empty());
        let durable = named(&commands, "/durable");
        assert_eq!(durable.len(), 1);
        assert_eq!(
            durable[0]["source"],
            if enabled { "builtin" } else { "extension" }
        );
        assert_eq!(named(&commands, "/inventory-only").len(), 1);
        assert_eq!(
            named(&commands, "/inventory-only")[0]["source"],
            "extension"
        );

        // Both extension and template spellings of aliases/hidden names stay blocked.
        let merged = merge_command_list_reserved(
            crate::interactive_tui::build_builtin_commands_for_abi(enabled),
            vec![("q".into(), "extension alias".into())],
            vec![("/context".into(), "hidden template".into())],
            reserved,
        );
        assert!(named(&merged, "/q").is_empty());
        assert!(named(&merged, "/context").is_empty());
    }
}
