//! Test-only source-order acceptance for native event/reload retirement.
//!
//! This does not open a terminal or run the live event loop. It protects the
//! host's existing cleanup tail while keeping PTY and delivery claims out of
//! the bounded slice.

const HOST_SOURCE: &str = include_str!("../../interactive_tui.rs");

fn after_shutdown(needle: &str) -> usize {
    let shutdown = HOST_SOURCE
        .rfind("    // ---- Shutdown ----")
        .expect("interactive TUI shutdown tail must remain present");
    HOST_SOURCE[shutdown..]
        .find(needle)
        .map(|offset| shutdown + offset)
        .unwrap_or_else(|| panic!("shutdown source is missing marker: {needle:?}"))
}

#[test]
fn event_and_reload_handles_retire_only_after_native_settlement() {
    let settle = after_shutdown(
        "settlement::settle_native(harness, state.session_write_tasks.seal()).await",
    );
    let tick = after_shutdown("tick_handle.abort();");
    let drain = after_shutdown("if let Some(handle) = drain_handle");
    let mailbox = after_shutdown("reload_context.mailbox.clear();");
    let reload = after_shutdown("reload_bridge_handle.abort();");
    let tui = after_shutdown("tui.stop(Default::default());");

    assert!(settle < tick);
    assert!(tick < drain);
    assert!(drain < mailbox);
    assert!(mailbox < reload);
    assert!(reload < tui);
}

#[test]
fn reload_bridge_is_installed_before_its_shutdown_cleanup() {
    let install = HOST_SOURCE
        .find("reload_context.mailbox.install(reload_sig_tx);")
        .expect("reload mailbox installation must remain present");
    let bridge = HOST_SOURCE
        .find("let reload_bridge_handle = tokio::spawn(async move {")
        .expect("reload bridge task must remain present");
    let clear = after_shutdown("reload_context.mailbox.clear();");
    let abort = after_shutdown("reload_bridge_handle.abort();");

    assert!(install < bridge);
    assert!(clear < abort);
}

#[test]
fn event_consumer_is_kept_live_through_native_settlement() {
    let source = HOST_SOURCE;
    assert!(source.contains("the streaming event consumer stays live through producer settlement"));

    let settle = after_shutdown(
        "settlement::settle_native(harness, state.session_write_tasks.seal()).await",
    );
    let event_abort = after_shutdown("if let Some(handle) = drain_handle");
    assert!(settle < event_abort);
}
