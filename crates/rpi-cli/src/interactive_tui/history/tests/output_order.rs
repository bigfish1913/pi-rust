//! The host owns transcript placement, independent of tool or message format.
use super::*;
use crate::transcript_view::UiEvent;
use rpi_tui::AssistantBlock;

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
    let first = output
        .find(before)
        .unwrap_or_else(|| panic!("missing {before}: {output}"));
    let second = output
        .find(after)
        .unwrap_or_else(|| panic!("missing {after}: {output}"));
    assert!(first < second, "{before} must precede {after}: {output}");
}

fn todo(done: bool, is_error: bool) -> UiEvent {
    UiEvent::ToolEnd {
        id: format!("todo-{done}-{is_error}"),
        name: "todo".into(),
        result: serde_json::json!({
            "content": [{"type":"text", "text":"synthetic-todo-error"}],
            "details":{"todos":[{"id":1,"text":"ordered task body","done":done}]}
        }),
        is_error,
    }
}

#[tokio::test]
async fn managed_todo_updates_and_errors_stay_before_later_replies() {
    let (_, state, _) = fixture(0).await;
    restore_bound(&state, &state.chat_container).await;
    state.transcript_view().add_user("before tasks");
    state.transcript_view().apply(&todo(false, false), 100);
    assistant(&state, "work after tasks");
    state.transcript_view().apply(&todo(true, false), 100);
    state.transcript_view().apply(&todo(false, true), 100);
    state.transcript_view().add_user("after tasks and error");
    assistant(&state, "later assistant reply");
    for expanded in [false, true] {
        if expanded {
            state.toggle_tool_outputs();
        }
        for regular in [false, true] {
            let output = if regular {
                rpi_tui::render::with_main_screen_frame(|| text(&state))
            } else {
                text(&state)
            };
            assert_order(&output, "before tasks", "1/1 completed");
            assert_order(&output, "1/1 completed", "work after tasks");
            assert_order(&output, "work after tasks", "synthetic-todo-error");
            assert_order(&output, "synthetic-todo-error", "after tasks and error");
            assert_order(&output, "after tasks and error", "later assistant reply");
            assert_eq!(output.matches("1/1 completed").count(), 1);
        }
    }
    let output = text(&state);
    assert_order(&output, "ordered task body", "work after tasks");
    assistant(
        &state,
        &(0..40)
            .map(|i| format!("tail-{i:02}\n\n"))
            .collect::<String>(),
    );
    let scroll = ScrollView::new(
        state.chat_container.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            scrollbar: ScrollbarMode::Hidden,
            ..Default::default()
        },
    );
    let tail = strip_ansi(&scroll.render_with_viewport(100, 8).join("\n"));
    assert!(tail.contains("tail-39"), "{tail}");
    assert!(!tail.contains("completed"), "{tail}");
}

#[tokio::test]
async fn restored_todos_use_entry_order_and_retire_with_their_page() {
    let (storage, state, tui) = fixture(0).await;
    for (index, kind, label) in [
        (0, "todo", "old task body"),
        (2, "todo-state", "new task body"),
    ] {
        append(
            &storage,
            index,
            ProvisionedKind::Custom {
                custom_type: kind.into(),
                data: Some(serde_json::json!({"todos":[{"text":label,"done":false}]})),
            },
        )
        .await;
        append(
            &storage,
            index + 1,
            ProvisionedKind::Message {
                message: user_message_with_images(&format!("after-task-{index}"), vec![]),
                terminate: None,
            },
        )
        .await;
    }
    restore_bound(&state, &state.chat_container).await;
    let restored = text(&state);
    assert!(!restored.contains("old task body"));
    assert_eq!(restored.matches("new task body").count(), 1);
    assert_order(&restored, "new task body", "after-task-0");
    assert_order(&restored, "after-task-0", "after-task-2");
    assistant(&state, "live after restored tasks");
    state.apply_tool_end(&todo(true, false), 100);
    let updated = text(&state);
    assert!(!updated.contains("new task body"));
    assert_eq!(updated.matches("ordered task body").count(), 1);
    assert_order(&updated, "1/1 completed", "after-task-0");
    assert_order(&updated, "after-task-2", "live after restored tasks");
    for index in 4..65 {
        append(
            &storage,
            index,
            ProvisionedKind::Message {
                message: user_message_with_images(&format!("newer-{index}"), vec![]),
                terminate: None,
            },
        )
        .await;
    }
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("task body"));
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    assert_order(&text(&state), "new task body", "after-task-0");
    request(state.clone(), tui, Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("task body"));
}

#[tokio::test]
async fn history_failure_notice_scrolls_above_subsequent_output() {
    let (_, state, tui) = fixture(80).await;
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    state.transcript_view().add_user("before history failure");
    state
        .history_page
        .resident
        .lock()
        .unwrap()
        .page
        .as_mut()
        .unwrap()
        .next_before_id = Some("invalid-cursor".into());
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    state.transcript_view().add_user("after history failure");
    let output = text(&state);
    assert_order(&output, "before history failure", "Could not load history");
    assert_order(&output, "Could not load history", "after history failure");
    // Successful navigation removes the error even if a running reply keeps
    // the live container alive instead of clearing it at settlement.
    state.set_status(RunStatus::Working);
    request(state.clone(), tui.clone(), Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("Could not load history"));
    assert!(text(&state).contains("after history failure"));
    state.set_status(RunStatus::Idle);
    request(state.clone(), tui.clone(), Direction::Older);
    finish_request(&state).await;
    state
        .history_page
        .resident
        .lock()
        .unwrap()
        .page
        .as_mut()
        .unwrap()
        .next_after_id = Some("invalid-cursor".into());
    request(state.clone(), tui.clone(), Direction::Newer);
    finish_request(&state).await;
    assert!(text(&state).contains("Could not load history"));
    state
        .transcript_view()
        .add_user("detached prompt on older page");
    assert!(!text(&state).contains("detached prompt on older page"));
    request(state.clone(), tui, Direction::Latest);
    finish_request(&state).await;
    assert!(!text(&state).contains("Could not load history"));
}

#[tokio::test]
async fn ordinary_tool_lifecycles_remain_in_transcript_order() {
    for name in [
        "read",
        "write",
        "edit",
        "bash",
        "docs",
        "custom_extension_tool",
    ] {
        let (_, state, _) = fixture(0).await;
        restore_bound(&state, &state.chat_container).await;
        let view = state.transcript_view();
        view.set_expanded(true);
        view.add_user("before ordinary tool");
        let args = serde_json::json!({"path":"example.txt","command":"echo synthetic"});
        let result = serde_json::json!({"content":[{"type":"text","text":"ordinary tool output"}]});
        view.apply(
            &UiEvent::ToolStart {
                id: name.into(),
                name: name.into(),
                args: args.clone(),
            },
            100,
        );
        view.apply(
            &UiEvent::ToolUpdate {
                id: name.into(),
                name: name.into(),
                args,
                result: result.clone(),
            },
            100,
        );
        view.apply(
            &UiEvent::ToolEnd {
                id: name.into(),
                name: name.into(),
                result,
                is_error: false,
            },
            100,
        );
        view.add_user("after ordinary tool");
        for regular in [false, true] {
            let output = if regular {
                rpi_tui::render::with_main_screen_frame(|| text(&state))
            } else {
                text(&state)
            };
            assert_order(&output, "before ordinary tool", "ordinary tool output");
            assert_order(&output, "ordinary tool output", "after ordinary tool");
        }
    }
}

#[tokio::test]
async fn commands_and_extension_messages_use_the_visible_transcript_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let home = crate::config::AgentHome::from_option(Some(tmp.path().to_path_buf())).unwrap();
    let resolved = crate::provider::resolve(
        &home,
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let args = crate::args::Args {
        trust_override: Some(false),
        no_session: true,
        no_extensions: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("test context".into()),
        ..Default::default()
    };
    let (harness, _events, reload) =
        crate::session::build(&home, &resolved, &args, tmp.path(), false)
            .await
            .unwrap();
    let session = Arc::new(crate::agent_session::AgentSession::new(
        harness,
        vec![],
        vec![],
        tmp.path(),
    ));
    let (_, initial_state, tui) = fixture(80).await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut ctx = CommandContext {
        tui: tui.clone(),
        tx,
        editor: initial_state.editor.clone(),
        editor_container: Arc::new(Container::new()),
        lane: session.harness().lane("main"),
        model_catalog: Arc::new(std::sync::RwLock::new(vec![])),
        cwd: tmp.path().to_path_buf(),
        resources: Arc::new(Mutex::new(Default::default())),
        startup_context: None,
        reload_context: Arc::new(reload),
        session,
        streaming: false,
        state: initial_state,
    };
    let registry = build_builtin_registry();
    for page in [None, Some(Direction::Latest), Some(Direction::Older)] {
        let (_, state, _) = fixture(80).await;
        if let Some(direction) = page {
            request(state.clone(), tui.clone(), Direction::Latest);
            finish_request(&state).await;
            if direction == Direction::Older {
                request(state.clone(), tui.clone(), direction);
                finish_request(&state).await;
            }
        } else {
            state.history_page.reset();
        }
        ctx.editor = state.editor.clone();
        ctx.state = state.clone();
        for (name, args) in [
            ("/help", ""),
            ("/version", ""),
            ("/changelog", ""),
            ("/changelog", "all"),
            ("/changelog", "invalid"),
            ("/hotkeys", ""),
            ("/skills", ""),
        ] {
            let slot = state.notice_container();
            let count = slot.child_count();
            registry.find(name).unwrap().execute(&ctx, args);
            assert!(
                slot.child_count() > count,
                "{name} {args} must write to the host-selected slot on {page:?}"
            );
        }
        for value in [
            serde_json::json!({"kind":"message","text":"plain extension output"}),
            serde_json::json!({"kind":"message","markdown":true,"text":"**Markdown extension output**"}),
            serde_json::json!({"markdown":true,"text":"default-kind output"}),
            serde_json::json!({"kind":"message","markdown":true,"lines":["line-based extension output"]}),
        ] {
            let slot = state.notice_container();
            let count = slot.child_count();
            handle_extension_ui_result(
                Some(value.clone()),
                &ctx,
                state.extension_session.clone(),
                "test-command".into(),
            );
            assert!(
                slot.child_count() > count,
                "{value} must write to the host-selected slot on {page:?}"
            );
        }
        let slot = state.notice_container();
        let count = slot.child_count();
        handle_extension_ui_result(
            Some(serde_json::Value::Null),
            &ctx,
            state.extension_session.clone(),
            "cancelled-command".into(),
        );
        assert_eq!(slot.child_count(), count, "cancellation is not a message");
        handle_extension_ui_result(
            None,
            &ctx,
            state.extension_session.clone(),
            "failed-command".into(),
        );
        assert!(slot.child_count() > count);
        if page != Some(Direction::Older) {
            state
                .transcript_view()
                .add_user("reply after command outputs");
            for regular in [false, true] {
                let output = if regular {
                    rpi_tui::render::with_main_screen_frame(|| text(&state))
                } else {
                    text(&state)
                };
                assert_order(
                    &output,
                    "Markdown extension output",
                    "reply after command outputs",
                );
                assert_order(
                    &output,
                    "Extension command failed",
                    "reply after command outputs",
                );
            }
        } else {
            request(state.clone(), tui.clone(), Direction::Latest);
            finish_request(&state).await;
            assert!(!text(&state).contains("extension output"));
        }
    }

    // The command snapshot exposes a skill absent from the real harness: the
    // activation notice is exercised, but the async run fails before any
    // provider call. No extension discovery or network request is needed.
    let state = super::super::super::tests::test_tui_state();
    ctx.editor = state.editor.clone();
    ctx.state = state.clone();
    ctx.resources.lock().unwrap().skills = Some(vec![rpi_harness::types::Skill {
        name: "output-order-skill".into(),
        description: "synthetic".into(),
        content: String::new(),
        file_path: "unused/SKILL.md".into(),
        disable_model_invocation: None,
    }]);
    // Bind the managed live slot without changing the harness's skill registry.
    let (storage, _, _) = fixture(0).await;
    state.history_page.resident.lock().unwrap().storage = Some(storage);
    state.history_page.mount_active(&state.chat_container);
    registry
        .find("/skills")
        .unwrap()
        .execute(&ctx, "output-order-skill");
    state.transcript_view().add_user("after skill activation");
    assert_order(
        &text(&state),
        "Activating output-order-skill",
        "after skill activation",
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while *state.status.lock().unwrap() == RunStatus::Working {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn custom_message_renderers_and_fallbacks_share_the_live_transcript_slot() {
    extern "C" fn render(
        _input: rpi_plugin_sdk::StbStringRef,
        output: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: invoke_extension_renderer lends an initialized, exclusive
        // output slot and reclaims the SDK string with the matching callback.
        let mut output = unsafe { rpi_plugin_sdk::callbacks::Output::from_raw(output) };
        output.write(r#"{"text":"rendered custom event","markdown":true}"#.into());
        0
    }
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    for registered in [false, true] {
        let (_, state, tui) = fixture(80).await;
        restore_bound(&state, &state.chat_container).await;
        if registered {
            let mut registry = rpi_extensions::ExtensionRegistry::new();
            registry.register_renderer(rpi_extensions::RegisteredRenderer {
                name: "test-message-renderer".into(),
                kind: rpi_extensions::RegisteredRendererKind::Message,
                render_fn: render,
                plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                user_data: std::ptr::null_mut(),
            });
            *state.extension_session.lock().unwrap() = rpi_extensions::ExtensionSession::from_parts(
                Arc::new(registry.snapshot()),
                Arc::new(rpi_extensions::PluginKeepalive::new(vec![], None)),
                vec![],
                None,
            );
        }
        let event = || AgentEvent::MessageStart {
            message_id: None,
            message: AgentMessage::Custom(rpi_agent::CustomMessage::new(
                "ordered-custom",
                vec![],
                serde_json::json!({}),
                0,
            )),
        };
        let marker = if registered {
            "rendered custom event"
        } else {
            "ordered-custom"
        };
        state.transcript_view().add_user("before custom event");
        handle_agent_event(event(), &tui, &state, &lane).await;
        state.transcript_view().add_user("after custom event");
        for regular in [false, true] {
            let output = if regular {
                rpi_tui::render::with_main_screen_frame(|| text(&state))
            } else {
                text(&state)
            };
            assert_order(&output, "before custom event", marker);
            assert_order(&output, marker, "after custom event");
        }
        state.set_status(RunStatus::Working);
        request(state.clone(), tui.clone(), Direction::Older);
        finish_request(&state).await;
        let older = text(&state);
        handle_agent_event(event(), &tui, &state, &lane).await;
        assert_eq!(
            text(&state),
            older,
            "a live custom message must not mutate an older page"
        );
        request(state.clone(), tui, Direction::Latest);
        finish_request(&state).await;
        assert_eq!(text(&state).matches(marker).count(), 2);
    }
}
