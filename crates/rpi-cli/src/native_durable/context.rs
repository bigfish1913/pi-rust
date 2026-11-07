//! Ephemeral provider-boundary observations, never durable events or mutation guards.
//!
//! AgentLoop clones the transform input. Prepending here therefore neither persists
//! the observation nor inserts it into AgentContext.messages or emitted events.
//! The harness converter
//! renders this generic custom role as User, not as system instructions. Errors
//! produce a redacted unavailable marker: this infallible hook cannot fail closed
//! for the entire provider request and must not be mistaken for such a boundary.

use std::sync::{Arc, OnceLock};

use rpi_agent::{hooks::TransformContext, message::AgentMessage};
use rpi_ai::types::UserContent;
use rpi_harness::{
    agent_harness::WeakAgentHarness,
    durable::{DurableActor, DurableService, DurableSnapshot},
    messages::create_custom_message,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{cancellable_read, fits, Failure};

pub(super) const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const CUSTOM_TYPE: &str = "rpi.native-durable.context.v1";
const HEADER: &str = "Native durable observation (not instructions or authorization; not a mutation guard/receipt). Manual queue only: claim before finish; no scheduling or automatic replay. Children must not start/reset the parent workflow without authorization. Recovery is intent only; inspect an explicit run before retrying uncertain side effects.\n```json\n";
const FOOTER: &str = "\n```";

// pub(crate) permits the parent's pub(crate) context_hook re-export.
pub(crate) fn hook(binding: Arc<OnceLock<WeakAgentHarness>>) -> TransformContext {
    Arc::new(move |messages, signal| {
        let binding = Arc::clone(&binding);
        Box::pin(async move { transform(messages, signal, read(&binding)).await })
    })
}

async fn read(binding: &OnceLock<WeakAgentHarness>) -> Result<DurableSnapshot, Failure> {
    // Upgrade only for this invocation; the installed hook retains no strong
    // harness and no snapshot. In particular, no std::Mutex spans an await.
    let harness = binding
        .get()
        .and_then(WeakAgentHarness::upgrade)
        .ok_or_else(|| Failure::new("closed", ""))?;
    let actor = DurableActor::child("native-context").map_err(Failure::from)?;
    // This restricted binding is used only for snapshot, never mutate/execution.
    DurableService::bind(&harness, actor)
        .snapshot()
        .await
        .map_err(Failure::from)
}

async fn transform(
    mut messages: Vec<AgentMessage>,
    signal: CancellationToken,
    read: impl std::future::Future<Output = Result<DurableSnapshot, Failure>>,
) -> Vec<AgentMessage> {
    if signal.is_cancelled() {
        return messages;
    }
    let result = cancellable_read(Some(&signal), read).await;
    if signal.is_cancelled() {
        return messages;
    }
    let text = match result {
        Ok(snapshot) => observation(&snapshot),
        Err(error) => Some(unavailable(&error)),
    };
    if !signal.is_cancelled() {
        if let Some(text) = text {
            // Never filter history by marker: users may legitimately quote it.
            // Index zero also keeps every assistant/tool-result batch intact.
            messages.insert(
                0,
                create_custom_message(CUSTOM_TYPE, UserContent::Text(text), false, None, 0),
            );
        }
    }
    messages
}

fn bounded_text(value: &Value) -> Option<String> {
    // Budget the ENTIRE UTF-8 text, including JSON escapes and the fixed envelope,
    // before allocating the serialized JSON string. Never cut a business string.
    let budget = MAX_CONTEXT_BYTES - HEADER.len() - FOOTER.len();
    if !fits(value, budget) {
        return None;
    }
    serde_json::to_string(value)
        .ok()
        .map(|body| format!("{HEADER}{body}{FOOTER}"))
}

fn unavailable(error: &Failure) -> String {
    // Codes are allowlisted, never reflected from arbitrary diagnostics/paths.
    let code = match error.code.as_str() {
        "closed" => "closed",
        "recovering" => "recovering",
        "conflict" => "conflict",
        "permission_denied" => "permission_denied",
        "invalid_request" => "invalid_request",
        "invalid_journal" => "invalid_journal",
        "legacy_journal_present" => "legacy_journal_present",
        "limit_exceeded" => "limit_exceeded",
        "commit_uncertain" => "commit_uncertain",
        "execution_unavailable" => "execution_unavailable",
        "execution_error" => "execution_error",
        "control_error" => "control_error",
        _ => "read_failure",
    };
    // A constant-size schema and static allowlisted codes are always in budget.
    format!(
        "{HEADER}{{\"version\":1,\"detail\":\"summary\",\"availability\":\"unavailable\",\"code\":\"{code}\"}}{FOOTER}"
    )
}

fn observation(snapshot: &DurableSnapshot) -> Option<String> {
    let open_queue: Vec<_> = snapshot
        .state
        .queue
        .iter()
        .filter(|item| matches!(item.status.as_str(), "queued" | "claimed"))
        .collect();
    let recovery = snapshot.execution_control.recovery_request.as_ref();
    if snapshot.state.workflow.is_none() && open_queue.is_empty() && recovery.is_none() {
        // Terminal manual-queue history alone does not consume context.
        return None;
    }
    let mut counts = json!({"queued":0,"claimed":0,"completed":0,"failed":0,"cancelled":0});
    for item in &snapshot.state.queue {
        let count = &mut counts[item.status.as_str()];
        *count = json!(count.as_u64().unwrap_or(0) + 1);
    }
    let recovery = recovery.map(|request| {
        json!({
            "runId": request.run_id,
            "unknownInvocationCount": request.unknown_invocation_ids.len(),
            "replaysTools": false
        })
    });
    // Deliberately project, not serialize the snapshot: no bindingToken,
    // entry/leaf IDs, expected guard, forgotten IDs, runtime facts or receipts.
    let mut projection = json!({
        "version": 1,
        "availability": "available",
        "detail": "full",
        "sessionId": snapshot.scope.session_id,
        "lane": snapshot.scope.lane,
        "controlRevision": snapshot.scope.control_revision,
        "workflow": snapshot.state.workflow,
        "queueView": "queued_and_claimed_only",
        "openQueue": open_queue,
        "queueCounts": counts,
        "recoveryRequest": recovery
    });
    if let Some(text) = bounded_text(&projection) {
        return Some(text);
    }

    let workflow = snapshot.state.workflow.as_ref().map(|workflow| {
        let mut steps = json!({"pending":0,"in_progress":0,"completed":0});
        for step in &workflow.steps {
            // Serialize only the enum; no dependency on private/re-exported types.
            if let Ok(Value::String(status)) = serde_json::to_value(step.status) {
                let count = &mut steps[status];
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
        }
        json!({"name":workflow.name,"status":workflow.status.as_str(),"stepCounts":steps})
    });
    projection["detail"] = json!("summary");
    projection["workflow"] = json!(workflow);
    projection
        .as_object_mut()
        .expect("projection object")
        .remove("openQueue");
    projection["openQueueCounts"] = json!({"queued":counts["queued"],"claimed":counts["claimed"]});
    projection["omittedDetails"] = json!(["workflow.steps", "workflow.reason", "openQueue"]);
    projection["detailsNotice"] = json!("Use manual durable status for omitted details.");
    Some(
        bounded_text(&projection)
            .unwrap_or_else(|| unavailable(&Failure::new("limit_exceeded", ""))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{
        Api, AssistantMessage, Content, Message, ToolCall, ToolCallType, ToolResultMessage,
        ToolResultRole, UserMessage,
    };
    use rpi_harness::{
        agent_harness::AgentHarness,
        durable::{DurableError, DurableMutation},
        messages::{convert_to_llm, custom_data},
        session::types::{Entry, EntryQuery},
        types::AgentHarnessOptions,
    };
    use std::time::Duration;

    async fn memory() -> AgentHarness {
        AgentHarness::create(AgentHarnessOptions::default())
            .await
            .unwrap()
    }
    fn owner(h: &AgentHarness) -> DurableService {
        DurableService::bind(h, DurableActor::owner("context-test").unwrap())
    }
    fn bound(h: &AgentHarness) -> TransformContext {
        let binding = Arc::new(OnceLock::new());
        assert!(binding.set(h.downgrade()).is_ok());
        hook(binding)
    }
    fn history() -> Vec<AgentMessage> {
        vec![AgentMessage::User(UserMessage::new(HEADER, 1))]
    }
    fn start() -> Value {
        json!({"action":"start","name":"流程🙂","steps":[{"id":"s","title":"Verify"}]})
    }
    async fn apply(service: &DurableService, id: &str, action: Value) {
        service
            .mutate(DurableMutation {
                request_id: id.into(),
                expected: service.snapshot().await.unwrap().scope,
                action: serde_json::from_value(action).unwrap(),
                run_id: None,
            })
            .await
            .unwrap();
    }
    fn text(message: &AgentMessage) -> &str {
        let AgentMessage::Custom(custom) = message else {
            panic!("custom observation")
        };
        let [Content::Text(content)] = custom.content.as_slice() else {
            panic!("text observation")
        };
        &content.text
    }
    fn payload(text: &str) -> Value {
        assert!(text.len() <= MAX_CONTEXT_BYTES);
        serde_json::from_str(
            text.strip_prefix(HEADER)
                .unwrap()
                .strip_suffix(FOOTER)
                .unwrap(),
        )
        .unwrap()
    }
    async fn records(h: &AgentHarness) -> Vec<Entry> {
        h.session()
            .storage()
            .find_entries(&EntryQuery {
                entry_type: Some("custom"),
                custom_type: Some("rpi.durable.control.v1".into()),
                ..Default::default()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn default_idle_memory_is_unchanged() {
        let h = memory().await;
        assert_eq!(
            bound(&h)(history(), CancellationToken::new()).await,
            history()
        );
        assert!(records(&h).await.is_empty());
    }

    #[tokio::test]
    async fn refresh_pause_terminal_statuses_and_reset_do_not_filter_or_accumulate_history() {
        let h = memory().await;
        let service = owner(&h);
        let hook = bound(&h);
        let mut original = history();
        // Even a genuine marker-shaped history entry belongs to the caller.
        original.push(create_custom_message(
            CUSTOM_TYPE,
            UserContent::Text("user quote".into()),
            false,
            None,
            0,
        ));
        apply(&service, "start", start()).await;
        for (id, action, status) in [
            (
                "pause",
                json!({"action":"pause","reason":"waiting"}),
                "paused",
            ),
            ("resume", json!({"action":"resume"}), "running"),
            (
                "checkpoint",
                json!({"action":"checkpoint","step":"s","status":"completed","note":"verified"}),
                "running",
            ),
            ("complete", json!({"action":"complete"}), "completed"),
        ] {
            apply(&service, id, action).await;
            let out = hook(original.clone(), CancellationToken::new()).await;
            assert_eq!(&out[1..], original.as_slice());
            assert_eq!(out.len(), original.len() + 1);
            let data = payload(text(&out[0]));
            assert_eq!(data["workflow"]["status"], status);
            assert_eq!(
                data["controlRevision"],
                service.snapshot().await.unwrap().scope.control_revision
            );
            if id == "pause" {
                assert_eq!(data["workflow"]["reason"], "waiting");
            }
            if id == "checkpoint" {
                assert_eq!(data["workflow"]["steps"][0]["note"], "verified");
            }
        }
        apply(&service, "restart", start()).await;
        apply(
            &service,
            "fail",
            json!({"action":"fail","reason":"blocked"}),
        )
        .await;
        let out = hook(original.clone(), CancellationToken::new()).await;
        assert_eq!(payload(text(&out[0]))["workflow"]["status"], "failed");
        apply(&service, "reset", json!({"action":"reset"})).await;
        assert_eq!(
            hook(original.clone(), CancellationToken::new()).await,
            original
        );
    }

    #[tokio::test]
    async fn session_replacement_reads_the_new_binding_not_a_cached_workflow() {
        let h = memory().await;
        let hook = bound(&h);
        apply(&owner(&h), "old-start", start()).await;
        let old = owner(&h).snapshot().await.unwrap();
        assert!(
            payload(text(&hook(history(), CancellationToken::new()).await[0]))["workflow"]
                .is_object()
        );
        let replacement = memory().await;
        apply(
            &owner(&replacement),
            "new-start",
            json!({"action":"start","name":"replacement",
            "steps":[{"id":"new","title":"New session only"}]}),
        )
        .await;
        h.set_session(replacement.session().clone()).await.unwrap();
        let current = owner(&h).snapshot().await.unwrap();
        assert_ne!(current.scope.binding_token, old.scope.binding_token);
        let entries = records(&h).await;
        let out = hook(history(), CancellationToken::new()).await;
        let data = payload(text(&out[0]));
        assert_eq!(data["workflow"]["name"], "replacement");
        assert_eq!(data["workflow"]["steps"][0]["id"], "new");
        assert_eq!(data["controlRevision"], current.scope.control_revision);
        assert_eq!(owner(&h).snapshot().await.unwrap(), current);
        assert_eq!(records(&h).await, entries);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inherited_child_observes_parent_without_a_second_control_journal() {
        use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
        use rpi_ai::types::StopReason;
        use rpi_harness::agent_harness::AgentLane;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::factory(move |ctx| {
            let observations: Vec<_> = ctx
                .messages
                .iter()
                .filter_map(|message| {
                    let Message::User(user) = message else {
                        return None;
                    };
                    let UserContent::Blocks(blocks) = &user.content else {
                        return None;
                    };
                    let [Content::Text(content)] = blocks.as_slice() else {
                        return None;
                    };
                    content
                        .text
                        .starts_with(HEADER)
                        .then_some(content.text.as_str())
                })
                .collect();
            assert_eq!(observations.len(), 1);
            assert_eq!(payload(observations[0])["workflow"]["name"], "流程🙂");
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            faux_assistant_message("done", StopReason::Stop)
        })]);
        let provider = FauxProvider::new(script);
        let binding = Arc::new(OnceLock::new());
        let parent = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            transform_context: Some(hook(binding.clone())),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(binding.set(parent.downgrade()).is_ok());
        apply(&owner(&parent), "start", start()).await;
        let before = owner(&parent).snapshot().await.unwrap();
        let entries = records(&parent).await;
        use rpi_harness::session::{
            memory::{InMemorySessionStorage, SystemClock},
            session::{DefaultIdGenerator, Session},
            types::SessionMetadata,
        };
        let child_session = Session::new(
            Arc::new(InMemorySessionStorage::new(
                SessionMetadata {
                    id: "synthetic-native-context-child".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )),
            None,
        );
        let (emitter, _) = rpi_agent::CollectorEmitter::new();
        let child = parent
            .create_child(child_session, Arc::new(emitter), 2, None)
            .await
            .unwrap();
        child
            .harness
            .prompt_text("Observe only", vec![])
            .await
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(owner(&parent).snapshot().await.unwrap(), before);
        assert_eq!(records(&parent).await, entries);
        assert!(records(&child.harness).await.is_empty());
        assert!(owner(&child.harness)
            .snapshot()
            .await
            .unwrap()
            .state
            .workflow
            .is_none());
    }

    #[tokio::test]
    async fn claimed_queue_read_preserves_revisions_records_receipts_and_actor() {
        let h = memory().await;
        let service = owner(&h);
        apply(
            &service,
            "enqueue",
            json!({"action":"enqueue","text":"manual work"}),
        )
        .await;
        let item = service.snapshot().await.unwrap().state.queue[0].id.clone();
        apply(&service, "claim", json!({"action":"claim","item":item})).await;
        let before = service.snapshot().await.unwrap();
        let entries = records(&h).await;
        let receipt = service.request_outcome("claim").await.unwrap();
        for _ in 0..3 {
            let out = bound(&h)(history(), CancellationToken::new()).await;
            let data = payload(text(&out[0]));
            assert_eq!(data["queueView"], "queued_and_claimed_only");
            assert_eq!(data["openQueue"][0]["text"], "manual work");
            assert_eq!(data["openQueue"][0]["claimedBy"], "context-test");
            assert_eq!(data["queueCounts"]["claimed"], 1);
        }
        assert_eq!(service.snapshot().await.unwrap(), before);
        assert_eq!(records(&h).await, entries);
        assert_eq!(service.request_outcome("claim").await.unwrap(), receipt);
        // A context reader does not replace/start the parent's workflow.
        let reader = DurableService::bind(&h, DurableActor::child("native-context").unwrap());
        assert!(matches!(
            reader
                .mutate(DurableMutation {
                    request_id: "forbidden".into(),
                    expected: before.scope,
                    action: serde_json::from_value(start()).unwrap(),
                    run_id: None,
                })
                .await,
            Err(DurableError::PermissionDenied)
        ));
        apply(
            &service,
            "finish",
            json!({"action":"finish","item":item,"status":"completed"}),
        )
        .await;
        assert_eq!(
            bound(&h)(history(), CancellationToken::new()).await,
            history()
        );
        apply(&service, "show-counts", start()).await;
        let out = bound(&h)(history(), CancellationToken::new()).await;
        let data = payload(text(&out[0]));
        assert_eq!(data["openQueue"], json!([]));
        assert_eq!(data["queueCounts"]["completed"], 1);
    }

    #[tokio::test]
    async fn cancellation_before_and_during_read_returns_original_quickly() {
        let signal = CancellationToken::new();
        signal.cancel();
        let out = transform(history(), signal, async { panic!("cancelled read polled") }).await;
        assert_eq!(out, history());
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let future = transform(history(), signal, async move {
            cancel.cancel();
            std::future::pending::<Result<DurableSnapshot, Failure>>().await
        });
        let out = tokio::time::timeout(Duration::from_millis(250), future)
            .await
            .unwrap();
        assert_eq!(out, history());
        // Cancellation after a successful read must not inject its stale result.
        let h = memory().await;
        apply(&owner(&h), "start", start()).await;
        let snapshot = owner(&h).snapshot().await.unwrap();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        assert_eq!(
            transform(history(), signal, async move {
                cancel.cancel();
                Ok(snapshot)
            })
            .await,
            history()
        );
    }

    #[tokio::test]
    async fn missing_dropped_closed_and_failed_reads_are_redacted_not_empty_success() {
        let missing = hook(Arc::new(OnceLock::new()));
        let h = memory().await;
        let dropped = bound(&h);
        drop(h);
        let h = memory().await;
        let closed = bound(&h);
        h.close().await.unwrap();
        for hook in [missing, dropped, closed] {
            let out = hook(history(), CancellationToken::new()).await;
            assert_eq!(&out[1..], history().as_slice());
            assert_eq!(
                payload(text(&out[0])),
                json!({"version":1,"detail":"summary","availability":"unavailable","code":"closed"})
            );
        }
        for error in [
            Failure::from(DurableError::ReadFailure(
                "C:/secret/session request-secret".into(),
            )),
            Failure::from(DurableError::InvalidJournal("private payload".into())),
            Failure::new("/private/unknown-code", "secret"),
        ] {
            let out = transform(history(), CancellationToken::new(), async { Err(error) }).await;
            let text = text(&out[0]);
            let data = payload(text);
            assert_eq!(data["availability"], "unavailable");
            assert!(data.get("workflow").is_none());
            assert!(!text.contains("private") && !text.contains("secret"));
        }
    }

    #[tokio::test]
    async fn oversized_unicode_escaped_notes_use_complete_bounded_summary() {
        let h = memory().await;
        apply(&owner(&h), "start", start()).await;
        let mut fixture = serde_json::to_value(owner(&h).snapshot().await.unwrap()).unwrap();
        let name = "完整🙂".repeat(100);
        fixture["state"]["workflow"]["name"] = json!(name);
        fixture["state"]["workflow"]["status"] = json!("paused");
        fixture["state"]["workflow"]["reason"] = json!("waiting");
        fixture["state"]["workflow"]["steps"][0]["note"] = json!("\u{1}\u{2}\u{3}🙂".repeat(1000));
        let snapshot: DurableSnapshot = serde_json::from_value(fixture).unwrap();
        let rendered = observation(&snapshot).unwrap();
        let data = payload(&rendered);
        assert_eq!(data["detail"], "summary");
        assert_eq!(data["workflow"]["name"], name);
        assert_eq!(data["workflow"]["status"], "paused");
        assert_eq!(data["workflow"]["stepCounts"]["pending"], 1);
        assert!(data["detailsNotice"]
            .as_str()
            .unwrap()
            .contains("manual durable status"));
        assert!(data.get("openQueue").is_none());
        assert!(!data["omittedDetails"].as_array().unwrap().is_empty());
        assert_eq!(rendered.matches("Native durable observation").count(), 1);
        let mut oversized = snapshot;
        oversized.scope.session_id = "🙂".repeat(MAX_CONTEXT_BYTES);
        assert_eq!(
            payload(&observation(&oversized).unwrap())["code"],
            "limit_exceeded"
        );
        let exact =
            json!({"text":"x".repeat(MAX_CONTEXT_BYTES - HEADER.len() - FOOTER.len() - 11)});
        assert_eq!(bounded_text(&exact).unwrap().len(), MAX_CONTEXT_BYTES);
        assert!(bounded_text(&json!({"text":"x".repeat(MAX_CONTEXT_BYTES)})).is_none());
    }

    #[tokio::test]
    async fn recovery_projection_is_explicit_intent_and_never_forgotten_or_native_facts() {
        let h = memory().await;
        let mut fixture = serde_json::to_value(owner(&h).snapshot().await.unwrap()).unwrap();
        fixture["executionControl"] = json!({
            "forgottenRunIds":["forgotten-secret"],
            "recoveryRequest":{
                "runId":"explicit-run", "startedSeq":9, "observedHistoryRevision":12,
                "unknownInvocationIds":["invocation-secret-a","invocation-secret-b"]
            }
        });
        // Pure public snapshot fixture, not a forged reserved ControlRecord.
        let mut snapshot: DurableSnapshot = serde_json::from_value(fixture).unwrap();
        let text = observation(&snapshot).unwrap();
        let data = payload(&text);
        assert_eq!(
            data["recoveryRequest"],
            json!({"runId":"explicit-run","unknownInvocationCount":2,"replaysTools":false})
        );
        for forbidden in [
            "forgotten-secret",
            "invocation-secret",
            "bindingToken",
            "entryId",
            "historyRevision",
            "startedSeq",
            "expected",
            "receipts",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}");
        }
        snapshot.execution_control.recovery_request = None;
        assert!(
            observation(&snapshot).is_none(),
            "forgotten IDs do not imply a latest run"
        );
    }

    #[tokio::test]
    async fn prepend_preserves_tool_result_batch_and_converts_to_user() {
        let h = memory().await;
        apply(&owner(&h), "start", start()).await;
        let mut assistant = AssistantMessage::empty(Api::OpenaiCompletions, "test", "model", 1);
        assistant.content = vec![Content::ToolCall(ToolCall {
            kind: ToolCallType,
            id: "call".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        })];
        let original = vec![
            AgentMessage::Assistant(Box::new(assistant)),
            AgentMessage::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole,
                tool_call_id: "call".into(),
                tool_name: "read".into(),
                content: vec![Content::text("ok")],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 2,
            })),
        ];
        let out = bound(&h)(original.clone(), CancellationToken::new()).await;
        assert_eq!(&out[1..], original.as_slice());
        let metadata = custom_data(&out[0]).unwrap();
        assert_eq!(metadata.custom_type, CUSTOM_TYPE);
        assert!(!metadata.display);
        assert!(metadata.details.is_none());
        let AgentMessage::Custom(custom) = &out[0] else {
            panic!("custom")
        };
        assert_eq!(custom.timestamp, 0);
        let converted = convert_to_llm(out);
        assert!(matches!(
            (&converted[0], &converted[1], &converted[2]),
            (
                Message::User(_),
                Message::Assistant(_),
                Message::ToolResult(_)
            )
        ));
    }
}
