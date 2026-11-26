use super::*;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript};
use rpi_ai::types::StopReason;
use rpi_harness::types::AgentHarnessOptions;

async fn parent(script: FauxScript) -> AgentHarness {
    let provider = FauxProvider::new(script);
    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "parent".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        session,
        ..Default::default()
    })
    .await
    .unwrap()
}
fn job(owner: u64) -> Arc<Job> {
    Arc::new(Job {
        owner,
        cancel: CancellationToken::new(),
        invocation_signal: None,
        state: Mutex::new(State::default()),
    })
}
fn delta(text: String) -> AgentEvent {
    let partial = Arc::new(faux_assistant_message("", StopReason::Stop));
    AgentEvent::MessageUpdate {
        message_id: None,
        message: partial.clone(),
        assistant_message_event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text,
            partial,
        },
    }
}
async fn wait(manager: &ChildAgents, parent: &AgentHarness, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let value = manager
                .handle(parent, 7, json!({"op":"poll","childId":id}))
                .await
                .unwrap();
            if value["status"] != "running" {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("child must finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_reports_and_owner_scoped_disposal_are_independent_of_parent_transcript() {
    let parent = parent(FauxScript::new().with_text("child report")).await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = started["childId"].as_str().unwrap();
    assert_ne!(id, parent.session().storage().metadata().id);
    let final_value = wait(&manager, &parent, id).await;
    assert_eq!(final_value["result"]["text"], "child report");
    assert_eq!(final_value["status"], "done");
    assert!(parent.get_tip_id().await.unwrap().is_none());
    for op in ["poll", "cancel", "dispose"] {
        assert!(manager
            .handle(&parent, 8, json!({"op":op,"childId":id}))
            .await
            .is_err());
    }
    manager
        .handle(&parent, 7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(manager.jobs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_and_unload_stop_pending_children_and_reject_stale_starts() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow report")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = started["childId"].as_str().unwrap();
    manager
        .handle(&parent, 7, json!({"op":"cancel","childId":id}))
        .await
        .unwrap();
    assert!(wait(&manager, &parent, id).await["error"]
        .as_str()
        .unwrap()
        .contains("cancelled"));
    manager.cancel_owner(7);
    assert!(manager.jobs.lock().unwrap().is_empty());
    assert!(manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap_err()
        .contains("unloaded"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_timeout_and_parent_scope_cancel_are_observable() {
    for cancel_parent in [false, true] {
        let parent = parent(
            FauxScript::new()
                .with_text("slow report")
                .with_tokens_per_second(0.1),
        )
        .await;
        let job = job(7);
        let child = parent
            .create_child(
                AgentHarnessOptions::default().session,
                Arc::new(Emitter(job.clone())),
                1,
                None,
            )
            .await
            .unwrap();
        if cancel_parent {
            child.scope.cancel();
        }
        run_child(child, job.clone(), "report".into(), 1, 0).await;
        let state = job.state.lock().unwrap();
        let error = state.error.as_deref().unwrap();
        assert!(
            error.contains(if cancel_parent {
                "parent run ended"
            } else {
                "timed out"
            }),
            "{error}"
        );
    }
}

#[test]
fn text_and_event_budgets_are_enforced_before_queue_growth() {
    let job = job(1);
    job.observe(delta("a".repeat(MAX_OUTPUT)));
    job.observe(delta("b".into()));
    assert!(job.cancel.is_cancelled());
    let state = job.state.lock().unwrap();
    assert_eq!(state.bytes, MAX_OUTPUT);
    assert_eq!(state.deltas.len(), 1);
    drop(state);
    let job = super::tests::job(1);
    job.state.lock().unwrap().events = MAX_EVENTS;
    job.observe(delta(String::new()));
    assert!(job.cancel.is_cancelled());
    assert!(job.state.lock().unwrap().deltas.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poll_drains_bounded_batches_before_delivering_a_terminal_result() {
    let parent = parent(FauxScript::new()).await;
    let manager = ChildAgents::default();
    let job = job(7);
    for _ in 0..4 {
        job.observe(delta("x".repeat(32 * 1024)));
    }
    job.state.lock().unwrap().result = Some(json!({"status":"completed","text":"report"}));
    manager.jobs.lock().unwrap().insert("c".into(), job);
    let a = manager
        .handle(&parent, 7, json!({"op":"poll","childId":"c"}))
        .await
        .unwrap();
    assert_eq!(a["status"], "running");
    assert!(a["result"].is_null());
    assert_eq!(a["deltas"].as_array().unwrap().len(), 2);
    let b = manager
        .handle(&parent, 7, json!({"op":"poll","childId":"c"}))
        .await
        .unwrap();
    assert_eq!(b["status"], "done");
    assert_eq!(b["result"]["text"], "report");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_is_bounded_even_while_disposed_children_are_cleaning_up() {
    let parent = parent(FauxScript::new()).await;
    let manager = ChildAgents::default();
    manager.active.store(MAX_JOBS, Ordering::Release);
    assert!(manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap_err()
        .contains("cleanup"));
    assert!(manager.jobs.lock().unwrap().is_empty());
    manager.active.store(0, Ordering::Release);
    for n in 0..MAX_JOBS {
        manager.jobs.lock().unwrap().insert(format!("c{n}"), job(7));
    }
    assert!(manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap_err()
        .contains("capacity"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roster_reports_live_children_and_stop_terminates_by_id() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow report")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(
            &parent,
            7,
            json!({"op":"start","task":"survey the repo","role":"scout"}),
        )
        .await
        .unwrap();
    let id = started["childId"].as_str().unwrap();

    let roster = manager.roster();
    let row = roster
        .iter()
        .find(|row| row["childId"] == id)
        .expect("the running child must be listed");
    assert_eq!(row["status"], "running");
    assert_eq!(row["role"], "scout");
    assert_eq!(row["taskPreview"], "survey the repo");
    assert!(row["startedMs"].as_u64().unwrap() > 0);
    assert!(row["finishedMs"].is_null());

    let stopped = manager.stop(id).unwrap();
    assert_eq!(stopped["status"], "cancelled");
    let row = manager
        .roster()
        .into_iter()
        .find(|row| row["childId"] == id)
        .expect("the terminated child stays in the roster until dispose");
    assert_eq!(row["status"], "failed");
    assert!(row["finishedMs"].as_u64().unwrap() > 0);
    assert!(manager.stop("nope").is_err());

    manager
        .handle(&parent, 7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(manager.roster().is_empty());
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// Stopping an already-terminal child reports `already-finished` and must not
/// rewrite its recorded outcome or terminal timestamp: the roster is a history,
/// not a live view a late Stop click can corrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_on_a_finished_child_reports_already_finished_without_rewriting_it() {
    let parent = parent(FauxScript::new().with_text("child report")).await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = started["childId"].as_str().unwrap();
    let finished = wait(&manager, &parent, id).await;
    assert_eq!(finished["status"], "done");

    let before = manager
        .roster()
        .into_iter()
        .find(|row| row["childId"] == id)
        .expect("finished child stays in the roster until dispose");
    assert_eq!(before["status"], "done");
    let finished_ms = before["finishedMs"].clone();
    assert!(finished_ms.as_u64().unwrap() > 0);

    let stopped = manager.stop(id).unwrap();
    assert_eq!(stopped["status"], "already-finished");
    let after = manager
        .roster()
        .into_iter()
        .find(|row| row["childId"] == id)
        .expect("still listed");
    assert_eq!(
        after["status"], "done",
        "stop must not flip a done child to failed"
    );
    assert_eq!(
        after["finishedMs"], finished_ms,
        "the terminal timestamp is history"
    );

    manager
        .handle(&parent, 7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// `finished_ms` is first-write-wins: a later `fail()` (for example a Stop
/// click) must not move the timestamp of a child that already settled.
#[test]
fn mark_finished_keeps_the_first_terminal_timestamp() {
    let child = job(7);
    {
        let mut state = child.state.lock().unwrap();
        state.finished_ms = Some(1_000);
    }
    child.mark_finished();
    assert_eq!(
        child.state.lock().unwrap().finished_ms,
        Some(1_000),
        "mark_finished must not overwrite an existing terminal timestamp"
    );

    let fresh = job(7);
    fresh.mark_finished();
    let first = fresh.state.lock().unwrap().finished_ms;
    assert!(first.is_some());
    fresh.mark_finished();
    assert_eq!(fresh.state.lock().unwrap().finished_ms, first);
}

/// `stop` is the HOST path: it is intentionally not owner-scoped, unlike the
/// plugin path (`handle_existing` / `cancel`), which rejects a foreign owner.
/// Pin both halves so the missing owner filter stays a documented decision
/// instead of an accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_is_host_privileged_while_the_plugin_path_stays_owner_scoped() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow report")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = started["childId"].as_str().unwrap();

    // Plugin path: a different owner cannot cancel it.
    assert!(
        manager
            .handle(&parent, 8, json!({"op":"cancel","childId":id}))
            .await
            .is_err(),
        "a foreign owner must not cancel another plugin's child"
    );
    assert_eq!(manager.roster()[0]["status"], "running");

    // Host path: the host itself may stop any child it holds.
    assert_eq!(manager.stop(id).unwrap()["status"], "cancelled");
    assert_eq!(manager.roster()[0]["status"], "failed");

    manager
        .handle(&parent, 7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn child_limits_reject_wrong_types_and_out_of_range_values() {
    for value in [json!(-1), json!(0), json!(33), json!("8"), Value::Null] {
        assert!(bounded_number(&json!({"maxTurns":value}), "maxTurns", 8, 1, 32).is_err());
    }
    assert_eq!(bounded_number(&json!({}), "maxTurns", 8, 1, 32).unwrap(), 8);
}

mod invocation;
mod security;

fn observation_query(parent: &str, command: &str) -> Value {
    json!({"type":command,"version":1,"parentSessionId":parent})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_report_budget_failure_is_terminal_and_evictable() {
    let report = "report";
    let parent = parent(FauxScript::new().with_text(report)).await;
    let child_job = job(7);
    {
        let mut state = child_job.state.lock().unwrap();
        // Streamed text still fits; adding the final report exceeds the budget.
        state.bytes = MAX_OUTPUT - 2 * report.len() + 1;
        state.parent_session_id = "parent".into();
    }
    let session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "over-budget".into(),
                created_at: 0,
                parent_session_id: Some("parent".into()),
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    let child = parent
        .create_child(session, Arc::new(Emitter(child_job.clone())), 8, None)
        .await
        .unwrap();
    run_child(child, child_job.clone(), "report".into(), 8, 10).await;
    let manager = ChildAgents::default();
    manager
        .observations
        .lock()
        .unwrap()
        .remember("over-budget".into(), child_job);
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let row = &roster["children"][0];
    assert_eq!(row["status"], "failed");
    assert!(row["finishedMs"].as_u64().is_some(), "{row}");
    let page = manager
        .query(
            "parent",
            json!({"type":"get_child_agent_events","version":1,"parentSessionId":"parent",
        "epoch":roster["epoch"],"childId":"over-budget","afterSeq":0,"limit":32}),
        )
        .unwrap();
    assert_eq!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == json!({"type":"child_status","status":"failed"}))
            .count(),
        1
    );
    for index in 0..32 {
        let running = job(7);
        running.state.lock().unwrap().parent_session_id = "parent".into();
        manager
            .observations
            .lock()
            .unwrap()
            .remember(format!("running-{index}"), running);
    }
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let children = roster["children"].as_array().unwrap();
    assert_eq!(children.len(), 32);
    assert!(children.iter().all(|row| row["childId"] != "over-budget"));
    assert_eq!(children[0]["childId"], "running-31");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_observations_survive_disposal_and_are_parent_scoped() {
    let parent = parent(FauxScript::new().with_text("native child final")).await;
    let manager = ChildAgents::default();
    let child = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = child["childId"].as_str().unwrap();
    wait(&manager, &parent, id).await;
    manager
        .handle_existing(7, json!({"op":"dispose","childId":id}))
        .unwrap();
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    assert_eq!(roster["children"][0]["childId"], id);
    assert_eq!(roster["children"][0]["status"], "done");
    let query = json!({"type":"get_child_agent_events","version":1,"parentSessionId":"parent",
        "epoch":roster["epoch"],"childId":id,"afterSeq":0,"limit":32});
    let page = manager.query("parent", query.clone()).unwrap();
    assert!(page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"]["type"] == "message_end"));
    assert!(page.to_string().contains("native child final"));
    assert!(!page["resyncRequired"].as_bool().unwrap());
    assert!(manager.query("another-parent", query).is_err());
    assert!(manager
        .query(
            "parent",
            json!({"type":"cancel_child_agent","version":1,
        "parentSessionId":"parent","epoch":"stale","childId":id})
        )
        .is_err());
    let other = manager
        .query("other", observation_query("other", "get_child_agents"))
        .unwrap();
    assert!(other["children"].as_array().unwrap().is_empty());
}

#[test]
fn observation_tails_are_bounded_and_report_eviction_and_oversized_events() {
    let manager = ChildAgents::default();
    let child = job(7);
    {
        let mut state = child.state.lock().unwrap();
        state.parent_session_id = "parent".into();
        for _ in 0..400 {
            state.journal.push(json!({"type":"owned-fixture"}));
        }
        state.journal.observe(&delta("x".repeat(40000)));
    }
    manager
        .observations
        .lock()
        .unwrap()
        .remember("child".into(), child);
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let mut request = json!({"type":"get_child_agent_events","version":1,"parentSessionId":"parent",
        "epoch":roster["epoch"],"childId":"child","afterSeq":0,"limit":32});
    let page = manager.query("parent", request.clone()).unwrap();
    assert_eq!(page["resyncRequired"], true);
    assert_eq!(page["events"].as_array().unwrap().len(), 32);
    assert_eq!(page["hasMore"], true);
    request["afterSeq"] = json!(400);
    let last = manager.query("parent", request.clone()).unwrap();
    assert_eq!(last["events"][0]["event"]["type"], "observation_gap");
    assert_eq!(last["nextSeq"], 401);
    assert!(serde_json::to_vec(&last).unwrap().len() <= 131072);
    request["afterSeq"] = json!(402);
    assert!(manager.query("parent", request).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_child_cancel_reaches_the_owned_runner() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(&parent, 7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let reply = manager
        .query(
            "parent",
            json!({"type":"cancel_child_agent","version":1,
        "parentSessionId":"parent","epoch":roster["epoch"],"childId":started["childId"]}),
        )
        .unwrap();
    assert_eq!(reply["status"], "cancelled");
    let final_value = wait(&manager, &parent, started["childId"].as_str().unwrap()).await;
    assert_eq!(final_value["status"], "failed");
    assert!(final_value["error"]
        .as_str()
        .unwrap()
        .contains("cancelled by user"));
}

#[test]
fn observation_retention_evicts_finished_children_without_reviving_handles() {
    let manager = ChildAgents::default();
    for i in 0..40 {
        let child = job(7);
        child.state.lock().unwrap().parent_session_id = "parent".into();
        child.fail("synthetic completion");
        manager
            .observations
            .lock()
            .unwrap()
            .remember(format!("child-{i}"), child);
    }
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let children = roster["children"].as_array().unwrap();
    assert_eq!(children.len(), 32);
    assert_eq!(children[0]["childId"], "child-39");
    assert_eq!(children[31]["childId"], "child-8");
    assert!(manager.jobs.lock().unwrap().is_empty());
}

#[test]
fn cancellation_keeps_native_tool_cleanup_events_observable() {
    let child = job(7);
    child.observe(AgentEvent::ToolExecutionStart {
        tool_call_id: "tool".into(),
        tool_name: "owned-fixture".into(),
        args: json!({}),
    });
    child.fail("cancel requested");
    child.observe(AgentEvent::ToolExecutionEnd {
        tool_call_id: "tool".into(),
        tool_name: "owned-fixture".into(),
        result: Default::default(),
        is_error: true,
    });
    let state = child.state.lock().unwrap();
    assert!(state.error.is_some());
    // The journal is queried through the real bounded native envelope below.
    drop(state);
    let manager = ChildAgents::default();
    child.state.lock().unwrap().parent_session_id = "parent".into();
    manager
        .observations
        .lock()
        .unwrap()
        .remember("child".into(), child);
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let page=manager.query("parent",json!({"type":"get_child_agent_events","version":1,
        "parentSessionId":"parent","epoch":roster["epoch"],"childId":"child","afterSeq":0,"limit":32})).unwrap();
    let kinds: Vec<_> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["tool_execution_start", "child_status", "tool_execution_end"]
    );
}

#[tokio::test]
async fn observation_roster_preserves_parent_tool_call_binding() {
    let parent = parent(FauxScript::new().with_text("bound child")).await;
    let manager = ChildAgents::default();
    let started = manager
        .handle(
            &parent,
            7,
            json!({"op":"start","task":"report","role":"same-role","toolCallId":"parent-call-42"}),
        )
        .await
        .unwrap();
    let roster = manager
        .query("parent", observation_query("parent", "get_child_agents"))
        .unwrap();
    let row = roster["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["childId"] == started["childId"])
        .unwrap();
    assert_eq!(row["parentToolCallId"], "parent-call-42");
    assert_eq!(observation_capabilities()["parentToolCallId"], true);
    assert!(manager
        .handle(
            &parent,
            7,
            json!({"op":"start","task":"report","toolCallId":42})
        )
        .await
        .is_err());
    manager.cancel_owner(7);
}
