//! Test-only source-order acceptance for the explicit native TUI shutdown tail.
//!
//! These checks deliberately inspect the existing host wiring rather than opening
//! a terminal. They protect the bounded ordering contract without claiming PTY
//! delivery, event rendering, or root-future cleanup.

const HOST_SOURCE: &str = include_str!("../../interactive_tui.rs");

// Git may materialize this source as CRLF on Windows. Normalize before
// checking source-order sentinels so these tests assert wiring, not checkout
// line endings.
fn normalized_host_source() -> String {
    HOST_SOURCE.replace("\r\n", "\n")
}

fn offset(source: &str, needle: &str) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("shutdown source is missing marker: {needle:?}"))
}

#[test]
fn native_shutdown_settles_before_retiring_runtime_tasks() {
    let source = normalized_host_source();
    let shutdown = source
        .rfind("    // ---- Shutdown ----")
        .expect("shutdown tail must remain present");
    let native = &source[shutdown..];
    let native_branch_end = native
        .find("    } else {\n        // Let anything the lane has already committed")
        .expect("native shutdown branch must retain a flag-off branch");
    let native_branch = &native[..native_branch_end];

    let stop_input = offset(native_branch, "*running.lock().unwrap() = false;");
    let cancel_dialog = offset(native_branch, "ask_user_bridge.shutdown();");
    let join_input = offset(
        native_branch,
        "if key_handle.take().unwrap().await.is_err()",
    );
    let reconcile = offset(native_branch, "lane.abort_and_reconcile().await");
    let settle = offset(
        native_branch,
        "settlement::settle_native(harness, state.session_write_tasks.seal()).await",
    );
    assert!(stop_input < cancel_dialog);
    assert!(cancel_dialog < join_input);
    assert!(join_input < reconcile);
    assert!(reconcile < settle);

    for marker in [
        "for handle in update_check_handles",
        "tick_handle.abort();",
        "if let Some(handle) = drain_handle",
        "reload_context.mailbox.clear();",
        "reload_bridge_handle.abort();",
        "tui.stop(Default::default());",
    ] {
        assert!(
            settle < offset(native, marker),
            "native settlement must precede runtime retirement marker {marker:?}"
        );
    }
}

#[test]
fn flag_off_shutdown_does_not_invoke_native_settlement() {
    let source = normalized_host_source();
    let shutdown = source
        .rfind("    // ---- Shutdown ----")
        .expect("shutdown tail must remain present");
    let tail = &source[shutdown..];
    let flag_off = tail
        .find("    } else {\n        // Let anything the lane has already committed")
        .expect("flag-off shutdown branch must remain present");
    let disabled_branch = &tail[flag_off..];
    assert!(!disabled_branch.contains("settlement::settle_native"));
    assert!(disabled_branch.contains("lane.wait_for_idle().await"));
    assert!(disabled_branch.contains("lane.drain_session().await"));
}

#[test]
fn native_shutdown_keeps_dialog_shutdown_idempotent_and_single_settlement() {
    let source = normalized_host_source();
    let shutdown = source
        .rfind("    // ---- Shutdown ----")
        .expect("shutdown tail must remain present");
    let tail = &source[shutdown..];
    assert_eq!(tail.matches("ask_user_bridge.shutdown();").count(), 2);
    assert_eq!(tail.matches("lane.abort_and_reconcile().await").count(), 1);
    assert_eq!(tail.matches("settlement::settle_native").count(), 1);
}
