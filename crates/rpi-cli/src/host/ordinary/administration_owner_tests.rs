// Included beside service guarded tests to reuse real Harness/JSONL gates.
use crate::host::administration::{
    AdministrativeCoordinator, AdministrativeError, AdministrativeGrant, AdministrativePermissions,
};

async fn administrative_idle(coordinator: &AdministrativeCoordinator) {
    tokio::time::timeout(DEADLINE, async {
        while coordinator.active_jobs() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn administrative_request(
    grant: &AdministrativeGrant,
    catalog: &HostCatalog,
    action: LifecycleAction,
) -> LifecycleRequest {
    LifecycleRequest {
        scope: grant.scope(),
        request_id: RequestId::new(),
        epoch: catalog.epoch(),
        action,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn administrative_owned_close_dedupes_abandoned_waiters_and_bounds_distinct_jobs() {
    let fixture = Fixture::new(2).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("administrative-late-close", &provider, true)
        .await;
    let other = fixture
        .record("administrative-capacity", &provider, false)
        .await;
    let late = Gate::new();
    let callback = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = callback.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            permits(&gate.release, 1).await;
            None
        })
    });
    let tool = Gate::new();
    tool.release.add_permits(1);
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(tool), Some(hook)),
    )
    .await;
    let ticket = client
        .submit(prompt("administrative-owned-close"))
        .await
        .unwrap();
    permits(&late.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let grant = Arc::new(
        AdministrativeGrant::new(
            ControlScopeId::new(),
            [record.spec.id, other.spec.id].into(),
            AdministrativePermissions {
                close: true,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let coordinator = AdministrativeCoordinator::new(
        service.clone(),
        crate::host::TrustedProfileSet::freeze(vec![])
            .await
            .unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    let request = administrative_request(
        &grant,
        &fixture.catalog,
        LifecycleAction::Close {
            agent_id: record.spec.id,
            revision: running.record.revision,
        },
    );
    let owner = coordinator.clone();
    let authority = grant.clone();
    let command = request.clone();
    let waiter = tokio::spawn(async move { owner.execute(&authority, command).await });
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(coordinator.active_jobs(), 1);
    managed_writer_held(&record).await;
    let owner = coordinator.clone();
    let authority = grant.clone();
    let command = request.clone();
    let duplicate = tokio::spawn(async move { owner.execute(&authority, command).await });
    let mut changed = request.clone();
    changed.action = LifecycleAction::Close {
        agent_id: record.spec.id,
        revision: running.record.revision + 1,
    };
    assert_eq!(
        coordinator.execute(&grant, changed).await,
        Err(AdministrativeError::Conflict)
    );
    let other_request = administrative_request(
        &grant,
        &fixture.catalog,
        LifecycleAction::Close {
            agent_id: other.spec.id,
            revision: other.revision,
        },
    );
    assert_eq!(
        coordinator.execute(&grant, other_request.clone()).await,
        Err(AdministrativeError::Capacity)
    );
    assert!(coordinator
        .receipt(&grant, &other_request)
        .await
        .unwrap()
        .is_none());
    assert!(!duplicate.is_finished());
    assert!(ticket.completion.borrow().is_none());
    late.release.add_permits(1);
    let receipt = tokio::time::timeout(DEADLINE, duplicate)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::Closed { .. }
        }
    ));
    assert_eq!(
        coordinator
            .receipt(&grant, &request)
            .await
            .unwrap()
            .unwrap(),
        receipt
    );
    administrative_idle(&coordinator).await;
    let rejected = coordinator.execute(&grant, other_request).await.unwrap();
    assert_eq!(
        rejected.status,
        LifecycleState::Rejected {
            code: LifecycleFailure::NotFound
        }
    );
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn administrative_cancel_receipt_is_signal_only_and_cannot_cancel_a_later_ticket() {
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("administrative-signal", &provider, true)
        .await;
    let late = Gate::new();
    let callback = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = callback.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            permits(&gate.release, 1).await;
            None
        })
    });
    let tool = Gate::new();
    tool.release.add_permits(1);
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider.clone(), Some(tool.clone()), Some(hook)),
    )
    .await;
    let ticket = client
        .submit(prompt("administrative-signal"))
        .await
        .unwrap();
    permits(&tool.entered, 1).await;
    permits(&late.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let grant = AdministrativeGrant::new(
        ControlScopeId::new(),
        [record.spec.id].into(),
        AdministrativePermissions {
            cancel: true,
            ..Default::default()
        },
    )
    .unwrap();
    let coordinator = AdministrativeCoordinator::new(
        service.clone(),
        crate::host::TrustedProfileSet::freeze(vec![])
            .await
            .unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    let wrong = administrative_request(
        &grant,
        &fixture.catalog,
        LifecycleAction::Cancel {
            agent_id: record.spec.id,
            revision: running.record.revision,
            ticket_id: RunId::new(),
        },
    );
    assert_eq!(
        coordinator.execute(&grant, wrong).await.unwrap().status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: false }
        }
    );
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    administrative_idle(&coordinator).await;
    let request = administrative_request(
        &grant,
        &fixture.catalog,
        LifecycleAction::Cancel {
            agent_id: record.spec.id,
            revision: running.record.revision,
            ticket_id: ticket.id,
        },
    );
    let receipt = coordinator.execute(&grant, request.clone()).await.unwrap();
    assert_eq!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: true }
        }
    );
    assert!(ticket.completion.borrow().is_none());
    managed_writer_held(&record).await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    late.release.add_permits(1);
    tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    provider.script().set_responses(vec![
        FauxStep::tool_call("gate", serde_json::json!({})),
        FauxStep::text("later-finished"),
    ]);
    let later = client.submit(prompt("administrative-later")).await.unwrap();
    permits(&tool.entered, 1).await;
    assert_eq!(coordinator.execute(&grant, request).await.unwrap(), receipt);
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert!(later.completion.borrow().is_none());
    tool.release.add_permits(1);
    permits(&late.entered, 1).await;
    late.release.add_permits(1);
    complete(&later).await;
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn administrative_stop_failed_settlement_publishes_uncertain_and_keeps_retryable_owner() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("administrative-stop-failure", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let _client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    let coordinator = AdministrativeCoordinator::new(
        service.clone(),
        crate::host::TrustedProfileSet::freeze(vec![])
            .await
            .unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    let grant = AdministrativeGrant::new(
        ControlScopeId::new(),
        Default::default(),
        AdministrativePermissions {
            stop: true,
            ..Default::default()
        },
    )
    .unwrap();
    let request = administrative_request(&grant, &fixture.catalog, LifecycleAction::Stop {});
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_administrative_closed BEFORE UPDATE ON agents WHEN NEW.resident=0 BEGIN SELECT RAISE(ABORT,'injected final Closed failure'); END;").unwrap();
    let receipt = coordinator.execute(&grant, request.clone()).await.unwrap();
    assert!(matches!(receipt.status, LifecycleState::Uncertain { .. }));
    assert!(service.is_stopping());
    assert!(!service.is_closed());
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    // Native cleanup completed before final metadata failed; retained custody
    // must not be misreported as a still-held physical writer.
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let acquired = env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap();
    assert!(acquired.is_some());
    drop(acquired);
    administrative_idle(&coordinator).await;
    assert_eq!(coordinator.execute(&grant, request).await.unwrap(), receipt);
    db.execute_batch("DROP TRIGGER reject_administrative_closed")
        .unwrap();
    drop(db);
    service.shutdown().await.unwrap();
    assert_eq!(service.owner_count(), 0);
    assert!(service.is_closed());
}

#[tokio::test(flavor = "current_thread")]
async fn administrative_stop_ipc_timeout_keeps_late_callback_writer_and_owner_until_actual_settlement(
) {
    use crate::host::{
        client::{ClientError, ConnectionConfig, HostClient},
        daemon::DaemonAdministrationConfig,
        protocol as wire,
        server::{ControlConfig, ControlServer},
    };
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(1).await;
    std::fs::set_permissions(fixture.root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let provider = managed_gate_provider();
    let record = fixture.record("ipc-stop-late", &provider, true).await;
    let late = Gate::new();
    let callback = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = callback.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            permits(&gate.release, 1).await;
            None
        })
    });
    let tool = Gate::new();
    tool.release.add_permits(1);
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider, Some(tool), Some(hook)),
    )
    .await;
    let ticket = client.submit(prompt("ipc-stop-late")).await.unwrap();
    permits(&late.entered, 1).await;
    let scope = ControlScopeId::new();
    let admin = DaemonAdministrationConfig {
        scope,
        allowed_agents: Default::default(),
        profiles: vec![],
        max_jobs: NonZeroUsize::new(1).unwrap(),
        permissions: AdministrativePermissions {
            stop: true,
            ..Default::default()
        },
    }
    .prepare(vec![])
    .await
    .unwrap()
    .assemble(service.clone())
    .unwrap();
    let config = ControlConfig {
        socket_path: fixture.root.path().join("s"),
        host_id: wire::HostId::new(),
        token: wire::AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
        max_connections: 4,
        handshake_timeout: DEADLINE,
        request_timeout: std::time::Duration::from_millis(200),
    };
    let mut server = ControlServer::bind_with_administration(
        config.clone(),
        service.clone(),
        Some(admin.clone()),
    )
    .await
    .unwrap();
    let mut peer = HostClient::connect(ConnectionConfig {
        socket_path: config.socket_path.clone(),
        host_id: config.host_id,
        token: config.token.clone(),
        timeout: DEADLINE,
    })
    .await
    .unwrap();
    let request = LifecycleRequest {
        scope,
        request_id: RequestId::new(),
        epoch: fixture.catalog.epoch(),
        action: LifecycleAction::Stop {},
    };
    let command = wire::Command::AdministrativeStop {
        request: serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap(),
    };
    assert!(
        matches!(peer.call(command).await.unwrap_err(), ClientError::OutcomeUnknown { request_id, .. }
        if request_id.to_string() == request.request_id.to_string())
    );
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    assert!(service.is_stopping());
    assert!(!service.is_closed());
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert_eq!(admin.active_jobs(), 1);
    assert_eq!(
        admin.receipt(&request).await.unwrap().unwrap().status,
        LifecycleState::Prepared {}
    );
    managed_writer_held(&record).await;
    assert!(ticket.completion.borrow().is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), server.wait())
            .await
            .is_err()
    );
    assert!(config.socket_path.exists());
    late.release.add_permits(1);
    tokio::time::timeout(DEADLINE, server.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(server.stopped_by_administration());
    assert!(service.is_closed());
    assert_eq!(service.owner_count(), 0);
    assert!(!config.socket_path.exists());
    assert_eq!(
        admin.receipt(&request).await.unwrap().unwrap().status,
        LifecycleState::Succeeded {
            result: LifecycleResult::NativeSettled {}
        }
    );
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let lease = env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap();
    assert!(lease.is_some());
    drop(lease);
    service.shutdown().await.unwrap();
}
