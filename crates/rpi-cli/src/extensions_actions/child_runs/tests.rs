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
async fn wait(manager: &ChildRuns, parent: &AgentHarness, id: &str) -> Value {
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
    let manager = ChildRuns::default();
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
