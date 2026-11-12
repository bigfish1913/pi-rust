//! Source-level guard for the startup/update transcript wiring.
//!
//! The durable-preview merge (85b66d1) silently restored the pre-fix wiring in
//! `interactive_tui.rs`: the startup banner, launch notices and update cards
//! were appended to the chat root again, and runtime notes bypassed the
//! history notice slot. The `history` component tests kept passing because
//! they build the containers themselves, so nothing failed — the regression
//! only showed up on screen as output rendered around (or above) the update
//! card. These assertions pin the host wiring, not the component behaviour.

const HOST_SOURCE: &str = include_str!("../../interactive_tui.rs");

// Git may materialize this source as CRLF on Windows. Normalize before
// checking sentinels so these tests assert wiring, not checkout line endings.
fn host_source() -> String {
    HOST_SOURCE.replace("\r\n", "\n")
}

fn offset(source: &str, needle: &str) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("host source is missing wiring marker: {needle:?}"))
}

#[test]
fn startup_banner_and_launch_notices_land_in_the_retained_header() {
    let source = host_source();
    let header = offset(
        &source,
        "let history_page = history::HistoryUi::with_native_durable(args.native_durable);",
    );
    let chat = offset(&source, "let chat_container = Arc::new(Container::new());");
    assert!(
        header < chat,
        "the retained history UI must exist before any startup content is added"
    );
    for marker in [
        "Some(add_welcome_message_with_capabilities(\n            &history_page.header,",
        "add_note_message(&history_page.header, notice);",
        "crate::extras::maybe_first_time_setup(&history_page.header);",
        "maybe_add_startup_changelog(\n        harness,\n        &history_page.header,",
        // The same instance must be the one the TUI state owns, otherwise the
        // header is populated on a container nothing renders.
        "    let state = Arc::new(TuiState {\n        session_write_tasks: settlement::SessionWriteTasks::new(args.native_durable),\n        history_page,",
    ] {
        assert!(
            source.contains(marker),
            "startup content must target the retained header: {marker:?}"
        );
    }
    for marker in [
        "Some(add_welcome_message_with_capabilities(\n            &chat_container,",
        "add_note_message(&chat_container, notice);",
        "crate::extras::maybe_first_time_setup(&chat_container);",
        "maybe_add_startup_changelog(\n        harness,\n        &chat_container,",
    ] {
        assert!(
            !source.contains(marker),
            "startup content must not be appended to the chat root: {marker:?}"
        );
    }
}

#[test]
fn update_checks_target_the_reserved_update_slot() {
    let source = host_source();
    let slot = offset(
        &source,
        "let update_container = state.history_page.updates.clone();",
    );
    let add = offset(&source, "add_update_notices(&update_container, &report);");
    assert!(
        slot < add,
        "the update slot must be captured before the background check renders"
    );
    for marker in [
        "add_update_notices(&chat_container, &report)",
        "add_update_notices(&rpi_chat, &report)",
        "let rpi_chat = chat_container.clone();",
    ] {
        assert!(
            !source.contains(marker),
            "late update cards must not be appended to the chat root: {marker:?}"
        );
    }
}

#[test]
fn runtime_notes_go_through_the_history_notice_slot() {
    let source = host_source();
    for marker in [
        "add_note_message(&chat_container",
        "add_error_message(&chat_container",
        "add_note_message(&state.chat_container",
        "add_error_message(&state.chat_container",
        "add_note_message(&state_for_key.chat_container",
        "add_error_message(&state_for_key.chat_container",
        "add_note_message(&ctx_for_cb.chat,",
        "add_error_message(&ctx_for_cb.chat,",
        "let chat = ctx_for_cb.chat.clone();",
        "let chat = state_for_key.chat_container.clone();",
    ] {
        assert!(
            !source.contains(marker),
            "runtime notices must use `notice_container()`: {marker:?}"
        );
    }
}
