//! Test-only source-contract acceptance for dialog mailbox continuity across reload.
//!
//! The live reload path is not invoked here; this keeps the check independent
//! of extension/resource discovery while pinning the session-scoped mailbox
//! ownership contract.

const SESSION_SOURCE: &str = include_str!("../session.rs");
const TUI_SOURCE: &str = include_str!("../interactive_tui.rs");
const DIALOG_SOURCE: &str = include_str!("../interactive_tui/extensions.rs");

#[test]
fn initial_and_fresh_bridges_receive_the_session_dialog_mailbox() {
    let initial_mailbox = SESSION_SOURCE
        .find("ui_dialog_mailbox.clone(),")
        .expect("initial action bridge must receive the dialog mailbox");
    let fresh_bridge = SESSION_SOURCE
        .find("let fresh_bridge = rpi_extensions::ActionBridge::with_reload_and_ui(")
        .expect("reload must build a fresh action bridge");
    let fresh_mailbox = SESSION_SOURCE
        .find("ctx.ui_dialog.clone(),")
        .expect("fresh action bridge must receive the session dialog mailbox");
    let publish = SESSION_SOURCE
        .find("*ctx.action_bridge.lock().unwrap() = Some(Arc::clone(&fresh_bridge));")
        .expect("reload must publish the fresh bridge");

    assert!(initial_mailbox < fresh_bridge);
    assert!(fresh_bridge < fresh_mailbox);
    assert!(fresh_mailbox < publish);
}

#[test]
fn only_tui_shutdown_detaches_the_session_dialog_mailbox() {
    assert_eq!(
        SESSION_SOURCE.matches("ui_dialog.detach()").count(),
        0,
        "reload/session code must not detach the TUI-owned mailbox"
    );
    assert_eq!(
        SESSION_SOURCE.matches("ui_dialog_mailbox.detach()").count(),
        0,
        "initial session construction must not detach the mailbox"
    );
    assert!(TUI_SOURCE.contains("ask_user_bridge.shutdown();"));
    assert!(DIALOG_SOURCE.contains("self.mailbox.detach_if_generation(generation)"));
}
