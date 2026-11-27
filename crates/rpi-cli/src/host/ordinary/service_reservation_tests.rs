// Included beside service_guarded_tests.rs. These are trusted native callback
// tests, not the profile-authorizing administrative coordinator or IPC dispatch.
use crate::host::{
    lifecycle::{
        LifecycleAction, LifecycleFailure, LifecycleRequest, LifecycleResult, LifecycleState,
    },
    CatalogError, ControlScopeId, HostEpoch, ProfileRef, ProfileVersion, RequestId,
};

fn reserved_request(catalog: &HostCatalog, action: LifecycleAction) -> LifecycleRequest {
    LifecycleRequest {
        scope: ControlScopeId::new(),
        request_id: RequestId::new(),
        epoch: catalog.epoch(),
        action,
    }
}
fn reserved_catalog_error(error: CatalogError) -> ServiceError {
    ServiceError::Catalog(Arc::new(error))
}
async fn reserved_wait_stopping(service: &HostService) {
    tokio::time::timeout(DEADLINE, async {
        while !service.is_stopping() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_native_methods_and_journal_finish_do_not_reenter_a_queued_shutdown_writer() {
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("reserved-native-transaction", &provider, true)
        .await;
    let id = record.spec.id;
    let restore_guard = fixture.catalog.guard(&record);
    let restore = reserved_request(
        &fixture.catalog,
        LifecycleAction::RestoreProfile {
            profile: ProfileRef {
                agent_id: id,
                version: ProfileVersion::new(),
            },
            revision: restore_guard.revision,
        },
    );
    let tool = Gate::new();
    let binding = OrdinaryBinding::legacy(binder(provider, Some(tool.clone()), None));
    let before = Gate::new();
    let publish = Gate::new();
    let entered = before.clone();
    let publication = publish.clone();
    let service = HostService::new(fixture.catalog.clone());
    let worker = service.clone();
    let waiter = tokio::spawn(async move {
        worker
            .with_administration(move |reserved| {
                Box::pin(async move {
                    let catalog = reserved.catalog();
                    assert!(
                        catalog
                            .prepare_lifecycle(restore.clone())
                            .await
                            .map_err(reserved_catalog_error)?
                            .fresh
                    );
                    entered.entered.add_permits(1);
                    permits(&entered.release, 1).await;
                    // Shutdown is now queued. Calling any public lifecycle method here
                    // would recursively read a fair RwLock behind that writer.
                    let client = reserved
                        .restore_bound(id, restore_guard, OrdinaryLimits::default(), binding)
                        .await?;
                    let loaded = client.snapshot().await.unwrap().record;
                    catalog
                        .finish_lifecycle(
                            restore.clone(),
                            LifecycleState::Succeeded {
                                result: LifecycleResult::Loaded {
                                    revision: loaded.revision,
                                },
                            },
                        )
                        .await
                        .map_err(reserved_catalog_error)?;
                    let ticket = client.submit(prompt("reserved-cancel")).await.unwrap();
                    permits(&tool.entered, 1).await;
                    let running = client.snapshot().await.unwrap().record;
                    let guard = catalog.guard(&running);
                    let cancel = reserved_request(
                        catalog,
                        LifecycleAction::Cancel {
                            agent_id: id,
                            revision: guard.revision,
                            ticket_id: ticket.id,
                        },
                    );
                    assert!(
                        catalog
                            .prepare_lifecycle(cancel.clone())
                            .await
                            .map_err(reserved_catalog_error)?
                            .fresh
                    );
                    let signalled = reserved.cancel_guarded(id, guard, ticket.id).await?;
                    assert!(signalled);
                    catalog
                        .finish_lifecycle(
                            cancel.clone(),
                            LifecycleState::Succeeded {
                                result: LifecycleResult::CancelSignalled { signalled },
                            },
                        )
                        .await
                        .map_err(reserved_catalog_error)?;
                    let report = tokio::time::timeout(DEADLINE, ticket.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    assert!(report.catalog_error.is_none(), "{report:?}");
                    let idle = client.snapshot().await.unwrap().record;
                    let close = reserved_request(
                        catalog,
                        LifecycleAction::Close {
                            agent_id: id,
                            revision: idle.revision,
                        },
                    );
                    assert!(
                        catalog
                            .prepare_lifecycle(close.clone())
                            .await
                            .map_err(reserved_catalog_error)?
                            .fresh
                    );
                    reserved.close_guarded(id, catalog.guard(&idle)).await?;
                    let closed = catalog.get(id).await.map_err(reserved_catalog_error)?;
                    assert_eq!(closed.state, AgentState::Closed);
                    publication.entered.add_permits(1);
                    permits(&publication.release, 1).await;
                    catalog
                        .finish_lifecycle(
                            close.clone(),
                            LifecycleState::Succeeded {
                                result: LifecycleResult::Closed {
                                    revision: closed.revision,
                                },
                            },
                        )
                        .await
                        .map_err(reserved_catalog_error)?;
                    Ok(vec![restore, cancel, close])
                })
            })
            .await
    });
    permits(&before.entered, 1).await;
    let draining = service.clone();
    let shutdown = tokio::spawn(async move { draining.shutdown().await });
    // On this current-thread runtime, observing stopping means the shutdown
    // task polled through its store and enqueued its writer before yielding.
    reserved_wait_stopping(&service).await;
    assert!(!shutdown.is_finished());
    before.release.add_permits(1);
    permits(&publish.entered, 1).await;
    assert_eq!(service.owner_count(), 0);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    assert!(!service.is_closed());
    assert!(!shutdown.is_finished());
    publish.release.add_permits(1);
    let requests = tokio::time::timeout(DEADLINE, waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(DEADLINE, shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    for request in requests {
        let admission = reopened.prepare_lifecycle(request).await.unwrap();
        assert!(!admission.fresh);
        assert!(matches!(
            admission.receipt.status,
            LifecycleState::Succeeded { .. }
        ));
    }
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_stop_survives_abandoned_waiter_and_publishes_before_storage_close() {
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("reserved-stop-late-callback", &provider, true)
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
    let ticket = client.submit(prompt("reserved-stop")).await.unwrap();
    permits(&late.entered, 1).await;
    let request = reserved_request(&fixture.catalog, LifecycleAction::Stop {});
    let owned_request = request.clone();
    let publish = Gate::new();
    let publication = publish.clone();
    let stopping = service.clone();
    let waiter = tokio::spawn(async move {
        stopping
            .with_stop_reservation(false, move |reserved| {
                Box::pin(async move {
                    assert!(
                        reserved
                            .catalog()
                            .prepare_lifecycle(owned_request.clone())
                            .await
                            .map_err(reserved_catalog_error)?
                            .fresh
                    );
                    let drained = reserved.drain().await?;
                    assert_eq!(
                        drained
                            .catalog()
                            .status()
                            .await
                            .map_err(reserved_catalog_error)?
                            .resident,
                        0
                    );
                    drained
                        .catalog()
                        .finish_lifecycle(
                            owned_request,
                            LifecycleState::Succeeded {
                                result: LifecycleResult::NativeSettled {},
                            },
                        )
                        .await
                        .map_err(reserved_catalog_error)?;
                    publication.entered.add_permits(1);
                    permits(&publication.release, 1).await;
                    drained.close_storage().await
                })
            })
            .await
    });
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(service.owner_count(), 1);
    managed_writer_held(&record).await;
    assert!(ticket.completion.borrow().is_none());
    late.release.add_permits(1);
    permits(&publish.entered, 1).await;
    assert_eq!(service.owner_count(), 0);
    assert!(service.is_stopping());
    assert!(!service.is_closed());
    let receipt = fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::NativeSettled {}
        }
    );
    assert!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
            .await
            .is_err()
    );
    assert!(matches!(
        service
            .with_administration::<(), _>(|_| Box::pin(async {
                panic!("new administration must be rejected")
            }))
            .await,
        Err(ServiceError::Stopping)
    ));
    let draining = service.clone();
    let shutdown = tokio::spawn(async move { draining.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    publish.release.add_permits(1);
    tokio::time::timeout(DEADLINE, shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(service.is_closed());
    let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let duplicate = reopened.prepare_lifecycle(request).await.unwrap();
    assert!(!duplicate.fresh);
    assert_eq!(duplicate.receipt.status, receipt.status);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_stop_rejection_or_nonfresh_intent_does_not_drain_or_mark_stopping() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture
        .record("reserved-stop-no-effect", &provider, false)
        .await;
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(&service, &fixture, &record, binder(provider, None, None)).await;
    let mut stale = reserved_request(&fixture.catalog, LifecycleAction::Stop {});
    stale.epoch = HostEpoch::new();
    let result = service
        .with_stop_reservation(false, move |reserved| {
            Box::pin(async move {
                Err::<(), _>(reserved_catalog_error(
                    reserved
                        .catalog()
                        .prepare_lifecycle(stale)
                        .await
                        .unwrap_err(),
                ))
            })
        })
        .await;
    assert!(matches!(result, Err(ServiceError::Catalog(_))));
    assert!(!service.is_stopping());
    assert!(!service.is_closed());
    let request = reserved_request(&fixture.catalog, LifecycleAction::Stop {});
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    let duplicate = request.clone();
    let status = service
        .with_stop_reservation(false, move |reserved| {
            Box::pin(async move {
                let admission = reserved
                    .catalog()
                    .prepare_lifecycle(duplicate)
                    .await
                    .map_err(reserved_catalog_error)?;
                assert!(!admission.fresh);
                Ok(admission.receipt.status)
            })
        })
        .await
        .unwrap();
    assert_eq!(status, LifecycleState::Prepared {});
    assert!(!service.is_stopping());
    assert!(!service.is_closed());
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Live));
    let snapshot = client.snapshot().await.unwrap();
    assert!(!snapshot.cancel_requested && !snapshot.retiring);
    managed_writer_held(&record).await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    // The fixture still owns the original fresh admission and did not drain.
    fixture
        .catalog
        .finish_lifecycle(
            request,
            LifecycleState::Rejected {
                code: LifecycleFailure::InvalidRequest,
            },
        )
        .await
        .unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_stop_publication_failure_retains_catalog_and_never_fakes_a_terminal_receipt() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let request = reserved_request(&fixture.catalog, LifecycleAction::Stop {});
    let owned_request = request.clone();
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_reserved_stop_result BEFORE UPDATE ON lifecycle_requests WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected stop publication failure'); END;").unwrap();
    let result = service
        .with_stop_reservation(false, move |reserved| {
            Box::pin(async move {
                assert!(
                    reserved
                        .catalog()
                        .prepare_lifecycle(owned_request.clone())
                        .await
                        .map_err(reserved_catalog_error)?
                        .fresh
                );
                let drained = reserved.drain().await?;
                drained
                    .catalog()
                    .finish_lifecycle(
                        owned_request,
                        LifecycleState::Succeeded {
                            result: LifecycleResult::NativeSettled {},
                        },
                    )
                    .await
                    .map_err(reserved_catalog_error)?;
                drained.close_storage().await
            })
        })
        .await;
    assert!(matches!(result, Err(ServiceError::Catalog(_))));
    assert!(service.is_stopping());
    assert!(!service.is_closed());
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    assert_eq!(
        fixture
            .catalog
            .lifecycle_receipt(request.scope, request.request_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        LifecycleState::Prepared {}
    );
    drop(db);
    service.shutdown().await.unwrap();
    let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let duplicate = reopened.prepare_lifecycle(request).await.unwrap();
    assert!(!duplicate.fresh);
    assert_eq!(duplicate.receipt.status, LifecycleState::Interrupted {});
    reopened.shutdown().await.unwrap();
}
