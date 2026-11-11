//! Runtime acceptance for ask-user/dialog retirement during TUI shutdown.
//!
//! The mailbox is the real rpi-extensions implementation; no terminal or
//! plugin process is started.

use super::AskUserBridge;

fn open(mailbox: &rpi_extensions::UiDialogMailbox, request_id: &str) {
    mailbox
        .handle(serde_json::json!({
            "op": "open",
            "requestId": request_id,
            "toolCallId": format!("call-{request_id}"),
            "ui": {"kind": "input", "question": "Continue?"}
        }))
        .expect("attached UI accepts dialog");
}

#[test]
fn shutdown_cancels_visible_dialog_and_rejects_late_open() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let bridge = AskUserBridge::new(mailbox.clone());
    assert!(bridge.attach());
    open(&mailbox, "visible");
    assert_eq!(bridge.take_pending_for_tui().unwrap().request_id, "visible");

    bridge.shutdown();

    assert_eq!(mailbox.poll("visible").unwrap()["status"], "cancelled");
    assert!(bridge.take_pending_for_tui().is_none());
    assert!(mailbox
        .handle(serde_json::json!({
            "op": "open",
            "requestId": "late",
            "toolCallId": "call-late",
            "ui": {"kind": "input"}
        }))
        .is_err());
}

#[test]
fn repeated_shutdown_cancels_queued_dialogs_once_and_stays_detached() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let bridge = AskUserBridge::new(mailbox.clone());
    assert!(bridge.attach());
    open(&mailbox, "active");
    open(&mailbox, "queued");
    assert_eq!(bridge.take_pending_for_tui().unwrap().request_id, "active");

    bridge.shutdown();
    bridge.shutdown();

    assert_eq!(mailbox.poll("active").unwrap()["status"], "cancelled");
    assert_eq!(mailbox.poll("queued").unwrap()["status"], "cancelled");
    assert!(!mailbox.is_attached());
}
