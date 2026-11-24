use crate::host::ProfileVersion;

#[tokio::test]
async fn profile_version_is_part_of_the_entire_deduplicated_journal_request() {
    let mut fixture = Fixture::new().await;
    let reference = ProfileRef {
        agent_id: AgentId::new(),
        version: ProfileVersion::new(),
    };
    let request = fixture.request(LifecycleAction::CreateProfile { profile: reference });
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    assert!(
        !fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    let mut changed = request.clone();
    changed.action = LifecycleAction::CreateProfile {
        profile: ProfileRef {
            version: ProfileVersion::new(),
            ..reference
        },
    };
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(changed).await,
        Err(CatalogError::LifecycleConflict)
    ));
    // Versioning does not create a second pending lane for the same Agent.
    let competing = fixture.request(LifecycleAction::CreateProfile {
        profile: ProfileRef {
            version: ProfileVersion::new(),
            ..reference
        },
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(competing).await,
        Err(CatalogError::LifecycleBusy)
    ));
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(
                request.clone(),
                LifecycleState::Succeeded {
                    result: LifecycleResult::Closed { revision: 1 },
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
                result: LifecycleResult::Loaded { revision: 1 },
            },
        )
        .await
        .unwrap();
    fixture.reopen().await;
    let retry = fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    assert!(!retry.fresh);
    assert_eq!(retry.receipt, terminal);
    assert_eq!(
        retry.receipt.request.action,
        LifecycleAction::CreateProfile { profile: reference }
    );
    // Journal custody never creates/loads an Agent or authorizes this ref.
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn profile_restore_retains_revision_existence_result_and_pending_guards() {
    let fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    let profile = ProfileRef {
        agent_id: record.spec.id,
        version: ProfileVersion::new(),
    };
    let create = fixture.request(LifecycleAction::CreateProfile { profile });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(create).await,
        Err(CatalogError::AlreadyRegistered)
    ));
    let missing = fixture.request(LifecycleAction::RestoreProfile {
        profile: ProfileRef {
            agent_id: AgentId::new(),
            ..profile
        },
        revision: 1,
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(missing).await,
        Err(CatalogError::UnknownAgent)
    ));
    for revision in [0, i64::MAX as u64 + 1, record.revision + 1] {
        let invalid = fixture.request(LifecycleAction::RestoreProfile { profile, revision });
        assert!(matches!(
            fixture.catalog.prepare_lifecycle(invalid.clone()).await,
            Err(CatalogError::InvalidRecord(_) | CatalogError::RevisionConflict { .. })
        ));
        assert!(fixture
            .catalog
            .lifecycle_receipt(invalid.scope, invalid.request_id)
            .await
            .unwrap()
            .is_none());
    }
    let request = fixture.request(LifecycleAction::RestoreProfile {
        profile,
        revision: record.revision,
    });
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    let close = fixture.request(LifecycleAction::Close {
        agent_id: record.spec.id,
        revision: record.revision,
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(close).await,
        Err(CatalogError::LifecycleBusy)
    ));
    let mut altered = request.clone();
    altered.action = LifecycleAction::RestoreProfile {
        profile: ProfileRef {
            version: ProfileVersion::new(),
            ..profile
        },
        revision: record.revision,
    };
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(altered).await,
        Err(CatalogError::LifecycleConflict)
    ));
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(
                request.clone(),
                LifecycleState::Succeeded {
                    result: LifecycleResult::Loaded {
                        revision: record.revision - 1
                    },
                }
            )
            .await,
        Err(CatalogError::InvalidRecord(_))
    ));
    fixture
        .catalog
        .finish_lifecycle(
            request,
            LifecycleState::Succeeded {
                result: LifecycleResult::Loaded {
                    revision: record.revision,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().revision,
        record.revision
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

// Insert the original v2 JSON shape literally, not by serializing the revised
// enum. A later binary must preserve these receipts without minting profile refs
// or reinterpreting unbound legacy requests as executable profile operations.
fn insert_literal_v2_receipt(
    fixture: &Fixture,
    request: &LifecycleRequest,
    status: serde_json::Value,
) -> String {
    let (agent, action) = match request.action {
        LifecycleAction::Create { agent_id } => (
            agent_id,
            serde_json::json!({"op": "create", "agent_id": agent_id}),
        ),
        LifecycleAction::Restore { agent_id, revision } => (
            agent_id,
            serde_json::json!({"op": "restore", "agent_id": agent_id, "revision": revision}),
        ),
        _ => panic!("legacy fixture requires an unbound v2 action"),
    };
    let data = serde_json::to_string(&serde_json::json!({
        "request": {"scope": request.scope, "request_id": request.request_id,
            "epoch": request.epoch, "action": action},
        "status": status
    }))
    .unwrap();
    fixture.db().execute(
        "INSERT INTO lifecycle_requests (scope_id,request_id,epoch,agent_id,status,data) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![format!("{}", request.scope), format!("{}", request.request_id),
            format!("{}", request.epoch), format!("{agent}"), status["state"].as_str().unwrap(), data],
    ).unwrap();
    data
}

#[tokio::test]
async fn literal_v2_legacy_and_versioned_profile_rows_recover_without_replay_or_conversion() {
    let mut fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    let legacy_create = fixture.create();
    insert_literal_v2_receipt(
        &fixture,
        &legacy_create,
        serde_json::json!({"state": "prepared"}),
    );
    let legacy_restore = fixture.request(LifecycleAction::Restore {
        agent_id: record.spec.id,
        revision: record.revision,
    });
    let legacy_bytes = insert_literal_v2_receipt(
        &fixture,
        &legacy_restore,
        serde_json::json!({"state": "succeeded", "result": {"kind": "loaded", "revision": record.revision}}),
    );
    let profile_create = fixture.request(LifecycleAction::CreateProfile {
        profile: ProfileRef {
            agent_id: AgentId::new(),
            version: ProfileVersion::new(),
        },
    });
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(profile_create.clone())
            .await
            .unwrap()
            .fresh
    );
    let profile_restore = fixture.request(LifecycleAction::RestoreProfile {
        profile: ProfileRef {
            agent_id: record.spec.id,
            version: ProfileVersion::new(),
        },
        revision: record.revision,
    });
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(profile_restore.clone())
            .await
            .unwrap()
            .fresh
    );
    let terminal = fixture
        .catalog
        .finish_lifecycle(
            profile_restore.clone(),
            LifecycleState::Succeeded {
                result: LifecycleResult::Loaded {
                    revision: record.revision,
                },
            },
        )
        .await
        .unwrap();
    fixture.reopen().await;
    for pending in [legacy_create, profile_create] {
        let duplicate = fixture
            .catalog
            .prepare_lifecycle(pending.clone())
            .await
            .unwrap();
        assert!(!duplicate.fresh);
        assert_eq!(duplicate.receipt.request, pending);
        assert_eq!(duplicate.receipt.status, LifecycleState::Interrupted {});
    }
    let legacy = fixture
        .catalog
        .prepare_lifecycle(legacy_restore.clone())
        .await
        .unwrap();
    assert!(!legacy.fresh);
    assert_eq!(legacy.receipt.request, legacy_restore);
    assert!(matches!(
        legacy.receipt.status,
        LifecycleState::Succeeded { .. }
    ));
    assert_eq!(
        fixture
            .catalog
            .prepare_lifecycle(profile_restore)
            .await
            .unwrap()
            .receipt,
        terminal
    );
    let stored: String = fixture
        .db()
        .query_row(
            "SELECT data FROM lifecycle_requests WHERE scope_id=?1 AND request_id=?2",
            rusqlite::params![
                format!("{}", legacy_restore.scope),
                format!("{}", legacy_restore.request_id)
            ],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, legacy_bytes);
    assert_eq!(
        fixture
            .db()
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}
