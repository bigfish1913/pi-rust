// Included by host_control.rs. Actual native binary + local HTTP, not a DLL E2E.
use rpi_cli::host::protocol as execution_wire;
fn execution_intent(
    client: &rpi_cli::host::client::HostClient,
    scope: execution_wire::ControlScopeId,
    action: execution_wire::AdministrativeAction,
) -> execution_wire::AdministrativeRequest {
    execution_wire::AdministrativeRequest {
        scope,
        request_id: execution_wire::RequestId::new(),
        epoch: client.welcome().epoch,
        action,
    }
}
fn execution_command(intent: &execution_wire::AdministrativeRequest) -> execution_wire::Command {
    use execution_wire::{AdministrativeAction as A, Command as C};
    match intent.action {
        A::CreateProfile { .. } => C::AdministrativeCreate {
            request: intent.clone(),
        },
        A::RestoreProfile { .. } => C::AdministrativeRestore {
            request: intent.clone(),
        },
        A::Close { .. } => C::AdministrativeClose {
            request: intent.clone(),
        },
        A::CancelSubmitted { .. } => C::AdministrativeCancel {
            request: intent.clone(),
        },
        A::Stop {} => C::AdministrativeStop {
            request: intent.clone(),
        },
        _ => panic!("not an enabled administrative execution"),
    }
}
fn execution_succeeded(data: &Value) {
    use rpi_cli::host::lifecycle::{LifecycleReceipt, LifecycleState};
    let receipt: LifecycleReceipt = serde_json::from_value(data["receipt"].clone()).unwrap();
    assert!(
        matches!(receipt.status, LifecycleState::Succeeded { .. }),
        "{receipt:?}"
    );
}
async fn execution_revision(
    client: &mut rpi_cli::host::client::HostClient,
    agent_id: execution_wire::AgentId,
) -> u64 {
    client
        .call(execution_wire::Command::Inspect { agent_id })
        .await
        .unwrap()["revision"]
        .as_u64()
        .unwrap()
}

async fn execution_http_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 1024 * 1024);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
        })
        .unwrap();
    assert!(length <= 1024 * 1024 - header_end);
    while bytes.len() < header_end + length {
        let n = stream.read(&mut chunk).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 1024 * 1024);
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_dynamic_profile_http_close_restore_and_historical_execute_do_not_replay() {
    use execution_wire::{AdministrativeAction as A, Command as C};
    let mut fixture = Fixture::new();
    let (agent, path, mut profile) = administrative_profile(&fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    // The metadata-only fixture's 1024-token window would auto-compact the
    // native system prompt before user execution, adding an unrelated HTTP call.
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let provider = tokio::spawn(async move {
        for round in 0..2 {
            let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
                .await
                .unwrap()
                .unwrap();
            let received = execution_http_request(&mut stream).await;
            assert!(received.contains("Bearer administration-test-private-key"));
            assert!(
                received.contains(&format!("dynamic-wire-{round}")),
                "expected fixture user prompt in HTTP round {round}"
            );
            let reply = sse_completion();
            use tokio::io::AsyncWriteExt;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["administration"] = json!({"scope": scope, "profiles": [path], "allowed_agents": [agent], "permissions": ["create", "restore", "close"]});
    fixture.save();
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    let found = client
        .call(C::Profiles {
            after: None,
            limit: 1,
        })
        .await
        .unwrap();
    let reference: execution_wire::ProfileRef =
        serde_json::from_value(found["profiles"][0].clone()).unwrap();
    let create = execution_intent(&client, scope, A::CreateProfile { profile: reference });
    let created = client.call(execution_command(&create)).await.unwrap();
    execution_succeeded(&created);
    assert_eq!(
        client.call(execution_command(&create)).await.unwrap(),
        created
    );
    let mut restored = None;
    for round in 0..2 {
        let id = execution_wire::RequestId::new();
        let submitted = client
            .call(C::Submit {
                agent_id: reference.agent_id,
                request_id: id,
                prompt: format!("dynamic-wire-{round}"),
            })
            .await
            .unwrap();
        assert_eq!(submitted["durable"], true);
        tokio::time::timeout(DEADLINE, async {
            loop {
                let data = client
                    .call(C::Receipt {
                        agent_id: reference.agent_id,
                        request_id: id,
                    })
                    .await
                    .unwrap();
                if data["execution"]["finished"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let revision = execution_revision(&mut client, reference.agent_id).await;
        let close = execution_intent(
            &client,
            scope,
            A::Close {
                agent_id: reference.agent_id,
                revision,
            },
        );
        let closed = client.call(execution_command(&close)).await.unwrap();
        execution_succeeded(&closed);
        assert_eq!(
            client.call(execution_command(&close)).await.unwrap(),
            closed
        );
        assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 0);
        if round == 0 {
            let revision = execution_revision(&mut client, reference.agent_id).await;
            let intent = execution_intent(
                &client,
                scope,
                A::RestoreProfile {
                    profile: reference,
                    revision,
                },
            );
            let receipt = client.call(execution_command(&intent)).await.unwrap();
            execution_succeeded(&receipt);
            restored = Some((intent, receipt));
        }
    }
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let session = fixture.root.path().join("dynamic.jsonl");
    let before = std::fs::read(&session).unwrap();
    let text = String::from_utf8(before.clone()).unwrap();
    assert!(
        text.contains("dynamic-wire-0")
            && text.contains("dynamic-wire-1")
            && text.contains("binary-profile-answer")
    );
    assert!(!text.contains("administration-test-private-key"));
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    let found = client
        .call(C::Profiles {
            after: None,
            limit: 1,
        })
        .await
        .unwrap();
    let new_ref: execution_wire::ProfileRef =
        serde_json::from_value(found["profiles"][0].clone()).unwrap();
    assert_ne!(new_ref.version, reference.version);
    assert_ne!(client.welcome().epoch, create.epoch);
    assert_eq!(
        client.call(execution_command(&create)).await.unwrap(),
        created
    );
    let (restore, receipt) = restored.unwrap();
    assert_eq!(
        client.call(execution_command(&restore)).await.unwrap(),
        receipt
    );
    assert_eq!(
        client
            .call(C::AdministrativeReceipt { request: create })
            .await
            .unwrap()["receipt"],
        created["receipt"]
    );
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 0);
    assert_eq!(std::fs::read(session).unwrap(), before);
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGINT).await;
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_abandoned_administrative_reply_is_reconciled_without_recreating_session() {
    use execution_wire::{AdministrativeAction as A, Command as C};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut fixture = Fixture::new();
    let (agent, path, _) = administrative_profile(&fixture);
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["administration"] = json!({"scope": scope, "profiles": [path], "allowed_agents": [agent], "permissions": ["create"]});
    fixture.save();
    let (child, _) = fixture.start().await;
    let mut observer = administrative_client(&fixture).await;
    let found = observer
        .call(C::Profiles {
            after: None,
            limit: 1,
        })
        .await
        .unwrap();
    let reference = serde_json::from_value(found["profiles"][0].clone()).unwrap();
    let intent = execution_intent(&observer, scope, A::CreateProfile { profile: reference });
    let mut raw = tokio::net::UnixStream::connect(fixture.socket())
        .await
        .unwrap();
    let hello = execution_wire::Hello {
        protocol: execution_wire::PROTOCOL.into(),
        version: execution_wire::Version::default(),
        host_id: observer.welcome().host_id,
        token: execution_wire::AuthToken::new(fixture.value["token"].as_str().unwrap().into())
            .unwrap(),
    };
    let bytes = serde_json::to_vec(&hello).unwrap();
    raw.write_u32(bytes.len() as u32).await.unwrap();
    raw.write_all(&bytes).await.unwrap();
    let len = raw.read_u32().await.unwrap() as usize;
    assert!(len <= execution_wire::MAX_FRAME_BYTES);
    let mut bytes = vec![0; len];
    raw.read_exact(&mut bytes).await.unwrap();
    let welcome: execution_wire::Welcome = serde_json::from_slice(&bytes).unwrap();
    let frame = execution_wire::Request {
        call_id: execution_wire::CallId::new(),
        epoch: welcome.epoch,
        command: execution_command(&intent),
    };
    let bytes = serde_json::to_vec(&frame).unwrap();
    raw.write_u32(bytes.len() as u32).await.unwrap();
    raw.write_all(&bytes).await.unwrap();
    drop(raw); // No lifecycle reply is consumed; this does not grant a replay.
    let completed = tokio::time::timeout(DEADLINE, async {
        loop {
            let data = observer
                .call(C::AdministrativeReceipt {
                    request: intent.clone(),
                })
                .await
                .unwrap();
            if data["found"] == true {
                let receipt: rpi_cli::host::lifecycle::LifecycleReceipt =
                    serde_json::from_value(data["receipt"].clone()).unwrap();
                if matches!(
                    receipt.status,
                    rpi_cli::host::lifecycle::LifecycleState::Succeeded { .. }
                ) {
                    break data;
                }
                assert!(matches!(
                    receipt.status,
                    rpi_cli::host::lifecycle::LifecycleState::Prepared {}
                ));
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let session = fixture.root.path().join("dynamic.jsonl");
    let before = std::fs::read(&session).unwrap();
    let repeated = observer.call(execution_command(&intent)).await.unwrap();
    assert_eq!(repeated["receipt"], completed["receipt"]);
    assert_eq!(std::fs::read(session).unwrap(), before);
    assert_eq!(observer.call(C::Status {}).await.unwrap()["resident"], 1);
    drop(observer);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    fixture.no_resources();
}
