use super::*;
use rpi_tui::Component;

/// A draft injected from a voice transcription is submitted only when it is
/// still exactly what we put in the editor and no run has started; any
/// user edit (or a cleared editor) hands it back for editing instead.
#[test]
fn auto_send_cancels_when_the_user_edits_the_draft() {
    let now = std::time::Instant::now();
    let pending = AutoSendPending {
        deadline: now + std::time::Duration::from_secs(2),
        text: "hello world".to_string(),
    };
    // Untouched → still counting down.
    assert!(matches!(
        auto_send_step(Some(&pending), "hello world", true, now),
        AutoSendStep::Wait(_)
    ));
    // One added character is the user's correction: never send a draft
    // they are actively editing.
    assert_eq!(
        auto_send_step(Some(&pending), "hello worlds", true, now),
        AutoSendStep::Cancel
    );
    // Cleared out from under us (a slash command, Esc).
    assert_eq!(
        auto_send_step(Some(&pending), "", true, now),
        AutoSendStep::Cancel
    );
    // Whitespace is not content.
    assert_eq!(
        auto_send_step(Some(&pending), "   ", true, now),
        AutoSendStep::Cancel
    );
}

/// The countdown fires exactly at the deadline, and never injects a prompt
/// into a run that started while it was ticking.
#[test]
fn auto_send_submits_at_the_deadline_only_while_idle() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let pending = AutoSendPending {
        deadline,
        text: "go".to_string(),
    };
    assert!(matches!(
        auto_send_step(
            Some(&pending),
            "go",
            true,
            deadline - std::time::Duration::from_millis(1)
        ),
        AutoSendStep::Wait(_)
    ));
    assert_eq!(
        auto_send_step(Some(&pending), "go", true, deadline),
        AutoSendStep::Submit
    );
    // A turn started underneath the countdown — keep the draft, do not
    // queue a prompt mid-turn.
    assert_eq!(
        auto_send_step(Some(&pending), "go", false, deadline),
        AutoSendStep::Cancel
    );
    // Nothing pending.
    assert_eq!(
        auto_send_step(None, "go", true, deadline),
        AutoSendStep::Idle
    );
}

/// The footer text the countdown writes is prefixed so `clear_auto_send`
/// can tell its own message from an unrelated run status.
#[test]
fn auto_send_footer_label_is_recognisable() {
    let label = format!("{AUTO_SEND_PREFIX}2.0s · any key to edit");
    assert!(label.starts_with(AUTO_SEND_PREFIX));
    assert!(!"Working…".starts_with(AUTO_SEND_PREFIX));
}

#[test]
fn markdown_flag_only_comes_from_an_explicit_true() {
    // Tools opt in by setting `details.markdown = true` (rpi-todo does).
    assert!(tool_result_requests_markdown(Some(
        &serde_json::json!({"kind": "todo", "markdown": true})
    )));
    // Absence, a false value, or a non-bool value keeps plain rendering,
    // so every other tool is unaffected.
    assert!(!tool_result_requests_markdown(None));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"kind": "todo"})
    )));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"markdown": false})
    )));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"markdown": "true"})
    )));
}

/// Minimal `TuiState` for unit tests that only touch state flags. Mirrors
/// the explicit literals other tests build, but keeps one copy in sync.
fn test_tui_state() -> Arc<TuiState> {
    Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    })
}

/// A `SetEditorText` injection — and the host's own auto-send clear — must be
/// attributed to `extension`, not to the user.
///
/// A hands-free voice extension stands down whenever `source == "user"`
/// (the human took the keyboard), so mislabelling the plugin's own injected
/// transcription would cancel the whole conversation on its first turn.
#[test]
fn editor_change_source_separates_tui_writes_from_typing() {
    use std::sync::atomic::Ordering;
    let state = test_tui_state();

    // Nothing changed yet → nothing to report.
    assert!(state.take_editor_change().is_none());

    // The render tick applies an extension's draft…
    state.editor_text.push(rpi_extensions::EditorTextEdit {
        text: "你好".to_string(),
        mode: rpi_extensions::EditorTextMode::Replace,
        auto_send_ms: Some(2000),
    });
    assert!(state.drain_editor_text(), "the queued draft must land");
    assert!(
        state.programmatic_editor_write.load(Ordering::SeqCst),
        "a mailbox-applied edit must be marked programmatic"
    );

    // …so its change is ours, not the user's.
    let payload = state
        .take_editor_change()
        .expect("draft applied ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("extension"));
    assert_eq!(payload["chars"], serde_json::json!(2));
    assert_eq!(payload["empty"], serde_json::json!(false));

    // The mark was consumed: the same text twice is not re-reported, and a
    // genuine human edit is attributed to the user.
    assert!(state.take_editor_change().is_none());
    state.editor.set_text("你好啊");
    let payload = state.take_editor_change().expect("typing ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
    assert_eq!(payload["chars"], serde_json::json!(3));
}

/// The attribution mark must not survive unrelated changes, or one injected
/// draft would make the *next* keystroke look programmatic too.
#[test]
fn programmatic_mark_is_consumed_even_with_no_subscriber() {
    use std::sync::atomic::Ordering;
    let state = test_tui_state();
    assert!(
        state
            .extension_session
            .lock()
            .unwrap()
            .snapshot_arc()
            .is_none(),
        "this test relies on nobody subscribing"
    );

    state.editor_text.push(rpi_extensions::EditorTextEdit {
        text: "draft".to_string(),
        mode: rpi_extensions::EditorTextMode::Replace,
        auto_send_ms: None,
    });
    state.drain_editor_text();
    // `sync_editor_change` bails out early (no subscriber)…
    state.sync_editor_change();
    // …but the mark is gone, so the next real edit is the user's.
    assert!(!state.programmatic_editor_write.load(Ordering::SeqCst));
    state.editor.set_text("draft +");
    let payload = state.take_editor_change().expect("typing ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
}

/// Regression: the editor clear that follows *every* submit is host
/// housekeeping, not the user typing.
///
/// Reporting it as `source:"user"` switched a hands-free voice session off
/// the instant it was switched on: `/voice auto` turns the mode on, then the
/// render tick notices the cleared editor ~80ms later, stands the mode down
/// and aborts the listen it just started — so the microphone never stayed
/// open and the session looked like it "didn't hear anything".
#[test]
fn submit_clear_is_attributed_to_the_host_not_the_user() {
    let state = test_tui_state();

    // Typing the command is a genuine user edit.
    state.editor.set_text("/voice auto");
    assert_eq!(
        state.take_editor_change().unwrap()["source"],
        serde_json::json!("user")
    );

    // The submit path's clear is not.
    state.clear_editor_programmatically();
    let payload = state.take_editor_change().expect("clearing is a change");
    assert_eq!(payload["empty"], serde_json::json!(true));
    assert_eq!(
        payload["source"],
        serde_json::json!("extension"),
        "a submit-clear must not be reported as the user typing"
    );
}

/// A user-initiated clear (Esc, select-all + delete) still goes through the
/// editor itself and must stay `user` — only host writes are reattributed.
#[test]
fn user_initiated_clear_stays_a_user_change() {
    let state = test_tui_state();
    state.editor.set_text("draft");
    assert_eq!(
        state.take_editor_change().unwrap()["source"],
        serde_json::json!("user")
    );
    state.editor.clear();
    let payload = state.take_editor_change().expect("clearing is a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
}

#[test]
fn live_panel_repaints_while_a_tool_or_bash_is_running() {
    // No panels → only the dock loader animates (reuse the cached
    // transcript instead of rebuilding a long history).
    let empty: HashMap<String, Arc<ToolExecutionComponent>> = HashMap::new();
    assert!(!transcript_has_live_panel(false, &empty));

    // A running tool call is a live panel: its elapsed readout is computed
    // at render time, so it must force a transcript rebuild every tick.
    let mut tools = HashMap::new();
    let tool = Arc::new(ToolExecutionComponent::new("search", "{}"));
    tool.set_running();
    tools.insert("tc1".to_string(), tool);
    assert!(transcript_has_live_panel(false, &tools));

    // A finished tool no longer needs the repaint, but a running bash
    // command still does.
    tools.get("tc1").unwrap().set_result("done", false);
    assert!(!transcript_has_live_panel(false, &tools));
    assert!(transcript_has_live_panel(true, &tools));
}

#[test]
fn dequeue_merge_preserves_a_typed_draft() {
    assert_eq!(merge_queued_into_draft("queued", ""), "queued");
    assert_eq!(merge_queued_into_draft("queued", "   "), "queued");
    assert_eq!(
        merge_queued_into_draft("queued", "typed"),
        "queued\n\ntyped"
    );
}

#[test]
fn dequeue_places_the_caret_at_the_end_of_the_last_line_in_bytes() {
    // Restoring queued messages followed by the old `(0, char_count)` call
    // parked the caret at the end of the FIRST line, and treated a char
    // count as a byte offset (so any CJK draft landed mid-character).
    let editor = Arc::new(Editor::simple());
    // Multi-line + multibyte: the caret must be on the LAST row, after the
    // final byte of that row.
    let text = "排队\n第二行";
    set_editor_text_caret_at_end(&editor, text);
    assert_eq!(editor.get_text(), text);
    assert_eq!(
        editor.cursor_position(),
        (1, "第二行".len()),
        "caret must sit at the end of the last line, in bytes"
    );
}

#[test]
fn settings_menu_covers_the_wired_settings_surface() {
    let state = test_tui_state();
    let settings = crate::settings::Settings::default();
    let items = settings_menu_items(&settings, &state, "claude-sonnet-5");
    let keys: Vec<&str> = items.iter().map(|item| item.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "theme",
            "model",
            "thinking",
            "scoped-models",
            "hide-thinking",
            "show-images",
            "cache-miss-notices",
            "quiet-startup",
            "terminal-progress",
            "fullscreen-copy-on-select",
            "double-escape-action",
            "editor-padding",
            "autocomplete-max-items",
            "http-idle-timeout",
        ]
    );
    // Submenu rows must not cycle their displayed value.
    for item in items.iter().filter(|item| item.has_submenu) {
        assert!(
            item.values.is_empty(),
            "{} should be submenu-only",
            item.key
        );
    }
    // Value rows advertise a cycle that contains the current value.
    for item in items.iter().filter(|item| !item.has_submenu) {
        assert!(!item.values.is_empty(), "{} has no values", item.key);
        assert!(
            item.values.iter().any(|value| value == &item.value),
            "{} current value {} not in {:?}",
            item.key,
            item.value,
            item.values
        );
    }
    // The default model row falls back to the running lane's model.
    assert_eq!(items[1].value, "claude-sonnet-5");
    // Native defaults: cache-miss notices off, terminal progress on.
    assert_eq!(items[6].value, "false");
    assert_eq!(items[8].value, "true");
}

#[test]
fn apply_setting_change_updates_settings_and_live_state() {
    let state = test_tui_state();
    let mut settings = crate::settings::Settings::default();

    // Live: hide-thinking flips both the runtime flag and the persisted one.
    let note = apply_setting_change(&state, "hide-thinking", "true", &mut settings);
    assert!(state.hide_thinking());
    assert_eq!(settings.hide_thinking_block, Some(true));
    assert!(note.expect("user-visible note").contains("hidden"));

    // Live: show-images writes the runtime mutex.
    apply_setting_change(&state, "show-images", "false", &mut settings);
    assert!(!*state.show_images.lock().unwrap());
    assert_eq!(settings.show_images, Some(false));

    // Live: cache-miss notices.
    apply_setting_change(&state, "cache-miss-notices", "true", &mut settings);
    assert!(*state.cache_miss_notices.lock().unwrap());
    assert_eq!(settings.show_cache_miss_notices, Some(true));

    // Persist-only rows change settings without a transcript note.
    assert_eq!(
        apply_setting_change(&state, "editor-padding", "3", &mut settings),
        None
    );
    assert_eq!(settings.editor_padding_x, Some(3));
    apply_setting_change(&state, "double-escape-action", "none", &mut settings);
    assert_eq!(settings.double_escape_action.as_deref(), Some("none"));
    apply_setting_change(&state, "autocomplete-max-items", "20", &mut settings);
    assert_eq!(settings.autocomplete_max_visible, Some(20));

    // HTTP idle timeout maps labels onto the numeric/`disabled` forms.
    apply_setting_change(&state, "http-idle-timeout", "disabled", &mut settings);
    assert_eq!(settings.http_idle_timeout_ms(), Some(0));
    apply_setting_change(&state, "http-idle-timeout", "1m", &mut settings);
    assert_eq!(settings.http_idle_timeout_ms(), Some(60_000));

    // Unknown keys are ignored rather than panicking.
    assert_eq!(
        apply_setting_change(&state, "nope", "x", &mut settings),
        None
    );
}

#[test]
fn add_user_message_renders_a_visible_user_bubble() {
    // The submit handler renders directly-sent prompts through this helper
    // (the harness emits no user `message_start` for them), so a silent
    // failure here would make every prompt vanish from the transcript.
    let chat = Arc::new(Container::new());
    add_user_message(&chat, "hello from the user");
    let rendered = crate::interactive_tui::collect_transcript_lines(&chat);
    let plain = strip_ansi(&rendered.join("\n"));
    assert!(
        plain.contains("hello from the user"),
        "user bubble must show the prompt text; got: {plain:?}"
    );
}

#[test]
fn verbose_overrides_quiet_startup_listing() {
    assert!(should_show_startup_listing(false, false));
    assert!(should_show_startup_listing(true, false));
    assert!(should_show_startup_listing(true, true));
    assert!(!should_show_startup_listing(false, true));
}

#[test]
fn resume_command_omits_the_default_session_dir() {
    let cwd = std::path::Path::new("/proj/demo");
    assert_eq!(format_resume_command("", cwd, None), None);
    assert_eq!(
        format_resume_command("abc-123", cwd, None).as_deref(),
        Some("rpi --session abc-123")
    );
    // The default dir carries no flag (mirrors upstream).
    let default = crate::session::default_session_dir(cwd);
    assert_eq!(
        format_resume_command("abc-123", cwd, Some(default.as_path())).as_deref(),
        Some("rpi --session abc-123")
    );
    // A custom dir is echoed, quoting when needed.
    let custom = std::path::Path::new("/tmp/my sessions");
    assert_eq!(
        format_resume_command("abc-123", cwd, Some(custom)).as_deref(),
        Some("rpi --session-dir \"/tmp/my sessions\" --session abc-123")
    );
}

#[test]
fn quote_shell_arg_quotes_only_when_needed() {
    assert_eq!(quote_shell_arg("plain"), "plain");
    assert_eq!(quote_shell_arg("with space"), "\"with space\"");
    assert_eq!(quote_shell_arg("a&b"), "\"a&b\"");
}

#[test]
fn trust_command_uses_the_session_cwd_after_process_chdir() {
    const CHILD_ENV: &str = "RPI_TEST_TRUST_COMMAND_CHILD";
    const SESSION_CWD_ENV: &str = "RPI_TEST_TRUST_COMMAND_SESSION_CWD";
    const TEST_NAME: &str =
        "interactive_tui::tests::trust_command_uses_the_session_cwd_after_process_chdir";

    if std::env::var_os(CHILD_ENV).is_some() {
        let session_cwd = std::path::PathBuf::from(
            std::env::var_os(SESSION_CWD_ENV).expect("child session cwd should be configured"),
        );
        let changed_cwd = std::env::current_dir().unwrap();

        set_project_trust_for_command(&session_cwd, Some(true)).unwrap();

        assert_eq!(
            crate::config::project_trust_decision(&session_cwd).unwrap(),
            Some(true)
        );
        assert_eq!(
            crate::config::project_trust_decision(&changed_cwd).unwrap(),
            None
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let session_cwd = tmp.path().join("session-project");
    let changed_cwd = tmp.path().join("extension-cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&session_cwd).unwrap();
    std::fs::create_dir_all(&changed_cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(SESSION_CWD_ENV, &session_cwd)
        .env(crate::config::CONFIG_DIR_ENV, &agent_dir)
        .current_dir(&changed_cwd)
        .status()
        .unwrap();

    assert!(status.success(), "child test process failed: {status}");
}

#[test]
fn tui_startup_settings_prefer_rpi_project_fields_over_pi() {
    let global = crate::settings::Settings {
        editor_padding_x: Some(3),
        autocomplete_max_visible: Some(6),
        hide_thinking_block: Some(false),
        quiet_startup: Some(true),
        show_terminal_progress: Some(true),
        ..Default::default()
    };
    let rpi_project = crate::settings::Settings {
        editor_padding_x: Some(0),
        hide_thinking_block: Some(true),
        quiet_startup: Some(false),
        show_terminal_progress: Some(false),
        ..Default::default()
    };
    let pi_project = crate::settings::Settings {
        editor_padding_x: Some(9),
        autocomplete_max_visible: Some(12),
        quiet_startup: Some(true),
        ..Default::default()
    };

    assert_eq!(
        resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true),
        TuiStartupSettings {
            editor_padding_x: 0,
            autocomplete_max_visible: 12,
            hide_thinking: true,
            quiet_startup: false,
            show_terminal_progress: false,
            show_images: true,
            cache_miss_notices: false,
        }
    );
}

#[test]
fn tui_startup_settings_fall_back_from_rpi_to_pi_per_field() {
    let global = crate::settings::Settings {
        quiet_startup: Some(false),
        ..Default::default()
    };
    let rpi_project = crate::settings::Settings::default();
    let pi_project = crate::settings::Settings {
        quiet_startup: Some(true),
        ..Default::default()
    };

    let resolved = resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true);

    assert!(resolved.quiet_startup);
}

#[test]
fn tui_startup_settings_use_global_values_without_project_fields() {
    let global = crate::settings::Settings {
        editor_padding_x: Some(7),
        autocomplete_max_visible: Some(8),
        hide_thinking_block: Some(true),
        quiet_startup: Some(true),
        show_terminal_progress: Some(false),
        show_images: Some(false),
        show_cache_miss_notices: Some(true),
        ..Default::default()
    };

    assert_eq!(
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true,),
        TuiStartupSettings {
            editor_padding_x: 7,
            autocomplete_max_visible: 8,
            hide_thinking: true,
            quiet_startup: true,
            show_terminal_progress: false,
            show_images: false,
            cache_miss_notices: true,
        }
    );
}

#[test]
fn tui_startup_settings_read_native_nested_terminal_block() {
    // upstream nests these under `terminal`; rpi's flat keys stay
    // compatible and the nested block is the fallback.
    let global = crate::settings::Settings {
        terminal: Some(crate::settings::TerminalSettings {
            show_images: Some(false),
            show_terminal_progress: Some(false),
            clear_on_shrink: None,
        }),
        ..Default::default()
    };
    let resolved =
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
    assert!(!resolved.show_images);
    assert!(!resolved.show_terminal_progress);
}

#[test]
fn tui_startup_settings_flat_keys_win_over_nested_terminal_block() {
    let global = crate::settings::Settings {
        show_images: Some(true),
        terminal: Some(crate::settings::TerminalSettings {
            show_images: Some(false),
            show_terminal_progress: None,
            clear_on_shrink: None,
        }),
        ..Default::default()
    };
    let resolved =
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
    assert!(resolved.show_images);
}

#[test]
fn tui_startup_settings_ignore_untrusted_project_values() {
    let global = crate::settings::Settings {
        quiet_startup: Some(false),
        show_terminal_progress: Some(true),
        ..Default::default()
    };
    let project = crate::settings::Settings {
        quiet_startup: Some(true),
        show_terminal_progress: Some(false),
        ..Default::default()
    };

    let resolved = resolve_tui_startup_settings(&global, &[project], false);

    assert!(!resolved.quiet_startup);
    assert!(resolved.show_terminal_progress);
}

#[test]
fn transcript_page_uses_viewport_with_overlap() {
    assert_eq!(transcript_page_size(24), 20);
    assert_eq!(transcript_page_size(4), 1);
    assert_eq!(transcript_page_size(0), 1);
}

#[test]
fn parse_user_bash_distinguishes_single_and_double_bang() {
    // `!cmd` runs and stays in context.
    assert_eq!(parse_user_bash("!ls -la"), Some(("ls -la", false)));
    // `!!cmd` runs but is excluded from context.
    assert_eq!(parse_user_bash("!!ls -la"), Some(("ls -la", true)));
    // Surrounding whitespace on the command is trimmed, the `!` prefix is not.
    assert_eq!(parse_user_bash("!  echo hi  "), Some(("echo hi", false)));
    assert_eq!(parse_user_bash("!!  echo hi"), Some(("echo hi", true)));
}

#[test]
fn parse_user_bash_ignores_bare_bang_and_plain_text() {
    // A bare `!` / `!!` has no command: fall through to the prompt path
    // instead of executing an empty shell line.
    assert_eq!(parse_user_bash("!"), None);
    assert_eq!(parse_user_bash("!!"), None);
    assert_eq!(parse_user_bash("!   "), None);
    assert_eq!(parse_user_bash("hello"), None);
    assert_eq!(parse_user_bash(""), None);
    // `/` keeps routing to the slash-command registry.
    assert_eq!(parse_user_bash("/model"), None);
}

#[test]
fn user_bash_run_slot_guards_and_cancels() {
    let state = test_tui_state();
    assert!(!state.user_bash_running());
    let cancel = state.begin_user_bash();
    assert!(state.user_bash_running());
    // Esc cancels without freeing the slot; the spawned task clears it when
    // the capture resolves (upstream's `isBashRunning`).
    assert!(state.cancel_user_bash());
    assert!(state.user_bash_running());
    assert!(cancel.is_cancelled());
    state.finish_user_bash();
    assert!(!state.user_bash_running());
    // Nothing to cancel once idle.
    assert!(!state.cancel_user_bash());
}

#[test]
fn key_repeat_is_dispatched_but_release_is_not() {
    assert!(should_dispatch_key(KeyEventKind::Press));
    assert!(should_dispatch_key(KeyEventKind::Repeat));
    assert!(!should_dispatch_key(KeyEventKind::Release));
}

#[test]
fn test_layout_renders_welcome_message() {
    let chat = Arc::new(Container::new());
    add_welcome_message(&chat);

    let scroll = Arc::new(ScrollView::new(
        chat.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            ..Default::default()
        },
    ));

    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    let dock = Arc::new(Container::new());
    dock.add_child(editor);

    let footer = Arc::new(FooterComponent::new());

    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(dock)),
        StackChild::Entry(StackEntry::new(footer)),
    ]);

    let frame = rpi_tui::render_layout_frame(Arc::new(root), 80, 24);

    let all: String = frame.lines.join("\n");
    assert!(
        all.contains("rpi"),
        "Welcome message not found. Rendered: {}",
        all
    );
    assert!(
        all.contains("Type your message"),
        "Help text not found. Rendered: {}",
        all
    );
}

#[test]
fn test_chat_container_has_welcome_content() {
    let chat = Arc::new(Container::new());
    add_welcome_message_with_capabilities(
        &chat,
        &["read".into(), "bash".into(), "web_fetch".into()],
        &["rust-review".into(), "release".into()],
    );

    let lines = chat.render(80);
    let all: String = lines.join("\n");
    // Welcome title is "rpi" (accent bold) + "interactive TUI" (muted),
    // joined by an ANSI reset — strip ANSI before checking the substring.
    let plain = strip_ansi(&all);
    assert!(
        plain.contains("rpi"),
        "Welcome message not in chat container: {:?}",
        lines
    );
    assert!(plain.contains("Tools (3)"), "Tool count missing: {plain}");
    assert!(
        plain.contains("read · bash · web_fetch"),
        "Tool names missing: {plain}"
    );
    assert!(plain.contains("Skills (2)"), "Skill count missing: {plain}");
    assert!(
        plain.contains("rust-review · release"),
        "Skill names missing: {plain}"
    );
}

#[test]
fn welcome_header_renders_the_brand_mark() {
    let chat = Arc::new(Container::new());
    add_welcome_message(&chat);
    let lines: Vec<String> = chat.render(80).iter().map(|l| strip_ansi(l)).collect();
    let all = lines.join("\n");

    // The three-bar mark: the bottom row carries all three bars, and the
    // wordmark sits on its baseline next to the middle bar.
    assert!(
        lines.iter().any(|l| l.contains("██ ██ ██")),
        "brand mark missing: {all}"
    );
    assert!(
        all.contains("rpi"),
        "the mark must carry the product name: {all}"
    );

    // Regression guard for the layout bug this replaced: `π` is East-Asian
    // Ambiguous — width 1 normally, width 2 under a CJK locale — which
    // walked the old box borders out of alignment depending on the user's
    // system. The crab is deliberately wide but unambiguous, and `crate::brand`
    // tests pin its width so the padding cannot silently drift.
    assert!(
        !all.contains('π'),
        "the header must not use east-asian-ambiguous glyphs: {all}"
    );
}

#[test]
fn update_notices_render_inside_the_transcript() {
    let chat = Arc::new(Container::new());
    let report = crate::updates::UpdateReport {
        notices: vec![crate::updates::UpdateNotice {
            name: "rpi".into(),
            current: "0.1.10".into(),
            latest: "0.1.11".into(),
            command: "rpi update".into(),
        }],
        warnings: vec![crate::updates::UpdateWarning {
            message: "The previously scheduled rpi update failed: access denied".into(),
            command: "rpi update".into(),
        }],
    };

    add_update_notices(&chat, &report);

    assert_eq!(chat.child_count(), 1);
    let plain = strip_ansi(&chat.render(80).join("\n"));
    assert!(plain.contains("Update Failed"), "{plain}");
    assert!(
        plain.contains("rpi update failed: access denied"),
        "{plain}"
    );
    assert!(plain.contains("Update Available"), "{plain}");
    assert!(plain.contains("New version 0.1.11 is available"), "{plain}");
    assert!(plain.contains("rpi update"), "{plain}");
}

#[test]
fn empty_update_report_does_not_add_transcript_content() {
    let chat = Arc::new(Container::new());

    add_update_notices(&chat, &crate::updates::UpdateReport::default());

    assert_eq!(chat.child_count(), 0);
}

#[test]
fn welcome_capabilities_show_empty_state() {
    let plain = strip_ansi(&welcome_capability_line("Skills", &[]));
    assert_eq!(plain, "Skills (0) none");
}

#[test]
fn completed_stream_reconciles_the_final_tail_before_detaching() {
    let component = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    component.set_streaming(true);
    component.update_blocks(&[AssistantBlock::Text("partial response".into())]);

    let current = Arc::new(Mutex::new(Some(component.clone())));
    let cached = Mutex::new("partial response".to_string());
    let mut final_message = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "faux-model", 0);
    final_message.content = vec![Content::text(
        "partial response with the previously missing final tail",
    )];
    final_message.stop_reason = rpi_ai::types::StopReason::Stop;

    reconcile_streamed_assistant_completion(&current, &cached, Some(&final_message));

    assert!(current.lock().unwrap().is_none());
    assert_eq!(
        cached.lock().unwrap().as_str(),
        "partial response with the previously missing final tail"
    );
    let rendered = strip_ansi(&component.render(100).join("\n"));
    assert!(
        rendered.contains("previously missing final tail"),
        "{rendered}"
    );
}

/// Reproduction for "Tab 补全了但显示没刷新": after `accept_top_suggestion`
/// replaces the editor text, the NEXT rendered frame must show the
/// completed text (" /model " with the caret after it), not the old
/// prefix. Mirrors the real dock layout (autocomplete_container above the
/// bordered editor) and drives the same accept path the Tab handler uses.
#[test]
fn tab_accept_suggestion_reflects_in_next_render() {
    use rpi_tui::render_layout_frame;

    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);
    let editor_container = Arc::new(Container::new());
    editor_container.add_child(editor.clone());
    let autocomplete_container = Arc::new(Container::new());
    let footer = Arc::new(rpi_tui::Text::new("FOOTER", 0, 0));
    let dock = Arc::new(VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
        StackChild::Entry(
            StackEntry::new(editor_container.clone())
                .shrink(0)
                .min_size(3),
        ),
        StackChild::Entry(StackEntry::new(footer)),
    ]));

    // Simulate the user typing "/mo" (the popup shows suggestions).
    let manager = AutocompleteManager::new();
    let mut combined = CombinedAutocompleteProvider::new();
    combined.add_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    combined.add_provider(Arc::new(FilePathAutocompleteProvider::new()));
    manager.set_provider(Arc::new(combined));
    // Simulate typing "/mo" via the real insert path (advances the caret
    // by char length, like `handle_key` does).
    editor.insert("/mo");
    assert_eq!(editor.cursor_position(), (0, 3));

    let frame_before = render_layout_frame(dock.clone(), 80, 10);
    assert!(
        frame_before.lines.iter().any(|l| l.contains("/mo")),
        "precondition: editor shows the typed prefix. Frame rows:\n{}",
        frame_before
            .lines
            .iter()
            .map(|l| format!("  [{l}]"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Tab: accept the top suggestion (the same code path as the key loop).
    let text = editor.get_text();
    let cursor = editor_cursor_offset(&editor, &text);
    let sugg = manager
        .get_suggestions(&text, cursor)
        .expect("slash suggestions for /mo");
    let top = sugg.items.first().expect("at least one suggestion");
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    let mut replaced = String::new();
    replaced.push_str(&text[..start]);
    replaced.push_str(&top.text);
    replaced.push_str(&text[end..]);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    let new_cursor = start + top.text.len();
    editor.set_text(&replaced);
    set_editor_cursor_offset(&editor, &replaced, new_cursor);
    autocomplete_container.clear();
    assert_eq!(editor.get_text(), "/model");

    // The next render MUST display the completed text.
    let frame_after = render_layout_frame(dock, 80, 10);
    let all: String = frame_after.lines.join("\n");
    assert!(
        all.contains("/model"),
        "completed text missing from next render. Got:\n{all}"
    );
    // The caret must sit AFTER the completed command (the snap_boundary
    // regression put it one char early: "/mode|l" with the final char
    // dangling past the caret).
    let editor_line = frame_after
        .lines
        .iter()
        .find(|l| l.contains("/model"))
        .expect("editor row with completed text");
    assert!(
        editor_line.contains(&format!("/model{}", rpi_tui::CURSOR_MARKER)),
        "caret must follow the full completed text. Got: {editor_line:?}"
    );
}

#[test]
fn multiline_autocomplete_preserves_row_and_column() {
    let editor = Arc::new(Editor::simple());
    editor.set_text("first\n/mo");
    editor.set_cursor(1, 3);

    let text = editor.get_text();
    assert_eq!(editor_cursor_offset(&editor, &text), 9);

    let replaced = "first\n/model";
    editor.set_text(replaced);
    set_editor_cursor_offset(&editor, replaced, 12);
    assert_eq!(editor.cursor_position(), (1, 6));
}

#[test]
fn test_slash_command_dispatch() {
    // The registry is the single source of truth for dispatch: `find(token)`
    // returns the command (by name or alias) whose `name()` is the canonical
    // form, or `None` for an unknown token. This replaces the old enum-based
    // `handle_slash_command` assertions with equivalent registry lookups.
    let registry = build_builtin_registry();

    // Helper: a token resolves to the command with this canonical name.
    let resolves_to = |token: &str, canonical: &str| {
        let found = registry.find(token).expect("{token} should resolve");
        assert_eq!(
            found.name(),
            canonical,
            "{token} resolved to {} (expected {canonical})",
            found.name()
        );
    };

    resolves_to("/help", "/help");
    resolves_to("/?", "/help"); // alias → canonical
    resolves_to("/clear", "/clear");
    resolves_to("/new", "/clear"); // alias
    resolves_to("/q", "/exit"); // alias
    resolves_to("/quit", "/exit"); // alias
    resolves_to("/version", "/version");
    resolves_to("/v", "/version"); // alias
    resolves_to("/changelog", "/changelog");
    resolves_to("/hotkeys", "/hotkeys");
    resolves_to("/model", "/model");
    resolves_to("/m", "/model"); // alias
    resolves_to("/theme", "/theme");
    resolves_to("/session", "/session");
    resolves_to("/resume", "/session"); // alias
    resolves_to("/compact", "/compact");
    resolves_to("/copy", "/copy");
    resolves_to("/thinking", "/thinking");
    resolves_to("/think", "/thinking"); // alias
    resolves_to("/tools", "/tools");
    resolves_to("/images", "/images");
    resolves_to("/context", "/context");
    // Out-of-v1-scope commands resolve to their own UnsupportedCommand entry.
    resolves_to("/settings", "/settings");
    resolves_to("/name", "/name");
    resolves_to("/export", "/export");

    // Unknown token → not found.
    assert!(registry.find("/nope").is_none(), "/nope should be unknown");
}

#[test]

fn test_registry_visible_entries_cover_dispatch() {
    // The autocomplete list is derived from the registry, so every visible
    // command the dispatcher recognizes must appear in it — by construction,
    // but this guards against a future command being registered with
    // `visible()` / a non-empty description that the builder drops.
    let registry = build_builtin_registry();
    let names: Vec<String> = registry
        .visible_entries()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    for recognized in [
        "/help",
        "/clear",
        "/new",
        "/exit",
        "/quit",
        "/version",
        "/changelog",
        "/model",
        "/session",
        "/theme",
        "/compact",
        "/copy",
        "/hotkeys",
        "/tools",
        "/images",
        "/thinking",
        "/usage",
    ] {
        assert!(
            names.contains(&recognized.to_string()),
            "{recognized} missing from autocomplete list"
        );
    }
    // Hidden commands stay off the list.
    for hidden in ["/context", "/q", "/m", "/v", "/think", "/resume", "/?"] {
        assert!(
            !names.contains(&hidden.to_string()),
            "{hidden} should be hidden from autocomplete"
        );
    }
}

#[test]
fn test_agent_event_mapping_creates_assistant_and_tool() {
    // Synthetic AgentEvent sequence → UI mutations, exercised against the
    // real drain handler with a no-op TUI stand-in.
    use rpi_ai::types::{
        StopReason, TextContent, TextContentType, ThinkingContent, ThinkingContentType, ToolCall,
        ToolCallType, Usage,
    };

    let state = Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    });

    // The drain handler takes `Arc<TuiAltScreen>`, which needs a real
    // terminal; instead, exercise the *mutation* half directly against a
    // captured chat container via a synthetic message-start event's data.
    let assistant = AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![
            Content::Thinking(ThinkingContent {
                kind: ThinkingContentType,
                thinking: "Reasoning about the reply.".into(),
                thinking_signature: None,
                redacted: false,
            }),
            Content::Text(TextContent {
                kind: TextContentType,
                text: "Hello.".into(),
                text_signature: None,
            }),
            Content::ToolCall(ToolCall {
                kind: ToolCallType,
                id: "tc1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo hi"}),
                thought_signature: None,
                namespace: None,
            }),
        ],
        api: rpi_ai::Api::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };

    // Manually apply the MessageStart assistant branch logic (mirrors the
    // drain handler, without needing a TuiAltScreen).
    let comp = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    comp.set_streaming(true);
    comp.update_blocks(&assistant_blocks(&assistant));
    let chat = Arc::new(Container::new());
    chat.add_child(comp.clone());
    *state.current_assistant.lock().unwrap() = Some(comp);

    // Manually apply the MessageUpdate tool-call scan (mirrors drain).
    for c in &assistant.content {
        if let Content::ToolCall(tc) = c {
            let mut tools = state.tool_components.lock().unwrap();
            if !tools.contains_key(&tc.id) {
                let tc_comp = Arc::new(ToolExecutionComponent::new(
                    &tc.name,
                    &tc.arguments.to_string(),
                ));
                tc_comp.set_running();
                chat.add_child(tc_comp.clone());
                tools.insert(tc.id.clone(), tc_comp);
            }
        }
    }

    // Assert: the assistant component rendered the text + the thinking
    // block (the update_blocks path keeps thinking visible), and a tool
    // component was registered.
    let rendered = chat.render(80);
    let joined: String = rendered.join("\n");
    assert!(
        joined.contains("Hello."),
        "assistant text not rendered: {joined}"
    );
    assert!(
        joined.contains("Reasoning about the reply."),
        "thinking block not rendered: {joined}"
    );
    assert_eq!(state.tool_components.lock().unwrap().len(), 1);
    assert!(state.current_assistant.lock().unwrap().is_some());

    // Manually apply ToolExecutionEnd (mirrors drain).
    let ended = state.tool_components.lock().unwrap().remove("tc1").unwrap();
    ended.set_result("hi", false);
    assert!(state.tool_components.lock().unwrap().is_empty());

    // A running bash panel owns the visible spinner. The global loader is
    // hidden until the last concurrent bash tool completes, then restored
    // while the agent remains in the Working state.
    assert!(state.try_start_working());
    assert!(
        !state.try_start_working(),
        "a second submit must be rejected"
    );
    state.set_status(RunStatus::Idle);
    state.set_status(RunStatus::Working);
    // The active loader now renders inside the editor's top border.
    assert!(state.editor.working().is_some());
    assert_eq!(state.status_container.child_count(), 0);
    assert!(state.footer.get_status().is_empty());
    state.show_retry(3, 10, 8_000);
    let retry_status = strip_ansi(&state.status_container.render(80).join("\n"));
    assert!(retry_status.contains("Retrying (3/10)"));
    state.set_status(RunStatus::Working);
    {
        let mut bash = state.bash_components.lock().unwrap();
        bash.insert(
            "bash-1".into(),
            Arc::new(BashExecutionComponent::new("one")),
        );
        bash.insert(
            "bash-2".into(),
            Arc::new(BashExecutionComponent::new("two")),
        );
    }
    state.sync_working_loader_with_bash();
    assert_eq!(state.status_container.child_count(), 0);
    state.bash_components.lock().unwrap().remove("bash-1");
    state.sync_working_loader_with_bash();
    assert_eq!(state.status_container.child_count(), 0);
    state.bash_components.lock().unwrap().remove("bash-2");
    state.sync_working_loader_with_bash();
    // The editor-border indicator stays visible across bash panels, so the
    // status container stays empty.
    assert_eq!(state.status_container.child_count(), 0);
    assert!(state.editor.working().is_some());

    state.set_status(RunStatus::Aborting);
    assert_eq!(state.status_container.child_count(), 0);
    assert!(!state.loader.is_running());
}

#[test]
fn fresh_launch_does_not_restore_old_history() {
    let fresh = Args::default();
    assert!(!launch_restores_history(&fresh));

    let continued = Args {
        continue_session: true,
        ..Args::default()
    };
    assert!(launch_restores_history(&continued));

    let selected = Args {
        session: Some("session-id".into()),
        ..Args::default()
    };
    assert!(launch_restores_history(&selected));
}

#[test]
fn context_badge_ignores_aborted_and_error_turns() {
    use rpi_ai::types::{Api, StopReason, Usage};

    let footer = Arc::new(FooterComponent::new());
    footer.set_context_window(100_000);

    // A real response fills in the percent (`?` -> `25.0%/100k`).
    let mut ok = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    ok.stop_reason = StopReason::Stop;
    ok.usage = Usage {
        input: 25_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &ok);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");

    // An aborted turn must not clobber the last good value.
    let mut aborted = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    aborted.stop_reason = StopReason::Aborted;
    aborted.usage = Usage {
        input: 90_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &aborted);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");

    // An errored turn likewise (its usage is not a real measurement).
    let mut errored = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    errored.stop_reason = StopReason::Error;
    errored.usage = Usage {
        input: 90_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &errored);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");
}

#[test]
fn test_short_model_name() {
    assert_eq!(
        short_model_name("anthropic:claude-sonnet-5"),
        "claude-sonnet-5"
    );
    assert_eq!(short_model_name("claude-sonnet-5"), "claude-sonnet-5");
}

#[test]
fn model_selector_items_are_deduplicated_and_provider_qualified() {
    use rpi_ai::{Api, Model};

    let mut gateway = Model::new(
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        Api::OpenaiCompletions,
        "routeryo-copy",
        "https://gateway.example.com",
    );
    let duplicate = gateway.clone();
    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    gateway.headers = Some(std::collections::BTreeMap::from([(
        "authorization".into(),
        "Bearer test".into(),
    )]));

    let items = model_selector_items(&[gateway, duplicate, anthropic], "gpt-5.6-sol");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].value, "gpt-5.6-sol");
    assert_eq!(items[0].label, "GPT 5.6 Sol");
    assert_eq!(
        items[0].description.as_deref(),
        Some("routeryo-copy/gpt-5.6-sol (current)")
    );
    assert_eq!(items[1].description.as_deref(), Some("claude-sonnet-5"));
}

#[test]
fn model_selector_search_text_covers_provider_name_and_qualified_id() {
    use rpi_ai::{Api, Model};

    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    let items = model_selector_items(&[anthropic], "");
    let search = items[0].search_key();
    // Mirrors native `getModelSelectorSearchText`: provider-prefixed first,
    // then the display name.
    assert!(search.starts_with("anthropic anthropic/claude-sonnet-5"));
    assert!(search.contains("Claude Sonnet 5"), "{search}");

    // A provider query and a display-name query both match through the
    // real fuzzy filter used by the searchable selector.
    use rpi_tui::fuzzy_filter;
    for query in ["anthropic", "sonnet", "Claude Sonnet", "claude-sonnet-5"] {
        let matched = fuzzy_filter(&items, query, |item| item.search_key());
        assert_eq!(matched.len(), 1, "query {query:?} matched nothing");
    }
}

#[test]
fn model_selector_match_accepts_bare_and_qualified_ids() {
    use rpi_ai::{Api, Model};

    let gateway = Model::new(
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        Api::OpenaiCompletions,
        "routeryo-copy",
        "https://gateway.example.com",
    );
    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    let catalog = [gateway, anthropic];
    assert_eq!(
        find_model_selector_match(&catalog, "gpt-5.6-sol")
            .unwrap()
            .provider,
        "routeryo-copy"
    );
    assert_eq!(
        find_model_selector_match(&catalog, "routeryo-copy/gpt-5.6-sol")
            .unwrap()
            .id,
        "gpt-5.6-sol"
    );
    assert_eq!(
        find_model_selector_match(&catalog, "anthropic/claude-sonnet-5")
            .unwrap()
            .id,
        "claude-sonnet-5"
    );
    assert!(find_model_selector_match(&catalog, "other/gpt-5.6-sol").is_none());
}

#[test]
fn assistant_error_text_keeps_terminal_provider_diagnostic_visible() {
    use rpi_ai::types::{AssistantMessage, AssistantRole, StopReason, Usage};

    let failed = AssistantMessage {
        role: AssistantRole,
        content: Vec::new(),
        api: rpi_ai::Api::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some("upstream returned 401".into()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };
    assert_eq!(
        assistant_error_text(&failed).as_deref(),
        Some("upstream returned 401")
    );

    let mut no_detail = failed;
    no_detail.error_message = Some("  ".into());
    assert_eq!(
        assistant_error_text(&no_detail).as_deref(),
        Some("Provider request failed.")
    );

    let mut aborted = no_detail;
    aborted.stop_reason = StopReason::Aborted;
    aborted.error_message = Some("abort error: Request aborted".into());
    assert_eq!(
        assistant_error_text(&aborted).as_deref(),
        Some("abort error: Request aborted")
    );

    aborted.error_message = None;
    assert_eq!(
        assistant_error_text(&aborted).as_deref(),
        Some("Request aborted.")
    );
}

#[test]
fn sanitize_error_message_keeps_diagnostics_without_terminal_controls() {
    assert_eq!(
        sanitize_error_message("405\r\nMethod Not Allowed\x1b[2J"),
        "405\nMethod Not Allowed"
    );
    assert_eq!(sanitize_error_message("\0\tmessage"), "\tmessage");
    assert_eq!(sanitize_error_message("   "), "Provider request failed.");
    let long = "x".repeat(20_000);
    let cleaned = sanitize_error_message(&long);
    assert!(cleaned.chars().count() <= 16 * 1024 + 1);
    assert!(cleaned.ends_with('…'));
}

#[test]
fn test_cycle_next_model_wraps_around() {
    use rpi_ai::{Api, Model};
    let mk = |id: &str| {
        Model::new(
            id,
            id,
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        )
    };
    let catalog = [mk("a"), mk("b"), mk("c")];
    // Next after "a" is "b"; after "c" wraps to "a".
    assert_eq!(cycle_next_model(&catalog, "a").unwrap().id, "b");
    assert_eq!(cycle_next_model(&catalog, "c").unwrap().id, "a");
    // An unknown current id falls back to the first model.
    assert_eq!(cycle_next_model(&catalog, "zzz").unwrap().id, "a");
    // Empty catalog yields None.
    let empty: Vec<Model> = vec![];
    assert!(cycle_next_model(&empty, "a").is_none());
}

#[test]
fn test_cycle_prev_model_wraps_around() {
    use rpi_ai::{Api, Model};
    let mk = |id: &str| {
        Model::new(
            id,
            id,
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        )
    };
    let catalog = [mk("a"), mk("b"), mk("c")];
    // Previous before "b" is "a"; before "a" wraps to "c".
    assert_eq!(cycle_prev_model(&catalog, "b").unwrap().id, "a");
    assert_eq!(cycle_prev_model(&catalog, "a").unwrap().id, "c");
    // An unknown current id falls back to the first model.
    assert_eq!(cycle_prev_model(&catalog, "zzz").unwrap().id, "a");
    // Empty catalog yields None.
    let empty: Vec<Model> = vec![];
    assert!(cycle_prev_model(&empty, "a").is_none());
}

#[test]
fn test_autocomplete_slash_suggestions_render() {
    // The autocomplete container should render at least one suggestion
    // line when the editor holds a `/` prefix, and clear when it doesn't.
    let state = Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    });
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            build_builtin_registry().visible_entries(),
        )));
        state.autocomplete.set_provider(Arc::new(combined));
    }

    let editor = Arc::new(Editor::simple());
    editor.set_text("/he");
    editor.set_cursor(0, 3);
    refresh_autocomplete(&state, &editor);
    let lines = state.autocomplete_container.render(80);
    let joined: String = lines.join("\n");
    assert!(
        joined.contains("/help"),
        "slash suggestions not rendered: {joined}"
    );

    // Clear: no suggestions for plain text.
    editor.set_text("hello");
    editor.set_cursor(0, 5);
    refresh_autocomplete(&state, &editor);
    assert!(state.autocomplete_container.render(80).is_empty());
}

#[test]
fn test_select_list_swap_restores_editor() {
    // The editor-container swap: opening a selector replaces the editor
    // child; closing restores it. Verify the container child count + the
    // active_selector flag round-trip.
    let state = Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    });
    let editor_container = Arc::new(Container::new());
    let editor = Arc::new(Editor::simple());
    editor_container.add_child(editor.clone());
    assert!(!state.selector_open());

    let tui_terminal = Box::new(ProcessTerminal::new());
    let tui = Arc::new(TuiAltScreen::new(tui_terminal, true, None));
    let list = Arc::new(SelectList::new(
        vec![SelectItem::new("a", "A"), SelectItem::new("b", "B")],
        5,
    ));
    open_selector(
        &state,
        &editor_container,
        &editor,
        &tui,
        list,
        SelectorKind::Theme,
    );
    assert!(state.selector_open());
    // list only (editor swapped out).
    assert_eq!(editor_container.child_count(), 1);

    close_selector(&state, &editor_container, &editor, &tui);
    assert!(!state.selector_open());
    // editor restored.
    assert_eq!(editor_container.child_count(), 1);
}

#[test]
fn test_message_history_browse_restores_draft() {
    // ↑/↓ recall semantics (mirrors TS navigateHistory): push two
    // messages, browse older → newer → back past the newest restores the
    // draft the user was typing.
    let state = Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    });
    let editor = Arc::new(Editor::simple());

    push_history(&state, "first message");
    push_history(&state, "second message");
    // Consecutive duplicate is skipped.
    push_history(&state, "second message");
    push_history(&state, "   "); // empty → skipped
    assert_eq!(state.history.lock().unwrap().len(), 2);
    assert_eq!(state.history.lock().unwrap()[0], "second message");

    // User starts typing a fresh prompt.
    editor.set_text("half-typed");
    editor.set_cursor(0, 11);

    // ↑ → most recent.
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "second message");
    assert_eq!(*state.history_index.lock().unwrap(), 0);
    // ↑ → older.
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "first message");
    assert_eq!(*state.history_index.lock().unwrap(), 1);
    // ↑ past the oldest → stays (no wrap).
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "first message");
    // ↓ → newer.
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "second message");
    // ↓ past the newest → restores the draft.
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "half-typed");
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

#[test]
fn test_accept_top_suggestion_replaces_prefix() {
    // `/he` + Tab → `/help ` (slash command provider inserts a space).
    let state = Arc::new(TuiState {
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
    });
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            build_builtin_registry().visible_entries(),
        )));
        state.autocomplete.set_provider(Arc::new(combined));
    }
    let editor = Arc::new(Editor::simple());
    editor.set_text("/he");
    editor.set_cursor(0, 3);
    refresh_autocomplete(&state, &editor);
    let accepted = accept_top_suggestion(&state, &editor);
    assert!(accepted, "should accept the top suggestion");
    let text = editor.get_text();
    assert!(
        text.starts_with("/help"),
        "editor text should start with /help, got {text}"
    );
}

#[test]
fn configured_key_parser_supports_native_notation() {
    let combo = parse_configured_key("Ctrl+G").expect("ctrl+g should parse");
    assert_eq!(combo.code, KeyCode::Char('g'));
    assert!(combo.modifiers.contains(KeyModifiers::CONTROL));
    let combo = parse_configured_key("shift+tab").expect("shift+tab should parse");
    assert_eq!(combo.code, KeyCode::BackTab);
}

#[test]
fn double_escape_trigger_has_half_second_window() {
    let now = std::time::Instant::now();
    assert!(!double_escape_trigger(None, now));
    assert!(double_escape_trigger(
        Some(now - std::time::Duration::from_millis(500)),
        now
    ));
    assert!(!double_escape_trigger(
        Some(now - std::time::Duration::from_millis(501)),
        now
    ));
}

#[test]
fn ctrl_c_clears_first_and_only_exits_inside_the_window() {
    // upstream's `handleCtrlC`: a lone press clears the editor and never
    // quits by itself; only a second press within 500ms exits. It must not
    // abort — Esc owns that.
    let now = std::time::Instant::now();
    assert_eq!(ctrl_c_action(None, now), CtrlCAction::Clear);
    assert_eq!(
        ctrl_c_action(Some(now - std::time::Duration::from_millis(500)), now),
        CtrlCAction::Exit
    );
    // A slower second press is a fresh "clear", not a quit.
    assert_eq!(
        ctrl_c_action(Some(now - std::time::Duration::from_millis(501)), now),
        CtrlCAction::Clear
    );
}

#[test]
fn pasted_multi_line_block_becomes_one_prompt_not_many() {
    // End-to-end shape of the fix: replay the key stream a Windows paste
    // produces ("l1\nl2\nl3" + a trailing Enter) through the same pieces
    // the key loop uses — the burst discriminator for each Enter and the
    // editor for the text — and assert that *no* submit fired while the
    // block arrived, then exactly one submit on the user's own Enter.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let editor = rpi_tui::Editor::simple();
    let submits = Arc::new(AtomicUsize::new(0));
    let submits_for_cb = submits.clone();
    editor.on_submit(Arc::new(move |_text: &str| {
        submits_for_cb.fetch_add(1, Ordering::SeqCst);
    }));

    // Every pasted key is delivered back-to-back, so `now` is unchanged and
    // the console queue is never empty until the paste is drained.
    let now = std::time::Instant::now();
    let mut last_text_key_at: Option<std::time::Instant> = None;
    let block = ["l1", "l2", "l3"];
    for (index, line) in block.iter().enumerate() {
        for ch in line.chars() {
            editor.insert(&ch.to_string());
            last_text_key_at = Some(now);
        }
        if index + 1 < block.len() {
            // The pasted newline: more input is still queued behind it.
            let more_queued = true;
            assert!(
                enter_is_paste_burst(last_text_key_at, now, more_queued),
                "pasted newline {index} must not submit"
            );
            editor.insert("\n");
            last_text_key_at = Some(now);
        }
    }
    assert_eq!(editor.get_text(), "l1\nl2\nl3");
    assert_eq!(submits.load(Ordering::SeqCst), 0, "paste must not submit");

    // The user's own Enter: a real gap, nothing queued.
    let typed_at = now + PASTE_BURST_GAP + std::time::Duration::from_millis(80);
    assert!(!enter_is_paste_burst(last_text_key_at, typed_at, false));
    editor.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ));
    assert_eq!(
        submits.load(Ordering::SeqCst),
        1,
        "typed Enter submits once"
    );
}

#[test]
fn pasted_win_crlf_text_has_no_stray_carriage_returns() {
    // A Windows paste delivers CRLF; the editor must fold it to LF so the
    // rendered block is clean and the cursor arithmetic stays sound.
    let editor = rpi_tui::Editor::simple();
    editor.insert("alpha\r\nbeta\r\n");
    assert_eq!(editor.get_text(), "alpha\nbeta\n");
    assert!(!editor.get_text().contains('\r'));
}

#[test]
fn pasted_enter_is_not_a_submit() {
    let now = std::time::Instant::now();

    // Queued input behind the Enter => paste, regardless of history.
    assert!(enter_is_paste_burst(None, now, true));

    // Enter immediately after pasted characters => paste (also covers a
    // paste whose final line ends with a newline, where nothing is queued).
    assert!(enter_is_paste_burst(
        Some(now - std::time::Duration::from_millis(1)),
        now,
        false
    ));
    assert!(enter_is_paste_burst(
        Some(now - (PASTE_BURST_GAP - std::time::Duration::from_millis(1))),
        now,
        false
    ));

    // A typed Enter: nothing queued and a real gap since the last key.
    assert!(!enter_is_paste_burst(None, now, false));
    assert!(!enter_is_paste_burst(
        Some(now - PASTE_BURST_GAP),
        now,
        false
    ));
    assert!(!enter_is_paste_burst(
        Some(now - std::time::Duration::from_millis(120)),
        now,
        false
    ));
}

#[test]
fn paste_probe_ignores_the_enter_keys_own_release() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let release =
        KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);

    // Windows queues press+release together over RDP: the release behind a
    // lone Enter must NOT count as queued paste content.
    let mut queued = vec![Event::Key(release)].into_iter();
    let (more, stashed) = probe_paste_input(None, || queued.next());
    assert!(!more, "a key release alone is not queued paste input");
    assert!(stashed.is_none());

    // A real follow-up key (the next pasted line) IS queued input and is
    // stashed for the caller instead of being lost.
    let next = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    let mut queued = vec![Event::Key(release), Event::Key(next)].into_iter();
    let (more, stashed) = probe_paste_input(None, || queued.next());
    assert!(more, "a queued key press is paste continuation");
    assert!(matches!(stashed, Some(Event::Key(_))));

    // Nothing queued at all.
    let (more, stashed) = probe_paste_input(None, || None);
    assert!(!more);
    assert!(stashed.is_none());

    // An already-stashed event counts as queued input, without reading more.
    let (more, stashed) = probe_paste_input(Some(Event::Key(next)), || {
        panic!("must not read when an event is already stashed")
    });
    assert!(more);
    assert!(matches!(stashed, Some(Event::Key(_))));
}

// ---- ask_user TUI bridge ----

#[test]
fn ask_user_prompt_parser_supports_flat_and_questions_shapes() {
    let flat = rpi_extensions::UiDialogRequest {
        request_id: "r1".into(),
        tool_call_id: Some("c1".into()),
        ui: serde_json::json!({
            "kind": "selector",
            "id": "proxy_port",
            "header": "Proxy",
            "question": "端口?",
            "options": [{"title": "7890", "description": "text"}, {"title": "7897"}],
            "allowMultiple": true,
            "allowFreeform": false,
            "suggest": "1080"
        }),
        raw: serde_json::json!({}),
    };
    let prompt = parse_ask_user_prompt(&flat);
    assert_eq!(prompt.question_id, "proxy_port");
    assert_eq!(prompt.header.as_deref(), Some("Proxy"));
    assert_eq!(prompt.options.len(), 2);
    assert_eq!(prompt.options[0].1.as_deref(), Some("text"));
    assert!(prompt.allow_multiple);
    assert!(!prompt.allow_freeform);
    assert_eq!(prompt.suggest.as_deref(), Some("1080"));

    let alias = rpi_extensions::UiDialogRequest {
        ui: serde_json::json!({
            "questions": [{"id": "q", "question": "Q?", "options": []}]
        }),
        ..flat.clone()
    };
    let prompt = parse_ask_user_prompt(&alias);
    assert_eq!(prompt.question_id, "q");
    assert!(prompt.allow_freeform, "no options defaults to freeform");

    let confirm = rpi_extensions::UiDialogRequest {
        ui: serde_json::json!({"kind": "confirm", "question": "Go?"}),
        ..flat.clone()
    };
    let prompt = parse_ask_user_prompt(&confirm);
    assert_eq!(prompt.kind, "confirm");
    assert_eq!(prompt.options.len(), 2);
    assert_eq!(prompt.options[0].0, "Yes");
}

#[test]
fn ask_user_bridge_answers_and_cancels_by_request_id() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    mailbox.attach();
    let bridge = AskUserBridge::new(mailbox.clone());

    // Two concurrent requests must not cross answers.
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r1", "toolCallId": "call-r1",
            "ui": {"kind": "input", "question": "Port?"}
        }))
        .unwrap();
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r2", "toolCallId": "call-r2",
            "ui": {"kind": "input", "question": "Level?"}
        }))
        .unwrap();

    let first = bridge.take_pending().expect("r1 visible");
    assert_eq!(first.request_id, "r1");
    assert!(
        bridge.take_pending().is_none(),
        "only one request occupies the input slot"
    );
    bridge.respond("r1", serde_json::json!({"text": "7897"}));
    assert_eq!(mailbox.poll("r1").unwrap()["answer"]["text"], "7897");

    let second = bridge.take_pending().expect("r2 visible");
    assert_eq!(second.request_id, "r2");
    bridge.cancel("r2");
    assert_eq!(mailbox.poll("r2").unwrap()["status"], "cancelled");

    // cancel_all drops anything still queued.
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r3",
            "ui": {"kind": "input", "question": "X?"}
        }))
        .unwrap();
    bridge.cancel_all();
    assert_eq!(mailbox.poll("r3").unwrap()["status"], "cancelled");

    // A detached mailbox rejects new prompts (headless contract).
    bridge.shutdown();
    assert!(mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r4", "ui": {"kind": "input"}
        }))
        .is_err());
}
