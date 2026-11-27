//! Current-owner Stop completion, bounded response drain and historical isolation.
use super::*;
use crate::host::lifecycle::LifecycleResult;

async fn retired(daemon: &mut HostDaemon) {
    tokio::time::timeout(Duration::from_secs(3), daemon.wait_control())
        .await
        .unwrap()
        .unwrap();
    assert!(daemon.control_stopped_by_administration());
    assert!(daemon.service().is_closed());
}
fn native_settled(receipt: &LifecycleReceipt, intent: &LifecycleRequest) {
    assert_eq!(&receipt.request, intent);
    assert_eq!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::NativeSettled {},
        }
    );
}

#[tokio::test]
async fn ipc_stop_flushes_original_reply_retires_idle_peers_and_old_history_cannot_stop_restart() {
    let root = tempfile::tempdir().unwrap();
    let profile = input(root.path(), "stop-original");
    let spec = profile.spec.clone();
    let scope = ControlScopeId::new();
    let mut config = control(root.path());
    config.max_connections = 8;
    // Idle peers must not extend retirement to the ordinary 15-second timeout.
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        config.clone(),
        vec![profile],
        Some(policy(scope, BTreeSet::from([spec.id]), vec![])),
    )
    .await
    .unwrap();
    let mut client = connect(&config).await;
    let _idle = connect(&config).await;
    let _half_handshake = tokio::net::UnixStream::connect(&config.socket_path)
        .await
        .unwrap();
    let intent = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::Stop {},
    );
    let mut wrong = intent.clone();
    wrong.epoch = crate::host::HostEpoch::new();
    assert_eq!(
        client.call(execute_command(&wrong)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::StaleEpoch)
    );
    wrong = intent.clone();
    wrong.scope = ControlScopeId::new();
    assert_eq!(
        client.call(execute_command(&wrong)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Unauthorized)
    );
    assert!(!daemon.service().is_stopping());
    let stopped = decoded(client.call(execute_command(&intent)).await.unwrap());
    native_settled(&stopped, &intent);
    retired(&mut daemon).await;
    assert!(!config.socket_path.exists());
    writer_free(&spec).await;
    daemon.shutdown().await.unwrap();
    let bytes = std::fs::read(&spec.session.as_ref().unwrap().path).unwrap();
    let next_config = control(root.path());
    let mut next = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        next_config.clone(),
        vec![],
        Some(policy(scope, BTreeSet::new(), vec![])),
    )
    .await
    .unwrap();
    let mut peer = connect(&next_config).await;
    assert_ne!(peer.welcome().epoch, client.welcome().epoch);
    for _ in 0..3 {
        assert_eq!(
            decoded(peer.call(receipt_command(&intent)).await.unwrap()),
            stopped
        );
        assert_eq!(
            decoded(peer.call(execute_command(&intent)).await.unwrap()),
            stopped
        );
        assert_eq!(
            peer.call(wire::Command::Status {}).await.unwrap()["resident"],
            0
        );
        assert!(!next.service().is_stopping());
    }
    assert_eq!(
        std::fs::read(&spec.session.as_ref().unwrap().path).unwrap(),
        bytes
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), next.wait_control())
            .await
            .is_err()
    );
    next.shutdown().await.unwrap();
    assert!(!next.control_stopped_by_administration());
}

#[tokio::test]
async fn ipc_stop_lost_waiter_keeps_owned_job_and_retires_without_a_reply_observer() {
    for disconnect in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut config = control(root.path());
        config.request_timeout = Duration::from_millis(300);
        let scope = ControlScopeId::new();
        let mut authority = policy(scope, BTreeSet::new(), vec![]);
        authority.max_jobs = NonZeroUsize::new(1).unwrap();
        let mut daemon = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            config.clone(),
            vec![],
            Some(authority),
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
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        drop(reservation);
                        Ok(())
                    })
                })
                .await
                .unwrap();
        });
        held.await.unwrap();
        let admin = daemon.administration().unwrap().clone();
        let intent = request(&daemon, &admin, LifecycleAction::Stop {});
        let mut client = connect(&config).await;
        if disconnect {
            let mut raw = tokio::net::UnixStream::connect(&config.socket_path)
                .await
                .unwrap();
            write_frame(
                &mut raw,
                &encode_frame(
                    &wire::Hello {
                        protocol: wire::PROTOCOL.into(),
                        version: wire::Version::default(),
                        host_id: config.host_id,
                        token: config.token.clone(),
                    },
                    wire::MAX_FRAME_BYTES,
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let welcome: wire::Welcome = read_frame(&mut raw, wire::MAX_FRAME_BYTES).await.unwrap();
            write_frame(
                &mut raw,
                &encode_frame(
                    &wire::Request {
                        call_id: wire::CallId::new(),
                        epoch: welcome.epoch,
                        command: execute_command(&intent),
                    },
                    wire::MAX_FRAME_BYTES,
                )
                .unwrap(),
            )
            .await
            .unwrap();
            drop(raw);
            tokio::time::timeout(Duration::from_secs(3), async {
                while admin.active_jobs() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            assert!(
                matches!(client.call(execute_command(&intent)).await.unwrap_err(),
                ClientError::OutcomeUnknown { request_id, .. } if request_id.to_string() == intent.request_id.to_string())
            );
            client = connect(&config).await;
        }
        assert_eq!(admin.active_jobs(), 1);
        assert!(!daemon.service().is_stopping());
        assert_eq!(
            client.call(receipt_command(&intent)).await.unwrap()["found"],
            false
        );
        let other = request(&daemon, &admin, LifecycleAction::Stop {});
        assert_eq!(
            client.call(execute_command(&other)).await.unwrap_err(),
            ClientError::Rejected(wire::ErrorCode::Capacity)
        );
        release.send(()).unwrap();
        fence.await.unwrap();
        retired(&mut daemon).await;
        native_settled(&admin.receipt(&intent).await.unwrap().unwrap(), &intent);
        daemon.shutdown().await.unwrap();
        let next_config = control(root.path());
        let mut next = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            next_config.clone(),
            vec![],
            Some(policy(scope, BTreeSet::new(), vec![])),
        )
        .await
        .unwrap();
        let mut peer = connect(&next_config).await;
        native_settled(
            &decoded(peer.call(execute_command(&intent)).await.unwrap()),
            &intent,
        );
        assert_eq!(
            peer.call(wire::Command::Status {}).await.unwrap()["stopping"],
            false
        );
        next.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ipc_stop_publication_failure_keeps_catalog_and_listener_for_original_intent_reconciliation(
) {
    let root = tempfile::tempdir().unwrap();
    let config = control(root.path());
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        config.clone(),
        vec![],
        Some(policy(ControlScopeId::new(), BTreeSet::new(), vec![])),
    )
    .await
    .unwrap();
    let db = rusqlite::Connection::open(root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_stop_reply BEFORE UPDATE ON lifecycle_requests WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected stop publication failure'); END;").unwrap();
    let intent = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::Stop {},
    );
    let mut client = connect(&config).await;
    assert_eq!(
        client.call(execute_command(&intent)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::OutcomeUnknown)
    );
    assert!(daemon.service().is_stopping());
    assert!(!daemon.service().is_closed());
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["stopping"],
        true
    );
    assert_eq!(
        decoded(client.call(receipt_command(&intent)).await.unwrap()).status,
        LifecycleState::Prepared {}
    );
    assert_eq!(
        decoded(client.call(execute_command(&intent)).await.unwrap()).status,
        LifecycleState::Prepared {}
    );
    assert!(config.socket_path.exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), daemon.wait_control())
            .await
            .is_err()
    );
    assert!(!daemon.control_stopped_by_administration());
    drop(db);
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn ipc_stop_response_drain_is_bounded_even_when_another_peer_never_reads_large_replies() {
    let root = tempfile::tempdir().unwrap();
    let mut config = control(root.path());
    config.request_timeout = Duration::from_millis(300);
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        config.clone(),
        vec![],
        Some(policy(ControlScopeId::new(), BTreeSet::new(), vec![])),
    )
    .await
    .unwrap();
    let mut spec = input(root.path(), "slow-stop-peer").spec;
    spec.launch.tools = vec!["\"".repeat(128); 256];
    let agent = spec.id;
    daemon.service().catalog().register(spec).await.unwrap();
    let mut raw = tokio::net::UnixStream::connect(&config.socket_path)
        .await
        .unwrap();
    write_frame(
        &mut raw,
        &encode_frame(
            &wire::Hello {
                protocol: wire::PROTOCOL.into(),
                version: wire::Version::default(),
                host_id: config.host_id,
                token: config.token.clone(),
            },
            wire::MAX_FRAME_BYTES,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let welcome: wire::Welcome = read_frame(&mut raw, wire::MAX_FRAME_BYTES).await.unwrap();
    for _ in 0..8 {
        write_frame(
            &mut raw,
            &encode_frame(
                &wire::Request {
                    call_id: wire::CallId::new(),
                    epoch: welcome.epoch,
                    command: wire::Command::Inspect {
                        agent_id: serde_json::from_value(json!(agent)).unwrap(),
                    },
                },
                wire::MAX_FRAME_BYTES,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    // Deliberately do not consume raw's large replies. Stop on a separate
    // connection must still receive its own full reply and retire boundedly.
    let mut client = connect(&config).await;
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["registered"],
        1
    );
    let intent = request(
        &daemon,
        daemon.administration().unwrap(),
        LifecycleAction::Stop {},
    );
    native_settled(
        &decoded(client.call(execute_command(&intent)).await.unwrap()),
        &intent,
    );
    retired(&mut daemon).await;
    assert!(!config.socket_path.exists());
    daemon.shutdown().await.unwrap();
}
