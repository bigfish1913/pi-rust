use super::*;
use crate::host::{
    client::{ClientError, ConnectionConfig, HostClient},
    registry_tests::make_spec,
    AgentMode, HostCatalog, HostConfig,
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const DEADLINE: Duration = Duration::from_secs(5);
struct Fixture {
    _root: tempfile::TempDir,
    config: ControlConfig,
    service: HostService,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::Builder::new().prefix("hs").tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = HostCatalog::open(root.path().join("d"), HostConfig::default())
            .await
            .unwrap();
        let config = ControlConfig {
            socket_path: root.path().join("s"),
            host_id: HostId::new(),
            token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            max_connections: 8,
            handshake_timeout: DEADLINE,
            request_timeout: DEADLINE,
        };
        Self {
            _root: root,
            config,
            service: HostService::new(catalog),
        }
    }
    fn client(&self) -> ConnectionConfig {
        ConnectionConfig {
            socket_path: self.config.socket_path.clone(),
            host_id: self.config.host_id,
            token: self.config.token.clone(),
            timeout: DEADLINE,
        }
    }
    async fn start(&self) -> ControlServer {
        ControlServer::bind(self.config.clone(), self.service.clone())
            .await
            .unwrap()
    }
    async fn raw(&self) -> (UnixStream, Welcome) {
        let mut stream = UnixStream::connect(&self.config.socket_path).await.unwrap();
        send(
            &mut stream,
            &Hello {
                protocol: PROTOCOL.into(),
                version: Version::default(),
                host_id: self.config.host_id,
                token: self.config.token.clone(),
            },
        )
        .await;
        let welcome = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
        (stream, welcome)
    }
}
async fn send(stream: &mut UnixStream, value: &impl serde::Serialize) {
    write_frame(stream, &encode_frame(value, MAX_FRAME_BYTES).unwrap())
        .await
        .unwrap();
}
async fn response(stream: &mut UnixStream, epoch: Epoch, command: Command) -> Response {
    let request = Request {
        call_id: CallId::new(),
        epoch,
        command,
    };
    send(stream, &request).await;
    let response: Response = tokio::time::timeout(DEADLINE, read_frame(stream, MAX_FRAME_BYTES))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.call_id, request.call_id);
    response
}

#[tokio::test(flavor = "current_thread")]
async fn real_catalog_queries_paginate_and_detach_without_changing_ownership() {
    let fixture = Fixture::new().await;
    let first = fixture
        .service
        .catalog()
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    fixture
        .service
        .catalog()
        .register(make_spec(AgentMode::Mini))
        .await
        .unwrap();
    let mut server = fixture.start().await;
    assert_eq!(
        std::fs::metadata(&fixture.config.socket_path)
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    let mut client = HostClient::connect(fixture.client()).await.unwrap();
    assert_eq!(
        client.welcome().capabilities,
        vec![
            Capability::Status,
            Capability::List,
            Capability::Inspect,
            Capability::Receipts,
            Capability::Results,
            Capability::Submit
        ]
    );
    assert_eq!(client.welcome().limits.max_in_flight, 1);
    assert_eq!(
        client.call(Command::Status {}).await.unwrap()["registered"],
        2
    );
    let page = client
        .call(Command::List {
            after: None,
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(page["agents"].as_array().unwrap().len(), 1);
    assert!(page["agents"][0].get("launch").is_none());
    let after: AgentId = serde_json::from_value(page["next_cursor"].clone()).unwrap();
    let second = client
        .call(Command::List {
            after: Some(after),
            limit: 1,
        })
        .await
        .unwrap();
    assert_ne!(page["agents"][0]["id"], second["agents"][0]["id"]);
    assert!(second["next_cursor"].is_null());
    let inspected = client
        .call(Command::Inspect {
            agent_id: first.spec.id.to_string().try_into().unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(inspected, serde_json::to_value(&first).unwrap());
    assert_eq!(
        client
            .call(Command::Inspect {
                agent_id: AgentId::new()
            })
            .await
            .unwrap_err(),
        ClientError::Rejected(ErrorCode::NotFound)
    );
    client.detach();
    drop(client);
    assert_eq!(
        fixture.service.catalog().status().await.unwrap().registered,
        2
    );
    server.stop().await.unwrap();
    assert!(!fixture.config.socket_path.exists());
    // IPC stop is detach, NOT service shutdown or catalog close.
    assert!(!fixture.service.is_closed());
    assert_eq!(
        fixture.service.catalog().status().await.unwrap().registered,
        2
    );
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_mutations_and_stale_epochs_never_reach_catalog_mutation() {
    let fixture = Fixture::new().await;
    let record = fixture
        .service
        .catalog()
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let mut server = fixture.start().await;
    let (mut stream, welcome) = fixture.raw().await;
    let mutation = AgentMutation {
        agent_id: record.spec.id.to_string().try_into().unwrap(),
        request_id: RequestId::new(),
        revision: record.revision,
    };
    for command in [
        Command::Close { mutation },
        Command::Restore { mutation },
        Command::Cancel {
            mutation,
            ticket_id: TicketId::new(),
        },
        Command::Stop {
            request_id: RequestId::new(),
        },
    ] {
        assert!(matches!(
            response(&mut stream, welcome.epoch, command).await.reply,
            Reply::Error {
                code: ErrorCode::Unsupported
            }
        ));
    }
    // Submit is dispatched but bounded: invalid prompts never reach a backend,
    // and an unloaded Agent is NotFound without admitting anything.
    assert!(matches!(
        response(
            &mut stream,
            welcome.epoch,
            Command::Submit {
                agent_id: mutation.agent_id,
                request_id: RequestId::new(),
                prompt: "  \n ".into(),
            }
        )
        .await
        .reply,
        Reply::Error {
            code: ErrorCode::InvalidRequest
        }
    ));
    let submit_request = RequestId::new();
    assert!(matches!(
        response(
            &mut stream,
            welcome.epoch,
            Command::Submit {
                agent_id: mutation.agent_id,
                request_id: submit_request,
                prompt: "never admitted".into(),
            }
        )
        .await
        .reply,
        Reply::Error {
            code: ErrorCode::NotFound
        }
    ));
    assert!(fixture
        .service
        .admission(record.spec.id, &submit_request.to_string())
        .await
        .is_err());
    // Receipts are a validated read: an unloaded Agent is NotFound and the
    // registered record is not touched.
    assert!(matches!(
        response(
            &mut stream,
            welcome.epoch,
            Command::Receipt {
                agent_id: mutation.agent_id,
                request_id: mutation.request_id,
            }
        )
        .await
        .reply,
        Reply::Error {
            code: ErrorCode::NotFound
        }
    ));
    assert_eq!(
        fixture
            .service
            .catalog()
            .get(record.spec.id)
            .await
            .unwrap()
            .revision,
        record.revision
    );
    assert!(matches!(
        response(&mut stream, Epoch::new(), Command::Status {})
            .await
            .reply,
        Reply::Error {
            code: ErrorCode::StaleEpoch
        }
    ));
    assert!(matches!(
        response(
            &mut stream,
            welcome.epoch,
            Command::List {
                after: None,
                limit: 129
            }
        )
        .await
        .reply,
        Reply::Error {
            code: ErrorCode::InvalidRequest
        }
    ));
    assert_eq!(
        fixture.service.catalog().get(record.spec.id).await.unwrap(),
        record
    );
    assert_eq!(fixture.service.owner_count(), 0);
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn authentication_protocol_and_host_identity_fail_closed_without_a_welcome() {
    let fixture = Fixture::new().await;
    let mut server = fixture.start().await;
    for case in 0..3 {
        let mut stream = UnixStream::connect(&fixture.config.socket_path)
            .await
            .unwrap();
        let mut hello = Hello {
            protocol: PROTOCOL.into(),
            version: Version::default(),
            host_id: fixture.config.host_id,
            token: fixture.config.token.clone(),
        };
        match case {
            0 => hello.token = AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            1 => hello.host_id = HostId::new(),
            _ => hello.version.major += 1,
        }
        send(&mut stream, &hello).await;
        assert!(tokio::time::timeout(
            DEADLINE,
            read_frame::<_, Welcome>(&mut stream, MAX_FRAME_BYTES)
        )
        .await
        .unwrap()
        .is_err());
    }
    let mut client = HostClient::connect(fixture.client()).await.unwrap();
    assert_eq!(
        client.call(Command::Status {}).await.unwrap()["registered"],
        0
    );
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_headers_malformed_frames_and_slow_handshakes_are_bounded() {
    let mut fixture = Fixture::new().await;
    fixture.config.handshake_timeout = Duration::from_millis(100);
    let mut server = fixture.start().await;
    let mut oversized = UnixStream::connect(&fixture.config.socket_path)
        .await
        .unwrap();
    oversized
        .write_u32((MAX_FRAME_BYTES + 1) as u32)
        .await
        .unwrap();
    assert!(tokio::time::timeout(DEADLINE, oversized.read_u32())
        .await
        .unwrap()
        .is_err());
    let mut malformed = UnixStream::connect(&fixture.config.socket_path)
        .await
        .unwrap();
    write_frame(&mut malformed, b"not-json").await.unwrap();
    assert!(tokio::time::timeout(DEADLINE, malformed.read_u32())
        .await
        .unwrap()
        .is_err());
    let mut slow = UnixStream::connect(&fixture.config.socket_path)
        .await
        .unwrap();
    slow.write_u32(100).await.unwrap();
    slow.write_all(b"{").await.unwrap();
    // A different connection is never blocked by this partial frame.
    let mut client = HostClient::connect(fixture.client()).await.unwrap();
    client.call(Command::Status {}).await.unwrap();
    assert!(tokio::time::timeout(DEADLINE, slow.read_u32())
        .await
        .unwrap()
        .is_err());
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_cap_and_idle_deadline_release_slots_without_unbounded_tasks() {
    let mut fixture = Fixture::new().await;
    fixture.config.max_connections = 1;
    fixture.config.request_timeout = Duration::from_millis(100);
    let mut server = fixture.start().await;
    let (mut idle, _) = fixture.raw().await;
    // Handshake receipt proves that the first task holds the only slot.
    let mut excess = UnixStream::connect(&fixture.config.socket_path)
        .await
        .unwrap();
    assert!(tokio::time::timeout(DEADLINE, excess.read_u32())
        .await
        .unwrap()
        .is_err());
    assert!(tokio::time::timeout(DEADLINE, idle.read_u32())
        .await
        .unwrap()
        .is_err());
    // Wait for the completed task to be reaped (not a fixed race-prone sleep).
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(mut client) = HostClient::connect(fixture.client()).await {
                if client.call(Command::Status {}).await.is_ok() {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_closes_idle_clients_and_abandoned_wait_does_not_lose_cleanup() {
    let fixture = Fixture::new().await;
    let mut server = fixture.start().await;
    let (mut stream, _) = fixture.raw().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), server.wait())
            .await
            .is_err()
    );
    server.stop().await.unwrap();
    server.stop().await.unwrap();
    assert!(stream.read_u32().await.is_err());
    assert!(!fixture.config.socket_path.exists());
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsafe_parent_and_existing_files_or_sockets_are_never_repaired_or_removed() {
    let fixture = Fixture::new().await;
    std::fs::set_permissions(fixture._root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ControlServer::bind(fixture.config.clone(), fixture.service.clone()).await,
        Err(ServerError::UnsafeEndpoint)
    ));
    std::fs::set_permissions(fixture._root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&fixture.config.socket_path, b"keep-this-file").unwrap();
    assert!(
        ControlServer::bind(fixture.config.clone(), fixture.service.clone())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(&fixture.config.socket_path).unwrap(),
        b"keep-this-file"
    );
    std::fs::remove_file(&fixture.config.socket_path).unwrap();
    let mut server = fixture.start().await;
    let inode = std::fs::metadata(&fixture.config.socket_path)
        .unwrap()
        .ino();
    assert!(
        ControlServer::bind(fixture.config.clone(), fixture.service.clone())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::metadata(&fixture.config.socket_path)
            .unwrap()
            .ino(),
        inode
    );
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_never_unlinks_a_replacement_endpoint() {
    let fixture = Fixture::new().await;
    let mut server = fixture.start().await;
    std::fs::remove_file(&fixture.config.socket_path).unwrap();
    std::fs::write(&fixture.config.socket_path, b"replacement").unwrap();
    server.stop().await.unwrap();
    assert_eq!(
        std::fs::read(&fixture.config.socket_path).unwrap(),
        b"replacement"
    );
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_options_do_not_create_endpoints_and_errors_never_echo_secrets() {
    let fixture = Fixture::new().await;
    for case in 0..3 {
        let mut config = fixture.config.clone();
        match case {
            0 => config.max_connections = 65,
            1 => config.handshake_timeout = Duration::ZERO,
            _ => config.request_timeout = Duration::from_secs(301),
        }
        let error = ControlServer::bind(config, fixture.service.clone())
            .await
            .err()
            .unwrap();
        assert_eq!(error, ServerError::InvalidConfiguration);
        assert!(!error
            .to_string()
            .contains(&String::from(fixture.config.token.clone())));
        assert!(!fixture.config.socket_path.exists());
    }
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn slow_response_consumer_cannot_block_other_clients_and_is_disconnected_on_deadline() {
    let mut fixture = Fixture::new().await;
    fixture.config.request_timeout = Duration::from_millis(100);
    let mut spec = make_spec(AgentMode::Persistent);
    spec.launch.tools = vec!["\"".repeat(128); 256];
    let record = fixture.service.catalog().register(spec).await.unwrap();
    let mut server = fixture.start().await;
    let (mut slow, welcome) = fixture.raw().await;
    for _ in 0..8 {
        send(
            &mut slow,
            &Request {
                call_id: CallId::new(),
                epoch: welcome.epoch,
                command: Command::Inspect {
                    agent_id: record.spec.id.to_string().try_into().unwrap(),
                },
            },
        )
        .await;
    }
    // Deliberately never read this connection's large replies. Another client's
    // status roundtrip must still complete on this same runtime thread.
    let mut client = HostClient::connect(fixture.client()).await.unwrap();
    assert_eq!(
        client.call(Command::Status {}).await.unwrap()["registered"],
        1
    );
    tokio::time::timeout(DEADLINE, async {
        loop {
            match slow.try_write(&[0]) {
                Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    server.stop().await.unwrap();
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_bind_failure_releases_its_catalog_and_unsupported_timer_policy_has_no_side_effects()
{
    use crate::host::daemon::{DaemonError, HostDaemon};
    let fixture = Fixture::new().await;
    let directory = fixture._root.path().join("other");
    std::fs::write(&fixture.config.socket_path, b"keep").unwrap();
    assert!(matches!(
        HostDaemon::open(
            directory.clone(),
            HostConfig::default(),
            fixture.config.clone()
        )
        .await,
        Err(DaemonError::Control(ServerError::UnsafeEndpoint))
    ));
    let reopened = HostCatalog::open(directory, HostConfig::default())
        .await
        .unwrap();
    reopened.shutdown().await.unwrap();
    assert_eq!(std::fs::read(&fixture.config.socket_path).unwrap(), b"keep");
    let directory = fixture._root.path().join("must-not-create");
    let config = HostConfig {
        default_timeouts: crate::host::Timeouts {
            disconnected_ms: std::num::NonZeroU64::new(1),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(matches!(
        HostDaemon::open(directory.clone(), config, fixture.config.clone()).await,
        Err(DaemonError::UnsupportedTimers)
    ));
    assert!(!directory.exists());
    fixture.service.shutdown().await.unwrap();
}
