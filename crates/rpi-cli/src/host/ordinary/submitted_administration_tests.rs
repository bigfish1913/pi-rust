// Included alongside the owned administrative Harness/JSONL tests.
#[tokio::test(flavor = "current_thread")]
async fn submitted_cancel_owned_journal_survives_lost_waiter_without_retargeting_later_input() {
    let fixture = Fixture::new(1).await;
    let provider = managed_gate_provider();
    let record = fixture
        .record("owned-submitted-cancel", &provider, true)
        .await;
    let tool = Gate::new();
    tool.release.add_permits(1);
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
    let service = HostService::new(fixture.catalog.clone());
    let client = managed_restore(
        &service,
        &fixture,
        &record,
        binder(provider.clone(), Some(tool.clone()), Some(hook)),
    )
    .await;
    let host = uuid::Uuid::new_v4().to_string();
    let input = RequestId::new();
    let OrdinaryOperation::Prompt(messages) = prompt("owned-original") else {
        unreachable!()
    };
    let ticket = client
        .submit_admitted(host.clone(), input.to_string(), messages)
        .await
        .unwrap();
    ticket.wait_admitted().await.unwrap();
    permits(&tool.entered, 1).await;
    permits(&late.entered, 1).await;
    let running = client.snapshot().await.unwrap();
    let grant = Arc::new(
        AdministrativeGrant::new(
            ControlScopeId::new(),
            [record.spec.id].into(),
            AdministrativePermissions {
                cancel: true,
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
        LifecycleAction::CancelSubmitted {
            agent_id: record.spec.id,
            revision: running.record.revision,
            submitted_request_id: input,
        },
    );
    let deny = AdministrativeGrant::new(
        grant.scope(),
        [record.spec.id].into(),
        AdministrativePermissions::default(),
    )
    .unwrap();
    assert!(matches!(
        coordinator.execute(&deny, request.clone()).await,
        Err(AdministrativeError::Unauthorized)
    ));
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    let mut missing = request.clone();
    missing.request_id = RequestId::new();
    missing.action = LifecycleAction::CancelSubmitted {
        agent_id: record.spec.id,
        revision: running.record.revision,
        submitted_request_id: RequestId::new(),
    };
    let no_signal = coordinator.execute(&grant, missing.clone()).await.unwrap();
    assert_eq!(
        no_signal.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: false }
        }
    );
    administrative_idle(&coordinator).await;
    // Hold reservation before prepare; abandoning the remote-equivalent waiter
    // must neither cancel the owned job nor redirect it when the fence opens.
    let gate = Gate::new();
    let held = gate.clone();
    let reserved_service = service.clone();
    let fence = tokio::spawn(async move {
        reserved_service
            .with_stop_reservation(false, move |reserved| {
                Box::pin(async move {
                    let _held = reserved;
                    held.entered.add_permits(1);
                    permits(&held.release, 1).await;
                    Ok(())
                })
            })
            .await
    });
    permits(&gate.entered, 1).await;
    let worker = coordinator.clone();
    let authority = grant.clone();
    let intent = request.clone();
    let waiter = tokio::spawn(async move { worker.execute(&authority, intent).await });
    tokio::time::timeout(DEADLINE, async {
        while coordinator.active_jobs() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(coordinator
        .receipt(&grant, &request)
        .await
        .unwrap()
        .is_none());
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(coordinator.active_jobs(), 1);
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    gate.release.add_permits(1);
    tokio::time::timeout(DEADLINE, fence)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    administrative_idle(&coordinator).await;
    let terminal = coordinator
        .receipt(&grant, &request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: true }
        }
    );
    assert!(client.snapshot().await.unwrap().cancel_requested);
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
        FauxStep::text("later-input-finished"),
    ]);
    let next_id = RequestId::new();
    let OrdinaryOperation::Prompt(messages) = prompt("owned-later-input") else {
        unreachable!()
    };
    let next = client
        .submit_admitted(host, next_id.to_string(), messages)
        .await
        .unwrap();
    next.wait_admitted().await.unwrap();
    permits(&tool.entered, 1).await;
    assert_eq!(
        coordinator.execute(&grant, request.clone()).await.unwrap(),
        terminal
    );
    administrative_idle(&coordinator).await;
    let next_snapshot = client.snapshot().await.unwrap();
    assert_eq!(
        next_snapshot.submitted_request_id.as_deref(),
        Some(next_id.to_string().as_str())
    );
    assert!(!next_snapshot.cancel_requested);
    assert!(!service
        .cancel_submitted_ordinary_guarded(
            record.spec.id,
            fixture.catalog.guard(&next_snapshot.record),
            input
        )
        .await
        .unwrap());
    let mut changed = request;
    changed.action = LifecycleAction::CancelSubmitted {
        agent_id: record.spec.id,
        revision: running.record.revision,
        submitted_request_id: next_id,
    };
    assert!(matches!(
        coordinator.execute(&grant, changed).await,
        Err(AdministrativeError::Conflict)
    ));
    administrative_idle(&coordinator).await;
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    tool.release.add_permits(1);
    late.release.add_permits(1);
    let report = tokio::time::timeout(DEADLINE, next.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,HarnessRunOutcome::Completed {..})),
        "{report:?}"
    );
    service.shutdown().await.unwrap();
}
