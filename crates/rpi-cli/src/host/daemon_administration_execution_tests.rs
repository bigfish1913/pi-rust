//! Real authenticated IPC driving the existing owned/durable native facade.
use super::*;
use crate::host::{
    client::{encode_frame, read_frame, write_frame},
    lifecycle::LifecycleReceipt,
};

fn execute_command(intent: &LifecycleRequest) -> wire::Command {
    let request = serde_json::from_value(json!(intent)).unwrap();
    match intent.action {
        LifecycleAction::CreateProfile { .. } => wire::Command::AdministrativeCreate { request },
        LifecycleAction::RestoreProfile { .. } => wire::Command::AdministrativeRestore { request },
        LifecycleAction::Close { .. } => wire::Command::AdministrativeClose { request },
        LifecycleAction::CancelSubmitted { .. } => wire::Command::AdministrativeCancel { request },
        LifecycleAction::Stop {} => wire::Command::AdministrativeStop { request },
        _ => panic!("not an enabled execution intent"),
    }
}
fn decoded(data: serde_json::Value) -> LifecycleReceipt {
    serde_json::from_value(data["receipt"].clone()).unwrap()
}
async fn revision(client: &mut HostClient, agent: AgentId) -> u64 {
    client
        .call(wire::Command::Inspect {
            agent_id: serde_json::from_value(json!(agent)).unwrap(),
        })
        .await
        .unwrap()["revision"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn ipc_profile_execution_is_guarded_idempotent_and_reconciles_old_versions_before_loading() {
    let root = tempfile::tempdir().unwrap();
    let profile = input(root.path(), "execute");
    let spec = profile.spec.clone();
    let scope = ControlScopeId::new();
    let control = control(root.path());
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control.clone(),
        vec![],
        Some(policy(scope, BTreeSet::from([spec.id]), vec![profile])),
    )
    .await
    .unwrap();
    let reference = daemon
        .administration()
        .unwrap()
        .profile_references()
        .next()
        .unwrap();
    let create = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::CreateProfile { profile: reference },
    );
    let mut client = connect(&control).await;
    let created = decoded(client.call(execute_command(&create)).await.unwrap());
    succeeded(&created);
    assert_eq!(created.request, create);
    assert_eq!(
        decoded(client.call(execute_command(&create)).await.unwrap()),
        created
    );
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        1
    );
    let mut wrong = create.clone();
    wrong.request_id = RequestId::new();
    wrong.scope = ControlScopeId::new();
    assert_eq!(
        client.call(execute_command(&wrong)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Unauthorized)
    );
    wrong = create.clone();
    wrong.request_id = RequestId::new();
    wrong.epoch = crate::host::HostEpoch::new();
    assert_eq!(
        client.call(execute_command(&wrong)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::StaleEpoch)
    );
    let current = revision(&mut client, spec.id).await;
    let bad_close = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::Close {
            agent_id: spec.id,
            revision: current + 1,
        },
    );
    assert_eq!(
        client.call(execute_command(&bad_close)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::StaleRevision)
    );
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        1
    );
    let close = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::Close {
            agent_id: spec.id,
            revision: current,
        },
    );
    let closed = decoded(client.call(execute_command(&close)).await.unwrap());
    succeeded(&closed);
    assert_eq!(
        decoded(client.call(execute_command(&close)).await.unwrap()),
        closed
    );
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        0
    );
    let restore = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::RestoreProfile {
            profile: reference,
            revision: revision(&mut client, spec.id).await,
        },
    );
    succeeded(&decoded(
        client.call(execute_command(&restore)).await.unwrap(),
    ));
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        1
    );
    drop(client);
    daemon.shutdown().await.unwrap();
    let before = std::fs::read(spec.session.as_ref().unwrap().path.clone()).unwrap();
    let mut profile = input(root.path(), "execute");
    profile.spec = spec.clone();
    let next_control = super::control(root.path());
    let mut next = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        next_control.clone(),
        vec![],
        Some(policy(scope, BTreeSet::from([spec.id]), vec![profile])),
    )
    .await
    .unwrap();
    let new_ref = next
        .administration()
        .unwrap()
        .profile_references()
        .next()
        .unwrap();
    assert_ne!(new_ref, reference);
    let mut client = connect(&next_control).await;
    assert_eq!(
        decoded(client.call(execute_command(&create)).await.unwrap()),
        created
    );
    assert_eq!(
        decoded(client.call(execute_command(&restore)).await.unwrap()).request,
        restore
    );
    let mut altered = create.clone();
    altered.action = LifecycleAction::CreateProfile { profile: new_ref };
    assert_eq!(
        client.call(execute_command(&altered)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Conflict)
    );
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        0
    );
    assert_eq!(
        std::fs::read(spec.session.as_ref().unwrap().path.clone()).unwrap(),
        before
    );
    drop(client);
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn ipc_default_grant_denies_all_execution_and_raw_mismatched_stop_never_executes() {
    for all_permissions in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = input(root.path(), "deny");
        let spec = profile.spec.clone();
        let control = control(root.path());
        let mut config = policy(
            ControlScopeId::new(),
            BTreeSet::from([spec.id]),
            vec![profile],
        );
        if !all_permissions {
            config.permissions = AdministrativePermissions::default();
        }
        let mut daemon = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            control.clone(),
            vec![],
            Some(config),
        )
        .await
        .unwrap();
        let admin = daemon.administration().unwrap();
        let reference = admin.profile_references().next().unwrap();
        let mut client = connect(&control).await;
        if !all_permissions {
            for action in [
                LifecycleAction::CreateProfile { profile: reference },
                LifecycleAction::RestoreProfile {
                    profile: reference,
                    revision: 1,
                },
                LifecycleAction::Close {
                    agent_id: spec.id,
                    revision: 1,
                },
                LifecycleAction::CancelSubmitted {
                    agent_id: spec.id,
                    revision: 1,
                    submitted_request_id: RequestId::new(),
                },
                LifecycleAction::Stop {},
            ] {
                let intent = request(&daemon, admin, action);
                assert_eq!(
                    client.call(execute_command(&intent)).await.unwrap_err(),
                    ClientError::Rejected(wire::ErrorCode::Unauthorized)
                );
            }
        }
        // Bypass client.valid: even a policy granting Stop must not execute a
        // Stop smuggled through the administrative_create transport operation.
        let mut raw = tokio::net::UnixStream::connect(&control.socket_path)
            .await
            .unwrap();
        let hello = wire::Hello {
            protocol: wire::PROTOCOL.into(),
            version: wire::Version::default(),
            host_id: control.host_id,
            token: control.token.clone(),
        };
        write_frame(
            &mut raw,
            &encode_frame(&hello, wire::MAX_FRAME_BYTES).unwrap(),
        )
        .await
        .unwrap();
        let welcome: wire::Welcome = read_frame(&mut raw, wire::MAX_FRAME_BYTES).await.unwrap();
        let intent = request(&daemon, admin, LifecycleAction::Stop {});
        let malformed = wire::Request {
            call_id: wire::CallId::new(),
            epoch: welcome.epoch,
            command: wire::Command::AdministrativeCreate {
                request: serde_json::from_value(json!(intent)).unwrap(),
            },
        };
        write_frame(
            &mut raw,
            &encode_frame(&malformed, wire::MAX_FRAME_BYTES).unwrap(),
        )
        .await
        .unwrap();
        let response: wire::Response = read_frame(&mut raw, wire::MAX_FRAME_BYTES).await.unwrap();
        assert!(matches!(
            response.reply,
            wire::Reply::Error {
                code: wire::ErrorCode::InvalidRequest
            }
        ));
        assert_eq!(
            client.call(wire::Command::Status {}).await.unwrap()["registered"],
            0
        );
        assert!(!daemon.service().is_stopping());
        assert_eq!(admin.active_jobs(), 0);
        assert!(!spec.session.unwrap().path.exists());
        for capability in [
            wire::Capability::Create,
            wire::Capability::Restore,
            wire::Capability::Close,
            wire::Capability::Cancel,
            wire::Capability::Stop,
        ] {
            assert!(!welcome.supports(capability));
        }
        drop(raw);
        drop(client);
        daemon.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ipc_deadline_drops_only_waiter_owned_create_keeps_capacity_and_finishes_after_fence() {
    let root = tempfile::tempdir().unwrap();
    let first = input(root.path(), "late-first");
    let first_spec = first.spec.clone();
    let second = input(root.path(), "late-second");
    let second_spec = second.spec.clone();
    let mut control = control(root.path());
    control.request_timeout = Duration::from_millis(500);
    let mut config = policy(
        ControlScopeId::new(),
        BTreeSet::from([first_spec.id, second_spec.id]),
        vec![first, second],
    );
    config.max_jobs = NonZeroUsize::new(1).unwrap();
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control.clone(),
        vec![],
        Some(config),
    )
    .await
    .unwrap();
    let service = daemon.service().clone();
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let fence = tokio::spawn(async move {
        service
            .with_stop_reservation(false, move |reservation| {
                Box::pin(async move {
                    let _ = entered.send(());
                    let _ = released.await;
                    drop(reservation);
                    Ok(())
                })
            })
            .await
            .unwrap();
    });
    held.await.unwrap();
    let admin = daemon.administration().unwrap();
    let refs: BTreeMap<_, _> = admin
        .profile_references()
        .map(|r| (r.agent_id, r))
        .collect();
    let create = request(
        &daemon,
        admin,
        LifecycleAction::CreateProfile {
            profile: refs[&first_spec.id],
        },
    );
    let mut client = connect(&control).await;
    let failure = client.call(execute_command(&create)).await.unwrap_err();
    assert!(
        matches!(failure, ClientError::OutcomeUnknown { request_id, .. } if request_id.to_string() == create.request_id.to_string())
    );
    assert_eq!(admin.active_jobs(), 1);
    assert!(!first_spec.session.as_ref().unwrap().path.exists());
    let mut observer = connect(&control).await;
    let other = request(
        &daemon,
        admin,
        LifecycleAction::CreateProfile {
            profile: refs[&second_spec.id],
        },
    );
    assert_eq!(
        observer.call(execute_command(&other)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Capacity)
    );
    assert_eq!(
        observer.call(receipt_command(&create)).await.unwrap()["found"],
        false
    );
    assert!(!second_spec.session.as_ref().unwrap().path.exists());
    release.send(()).unwrap();
    fence.await.unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let data = observer.call(receipt_command(&create)).await.unwrap();
            if data["found"] == true {
                let receipt = decoded(data);
                if matches!(receipt.status, LifecycleState::Succeeded { .. }) {
                    break receipt;
                }
                assert!(matches!(receipt.status, LifecycleState::Prepared {}));
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        decoded(observer.call(execute_command(&create)).await.unwrap()),
        completed
    );
    assert_eq!(
        observer.call(wire::Command::Status {}).await.unwrap()["resident"],
        1
    );
    assert_eq!(admin.active_jobs(), 0);
    drop(client);
    drop(observer);
    daemon.shutdown().await.unwrap();
}

#[path = "daemon_stop_tests.rs"]
mod stop;
