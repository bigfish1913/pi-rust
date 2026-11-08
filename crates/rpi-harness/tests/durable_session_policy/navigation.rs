//! Candidate-branch admission; all fixtures are synthetic and provider-free.
use super::*;
use rpi_harness::agent_harness::{NavigationOutcome, NavigationResult};

fn failed(result: NavigationResult) -> String {
    match result.outcome {
        NavigationOutcome::Failed { error, .. } => {
            assert_eq!(error.code, "navigation_failed");
            error.message
        }
        other => panic!("expected rejected navigation, got {other:?}"),
    }
}

#[tokio::test]
async fn validates_candidate_not_current_before_moving() {
    let h = seeded().await;
    h.follow_up(AgentMessage::User(UserMessage::new("old queue", 0)))
        .await
        .unwrap();
    let child = h
        .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
        .await
        .unwrap();
    let session = h.session();
    session.create_lane("fixtures", None).await.unwrap();
    let mut targets = Vec::new();
    for (id, kind, data) in [
        ("bad-legacy-one", "durable-state", None),
        ("bad-legacy-two", "rpi-durable-runtime", None),
        (
            "bad-corrupt",
            CONTROL,
            Some(json!({"private":"NOT-TO-BE-REFLECTED"})),
        ),
    ] {
        session.move_lane("fixtures", None).await.unwrap();
        custom(session, "fixtures", id, kind, data).await;
        targets.push(id.to_owned());
    }
    // A genuine side-lane receipt is valid for that lane, never for main.
    session.move_lane("fixtures", None).await.unwrap();
    let side =
        DurableService::bind_lane(&h, DurableActor::owner("policy-owner").unwrap(), "fixtures");
    apply(
        &side,
        "side",
        json!({"action":"enqueue", "text":"side queue"}),
    )
    .await;
    let side_leaf = side.snapshot().await.unwrap().scope.entry_id.unwrap();
    targets.push(side_leaf.clone());

    // Copy a genuine main receipt with its complete original entry identity.
    // Reserve disjoint IDs: duplicate-entry failure must not mask foreign scope.
    let other = strict(memory()).await;
    for _ in 0..100 {
        other.session().id_generator().next();
    }
    apply(
        &service(&other),
        "foreign",
        json!({"action":"enqueue", "text":"foreign"}),
    )
    .await;
    let foreign_id = service(&other)
        .snapshot()
        .await
        .unwrap()
        .scope
        .entry_id
        .unwrap();
    let Entry::Custom(foreign) = other
        .session()
        .get_entry(&foreign_id)
        .await
        .unwrap()
        .unwrap()
    else {
        panic!()
    };
    session.move_lane("fixtures", None).await.unwrap();
    custom(session, "fixtures", &foreign_id, CONTROL, foreign.data).await;
    targets.push(foreign_id);
    assert!(matches!(
        h.lane("fixtures")
            .navigate_tree(Some(&side_leaf), false, None, None)
            .await
            .unwrap()
            .outcome,
        NavigationOutcome::Completed { .. }
    ));

    let before = service(&h).snapshot().await.unwrap();
    let queues = h.queued_messages().await.unwrap();
    let bytes = image(session).await;
    for target in targets.iter().map(String::as_str).chain(["missing-target"]) {
        let error = failed(
            bounded(h.navigate_tree(Some(target), false, None, None))
                .await
                .unwrap(),
        );
        assert!(!error.contains("NOT-TO-BE-REFLECTED"));
        assert_eq!(service(&h).snapshot().await.unwrap(), before);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert_eq!(image(session).await, bytes);
        assert!(!child.scope.is_cancelled());
    }
    // Empty is a real candidate, not a fallback to current. Returning explicitly
    // restores the original native receipt; a stale guard remains stale.
    let pending = request(&service(&h), "stale-nav", json!({"action":"pause"})).await;
    let prior = service(&h)
        .request_outcome("start")
        .await
        .unwrap()
        .unwrap()
        .entry_id;
    for target in [
        None,
        Some(""),
        Some(prior.as_str()),
        before.scope.leaf_id.as_deref(),
    ] {
        let result = h.navigate_tree(target, false, None, None).await.unwrap();
        assert!(matches!(
            result.outcome,
            NavigationOutcome::Completed { .. }
        ));
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert!(!child.scope.is_cancelled());
        if target.is_none() || target == Some("") {
            let empty = service(&h).snapshot().await.unwrap();
            assert_eq!(empty.scope.leaf_id, None);
            assert!(empty.state.workflow.is_none());
            assert!(empty.state.queue.is_empty());
            assert!(service(&h)
                .request_outcome("enqueue")
                .await
                .unwrap()
                .is_none());
        } else if target == Some(prior.as_str()) {
            let historic = service(&h).snapshot().await.unwrap();
            assert!(historic.state.workflow.is_some());
            assert!(historic.state.queue.is_empty());
            assert!(service(&h)
                .request_outcome("enqueue")
                .await
                .unwrap()
                .is_none());
        }
    }
    assert_eq!(service(&h).snapshot().await.unwrap().state, before.state);
    assert!(service(&h)
        .request_outcome("enqueue")
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        service(&h).mutate(pending).await,
        Err(DurableError::Conflict)
    ));
}

#[tokio::test]
async fn read_failure_race_and_pre_move_cancellation_keep_old_state() {
    let base = memory();
    let probe = probe::Probe::new(base.storage());
    let h = strict(Session::new(probe.clone(), None)).await;
    apply(
        &service(&h),
        "root",
        json!({"action":"enqueue", "text":"root"}),
    )
    .await;
    h.follow_up(AgentMessage::User(UserMessage::new("stay", 0)))
        .await
        .unwrap();
    base.create_lane("candidate", None).await.unwrap();
    custom(
        &base,
        "candidate",
        "candidate",
        "synthetic-navigation",
        None,
    )
    .await;
    let child = h
        .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
        .await
        .unwrap();
    let old_leaf = h.view().get_leaf_id().await.unwrap();
    let queues = h.queued_messages().await.unwrap();
    for mode in ["read-failure", "race", "cancel"] {
        let before = service(&h).snapshot().await.unwrap();
        let mut expected = image(&base).await;
        if mode == "read-failure" {
            probe.fail_reads.store(true, Ordering::SeqCst);
            let error = failed(
                h.navigate_tree(Some("candidate"), false, None, None)
                    .await
                    .unwrap(),
            );
            assert!(!error.contains("injected"));
            probe.fail_reads.store(false, Ordering::SeqCst);
        } else {
            probe.pause_branch_reads.store(true, Ordering::SeqCst);
            let owner = h.clone();
            let task = tokio::spawn(async move {
                owner
                    .navigate_tree(Some("candidate"), false, None, None)
                    .await
            });
            bounded(probe.branch_read_entered.notified()).await;
            if mode == "cancel" {
                task.abort();
                assert!(bounded(task).await.unwrap_err().is_cancelled());
            } else {
                custom(&base, "candidate", "racing-writer", "synthetic-race", None).await;
                expected = image(&base).await;
                probe.release_branch_read.notify_one();
                assert!(failed(bounded(task).await.unwrap().unwrap())
                    .contains("changed during validation"));
            }
        }
        let after = bounded(service(&h).snapshot()).await.unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.scope.leaf_id, old_leaf);
        assert_eq!(image(&base).await, expected);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert!(!child.scope.is_cancelled());
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    }
    // Only the pre-move read was dropped. No detached move can publish later.
    probe.release_branch_read.notify_one();
    let result = h
        .navigate_tree(Some("candidate"), false, None, None)
        .await
        .unwrap();
    assert!(matches!(
        result.outcome,
        NavigationOutcome::Completed { .. }
    ));
}
