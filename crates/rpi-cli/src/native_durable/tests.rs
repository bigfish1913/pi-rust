use super::*;
use rpi_agent::tool_execution_scope::ToolExecutionScope;
use rpi_harness::types::AgentHarnessOptions;

async fn memory() -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions::default())
        .await
        .unwrap()
}
fn start() -> Value {
    json!({"action":"start", "name":"wire", "steps":[{"id":"step", "title":"Verify"}]})
}
fn bound_tool(h: &AgentHarness) -> NativeDurableTool {
    let binding = Arc::new(OnceLock::new());
    assert!(binding.set(h.downgrade()).is_ok());
    NativeDurableTool::new(binding)
}
async fn execute(tool: &NativeDurableTool, params: Value) -> Result<AgentToolResult, AgentError> {
    tool.execute(
        "provider-id-is-not-a-request-id",
        params,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
}
async fn state(h: &AgentHarness) -> Value {
    rpc(h, &json!({"type":"get_durable_state", "version":1}), true)
        .await
        .unwrap()["state"]
        .clone()
}
async fn mutation(h: &AgentHarness, id: &str, action: Value) -> Value {
    json!({"type":"durable_mutate", "version":1, "requestId":id,
        "expected":state(h).await["scope"], "action":action})
}

#[test]
fn strict_parser_rejects_duplicates_at_any_depth_and_trailing_data() {
    for input in [
        r#"{"version":1,"version":1}"#,
        r#"{"action":{"action":"reset","action":"start"}}"#,
        r#"{"steps":[{"id":"a","\u0069d":"b"}]}"#,
        r#"{"__rpi":{"actor":"a","actor":"b"}}"#,
        "{} {}",
        "{",
        "[NaN]",
    ] {
        assert_eq!(
            parse_json(input).unwrap_err().code,
            "invalid_request",
            "{input}"
        );
    }
    let raw = r#"{"a":[null,true,false,-1,1,1.25,"x"],"b":{}}"#;
    assert_eq!(
        parse_json(raw).unwrap(),
        serde_json::from_str::<Value>(raw).unwrap()
    );
}
#[test]
fn raw_size_budget_includes_whitespace_and_has_an_exact_boundary() {
    let exact = format!("{}null", " ".repeat(MAX_REQUEST_BYTES - 4));
    assert!(parse_json(&exact).is_ok());
    assert_eq!(
        parse_json(&(exact + " ")).unwrap_err().code,
        "limit_exceeded"
    );
    assert_eq!(
        request_budget(&json!("x".repeat(MAX_REQUEST_BYTES)))
            .unwrap_err()
            .code,
        "limit_exceeded"
    );
}
#[test]
fn capability_is_pure_and_explicit_about_prototype_limits() {
    let disabled = capabilities(false);
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["version"], 1);
    assert_eq!(disabled["scheduler"], false);
    assert_eq!(disabled["scope"], "current_lane");
    assert_eq!(disabled["execution"], "explicit_run");
    assert_eq!(disabled["limits"]["receipts"], 64);
    assert_eq!(disabled["limits"]["recordBytes"], 1024 * 1024);
    let mut enabled = capabilities(true);
    enabled["enabled"] = json!(false);
    assert_eq!(enabled, disabled);
}
#[tokio::test]
async fn rpc_rejects_versions_unknown_fields_and_oversize_ids() {
    let h = memory().await;
    for version in [
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(2),
        json!(1.0),
        json!("1"),
    ] {
        assert_eq!(
            rpc(
                &h,
                &json!({"type":"get_durable_state","version":version}),
                true
            )
            .await
            .unwrap_err()
            .code,
            "invalid_request"
        );
    }
    for request in [
        json!({"type":"get_durable_state"}),
        json!({"type":"get_durable_state","version":1,"actor":"owner"}),
        json!({"type":"get_durable_state","version":1,"__rpi":{}}),
        json!({"type":"get_durable_state","version":1,"lane":"side"}),
        json!({"type":"get_durable_state","version":1,"runId":null}),
        json!({"type":"get_durable_request","version":1}),
        json!({"type":"get_durable_request","version":1,"requestId":"bad id"}),
    ] {
        assert_eq!(
            rpc(&h, &request, true).await.unwrap_err().code,
            "invalid_request"
        );
    }
    let request = json!({"type":"get_durable_state","version":1,"id":"x".repeat(MAX_ID_BYTES)});
    let error = rpc(&h, &request, true).await.unwrap_err();
    assert_eq!(error.code, "limit_exceeded");
    let response = rpc_response(request["id"].clone(), "get_durable_state", Err(error));
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["success"], false);
    assert_eq!(
        rpc(&h, &json!({"type":"get_durable_state","version":1}), false)
            .await
            .unwrap_err()
            .code,
        "disabled"
    );
}
#[tokio::test]
async fn rpc_requires_complete_guard_and_explicit_request_identity() {
    let h = memory().await;
    let mut request = mutation(&h, "start", start()).await;
    for field in ["requestId", "expected"] {
        let mut incomplete = request.clone();
        incomplete.as_object_mut().unwrap().remove(field);
        assert_eq!(
            rpc(&h, &incomplete, true).await.unwrap_err().code,
            "invalid_request"
        );
    }
    for field in SCOPE_FIELDS {
        let mut incomplete = request.clone();
        incomplete["expected"]
            .as_object_mut()
            .unwrap()
            .remove(*field);
        assert_eq!(
            rpc(&h, &incomplete, true).await.unwrap_err().code,
            "invalid_request"
        );
    }
    for extra in ["actor", "nativeDurableActor", "claimedBy", "unknown"] {
        let mut invalid = request.clone();
        invalid["action"][extra] = json!("spoofed");
        assert_eq!(
            rpc(&h, &invalid, true).await.unwrap_err().code,
            "invalid_request"
        );
    }
    request["action"]["steps"][0]["status"] = json!("completed");
    let error = rpc(&h, &request, true).await.unwrap_err();
    let response = rpc_response(json!(42), "durable_mutate", Err(error));
    assert_eq!(response["success"], false);
    assert_eq!(response["status"], "error");
    assert_eq!(response["id"], 42);
    assert_eq!(response["errorDetails"]["code"], "invalid_request");
    assert_eq!(response["errorDetails"]["requestId"], "start");
    assert!(response.get("data").is_none());
    assert!(response.get("result").is_none());
    assert_eq!(state(&h).await["scope"]["controlRevision"], 0);
}
#[tokio::test]
async fn rpc_lifecycle_receipts_and_transport_id_independent_dedup() {
    let h = memory().await;
    let mut request = mutation(&h, "start", start()).await;
    request["id"] = json!({"transport":"same-id"});
    let first = rpc(&h, &request, true).await.unwrap();
    assert_eq!(first["version"], 1);
    assert_eq!(first["outcome"]["replayed"], false);
    let replay = rpc(&h, &request, true).await.unwrap();
    assert_eq!(replay["outcome"]["replayed"], true);
    assert_eq!(replay["outcome"]["receipt"], first["outcome"]["receipt"]);
    request["id"] = json!("a-different-transport-id");
    assert_eq!(
        rpc(&h, &request, true).await.unwrap()["outcome"]["replayed"],
        true
    );
    let response = rpc_response(json!(1), "durable_mutate", Ok(first.clone()));
    assert_eq!(response["success"], true);
    assert_eq!(response["data"], first);
    assert_eq!(response["data"], response["result"]);
    for (id, action) in [
        (
            "checkpoint",
            json!({"action":"checkpoint","step":"step","status":"completed"}),
        ),
        ("complete", json!({"action":"complete"})),
        ("enqueue", json!({"action":"enqueue","text":"verify queue"})),
    ] {
        let request = mutation(&h, id, action).await;
        rpc(&h, &request, true).await.unwrap();
    }
    let item = state(&h).await["state"]["queue"][0]["id"].clone();
    for (id, action) in [
        ("claim", json!({"action":"claim","item":item})),
        (
            "finish",
            json!({"action":"finish","item":item,"status":"completed"}),
        ),
        ("reset", json!({"action":"reset"})),
    ] {
        let request = mutation(&h, id, action).await;
        rpc(&h, &request, true).await.unwrap();
    }
    let current = state(&h).await;
    assert!(current["state"]["workflow"].is_null());
    assert_eq!(current["state"]["queue"][0]["status"], "completed");
    let receipt = rpc(
        &h,
        &json!({"type":"get_durable_request","version":1,"requestId":"reset"}),
        true,
    )
    .await
    .unwrap();
    assert_eq!(receipt["receipt"]["actor"], "native-cli");
    assert_eq!(receipt["receipt"]["requestId"], "reset");
    assert_eq!(
        rpc(
            &h,
            &json!({"type":"get_durable_request","version":1,"requestId":"absent"}),
            true
        )
        .await
        .unwrap()["receipt"],
        Value::Null
    );
    assert_eq!(rpc(&h, &request, true).await.unwrap_err().code, "conflict");
}
#[tokio::test]
async fn tool_cancellation_missing_id_flat_validation_and_slash_share_core() {
    let h = memory().await;
    let tool = bound_tool(&h);
    assert_eq!(tool.schema().name, "durable");
    assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = tool
        .execute(
            "provider-id",
            json!({"action":"reset","requestId":"cancelled"}),
            cancelled,
            Arc::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    for invalid in [
        json!({"action":"reset"}),
        json!({"actor":"owner"}),
        json!({"action":"queue","runId":"run"}),
        json!({"action":"status","runId":"run","requestId":"ambiguous"}),
        json!({"action":"reset","requestId":"reset","name":"ignored?"}),
        json!({"action":"start","requestId":"start","name":"missing steps"}),
    ] {
        assert!(execute(&tool, invalid)
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid_request"));
    }
    let status = execute(&tool, json!({"__rpi":{"actor":"not-an-actor"}}))
        .await
        .unwrap();
    assert_eq!(status.details["state"]["scope"]["controlRevision"], 0);
    assert_eq!(command(&h, "  ").await.unwrap(), status.details);
    let applied = command(&h, r#"{"action":"reset","requestId":"slash"}"#)
        .await
        .unwrap();
    assert_eq!(applied["outcome"]["receipt"]["actor"], "native-cli");
    assert!(command(
        &h,
        r#"{"action":"reset","action":"reset","requestId":"duplicate"}"#
    )
    .await
    .is_err());
    assert!(command(&h, "status").await.is_err());
    assert_eq!(
        command(&h, &" ".repeat(MAX_REQUEST_BYTES + 1))
            .await
            .unwrap_err()
            .code,
        "limit_exceeded"
    );
    // Explicit-run reads must not silently return the lane-global snapshot.
    assert_eq!(
        rpc(
            &h,
            &json!({"type":"get_durable_state", "version":1,"runId":"missing-run"}),
            true
        )
        .await
        .unwrap_err()
        .code,
        "execution_unavailable"
    );
    assert!(
        execute(&tool, json!({"action":"status","runId":"missing-run"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("execution_unavailable")
    );
    assert_eq!(
        execute(&tool, json!({"action":"queue"}))
            .await
            .unwrap()
            .details,
        command(&h, "").await.unwrap()
    );
    let mut flat = start();
    flat["requestId"] = json!("flat");
    flat["expected"] = state(&h).await["scope"].clone();
    let result = execute(&tool, flat.clone()).await.unwrap();
    assert_eq!(result.details["outcome"]["replayed"], false);
    assert_eq!(
        execute(&tool, flat).await.unwrap().details["outcome"]["replayed"],
        true
    );
}
#[tokio::test]
async fn shared_codec_preserves_envelope_and_disabled_precedence_before_actor_binding() {
    let h = memory().await;
    let before = state(&h).await;
    // Missing trusted child identity must not run before envelope/opt-in checks.
    Arc::new(ToolExecutionScope::default())
        .run(async {
            let state = json!({"type":"get_durable_state","version":1});
            assert_eq!(rpc(&h, &state, false).await.unwrap_err().code, "disabled");
            assert_eq!(
                rpc(&h, &json!({"type":"get_durable_state","version":2}), true)
                    .await
                    .unwrap_err()
                    .code,
                "invalid_request"
            );
            assert_eq!(
                rpc(&h, &state, true).await.unwrap_err().code,
                "permission_denied"
            );
        })
        .await;
    assert_eq!(state(&h).await, before);
}

#[tokio::test]
async fn child_identity_fails_closed_and_claims_and_receipts_are_actor_scoped() {
    let h = memory().await;
    let tool = bound_tool(&h);
    Arc::new(ToolExecutionScope::default())
        .run(async {
            assert!(execute(&tool, json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("permission_denied"));
        })
        .await;
    for context in [
        json!({}),
        json!({"nativeDurableActor":null}),
        json!({"nativeDurableActor":"bad id"}),
    ] {
        Arc::new(ToolExecutionScope::with_context(context))
            .run(async {
                let error = execute(&tool, json!({"__rpi":{"nativeDurableActor":"forged"}}))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("permission_denied"));
            })
            .await;
    }
    let queued = command(
        &h,
        r#"{"action":"enqueue","requestId":"enqueue","text":"owned claim"}"#,
    )
    .await
    .unwrap();
    let item = queued["outcome"]["snapshot"]["state"]["queue"][0]["id"].clone();
    let child = Arc::new(ToolExecutionScope::with_context(
        json!({"nativeDurableActor":"child-unique-1"}),
    ));
    child
        .run(async {
            for action in ["start", "reset", "recover", "forget_run"] {
                let mut request = mutation(
                    &h,
                    action,
                    if action == "start" {
                        start()
                    } else {
                        json!({"action":action})
                    },
                )
                .await;
                if matches!(action, "recover" | "forget_run") {
                    request["runId"] = json!("explicit-run");
                }
                assert_eq!(
                    rpc(&h, &request, true).await.unwrap_err().code,
                    "permission_denied"
                );
            }
            let claimed = execute(
                &tool,
                json!({"action":"claim","requestId":"child-claim","item":item}),
            )
            .await
            .unwrap();
            assert_eq!(
                claimed.details["outcome"]["receipt"]["actor"],
                "child-unique-1"
            );
            let own = rpc(
                &h,
                &json!({"type":"get_durable_request","version":1,"requestId":"child-claim"}),
                true,
            )
            .await
            .unwrap();
            assert!(!own["receipt"].is_null());
            let parent = rpc(
                &h,
                &json!({"type":"get_durable_request","version":1,"requestId":"enqueue"}),
                true,
            )
            .await
            .unwrap();
            assert!(parent["receipt"].is_null());
        })
        .await;
    let parent = rpc(
        &h,
        &json!({"type":"get_durable_request","version":1,"requestId":"child-claim"}),
        true,
    )
    .await
    .unwrap();
    assert!(parent["receipt"].is_null());
    let other = Arc::new(ToolExecutionScope::with_context(
        json!({"nativeDurableActor":"child-unique-2"}),
    ));
    other.run(async {
        assert!(execute(&tool, json!({"action":"finish","requestId":"wrong-owner","item":item,"status":"completed"})).await.unwrap_err().to_string().contains("permission_denied"));
    }).await;
    child.run(async {
        execute(&tool, json!({"action":"finish","requestId":"right-owner","item":item,"status":"completed"})).await.unwrap();
    }).await;
}
#[tokio::test]
async fn weak_binding_does_not_keep_owner_alive_and_observes_session_rebind() {
    let unbound = NativeDurableTool::new(Arc::new(OnceLock::new()));
    assert!(execute(&unbound, json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("closed"));
    let h = memory().await;
    let tool = bound_tool(&h);
    let old = execute(&tool, json!({})).await.unwrap().details["state"]["scope"].clone();
    let replacement = memory().await;
    h.session().set_storage(replacement.session().storage());
    let current = execute(&tool, json!({})).await.unwrap().details["state"]["scope"].clone();
    assert_ne!(old["bindingToken"], current["bindingToken"]);
    // Memory defaults intentionally reuse the logical id; replacement fencing
    // must still detect a different backing store.
    assert_eq!(old["sessionId"], current["sessionId"]);
    let stale = json!({"action":"reset","requestId":"stale","expected":old});
    assert!(execute(&tool, stale)
        .await
        .unwrap_err()
        .to_string()
        .contains("conflict"));
    drop(h);
    assert!(execute(&tool, json!({}))
        .await
        .unwrap_err()
        .to_string()
        .contains("closed"));
}
#[test]
fn response_limit_never_truncates_and_distinguishes_committed_mutations() {
    let oversized = json!({"body":"x".repeat(MAX_RESPONSE_BYTES)});
    assert_eq!(
        response_budget(oversized.clone(), None).unwrap_err().code,
        "limit_exceeded"
    );
    let error = response_budget(oversized, Some("committed-id")).unwrap_err();
    assert_eq!(error.code, "commit_uncertain");
    assert_eq!(error.request_id.as_deref(), Some("committed-id"));
    let response = rpc_response(Value::Null, "durable_mutate", Err(error));
    assert_eq!(response["success"], false);
    assert_eq!(response["errorDetails"]["requestId"], "committed-id");
}
#[tokio::test]
async fn late_response_limit_preserves_real_commit_and_receipt_lookup() {
    let h = memory().await;
    let steps: Vec<_> = (0..21)
        .map(|i| json!({"id":format!("s{i}"),"title":"Bound check"}))
        .collect();
    let request = mutation(
        &h,
        "start",
        json!({"action":"start","name":"late bound","steps":steps}),
    )
    .await;
    rpc(&h, &request, true).await.unwrap();
    let note = "\u{1}".repeat(4000); // Six encoded bytes per scalar; each request is <64 KiB.
    for i in 0..21 {
        let id = format!("note-{i}");
        let request = mutation(
            &h,
            &id,
            json!({"action":"checkpoint","step":format!("s{i}"),"note":note}),
        )
        .await;
        let result = rpc(&h, &request, true).await;
        // The outcome includes both the snapshot and this receipt's note.
        if i < 19 {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.code, "commit_uncertain");
            assert_eq!(error.request_id.as_deref(), Some(id.as_str()));
            let receipt = rpc(
                &h,
                &json!({"type":"get_durable_request","version":1,"requestId":id}),
                true,
            )
            .await
            .unwrap();
            assert_eq!(receipt["receipt"]["requestId"], id);
            let flat = json!({"action":"status","requestId":id});
            assert_eq!(
                execute(&bound_tool(&h), flat.clone())
                    .await
                    .unwrap()
                    .details,
                receipt
            );
            assert_eq!(command(&h, &flat.to_string()).await.unwrap(), receipt);
        }
    }
    assert_eq!(
        rpc(&h, &json!({"type":"get_durable_state","version":1}), true)
            .await
            .unwrap_err()
            .code,
        "limit_exceeded"
    );
}

#[tokio::test]
async fn cancellation_drops_pending_observation_without_detaching_a_worker() {
    let signal = CancellationToken::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Guard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let task = tokio::spawn({
        let signal = signal.clone();
        let entered = entered.clone();
        let dropped = dropped.clone();
        async move {
            cancellable_read(Some(&signal), async {
                let _guard = Guard(dropped);
                entered.notify_one();
                std::future::pending::<Result<(), Failure>>().await
            })
            .await
        }
    });
    entered.notified().await;
    signal.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().code, "cancelled");
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_child_wrapper_uses_host_actor_and_cannot_reset_parent_workflow() {
    use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
    use rpi_harness::agent_harness::AgentLane;
    use rpi_harness::session::{
        memory::{InMemorySessionStorage, SystemClock},
        session::{DefaultIdGenerator, Session},
        types::SessionMetadata,
    };
    use rpi_harness::types::HarnessTool;
    let script = FauxScript::new();
    let provider = FauxProvider::new(script.clone());
    let binding = Arc::new(OnceLock::new());
    let tool = Arc::new(NativeDurableTool::new(binding.clone()));
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![HarnessTool::new(tool)],
        active_tool_names: vec!["durable".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(binding.set(parent.downgrade()).is_ok());
    command(&parent, r#"{"action":"start","requestId":"parent-start","name":"parent","steps":[{"id":"s","title":"Preserve"}]}"#).await.unwrap();
    let queued = command(
        &parent,
        r#"{"action":"enqueue","requestId":"parent-queue","text":"Shared work"}"#,
    )
    .await
    .unwrap();
    let item = queued["outcome"]["snapshot"]["state"]["queue"][0]["id"].clone();
    script.set_responses(vec![
        FauxStep::tool_call(
            "durable",
            json!({"action":"reset","requestId":"forbidden-reset"}),
        ),
        FauxStep::tool_call(
            "durable",
            json!({"action":"claim","requestId":"child-claim-real","item":item}),
        ),
        FauxStep::text("done"),
    ]);
    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "synthetic-native-child".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    let (collector, _) = rpi_agent::CollectorEmitter::new();
    let child = parent
        .create_child(session, Arc::new(collector), 4, None)
        .await
        .unwrap();
    child
        .harness
        .prompt_text("claim the shared work", vec![])
        .await
        .unwrap();
    let state = state(&parent).await;
    assert_eq!(state["state"]["workflow"]["name"], "parent");
    let actor = state["state"]["queue"][0]["claimedBy"].as_str().unwrap();
    assert!(actor.starts_with("child-"), "{state}");
    let root_receipt = rpc(
        &parent,
        &json!({"type":"get_durable_request","version":1,"requestId":"child-claim-real"}),
        true,
    )
    .await
    .unwrap();
    assert!(
        root_receipt["receipt"].is_null(),
        "owner must not impersonate child receipts"
    );
    assert_eq!(state["scope"]["controlRevision"], 3);
    let child_state = rpc(
        &child.harness,
        &json!({"type":"get_durable_state","version":1}),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        child_state["state"]["scope"]["controlRevision"], 0,
        "shared tool must not introduce a second writer in the child session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_explicit_run_forget_keeps_native_result_and_never_calls_provider_again() {
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};
    use rpi_harness::agent_harness::AgentLane;
    let provider = FauxProvider::new(FauxScript::new().with_text("synthetic result"));
    let h = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        ..Default::default()
    })
    .await
    .unwrap();
    let finished = h.prompt_text("synthetic prompt", vec![]).await.unwrap();
    let audit = h.get_result(&finished.run_id).await.unwrap().unwrap();
    let query = json!({"type":"get_durable_state","version":1,"runId":finished.run_id});
    let run = rpc(&h, &query, true).await.unwrap();
    assert_eq!(run["state"]["run"]["status"], "completed");
    let mut recover = mutation(&h, "reject-terminal-recover", json!({"action":"recover"})).await;
    recover["runId"] = json!(finished.run_id);
    assert_eq!(
        rpc(&h, &recover, true).await.unwrap_err().code,
        "execution_error"
    );
    let mut forget = mutation(&h, "forget-completed", json!({"action":"forget_run"})).await;
    forget["runId"] = json!(finished.run_id);
    assert_eq!(
        rpc(&h, &forget, true).await.unwrap()["outcome"]["receipt"]["runId"],
        finished.run_id
    );
    assert!(rpc(&h, &query, true).await.unwrap()["state"]["run"].is_null());
    let after = h.get_result(&finished.run_id).await.unwrap().unwrap();
    assert_eq!(after.run_id, audit.run_id);
    match (after.outcome, audit.outcome) {
        (
            rpi_harness::agent_harness::HarnessRunOutcome::Completed {
                leaf_id: a,
                final_entry_id: b,
                ..
            },
            rpi_harness::agent_harness::HarnessRunOutcome::Completed {
                leaf_id: c,
                final_entry_id: d,
                ..
            },
        ) => {
            assert_eq!((a, b), (c, d));
        }
        other => panic!("native result changed: {other:?}"),
    }
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn native_tool_budget_counts_text_details_duplication_and_json_escaping() {
    let mut value = json!({"version":1,"state":{"note":"\u{1}".repeat(MAX_RESPONSE_BYTES / 8)}});
    assert!(fits(&value, MAX_RESPONSE_BYTES));
    assert_eq!(
        tool_result(value.clone()).unwrap_err().code,
        "limit_exceeded"
    );
    value["outcome"] = json!({"receipt":{"requestId":"committed-tool"}});
    let error = tool_result(value).unwrap_err();
    assert_eq!(error.code, "commit_uncertain");
    assert_eq!(error.request_id.as_deref(), Some("committed-tool"));
}
