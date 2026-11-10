//! Runtime acceptance for re-attaching the session dialog mailbox after TUI
//! retirement.
//!
//! The same session mailbox is intentionally reused by a reconstructed TUI;
//! shutdown cancels the old turn, and a newly attached bridge may accept later
//! dialogs without creating a second mailbox.

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
fn a_fresh_tui_bridge_can_reattach_after_shutdown() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let old_bridge = AskUserBridge::new(mailbox.clone());
    assert!(old_bridge.attach());
    open(&mailbox, "before-restart");
    assert_eq!(
        old_bridge.take_pending_for_tui().unwrap().request_id,
        "before-restart"
    );

    old_bridge.shutdown();

    assert_eq!(
        mailbox.poll("before-restart").unwrap()["status"],
        "cancelled"
    );
    assert!(!mailbox.is_attached());

    let fresh_bridge = AskUserBridge::new(mailbox.clone());
    fresh_bridge.attach();
    open(&mailbox, "after-restart");
    assert_eq!(
        fresh_bridge.take_pending_for_tui().unwrap().request_id,
        "after-restart"
    );
    fresh_bridge.respond("after-restart", serde_json::json!("accepted"));
    assert_eq!(mailbox.poll("after-restart").unwrap()["answer"], "accepted");

    fresh_bridge.shutdown();
}

#[test]
fn reattachment_does_not_resurrect_the_retired_dialog() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let old_bridge = AskUserBridge::new(mailbox.clone());
    assert!(old_bridge.attach());
    open(&mailbox, "retired");
    assert_eq!(
        old_bridge.take_pending_for_tui().unwrap().request_id,
        "retired"
    );
    old_bridge.shutdown();

    let fresh_bridge = AskUserBridge::new(mailbox.clone());
    fresh_bridge.attach();
    assert!(fresh_bridge.take_pending_for_tui().is_none());
    assert_eq!(mailbox.poll("retired").unwrap()["status"], "cancelled");

    fresh_bridge.shutdown();
}
