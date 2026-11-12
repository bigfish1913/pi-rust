//! Synthetic update cards and streaming components only; no network/user data.
use super::*;

fn report() -> crate::updates::UpdateReport {
    crate::updates::UpdateReport {
        notices: vec![crate::updates::UpdateNotice {
            name: "rpi-im-message".into(),
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            command: "rpi extension update rpi-im-message".into(),
        }],
        warnings: Vec::new(),
    }
}

#[test]
fn startup_update_card_scrolls_above_streaming_output_even_if_check_finishes_late() {
    for width in [28, 100] {
        for late_notice in [false, true] {
            let history = HistoryUi::default();
            let chat = Arc::new(Container::new());
            // Reserve the slot before the async check has any content.
            history.mount_active(&chat);
            let scroll = ScrollView::new(
                chat.clone(),
                ScrollViewOptions {
                    follow: FollowMode::End,
                    scrollbar: ScrollbarMode::Hidden,
                    ..Default::default()
                },
            );
            let assistant = Arc::new(AssistantMessageComponent::new(
                AssistantMessageOptions::default(),
            ));
            assistant.set_streaming(true);
            if late_notice {
                history.active.add_child(assistant.clone());
                assistant.update_blocks(&[AssistantBlock::Text("first-chunk".into())]);
                let _ = scroll.render_with_viewport(width, 8);
            }
            add_update_notices(&history.updates, &report());
            if !late_notice {
                history.active.add_child(assistant.clone());
            }
            let mut output = String::new();
            for index in 0..40 {
                output.push_str(&format!("stream-line-{index:02}\n\n"));
                assistant.update_blocks(&[AssistantBlock::Text(output.clone())]);
                let full = strip_ansi(&chat.render(width).join("\n"));
                let card = full.find("Package Update Available").unwrap();
                let stream = full.find("stream-line-00").unwrap();
                assert!(
                    card < stream,
                    "notice must precede streaming output: {full}"
                );
                let _ = scroll.render_with_viewport(width, 8);
            }
            let tail = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
            assert!(tail.contains("stream-line-39"), "{tail}");
            assert!(!tail.contains("Package Update Available"), "{tail}");
            // The box scrolled into history, rather than being deleted or pinned.
            scroll.scroll_to_start();
            let top = strip_ansi(&scroll.render_with_viewport(width, 8).join("\n"));
            assert!(top.contains("Package Update Available"), "{top}");
            assert!(!top.contains("stream-line-39"), "{top}");
        }
    }
}

#[tokio::test]
async fn update_card_survives_history_rebuilds_once_before_the_page_and_live_slot() {
    let (storage, state, tui) = fixture(80).await;
    restore_bound(&state, &state.chat_container).await;
    add_update_notices(&state.history_page.updates, &report());
    state.history_page.regular_update_hint.update(&report());
    for direction in [Direction::Older, Direction::Latest] {
        request(state.clone(), tui.clone(), direction);
        finish_request(&state).await;
        let content = text(&state);
        assert_live_hint_once(&state.chat_container, &state.history_page);
        assert_eq!(content.matches("Package Update Available").count(), 1);
        assert!(content.find("Package Update Available") < content.find("history-entry-"));
    }
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("live-user-after-update");
    let content = text(&state);
    assert!(content.find("Package Update Available") < content.find("live-user-after-update"));
    append(
        &storage,
        80,
        ProvisionedKind::Message {
            message: AgentMessage::User(UserMessage {
                role: Default::default(),
                timestamp: 80,
                content: rpi_ai::types::UserContent::Text("settled-user-after-update".into()),
            }),
            terminate: None,
        },
    )
    .await;
    state.set_status(RunStatus::Idle);
    settle(&state, &tui);
    finish_request(&state).await;
    restore_bound(&state, &state.chat_container).await;
    let content = text(&state);
    assert_live_hint_once(&state.chat_container, &state.history_page);
    assert_eq!(content.matches("Package Update Available").count(), 1);
    assert!(content.find("Package Update Available") < content.find("settled-user-after-update"));
    assert!(!content.contains("live-user-after-update"));
}

#[test]
fn reset_clears_startup_update_cards_and_repeated_mounts_do_not_duplicate_them() {
    let history = HistoryUi::default();
    let chat = Arc::new(Container::new());
    for _ in 0..3 {
        history.mount_active(&chat);
    }
    add_update_notices(&history.updates, &report());
    assert_eq!(chat.child_count(), 4); // Includes the reserved live hint.
    assert_eq!(
        strip_ansi(&chat.render(100).join("\n"))
            .matches("Package Update Available")
            .count(),
        1
    );
    history.reset();
    assert!(!strip_ansi(&chat.render(100).join("\n")).contains("Package Update Available"));
}

fn regular_rows(component: &dyn Component, width: usize) -> Vec<String> {
    rpi_tui::render::with_main_screen_frame(|| component.render(width))
}

fn assert_live_hint_once(chat: &Arc<Container>, history: &HistoryUi) {
    let hint: Arc<dyn Component> = history.regular_update_hint.clone();
    let children = chat.get_children();
    assert_eq!(
        children
            .iter()
            .filter(|child| Arc::ptr_eq(child, &hint))
            .count(),
        1
    );
    let position = children
        .iter()
        .position(|child| Arc::ptr_eq(child, &hint))
        .unwrap();
    let panels: Arc<dyn Component> = history.panels.clone();
    assert!(Arc::ptr_eq(&children[position - 1], &panels));
    let rows = regular_rows(chat.as_ref(), 100).join("\n");
    assert_eq!(
        rows.matches("Update: rpi-im-message 0.2.0 available")
            .count(),
        1
    );
    assert!(rows.rfind("Update: ") > rows.rfind("history-entry-"));
}

#[test]
fn live_hint_is_regular_only_and_keeps_the_typed_status_and_action() {
    let hint = RegularUpdateHint::default();
    assert!(regular_rows(&hint, 100).is_empty());
    hint.update(&report());
    assert!(
        hint.render(100).is_empty(),
        "fullscreen/ordinary renders must be unchanged"
    );
    let rows = regular_rows(&hint, 100);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("0.2.0 available"));
    assert!(rows[0].contains("rpi extension update rpi-im-message"));
    assert!(
        hint.render(100).is_empty(),
        "the regular frame scope must not leak"
    );
    hint.update(&crate::updates::UpdateReport::default());
    assert!(regular_rows(&hint, 100).is_empty());
}

#[test]
fn live_hint_prioritizes_warning_and_reserves_action_space_when_truncated() {
    let hint = RegularUpdateHint::default();
    let mut report = report();
    report.warnings.push(crate::updates::UpdateWarning {
        message: format!("\x1b[31mfailed\x1b[0m\n\r\t\x07{}", "界".repeat(200)),
        command: "rpi\nupdate\x1b[0m".into(),
    });
    hint.update(&report);
    for width in [0, 1, 2, 3, 8, 28, 100] {
        let rows = regular_rows(&hint, width);
        assert_eq!(rows.len(), usize::from(width > 0));
        for row in rows {
            assert!(rpi_tui::visible_width(&row) <= width, "{width}: {row:?}");
            assert!(!row.chars().any(char::is_control), "{row:?}");
        }
    }
    let row = regular_rows(&hint, 100).remove(0);
    assert!(row.starts_with("Update failed: failed"), "{row}");
    assert!(row.contains('…'), "{row}");
    assert!(row.ends_with(" | rpi update"), "{row}");
    assert!(
        !row.contains("rpi-im-message"),
        "only one notice is summarized"
    );
}

#[test]
fn live_hint_reserves_once_after_panels_before_durable_and_reset_clears_it() {
    let history = HistoryUi::default();
    let chat = Arc::new(Container::new());
    add_note_message(&history.native_durable_notice, "durable-last");
    for _ in 0..3 {
        history.mount_active(&chat);
    }
    // Arrives after the slot has already been rendered empty.
    assert!(!regular_rows(chat.as_ref(), 100)
        .join("\n")
        .contains("Update: "));
    add_update_notices(&history.updates, &report());
    history.regular_update_hint.update(&report());
    for _ in 0..3 {
        history.mount_active(&chat);
    }
    assert_eq!(chat.child_count(), 5);
    assert_live_hint_once(&chat, &history);
    let regular = regular_rows(chat.as_ref(), 100).join("\n");
    assert!(regular.find("Update: ") < regular.find("durable-last"));
    assert_eq!(regular.matches("Package Update Available").count(), 1);
    assert!(!chat.render(100).join("\n").contains("Update: "));
    history.reset();
    assert!(regular_rows(chat.as_ref(), 100).is_empty());
    history.mount_active(&chat);
    assert!(regular_rows(chat.as_ref(), 100).is_empty());
}

#[test]
fn live_hint_prefers_cli_release_over_package_release() {
    let hint = RegularUpdateHint::default();
    let mut report = report();
    report.notices.push(crate::updates::UpdateNotice {
        name: "rpi".into(),
        current: "1.0.0".into(),
        latest: "2.0.0".into(),
        command: "rpi update".into(),
    });
    hint.update(&report);
    let rows = regular_rows(&hint, 100);
    assert_eq!(rows, ["Update: rpi 2.0.0 available | rpi update"]);
}
