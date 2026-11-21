use super::*;
use crate::{agent_harness::AgentHarness, events::RunEndEvent, types::AgentHarnessOptions};
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_follows_run_end_callbacks_and_their_durable_context_writes() {
    let provider = FauxProvider::new(FauxScript::new().with_text("done"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let failed = CancellationToken::new();
    let (observer, mut events) = ExecutionObserver::channel(256, failed.clone());
    harness.set_execution_observer(Some(observer));
    let entered = CancellationToken::new();
    let callback_entered = entered.clone();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let callback_harness = harness.clone();
    let _subscription = harness.events().on::<RunEndEvent, _>(move |_| {
        callback_entered.cancel();
        released
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                callback_harness
                    .lane("main")
                    .send_message(
                        rpi_ai::UserMessage::new("callback context", 0).into(),
                        crate::types::SendMessageOptions {
                            trigger_turn: Some(false),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            })
        });
    });
    let request = ExecutionRequest::new(serde_json::json!("request"));
    let run_harness = harness.clone();
    let run_request = request.clone();
    let run = tokio::spawn(async move {
        run_request
            .scope(run_harness.lane("main").prompt_text("hello", vec![]))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.cancelled())
        .await
        .unwrap();
    assert!(harness
        .lane("main")
        .active_operation()
        .await
        .unwrap()
        .is_some());
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, ExecutionEvent::Released { .. }),
            "RunEnd is not lease release"
        );
    }
    release.send(()).unwrap();
    let result = run.await.unwrap().unwrap();
    let terminal = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            ExecutionEvent::Released {
                identity,
                run_id,
                recovery_required,
                outcome,
                ..
            } => {
                assert_eq!(run_id, result.run_id);
                assert!(!recovery_required);
                assert!(matches!(outcome, Some(ExecutionOutcome::Run(_))));
                break identity;
            }
            _ => {}
        }
    };
    assert_eq!(terminal.request_id, Some(serde_json::json!("request")));
    assert_eq!(
        terminal.execution_id,
        request.last_execution().unwrap().execution_id
    );
    assert!(harness
        .lane("main")
        .active_operation()
        .await
        .unwrap()
        .is_none());
    let messages = harness
        .session()
        .view("main")
        .find_entries(&crate::session::types::EntryQuery::default())
        .await
        .unwrap();
    assert!(messages.iter().any(|message| serde_json::to_string(message)
        .unwrap()
        .contains("callback context")));
    assert!(!failed.is_cancelled());
}

#[tokio::test]
async fn overflow_and_reader_loss_fail_closed_instead_of_losing_release() {
    for close_reader in [false, true] {
        let failed = CancellationToken::new();
        let (observer, receiver) = ExecutionObserver::channel(3, failed.clone());
        let first = ExecutionLease::new(observer.clone(), "main");
        let second = ExecutionLease::new(observer, "main");
        if close_reader {
            drop(receiver);
        }
        first.release("first".into(), false);
        second.release("second".into(), false);
        assert!(failed.is_cancelled());
    }
}

fn tool_partial() -> AgentEvent {
    AgentEvent::ToolExecutionUpdate {
        tool_call_id: "call".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({}),
        partial_result: Arc::new(rpi_agent::types::AgentToolResult::default()),
    }
}

#[tokio::test]
async fn tool_partial_flood_preserves_start_release_and_barrier_order() {
    let failed = CancellationToken::new();
    let (observer, mut events) = ExecutionObserver::channel(4, failed.clone());
    let lease = ExecutionLease::new(observer.clone(), "main");
    let emitter = lease.emitter(Arc::new(rpi_agent::CollectorEmitter::default()), "run");
    for _ in 0..1000 {
        emitter.try_emit(tool_partial());
    }
    lease.release("run".into(), false);
    assert_eq!(observer.dropped_progress(), 998);
    assert!(!failed.is_cancelled());
    let flush_observer = observer.clone();
    let flush = tokio::spawn(async move { flush_observer.flush().await });
    let mut sequence = Vec::new();
    loop {
        match events.recv().await.unwrap() {
            ExecutionEvent::Started { .. } => sequence.push("start"),
            ExecutionEvent::Progress {
                event: AgentEvent::ToolExecutionUpdate { .. },
                ..
            } => sequence.push("partial"),
            ExecutionEvent::Released { .. } => sequence.push("release"),
            ExecutionEvent::Barrier(done) => {
                let _ = done.send(());
                break;
            }
            _ => panic!("unexpected event"),
        }
    }
    flush.await.unwrap();
    assert_eq!(sequence, ["start", "partial", "partial", "release"]);
    assert!(!failed.is_cancelled());
}

#[tokio::test]
async fn awaited_message_boundaries_backpressure_without_consuming_release_capacity() {
    let failed = CancellationToken::new();
    let (observer, mut events) = ExecutionObserver::channel(3, failed.clone());
    let lease = ExecutionLease::new(observer.clone(), "main");
    observer
        .send(ExecutionEvent::SessionProgress(AgentEvent::TurnStart))
        .await;
    // The progress budget is full, but the second async event waits, not drops.
    let next_observer = observer.clone();
    let mut next =
        Box::pin(next_observer.send(ExecutionEvent::SessionProgress(AgentEvent::AgentStart)));
    assert!(futures::poll!(&mut next).is_pending());
    assert_eq!(observer.dropped_progress(), 0);
    lease.release("run".into(), false);
    assert!(!failed.is_cancelled());
    assert!(matches!(
        events.recv().await,
        Some(ExecutionEvent::Started { .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(ExecutionEvent::SessionProgress(AgentEvent::TurnStart))
    ));
    next.await;
    assert!(matches!(
        events.recv().await,
        Some(ExecutionEvent::Released { .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(ExecutionEvent::SessionProgress(AgentEvent::AgentStart))
    ));
    assert!(!failed.is_cancelled());
}

#[tokio::test]
async fn reader_loss_unblocks_an_awaited_progress_budget() {
    let failed = CancellationToken::new();
    let (observer, events) = ExecutionObserver::channel(3, failed.clone());
    observer.publish(ExecutionEvent::SessionProgress(tool_partial()));
    let mut next = Box::pin(observer.send(ExecutionEvent::SessionProgress(AgentEvent::AgentStart)));
    assert!(futures::poll!(&mut next).is_pending());
    drop(events);
    tokio::time::timeout(Duration::from_secs(1), next)
        .await
        .unwrap();
    assert!(failed.is_cancelled());
}

#[tokio::test]
async fn partial_reader_loss_and_unexpected_sync_boundary_loss_still_fail_closed() {
    for close_reader in [false, true] {
        let failed = CancellationToken::new();
        let (observer, events) = ExecutionObserver::channel(3, failed.clone());
        observer.publish(ExecutionEvent::SessionProgress(tool_partial()));
        if close_reader {
            drop(events);
            observer.publish(ExecutionEvent::SessionProgress(tool_partial()));
        } else {
            observer.publish(ExecutionEvent::SessionProgress(AgentEvent::AgentStart));
        }
        assert!(failed.is_cancelled());
        assert_eq!(observer.dropped_progress(), 0);
    }
}

#[tokio::test]
async fn request_scope_is_not_inherited_by_an_autonomous_task() {
    let failed = CancellationToken::new();
    let (observer, mut receiver) = ExecutionObserver::channel(8, failed);
    let request = ExecutionRequest::new(serde_json::json!("parent"));
    request
        .scope(async {
            let owned = ExecutionLease::new(observer.clone(), "main");
            assert_eq!(owned.identity.request_id, Some(serde_json::json!("parent")));
            tokio::spawn(async move {
                let autonomous = ExecutionLease::new(observer, "main");
                assert!(autonomous.identity.request_id.is_none());
            })
            .await
            .unwrap();
        })
        .await;
    assert!(receiver.try_recv().is_ok());
}

#[tokio::test]
async fn staged_outcome_is_not_proof_of_durable_finalization() {
    let (observer, mut events) = ExecutionObserver::channel(8, CancellationToken::new());
    let mut lease = ExecutionLease::new(observer, "main");
    lease.durable_intent = true;
    lease.outcome = Some(ExecutionOutcome::Skipped);
    lease.release("run".into(), false);
    assert!(matches!(
        events.recv().await.unwrap(),
        ExecutionEvent::Started { .. }
    ));
    assert!(matches!(
        events.recv().await.unwrap(),
        ExecutionEvent::Released {
            recovery_required: true,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_polls_release_distinct_leases_without_a_terminal_run_end() {
    use rpi_ai::{
        providers::faux::{faux_assistant_message, DeferredPoll, FauxStep},
        types::{DeferredHandle, StopReason},
        Provider,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let script = FauxScript::new();
    let provider = FauxProvider::new(script.clone());
    let handle = DeferredHandle {
        provider: provider.id().into(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().into(),
        id: "parked".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    script.set_responses(vec![FauxStep::Message(parked)]);
    provider.push_deferred_poll(DeferredPoll::StillDeferred(DeferredHandle {
        id: "parked-again".into(),
        ..handle.clone()
    }));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let (observer, mut events) = ExecutionObserver::channel(256, CancellationToken::new());
    harness.set_execution_observer(Some(observer));
    let ends = Arc::new(AtomicUsize::new(0));
    let count = ends.clone();
    let _callback = harness.events().on::<RunEndEvent, _>(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });
    let first = ExecutionRequest::new(serde_json::json!("first"));
    let result = first
        .scope(harness.lane("main").prompt_text("hello", vec![]))
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Suspended { .. }
    ));
    let poll = ExecutionRequest::new(serde_json::json!("poll"));
    let resumed = poll.scope(harness.resume(&handle.id)).await.unwrap();
    assert_eq!(result.run_id, resumed.run_id);
    assert_ne!(
        first.last_execution().unwrap().execution_id,
        poll.last_execution().unwrap().execution_id
    );
    let mut releases = 0;
    while let Ok(event) = events.try_recv() {
        if let ExecutionEvent::Released {
            recovery_required,
            outcome,
            run_id,
            ..
        } = event
        {
            assert_eq!(run_id, result.run_id);
            assert!(!recovery_required);
            assert!(matches!(
                outcome,
                Some(ExecutionOutcome::Run(HarnessRunOutcome::Suspended { .. }))
            ));
            releases += 1;
        }
    }
    assert_eq!(releases, 2);
    assert_eq!(ends.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness
            .session()
            .find_open_operations("main", None)
            .await
            .unwrap()
            .len(),
        1
    );
}
