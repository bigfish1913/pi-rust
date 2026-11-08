//! Runtime acceptance for a bridge whose attachment attempt failed.
//!
//! A bridge that never acquired a local generation must not use the mailbox's
//! current generation during shutdown. The owner remains usable and its dialog
//! is neither detached nor cancelled.

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
fn unowned_failed_bridge_shutdown_cannot_detach_current_owner() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let owner = AskUserBridge::new(mailbox.clone());
    assert!(owner.attach());

    // Model the attach result being rejected at generation exhaustion. The
    // bridge must clear the constructor's observed generation before shutdown;
    // otherwise it could infer ownership from the current owner.
    let failed_bridge = AskUserBridge::new(mailbox.clone());
    assert!(!failed_bridge.record_attach_result(None));
    failed_bridge.shutdown();

    assert!(mailbox.is_attached());
    open(&mailbox, "owner-dialog");
    assert_eq!(
        owner.take_pending_for_tui().unwrap().request_id,
        "owner-dialog"
    );
    owner.respond("owner-dialog", serde_json::json!("kept"));
    assert_eq!(mailbox.poll("owner-dialog").unwrap()["answer"], "kept");

    owner.shutdown();
}
