use super::*;
use crate::host::{
    AgentProfileFile, AgentSpec, HostCatalog, HostConfig, HostStartupAgent, ProfileVersion,
};
use rpi_harness::session::{values::SessionValueWriter, Session};
use rpi_tools::OsExecutionEnv;
use serde_json::json;
use std::{path::Path, time::Duration};

const DEADLINE: Duration = Duration::from_secs(15);
fn input(root: &Path, name: &str) -> HostStartupAgent {
    let root = root.canonicalize().unwrap();
    serde_json::from_value::<AgentProfileFile>(json!({
        "agent": {
            "id": AgentId::new(), "name": name, "cwd": root,
            "session": {"id": name, "path": root.join(format!("{name}.jsonl"))},
            "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []
        },
        "runtime": {"providers": {"fixture": {
            "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "native-test-key",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]
        }}},
        "toolEnvironment": {"HOME": root, "PATH": "/usr/bin:/bin"}, "shell": "/bin/sh", "thinking": "off"
    })).unwrap().into_startup().unwrap()
}
fn permissions() -> AdministrativePermissions {
    AdministrativePermissions {
        create: true,
        restore: true,
        cancel: true,
        close: true,
        stop: true,
    }
}
struct Fixture {
    root: tempfile::TempDir,
    catalog: HostCatalog,
    service: HostService,
    coordinator: AdministrativeCoordinator,
    grant: AdministrativeGrant,
    reference: ProfileRef,
    spec: AgentSpec,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let agent = input(root.path(), "actor-agent");
        let spec = agent.spec.clone();
        let profiles = TrustedProfileSet::freeze(vec![agent]).await.unwrap();
        let reference = profiles.references().next().unwrap();
        let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
            .await
            .unwrap();
        let service = HostService::new(catalog.clone());
        let coordinator = AdministrativeCoordinator::new(
            service.clone(),
            profiles,
            NonZeroUsize::new(2).unwrap(),
        )
        .unwrap();
        let grant =
            AdministrativeGrant::new(ControlScopeId::new(), [spec.id].into(), permissions())
                .unwrap();
        Self {
            root,
            catalog,
            service,
            coordinator,
            grant,
            reference,
            spec,
        }
    }
    fn request(&self, action: LifecycleAction) -> LifecycleRequest {
        LifecycleRequest {
            scope: self.grant.scope(),
            request_id: RequestId::new(),
            epoch: self.catalog.epoch(),
            action,
        }
    }
    fn create(&self) -> LifecycleRequest {
        self.request(LifecycleAction::CreateProfile {
            profile: self.reference,
        })
    }
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.root.path().join("host/agents.sqlite3")).unwrap()
    }
}
async fn idle(coordinator: &AdministrativeCoordinator) {
    tokio::time::timeout(DEADLINE, async {
        while coordinator.active_jobs() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn succeeded(receipt: &LifecycleReceipt) {
    assert!(
        matches!(receipt.status, LifecycleState::Succeeded { .. }),
        "{receipt:?}"
    );
}

#[tokio::test]
async fn grants_precede_lookup_and_side_effects_and_service_custody_is_not_reissued() {
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let denied = AdministrativeGrant::new(
        fixture.grant.scope(),
        [fixture.spec.id].into(),
        AdministrativePermissions::default(),
    )
    .unwrap();
    assert_eq!(
        fixture.coordinator.execute(&denied, request.clone()).await,
        Err(AdministrativeError::Unauthorized)
    );
    assert_eq!(
        fixture.coordinator.receipt(&denied, &request).await,
        Err(AdministrativeError::Unauthorized)
    );
    let mut foreign = request.clone();
    foreign.scope = ControlScopeId::new();
    assert_eq!(
        fixture.coordinator.execute(&fixture.grant, foreign).await,
        Err(AdministrativeError::Unauthorized)
    );
    let mut other_agent = request.clone();
    other_agent.action = LifecycleAction::CreateProfile {
        profile: ProfileRef {
            agent_id: AgentId::new(),
            ..fixture.reference
        },
    };
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, other_agent)
            .await,
        Err(AdministrativeError::Unauthorized)
    );
    let legacy = fixture.request(LifecycleAction::Create {
        agent_id: fixture.spec.id,
    });
    assert_eq!(
        fixture.coordinator.execute(&fixture.grant, legacy).await,
        Err(AdministrativeError::Unsupported)
    );
    idle(&fixture.coordinator).await;
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    assert!(!fixture.spec.session.as_ref().unwrap().path.exists());
    assert!(fixture
        .coordinator
        .receipt(&fixture.grant, &request)
        .await
        .unwrap()
        .is_none());
    let profiles = TrustedProfileSet::freeze(vec![]).await.unwrap();
    assert!(matches!(
        AdministrativeCoordinator::new(
            fixture.service.clone(),
            profiles,
            NonZeroUsize::new(1).unwrap()
        ),
        Err(AdministrativeError::Conflict)
    ));
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_create_executes_once_and_full_parameter_conflicts_never_touch_history() {
    let fixture = Fixture::new().await;
    let request = fixture.create();
    let (one, two) = tokio::join!(
        fixture.coordinator.execute(&fixture.grant, request.clone()),
        fixture.coordinator.execute(&fixture.grant, request.clone())
    );
    let receipt = one.unwrap();
    succeeded(&receipt);
    assert_eq!(two.unwrap(), receipt);
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 1);
    assert_eq!(fixture.service.owner_count(), 1);
    let path = &fixture.spec.session.as_ref().unwrap().path;
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request.clone())
            .await
            .unwrap(),
        receipt
    );
    // Simulate a job whose initial None lookup predates the previous job's
    // completed slot: the reserved path must reconcile before profile checks.
    idle(&fixture.coordinator).await;
    assert_eq!(
        fixture.coordinator.run(request.clone()).await.unwrap(),
        receipt
    );
    let mut changed = request;
    changed.action = LifecycleAction::CreateProfile {
        profile: ProfileRef {
            version: ProfileVersion::new(),
            ..fixture.reference
        },
    };
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, changed.clone())
            .await,
        Err(AdministrativeError::Conflict)
    );
    changed.request_id = RequestId::new();
    assert_eq!(
        fixture.coordinator.execute(&fixture.grant, changed).await,
        Err(AdministrativeError::InvalidRequest)
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn profile_lifecycle_and_stop_reconcile_old_versions_after_refreeze_without_loading() {
    let fixture = Fixture::new().await;
    let create = fixture.create();
    let created = fixture
        .coordinator
        .execute(&fixture.grant, create.clone())
        .await
        .unwrap();
    succeeded(&created);
    let loaded = fixture.catalog.get(fixture.spec.id).await.unwrap();
    let close = fixture.request(LifecycleAction::Close {
        agent_id: fixture.spec.id,
        revision: loaded.revision,
    });
    succeeded(
        &fixture
            .coordinator
            .execute(&fixture.grant, close)
            .await
            .unwrap(),
    );
    let closed = fixture.catalog.get(fixture.spec.id).await.unwrap();
    let restore = fixture.request(LifecycleAction::RestoreProfile {
        profile: fixture.reference,
        revision: closed.revision,
    });
    succeeded(
        &fixture
            .coordinator
            .execute(&fixture.grant, restore.clone())
            .await
            .unwrap(),
    );
    let stop = fixture.request(LifecycleAction::Stop {});
    let stopped = fixture
        .coordinator
        .execute(&fixture.grant, stop.clone())
        .await
        .unwrap();
    assert_eq!(
        stopped.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::NativeSettled {}
        }
    );
    assert!(fixture.service.is_closed());
    assert!(fixture.coordinator.stop_completed().is_cancelled());
    idle(&fixture.coordinator).await;
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, stop.clone())
            .await
            .unwrap(),
        stopped
    );
    // Same stale-entry race after Stop closed its worker: use only the bounded
    // cached result whose terminal checkpoint already succeeded, never replay.
    assert_eq!(
        fixture.coordinator.run(stop.clone()).await.unwrap(),
        stopped
    );
    let bytes = std::fs::read(&fixture.spec.session.as_ref().unwrap().path).unwrap();
    let catalog = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let service = HostService::new(catalog.clone());
    let mut replacement = input(fixture.root.path(), "actor-agent");
    replacement.spec = fixture.spec.clone();
    let profiles = TrustedProfileSet::freeze(vec![replacement]).await.unwrap();
    assert_ne!(
        profiles.references().next().unwrap().version,
        fixture.reference.version
    );
    let coordinator =
        AdministrativeCoordinator::new(service.clone(), profiles, NonZeroUsize::new(1).unwrap())
            .unwrap();
    for request in [create.clone(), restore, stop] {
        succeeded(&coordinator.execute(&fixture.grant, request).await.unwrap());
    }
    assert_eq!(
        coordinator.execute(&fixture.grant, create).await.unwrap(),
        created
    );
    assert!(!coordinator.stop_completed().is_cancelled());
    assert!(!service.is_stopping());
    assert_eq!(service.owner_count(), 0);
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    assert_eq!(
        std::fs::read(&fixture.spec.session.as_ref().unwrap().path).unwrap(),
        bytes
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn exclusive_create_partial_failure_is_uncertain_and_duplicate_cannot_replay() {
    let fixture = Fixture::new().await;
    let db = fixture.db();
    db.execute_batch("CREATE TRIGGER reject_actor_registration BEFORE INSERT ON agents BEGIN SELECT RAISE(ABORT,'injected registration failure'); END;").unwrap();
    let request = fixture.create();
    let receipt = fixture
        .coordinator
        .execute(&fixture.grant, request.clone())
        .await
        .unwrap();
    assert!(matches!(receipt.status, LifecycleState::Uncertain { .. }));
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    let path = &fixture.spec.session.as_ref().unwrap().path;
    let before = std::fs::read(path).unwrap();
    assert!(!before.is_empty());
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    drop(db);
    fixture.service.shutdown().await.unwrap();
}

async fn invalid_saved_session(agent: &HostStartupAgent) {
    let spec = &agent.spec;
    let session = spec.session.as_ref().unwrap();
    let storage = JsonlSessionStorage::create_new(
        Arc::new(OsExecutionEnv::with_cwd(spec.launch.cwd.clone())),
        session.path.to_str().unwrap(),
        JsonlV4Header::new(
            session.id.clone(),
            1,
            format!("{}", spec.launch.cwd.display()),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    SessionValueWriter::new(Session::new(Arc::new(storage.clone()), None))
        .set(
            "rpi.resource-selection.v1",
            json!({"version":"invalid-saved-declaration"}),
        )
        .await
        .unwrap();
    storage.drain().await;
    storage.release_writer_lease();
}
#[tokio::test]
async fn readonly_facts_reject_restore_before_starting_and_startup_before_registration() {
    let root = tempfile::tempdir().unwrap();
    let agent = input(root.path(), "invalid-saved");
    invalid_saved_session(&agent).await;
    let spec = agent.spec.clone();
    let before = std::fs::read(&spec.session.as_ref().unwrap().path).unwrap();
    let profiles = TrustedProfileSet::freeze(vec![agent]).await.unwrap();
    let reference = profiles.references().next().unwrap();
    let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let original = catalog.register(spec.clone()).await.unwrap();
    let service = HostService::new(catalog.clone());
    let coordinator =
        AdministrativeCoordinator::new(service.clone(), profiles, NonZeroUsize::new(1).unwrap())
            .unwrap();
    let grant =
        AdministrativeGrant::new(ControlScopeId::new(), [spec.id].into(), permissions()).unwrap();
    let request = LifecycleRequest {
        scope: grant.scope(),
        request_id: RequestId::new(),
        epoch: catalog.epoch(),
        action: LifecycleAction::RestoreProfile {
            profile: reference,
            revision: original.revision,
        },
    };
    assert_eq!(
        coordinator.execute(&grant, request.clone()).await,
        Err(AdministrativeError::InvalidRequest)
    );
    assert_eq!(
        catalog.get(spec.id).await.unwrap().revision,
        original.revision
    );
    assert!(coordinator
        .receipt(&grant, &request)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read(&spec.session.as_ref().unwrap().path).unwrap(),
        before
    );
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();

    let startup = input(root.path(), "invalid-startup");
    invalid_saved_session(&startup).await;
    let path = startup.spec.session.as_ref().unwrap().path.clone();
    let before = std::fs::read(&path).unwrap();
    let result = crate::host::daemon::HostDaemon::open_with_agents(
        root.path().join("startup-host"),
        HostConfig::default(),
        crate::host::server::ControlConfig {
            socket_path: root.path().join("control.sock"),
            host_id: crate::host::protocol::HostId::new(),
            token: crate::host::protocol::AuthToken::new(format!(
                "{}",
                uuid::Uuid::new_v4().simple()
            ))
            .unwrap(),
            max_connections: 1,
            handshake_timeout: DEADLINE,
            request_timeout: DEADLINE,
        },
        vec![startup],
    )
    .await;
    assert!(matches!(
        result,
        Err(crate::host::daemon::DaemonError::StartupAgent)
    ));
    let catalog = HostCatalog::open(root.path().join("startup-host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(catalog.status().await.unwrap().registered, 0);
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(!root.path().join("control.sock").exists());
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_terminal_publication_never_replays_a_loaded_owner_and_restart_only_interrupts() {
    let fixture = Fixture::new().await;
    let db = fixture.db();
    db.execute_batch("CREATE TRIGGER reject_actor_result BEFORE UPDATE ON lifecycle_requests WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected result failure'); END;").unwrap();
    let request = fixture.create();
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request.clone())
            .await,
        Err(AdministrativeError::OutcomeUnknown)
    );
    assert_eq!(fixture.service.owner_count(), 1);
    assert_eq!(
        fixture.service.phase(fixture.spec.id),
        Some(OwnerPhase::Live)
    );
    let bytes = std::fs::read(&fixture.spec.session.as_ref().unwrap().path).unwrap();
    let receipt = fixture
        .coordinator
        .execute(&fixture.grant, request.clone())
        .await
        .unwrap();
    assert_eq!(receipt.status, LifecycleState::Prepared {});
    assert_eq!(fixture.service.owner_count(), 1);
    assert_eq!(
        std::fs::read(&fixture.spec.session.as_ref().unwrap().path).unwrap(),
        bytes
    );
    drop(db);
    fixture.service.shutdown().await.unwrap();
    let catalog = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let service = HostService::new(catalog.clone());
    let coordinator = AdministrativeCoordinator::new(
        service.clone(),
        TrustedProfileSet::freeze(vec![]).await.unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    let receipt = coordinator.execute(&fixture.grant, request).await.unwrap();
    assert_eq!(receipt.status, LifecycleState::Interrupted {});
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_prepare_checkpoint_cannot_execute_or_claim_a_visible_intent() {
    let fixture = Fixture::new().await;
    let reader = fixture.db();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM lifecycle_requests", [], |row| {
            row.get(0)
        })
        .unwrap();
    let request = fixture.create();
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request.clone())
            .await,
        Err(AdministrativeError::OutcomeUnknown)
    );
    assert!(!fixture.spec.session.as_ref().unwrap().path.exists());
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    idle(&fixture.coordinator).await;
    let receipt = fixture
        .coordinator
        .execute(&fixture.grant, request)
        .await
        .unwrap();
    assert_eq!(receipt.status, LifecycleState::Prepared {});
    assert!(!fixture.spec.session.as_ref().unwrap().path.exists());
    assert_eq!(fixture.service.owner_count(), 0);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn administrative_limits_fail_before_claim_and_dropped_facade_never_transfers_epoch_custody()
{
    let root = tempfile::tempdir().unwrap();
    let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let service = HostService::new(catalog);
    let profiles = TrustedProfileSet::freeze(vec![]).await.unwrap();
    assert!(matches!(
        AdministrativeCoordinator::new(
            service.clone(),
            profiles,
            NonZeroUsize::new(MAX_ADMINISTRATIVE_JOBS + 1).unwrap()
        ),
        Err(AdministrativeError::InvalidRequest)
    ));
    let coordinator = AdministrativeCoordinator::new(
        service.clone(),
        TrustedProfileSet::freeze(vec![]).await.unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .unwrap();
    drop(coordinator);
    assert!(matches!(
        AdministrativeCoordinator::new(
            service.clone(),
            TrustedProfileSet::freeze(vec![]).await.unwrap(),
            NonZeroUsize::new(1).unwrap()
        ),
        Err(AdministrativeError::Conflict)
    ));
    let agents = (0..=MAX_TRUSTED_PROFILES).map(|_| AgentId::new()).collect();
    assert!(matches!(
        AdministrativeGrant::new(ControlScopeId::new(), agents, permissions()),
        Err(AdministrativeError::InvalidRequest)
    ));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_result_failure_keeps_drained_catalog_open_until_explicit_shutdown() {
    let fixture = Fixture::new().await;
    succeeded(
        &fixture
            .coordinator
            .execute(&fixture.grant, fixture.create())
            .await
            .unwrap(),
    );
    idle(&fixture.coordinator).await;
    let db = fixture.db();
    db.execute_batch("CREATE TRIGGER reject_actor_stop BEFORE UPDATE ON lifecycle_requests WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected stop publication failure'); END;").unwrap();
    let request = fixture.request(LifecycleAction::Stop {});
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request.clone())
            .await,
        Err(AdministrativeError::OutcomeUnknown)
    );
    assert!(fixture.service.is_stopping());
    assert!(!fixture.service.is_closed());
    assert!(!fixture.coordinator.stop_completed().is_cancelled());
    assert_eq!(fixture.service.owner_count(), 0);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    idle(&fixture.coordinator).await;
    assert_eq!(
        fixture
            .coordinator
            .execute(&fixture.grant, request)
            .await
            .unwrap()
            .status,
        LifecycleState::Prepared {}
    );
    drop(db);
    fixture.service.shutdown().await.unwrap();
}
