use super::*;
use serde_json::{json, Value};

fn action(op: &str, id: &str) -> Value {
    json!({"op":op,"requestId":id,"toolCallId":"same-call","ui":{},"answer":"injected",
        "sessionId":"forged","ui_session":"forged"})
}

fn run(
    mailbox: &UiDialogMailbox,
    session: Option<&str>,
    owner: u64,
    op: &str,
    id: &str,
) -> Result<Value, String> {
    mailbox.handle_with_owner(action(op, id), session.map(str::to_owned), owner)
}

#[test]
fn root_and_two_children_identical_local_ids_never_share_answers_or_capabilities() {
    let mailbox = UiDialogMailbox::new();
    mailbox.attach();
    let scopes = [(None, 11), (Some("child-a"), 12), (Some("child-b"), 13)];
    let mut requests = Vec::new();
    for (session, owner) in scopes {
        let opened = run(&mailbox, session, owner, "open", "same-request").unwrap();
        assert_eq!(opened["requestId"], "same-request");
        let request = mailbox.take_pending().unwrap();
        assert_eq!(request.session_id.as_deref(), session);
        assert_ne!(request.request_id, "same-request");
        assert!(uuid::Uuid::parse_str(&request.request_id).is_ok());
        requests.push(request);
    }
    for i in 0..3 {
        for j in 0..3 {
            if i == j {
                continue;
            }
            assert_ne!(requests[i].request_id, requests[j].request_id);
            for op in ["poll", "cancel", "respond", "response"] {
                assert!(run(
                    &mailbox,
                    scopes[i].0,
                    scopes[i].1,
                    op,
                    &requests[j].request_id
                )
                .is_err());
            }
            // Opening another scope's wire string is a NEW local request;
            // it neither discloses an answer nor claims that capability.
            let opened = run(
                &mailbox,
                scopes[i].0,
                scopes[i].1,
                "open",
                &requests[j].request_id,
            )
            .unwrap();
            assert_eq!(opened["status"], "pending");
            let spoof = mailbox.take_pending().unwrap();
            assert_ne!(spoof.request_id, requests[j].request_id);
            run(
                &mailbox,
                scopes[i].0,
                scopes[i].1,
                "cancel",
                &requests[j].request_id,
            )
            .unwrap();
            run(
                &mailbox,
                scopes[i].0,
                scopes[i].1,
                "poll",
                &requests[j].request_id,
            )
            .unwrap();
        }
    }
    assert!(mailbox
        .respond_wire("same-request", json!("spoof"))
        .is_err());
    assert!(mailbox.cancel_wire("same-request").is_err());
    for i in [2, 0, 1] {
        mailbox
            .respond_wire(&requests[i].request_id, json!(i))
            .unwrap();
        for (j, (session, owner)) in scopes.iter().enumerate() {
            if j != i && mailbox.is_pending_wire(&requests[j].request_id) {
                assert_eq!(
                    run(&mailbox, *session, *owner, "poll", "same-request").unwrap()["status"],
                    "pending"
                );
            }
        }
        let result = run(&mailbox, scopes[i].0, scopes[i].1, "poll", "same-request").unwrap();
        assert_eq!(result["requestId"], "same-request");
        assert_eq!(result["answer"], i);
        assert!(mailbox
            .respond_wire(&requests[i].request_id, json!("late"))
            .is_err());
    }
}

#[test]
fn sibling_invocations_and_plugins_within_one_session_are_isolated() {
    let mailbox = UiDialogMailbox::new();
    mailbox.attach();
    for owner in [1, 2, 3] {
        run(&mailbox, Some("child"), owner, "open", "local").unwrap();
    }
    let requests: Vec<_> = (0..3).map(|_| mailbox.take_pending().unwrap()).collect();
    run(&mailbox, Some("child"), 2, "respond", "local").unwrap();
    assert_eq!(
        run(&mailbox, Some("child"), 1, "poll", "local").unwrap()["status"],
        "pending"
    );
    assert_eq!(
        run(&mailbox, Some("child"), 2, "poll", "local").unwrap()["answer"],
        "injected"
    );
    run(&mailbox, Some("child"), 1, "cancel", "local").unwrap();
    assert_eq!(
        run(&mailbox, Some("child"), 3, "poll", "local").unwrap()["status"],
        "pending"
    );
    assert!(run(&mailbox, Some("child"), 4, "poll", "local").is_err());
    assert!(run(&mailbox, None, 1, "poll", "local").is_err());
    assert!(mailbox.is_pending_wire(&requests[2].request_id));
}

#[test]
fn qualified_tool_cancel_and_late_cleanup_retain_original_source() {
    let mailbox = UiDialogMailbox::new();
    mailbox.attach();
    for session in [None, Some("child-a"), Some("child-b")] {
        run(&mailbox, session, 0, "open", "local").unwrap();
    }
    let root = mailbox.take_pending().unwrap();
    let child_a = mailbox.take_pending().unwrap();
    let child_b = mailbox.take_pending().unwrap();
    // This retained owner is used after the originating execution scope exits.
    let captured_session = child_a.session_id.clone();
    mailbox.cancel_tool_call_with_session("same-call", captured_session.clone());
    assert!(!mailbox.is_pending_wire(&child_a.request_id));
    assert!(mailbox.is_pending_wire(&root.request_id));
    assert!(mailbox.is_pending_wire(&child_b.request_id));
    assert_eq!(
        run(&mailbox, Some("child-a"), 0, "poll", "local").unwrap()["status"],
        "cancelled"
    );
    assert!(mailbox
        .respond_wire(&child_a.request_id, json!(true))
        .is_err());
    // Late opens, including another plugin's approval, cannot escape the
    // source tool cancellation, but identical IDs in siblings/root can open.
    assert_eq!(
        run(&mailbox, Some("child-a"), 88, "open", "late").unwrap()["status"],
        "cancelled"
    );
    assert_eq!(
        run(&mailbox, Some("child-b"), 88, "open", "late").unwrap()["status"],
        "pending"
    );
    mailbox.cancel_tool_call("same-call");
    assert_eq!(mailbox.poll("local").unwrap()["status"], "cancelled");
    assert_eq!(
        run(&mailbox, Some("child-b"), 0, "poll", "local").unwrap()["status"],
        "pending"
    );
    assert_eq!(captured_session.as_deref(), Some("child-a"));
}

#[test]
fn reopen_has_fresh_wire_and_late_answers_cannot_hit_reused_local_id() {
    let mailbox = UiDialogMailbox::new();
    mailbox.attach();
    run(&mailbox, Some("child"), 3, "open", "reuse").unwrap();
    let old = mailbox.take_pending().unwrap();
    mailbox.cancel_wire(&old.request_id).unwrap();
    assert!(mailbox.respond_wire(&old.request_id, json!(true)).is_err());
    assert_eq!(
        run(&mailbox, Some("child"), 3, "poll", "reuse").unwrap()["status"],
        "cancelled"
    );
    run(&mailbox, Some("child"), 3, "open", "reuse").unwrap();
    let new = mailbox.take_pending().unwrap();
    assert_ne!(old.request_id, new.request_id);
    assert!(mailbox.respond_wire(&old.request_id, json!(true)).is_err());
    assert!(mailbox.is_pending_wire(&new.request_id));
    mailbox.detach();
    assert_eq!(
        run(&mailbox, Some("child"), 3, "poll", "reuse").unwrap()["status"],
        "cancelled"
    );
}

#[test]
fn root_legacy_plugin_envelope_and_native_helpers_remain_compatible() {
    let mailbox = UiDialogMailbox::new();
    mailbox.attach();
    assert_eq!(
        mailbox.handle(action("open", "root")).unwrap()["requestId"],
        "root"
    );
    let request = mailbox.take_pending().unwrap();
    assert!(request.session_id.is_none());
    assert_eq!(
        mailbox.handle(action("poll", "root")).unwrap()["status"],
        "pending"
    );
    mailbox
        .respond("root", json!({"values":["Allow once"]}))
        .unwrap();
    let result = mailbox.handle(action("poll", "root")).unwrap();
    assert_eq!(result["requestId"], "root");
    assert_eq!(result["answer"]["values"][0], "Allow once");
    assert!(!mailbox.is_pending_wire(&request.request_id));
}
