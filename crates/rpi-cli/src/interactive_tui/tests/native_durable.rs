//! Native-only structured slash arguments and plain-text view plumbing.
use super::*;

#[test]
fn native_durable_preserves_structured_arguments_while_legacy_keeps_normalization() {
    let payload = "{\n  \"action\": \"enqueue\", \"requestId\": \"spaces\", \"text\": \"  keep  界  spaces  \"\n}  ";
    let line = format!("  /durable  {payload}");
    let mut native = build_builtin_registry();
    assert!(native.find("/durable").is_none(), "no default opt-in");
    native.register(Arc::new(NativeDurableCommand));
    let command = native.find("/durable").unwrap();
    assert!(command.preserve_args_whitespace());
    assert_eq!(slash_arguments(&line, command.as_ref()), payload);
    assert_eq!(slash_arguments("/durable", command.as_ref()), "");
    assert_eq!(
        slash_arguments(" /durable   help  ", command.as_ref()),
        "help  "
    );
    assert!(native
        .visible_entries()
        .iter()
        .any(|entry| entry.name == "/durable"));

    // A legacy extension with the same name retains the old policy. Never
    // select the structured path merely by matching the command-name string.
    struct Legacy;
    impl SlashCommand for Legacy {
        fn name(&self) -> &str {
            "/durable"
        }
        fn execute(&self, _: &CommandContext, _: &str) {
            panic!("pure routing test")
        }
    }
    let legacy = Legacy;
    assert!(!legacy.preserve_args_whitespace());
    assert_eq!(
        slash_arguments(&line, &legacy),
        line.split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    );
    let builtin = native.find("/help").unwrap();
    assert_eq!(
        slash_arguments(" /help   a \t b  ", builtin.as_ref()),
        "a b"
    );
}

#[tokio::test]
async fn native_durable_argument_and_view_pipeline_preserves_business_data_without_extra_writes() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let payload = serde_json::json!({"action":"enqueue","requestId":"native-view-spaces",
        "text":"keep  界  spaces\n\u{1b}\u{85}  end"});
    let line = format!("/durable  {}", payload);
    let args = slash_arguments(&line, &NativeDurableCommand);
    let text = crate::native_durable::interactive_command(&harness, &args)
        .await
        .unwrap();
    assert!(text.contains("Mutation committed ACK"));
    assert!(text.contains("native-view-spaces"));
    assert!(text.contains("\\u001b\\u0085"));
    let before = crate::native_durable::command(&harness, "").await.unwrap();
    assert_eq!(
        before["state"]["state"]["queue"][0]["text"],
        payload["text"]
    );
    let chat = Arc::new(Container::new());
    add_note_message(&chat, &text);
    let output = strip_ansi(&chat.render(240).join("\n"));
    assert!(output.contains("Mutation committed ACK"));
    assert!(output.contains("native-view-spaces"));
    assert!(output.contains("Manual queue"));
    let help = crate::native_durable::interactive_command(&harness, "help")
        .await
        .unwrap();
    add_note_message(&chat, &help);
    let output = strip_ansi(&chat.render(240).join("\n"));
    assert!(output.contains("Open != Interrupted"));
    assert!(output.contains("INTENT ONLY"));
    assert!(output.contains("No automatic recovery or replay"));
    assert_eq!(
        crate::native_durable::command(&harness, "").await.unwrap(),
        before
    );
    harness.close().await.unwrap();
}

#[tokio::test]
async fn native_durable_runs_registry_to_plain_note_is_read_only_and_native_opt_in_only() {
    use rpi_harness::durable::{DurableActor, DurableService};

    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let core = DurableService::bind(&harness, DurableActor::owner("native-cli").unwrap());
    let before = core.snapshot().await.unwrap();
    let mut registry = build_builtin_registry();
    assert!(
        registry.find("/durable").is_none(),
        "legacy/default registry must not opt in"
    );
    registry.register(Arc::new(NativeDurableCommand));
    let command = registry.find("/durable").unwrap();
    assert!(command.preserve_args_whitespace());
    let args = slash_arguments(" /durable   runs \t ", command.as_ref());
    assert_eq!(
        args, "runs \t ",
        "retain original trailing whitespace, not normalized args"
    );
    let text = crate::native_durable::interactive_command(&harness, &args)
        .await
        .unwrap();
    let chat = Arc::new(Container::new());
    add_note_message(&chat, &text);
    let output = strip_ansi(&chat.render(240).join("\n"));
    for needle in [
        "read-only",
        "Verified runs: 0",
        "unavailable candidates: 0",
        "Choose an explicit runId",
        "Open != Interrupted",
        "not latest-run selection",
        "Manual queue, not workers",
        "No automatic recovery or replay",
        "not permission to execute",
    ] {
        assert!(
            output.contains(needle),
            "plain note omitted {needle}: {output}"
        );
    }
    assert_eq!(core.snapshot().await.unwrap(), before);
    harness.close().await.unwrap();
}

#[tokio::test]
async fn native_durable_runs_page_registry_to_plain_note_preserves_raw_cursor_and_remains_read_only(
) {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let mut registry = build_builtin_registry();
    assert!(registry.find("/durable").is_none());
    registry.register(Arc::new(NativeDurableCommand));
    let command = registry.find("/durable").unwrap();
    let before = crate::native_durable::command(&harness, "").await.unwrap();
    let args = slash_arguments(" /durable   runs-page  null \t ", command.as_ref());
    assert_eq!(args, "runs-page  null \t ");
    let text = crate::native_durable::interactive_command(&harness, &args)
        .await
        .unwrap();
    let chat = Arc::new(Container::new());
    add_note_message(&chat, &text);
    let output = strip_ansi(&chat.render(240).join("\n"));
    for needle in [
        "read-only",
        "Captured scope",
        "Scanned raw records: 0",
        "not a complete empty-history claim",
        "No next cursor",
        "No automatic recovery or replay",
    ] {
        assert!(output.contains(needle), "missing {needle}: {output}");
    }
    let scope = before["state"]["scope"].clone();
    let cursor = serde_json::json!({"scope":scope,"beforeSeq":1});
    let tail = format!(
        "runs-page  {}  ",
        serde_json::to_string_pretty(&cursor).unwrap()
    );
    let line = format!("/durable {tail}");
    assert_eq!(slash_arguments(&line, command.as_ref()), tail);
    // The copied cursor is passed as supplied, never rewritten to current scope.
    let error = crate::native_durable::interactive_command(&harness, &tail)
        .await
        .unwrap_err();
    assert!(error.contains("invalid_request"), "{error}"); // empty history: beforeSeq cannot be 1
    assert_eq!(
        crate::native_durable::command(&harness, "").await.unwrap(),
        before
    );
    harness.close().await.unwrap();
}
