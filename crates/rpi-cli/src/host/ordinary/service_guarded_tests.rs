// Owner coordinator guard integration only; no administrative journal or IPC.
use super::*;

fn managed_gate_provider() -> Arc<FauxProvider> {
    FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("finished"),
    )
}
fn service_guard_rejected(error: ServiceError) {
    assert!(
        matches!(error, ServiceError::Backend(BackendError::GuardRejected(_))),
        "{error:?}"
    );
}
async fn managed_writer_held(record: &AgentRecord) {
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn service_guards_restore_live_on_rejection_and_cancel_only_the_exact_ticket() {
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture.record("service-guard-live", &provider, true).await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider.clone(), Some(gate.clone()), None),
    )
    .await;
    let idle = client.snapshot().await.unwrap();
    let ticket = client.submit(prompt("guarded-managed-run")).await.unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let current = fixture.catalog.guard(&running.record);
    for guard in [
        fixture.catalog.guard(&idle.record),
        CatalogGuard {
            epoch: crate::host::HostEpoch::new(),
            revision: current.revision,
        },
    ] {
        service_guard_rejected(
            service
                .close_ordinary_guarded(record.spec.id, guard)
                .await
                .unwrap_err(),
        );
        service_guard_rejected(
            service
                .cancel_ordinary_guarded(record.spec.id, guard, ticket.id)
                .await
                .unwrap_err(),
        );
        assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Live));
        assert_eq!(service.owner_count(), 1);
        assert!(service.ordinary_client(record.spec.id).is_ok());
        let snapshot = client.snapshot().await.unwrap();
        assert!(!snapshot.cancel_requested && !snapshot.retiring && !snapshot.closed);
        assert_eq!(snapshot.ticket, Some(ticket.id));
        assert!(ticket.completion.borrow().is_none());
    }
    managed_writer_held(&record).await;
    assert!(!service
        .cancel_ordinary_guarded(record.spec.id, current, RunId::new())
        .await
        .unwrap());
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert!(service
        .cancel_ordinary_guarded(record.spec.id, current, ticket.id)
        .await
        .unwrap());
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    provider.script().set_responses(vec![
        FauxStep::tool_call("gate", serde_json::json!({})),
        FauxStep::text("later-finished"),
    ]);
    let later = client.submit(prompt("later-managed-run")).await.unwrap();
    permits(&gate.entered, 1).await;
    let latest = client.snapshot().await.unwrap();
    assert!(!service
        .cancel_ordinary_guarded(
            record.spec.id,
            fixture.catalog.guard(&latest.record),
            ticket.id
        )
        .await
        .unwrap());
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert!(later.completion.borrow().is_none());
    gate.release.add_permits(1);
    complete(&later).await;
    let idle = client.snapshot().await.unwrap();
    service
        .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&idle.record))
        .await
        .unwrap();
    assert_eq!(service.phase(record.spec.id), None);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_stale_guard_does_not_turn_a_closefailed_owner_back_into_live() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("service-guard-close-failure", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    let idle = client.snapshot().await.unwrap();
    let guard = fixture.catalog.guard(&idle.record);
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_guarded_closed BEFORE UPDATE ON agents WHEN OLD.resident=1 AND NEW.resident=0 BEGIN SELECT RAISE(ABORT,'injected final Closed failure'); END;").unwrap();
    assert!(matches!(
        service.close_ordinary_guarded(record.spec.id, guard).await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let closing = client.snapshot().await.unwrap();
    assert!(closing.retiring);
    assert_eq!(closing.record.state, AgentState::Closing);
    service_guard_rejected(
        service
            .close_ordinary_guarded(record.spec.id, guard)
            .await
            .unwrap_err(),
    );
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert!(matches!(
        service.ordinary_client(record.spec.id),
        Err(ServiceError::Backend(BackendError::Faulted))
    ));
    db.execute_batch("DROP TRIGGER reject_guarded_closed")
        .unwrap();
    service
        .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&closing.record))
        .await
        .unwrap();
    assert_eq!(service.owner_count(), 0);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_closing_failure_keeps_owner_and_unconditional_shutdown_still_signals_and_retries()
{
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("service-guard-closing-fault", &provider, true)
        .await;
    let gate = Gate::new();
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(gate.clone()), None),
    )
    .await;
    let ticket = client
        .submit(prompt("guarded-closing-fault"))
        .await
        .unwrap();
    permits(&gate.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_guarded_closing BEFORE UPDATE ON agents WHEN json_extract(NEW.data,'$.state')='closing' BEGIN SELECT RAISE(ABORT,'injected Closing failure'); END;").unwrap();
    assert!(matches!(
        service
            .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&running.record))
            .await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    let unchanged = client.snapshot().await.unwrap();
    assert!(!unchanged.cancel_requested && !unchanged.retiring);
    assert_eq!(unchanged.ticket, Some(ticket.id));
    assert!(ticket.completion.borrow().is_none());
    managed_writer_held(&record).await;
    assert!(matches!(
        service.shutdown().await,
        Err(ServiceError::ShutdownIncomplete { owners: 1 })
    ));
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    assert!(service.is_stopping());
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert!(client.snapshot().await.unwrap().retiring);
    assert!(matches!(
        service
            .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&running.record))
            .await,
        Err(ServiceError::Stopping)
    ));
    assert!(matches!(
        service
            .cancel_ordinary_guarded(
                record.spec.id,
                fixture.catalog.guard(&running.record),
                ticket.id
            )
            .await,
        Err(ServiceError::Stopping)
    ));
    db.execute_batch("DROP TRIGGER reject_guarded_closing")
        .unwrap();
    service.close_ordinary(record.spec.id).await.unwrap();
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn abandoned_guarded_close_keeps_service_owner_writer_and_shutdown_fence_until_late_callback()
{
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("service-guard-late-callback", &provider, true)
        .await;
    let gate = Gate::new();
    gate.release.add_permits(1);
    let late = Gate::new();
    let callback = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = callback.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            gate.release.acquire().await.unwrap().forget();
            None
        })
    });
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(gate), Some(hook)),
    )
    .await;
    let ticket = client.submit(prompt("managed-late-guarded")).await.unwrap();
    permits(&late.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let guard = fixture.catalog.guard(&running.record);
    let closer = service.clone();
    let id = record.spec.id;
    let waiter = tokio::spawn(async move { closer.close_ordinary_guarded(id, guard).await });
    wait_state(&fixture.catalog, id, AgentState::Closing).await;
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(service.phase(id), Some(OwnerPhase::Closing));
    assert_eq!(service.owner_count(), 1);
    assert!(ticket.completion.borrow().is_none());
    let closing = client.snapshot().await.unwrap();
    assert!(closing.cancel_requested && closing.retiring);
    assert!(matches!(
        service
            .close_ordinary_guarded(id, fixture.catalog.guard(&closing.record))
            .await,
        Err(ServiceError::Busy)
    ));
    let draining = service.clone();
    let shutdown = tokio::spawn(async move { draining.shutdown().await });
    tokio::time::timeout(DEADLINE, async {
        while !service.is_stopping() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!shutdown.is_finished());
    managed_writer_held(&record).await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(matches!(
        service.close_ordinary_guarded(id, guard).await,
        Err(ServiceError::Stopping)
    ));
    assert!(matches!(
        service.cancel_ordinary_guarded(id, guard, ticket.id).await,
        Err(ServiceError::Stopping)
    ));
    late.release.add_permits(1);
    ticket.wait().await.unwrap();
    tokio::time::timeout(DEADLINE, shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(service.is_closed());
    assert_eq!(service.owner_count(), 0);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn guarded_close_never_clears_a_startfailed_fence_without_a_native_owner() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("service-guard-startfailed", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_start_rollback BEFORE UPDATE ON agents WHEN OLD.resident=1 AND NEW.resident=0 BEGIN SELECT RAISE(ABORT,'injected startup rollback failure'); END;").unwrap();
    assert!(matches!(
        service
            .restore_ordinary(
                record.spec.id,
                fixture.catalog.guard(&record),
                OrdinaryLimits::default(),
                Box::new(|_, _, _| Err(BackendError::Binding("injected preflight rejection")))
            )
            .await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::StartFailed));
    let current = fixture.catalog.get(record.spec.id).await.unwrap();
    assert!(matches!(
        service
            .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&current))
            .await,
        Err(ServiceError::Backend(BackendError::Faulted))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::StartFailed));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    db.execute_batch("DROP TRIGGER reject_start_rollback")
        .unwrap();
    // Test-owned reconciliation of a known pre-writer binder rejection, not a
    // production recovery API. Guarded close must not invent this transition.
    let current = fixture
        .catalog
        .transition(
            record.spec.id,
            fixture.catalog.guard(&current),
            AgentState::Closed,
        )
        .await
        .unwrap();
    assert!(matches!(
        service
            .close_ordinary_guarded(record.spec.id, fixture.catalog.guard(&current))
            .await,
        Err(ServiceError::Backend(BackendError::Faulted))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::StartFailed));
    service.close_ordinary(record.spec.id).await.unwrap();
    service.shutdown().await.unwrap();
}
