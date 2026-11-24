// Included in ordinary::tests::administrative_guards. No IPC capability.
async fn admitted_input(
    client: &OrdinaryClient,
    host: &str,
    request: &str,
    text: &str,
) -> AdmittedRunTicket {
    let OrdinaryOperation::Prompt(messages) = prompt(text) else {
        unreachable!()
    };
    let ticket = client
        .submit_admitted(host.into(), request.into(), messages)
        .await
        .unwrap();
    ticket.wait_admitted().await.unwrap();
    ticket
}
fn input_id() -> String {
    crate::host::RequestId::new().to_string()
}
fn host_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn submitted_cancel_keeps_original_target_despite_duplicate_tickets_and_later_inputs() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("submitted-cancel", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider.clone(), Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let idle = client.snapshot().await.unwrap();
    let host = host_id();
    let original_id = input_id();
    let original = admitted_input(&client, &host, &original_id, "exact-original").await;
    permits(&gate.entered, 1).await;
    let duplicate = admitted_input(&client, &host, &original_id, "exact-original").await;
    assert_ne!(original.id, duplicate.id);
    let running = client.snapshot().await.unwrap();
    assert_eq!(
        running.submitted_request_id.as_deref(),
        Some(original_id.as_str())
    );
    assert_eq!(running.ticket, Some(original.id));
    let current = fixture.catalog.guard(&running.record);
    for guard in [
        fixture.catalog.guard(&idle.record),
        CatalogGuard {
            epoch: crate::host::HostEpoch::new(),
            revision: current.revision,
        },
    ] {
        rejected_guard(
            client
                .cancel_submitted_guarded(guard, &original_id)
                .await
                .unwrap_err(),
        );
    }
    assert!(!client.cancel_guarded(current, duplicate.id).await.unwrap());
    assert!(!client
        .cancel_submitted_guarded(current, &input_id())
        .await
        .unwrap());
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert!(client
        .cancel_submitted_guarded(current, &original_id)
        .await
        .unwrap());
    let report = tokio::time::timeout(DEADLINE, original.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,HarnessRunOutcome::Aborted {..})),
        "{report:?}"
    );
    let idle = client.snapshot().await.unwrap();
    assert!(idle.submitted_request_id.is_none());
    assert!(!client
        .cancel_submitted_guarded(fixture.catalog.guard(&idle.record), &original_id)
        .await
        .unwrap());
    provider.script().set_responses(vec![
        FauxStep::tool_call("gate", serde_json::json!({})),
        FauxStep::text("second-completed"),
    ]);
    let next_id = input_id();
    let next = admitted_input(&client, &host, &next_id, "different-input").await;
    permits(&gate.entered, 1).await;
    let next_snapshot = client.snapshot().await.unwrap();
    let current = fixture.catalog.guard(&next_snapshot.record);
    assert_eq!(
        next_snapshot.submitted_request_id.as_deref(),
        Some(next_id.as_str())
    );
    // A retry of the old durable input has another observer, never a new run.
    let old = admitted_input(&client, &host, &original_id, "exact-original").await;
    assert!(!client.cancel_guarded(current, old.id).await.unwrap());
    assert!(!client
        .cancel_submitted_guarded(current, &original_id)
        .await
        .unwrap());
    assert!(!client.snapshot().await.unwrap().cancel_requested);
    assert_eq!(client.snapshot().await.unwrap().ticket, Some(next.id));
    gate.release.add_permits(1);
    let report = tokio::time::timeout(DEADLINE, next.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,HarnessRunOutcome::Completed {..})),
        "{report:?}"
    );
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn submitted_cancel_retains_original_association_and_owner_until_late_callback_settles() {
    let fixture = Fixture::new(1).await;
    let provider = gate_provider();
    let record = fixture.record("submitted-late", &provider, true).await;
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
    let owner = fixture
        .restore(&record, binder(provider, Some(gate), Some(hook)))
        .await;
    let client = owner.client();
    let request = input_id();
    let ticket = admitted_input(&client, &host_id(), &request, "hold-late-callback").await;
    permits(&late.entered, 1).await;
    let current = fixture
        .catalog
        .guard(&client.snapshot().await.unwrap().record);
    assert!(client
        .cancel_submitted_guarded(current, &request)
        .await
        .unwrap());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(ticket.completion.borrow().is_none());
    let snapshot = client.snapshot().await.unwrap();
    assert_eq!(
        snapshot.submitted_request_id.as_deref(),
        Some(request.as_str())
    );
    assert!(snapshot.cancel_requested);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let path = record.spec.session.as_ref().unwrap().path.to_str().unwrap();
    assert!(env.try_acquire_writer_lease(path).await.is_err());
    late.release.add_permits(1);
    tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(client
        .snapshot()
        .await
        .unwrap()
        .submitted_request_id
        .is_none());
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn submitted_cancel_carries_deferred_identity_only_within_its_owner_lifetime() {
    use rpi_ai::{
        providers::faux::{faux_assistant_message, DeferredPoll},
        types::StopReason,
    };
    let fixture = Fixture::new(1).await;
    let source = FauxProvider::new(FauxScript::new());
    let (script, handle) = deferred_script(&source, "submitted-park");
    let provider = FauxProvider::new(script);
    let record = fixture.record("submitted-deferred", &provider, false).await;
    let owner = fixture
        .restore(&record, binder(provider.clone(), None, None))
        .await;
    let client = owner.client();
    let request = input_id();
    let original = admitted_input(&client, &host_id(), &request, "park-input").await;
    let report = tokio::time::timeout(DEADLINE, original.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,HarnessRunOutcome::Suspended {..})),
        "{report:?}"
    );
    let parked = client.snapshot().await.unwrap();
    assert_eq!(
        parked.submitted_request_id.as_deref(),
        Some(request.as_str())
    );
    assert!(parked.ticket.is_none());
    // Cancellation is not a deferred resume operation.
    assert!(!client
        .cancel_submitted_guarded(fixture.catalog.guard(&parked.record), &request)
        .await
        .unwrap());
    let next = rpi_ai::types::DeferredHandle {
        id: "submitted-next-park".into(),
        ..handle.clone()
    };
    provider.push_deferred_poll(DeferredPoll::StillDeferred(next.clone()));
    let polled = client
        .submit(OrdinaryOperation::ResumeDeferred(handle.id))
        .await
        .unwrap();
    polled.wait().await.unwrap();
    assert_eq!(
        client
            .snapshot()
            .await
            .unwrap()
            .submitted_request_id
            .as_deref(),
        Some(request.as_str())
    );
    // Restore observes the deferred handle, but cannot invent an input mapping.
    owner.close().await.unwrap();
    let closed = fixture.catalog.get(record.spec.id).await.unwrap();
    let owner = fixture
        .restore(&closed, binder(provider.clone(), None, None))
        .await;
    let client = owner.client();
    let restored = client.snapshot().await.unwrap();
    assert_eq!(restored.deferred.as_deref(), Some(next.id.as_str()));
    assert!(restored.submitted_request_id.is_none());
    assert!(!client
        .cancel_submitted_guarded(fixture.catalog.guard(&restored.record), &request)
        .await
        .unwrap());
    provider.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "resumed-explicitly",
        StopReason::Stop,
    )));
    complete(
        &client
            .submit(OrdinaryOperation::ResumeDeferred(next.id))
            .await
            .unwrap(),
    )
    .await;
    assert!(client
        .snapshot()
        .await
        .unwrap()
        .submitted_request_id
        .is_none());
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}
