// Included under ordinary::tests so coordinator tests use the same real
// Harness/JSONL/writer-lease fixtures as the native ordinary backend tests.
use crate::host::service::{HostService, OwnerPhase, ServiceError};

async fn managed_restore(
    service: &HostService,
    fixture: &Fixture,
    record: &AgentRecord,
    binder: OrdinaryBinder,
) -> OrdinaryClient {
    service
        .restore_ordinary(
            record.spec.id,
            fixture.catalog.guard(record),
            OrdinaryLimits::default(),
            binder,
        )
        .await
        .unwrap()
}
async fn wait_phase(service: &HostService, id: AgentId, phase: Option<OwnerPhase>) {
    tokio::time::timeout(DEADLINE, async {
        while service.phase(id) != phase {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
async fn wait_closed(service: &HostService) {
    tokio::time::timeout(DEADLINE, async {
        while !service.is_closed() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
fn delayed_binder(
    provider: Arc<FauxProvider>,
) -> (
    OrdinaryBinder,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let inner = binder(provider, None, None);
    (
        Box::new(move |spec, env, facts| {
            let _ = entered.send(());
            gate.recv_timeout(DEADLINE)
                .map_err(|_| BackendError::Binding("test startup gate expired"))?;
            inner(spec, env, facts)
        }),
        ready,
        release,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn service_owns_detached_agents_and_real_tools_overlap_without_a_global_lock() {
    let fixture = Fixture::new(2).await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    let mut records = Vec::new();
    let mut tickets = Vec::new();
    for name in ["managed-one", "managed-two"] {
        let provider = FauxProvider::new(
            FauxScript::new()
                .with_tool_call("gate", serde_json::json!({}))
                .with_text("managed-answer"),
        );
        let record = fixture.record(name, &provider, true).await;
        let client = managed_restore(
            &service,
            &fixture,
            &record,
            binder(provider, Some(gate.clone()), None),
        )
        .await;
        tickets.push(client.submit(prompt(name)).await.unwrap());
        drop(client);
        records.push(record);
    }
    permits(&gate.entered, 2).await;
    assert_eq!(service.owner_count(), 2);
    for record in &records {
        assert!(service
            .ordinary_client(record.spec.id)
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .active
            .is_some());
    }
    gate.release.add_permits(2);
    for ticket in &tickets {
        complete(ticket).await;
    }
    for record in &records {
        assert!(String::from_utf8(bytes(record))
            .unwrap()
            .contains("managed-answer"));
        assert_eq!(
            tool_results(record),
            vec![record.spec.launch.cwd.to_string_lossy().into_owned()]
        );
    }
    service.shutdown().await.unwrap();
    assert_eq!(service.owner_count(), 0);
    assert!(service.is_closed());
    service.shutdown().await.unwrap();
    let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(reopened.status().await.unwrap().resident, 0);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_abandoned_startup_installs_owner_and_duplicate_restore_never_takes_over() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("after-client-exit"));
    let record = fixture
        .record("managed-abandoned-start", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let (binder, ready, release) = delayed_binder(provider);
    let task_service = service.clone();
    let id = record.spec.id;
    let guard = fixture.catalog.guard(&record);
    let request = tokio::spawn(async move {
        task_service
            .restore_ordinary(id, guard, OrdinaryLimits::default(), binder)
            .await
    });
    tokio::time::timeout(DEADLINE, ready)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(service.phase(id), Some(OwnerPhase::Starting));
    request.abort();
    let _ = request.await;
    assert!(matches!(
        service
            .restore_ordinary(
                id,
                fixture.catalog.guard(&record),
                OrdinaryLimits::default(),
                Box::new(|_, _, _| panic!("duplicate binder cannot run"))
            )
            .await,
        Err(ServiceError::AlreadyOwned)
    ));
    release.send(()).unwrap();
    wait_phase(&service, id, Some(OwnerPhase::Live)).await;
    let client = service.ordinary_client(id).unwrap();
    complete(
        &client
            .submit(prompt("continue-after-reattach"))
            .await
            .unwrap(),
    )
    .await;
    assert!(String::from_utf8(bytes(&record))
        .unwrap()
        .contains("after-client-exit"));
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_shutdown_fences_startup_and_survives_its_callers_exit() {
    let fixture = Fixture::new(2).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture.record("managed-start-stop", &provider, false).await;
    let other = fixture
        .record("managed-rejected-start", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let (delayed, ready, release) = delayed_binder(provider.clone());
    let task_service = service.clone();
    let id = record.spec.id;
    let guard = fixture.catalog.guard(&record);
    let load = tokio::spawn(async move {
        task_service
            .restore_ordinary(id, guard, OrdinaryLimits::default(), delayed)
            .await
    });
    tokio::time::timeout(DEADLINE, ready)
        .await
        .unwrap()
        .unwrap();
    let shutdown_service = service.clone();
    let shutdown = tokio::spawn(async move { shutdown_service.shutdown().await });
    tokio::time::timeout(DEADLINE, async {
        while !service.is_stopping() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!service.is_closed());
    shutdown.abort();
    let _ = shutdown.await;
    release.send(()).unwrap();
    load.await.unwrap().unwrap();
    assert!(matches!(
        service
            .restore_ordinary(
                other.spec.id,
                fixture.catalog.guard(&other),
                OrdinaryLimits::default(),
                binder(provider, None, None)
            )
            .await,
        Err(ServiceError::Stopping)
    ));
    wait_closed(&service).await;
    assert_eq!(service.owner_count(), 0);
}

#[tokio::test]
async fn service_abandoned_close_retains_owner_writer_and_capacity_through_late_callback() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("unused"),
    );
    let record = fixture.record("managed-late-close", &provider, true).await;
    let gate = Gate::new();
    gate.release.add_permits(1);
    let late = Gate::new();
    let callback_gate = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let late = callback_gate.clone();
        Box::pin(async move {
            late.entered.add_permits(1);
            late.release.acquire().await.unwrap().forget();
            None
        })
    });
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider.clone(), Some(gate), Some(hook)),
    )
    .await;
    let ticket = client.submit(prompt("managed-late")).await.unwrap();
    permits(&late.entered, 1).await;
    let closer = service.clone();
    let id = record.spec.id;
    let request = tokio::spawn(async move { closer.close_ordinary(id).await });
    wait_state(&fixture.catalog, id, AgentState::Closing).await;
    request.abort();
    let _ = request.await;
    assert_eq!(service.phase(id), Some(OwnerPhase::Closing));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
    let other = fixture
        .record("managed-no-capacity", &provider, false)
        .await;
    assert!(matches!(
        service
            .restore_ordinary(
                other.spec.id,
                fixture.catalog.guard(&other),
                OrdinaryLimits::default(),
                binder(provider, None, None)
            )
            .await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    assert_eq!(service.phase(other.spec.id), None);
    late.release.add_permits(1);
    ticket.wait().await.unwrap();
    wait_phase(&service, id, None).await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_shutdown_preserves_a_failed_close_and_explicit_retry_can_settle_it() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("managed-close-fault", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_closed BEFORE UPDATE ON agents WHEN OLD.resident = 1 AND NEW.resident = 0 BEGIN SELECT RAISE(ABORT, 'synthetic close failure'); END;").unwrap();
    assert!(matches!(
        service.shutdown().await,
        Err(ServiceError::ShutdownIncomplete { owners: 1 })
    ));
    assert!(!service.is_closed());
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(client.snapshot().await.unwrap().retiring);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    // Native cleanup succeeded before the final Closed publication failed.
    // Preserve the owner/catalog slot, but do not pretend its writer is live.
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
    // A failed shutdown must keep its singleton lock, not just metadata.
    assert!(matches!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await,
        Err(CatalogError::AlreadyRunning)
    ));
    db.execute_batch("DROP TRIGGER refuse_closed").unwrap();
    service.close_ordinary(record.spec.id).await.unwrap();
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_failed_start_rolls_back_and_allows_an_explicit_exact_retry() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("managed-start-failure", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    assert!(matches!(
        service
            .restore_ordinary(
                record.spec.id,
                fixture.catalog.guard(&record),
                OrdinaryLimits::default(),
                Box::new(|_, _, _| Err(BackendError::Binding("synthetic preflight rejection")))
            )
            .await,
        Err(ServiceError::Backend(BackendError::Binding(_)))
    ));
    assert_eq!(service.owner_count(), 0);
    let current = fixture.catalog.get(record.spec.id).await.unwrap();
    assert_eq!(current.state, AgentState::Closed);
    managed_restore(&service, &fixture, &current, binder(provider, None, None)).await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_failed_start_rollback_retains_a_reconciliation_fence() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("managed-start-rollback-failure", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_rollback BEFORE UPDATE ON agents WHEN OLD.resident = 1 AND NEW.resident = 0 BEGIN SELECT RAISE(ABORT, 'synthetic rollback failure'); END;").unwrap();
    assert!(matches!(
        service
            .restore_ordinary(
                record.spec.id,
                fixture.catalog.guard(&record),
                OrdinaryLimits::default(),
                Box::new(|_, _, _| Err(BackendError::Binding("synthetic preflight rejection")))
            )
            .await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::StartFailed));
    assert!(matches!(
        service.close_ordinary(record.spec.id).await,
        Err(ServiceError::Backend(BackendError::Faulted))
    ));
    assert!(matches!(
        service.shutdown().await,
        Err(ServiceError::ShutdownIncomplete { owners: 1 })
    ));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    db.execute_batch("DROP TRIGGER refuse_rollback").unwrap();
    // Test-owned explicit reconciliation: no backend/writer was created. The
    // coordinator itself must never invent this transition from an error.
    let current = fixture.catalog.get(record.spec.id).await.unwrap();
    fixture
        .catalog
        .transition(
            record.spec.id,
            fixture.catalog.guard(&current),
            AgentState::Closed,
        )
        .await
        .unwrap();
    service.close_ordinary(record.spec.id).await.unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_closing_transition_failure_settles_execution_but_keeps_native_writer() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("unused"),
    );
    let record = fixture
        .record("managed-closing-fault", &provider, true)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(gate.clone()), None),
    )
    .await;
    let ticket = client
        .submit(prompt("cancel-despite-closing-fault"))
        .await
        .unwrap();
    permits(&gate.entered, 1).await;
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_closing BEFORE UPDATE ON agents WHEN NEW.resident = 1 AND NEW.busy = 1 AND NEW.executing = 0 BEGIN SELECT RAISE(ABORT, 'synthetic closing failure'); END;").unwrap();
    assert!(matches!(
        service.close_ordinary(record.spec.id).await,
        Err(ServiceError::Backend(BackendError::Catalog(_)))
    ));
    ticket.wait().await.unwrap();
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert!(client.snapshot().await.unwrap().active.is_none());
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
    db.execute_batch("DROP TRIGGER refuse_closing").unwrap();
    service.close_ordinary(record.spec.id).await.unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_uncertain_persistence_never_becomes_a_successful_shutdown() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("must-not-be-saved"),
    );
    let record = fixture
        .record("managed-uncertain-writer", &provider, true)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    gate.release.add_permits(1);
    let lock = record
        .spec
        .session
        .as_ref()
        .unwrap()
        .path
        .with_extension("jsonl.lock");
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let lock = lock.clone();
        Box::pin(async move {
            tokio::fs::write(lock, b"0\n").await.unwrap();
            None
        })
    });
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(gate), Some(hook)),
    )
    .await;
    let report = client
        .submit(prompt("preserve-uncertainty"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(report.persistence_uncertain);
    assert!(report.outcome.is_err());
    for _ in 0..2 {
        assert!(matches!(
            service.close_ordinary(record.spec.id).await,
            Err(ServiceError::Backend(BackendError::Unsaved))
        ));
        assert!(matches!(
            service.shutdown().await,
            Err(ServiceError::ShutdownIncomplete { owners: 1 })
        ));
        assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert!(!service.is_closed());
        assert!(
            client
                .snapshot()
                .await
                .unwrap()
                .last_report
                .as_ref()
                .unwrap()
                .persistence_uncertain
        );
    }
    // Test-only owner destruction after verifying the failure twice. This is
    // not a production repair/unload API and must never be called successful
    // shutdown. Reopening classifies the journal/catalog as interrupted.
    drop(service);
    tokio::time::timeout(DEADLINE, async {
        while client.inner.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.catalog.shutdown().await.unwrap();
    let recovered = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(
        recovered.get(record.spec.id).await.unwrap().state,
        AgentState::Interrupted
    );
    recovered.shutdown().await.unwrap();
}

async fn daemon_fixture() -> (
    Fixture,
    crate::host::daemon::HostDaemon,
    crate::host::client::ConnectionConfig,
) {
    daemon_fixture_with_timeout(DEADLINE).await
}

async fn daemon_fixture_with_timeout(
    request_timeout: Duration,
) -> (
    Fixture,
    crate::host::daemon::HostDaemon,
    crate::host::client::ConnectionConfig,
) {
    daemon_fixture_with_config(HostConfig::default(), request_timeout).await
}

async fn daemon_fixture_with_config(
    host: HostConfig,
    request_timeout: Duration,
) -> (
    Fixture,
    crate::host::daemon::HostDaemon,
    crate::host::client::ConnectionConfig,
) {
    use crate::host::{
        client::ConnectionConfig,
        daemon::HostDaemon,
        protocol::{AuthToken, HostId},
        server::ControlConfig,
    };
    let mut fixture = Fixture::new(1).await;
    fixture.catalog.shutdown().await.unwrap();
    let connection = ConnectionConfig {
        socket_path: fixture.root.path().join("host/s"),
        host_id: HostId::new(),
        token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
        timeout: DEADLINE,
    };
    let daemon = HostDaemon::open(
        fixture.root.path().join("host"),
        host,
        ControlConfig {
            socket_path: connection.socket_path.clone(),
            host_id: connection.host_id,
            token: connection.token.clone(),
            max_connections: 4,
            handshake_timeout: DEADLINE,
            request_timeout,
        },
    )
    .await
    .unwrap();
    fixture.catalog = daemon.service().catalog().clone();
    (fixture, daemon, connection)
}

#[tokio::test]
async fn persistent_idle_unload_waits_for_native_settlement_and_preserves_restore_identity() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("saved-before-unload"),
    );
    let gate = Gate::new();
    let mut spec = fixture.spec("idle-unload", &provider, true).await;
    let idle_ms = std::num::NonZeroU64::new(500).unwrap();
    spec.timeouts.idle = crate::host::TimeoutOverride::After(idle_ms);
    let record = fixture.catalog.register(spec).await.unwrap();
    let id = record.spec.id;
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let path = record.spec.session.as_ref().unwrap().path.to_str().unwrap();
    let old_client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider.clone(), Some(gate.clone()), None),
    )
    .await;
    let ticket = old_client.submit(prompt("keep-my-session")).await.unwrap();
    permits(&gate.entered, 1).await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert_eq!(
        fixture.catalog.get(id).await.unwrap().state,
        AgentState::Running
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(env.try_acquire_writer_lease(path).await.is_err());

    gate.release.add_permits(1);
    complete(&ticket).await;
    let idle = fixture.catalog.get(id).await.unwrap();
    assert_eq!(idle.state, AgentState::Idle);
    let saved = bytes(&record);
    assert!(String::from_utf8_lossy(&saved).contains("saved-before-unload"));
    wait_phase(&service, id, None).await;
    let unloaded = fixture.catalog.get(id).await.unwrap();
    assert_eq!(unloaded.state, AgentState::Unloaded);
    assert_eq!(unloaded.spec, record.spec);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    assert!(env.try_acquire_writer_lease(path).await.unwrap().is_some());

    let result = service
        .restore_ordinary(
            id,
            fixture.catalog.guard(&unloaded),
            OrdinaryLimits::default(),
            Box::new(|_, _, _| Err(BackendError::Binding("test restore binding failure"))),
        )
        .await;
    assert!(matches!(
        result,
        Err(ServiceError::Backend(BackendError::Binding(_)))
    ));
    let retry = fixture.catalog.get(id).await.unwrap();
    assert_eq!(retry.state, AgentState::Unloaded);
    assert!(service.phase(id).is_none());
    let restored = managed_restore(
        &service,
        &fixture,
        &retry,
        binder(provider, Some(gate), None),
    )
    .await;
    assert!(old_client.snapshot().await.is_err());
    assert!(restored.snapshot().await.unwrap().active.is_none());
    assert_eq!(bytes(&record), saved);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_idle_unload_preserves_pending_next_run_input() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(FauxScript::new());
    let mut spec = fixture.spec("idle-next-run", &provider, false).await;
    spec.timeouts.idle =
        crate::host::TimeoutOverride::After(std::num::NonZeroU64::new(400).unwrap());
    let record = fixture.catalog.register(spec).await.unwrap();
    let client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    client
        .deliver(
            UserMessage::new("pending-next-run", 1).into(),
            Delivery::NextRun,
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Live));
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Idle
    );
    assert!(String::from_utf8_lossy(&bytes(&record)).contains("pending-next-run"));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn authenticated_session_peers_pin_only_their_owner_and_attach_again_after_restore() {
    use crate::host::{
        area_rpc::SessionHello,
        client::{encode_frame, read_frame, write_frame},
        protocol::{AuthToken, Hello, HostId, Version, MAX_FRAME_BYTES, PROTOCOL},
        server::{ControlConfig, ControlServer},
    };
    use tokio::{io::AsyncReadExt, net::UnixStream};

    let fixture = Fixture::new(2).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("before-reconnect"),
    );
    let gate = Gate::new();
    let delay = Duration::from_millis(400);
    let mut records = Vec::new();
    let mut clients = Vec::new();
    for name in ["first-peer", "another-agent"] {
        let mut spec = fixture.spec(name, &provider, true).await;
        spec.timeouts.disconnected = crate::host::TimeoutOverride::After(
            std::num::NonZeroU64::new(delay.as_millis() as u64).unwrap(),
        );
        let record = fixture.catalog.register(spec).await.unwrap();
        clients.push(
            managed_restore(
                &service,
                &fixture,
                &record,
                binder(provider.clone(), Some(gate.clone()), None),
            )
            .await,
        );
        records.push(record);
    }
    let first = &records[0];
    let second = &records[1];
    let first_id = first.spec.id;
    let second_id = second.spec.id;
    let config = ControlConfig {
        socket_path: fixture.root.path().join("host/route.sock"),
        host_id: HostId::new(),
        token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
        max_connections: 8,
        handshake_timeout: DEADLINE,
        request_timeout: DEADLINE,
    };
    let mut server = ControlServer::bind_with_controls(
        config.clone(),
        service.clone(),
        None,
        None,
        None,
        crate::host::area_rpc::SessionRoutes {
            agents: [first_id, second_id].into(),
            restore_on_attach: false,
        },
        None,
    )
    .await
    .unwrap();
    let connect = |id: AgentId| {
        let config = config.clone();
        async move {
            let mut stream = UnixStream::connect(&config.socket_path).await.unwrap();
            let hello = SessionHello {
                session_rpc: Hello {
                    protocol: PROTOCOL.into(),
                    version: Version::default(),
                    host_id: config.host_id,
                    token: config.token,
                },
                agent_id: id,
            };
            write_frame(&mut stream, &encode_frame(&hello, MAX_FRAME_BYTES).unwrap())
                .await
                .unwrap();
            tokio::time::timeout(DEADLINE, async {
                let ready: serde_json::Value =
                    read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
                assert_eq!(ready["type"], "ready");
                assert_eq!(ready["agentId"], id.to_string());
                (stream, ready)
            })
            .await
            .unwrap()
        }
    };
    async fn receive(stream: &mut UnixStream) -> serde_json::Value {
        tokio::time::timeout(DEADLINE, read_frame(stream, MAX_FRAME_BYTES))
            .await
            .unwrap()
            .unwrap()
    }

    let (mut first_peer, initial) = connect(first_id).await;
    let (mut last_peer, _) = connect(first_id).await;
    let (mut second_peer, second_ready) = connect(second_id).await;
    let send = |command| encode_frame(&command, MAX_FRAME_BYTES).unwrap();
    assert!(initial["supportedCommands"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("set_thinking_level")));
    let original_thinking = initial["state"]["thinkingLevel"].clone();
    let change = serde_json::json!({"type":"set_thinking_level", "id":"change", "level":"high"});
    write_frame(&mut first_peer, &send(change)).await.unwrap();
    let response = receive(&mut first_peer).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["thinkingLevel"], "high");
    let changed = receive(&mut last_peer).await;
    assert_eq!(
        changed,
        serde_json::json!({"type":"thinking_level_changed", "thinkingLevel":"high"})
    );
    write_frame(
        &mut second_peer,
        &send(serde_json::json!({"type":"get_state", "id":"inspect"})),
    )
    .await
    .unwrap();
    let other = receive(&mut second_peer).await;
    assert_eq!(other["id"], "inspect");
    assert_eq!(
        other["data"]["thinkingLevel"],
        second_ready["state"]["thinkingLevel"]
    );
    drop(first_peer);
    tokio::time::sleep(2 * delay).await;
    assert_eq!(service.owner_count(), 2);

    let ticket = clients[0]
        .submit(prompt("before-last-peer-left"))
        .await
        .unwrap();
    permits(&gate.entered, 1).await;
    write_frame(
        &mut last_peer,
        &send(serde_json::json!({"type":"set_thinking_level", "id":"busy", "level":"low"})),
    )
    .await
    .unwrap();
    let rejected = tokio::time::timeout(DEADLINE, async {
        loop {
            let frame: serde_json::Value =
                read_frame(&mut last_peer, MAX_FRAME_BYTES).await.unwrap();
            if frame["id"] == "busy" {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(rejected["success"], false);
    assert_eq!(rejected["error"], BackendError::Busy.to_string());
    drop(last_peer);
    tokio::time::sleep(2 * delay).await;
    assert_eq!(
        fixture.catalog.get(first_id).await.unwrap().state,
        AgentState::Running
    );
    gate.release.add_permits(1);
    complete(&ticket).await;
    wait_phase(&service, first_id, None).await;
    let unloaded = fixture.catalog.get(first_id).await.unwrap();
    assert_eq!(unloaded.state, AgentState::Unloaded);
    let saved = bytes(first);
    assert!(String::from_utf8_lossy(&saved).contains("before-reconnect"));
    assert_eq!(service.phase(second_id), Some(OwnerPhase::Live));

    managed_restore(
        &service,
        &fixture,
        &unloaded,
        binder(provider, Some(gate), None),
    )
    .await;
    let (_new_peer, ready) = connect(first_id).await;
    assert_eq!(ready["sessionId"], first.spec.session.as_ref().unwrap().id);
    assert_eq!(ready["state"]["thinkingLevel"], original_thinking);
    assert_eq!(bytes(first), saved);
    assert!(clients[0].snapshot().await.is_err());

    service.close_ordinary(second_id).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(DEADLINE, second_peer.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(service.phase(first_id), Some(OwnerPhase::Live));
    server.stop().await.unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_host_idle_deadline_starts_after_last_owner_and_peer_leave() {
    use crate::host::{client::HostClient, protocol::Command};
    let idle = Duration::from_millis(180);
    let (fixture, mut daemon, connection) = daemon_fixture_with_config(
        HostConfig {
            host_idle_timeout_ms: std::num::NonZeroU64::new(idle.as_millis() as u64),
            ..Default::default()
        },
        DEADLINE,
    )
    .await;
    let mut client = HostClient::connect(connection.clone()).await.unwrap();
    let provider = FauxProvider::new(FauxScript::new().with_text("idle-resident"));
    let record = fixture
        .record("daemon-idle-resident", &provider, false)
        .await;
    managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, None, None),
    )
    .await;
    drop(client);
    assert!(tokio::time::timeout(2 * idle, daemon.wait_control())
        .await
        .is_err());
    assert_eq!(daemon.service().owner_count(), 1);
    client = HostClient::connect(connection.clone()).await.unwrap();
    daemon
        .service()
        .close_ordinary(record.spec.id)
        .await
        .unwrap();
    assert!(tokio::time::timeout(2 * idle, daemon.wait_control())
        .await
        .is_err());
    assert_eq!(
        client.call(Command::Status {}).await.unwrap()["resident"],
        0
    );
    drop(client);
    let disconnected = tokio::time::Instant::now();
    tokio::time::timeout(DEADLINE, daemon.wait_control())
        .await
        .unwrap()
        .unwrap();
    assert!(disconnected.elapsed() >= idle);
    assert!(daemon.control_stopped_for_idleness());
    assert!(daemon.service().is_closed());
    assert!(!connection.socket_path.exists());
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_idle_deadline_cannot_preempt_an_owned_lifecycle_reservation() {
    let idle = Duration::from_millis(120);
    let (_fixture, mut daemon, _) = daemon_fixture_with_config(
        HostConfig {
            host_idle_timeout_ms: std::num::NonZeroU64::new(idle.as_millis() as u64),
            ..Default::default()
        },
        DEADLINE,
    )
    .await;
    let service = daemon.service().clone();
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let reservation = tokio::spawn(async move {
        service
            .with_administration(move |reservation| {
                Box::pin(async move {
                    entered.send(()).unwrap();
                    released.await.unwrap();
                    drop(reservation);
                    Ok(())
                })
            })
            .await
            .unwrap();
    });
    held.await.unwrap();
    assert!(tokio::time::timeout(2 * idle, daemon.wait_control())
        .await
        .is_err());
    assert!(!daemon.service().is_stopping());
    release.send(()).unwrap();
    reservation.await.unwrap();
    tokio::time::timeout(DEADLINE, daemon.wait_control())
        .await
        .unwrap()
        .unwrap();
    assert!(daemon.control_stopped_for_idleness());
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_ipc_detach_and_listener_wait_cancellation_keep_a_real_tool_running() {
    use crate::host::{client::HostClient, protocol::Command};
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("after-ipc-detach"),
    );
    let record = fixture.record("daemon-detach", &provider, true).await;
    let gate = Gate::new();
    let native = managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, Some(gate.clone()), None),
    )
    .await;
    let ticket = native
        .submit(prompt("real-tool-before-ipc-detach"))
        .await
        .unwrap();
    drop(native);
    permits(&gate.entered, 1).await;
    let mut client = HostClient::connect(connection.clone()).await.unwrap();
    assert_eq!(
        client.call(Command::Status {}).await.unwrap()["executing"],
        1
    );
    client.detach();
    drop(client);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), daemon.wait_control())
            .await
            .is_err()
    );
    assert_eq!(daemon.service().owner_count(), 1);
    assert!(daemon
        .service()
        .ordinary_client(record.spec.id)
        .unwrap()
        .snapshot()
        .await
        .unwrap()
        .active
        .is_some());
    gate.release.add_permits(1);
    complete(&ticket).await;
    assert!(String::from_utf8(bytes(&record))
        .unwrap()
        .contains("after-ipc-detach"));
    daemon.shutdown().await.unwrap();
    assert!(!connection.socket_path.exists());
}

#[tokio::test]
async fn daemon_failed_shutdown_retains_query_access_owner_and_singleton_until_retry() {
    use crate::host::{client::HostClient, protocol::Command};
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("daemon-close-failure", &provider, false)
        .await;
    managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, None, None),
    )
    .await;
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_closed BEFORE UPDATE ON agents WHEN OLD.resident = 1 AND NEW.resident = 0 BEGIN SELECT RAISE(ABORT, 'synthetic close failure'); END;").unwrap();
    assert!(daemon.shutdown().await.is_err());
    assert!(!daemon.service().is_closed());
    assert_eq!(
        daemon.service().phase(record.spec.id),
        Some(OwnerPhase::CloseFailed)
    );
    let mut client = HostClient::connect(connection.clone()).await.unwrap();
    let status = client.call(Command::Status {}).await.unwrap();
    assert_eq!(status["owners"], 1);
    assert_eq!(status["resident"], 1);
    assert_eq!(status["stopping"], true);
    assert!(matches!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await,
        Err(CatalogError::AlreadyRunning)
    ));
    db.execute_batch("DROP TRIGGER refuse_closed").unwrap();
    daemon.shutdown().await.unwrap();
    assert_eq!(daemon.service().owner_count(), 0);
    assert!(!connection.socket_path.exists());
}

#[tokio::test]
async fn daemon_shutdown_grace_and_cancelled_wait_retain_late_callback_until_cleanup() {
    let grace = 30;
    let host = HostConfig {
        default_timeouts: crate::host::Timeouts {
            shutdown_grace_ms: std::num::NonZeroU64::new(grace),
            ..Default::default()
        },
        ..Default::default()
    };
    let (fixture, daemon, connection) = daemon_fixture_with_config(host, DEADLINE).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("unused"),
    );
    let record = fixture
        .record("daemon-late-shutdown", &provider, true)
        .await;
    let tool_gate = Gate::new();
    tool_gate.release.add_permits(1);
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
    let client = managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, Some(tool_gate), Some(hook)),
    )
    .await;
    let ticket = client.submit(prompt("late-callback")).await.unwrap();
    permits(&late.entered, 1).await;
    let daemon = Arc::new(tokio::sync::Mutex::new(daemon));
    let shutdown_daemon = daemon.clone();
    let request = tokio::spawn(async move { shutdown_daemon.lock().await.shutdown().await });
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    tokio::time::sleep(Duration::from_millis(2 * grace)).await;
    assert!(!request.is_finished());
    assert!(ticket.completion.borrow().is_none());
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let path = record.spec.session.as_ref().unwrap().path.to_str().unwrap();
    assert!(env.try_acquire_writer_lease(path).await.is_err());
    request.abort();
    let _ = request.await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let mut ipc = crate::host::client::HostClient::connect(connection.clone())
        .await
        .unwrap();
    assert_eq!(
        ipc.call(crate::host::protocol::Command::Status {})
            .await
            .unwrap()["owners"],
        1
    );
    late.release.add_permits(1);
    let _ = ticket.wait().await.unwrap();
    daemon.lock().await.shutdown().await.unwrap();
    assert!(env.try_acquire_writer_lease(path).await.unwrap().is_some());
    assert!(!connection.socket_path.exists());
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_prompt_receipt_precedes_settlement_and_never_replays() {
    use rpi_harness::agent_harness::HostAdmissionExecution;
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("admitted-answer"),
    );
    let record = fixture.record("admitted-one", &provider, true).await;
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(gate.clone()), None),
    )
    .await;
    let request_id = uuid::Uuid::now_v7().to_string();
    let messages = vec![UserMessage::new("durable input", 1).into()];
    let ticket = client
        .submit_admitted("host-1".into(), request_id.clone(), messages.clone())
        .await
        .unwrap();
    let receipt = tokio::time::timeout(DEADLINE, ticket.wait_admitted())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.request_id, request_id);
    assert_eq!(receipt.agent_id, record.spec.id.to_string());
    assert_eq!(receipt.entry_id, request_id);
    assert_eq!(receipt.entry_seq, 1);
    assert_eq!(receipt.history.revision, 1);
    assert_eq!(receipt.history.generation, 0);
    // The receipt is already durable while execution is still at the tool gate.
    permits(&gate.entered, 1).await;
    let status = client.admission(&request_id).await.unwrap().unwrap();
    assert!(matches!(
        status.execution,
        HostAdmissionExecution::Started { ref run_id, finished: false } if !run_id.is_empty()
    ));
    gate.release.add_permits(1);
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(report.outcome, Ok(OrdinaryOutput::Run(_))));
    wait_state(&fixture.catalog, record.spec.id, AgentState::Idle).await;
    let status = client.admission(&request_id).await.unwrap().unwrap();
    assert!(matches!(
        status.execution,
        HostAdmissionExecution::Started { finished: true, .. }
    ));
    let history = status.receipt.history;
    let listed = client.admissions().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], status);
    assert_eq!(
        service
            .admission(record.spec.id, &request_id)
            .await
            .unwrap(),
        Some(status.clone())
    );
    assert_eq!(service.admissions(record.spec.id).await.unwrap(), listed);

    // Retrying the identical request id must not execute or append again.
    let retry = client
        .submit_admitted("host-1".into(), request_id.clone(), messages)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(DEADLINE, retry.wait_admitted())
            .await
            .unwrap()
            .unwrap()
            .entry_seq,
        receipt.entry_seq
    );
    let replay = tokio::time::timeout(DEADLINE, retry.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        replay.outcome,
        Ok(OrdinaryOutput::AlreadyAdmitted)
    ));
    assert_eq!(
        client.snapshot().await.unwrap().record.state,
        AgentState::Idle
    );
    let status_again = client.admission(&request_id).await.unwrap().unwrap();
    assert_eq!(status_again.receipt.history, history);
    assert!(matches!(
        status_again.execution,
        HostAdmissionExecution::Started { finished: true, .. }
    ));
    assert_eq!(client.admissions().await.unwrap().len(), 1);
    // `HostService::shutdown` already closes the catalog.
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn admission_rejection_is_definite_and_leaves_the_agent_idle() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(FauxScript::new().with_text("never"));
    let record = fixture.record("admitted-reject", &provider, false).await;
    let client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    let messages = vec![UserMessage::new("rejected", 1).into()];
    for request_id in ["not-a-uuid".to_string(), uuid::Uuid::nil().to_string()] {
        let error = client
            .submit_admitted("host-1".into(), request_id, messages.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, BackendError::Admission(_)), "{error}");
    }
    assert!(client.admissions().await.unwrap().is_empty());
    let snapshot = client.snapshot().await.unwrap();
    assert_eq!(snapshot.record.state, AgentState::Idle);
    assert!(snapshot.active.is_none());
    // `HostService::shutdown` already closes the catalog.
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_ipc_receipt_reads_the_real_native_admission_without_mutating() {
    use crate::host::{client::HostClient, protocol::Command};
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(FauxScript::new().with_text("receipt-answer"));
    let record = fixture.record("daemon-receipt", &provider, false).await;
    let client = managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, None, None),
    )
    .await;
    let request_id = uuid::Uuid::now_v7().to_string();
    let messages = vec![UserMessage::new("ipc admitted input", 1).into()];
    let ticket = client
        .submit_admitted("host-1".into(), request_id.clone(), messages.clone())
        .await
        .unwrap();
    let receipt = tokio::time::timeout(DEADLINE, ticket.wait_admitted())
        .await
        .unwrap()
        .unwrap();
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(report.outcome, Ok(OrdinaryOutput::Run(_))));

    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id: crate::host::protocol::AgentId = record.spec.id.to_string().try_into().unwrap();
    let ipc_request: crate::host::protocol::RequestId = request_id.clone().try_into().unwrap();
    let data = ipc
        .call(Command::Receipt {
            agent_id,
            request_id: ipc_request,
        })
        .await
        .unwrap();
    assert_eq!(data["found"], true);
    assert_eq!(data["agent_id"], record.spec.id.to_string());
    assert_eq!(data["receipt"]["entryId"], receipt.entry_id);
    assert_eq!(data["receipt"]["entrySeq"], receipt.entry_seq);
    assert_eq!(
        data["receipt"]["history"]["revision"],
        receipt.history.revision
    );
    assert_eq!(
        data["receipt"]["history"]["generation"],
        receipt.history.generation
    );
    assert_eq!(data["execution"]["state"], "started");
    assert_eq!(data["execution"]["finished"], true);

    // Unknown request: honest negative, not an error and no new admission.
    let unknown = ipc
        .call(Command::Receipt {
            agent_id,
            request_id: uuid::Uuid::now_v7().to_string().try_into().unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(unknown["found"], false);
    assert_eq!(client.admissions().await.unwrap().len(), 1);
    // Unknown Agent never loads a session or creates a receipt.
    let missing = ipc
        .call(Command::Receipt {
            agent_id: uuid::Uuid::new_v4().to_string().try_into().unwrap(),
            request_id: uuid::Uuid::now_v7().to_string().try_into().unwrap(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        crate::host::client::ClientError::Rejected(crate::host::protocol::ErrorCode::NotFound)
    );
    drop(ipc);
    drop(client);
    daemon.shutdown().await.unwrap();
    assert!(!connection.socket_path.exists());
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_ipc_submit_admits_once_and_reports_a_native_receipt() {
    use crate::host::{client::ClientError, client::HostClient, protocol::Command};
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(FauxScript::new().with_text("ipc-submit-answer"));
    let record = fixture.record("daemon-submit", &provider, false).await;
    let _client = managed_restore(
        daemon.service(),
        &fixture,
        &record,
        binder(provider, None, None),
    )
    .await;
    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id: crate::host::protocol::AgentId = record.spec.id.to_string().try_into().unwrap();
    let request_id = crate::host::protocol::RequestId::new();

    // Unknown Agent never loads a session or admits input.
    let missing = ipc
        .call(Command::Submit {
            agent_id: uuid::Uuid::new_v4().to_string().try_into().unwrap(),
            request_id: crate::host::protocol::RequestId::new(),
            prompt: "no such agent".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        ClientError::Rejected(crate::host::protocol::ErrorCode::NotFound)
    );

    let data = ipc
        .call(Command::Submit {
            agent_id,
            request_id,
            prompt: "ipc durable prompt".into(),
        })
        .await
        .unwrap();
    assert_eq!(data["durable"], true);
    assert_eq!(data["agent_id"], record.spec.id.to_string());
    assert_eq!(data["receipt"]["requestId"], request_id.to_string());
    assert_eq!(data["receipt"]["entrySeq"], 1);
    assert_eq!(data["receipt"]["history"]["revision"], 1);

    // Duplicate request id: same receipt, no second admission or execution.
    let again = ipc
        .call(Command::Submit {
            agent_id,
            request_id,
            prompt: "ipc durable prompt".into(),
        })
        .await
        .unwrap();
    assert_eq!(again["receipt"]["entrySeq"], 1);
    assert_eq!(
        daemon
            .service()
            .admissions(record.spec.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Wait for the detached execution to settle through the receipt query.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let status = ipc
            .call(Command::Receipt {
                agent_id,
                request_id,
            })
            .await
            .unwrap();
        if status["execution"]["state"] == "started" && status["execution"]["finished"] == true {
            assert_eq!(status["receipt"]["entrySeq"], 1);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "submitted execution did not settle: {status}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(String::from_utf8(bytes(&record))
        .unwrap()
        .contains("ipc-submit-answer"));
    drop(ipc);
    daemon.shutdown().await.unwrap();
    assert!(!connection.socket_path.exists());
}
