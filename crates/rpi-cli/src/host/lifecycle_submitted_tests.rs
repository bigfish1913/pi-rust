#[tokio::test]
async fn submitted_cancel_journal_binds_target_preserves_legacy_and_never_regrants_after_restart() {
    let mut fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    let target = RequestId::new();
    let request = fixture.request(LifecycleAction::CancelSubmitted {
        agent_id: record.spec.id,
        revision: record.revision,
        submitted_request_id: target,
    });
    let (first, second) = tokio::join!(
        fixture.catalog.prepare_lifecycle(request.clone()),
        fixture.catalog.prepare_lifecycle(request.clone())
    );
    assert_ne!(first.unwrap().fresh, second.unwrap().fresh);
    let mut changed = request.clone();
    changed.action = LifecycleAction::CancelSubmitted {
        agent_id: record.spec.id,
        revision: record.revision,
        submitted_request_id: RequestId::new(),
    };
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(changed).await,
        Err(CatalogError::LifecycleConflict)
    ));
    let mut hybrid = serde_json::to_value(&request).unwrap();
    hybrid["action"]["ticket_id"] = serde_json::json!(RunId::new());
    assert!(serde_json::from_value::<LifecycleRequest>(hybrid).is_err());
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(
                request.clone(),
                LifecycleState::Succeeded {
                    result: LifecycleResult::Loaded {
                        revision: record.revision
                    }
                }
            )
            .await,
        Err(CatalogError::InvalidRecord(_))
    ));
    let terminal = fixture
        .catalog
        .finish_lifecycle(
            request.clone(),
            LifecycleState::Succeeded {
                result: LifecycleResult::CancelSignalled { signalled: true },
            },
        )
        .await
        .unwrap();
    let legacy = fixture.request(LifecycleAction::Cancel {
        agent_id: record.spec.id,
        revision: record.revision,
        ticket_id: RunId::new(),
    });
    fixture
        .catalog
        .prepare_lifecycle(legacy.clone())
        .await
        .unwrap();
    let legacy_terminal = fixture
        .catalog
        .finish_lifecycle(legacy.clone(), rejected())
        .await
        .unwrap();
    fixture.reopen().await;
    for (request, expected) in [(request.clone(), terminal), (legacy, legacy_terminal)] {
        let outcome = fixture.catalog.prepare_lifecycle(request).await.unwrap();
        assert!(!outcome.fresh);
        assert_eq!(outcome.receipt, expected);
    }
    let mut unfinished = request;
    unfinished.request_id = RequestId::new();
    unfinished.epoch = fixture.catalog.epoch();
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(unfinished.clone())
            .await
            .unwrap()
            .fresh
    );
    fixture.reopen().await;
    let outcome = fixture.catalog.prepare_lifecycle(unfinished).await.unwrap();
    assert!(!outcome.fresh);
    assert_eq!(outcome.receipt.status, LifecycleState::Interrupted {});
    fixture.catalog.shutdown().await.unwrap();
}
