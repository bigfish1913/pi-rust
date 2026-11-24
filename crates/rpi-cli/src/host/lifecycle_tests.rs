use super::*;
use crate::host::{AgentMode, AgentState, HostCatalog, HostConfig};

include!("lifecycle_profile_tests.rs");
include!("lifecycle_submitted_tests.rs");

struct Fixture {
    root: tempfile::TempDir,
    catalog: HostCatalog,
    scope: ControlScopeId,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
            .await
            .unwrap();
        Self {
            root,
            catalog,
            scope: ControlScopeId::new(),
        }
    }
    fn request(&self, action: LifecycleAction) -> LifecycleRequest {
        LifecycleRequest {
            scope: self.scope,
            request_id: RequestId::new(),
            epoch: self.catalog.epoch(),
            action,
        }
    }
    fn create(&self) -> LifecycleRequest {
        self.request(LifecycleAction::Create {
            agent_id: AgentId::new(),
        })
    }
    fn db(&self) -> Connection {
        Connection::open(self.root.path().join("host/agents.sqlite3")).unwrap()
    }
    async fn reopen(&mut self) {
        self.catalog.shutdown().await.unwrap();
        self.catalog = HostCatalog::open(self.root.path().join("host"), HostConfig::default())
            .await
            .unwrap();
    }
}
fn rejected() -> LifecycleState {
    LifecycleState::Rejected {
        code: LifecycleFailure::InvalidRequest,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sigkill_preserves_prepared_custody_without_replaying_or_regranting() {
    use std::{io::Write, os::unix::process::ExitStatusExt, process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    const CHILD: &str = "RPI_HOST_LIFECYCLE_CRASH_CHILD";
    const READY: &str = "LIFECYCLE_JOURNAL_PREPARED ";
    // Re-enter this exact test in a private subprocess, without a default-skipped
    // helper test. The parent kills it only after the real journal grants fresh.
    if let Some(directory) = std::env::var_os(CHILD) {
        let catalog = HostCatalog::open(directory.into(), HostConfig::default())
            .await
            .unwrap();
        let request = LifecycleRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: catalog.epoch(),
            action: LifecycleAction::Create {
                agent_id: AgentId::new(),
            },
        };
        assert!(
            catalog
                .prepare_lifecycle(request.clone())
                .await
                .unwrap()
                .fresh
        );
        println!("{READY}{}", serde_json::to_string(&request).unwrap());
        std::io::stdout().flush().unwrap();
        std::future::pending::<()>().await;
        unreachable!();
    }
    let mut fixture = Fixture::new().await;
    fixture.catalog.shutdown().await.unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("host::lifecycle::tests::sigkill_preserves_prepared_custody_without_replaying_or_regranting")
        .arg("--nocapture")
        .env(CHILD, fixture.root.path().join("host"))
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap().take(8192)).lines();
    let request: LifecycleRequest = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some((_, data)) = line.split_once(READY) {
                return serde_json::from_str(data).unwrap();
            }
        }
        panic!("journal child exited before committing Prepared");
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    assert_eq!(child.wait().await.unwrap().signal(), Some(9));
    fixture.catalog = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let receipt = fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, LifecycleState::Interrupted {});
    assert!(
        !fixture
            .catalog
            .prepare_lifecycle(request)
            .await
            .unwrap()
            .fresh
    );
    assert_eq!(
        fixture.catalog.status().await.unwrap().registered,
        0,
        "journal restart must not execute Create"
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_preparers_get_one_fresh_owner_and_full_request_conflicts_are_rejected() {
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let catalog = fixture.catalog.clone();
        let request = request.clone();
        tasks.push(tokio::spawn(async move {
            catalog.prepare_lifecycle(request).await.unwrap()
        }));
    }
    let mut fresh = 0;
    for task in tasks {
        let admission = task.await.unwrap();
        fresh += usize::from(admission.fresh);
        assert_eq!(admission.receipt.request, request);
        assert_eq!(admission.receipt.status, LifecycleState::Prepared {});
    }
    assert_eq!(fresh, 1);
    let mut changed = request.clone();
    changed.action = LifecycleAction::Create {
        agent_id: AgentId::new(),
    };
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(changed).await,
        Err(CatalogError::LifecycleConflict)
    ));
    let mut changed = request.clone();
    changed.epoch = HostEpoch::new();
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(changed).await,
        Err(CatalogError::LifecycleConflict)
    ));
    assert_eq!(
        fixture.catalog.status().await.unwrap().registered,
        0,
        "journal must not create or execute Agents"
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_unfinished_intents_without_transferring_execution_authority() {
    let mut fixture = Fixture::new().await;
    let request = fixture.create();
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    fixture.reopen().await;
    let receipt = fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, LifecycleState::Interrupted {});
    let duplicate = fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    assert!(!duplicate.fresh);
    assert_eq!(duplicate.receipt, receipt);
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(request.clone(), rejected())
            .await,
        Err(CatalogError::LifecycleConflict)
    ));
    let mut stale = fixture.create();
    stale.epoch = request.epoch;
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(stale).await,
        Err(CatalogError::EpochConflict)
    ));
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_results_survive_restart_are_immutable_and_do_not_claim_process_exit() {
    let mut fixture = Fixture::new().await;
    let request = fixture.request(LifecycleAction::Stop {});
    fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    let status = LifecycleState::Succeeded {
        result: LifecycleResult::NativeSettled {},
    };
    let receipt = fixture
        .catalog
        .finish_lifecycle(request.clone(), status.clone())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .catalog
            .finish_lifecycle(request.clone(), status.clone())
            .await
            .unwrap(),
        receipt
    );
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(request.clone(), rejected())
            .await,
        Err(CatalogError::LifecycleConflict)
    ));
    fixture.reopen().await;
    let duplicate = fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    assert!(!duplicate.fresh);
    assert_eq!(duplicate.receipt, receipt);
    assert_eq!(
        fixture
            .catalog
            .finish_lifecycle(request, status)
            .await
            .unwrap(),
        receipt
    );
    // There are no native owners in this journal-only fixture. NativeSettled
    // does not mean this process/catalog/listener has stopped.
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn namespaces_are_explicit_and_missing_requests_are_not_admitted_by_queries() {
    let fixture = Fixture::new().await;
    let first = fixture.create();
    assert!(fixture
        .catalog
        .lifecycle_receipt(first.scope, first.request_id)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(first.clone(), rejected())
            .await,
        Err(CatalogError::UnknownLifecycle)
    ));
    fixture
        .catalog
        .prepare_lifecycle(first.clone())
        .await
        .unwrap();
    fixture
        .catalog
        .finish_lifecycle(first.clone(), rejected())
        .await
        .unwrap();
    let mut second = fixture.create();
    second.request_id = first.request_id;
    second.scope = ControlScopeId::new();
    assert!(fixture
        .catalog
        .lifecycle_receipt(second.scope, second.request_id)
        .await
        .unwrap()
        .is_none());
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(second.clone())
            .await
            .unwrap()
            .fresh
    );
    assert_eq!(
        fixture
            .catalog
            .lifecycle_receipt(first.scope, first.request_id)
            .await
            .unwrap()
            .unwrap()
            .request,
        first
    );
    assert_eq!(
        fixture
            .catalog
            .lifecycle_receipt(second.scope, second.request_id)
            .await
            .unwrap()
            .unwrap()
            .request,
        second
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn epoch_revision_and_existing_agent_checks_precede_intent_commit() {
    let fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    let create = fixture.request(LifecycleAction::Create {
        agent_id: record.spec.id,
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(create).await,
        Err(CatalogError::AlreadyRegistered)
    ));
    let unknown = fixture.request(LifecycleAction::Restore {
        agent_id: AgentId::new(),
        revision: 1,
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(unknown).await,
        Err(CatalogError::UnknownAgent)
    ));
    let stale = fixture.request(LifecycleAction::Close {
        agent_id: record.spec.id,
        revision: record.revision + 1,
    });
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(stale.clone()).await,
        Err(CatalogError::RevisionConflict { .. })
    ));
    assert!(fixture
        .catalog
        .lifecycle_receipt(stale.scope, stale.request_id)
        .await
        .unwrap()
        .is_none());
    for revision in [0, i64::MAX as u64 + 1] {
        let invalid = fixture.request(LifecycleAction::Restore {
            agent_id: record.spec.id,
            revision,
        });
        assert!(matches!(
            fixture.catalog.prepare_lifecycle(invalid).await,
            Err(CatalogError::InvalidRecord(_))
        ));
    }
    let valid = fixture.request(LifecycleAction::Close {
        agent_id: record.spec.id,
        revision: record.revision,
    });
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(valid)
            .await
            .unwrap()
            .fresh
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_intents_exclude_conflicting_agent_actions_and_global_stop() {
    let fixture = Fixture::new().await;
    let first = fixture.create();
    fixture
        .catalog
        .prepare_lifecycle(first.clone())
        .await
        .unwrap();
    let mut conflicting = first.clone();
    conflicting.request_id = RequestId::new();
    conflicting.scope = ControlScopeId::new();
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(conflicting).await,
        Err(CatalogError::LifecycleBusy)
    ));
    let stop = fixture.request(LifecycleAction::Stop {});
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(stop.clone()).await,
        Err(CatalogError::LifecycleBusy)
    ));
    fixture
        .catalog
        .finish_lifecycle(first, rejected())
        .await
        .unwrap();
    fixture
        .catalog
        .prepare_lifecycle(stop.clone())
        .await
        .unwrap();
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(fixture.create()).await,
        Err(CatalogError::LifecycleBusy)
    ));
    fixture
        .catalog
        .finish_lifecycle(
            stop,
            LifecycleState::Succeeded {
                result: LifecycleResult::NativeSettled {},
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(fixture.create())
            .await
            .unwrap()
            .fresh
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn mismatched_results_unknown_fields_and_forged_interrupted_states_are_rejected() {
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let mut value = serde_json::to_value(&request).unwrap();
    value["action"]["credentials"] = serde_json::json!({"do_not_accept":"secret"});
    assert!(serde_json::from_value::<LifecycleRequest>(value).is_err());
    assert!(serde_json::from_value::<LifecycleState>(
        serde_json::json!({"state":"prepared","extra":true})
    )
    .is_err());
    fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    for state in [
        LifecycleState::Prepared {},
        LifecycleState::Interrupted {},
        LifecycleState::Succeeded {
            result: LifecycleResult::Closed { revision: 1 },
        },
        LifecycleState::Succeeded {
            result: LifecycleResult::Loaded { revision: 0 },
        },
    ] {
        assert!(matches!(
            fixture
                .catalog
                .finish_lifecycle(request.clone(), state)
                .await,
            Err(CatalogError::InvalidRecord(_))
        ));
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
    }
    fixture
        .catalog
        .finish_lifecycle(
            request,
            LifecycleState::Uncertain {
                code: LifecycleFailure::PersistenceUncertain,
            },
        )
        .await
        .unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_v1_catalog_migrates_in_place_preserving_registered_agents() {
    let mut fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    fixture.catalog.shutdown().await.unwrap();
    let db = fixture.db();
    db.execute_batch("DROP TABLE lifecycle_requests; PRAGMA user_version=1;")
        .unwrap();
    drop(db);
    fixture.catalog = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().spec,
        record.spec
    );
    let version: i64 = fixture
        .db()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(fixture.create())
            .await
            .unwrap()
            .fresh
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_journal_rolls_back_epoch_agent_reclassification_and_partial_recovery() {
    let mut fixture = Fixture::new().await;
    let record = fixture
        .catalog
        .register(crate::host::registry_tests::make_spec(
            AgentMode::Persistent,
        ))
        .await
        .unwrap();
    let record = fixture
        .catalog
        .transition(
            record.spec.id,
            fixture.catalog.guard(&record),
            AgentState::Starting,
        )
        .await
        .unwrap();
    let record = fixture
        .catalog
        .transition(
            record.spec.id,
            fixture.catalog.guard(&record),
            AgentState::Idle,
        )
        .await
        .unwrap();
    let request = fixture.create();
    fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    let epoch = fixture.catalog.epoch();
    fixture.catalog.shutdown().await.unwrap();
    let db = fixture.db();
    let saved: String = db
        .query_row("SELECT data FROM lifecycle_requests", [], |r| r.get(0))
        .unwrap();
    let mut bad: LifecycleReceipt = serde_json::from_str(&saved).unwrap();
    bad.request.scope = ControlScopeId::new();
    db.execute(
        "UPDATE lifecycle_requests SET data=?1",
        [encode(&bad).unwrap()],
    )
    .unwrap();
    assert!(matches!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await,
        Err(CatalogError::InvalidRecord(_))
    ));
    let stored_epoch: String = db
        .query_row("SELECT epoch FROM host_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored_epoch, epoch.to_string());
    let stored: String = db
        .query_row("SELECT data FROM agents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<crate::host::AgentRecord>(&stored).unwrap(),
        record
    );
    db.execute("UPDATE lifecycle_requests SET data=?1", [saved])
        .unwrap();
    drop(db);
    fixture.catalog = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Unloaded
    );
    assert_eq!(
        fixture
            .catalog
            .lifecycle_receipt(request.scope, request.request_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        LifecycleState::Interrupted {}
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn capacity_exhaustion_does_not_evict_identities_or_prevent_exact_duplicate_queries() {
    let fixture = Fixture::new().await;
    let db = fixture.db();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut first = None;
    for _ in 0..MAX_LIFECYCLE_REQUESTS {
        let request = fixture.create();
        let receipt = LifecycleReceipt {
            request: request.clone(),
            status: rejected(),
        };
        // Bulk terminal fixture population; it does not claim Agent execution.
        db.execute("INSERT INTO lifecycle_requests (scope_id,request_id,epoch,agent_id,status,data) VALUES (?1,?2,?3,?4,'rejected',?5)", params![request.scope.to_string(),request.request_id.to_string(),request.epoch.to_string(),request.action.agent().unwrap().to_string(),encode(&receipt).unwrap()]).unwrap();
        first.get_or_insert(request);
    }
    db.execute_batch("COMMIT").unwrap();
    drop(db);
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(fixture.create()).await,
        Err(CatalogError::LifecycleCapacity)
    ));
    let first = first.unwrap();
    assert!(
        !fixture
            .catalog
            .prepare_lifecycle(first.clone())
            .await
            .unwrap()
            .fresh
    );
    assert!(fixture
        .catalog
        .lifecycle_receipt(first.scope, first.request_id)
        .await
        .unwrap()
        .is_some());
    fixture.catalog.shutdown().await.unwrap();
    // External over-capacity corruption must fail closed on restart as well.
    // The delete below repairs only this deliberately corrupted test fixture;
    // there is no production eviction/reset API.
    let db = fixture.db();
    let extra = fixture.create();
    let receipt = LifecycleReceipt {
        request: extra.clone(),
        status: rejected(),
    };
    db.execute("INSERT INTO lifecycle_requests (scope_id,request_id,epoch,agent_id,status,data) VALUES (?1,?2,?3,?4,'rejected',?5)", params![extra.scope.to_string(),extra.request_id.to_string(),extra.epoch.to_string(),extra.action.agent().unwrap().to_string(),encode(&receipt).unwrap()]).unwrap();
    assert!(matches!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await,
        Err(CatalogError::InvalidRecord(_))
    ));
    db.execute(
        "DELETE FROM lifecycle_requests WHERE scope_id=?1 AND request_id=?2",
        params![extra.scope.to_string(), extra.request_id.to_string()],
    )
    .unwrap();
    drop(db);
    HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_insert_or_result_commit_never_grants_execution_or_rewrites_a_terminal_result() {
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let db = fixture.db();
    db.execute_batch("CREATE TRIGGER reject_intent BEFORE INSERT ON lifecycle_requests BEGIN SELECT RAISE(ABORT,'injected intent failure'); END;").unwrap();
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(request.clone()).await,
        Err(CatalogError::Database(_))
    ));
    assert!(fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .is_none());
    db.execute_batch("DROP TRIGGER reject_intent").unwrap();
    assert!(
        fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    db.execute_batch("CREATE TRIGGER reject_result BEFORE UPDATE ON lifecycle_requests BEGIN SELECT RAISE(ABORT,'injected result failure'); END;").unwrap();
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(request.clone(), rejected())
            .await,
        Err(CatalogError::Database(_))
    ));
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
    assert!(
        !fixture
            .catalog
            .prepare_lifecycle(request.clone())
            .await
            .unwrap()
            .fresh
    );
    db.execute_batch("DROP TRIGGER reject_result").unwrap();
    fixture
        .catalog
        .finish_lifecycle(request, rejected())
        .await
        .unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_visibility_after_failed_checkpoint_is_resynced_without_reexecuting() {
    let fixture = Fixture::new().await;
    let request = fixture.request(LifecycleAction::Stop {});
    fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    let reader = fixture.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM lifecycle_requests", [], |r| r.get(0))
        .unwrap();
    let status = LifecycleState::Succeeded {
        result: LifecycleResult::NativeSettled {},
    };
    assert!(matches!(
        fixture
            .catalog
            .finish_lifecycle(request.clone(), status.clone())
            .await,
        Err(CatalogError::LifecycleUncertain)
    ));
    assert!(matches!(
        fixture
            .catalog
            .lifecycle_receipt(request.scope, request.request_id)
            .await,
        Err(CatalogError::LifecycleUncertain)
    ));
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    let receipt = fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, status);
    let duplicate = fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    assert!(!duplicate.fresh);
    assert_eq!(duplicate.receipt, receipt);
    assert_eq!(
        fixture
            .catalog
            .finish_lifecycle(request, status)
            .await
            .unwrap(),
        receipt
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn incomplete_checkpoint_withholds_fresh_grant_and_receipts_then_resyncs_without_regranting()
{
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let reader = fixture.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM lifecycle_requests", [], |r| r.get(0))
        .unwrap();
    assert!(matches!(
        fixture.catalog.prepare_lifecycle(request.clone()).await,
        Err(CatalogError::LifecycleUncertain)
    ));
    assert!(matches!(
        fixture
            .catalog
            .lifecycle_receipt(request.scope, request.request_id)
            .await,
        Err(CatalogError::LifecycleUncertain)
    ));
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    let retry = fixture
        .catalog
        .prepare_lifecycle(request.clone())
        .await
        .unwrap();
    assert!(
        !retry.fresh,
        "visibility after failed checkpoint must not grant a second executor"
    );
    assert_eq!(retry.receipt.status, LifecycleState::Prepared {});
    assert!(fixture
        .catalog
        .lifecycle_receipt(request.scope, request.request_id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    fixture.catalog.shutdown().await.unwrap();
}
