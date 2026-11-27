use super::*;
use crate::host::{
    administration::AdministrativeError,
    daemon::{DaemonAdministration, DaemonAdministrationConfig, DaemonError, HostDaemon},
    lifecycle::{LifecycleAction, LifecycleRequest, LifecycleState},
    protocol::{AuthToken, HostId},
    server::ControlConfig,
    AgentProfileFile, AgentSpec, HostCatalog, HostConfig, ProfileVersion, RequestId,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

fn input(root: &Path, name: &str) -> HostStartupAgent {
    let root = root.canonicalize().unwrap();
    serde_json::from_value::<AgentProfileFile>(json!({
        "agent": {"id": AgentId::new(), "name": name, "cwd": root,
            "session": {"id": name, "path": root.join(format!("{name}.jsonl"))},
            "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []},
        "runtime": {"providers": {"fixture": {"api": "openai-completions",
            "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "daemon-native-test-key",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]}}},
        "toolEnvironment": {"HOME": root, "PATH": "/usr/bin:/bin"}, "shell": "/bin/sh", "thinking": "off"
    })).unwrap().into_startup().unwrap()
}
fn control(root: &Path) -> ControlConfig {
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    ControlConfig {
        socket_path: root.join("s"),
        host_id: HostId::new(),
        token: AuthToken::new(format!("{}", uuid::Uuid::new_v4().simple())).unwrap(),
        max_connections: 4,
        handshake_timeout: Duration::from_secs(15),
        request_timeout: Duration::from_secs(15),
    }
}
fn policy(
    scope: ControlScopeId,
    allowed: BTreeSet<AgentId>,
    profiles: Vec<HostStartupAgent>,
) -> DaemonAdministrationConfig {
    DaemonAdministrationConfig {
        scope,
        allowed_agents: allowed,
        profiles,
        max_jobs: NonZeroUsize::new(2).unwrap(),
        permissions: AdministrativePermissions {
            create: true,
            restore: true,
            cancel: true,
            close: true,
            stop: true,
        },
    }
}
fn request(
    daemon: &HostDaemon,
    admin: &DaemonAdministration,
    action: LifecycleAction,
) -> LifecycleRequest {
    LifecycleRequest {
        scope: admin.scope(),
        request_id: RequestId::new(),
        epoch: daemon.service().catalog().epoch(),
        action,
    }
}
fn succeeded(receipt: &LifecycleReceipt) {
    assert!(
        matches!(receipt.status, LifecycleState::Succeeded { .. }),
        "{receipt:?}"
    );
}
async fn writer_free(spec: &AgentSpec) {
    let env = OsExecutionEnv::with_cwd(spec.launch.cwd.clone());
    let lease = env
        .try_acquire_writer_lease(spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap();
    assert!(lease.is_some());
}

#[tokio::test]
async fn daemon_owns_fixed_authority_and_freezes_startup_and_dynamic_without_autoload() {
    let root = tempfile::tempdir().unwrap();
    let startup = input(root.path(), "startup");
    let start_spec = startup.spec.clone();
    let dynamic = input(root.path(), "dynamic");
    let dynamic_spec = dynamic.spec.clone();
    let hidden = input(root.path(), "hidden");
    let hidden_spec = hidden.spec.clone();
    let config = policy(
        ControlScopeId::new(),
        [start_spec.id, dynamic_spec.id].into(),
        vec![dynamic, hidden],
    );
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![startup],
        Some(config),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(daemon.welcome()).unwrap()["capabilities"],
        json!([
            "status",
            "list",
            "inspect",
            "receipts",
            "results",
            "submit",
            "profiles",
            "administrative_receipts",
            "administrative_create",
            "administrative_restore",
            "administrative_close",
            "administrative_cancel",
            "administrative_stop"
        ])
    );
    let admin = daemon.administration().unwrap().clone();
    let references: BTreeMap<_, _> = admin
        .profile_references()
        .map(|reference| (reference.agent_id, reference))
        .collect();
    assert_eq!(references.len(), 2);
    assert!(!references.contains_key(&hidden_spec.id));
    assert_eq!(
        daemon
            .service()
            .catalog()
            .status()
            .await
            .unwrap()
            .registered,
        1
    );
    assert!(!dynamic_spec.session.as_ref().unwrap().path.exists());
    assert!(!hidden_spec.session.as_ref().unwrap().path.exists());
    let create = request(
        &daemon,
        &admin,
        LifecycleAction::CreateProfile {
            profile: references[&dynamic_spec.id],
        },
    );
    let mut foreign = create.clone();
    foreign.scope = ControlScopeId::new();
    assert_eq!(
        admin.execute(foreign).await,
        Err(AdministrativeError::Unauthorized)
    );
    succeeded(&admin.execute(create).await.unwrap());
    let record = daemon.service().catalog().get(start_spec.id).await.unwrap();
    succeeded(
        &admin
            .execute(request(
                &daemon,
                &admin,
                LifecycleAction::Close {
                    agent_id: record.spec.id,
                    revision: record.revision,
                },
            ))
            .await
            .unwrap(),
    );
    let record = daemon.service().catalog().get(start_spec.id).await.unwrap();
    succeeded(
        &admin
            .execute(request(
                &daemon,
                &admin,
                LifecycleAction::RestoreProfile {
                    profile: references[&start_spec.id],
                    revision: record.revision,
                },
            ))
            .await
            .unwrap(),
    );
    let denied = request(
        &daemon,
        &admin,
        LifecycleAction::CreateProfile {
            profile: ProfileRef {
                agent_id: hidden_spec.id,
                version: ProfileVersion::new(),
            },
        },
    );
    assert_eq!(
        admin.execute(denied).await,
        Err(AdministrativeError::Unauthorized)
    );
    assert_eq!(daemon.service().owner_count(), 2);
    succeeded(
        &admin
            .execute(request(&daemon, &admin, LifecycleAction::Stop {}))
            .await
            .unwrap(),
    );
    assert!(daemon.service().is_closed());
    // Current-owner completion is independent of the receipt observer. Wait
    // for bounded transport retirement rather than assuming NativeSettled
    // itself is a listener/process-exit proof.
    tokio::time::timeout(std::time::Duration::from_secs(2), daemon.wait_control())
        .await
        .unwrap()
        .unwrap();
    assert!(daemon.control_stopped_by_administration());
    daemon.shutdown().await.unwrap();
    assert!(!root.path().join("s").exists());
}

#[tokio::test]
async fn daemon_refreeze_reconciles_old_scope_request_without_loading_or_rewriting() {
    let root = tempfile::tempdir().unwrap();
    let dynamic = input(root.path(), "historical");
    let spec = dynamic.spec.clone();
    let scope = ControlScopeId::new();
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![],
        Some(policy(scope, [spec.id].into(), vec![dynamic])),
    )
    .await
    .unwrap();
    let admin = daemon.administration().unwrap().clone();
    let old = admin.profile_references().next().unwrap();
    let create = request(
        &daemon,
        &admin,
        LifecycleAction::CreateProfile { profile: old },
    );
    let receipt = admin.execute(create.clone()).await.unwrap();
    succeeded(&receipt);
    daemon.shutdown().await.unwrap();
    let bytes = std::fs::read(&spec.session.as_ref().unwrap().path).unwrap();
    let mut dynamic = input(root.path(), "historical");
    dynamic.spec = spec.clone();
    let mut replacement = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![],
        Some(policy(scope, [spec.id].into(), vec![dynamic])),
    )
    .await
    .unwrap();
    let admin = replacement.administration().unwrap();
    assert_ne!(
        admin.profile_references().next().unwrap().version,
        old.version
    );
    assert_eq!(admin.execute(create.clone()).await.unwrap(), receipt);
    assert_eq!(admin.receipt(&create).await.unwrap().unwrap(), receipt);
    assert_eq!(replacement.service().owner_count(), 0);
    assert_eq!(
        std::fs::read(&spec.session.as_ref().unwrap().path).unwrap(),
        bytes
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_policy_defaults_deny_and_no_configuration_leaves_native_facade_absent() {
    let root = tempfile::tempdir().unwrap();
    let mut daemon = HostDaemon::open(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
    )
    .await
    .unwrap();
    assert!(daemon.administration().is_none());
    daemon.shutdown().await.unwrap();
    let dynamic = input(root.path(), "denied");
    let spec = dynamic.spec.clone();
    let mut config = policy(ControlScopeId::new(), Default::default(), vec![dynamic]);
    config.permissions = AdministrativePermissions::default();
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![],
        Some(config),
    )
    .await
    .unwrap();
    let admin = daemon.administration().unwrap();
    assert_eq!(admin.profile_references().len(), 0);
    let denied = request(
        &daemon,
        admin,
        LifecycleAction::CreateProfile {
            profile: ProfileRef {
                agent_id: spec.id,
                version: ProfileVersion::new(),
            },
        },
    );
    assert_eq!(
        admin.execute(denied.clone()).await,
        Err(AdministrativeError::Unauthorized)
    );
    assert_eq!(
        admin.receipt(&denied).await,
        Err(AdministrativeError::Unauthorized)
    );
    assert!(!spec.session.as_ref().unwrap().path.exists());
    assert_eq!(
        daemon
            .service()
            .catalog()
            .status()
            .await
            .unwrap()
            .registered,
        0
    );
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_rejects_bad_policy_and_cross_set_aliases_before_catalog_or_session_creation() {
    for case in [
        "limit",
        "unknown-grant",
        "agent-id",
        "session-id",
        "path",
        "hardlink",
        "model",
    ] {
        let root = tempfile::tempdir().unwrap();
        let startup = input(root.path(), "union-startup");
        let startup_spec = startup.spec.clone();
        let mut dynamic = input(root.path(), "union-dynamic");
        match case {
            "agent-id" => dynamic.spec.id = startup.spec.id,
            "session-id" => {
                dynamic.spec.session.as_mut().unwrap().id =
                    startup.spec.session.as_ref().unwrap().id.clone()
            }
            "path" => {
                dynamic.spec.session.as_mut().unwrap().path =
                    startup.spec.session.as_ref().unwrap().path.clone()
            }
            "hardlink" => {
                std::fs::write(
                    &startup.spec.session.as_ref().unwrap().path,
                    b"foreign history",
                )
                .unwrap();
                std::fs::hard_link(
                    &startup.spec.session.as_ref().unwrap().path,
                    &dynamic.spec.session.as_ref().unwrap().path,
                )
                .unwrap();
            }
            "model" => dynamic.spec.launch.model.as_mut().unwrap().model = "not-exact".into(),
            _ => {}
        }
        let mut config = policy(
            ControlScopeId::new(),
            [startup.spec.id, dynamic.spec.id].into(),
            vec![dynamic],
        );
        if case == "limit" {
            config.max_jobs = NonZeroUsize::new(MAX_ADMINISTRATIVE_JOBS + 1).unwrap();
        }
        if case == "unknown-grant" {
            config.allowed_agents.insert(AgentId::new());
        }
        let result = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            control(root.path()),
            vec![startup],
            Some(config),
        )
        .await;
        assert!(matches!(result, Err(DaemonError::Administration)), "{case}");
        assert!(!root.path().join("host").exists(), "{case}");
        assert!(!root.path().join("s").exists(), "{case}");
        if case == "hardlink" {
            assert_eq!(
                std::fs::read(&startup_spec.session.as_ref().unwrap().path).unwrap(),
                b"foreign history"
            );
        } else {
            assert!(
                !startup_spec.session.as_ref().unwrap().path.exists(),
                "{case}"
            );
        }
    }
}

#[tokio::test]
async fn daemon_rejects_unwired_mini_before_any_startup_or_catalog_side_effects() {
    for administrative in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let persistent = input(root.path(), "persistent-first");
        let session = persistent.spec.session.as_ref().unwrap().path.clone();
        let mut mini = input(root.path(), "mini-second");
        mini.spec.mode = super::super::AgentMode::Mini;
        mini.spec.session = None;
        let result = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            control(root.path()),
            vec![persistent, mini],
            administrative.then(|| policy(ControlScopeId::new(), BTreeSet::new(), vec![])),
        )
        .await;
        assert!(matches!(result, Err(DaemonError::StartupAgent)));
        assert!(!session.exists());
        assert!(!root.path().join("host").exists());
        assert!(!root.path().join("s").exists());
    }
    // A dynamic-only Mini must also fail closed during trusted-profile freeze.
    let root = tempfile::tempdir().unwrap();
    let mut mini = input(root.path(), "mini-dynamic");
    mini.spec.mode = super::super::AgentMode::Mini;
    mini.spec.session = None;
    let result = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![],
        Some(policy(
            ControlScopeId::new(),
            [mini.spec.id].into(),
            vec![mini],
        )),
    )
    .await;
    assert!(matches!(result, Err(DaemonError::Administration)));
    assert!(!root.path().join("host").exists());
    assert!(!root.path().join("s").exists());
    assert!(!root.path().join("mini-dynamic.jsonl").exists());
}

#[tokio::test]
async fn daemon_configured_bind_failure_settles_startup_and_preserves_foreign_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let startup = input(root.path(), "bind-startup");
    let spec = startup.spec.clone();
    let dynamic = input(root.path(), "bind-dynamic");
    let dynamic_spec = dynamic.spec.clone();
    let control = control(root.path());
    std::fs::write(&control.socket_path, b"foreign endpoint").unwrap();
    let result = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control,
        vec![startup],
        Some(policy(
            ControlScopeId::new(),
            [spec.id, dynamic_spec.id].into(),
            vec![dynamic],
        )),
    )
    .await;
    assert!(matches!(result, Err(DaemonError::Control(_))));
    assert_eq!(
        std::fs::read(root.path().join("s")).unwrap(),
        b"foreign endpoint"
    );
    assert!(!dynamic_spec.session.as_ref().unwrap().path.exists());
    writer_free(&spec).await;
    let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    assert_eq!(catalog.status().await.unwrap().registered, 1);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_configured_saved_facts_failure_settles_earlier_startup_without_registering_bad_history(
) {
    let root = tempfile::tempdir().unwrap();
    let startup = input(root.path(), "good-first");
    let spec = startup.spec.clone();
    let bad = input(root.path(), "bad-second");
    let bad_spec = bad.spec.clone();
    std::fs::write(
        &bad.spec.session.as_ref().unwrap().path,
        b"invalid history must not be repaired",
    )
    .unwrap();
    let result = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![startup, bad],
        Some(policy(
            ControlScopeId::new(),
            [spec.id, bad_spec.id].into(),
            vec![],
        )),
    )
    .await;
    assert!(matches!(result, Err(DaemonError::StartupAgent)));
    assert!(!root.path().join("s").exists());
    assert_eq!(
        std::fs::read(&bad_spec.session.as_ref().unwrap().path).unwrap(),
        b"invalid history must not be repaired"
    );
    writer_free(&spec).await;
    writer_free(&bad_spec).await;
    let catalog = HostCatalog::open(root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(catalog.status().await.unwrap().registered, 1);
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    assert!(matches!(
        catalog.get(bad_spec.id).await,
        Err(super::super::CatalogError::UnknownAgent)
    ));
    catalog.shutdown().await.unwrap();
}

#[path = "daemon_administration_query_tests.rs"]
mod queries;
#[path = "daemon_submitted_cancel_tests.rs"]
mod submitted_cancel;
