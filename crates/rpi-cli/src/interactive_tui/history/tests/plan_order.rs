//! Plans belong to transcript order, not the persistent current-task footer.
use super::*;
use crate::transcript_view::UiEvent;
use rpi_tui::AssistantBlock;

fn plan_event(active: bool) -> UiEvent {
    UiEvent::ToolEnd {
        id: if active {
            "plan-start"
        } else {
            "plan-complete"
        }
        .into(),
        name: if active {
            "plan_mode_start"
        } else {
            "plan_mode_complete"
        }
        .into(),
        result: serde_json::json!({"details":{
            "kind":"plan", "active":active, "plan":"Implementation checklist"
        }}),
        is_error: false,
    }
}

fn assistant(state: &TuiState, message: &str) {
    let blocks = vec![AssistantBlock::Text(message.into())];
    state.transcript_view().apply(
        &UiEvent::AssistantStart {
            blocks: blocks.clone(),
        },
        100,
    );
    state
        .transcript_view()
        .apply(&UiEvent::AssistantEnd { blocks }, 100);
}

fn assert_order(output: &str, before: &str, after: &str) {
    let before_row = output
        .find(before)
        .unwrap_or_else(|| panic!("missing {before}: {output}"));
    let after_row = output
        .find(after)
        .unwrap_or_else(|| panic!("missing {after}: {output}"));
    assert!(
        before_row < after_row,
        "{before} must precede {after}: {output}"
    );
}

#[tokio::test]
async fn managed_live_plan_ready_does_not_trail_later_messages() {
    let (_, state, _) = fixture(0).await;
    restore_bound(&state, &state.chat_container).await;
    state.set_status(RunStatus::Working);
    state.transcript_view().add_user("planning request");
    state.transcript_view().apply(&plan_event(true), 100);
    assistant(&state, "repository findings");
    state.transcript_view().apply(&plan_event(false), 100);
    state.transcript_view().add_user("implement the plan");
    assistant(&state, "implementation progress");

    for regular in [false, true] {
        let output = if regular {
            rpi_tui::render::with_main_screen_frame(|| text(&state))
        } else {
            text(&state)
        };
        assert_order(&output, "planning request", "Plan · Ready");
        assert_order(&output, "Plan · Ready", "repository findings");
        assert_order(&output, "repository findings", "implement the plan");
        assert_order(&output, "implement the plan", "implementation progress");
        assert_eq!(output.matches("Plan · Ready").count(), 1);
    }
    assert!(state.toggle_tool_outputs());
    let expanded = rpi_tui::render::with_main_screen_frame(|| text(&state));
    assert_order(&expanded, "Implementation checklist", "repository findings");
    assert_order(&expanded, "repository findings", "implementation progress");
    assert!(strip_ansi(&state.history_page.active.render(100).join("\n")).contains("Plan · Ready"));
}

#[tokio::test]
async fn restored_plan_stays_in_history_and_does_not_follow_every_page() {
    let (storage, state, tui) = fixture(0).await;
    for (index, active) in [(0, true), (2, false)] {
        append(
            &storage,
            index,
            ProvisionedKind::Custom {
                custom_type: "plan-mode".into(),
                data: Some(serde_json::json!({"active":active,"plan":"saved checklist"})),
            },
        )
        .await;
        append(
            &storage,
            index + 1,
            ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage {
                    role: Default::default(),
                    timestamp: index as i64,
                    content: rpi_ai::types::UserContent::Text(format!("after-plan-{index}")),
                }),
                terminate: None,
            },
        )
        .await;
    }
    restore_bound(&state, &state.chat_container).await;
    let output = text(&state);
    assert_order(&output, "Plan · Ready", "after-plan-0");
    assert_order(&output, "after-plan-0", "after-plan-2");
    assert_eq!(output.matches("Plan · Ready").count(), 1);
    assistant(&state, "next live reply");
    assert_order(&text(&state), "after-plan-2", "next live reply");

    // Once the plan has left the bounded history page, it must not remain as
    // a ready-state footer. Its persisted entry is still available on older pages.
    for index in 4..65 {
        append(
            &storage,
            index,
            ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage {
                    role: Default::default(),
                    timestamp: index as i64,
                    content: rpi_ai::types::UserContent::Text(format!("later-message-{index}")),
                }),
                terminate: None,
            },
        )
        .await;
    }
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("Plan · Ready"));
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert_order(&text(&state), "Plan · Ready", "after-plan-0");
    request(state.clone(), tui, Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("Plan · Ready"));
}
