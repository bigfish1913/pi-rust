//! Synthetic sessions only; no provider, real config or terminal keyboard input.
use super::*;
use rpi_harness::{
    durable::{DurableActor, DurableService},
    session::{
        memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
        types::{EntryQuery, SessionMetadata},
        Session,
    },
    types::AgentHarnessOptions,
};
use serde_json::{json, Value};

fn memory(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )),
        None,
    )
}
async fn harness(id: &str) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        session: memory(id),
        ..Default::default()
    })
    .await
    .unwrap()
}
fn state(enabled: bool) -> Arc<TuiState> {
    let mut state = super::super::tests::test_tui_state();
    Arc::get_mut(&mut state).unwrap().history_page = HistoryUi::with_native_durable(enabled);
    state
}
fn text(state: &TuiState) -> String {
    strip_ansi(&state.chat_container.render(240).join("\n"))
}
fn service(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner("native-cli").unwrap())
}
async fn command(h: &AgentHarness, value: Value) -> Value {
    crate::native_durable::command(h, &value.to_string())
        .await
        .unwrap()
}
async fn enqueue(h: &AgentHarness, id: &str) {
    command(
        h,
        json!({"action":"enqueue", "requestId":id, "text":"PRIVATE-queue"}),
    )
    .await;
}
fn count(state: &TuiState) -> usize {
    text(state)
        .matches("Observed native durable state:")
        .count()
}

#[tokio::test]
async fn view_entry_refresh_is_read_only_single_mounted_and_clears_terminal_state() {
    let h = harness("notice-view-entry").await;
    enqueue(&h, "queued-1").await;
    let state = state(true);
    let chat = &state.chat_container;
    state.history_page.bind(&h);
    let before = service(&h).snapshot().await.unwrap();
    let entries = h
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap();
    let receipt = service(&h).request_outcome("queued-1").await.unwrap();
    // Same fresh-launch path, without restoring any history.
    refresh_native_durable_notice(&h, &state, chat).await;
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("queued=1, claimed=0"));
    assert!(!text(&state).contains("PRIVATE-queue"));
    let notice: Arc<dyn Component> = state.history_page.native_durable_notice.clone();
    for _ in 0..3 {
        restore(&h, &state, chat).await;
        state.history_page.mount_active(chat);
        assert_eq!(count(&state), 1);
        assert_eq!(
            chat.get_children()
                .iter()
                .filter(|c| Arc::ptr_eq(c, &notice))
                .count(),
            1
        );
    }
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    assert_eq!(
        service(&h).request_outcome("queued-1").await.unwrap(),
        receipt
    );
    assert_eq!(
        h.session()
            .storage()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap(),
        entries
    );
    command(
        &h,
        json!({"action":"cancel","item":"item-1","requestId":"cancel-1"}),
    )
    .await;
    let terminal = service(&h).snapshot().await.unwrap();
    restore(&h, &state, chat).await;
    assert_eq!(
        count(&state),
        0,
        "terminal-only queue must not retain an old notice"
    );
    assert_eq!(service(&h).snapshot().await.unwrap(), terminal);
    h.close().await.unwrap();
}

#[tokio::test]
async fn flag_off_never_polls_and_closed_enabled_is_unavailable_not_empty() {
    let h = harness("notice-disabled").await;
    let state = state(false);
    state.history_page.bind(&h);
    refresh_notice_from(&h, &state, &state.chat_container, async {
        panic!("legacy view must not poll any native read");
        #[allow(unreachable_code)]
        None
    })
    .await;
    assert_eq!(count(&state), 0);
    assert_eq!(state.history_page.native_durable_notice.child_count(), 0);
    h.close().await.unwrap();
    refresh_native_durable_notice(&h, &state, &state.chat_container).await;
    assert_eq!(count(&state), 0);
    let enabled = super::native_durable_tests::state(true);
    enabled.history_page.bind(&h);
    refresh_native_durable_notice(&h, &enabled, &enabled.chat_container).await;
    assert_eq!(count(&enabled), 1);
    assert!(text(&enabled).contains("unavailable (closed)"));
    assert!(text(&enabled).contains("not an empty-state observation"));
}

#[tokio::test]
async fn late_generation_and_replaced_storage_are_not_published() {
    let h = harness("notice-old-binding").await;
    let state = state(true);
    state.history_page.bind(&h);
    refresh_notice_from(&h, &state, &state.chat_container, async {
        state.history_page.invalidate();
        Some("STALE-generation".into())
    })
    .await;
    assert!(!text(&state).contains("STALE"));
    refresh_notice_from(&h, &state, &state.chat_container, async {
        // Even an embedder that forgot UI invalidation cannot mount an old
        // observation onto a different SessionStorage instance.
        h.set_session(memory("notice-replaced-binding"))
            .await
            .unwrap();
        Some("STALE-binding".into())
    })
    .await;
    assert!(!text(&state).contains("STALE"));
    assert_eq!(state.history_page.native_durable_notice.child_count(), 0);
    enqueue(&h, "new-queue").await;
    state.reset_session_state();
    restore(&h, &state, &state.chat_container).await;
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("queued=1, claimed=0"));
    h.close().await.unwrap();
}

#[tokio::test]
async fn paging_remounts_the_same_notice_without_refreshing_or_writing() {
    let h = harness("notice-pages").await;
    let lane: Arc<dyn AgentLane> = h.lane("main");
    for i in 0..75 {
        lane.append_message(user_message_with_images(&format!("history-{i}"), vec![]))
            .await
            .unwrap();
    }
    enqueue(&h, "page-queue").await;
    let state = state(true);
    restore(&h, &state, &state.chat_container).await;
    let before = service(&h).snapshot().await.unwrap();
    let note_children = state.history_page.native_durable_notice.get_children();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    for direction in [Direction::Older, Direction::Newer, Direction::Latest] {
        request(state.clone(), tui.clone(), direction);
        let worker = state.history_page.worker.lock().unwrap().take().unwrap();
        worker.await.unwrap();
        assert_eq!(count(&state), 1);
        assert!(state
            .history_page
            .native_durable_notice
            .get_children()
            .iter()
            .zip(&note_children)
            .all(|(a, b)| Arc::ptr_eq(a, b)));
    }
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    h.close().await.unwrap();
}

#[tokio::test]
async fn jsonl_new_switch_import_and_unadopted_fork_use_the_current_view_only() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temp.path().join("sessions");
    let source = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let id = source.storage().metadata().id.clone();
    let h = AgentHarness::create(AgentHarnessOptions {
        session: source,
        require_native_durable: true,
        ..Default::default()
    })
    .await
    .unwrap();
    enqueue(&h, "jsonl-source").await;
    let rows = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    let source_path = &rows.iter().find(|r| r.id == id).unwrap().path;
    let bytes = std::fs::read(source_path).unwrap();
    let mut state = state(true);
    Arc::get_mut(&mut state).unwrap().session_dir = root.clone();
    let lane: Arc<dyn AgentLane> = h.lane("main");
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    let catalog = Arc::new(std::sync::RwLock::new(Vec::new()));
    restore(&h, &state, &state.chat_container).await;
    assert_eq!(count(&state), 1);
    assert!(start_new_session(&h, &lane, &cwd, &state.chat_container, &state, &context).await);
    assert_eq!(count(&state), 0);
    assert!(
        switch_to_session(
            &h,
            &lane,
            &id,
            &cwd,
            &state.chat_container,
            &state,
            &context,
            &catalog
        )
        .await
    );
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("queued=1, claimed=0"));
    assert_eq!(std::fs::read(source_path).unwrap(), bytes);
    let snapshot = service(&h).snapshot().await.unwrap();
    assert!(
        !switch_to_session(
            &h,
            &lane,
            "missing-synthetic-session",
            &cwd,
            &state.chat_container,
            &state,
            &context,
            &catalog
        )
        .await
    );
    assert_eq!(service(&h).snapshot().await.unwrap(), snapshot);
    assert_eq!(
        count(&state),
        1,
        "failed switch leaves the current view intact"
    );

    // P7 adoption is not delivered: copied receipts cannot name a new session.
    // Reject before publication, preserving the old view and its valid notice.
    let before_fork = service(&h).snapshot().await.unwrap();
    let plugin_id = context.session_id();
    fork_session(&h, &cwd, &state.chat_container, &state, &context).await;
    assert_eq!(h.session().storage().metadata().id, id);
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("Could not fork session"));
    assert!(!text(&state).contains("unavailable (invalid_journal)"));
    assert_eq!(service(&h).snapshot().await.unwrap(), before_fork);
    assert_eq!(context.session_id(), plugin_id);
    assert_eq!(std::fs::read(source_path).unwrap(), bytes);

    // Import retains the source identity, unlike fork. Both the imported
    // file and this independent external source are synthetic test fixtures.
    let external = temp.path().join("external");
    let imported = crate::session::create_jsonl_session(&external, &cwd.to_string_lossy())
        .await
        .unwrap();
    let imported_id = imported.storage().metadata().id.clone();
    let other = AgentHarness::create(AgentHarnessOptions {
        session: imported,
        require_native_durable: true,
        ..Default::default()
    })
    .await
    .unwrap();
    enqueue(&other, "import-first").await;
    enqueue(&other, "import-second").await;
    other.close().await.unwrap();
    // `close` stops the harness but does not release the external storage
    // binding; import must observe a genuinely available source lease.
    drop(other);
    let rows = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &external)
        .await
        .unwrap();
    let import_path = &rows.iter().find(|r| r.id == imported_id).unwrap().path;
    let import_bytes = std::fs::read(import_path).unwrap();
    import_session(
        &h,
        &lane,
        import_path,
        &cwd,
        &state.chat_container,
        &state,
        &context,
        &catalog,
    )
    .await;
    assert_eq!(h.session().storage().metadata().id, imported_id);
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("queued=2, claimed=0"));
    assert!(!text(&state).contains("unavailable"));
    assert_eq!(std::fs::read(import_path).unwrap(), import_bytes);
    assert_eq!(std::fs::read(source_path).unwrap(), bytes);
    h.close().await.unwrap();
}

#[tokio::test]
async fn native_branch_navigation_refreshes_without_leaking_the_previous_queue() {
    use rpi_harness::agent_harness::NavigationOutcome;
    let h = AgentHarness::create(AgentHarnessOptions {
        session: memory("notice-branch"),
        require_native_durable: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let lane: Arc<dyn AgentLane> = h.lane("main");
    enqueue(&h, "branch-a").await;
    let leaf = h.view().get_leaf_id().await.unwrap().unwrap();
    let state = state(true);
    restore(&h, &state, &state.chat_container).await;
    assert_eq!(count(&state), 1);
    h.session().create_lane("bad", None).await.unwrap();
    h.session()
        .storage()
        .append_entry(
            rpi_harness::session::types::ProvisionedEntry {
                id: "bad-navigation-target".into(),
                kind: rpi_harness::session::types::ProvisionedKind::Custom {
                    custom_type: "durable-state".into(),
                    data: Some(json!({"private":"PRIVATE-legacy"})),
                },
            },
            "bad",
        )
        .await
        .unwrap();
    let before = service(&h).snapshot().await.unwrap();
    let entries = h
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap();
    // Exercise the same lane call and failure component path as NavigateTree,
    // without resetting/restoring a rejected view or driving a live key loop.
    state.history_page.invalidate();
    let navigation = lane
        .navigate_tree(Some("bad-navigation-target"), false, None, None)
        .await
        .unwrap();
    let NavigationOutcome::Failed { error, .. } = navigation.outcome else {
        panic!()
    };
    assert_eq!(error.code, "navigation_failed");
    crate::interactive_tui::add_error_message(&state.chat_container, &error.message);
    assert_eq!(count(&state), 1);
    assert!(text(&state).contains("explicit native import"));
    assert!(!text(&state).contains("PRIVATE-legacy"));
    assert!(!text(&state).contains("unavailable"));
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    assert_eq!(
        h.session()
            .storage()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap(),
        entries
    );
    for (target, expected) in [(None, 0), (Some(leaf.as_str()), 1)] {
        state.history_page.invalidate();
        let navigation = lane.navigate_tree(target, false, None, None).await.unwrap();
        assert!(matches!(
            navigation.outcome,
            NavigationOutcome::Completed { .. }
        ));
        state.reset_session_state();
        state.chat_container.clear();
        let before = service(&h).snapshot().await.unwrap();
        restore(&h, &state, &state.chat_container).await;
        assert_eq!(count(&state), expected);
        assert_eq!(service(&h).snapshot().await.unwrap(), before);
        if expected == 1 {
            assert!(text(&state).contains("queued=1, claimed=0"));
            assert!(service(&h)
                .request_outcome("branch-a")
                .await
                .unwrap()
                .is_some());
        } else {
            assert!(service(&h)
                .request_outcome("branch-a")
                .await
                .unwrap()
                .is_none());
        }
    }
    h.close().await.unwrap();
}
