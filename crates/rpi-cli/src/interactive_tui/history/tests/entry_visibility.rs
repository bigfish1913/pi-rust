use super::*;
use rpi_extensions::{
    ExtensionRegistry, ExtensionSession, PluginKeepalive, RegisteredRenderer,
    RegisteredRendererKind,
};
use rpi_plugin_sdk::{StbString, StbStringRef};

extern "C" fn hide_entry(_: StbStringRef, out: *mut StbString, _: *mut std::ffi::c_void) -> i32 {
    // SAFETY: the callback receives a live output slot, reclaimed with the
    // matching SDK deallocator registered below.
    unsafe { *out = StbString::from_string(r#"{"display":false}"#.into()) };
    0
}

#[tokio::test]
async fn entry_visibility_survives_restore_and_history_paging_without_losing_storage() {
    let (storage, state, tui) = fixture(220).await;
    for index in 220..260 {
        append(
            &storage,
            index,
            ProvisionedKind::Custom {
                custom_type: "private-memory".into(),
                data: Some(serde_json::json!({"text": "internal raw fallback"})),
            },
        )
        .await;
    }
    let mut registry = ExtensionRegistry::new();
    registry.register_renderer(RegisteredRenderer {
        name: "private-memory".into(),
        kind: RegisteredRendererKind::Entry,
        render_fn: hide_entry,
        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
        user_data: std::ptr::null_mut(),
    });
    *state.extension_session.lock().unwrap() = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        PluginKeepalive::empty(),
        vec![],
        None,
    );
    restore_bound(&state, &state.chat_container).await;
    assert!(!text(&state).contains("internal raw fallback"));
    assert!(
        state
            .history_page
            .resident
            .lock()
            .unwrap()
            .page
            .as_ref()
            .unwrap()
            .has_more_before
    );

    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert!(text(&state).contains("history-entry-209"));
    assert!(!text(&state).contains("internal raw fallback"));

    request(state.clone(), tui, Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("internal raw fallback"));
    restore_bound(&state, &state.chat_container).await;
    assert!(!text(&state).contains("internal raw fallback"));
    // A display decision must not remove or rewrite persisted memory.
    let tree = Session::new(storage, None).view("main");
    let page = tree.branch_entry_page(&Default::default()).await.unwrap();
    assert!(page.entries.iter().any(|entry| {
        matches!(entry, Entry::Custom(custom) if custom.data.as_ref().unwrap()["text"] == "internal raw fallback")
    }));
}
