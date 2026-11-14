//! Remote approval frontend over the shared `UiDialogMailbox`.
//!
//! Mirrors what `--server` does: the request is published as a permission
//! dialog and the client's answer is translated back into an approval
//! decision. Only the remote frontend is installed here, so `ask` cannot fall
//! back to a local TUI.

use std::path::Path;
use std::time::Duration;

use rpi_cli::sandbox_approval;
use rpi_sandbox::{ApprovalDecision, ApprovalScope, WriteOp};

#[tokio::test(flavor = "multi_thread")]
async fn remote_permission_answer_becomes_a_decision() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    mailbox.attach();
    sandbox_approval::install_remote(mailbox.clone());

    let target = Path::new("/tmp/rpi-remote-approval.txt");
    let task = tokio::spawn(async move {
        sandbox_approval::ask(WriteOp::Create, target, "remote.txt", Some(3)).await
    });

    // The server-side projection would take the request and emit it.
    let request = loop {
        if let Some(request) = mailbox.take_pending() {
            break request;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(request.request_id.starts_with("sandbox:"));
    assert_eq!(request.ui["purpose"], "permission");
    assert_eq!(request.ui["options"][0]["permission"], "allow_once");

    mailbox
        .respond(
            &request.request_id,
            serde_json::json!({"permission": "allow_once"}),
        )
        .unwrap();
    let decision = task.await.unwrap().expect("remote frontend present");
    assert_eq!(
        decision,
        ApprovalDecision::Allow {
            scope: ApprovalScope::Once,
            recursive: false,
        }
    );

    // Cancel path: closing the dialog denies.
    let task = tokio::spawn(async move {
        sandbox_approval::ask(
            WriteOp::Overwrite,
            Path::new("/tmp/rpi-remote-cancel.txt"),
            "cancel.txt",
            None,
        )
        .await
    });
    let request = loop {
        if let Some(request) = mailbox.take_pending() {
            break request;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    mailbox.cancel(&request.request_id).unwrap();
    assert_eq!(task.await.unwrap(), Some(ApprovalDecision::Deny));
}
