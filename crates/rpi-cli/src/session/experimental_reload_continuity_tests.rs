//! Test-only acceptance for session-scoped reload mailbox continuity.
//!
//! The live reload routine is intentionally not invoked here: it would require
//! extension/resource discovery. These checks pin the existing wiring contract
//! without loading a provider or dynamic library.

const SESSION_SOURCE: &str = include_str!("../session.rs");
const TUI_SOURCE: &str = include_str!("../interactive_tui.rs");

#[test]
fn fresh_reload_bridge_captures_the_session_mailbox() {
    let mailbox_clone = SESSION_SOURCE
        .find("let reload_cb = rpi_extensions::reload_callback_from_mailbox(ctx.mailbox.clone());")
        .expect("reload must clone the session mailbox");
    let fresh_bridge = SESSION_SOURCE
        .find("let fresh_bridge = rpi_extensions::ActionBridge::with_reload_and_ui(")
        .expect("reload must build the fresh action bridge");
    let publish = SESSION_SOURCE
        .find("*ctx.action_bridge.lock().unwrap() = Some(Arc::clone(&fresh_bridge));")
        .expect("reload must publish the fresh bridge");

    assert!(mailbox_clone < fresh_bridge);
    assert!(fresh_bridge < publish);
    assert!(SESSION_SOURCE.matches("ctx.mailbox.clone()").count() >= 1);
}

#[test]
fn session_reload_does_not_retire_the_tui_mailbox() {
    let tui_install = TUI_SOURCE
        .find("reload_context.mailbox.install(reload_sig_tx);")
        .expect("TUI must install its bridge sender");
    let tui_clear = TUI_SOURCE
        .find("reload_context.mailbox.clear();")
        .expect("TUI shutdown must clear its sender");
    let session_clear = SESSION_SOURCE.matches("mailbox.clear()").count();

    assert!(tui_install < tui_clear);
    assert_eq!(
        session_clear, 0,
        "session reload must not clear the TUI-owned mailbox"
    );
    assert!(SESSION_SOURCE.contains("pub mailbox: rpi_extensions::ReloadMailbox"));
}
