//! Regression probes for extension ARGUMENT suggestions in the real TUI
//! pipeline: Tab through `accept_top_suggestion` and Enter through
//! `resolve_enter_completion`, i.e. the paths the provider's own unit tests
//! (`interactive_tui::extension_arguments`) never reach.
//!
//! Three behaviours are load-bearing and were each broken once:
//!
//! 1. accepting an argument completion must keep the caret AFTER the trailing
//!    space, or the message body the user types next glues onto the target
//!    (`/msg apmprod@h` + `hi` became `/msg apmprod@hhi `);
//! 2. the separating space between the command token and the accepted value is
//!    an ARGUMENT-only concern — applying it to an `@`-file completion turns
//!    `@notes.txt` into `@ notes.txt`;
//! 3. a path-looking argument of a slash command is NOT a completion context:
//!    `/cmd src/ma` + Enter must submit, and ↑/↓ must stay with the editor.

use super::*;
use crate::session::ExtensionSessionCell;
use rpi_extensions::{ExtensionRegistry, ExtensionSession};
use rpi_plugin_sdk::{StbString, StbStringRef};
use std::ffi::c_void;
use std::time::{Duration, Instant};

/// A fast plugin: returns the fixed FULL instance list (full-candidate-list
/// contract — the host asks once per refresh window and narrows locally).
extern "C" fn list_handler(
    _request_json: StbStringRef,
    out: *mut StbString,
    _ud: *mut c_void,
) -> i32 {
    let payload = serde_json::json!({
        "items": [
            {"value": "apmprod@h", "label": "1. apmprod（微信）", "description": "/export/home"},
            {"value": "apserver@h", "label": "2. apserver", "description": "/data"},
            {"value": "office@h", "label": "3. office（当前）", "description": "/Users/me"}
        ]
    });
    unsafe {
        *out = StbString::from_string(payload.to_string());
    }
    0
}

/// A TUI state whose autocomplete is the live extension-argument provider,
/// backed by a session with a real `/msg` suggester registered.
fn argument_state() -> Arc<TuiState> {
    let mut registry = ExtensionRegistry::new();
    registry.register_argument_suggester(
        "msg",
        list_handler,
        rpi_plugin_sdk::callbacks::free_string,
        std::ptr::null_mut(),
    );
    let session = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        rpi_extensions::PluginKeepalive::empty(),
        Vec::new(),
        None,
    );
    let cell: ExtensionSessionCell = Arc::new(Mutex::new(session));
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(ExtensionArgumentProvider::new(cell)));
    state
}

/// Drive the real pipeline until the first background frame lands (the FFI
/// call runs on the session's suggest worker), so the assertions below run
/// against a warm cache exactly like a live keystroke does.
fn wait_for_popup(state: &Arc<TuiState>, text: &str, cursor: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if state.autocomplete.get_suggestions(text, cursor).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("background frame never landed for {text:?}");
}

fn type_text(editor: &Arc<Editor>, text: &str) {
    for character in text.chars() {
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        ));
    }
}

#[test]
fn extension_argument_tab_at_the_command_boundary_takes_the_first_target_and_keeps_the_caret_past_the_space(
) {
    let state = argument_state();
    wait_for_popup(&state, "/msg", 4);
    let editor = state.editor.clone();
    editor.set_text("/msg");
    editor.set_cursor(0, 4);

    assert!(
        accept_top_suggestion(&state, &editor),
        "Tab on `/msg` must complete the first argument"
    );
    let text = editor.get_text();
    assert_eq!(text, "/msg apmprod@h ");
    assert_eq!(
        editor_cursor_offset(&editor, &text),
        text.len(),
        "the caret must sit AFTER the trailing space"
    );

    // The user's next keystrokes are the message body: they must land after the
    // separating space, never glued to the instance token.
    type_text(&editor, "hi");
    assert_eq!(editor.get_text(), "/msg apmprod@h hi");
}

#[test]
fn extension_argument_tab_after_a_typed_prefix_narrows_and_keeps_the_caret_past_the_space() {
    let state = argument_state();
    wait_for_popup(&state, "/msg ", 5);
    let editor = state.editor.clone();
    editor.set_text("/msg ap");
    editor.set_cursor(0, 7);

    assert!(
        accept_top_suggestion(&state, &editor),
        "Tab on `/msg ap` must complete the narrowed target"
    );
    let text = editor.get_text();
    assert_eq!(text, "/msg apmprod@h ");
    assert_eq!(editor_cursor_offset(&editor, &text), text.len());

    type_text(&editor, "hi");
    assert_eq!(editor.get_text(), "/msg apmprod@h hi");
}

#[test]
fn extension_argument_enter_on_a_fully_typed_command_runs_the_command_instead_of_picking_a_target()
{
    let state = argument_state();
    wait_for_popup(&state, "/msg", 4);
    let editor = state.editor.clone();
    editor.set_text("/msg");
    editor.set_cursor(0, 4);

    // No completion decision: the key falls through to the editor's submit, so
    // `/msg` runs its own flow instead of silently targeting the first instance.
    assert_eq!(enter_completion_action(&state, &editor), None);
}

#[test]
fn extension_argument_enter_in_the_argument_region_accepts_without_submitting() {
    let state = argument_state();
    wait_for_popup(&state, "/msg ap", 7);
    let editor = state.editor.clone();
    editor.set_text("/msg ap");
    editor.set_cursor(0, 7);

    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyOnly);
    assert!(completion.apply(&state, &editor));
    let text = editor.get_text();
    assert_eq!(text, "/msg apmprod@h ");
    assert_eq!(editor_cursor_offset(&editor, &text), text.len());
    assert!(
        completion.consumes_enter(),
        "the target is applied but the command is not submitted yet"
    );

    type_text(&editor, "hi");
    assert_eq!(editor.get_text(), "/msg apmprod@h hi");
}

#[test]
fn extension_argument_path_like_argument_never_hijacks_enter_or_arrows() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temporary.path().join("src")).unwrap();
    std::fs::write(temporary.path().join("src/main.rs"), "fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    editor.set_text("/cmd src/ma");
    editor.set_cursor(0, 11);

    assert!(
        state
            .autocomplete
            .get_suggestions("/cmd src/ma", 11)
            .is_some(),
        "fixture must produce a bare-word path popup inside the slash draft"
    );
    assert!(
        resolve_enter_completion(&state, &editor).is_none(),
        "Enter must submit `/cmd src/ma`, not rewrite its argument"
    );
    assert!(
        !navigate_autocomplete(&state, &editor, 1),
        "↑/↓ belong to the editor/history inside a slash-command draft"
    );
}

/// The separating space is an ARGUMENT-only concern: an `@`-file completion
/// replaces the span right after `@`, which is also `start > 0`, so a shared
/// "insert a leading space when start > 0" rule turns `@notes.txt` into
/// `@ notes.txt`. This pins the path branch to no separator.
#[test]
fn extension_argument_path_completion_does_not_get_a_leading_space() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(temporary.path().join("notes.txt"), "fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    editor.set_text("@not");
    editor.set_cursor(0, 4);

    assert!(
        accept_top_suggestion(&state, &editor),
        "Tab must complete the path"
    );
    let text = editor.get_text();
    assert!(
        text.starts_with("@/") || text.starts_with("@notes"),
        "the `@` reference must not gain a separator, got {text:?}"
    );
    assert!(
        !text.starts_with("@ "),
        "`@` must stay attached to the path: {text:?}"
    );
}
