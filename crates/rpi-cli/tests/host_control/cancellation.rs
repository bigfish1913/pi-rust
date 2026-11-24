// Actual binary + authenticated IPC + two real HTTP requests. No management DLL.
async fn abandon_cancel_reply(
    fixture: &Fixture,
    observer: &rpi_cli::host::client::HostClient,
    intent: &execution_wire::AdministrativeRequest,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    let n = raw.read_u32().await.unwrap() as usize;
    assert!(n <= execution_wire::MAX_FRAME_BYTES);
    let mut bytes = vec![0; n];
    raw.read_exact(&mut bytes).await.unwrap();
    let welcome: execution_wire::Welcome = serde_json::from_slice(&bytes).unwrap();
    let frame = execution_wire::Request {
        call_id: execution_wire::CallId::new(),
        epoch: welcome.epoch,
        command: execution_command(intent),
    };
    let bytes = serde_json::to_vec(&frame).unwrap();
    raw.write_u32(bytes.len() as u32).await.unwrap();
    raw.write_all(&bytes).await.unwrap();
    drop(raw); // The complete original intent is retained; never assign a new ID.
}
async fn cancellation_receipt(
    client: &mut rpi_cli::host::client::HostClient,
    intent: &execution_wire::AdministrativeRequest,
) -> Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let value = client
                .call(execution_wire::Command::AdministrativeReceipt {
                    request: intent.clone(),
                })
                .await
                .unwrap();
            if value["found"] == true {
                let receipt: rpi_cli::host::lifecycle::LifecycleReceipt =
                    serde_json::from_value(value["receipt"].clone()).unwrap();
                if matches!(
                    receipt.status,
                    rpi_cli::host::lifecycle::LifecycleState::Succeeded { .. }
                ) {
                    return value;
                }
                assert!(matches!(
                    receipt.status,
                    rpi_cli::host::lifecycle::LifecycleState::Prepared {}
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn cancellation_signalled(value: &Value, expected: bool) {
    use rpi_cli::host::lifecycle::{LifecycleReceipt, LifecycleResult, LifecycleState};
    let receipt: LifecycleReceipt = serde_json::from_value(value["receipt"].clone()).unwrap();
    assert_eq!(
        receipt.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled {
                signalled: expected
            }
        }
    );
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stable_cancel_lost_reply_aborts_http_without_retargeting_or_restart_replay()
{
    binary_cancel_scenario(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_cancel_cli_preserves_original_intent_after_lost_stdout_and_restart() {
    binary_cancel_scenario(true).await;
}
async fn binary_cancel_scenario(cli: bool) {
    use execution_wire::AdministrativeAction as A;
    use tokio::io::AsyncWriteExt;
    let mut fixture = Fixture::new();
    let (agent, path, mut profile) = administrative_profile(&fixture);
    let agent: execution_wire::AgentId = serde_json::from_value(json!(agent)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["agents"] = json!([path]);
    fixture.value["administration"] =
        json!({"scope":scope,"allowed_agents":[agent],"permissions":["cancel"],"max_jobs":1});
    fixture.save();
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (drop_first, dropped_first) = tokio::sync::oneshot::channel();
    let (arrived_second, arrival_second) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut first, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(execution_http_request(&mut first)
            .await
            .contains("binary-cancel-first"));
        arrived.send(()).unwrap();
        tokio::time::timeout(DEADLINE, dropped_first)
            .await
            .unwrap()
            .unwrap();
        drop(first);
        let (mut second, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(execution_http_request(&mut second)
            .await
            .contains("binary-cancel-second"));
        arrived_second.send(()).unwrap();
        tokio::time::timeout(DEADLINE, released)
            .await
            .unwrap()
            .unwrap();
        let reply = sse_completion();
        second.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
        second.shutdown().await.unwrap();
    });
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    assert!(client
        .welcome()
        .capabilities
        .contains(&execution_wire::Capability::AdministrativeCancel));
    assert!(!client
        .welcome()
        .capabilities
        .contains(&execution_wire::Capability::Cancel));
    let original = execution_wire::RequestId::new();
    let first_path = cli_request_file(
        &fixture,
        &json!({"agent_id":agent,"request_id":original,"prompt":"binary-cancel-first"}),
    );
    cli_request_result(&fixture, "submit", &first_path, 0).await;
    tokio::time::timeout(DEADLINE, arrival)
        .await
        .unwrap()
        .unwrap();
    let revision = execution_revision(&mut client, agent).await;
    let intent = execution_intent(
        &client,
        scope,
        A::CancelSubmitted {
            agent_id: agent,
            revision,
            submitted_request_id: original,
        },
    );
    let intent_path = cli_request_file(&fixture, &json!(intent));
    let original_bytes = std::fs::read(&intent_path).unwrap();
    let cancelled = if cli {
        let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(reader);
        let fd: std::os::fd::OwnedFd = writer.into();
        let output = tokio::time::timeout(
            DEADLINE,
            fixture
                .command()
                .args(["host", "administrative-cancel", "--config"])
                .arg(&fixture.config)
                .arg("--request")
                .arg(&intent_path)
                .stdout(Stdio::from(fd))
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
                .wait_with_output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("administration-test-private-key")
        );
        assert_eq!(std::fs::read(&intent_path).unwrap(), original_bytes);
        cli_request_result(&fixture, "administrative-receipt", &intent_path, 0).await
    } else {
        abandon_cancel_reply(&fixture, &client, &intent).await;
        cancellation_receipt(&mut client, &intent).await
    };
    cancellation_signalled(&cancelled, true);
    // The Provider still has not sent headers: a durable CancelSignalled is not
    // the finish marker, so observe settlement independently with the old file.
    tokio::time::timeout(DEADLINE, async {
        loop {
            if cli_request_result(&fixture, "receipt", &first_path, 0).await["execution"]
                ["finished"]
                == true
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop_first.send(()).unwrap();
    let next = execution_wire::RequestId::new();
    let second_path = cli_request_file(
        &fixture,
        &json!({"agent_id":agent,"request_id":next,"prompt":"binary-cancel-second"}),
    );
    cli_request_result(&fixture, "submit", &second_path, 0).await;
    tokio::time::timeout(DEADLINE, arrival_second)
        .await
        .unwrap()
        .unwrap();
    let repeated = if cli {
        cli_request_result(&fixture, "administrative-cancel", &intent_path, 0).await
    } else {
        client.call(execution_command(&intent)).await.unwrap()
    };
    assert_eq!(repeated["receipt"], cancelled["receipt"]);
    let revision = execution_revision(&mut client, agent).await;
    let mut changed = intent.clone();
    changed.action = A::CancelSubmitted {
        agent_id: agent,
        revision,
        submitted_request_id: next,
    };
    if cli {
        // A deliberately changed intent gets a different file, never overwrites
        // the retained original request or uses a new administrative ID.
        let changed_path = fixture.root.path().join("changed-cancel-intent.json");
        std::fs::write(&changed_path, serde_json::to_vec(&changed).unwrap()).unwrap();
        std::fs::set_permissions(&changed_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let output = fixture
            .call(
                "administrative-cancel",
                &["--request", changed_path.to_str().unwrap()],
            )
            .await;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Conflict"));
    } else {
        assert_eq!(
            client.call(execution_command(&changed)).await.unwrap_err(),
            rpi_cli::host::client::ClientError::Rejected(execution_wire::ErrorCode::Conflict)
        );
    }
    let absent = execution_intent(
        &client,
        scope,
        A::CancelSubmitted {
            agent_id: agent,
            revision,
            submitted_request_id: original,
        },
    );
    let absent_result = if cli {
        let path = cli_request_file(&fixture, &json!(absent));
        cli_request_result(&fixture, "administrative-cancel", &path, 0).await
    } else {
        client.call(execution_command(&absent)).await.unwrap()
    };
    cancellation_signalled(&absent_result, false);
    let running = cli_request_result(&fixture, "receipt", &second_path, 0).await;
    assert_eq!(running["execution"]["finished"], false);
    release.send(()).unwrap();
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(DEADLINE, async {
        loop {
            if cli_request_result(&fixture, "receipt", &second_path, 0).await["execution"]
                ["finished"]
                == true
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let session = fixture.root.path().join("dynamic.jsonl");
    let before = std::fs::read(&session).unwrap();
    let text = std::str::from_utf8(&before).unwrap();
    assert!(text.contains("\"aborted\"") && text.contains("binary-profile-answer"));
    assert_eq!(text.matches("rpi.host.admission.v1").count(), 2);
    assert!(!text.contains("administration-test-private-key"));
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    let recovered = if cli {
        cli_request_result(&fixture, "administrative-receipt", &intent_path, 0).await
    } else {
        cancellation_receipt(&mut client, &intent).await
    };
    assert_eq!(recovered, cancelled);
    let repeated = if cli {
        cli_request_result(&fixture, "administrative-cancel", &intent_path, 0).await
    } else {
        client.call(execution_command(&intent)).await.unwrap()
    };
    assert_eq!(repeated["receipt"], cancelled["receipt"]);
    let revision = execution_revision(&mut client, agent).await;
    let idle = execution_intent(
        &client,
        scope,
        A::CancelSubmitted {
            agent_id: agent,
            revision,
            submitted_request_id: original,
        },
    );
    let idle_result = if cli {
        let path = cli_request_file(&fixture, &json!(idle));
        cli_request_result(&fixture, "administrative-cancel", &path, 0).await
    } else {
        client.call(execution_command(&idle)).await.unwrap()
    };
    cancellation_signalled(&idle_result, false);
    assert_eq!(std::fs::read(&intent_path).unwrap(), original_bytes);
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGINT).await;
    assert_eq!(std::fs::read(session).unwrap(), before);
    fixture.no_resources();
}
