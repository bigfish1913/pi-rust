// Native administrative preconditions only, not lifecycle request receipts.

fn rejected_guard(error: BackendError) {
    assert!(matches!(error, BackendError::GuardRejected(_)), "{error:?}");
}
fn gate_provider() -> Arc<FauxProvider> {
    FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("gate-finished"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_cancel_checks_epoch_revision_and_exact_ticket_without_touching_a_later_run() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("guarded-cancel", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider.clone(), Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let idle = client.snapshot().await.unwrap();
    let ticket = client.submit(prompt("guarded-cancel")).await.unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let current = fixture.catalog.guard(&running.record);
    let foreign = CatalogGuard {
        epoch: crate::host::HostEpoch::new(),
        revision: current.revision,
    };
    for guard in [fixture.catalog.guard(&idle.record), foreign] {
        rejected_guard(client.cancel_guarded(guard, ticket.id).await.unwrap_err());
    }
    assert!(!client.cancel_guarded(current, RunId::new()).await.unwrap());
    assert!(ticket.completion.borrow().is_none());
    assert_eq!(client.snapshot().await.unwrap().ticket, Some(ticket.id));
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert!(client.cancel_guarded(current, ticket.id).await.unwrap());
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    let idle = client.snapshot().await.unwrap();
    rejected_guard(client.cancel_guarded(current, ticket.id).await.unwrap_err());
    assert!(!client
        .cancel_guarded(fixture.catalog.guard(&idle.record), ticket.id)
        .await
        .unwrap());
    provider.script().set_responses(vec![
        FauxStep::tool_call("gate", serde_json::json!({})),
        FauxStep::text("later-run-completed"),
    ]);
    let later = client.submit(prompt("a-different-run")).await.unwrap();
    permits(&gate.entered, 1).await;
    let later_snapshot = client.snapshot().await.unwrap();
    rejected_guard(client.cancel_guarded(current, later.id).await.unwrap_err());
    assert!(!client
        .cancel_guarded(fixture.catalog.guard(&later_snapshot.record), ticket.id)
        .await
        .unwrap());
    assert!(later.completion.borrow().is_none());
    assert_eq!(client.snapshot().await.unwrap().ticket, Some(later.id));
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    gate.release.add_permits(1);
    complete(&later).await;
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_close_rejects_stale_observations_without_retiring_cancelling_or_writing() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("guarded-close", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider, Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let idle = client.snapshot().await.unwrap();
    let ticket = client.submit(prompt("must-stay-live")).await.unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let original = bytes(&record);
    let foreign = CatalogGuard {
        epoch: crate::host::HostEpoch::new(),
        revision: running.record.revision,
    };
    for guard in [fixture.catalog.guard(&idle.record), foreign] {
        rejected_guard(owner.close_guarded(guard).await.unwrap_err());
    }
    let after = client.snapshot().await.unwrap();
    assert!(!after.retiring && !after.closed && !after.cancel_requested);
    assert_eq!(after.record, running.record);
    assert_eq!(after.ticket, Some(ticket.id));
    assert!(ticket.completion.borrow().is_none());
    assert_eq!(bytes(&record), original);
    gate.release.add_permits(1);
    complete(&ticket).await;
    let idle = client.snapshot().await.unwrap();
    owner
        .close_guarded(fixture.catalog.guard(&idle.record))
        .await
        .unwrap();
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_close_losing_the_catalog_revision_cas_does_not_cancel_or_retire() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("guarded-cas", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider, Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let ticket = client.submit(prompt("catalog-race")).await.unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let guard = fixture.catalog.guard(&running.record);
    // Deliberately violate the coordinator's sole-writer contract: the native
    // owner's cached guard still matches, but the database revision has moved.
    let competing = fixture
        .catalog
        .transition(record.spec.id, guard, AgentState::WaitingApproval)
        .await
        .unwrap();
    rejected_guard(owner.close_guarded(guard).await.unwrap_err());
    let after = client.snapshot().await.unwrap();
    assert!(!after.retiring && !after.closed && !after.cancel_requested);
    assert_eq!(after.ticket, Some(ticket.id));
    assert!(ticket.completion.borrow().is_none());
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap(),
        competing
    );
    // Undo ONLY this test's synthetic out-of-band mutation so the held native
    // run can settle normally. This is not a supported operator recovery API.
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE agents SET revision=?2,data=?3 WHERE id=?1",
            rusqlite::params![
                record.spec.id.to_string(),
                running.record.revision as i64,
                serde_json::to_string(&running.record).unwrap()
            ]
        )
        .unwrap(),
        1
    );
    drop(db);
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap(),
        running.record
    );
    owner.close_guarded(guard).await.unwrap();
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_close_database_admission_failure_is_not_unconditional_shutdown() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("guarded-db-failure", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider, Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let ticket = client
        .submit(prompt("closing-write-failure"))
        .await
        .unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let guard = fixture.catalog.guard(&running.record);
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_guarded_closing BEFORE UPDATE ON agents WHEN json_extract(NEW.data, '$.state') = 'closing' BEGIN SELECT RAISE(ABORT, 'injected Closing failure'); END;").unwrap();
    assert!(matches!(
        owner.close_guarded(guard).await,
        Err(BackendError::Catalog(_))
    ));
    let after = client.snapshot().await.unwrap();
    assert!(!after.retiring && !after.closed && !after.cancel_requested);
    assert_eq!(after.record, running.record);
    assert!(ticket.completion.borrow().is_none());
    db.execute_batch("DROP TRIGGER refuse_guarded_closing")
        .unwrap();
    owner.close_guarded(guard).await.unwrap();
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_close_of_an_old_closed_handle_cannot_acknowledge_a_reloaded_owner() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture.record("guarded-generation", &provider, false).await;
    let old = fixture
        .restore(&record, binder(provider.clone(), None, None))
        .await;
    old.close().await.unwrap();
    let closed = fixture.catalog.get(record.spec.id).await.unwrap();
    let guard = fixture.catalog.guard(&closed);
    let new = fixture.restore(&closed, binder(provider, None, None)).await;
    rejected_guard(old.close_guarded(guard).await.unwrap_err());
    let snapshot = new.client().snapshot().await.unwrap();
    assert!(!snapshot.retiring && !snapshot.closed);
    assert_eq!(snapshot.record.state, AgentState::Idle);
    new.close_guarded(fixture.catalog.guard(&snapshot.record))
        .await
        .unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_close_abandoned_waiter_keeps_writer_and_slot_until_late_callback_settles() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("guarded-late", &provider, true).await;
    let gate = Gate::new();
    gate.release.add_permits(1);
    let late = Gate::new();
    let hook_gate = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = hook_gate.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            gate.release.acquire().await.unwrap().forget();
            None
        })
    });
    let owner = Arc::new(
        fixture
            .restore(&record, binder(provider, Some(gate), Some(hook)))
            .await,
    );
    let client = owner.client();
    let ticket = client
        .submit(prompt("guarded-late-callback"))
        .await
        .unwrap();
    permits(&late.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let guard = fixture.catalog.guard(&running.record);
    let observer_owner = owner.clone();
    let observer = tokio::spawn(async move { observer_owner.close_guarded(guard).await });
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let path = record.spec.session.as_ref().unwrap().path.to_str().unwrap();
    assert!(env.try_acquire_writer_lease(path).await.is_err());
    assert!(ticket.completion.borrow().is_none());
    late.release.add_permits(1);
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closed).await;
    let report = ticket.wait().await.unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    let closed = fixture.catalog.get(record.spec.id).await.unwrap();
    owner
        .close_guarded(fixture.catalog.guard(&closed))
        .await
        .unwrap();
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().revision,
        closed.revision
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    assert!(env.try_acquire_writer_lease(path).await.unwrap().is_some());
    fixture.catalog.shutdown().await.unwrap();
}
