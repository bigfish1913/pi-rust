//! Runtime acceptance for generation-guarded dialog bridge retirement.
//!
//! A stale bridge handle may outlive a TUI reconstruction. Its shutdown must
//! clear only its own visible slot and must not detach or cancel the fresh
//! bridge's session mailbox.

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
fn stale_shutdown_does_not_detach_a_fresh_bridge_generation() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let stale_bridge = AskUserBridge::new(mailbox.clone());
    stale_bridge.attach();
    stale_bridge.shutdown();

    let fresh_bridge = AskUserBridge::new(mailbox.clone());
    fresh_bridge.attach();
    open(&mailbox, "fresh");
    assert_eq!(
        fresh_bridge.take_pending_for_tui().unwrap().request_id,
        "fresh"
    );

    stale_bridge.shutdown();

    assert!(mailbox.is_attached());
    assert_eq!(mailbox.poll("fresh").unwrap()["status"], "pending");
    fresh_bridge.respond("fresh", serde_json::json!("kept"));
    assert_eq!(mailbox.poll("fresh").unwrap()["answer"], "kept");

    fresh_bridge.shutdown();
}

#[test]
fn stale_shutdown_cannot_cancel_a_queued_fresh_dialog() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let stale_bridge = AskUserBridge::new(mailbox.clone());
    stale_bridge.attach();
    stale_bridge.shutdown();

    let fresh_bridge = AskUserBridge::new(mailbox.clone());
    fresh_bridge.attach();
    open(&mailbox, "queued");
    stale_bridge.shutdown();

    assert!(mailbox.is_attached());
    assert_eq!(
        fresh_bridge.take_pending_for_tui().unwrap().request_id,
        "queued"
    );
    fresh_bridge.cancel("queued");
    assert_eq!(mailbox.poll("queued").unwrap()["status"], "cancelled");

    fresh_bridge.shutdown();
}
