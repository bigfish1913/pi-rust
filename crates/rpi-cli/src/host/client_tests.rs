//! Real Unix sockets with bounded synthetic peers. These tests qualify the
//! client/contract only, not a production Host service or Agent execution.
use super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use tokio::net::{UnixListener, UnixStream};

struct Endpoint {
    _directory: tempfile::TempDir,
    config: ConnectionConfig,
    listener: UnixListener,
}
impl Endpoint {
    fn new() -> Self {
        let directory = tempfile::Builder::new().prefix("hc").tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = ConnectionConfig {
            socket_path: path,
            host_id: HostId::new(),
            token: AuthToken::new("fixture-only-auth-token-0123456789".into()).unwrap(),
            timeout: Duration::from_secs(2),
        };
        Self {
            _directory: directory,
            config,
            listener,
        }
    }
}
fn welcome(config: &ConnectionConfig) -> Welcome {
    Welcome {
        protocol: PROTOCOL.into(),
        version: Version::default(),
        host_id: config.host_id,
        epoch: Epoch::new(),
        capabilities: vec![
            Capability::Status,
            Capability::List,
            Capability::Inspect,
            Capability::Receipts,
            Capability::Cancel,
            Capability::Close,
        ],
        limits: Limits::default(),
    }
}
async fn authenticate(stream: &mut UnixStream, config: &ConnectionConfig, ready: &Welcome) {
    let hello: Hello = read_frame(stream, MAX_FRAME_BYTES).await.unwrap();
    assert_eq!(hello.protocol, PROTOCOL);
    assert_eq!(hello.version.major, MAJOR);
    assert_eq!(hello.host_id, config.host_id);
    assert!(hello.token.matches(&config.token));
    send(stream, ready).await;
}
async fn send(stream: &mut UnixStream, value: &impl Serialize) {
    write_frame(stream, &encode_frame(value, MAX_FRAME_BYTES).unwrap())
        .await
        .unwrap();
}
fn close_command() -> (RequestId, AgentId, Command) {
    let id = RequestId::new();
    let agent_id = AgentId::new();
    (
        id,
        agent_id,
        Command::Close {
            mutation: AgentMutation {
                request_id: id,
                agent_id,
                revision: 2,
            },
        },
    )
}

#[tokio::test]
async fn authenticated_serial_queries_and_remote_rejections_preserve_correlation() {
    let endpoint = Endpoint::new();
    let config = endpoint.config.clone();
    let client_config = config.clone();
    let ready = welcome(&config);
    let epoch = ready.epoch;
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut stream, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut stream, &config, &ready).await;
        for index in 0..3 {
            let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
            assert_eq!(request.epoch, epoch);
            let reply = if index == 1 {
                Reply::Error {
                    code: ErrorCode::Busy,
                }
            } else {
                Reply::Data {
                    data: json!({"sequence":index}),
                }
            };
            send(
                &mut stream,
                &Response {
                    call_id: request.call_id,
                    epoch,
                    reply,
                },
            )
            .await;
        }
        // Client detach is EOF, NOT stop/close/cancel.
        assert!(read_frame::<_, Request>(&mut stream, MAX_FRAME_BYTES)
            .await
            .is_err());
    });
    let mut client = HostClient::connect(client_config).await.unwrap();
    assert_eq!(
        client.call(Command::Status {}).await.unwrap(),
        json!({"sequence":0})
    );
    assert_eq!(
        client
            .call(Command::List {
                after: None,
                limit: 2
            })
            .await
            .unwrap_err(),
        ClientError::Rejected(ErrorCode::Busy)
    );
    assert_eq!(
        client
            .call(Command::Inspect {
                agent_id: AgentId::new()
            })
            .await
            .unwrap(),
        json!({"sequence":2})
    );
    client.detach();
    assert_eq!(
        client.call(Command::Status {}).await.unwrap_err(),
        ClientError::NotSent(Failure::Closed)
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn incompatible_identity_protocol_major_and_limits_fail_before_commands() {
    for variant in 0..5 {
        let endpoint = Endpoint::new();
        let config = endpoint.config.clone();
        let client_config = config.clone();
        let mut ready = welcome(&config);
        match variant {
            0 => ready.protocol = "native-rpc-is-not-shared-host".into(),
            1 => ready.version.major += 1,
            2 => ready.host_id = HostId::new(),
            3 => ready.limits.max_frame_bytes = MAX_FRAME_BYTES + 1,
            _ => ready.limits.max_in_flight = 0,
        }
        let peer = tokio::spawn(async move {
            let _directory = endpoint._directory;
            let (mut stream, _) = endpoint.listener.accept().await.unwrap();
            authenticate(&mut stream, &config, &ready).await;
            assert!(read_frame::<_, Request>(&mut stream, MAX_FRAME_BYTES)
                .await
                .is_err());
        });
        assert_eq!(
            HostClient::connect(client_config).await.unwrap_err(),
            ClientError::NotSent(Failure::IncompatiblePeer)
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn capabilities_page_and_revision_validation_do_not_send_and_keep_connection_usable() {
    let endpoint = Endpoint::new();
    let config = endpoint.config.clone();
    let client_config = config.clone();
    let mut ready = welcome(&config);
    ready.version.minor = u16::MAX;
    ready.limits.max_page_size = 2;
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut stream, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut stream, &config, &ready).await;
        let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
        assert!(matches!(request.command, Command::Status {}));
        send(
            &mut stream,
            &Response {
                call_id: request.call_id,
                epoch: ready.epoch,
                reply: Reply::Data {
                    data: json!({"ok":true}),
                },
            },
        )
        .await;
    });
    let mut client = HostClient::connect(client_config).await.unwrap();
    for limit in [0, 3, usize::MAX] {
        assert_eq!(
            client
                .call(Command::List { after: None, limit })
                .await
                .unwrap_err(),
            ClientError::NotSent(Failure::InvalidCommand)
        );
    }
    let (_, _, mut close) = close_command();
    if let Command::Close { mutation } = &mut close {
        mutation.revision = 0;
    }
    assert_eq!(
        client.call(close).await.unwrap_err(),
        ClientError::NotSent(Failure::InvalidCommand)
    );
    assert_eq!(
        client
            .call(Command::Stop {
                request_id: RequestId::new()
            })
            .await
            .unwrap_err(),
        ClientError::NotSent(Failure::UnsupportedCapability)
    );
    assert_eq!(
        client.call(Command::Status {}).await.unwrap(),
        json!({"ok":true})
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn mutation_eof_bad_correlation_epoch_or_frame_is_unknown_and_never_retried() {
    for variant in 0..5 {
        let endpoint = Endpoint::new();
        let config = endpoint.config.clone();
        let client_config = config.clone();
        let ready = welcome(&config);
        let (request_id, _, command) = close_command();
        let peer = tokio::spawn(async move {
            let _directory = endpoint._directory;
            let (mut stream, _) = endpoint.listener.accept().await.unwrap();
            authenticate(&mut stream, &config, &ready).await;
            let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
            assert_eq!(request.command.request_id(), Some(request_id));
            match variant {
                0 => return, // EOF after a possibly committed mutation.
                1 | 2 => {
                    send(
                        &mut stream,
                        &Response {
                            call_id: if variant == 1 {
                                CallId::new()
                            } else {
                                request.call_id
                            },
                            epoch: if variant == 2 {
                                Epoch::new()
                            } else {
                                request.epoch
                            },
                            reply: Reply::Data {
                                data: json!({"ok":true}),
                            },
                        },
                    )
                    .await
                }
                3 => {
                    stream.write_u32(u32::MAX).await.unwrap();
                }
                _ => {
                    write_frame(&mut stream, b"not-json").await.unwrap();
                }
            }
            assert!(read_frame::<_, Request>(&mut stream, MAX_FRAME_BYTES)
                .await
                .is_err());
        });
        let mut client = HostClient::connect(client_config).await.unwrap();
        let expected = [
            Failure::Connection,
            Failure::Correlation,
            Failure::EpochChanged,
            Failure::FrameTooLarge,
            Failure::MalformedFrame,
        ][variant];
        assert_eq!(
            client.call(command.clone()).await.unwrap_err(),
            ClientError::OutcomeUnknown {
                request_id,
                reason: expected
            }
        );
        assert_eq!(
            client.call(command).await.unwrap_err(),
            ClientError::NotSent(Failure::Closed)
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn result_query_requires_its_capability_and_is_never_a_mutation() {
    for supported in [false, true] {
        let endpoint = Endpoint::new();
        let config = endpoint.config.clone();
        let client_config = config.clone();
        let mut ready = welcome(&config);
        // A v1.4 peer can serve old commands without granting results.
        ready.version.minor = if supported { MINOR } else { 4 };
        if supported {
            ready.capabilities.push(Capability::Results);
        }
        let peer = tokio::spawn(async move {
            let _directory = endpoint._directory;
            let (mut stream, _) = endpoint.listener.accept().await.unwrap();
            authenticate(&mut stream, &config, &ready).await;
            if supported {
                let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
                assert!(matches!(request.command, Command::Result { .. }));
                assert!(request.command.request_id().is_none());
                // Deliberately lose a read-only response, never an execution.
            } else {
                assert!(read_frame::<_, Request>(&mut stream, MAX_FRAME_BYTES)
                    .await
                    .is_err());
            }
        });
        let mut client = HostClient::connect(client_config).await.unwrap();
        let command = Command::Result {
            agent_id: AgentId::new(),
            request_id: RequestId::new(),
            run_id: None,
            offset: 0,
            max_bytes: 4096,
        };
        let error = client.call(command.clone()).await.unwrap_err();
        if supported {
            assert!(matches!(error, ClientError::QueryFailed(_)));
            assert_eq!(
                client.call(command).await.unwrap_err(),
                ClientError::NotSent(Failure::Closed)
            );
        } else {
            assert_eq!(error, ClientError::NotSent(Failure::UnsupportedCapability));
        }
        drop(client);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn query_failure_is_not_a_mutation_receipt_and_poisoned_socket_is_closed() {
    let endpoint = Endpoint::new();
    let config = endpoint.config.clone();
    let client_config = config.clone();
    let ready = welcome(&config);
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut stream, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut stream, &config, &ready).await;
        let _: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
        write_frame(&mut stream, b"{\"private-path\":\"do-not-echo\"}")
            .await
            .unwrap();
    });
    let mut client = HostClient::connect(client_config).await.unwrap();
    let error = client.call(Command::Status {}).await.unwrap_err();
    assert_eq!(error, ClientError::QueryFailed(Failure::MalformedFrame));
    assert!(!error.to_string().contains("do-not-echo"));
    assert_eq!(
        client.call(Command::Status {}).await.unwrap_err(),
        ClientError::NotSent(Failure::Closed)
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn timeout_preserves_request_id_for_explicit_receipt_query_on_new_connection() {
    let endpoint = Endpoint::new();
    let config = endpoint.config.clone();
    let mut client_config = config.clone();
    client_config.timeout = Duration::from_millis(100);
    let ready = welcome(&config);
    let epoch = ready.epoch;
    let (request_id, agent_id, command) = close_command();
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut first, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut first, &config, &ready).await;
        let request: Request = read_frame(&mut first, MAX_FRAME_BYTES).await.unwrap();
        assert_eq!(request.command.request_id(), Some(request_id));
        assert!(read_frame::<_, Request>(&mut first, MAX_FRAME_BYTES)
            .await
            .is_err());
        let (mut second, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut second, &config, &ready).await;
        let query: Request = read_frame(&mut second, MAX_FRAME_BYTES).await.unwrap();
        assert!(matches!(
            query.command,
            Command::Receipt {
                agent_id: queried_agent,
                request_id: queried_request,
            } if queried_agent == agent_id && queried_request == request_id
        ));
        send(
            &mut second,
            &Response {
                call_id: query.call_id,
                epoch,
                reply: Reply::Data {
                    data: json!({"state":"outcome_unknown"}),
                },
            },
        )
        .await;
    });
    let mut first = HostClient::connect(client_config.clone()).await.unwrap();
    assert_eq!(
        first.call(command).await.unwrap_err(),
        ClientError::OutcomeUnknown {
            request_id,
            reason: Failure::Timeout
        }
    );
    client_config.timeout = Duration::from_secs(2);
    let mut second = HostClient::connect(client_config).await.unwrap();
    assert_eq!(
        second
            .call(Command::Receipt {
                agent_id,
                request_id
            })
            .await
            .unwrap(),
        json!({"state":"outcome_unknown"})
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn dropping_a_waiter_retires_the_socket_without_canceling_remote_work() {
    let endpoint = Endpoint::new();
    let config = endpoint.config.clone();
    let client_config = config.clone();
    let ready = welcome(&config);
    let (received, receipt) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut stream, _) = endpoint.listener.accept().await.unwrap();
        authenticate(&mut stream, &config, &ready).await;
        let _: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
        received.send(()).unwrap();
        assert!(read_frame::<_, Request>(&mut stream, MAX_FRAME_BYTES)
            .await
            .is_err());
    });
    let mut client = HostClient::connect(client_config).await.unwrap();
    {
        let pending = client.call(close_command().2);
        tokio::pin!(pending);
        tokio::select! {
            _ = receipt => {},
            result = &mut pending => panic!("peer has not answered: {result:?}"),
        }
        // Drop the exchange future while the peer still owns its operation.
    }
    assert_eq!(
        client.call(Command::Status {}).await.unwrap_err(),
        ClientError::NotSent(Failure::Closed)
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn endpoint_requires_private_current_uid_directory_and_socket_and_rejects_links() {
    let endpoint = Endpoint::new();
    let path = &endpoint.config.socket_path;
    let parent = path.parent().unwrap();
    assert!(check_endpoint(path).await.is_ok());
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        check_endpoint(path).await.unwrap_err(),
        Failure::UnsafeEndpoint
    );
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)).unwrap();
    assert_eq!(
        check_endpoint(path).await.unwrap_err(),
        Failure::UnsafeEndpoint
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let linked = parent.join("link");
    std::os::unix::fs::symlink(path, &linked).unwrap();
    assert_eq!(
        check_endpoint(&linked).await.unwrap_err(),
        Failure::UnsafeEndpoint
    );
    let file = parent.join("file");
    std::fs::write(&file, "not a socket").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        check_endpoint(&file).await.unwrap_err(),
        Failure::UnsafeEndpoint
    );
    let mut config = endpoint.config.clone();
    config.socket_path = PathBuf::from("relative");
    assert_eq!(
        HostClient::connect(config).await.unwrap_err(),
        ClientError::NotSent(Failure::InvalidConfiguration)
    );
    let mut config = endpoint.config.clone();
    config.timeout = Duration::ZERO;
    assert_eq!(
        HostClient::connect(config).await.unwrap_err(),
        ClientError::NotSent(Failure::InvalidConfiguration)
    );
    let mut config = endpoint.config.clone();
    config.socket_path = linked;
    assert_eq!(
        HostClient::connect(config).await.unwrap_err(),
        ClientError::NotSent(Failure::UnsafeEndpoint)
    );
    assert!(!format!("{:?}", endpoint.config).contains(&path.to_string_lossy().to_string()));
}

#[tokio::test]
async fn handshake_deadline_covers_partial_frames_and_no_automatic_reconnect() {
    let endpoint = Endpoint::new();
    let mut config = endpoint.config.clone();
    config.timeout = Duration::from_millis(100);
    let peer = tokio::spawn(async move {
        let _directory = endpoint._directory;
        let (mut stream, _) = endpoint.listener.accept().await.unwrap();
        let _: Hello = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
        stream.write_u32(100).await.unwrap();
        stream.write_all(b"{").await.unwrap();
        // The client times out instead of accepting a partial hello, retrying,
        // or leaving a background reader alive.
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
    });
    assert_eq!(
        HostClient::connect(config).await.unwrap_err(),
        ClientError::NotSent(Failure::Timeout)
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn frame_lengths_checked_before_body_and_json_is_strict() {
    for length in [0, (MAX_FRAME_BYTES + 1) as u32, u32::MAX] {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer.write_u32(length).await.unwrap();
        assert_eq!(
            read_frame::<_, serde_json::Value>(&mut reader, MAX_FRAME_BYTES)
                .await
                .unwrap_err(),
            Failure::FrameTooLarge
        );
    }
    for bytes in [b"{} trailing".as_slice(), b"{", b"{\"x\":1}\n{\"x\":2}"] {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_frame(&mut writer, bytes).await.unwrap();
        assert_eq!(
            read_frame::<_, serde_json::Value>(&mut reader, MAX_FRAME_BYTES)
                .await
                .unwrap_err(),
            Failure::MalformedFrame
        );
    }
    assert_eq!(
        encode_frame(&json!({"large":"1234567890"}), 5).unwrap_err(),
        Failure::FrameTooLarge
    );
}

#[tokio::test]
async fn administrative_execution_lost_reply_retains_original_mutation_identity() {
    for operation in 0..5 {
        for wait_for_timeout in [false, true] {
            let endpoint = Endpoint::new();
            let config = endpoint.config.clone();
            let client_config = config.clone();
            let mut ready = welcome(&config);
            ready.capabilities.extend([
                Capability::AdministrativeCreate,
                Capability::AdministrativeRestore,
                Capability::AdministrativeClose,
                Capability::AdministrativeCancel,
                Capability::AdministrativeStop,
            ]);
            let agent_id = AgentId::new();
            let profile = ProfileRef {
                agent_id,
                version: ProfileVersion::new(),
            };
            let action = match operation {
                0 => AdministrativeAction::CreateProfile { profile },
                1 => AdministrativeAction::RestoreProfile {
                    profile,
                    revision: 1,
                },
                2 => AdministrativeAction::Close {
                    agent_id,
                    revision: 1,
                },
                3 => AdministrativeAction::CancelSubmitted {
                    agent_id,
                    revision: 1,
                    submitted_request_id: RequestId::new(),
                },
                _ => AdministrativeAction::Stop {},
            };
            let intent = AdministrativeRequest {
                scope: ControlScopeId::new(),
                request_id: RequestId::new(),
                epoch: ready.epoch,
                action,
            };
            let id = intent.request_id;
            let command = match operation {
                0 => Command::AdministrativeCreate { request: intent },
                1 => Command::AdministrativeRestore { request: intent },
                2 => Command::AdministrativeClose { request: intent },
                3 => Command::AdministrativeCancel { request: intent },
                _ => Command::AdministrativeStop { request: intent },
            };
            let (release, released) = tokio::sync::oneshot::channel::<()>();
            let peer = tokio::spawn(async move {
                let _directory = endpoint._directory;
                let (mut stream, _) = endpoint.listener.accept().await.unwrap();
                authenticate(&mut stream, &config, &ready).await;
                let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
                assert_eq!(request.command.request_id(), Some(id));
                assert!(request.command.valid(Limits::default()));
                if wait_for_timeout {
                    let _ = released.await;
                }
            });
            let mut client = HostClient::connect(client_config).await.unwrap();
            let failure = client.call(command).await.unwrap_err();
            assert!(
                matches!(failure, ClientError::OutcomeUnknown { request_id, .. } if request_id == id)
            );
            if wait_for_timeout {
                assert!(matches!(
                    failure,
                    ClientError::OutcomeUnknown {
                        reason: Failure::Timeout,
                        ..
                    }
                ));
            }
            let _ = release.send(());
            peer.await.unwrap();
        }
    }
}

#[tokio::test]
async fn administrative_receipt_disconnect_and_timeout_are_queries_not_unknown_mutations() {
    for wait_for_timeout in [false, true] {
        let endpoint = Endpoint::new();
        let config = endpoint.config.clone();
        let client_config = config.clone();
        let mut ready = welcome(&config);
        ready.capabilities.push(Capability::AdministrativeReceipts);
        let target = AdministrativeRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: Epoch::new(),
            action: AdministrativeAction::Stop {},
        };
        let expected = target.clone();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let peer = tokio::spawn(async move {
            let _directory = endpoint._directory;
            let (mut stream, _) = endpoint.listener.accept().await.unwrap();
            authenticate(&mut stream, &config, &ready).await;
            let request: Request = read_frame(&mut stream, MAX_FRAME_BYTES).await.unwrap();
            assert_eq!(request.epoch, ready.epoch);
            assert_ne!(request.epoch, expected.epoch);
            assert_eq!(request.command.request_id(), None);
            assert!(
                matches!(request.command, Command::AdministrativeReceipt { request } if request == expected)
            );
            if wait_for_timeout {
                let _ = released.await;
            }
        });
        let mut client = HostClient::connect(client_config).await.unwrap();
        let failure = client
            .call(Command::AdministrativeReceipt { request: target })
            .await
            .unwrap_err();
        if wait_for_timeout {
            assert_eq!(failure, ClientError::QueryFailed(Failure::Timeout));
        } else {
            assert!(matches!(failure, ClientError::QueryFailed(_)));
        }
        assert_eq!(
            client.call(Command::Status {}).await.unwrap_err(),
            ClientError::NotSent(Failure::Closed)
        );
        let _ = release.send(());
        peer.await.unwrap();
    }
}
