//! #51: actual TUI components against a fixed experimental v2 harness.
//! Synthetic Memory/TempDir JSONL only. No keyboard/PTY, import or provider run.
use super::*;
use rpi_harness::{
    durable::{DurableActor, DurableService, ExperimentalDurablePolicy},
    session::{
        memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
        types::{EntryQuery, SessionMetadata},
        Session,
    },
    types::AgentHarnessOptions,
};
use serde_json::{json, Value};
const EPOCH: &str = "tui-epoch_51.test";
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
async fn experimental(session: Session, epoch: &str) -> AgentHarness {
    AgentHarness::create_experimental(
        AgentHarnessOptions {
            session,
            require_native_durable: true,
            ..Default::default()
        },
        ExperimentalDurablePolicy::new(epoch, vec!["native-cli".into()]).unwrap(),
    )
    .await
    .unwrap()
}
fn service(h: &AgentHarness) -> DurableService {
    DurableService::bind(h, DurableActor::owner("native-cli").unwrap())
}
async fn call(h: &AgentHarness, value: Value) -> Value {
    crate::native_durable::command(h, &value.to_string())
        .await
        .unwrap()
}
async fn enqueue(h: &AgentHarness, id: &str) -> Value {
    call(
        h,
        json!({"action":"enqueue","requestId":id,"text":"PRIVATE-queue-content"}),
    )
    .await
}
fn state(enabled: bool) -> Arc<TuiState> {
    let mut s = super::super::tests::test_tui_state();
    Arc::get_mut(&mut s).unwrap().history_page = HistoryUi::with_native_durable(enabled);
    s
}
fn text(s: &TuiState) -> String {
    strip_ansi(&s.chat_container.render(240).join("\n"))
}
fn count(s: &TuiState) -> usize {
    text(s).matches("Observed native durable state:").count()
}

#[tokio::test]
async fn registry_arguments_plain_note_and_wire_receipt_share_v2_state() {
    let h = experimental(memory("tui51-pipeline"), EPOCH).await;
    let empty = service(&h).snapshot().await.unwrap();
    assert!(super::super::build_session_command_registry(false)
        .find("/durable")
        .is_none());
    let registry = super::super::build_session_command_registry(true);
    let command = registry.find("/durable").unwrap();
    let business = "  keep  界  spaces\n\u{1b}\u{85}  end";
    let payload = json!({"action":"enqueue","requestId":"tui-queue","text":business});
    let raw = format!("{}  ", serde_json::to_string_pretty(&payload).unwrap());
    let args = slash_arguments(&format!(" /durable  {raw}"), command.as_ref());
    assert_eq!(args, raw);
    let view = crate::native_durable::interactive_command(&h, &args)
        .await
        .unwrap();
    assert!(view.contains("Mutation committed ACK"));
    assert!(view.contains("\\u001b\\u0085"));
    let chat = Arc::new(Container::new());
    add_note_message(&chat, &view);
    let rendered = strip_ansi(&chat.render(240).join("\n"));
    assert!(rendered.contains("tui-queue"));
    assert!(rendered.contains("Manual queue"));
    let before = service(&h).snapshot().await.unwrap();
    // Raw slash arguments are preserved above. Business normalization belongs
    // to the existing pure reducer (which trims outer whitespace), not the TUI.
    let expected = empty
        .state
        .apply(&serde_json::from_value(json!({"action":"enqueue","text":business})).unwrap())
        .unwrap();
    assert_eq!(expected.queue[0].text, business.trim());
    assert_eq!(before.state.queue[0].text, expected.queue[0].text);
    let receipt = service(&h).request_outcome("tui-queue").await.unwrap();
    assert!(receipt.is_some());
    let entries = h
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap();
    for arg in ["", "help", "runs", "runs-page null"] {
        let result = crate::native_durable::interactive_command(&h, arg)
            .await
            .unwrap();
        assert!(!result.is_empty());
    }
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    assert_eq!(
        service(&h).request_outcome("tui-queue").await.unwrap(),
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
    assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
    h.close().await.unwrap();
}

#[tokio::test]
async fn v2_notice_is_single_mounted_private_and_read_only_then_clears_when_terminal() {
    let h = experimental(memory("tui51-notice"), EPOCH).await;
    call(&h, json!({"action":"start","requestId":"start","name":"PRIVATE-workflow","steps":[{"id":"s","title":"PRIVATE-title"}]})).await;
    enqueue(&h, "queued").await;
    let s = state(true);
    let before = service(&h).snapshot().await.unwrap();
    let receipt = service(&h).request_outcome("queued").await.unwrap();
    let entries = h
        .session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap();
    let notice: Arc<dyn Component> = s.history_page.native_durable_notice.clone();
    for _ in 0..3 {
        restore(&h, &s, &s.chat_container).await;
        assert_eq!(count(&s), 1);
        assert!(text(&s).contains("workflow=running"));
        assert!(text(&s).contains("queued=1, claimed=0"));
        assert!(!text(&s).contains("PRIVATE-"));
        assert_eq!(
            s.chat_container
                .get_children()
                .iter()
                .filter(|x| Arc::ptr_eq(x, &notice))
                .count(),
            1
        );
    }
    assert_eq!(service(&h).snapshot().await.unwrap(), before);
    assert_eq!(
        service(&h).request_outcome("queued").await.unwrap(),
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
    call(&h, json!({"action":"reset","requestId":"reset"})).await;
    call(
        &h,
        json!({"action":"cancel","requestId":"cancel","item":"item-1"}),
    )
    .await;
    let terminal = service(&h).snapshot().await.unwrap();
    restore(&h, &s, &s.chat_container).await;
    assert_eq!(count(&s), 0);
    assert_eq!(service(&h).snapshot().await.unwrap(), terminal);
    h.close().await.unwrap();
}

#[tokio::test]
async fn real_v2_observation_cannot_cross_ui_generation_or_replaced_session_binding() {
    let h = experimental(memory("tui51-old-binding"), EPOCH).await;
    enqueue(&h, "old").await;
    let s = state(true);
    s.history_page.bind(&h);
    let observed = crate::native_durable::lifecycle_notice(&h, true).await;
    assert!(observed.as_ref().unwrap().contains("queued=1"));
    refresh_notice_from(&h, &s, &s.chat_container, async {
        s.history_page.invalidate();
        observed
    })
    .await;
    assert_eq!(count(&s), 0);
    s.history_page.bind(&h);
    let observed = crate::native_durable::lifecycle_notice(&h, true).await;
    refresh_notice_from(&h, &s, &s.chat_container, async {
        h.set_session(memory("tui51-new-binding")).await.unwrap();
        observed
    })
    .await;
    assert_eq!(count(&s), 0);
    assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
    assert!(service(&h).request_outcome("old").await.unwrap().is_none());
    enqueue(&h, "new-1").await;
    enqueue(&h, "new-2").await;
    s.reset_session_state();
    restore(&h, &s, &s.chat_container).await;
    assert_eq!(count(&s), 1);
    assert!(text(&s).contains("queued=2, claimed=0"));
    h.close().await.unwrap();
}

#[tokio::test]
async fn actual_tui_new_and_switch_restore_same_epoch_jsonl_notice_without_rewriting_source() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temp.path().join("sessions");
    let session = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
        .await
        .unwrap();
    let id = session.storage().metadata().id.clone();
    let h = experimental(session, EPOCH).await;
    enqueue(&h, "source").await;
    let receipt = service(&h).request_outcome("source").await.unwrap();
    let rows = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
        .await
        .unwrap();
    let path = &rows.iter().find(|x| x.id == id).unwrap().path;
    let bytes = std::fs::read(path).unwrap();
    let mut s = state(true);
    Arc::get_mut(&mut s).unwrap().session_dir = root.clone();
    let lane: Arc<dyn AgentLane> = h.lane("main");
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    let catalog = Arc::new(std::sync::RwLock::new(Vec::new()));
    restore(&h, &s, &s.chat_container).await;
    assert_eq!(count(&s), 1);
    assert!(start_new_session(&h, &lane, &cwd, &s.chat_container, &s, &context).await);
    assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
    assert_ne!(h.session().storage().metadata().id, id);
    assert_eq!(count(&s), 0);
    assert!(service(&h)
        .request_outcome("source")
        .await
        .unwrap()
        .is_none());
    enqueue(&h, "new-session-v2").await;
    assert!(
        switch_to_session(
            &h,
            &lane,
            &id,
            &cwd,
            &s.chat_container,
            &s,
            &context,
            &catalog
        )
        .await
    );
    assert_eq!(count(&s), 1);
    assert!(text(&s).contains("queued=1, claimed=0"));
    assert_eq!(
        service(&h).request_outcome("source").await.unwrap(),
        receipt
    );
    assert!(service(&h)
        .request_outcome("new-session-v2")
        .await
        .unwrap()
        .is_none());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
    h.close().await.unwrap();
}

#[tokio::test]
async fn actual_tui_switch_refuses_other_epoch_and_v1_without_replacing_scope_or_notice() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = temp.path().join("sessions");
    let h = experimental(
        crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
            .await
            .unwrap(),
        EPOCH,
    )
    .await;
    enqueue(&h, "source").await;
    let mut s = state(true);
    Arc::get_mut(&mut s).unwrap().session_dir = root.clone();
    let lane: Arc<dyn AgentLane> = h.lane("main");
    let context = rpi_extensions::ToolCallContext::new(cwd.to_string_lossy());
    let catalog = Arc::new(std::sync::RwLock::new(Vec::new()));
    restore(&h, &s, &s.chat_container).await;
    let before = service(&h).snapshot().await.unwrap();
    let plugin_id = context.session_id();
    for epoch in [Some("foreign-epoch"), None] {
        let session = crate::session::create_jsonl_session(&root, &cwd.to_string_lossy())
            .await
            .unwrap();
        let id = session.storage().metadata().id.clone();
        let other = if let Some(epoch) = epoch {
            experimental(session, epoch).await
        } else {
            AgentHarness::create(AgentHarnessOptions {
                session,
                require_native_durable: true,
                ..Default::default()
            })
            .await
            .unwrap()
        };
        enqueue(&other, "foreign").await;
        other.close().await.unwrap();
        let rows = crate::session::list_session_metadata_in_dir(&cwd.to_string_lossy(), &root)
            .await
            .unwrap();
        let path = &rows.iter().find(|x| x.id == id).unwrap().path;
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !switch_to_session(
                &h,
                &lane,
                &id,
                &cwd,
                &s.chat_container,
                &s,
                &context,
                &catalog
            )
            .await
        );
        assert_eq!(service(&h).snapshot().await.unwrap(), before);
        assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
        assert_eq!(context.session_id(), plugin_id);
        assert_eq!(count(&s), 1);
        assert!(text(&s).contains("queued=1, claimed=0"));
        assert!(!text(&s).contains("unavailable (invalid_journal)"));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    h.close().await.unwrap();
}

#[tokio::test]
async fn v2_navigation_restore_observes_selected_branch_not_latest_receipt() {
    let h = experimental(memory("tui51-navigation"), EPOCH).await;
    let start = call(&h, json!({"action":"start","requestId":"start","name":"test","steps":[{"id":"s","title":"test"}]})).await;
    let queued = enqueue(&h, "queued").await;
    let s = state(true);
    restore(&h, &s, &s.chat_container).await;
    assert!(text(&s).contains("queued=1, claimed=0"));
    let lane = h.lane("main");
    for (entry, expected) in [
        (
            start["outcome"]["receipt"]["entryId"].as_str().unwrap(),
            "queued=0, claimed=0",
        ),
        (
            queued["outcome"]["receipt"]["entryId"].as_str().unwrap(),
            "queued=1, claimed=0",
        ),
    ] {
        let result = lane
            .navigate_tree(Some(entry), false, None, None)
            .await
            .unwrap();
        assert!(matches!(
            result.outcome,
            rpi_harness::agent_harness::NavigationOutcome::Completed { .. }
        ));
        s.reset_session_state();
        let before = service(&h).snapshot().await.unwrap();
        restore(&h, &s, &s.chat_container).await;
        assert_eq!(count(&s), 1);
        assert!(text(&s).contains(expected), "{}", text(&s));
        assert!(text(&s).contains("workflow=running"));
        assert_eq!(service(&h).snapshot().await.unwrap(), before);
        assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
    }
    h.close().await.unwrap();
}

#[tokio::test]
async fn disabled_v2_view_never_polls_and_closed_enabled_view_is_not_empty() {
    let h = experimental(memory("tui51-closed"), EPOCH).await;
    let s = state(false);
    s.history_page.bind(&h);
    refresh_notice_from(&h, &s, &s.chat_container, async {
        panic!("disabled experimental view must not poll a read");
        #[allow(unreachable_code)]
        None
    })
    .await;
    assert_eq!(count(&s), 0);
    h.close().await.unwrap();
    let enabled = state(true);
    enabled.history_page.bind(&h);
    refresh_native_durable_notice(&h, &enabled, &enabled.chat_container).await;
    assert_eq!(count(&enabled), 1);
    assert!(text(&enabled).contains("unavailable (closed)"));
    assert!(text(&enabled).contains("not an empty-state observation"));
    assert_eq!(h.experimental_durable_epoch().as_deref(), Some(EPOCH));
}
