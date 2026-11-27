// Real rpi process + authenticated IPC + gated HTTP. Not management-DLL E2E.
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stop_flushes_native_receipt_then_exits_and_history_does_not_stop_restart() {
    binary_stop_scenario(false, false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stop_lost_reply_still_exits_and_original_intent_reconciles_on_restart() {
    binary_stop_scenario(true, false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stop_cli_preserves_original_intent_and_history_does_not_stop_new_live_owner()
{
    binary_stop_scenario(false, true).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stop_cli_lost_stdout_is_unknown_but_remote_exits_and_original_file_reconciles(
) {
    binary_stop_scenario(true, true).await;
}
async fn binary_stop_scenario(abandon: bool, cli: bool) {
    use execution_wire::{AdministrativeAction as A, Command as C};
    let mut fixture = Fixture::new();
    let (native_agent, path, mut profile) = administrative_profile(&fixture);
    let agent: execution_wire::AgentId = serde_json::from_value(json!(native_agent)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["agents"] = json!([path]);
    fixture.value["administration"] =
        json!({"scope":scope,"allowed_agents":[agent],"permissions":["stop"],"max_jobs":1});
    fixture.save();
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(execution_http_request(&mut stream)
            .await
            .contains("binary-stop-gated"));
        arrived.send(()).unwrap();
        // Deliberately never send response headers. Native shutdown must abort
        // the request without depending on HTTP completion or a reply reader.
        tokio::time::timeout(DEADLINE, released)
            .await
            .unwrap()
            .unwrap();
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let (child, ready) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    assert!(client
        .welcome()
        .supports(execution_wire::Capability::AdministrativeStop));
    assert!(!client.welcome().supports(execution_wire::Capability::Stop));
    let input = execution_wire::RequestId::new();
    client
        .call(C::Submit {
            agent_id: agent,
            request_id: input,
            prompt: "binary-stop-gated".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, arrival)
        .await
        .unwrap()
        .unwrap();
    let intent = execution_intent(&client, scope, A::Stop {});
    let original = cli_request_file(&fixture, &json!(intent));
    let original_bytes = std::fs::read(&original).unwrap();
    let receipt = if abandon {
        if cli {
            let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            drop(reader);
            let fd: std::os::fd::OwnedFd = writer.into();
            // output() would replace this explicit broken stdout FD.
            let output = tokio::time::timeout(
                DEADLINE,
                fixture
                    .command()
                    .args(["host", "administrative-stop", "--config"])
                    .arg(&fixture.config)
                    .arg("--request")
                    .arg(&original)
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
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr)
                .contains(fixture.value["token"].as_str().unwrap()));
            assert!(!String::from_utf8_lossy(&output.stderr)
                .contains("administration-test-private-key"));
        } else {
            abandon_cancel_reply(&fixture, &client, &intent).await;
        }
        None
    } else {
        let data = if cli {
            cli_request_result(&fixture, "administrative-stop", &original, 0).await
        } else {
            client.call(execution_command(&intent)).await.unwrap()
        };
        execution_succeeded(&data);
        assert_eq!(
            data["receipt"]["status"]["result"]["kind"],
            "native_settled"
        );
        Some(data["receipt"].clone())
    };
    let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("listener stopped"));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(fixture.value["token"].as_str().unwrap())
    );
    assert!(!fixture.socket().exists());
    assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
    let session = PathBuf::from(profile["agent"]["session"]["path"].as_str().unwrap());
    let bytes = std::fs::read(&session).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"aborted\""));
    assert_eq!(text.matches("rpi.host.admission.v1").count(), 1);
    assert!(!text.contains("administration-test-private-key"));
    // Prove process cleanup releases both singleton and the native Session
    // writer, not merely a successful journal receipt.
    let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
    let writer = rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    drop(writer);
    // Restart a NEW live owner from the exact Session, without input replay.
    // An old successful Stop must not drain this replacement owner.
    let (next, next_ready) = fixture.start().await;
    assert_ne!(ready["control"]["epoch"], next_ready["control"]["epoch"]);
    let mut peer = administrative_client(&fixture).await;
    let historical = cli_request_result(&fixture, "administrative-receipt", &original, 0).await;
    if let Some(receipt) = receipt {
        assert_eq!(historical["receipt"], receipt);
    }
    execution_succeeded(&historical);
    assert_eq!(
        historical["receipt"]["status"]["result"]["kind"],
        "native_settled"
    );
    for _ in 0..3 {
        let duplicate = if cli {
            cli_request_result(&fixture, "administrative-stop", &original, 0).await
        } else {
            peer.call(execution_command(&intent)).await.unwrap()
        };
        assert_eq!(duplicate["receipt"], historical["receipt"]);
        assert_eq!(peer.call(C::Status {}).await.unwrap()["resident"], 1);
    }
    let input_receipt = peer
        .call(C::Receipt {
            agent_id: agent,
            request_id: input,
        })
        .await
        .unwrap();
    assert_eq!(input_receipt["execution"]["finished"], true);
    assert_eq!(std::fs::read(&session).unwrap(), bytes);
    assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
    release.send(()).unwrap();
    provider.await.unwrap();
    fixture.stop(next, nix::sys::signal::Signal::SIGTERM).await;
    fixture.no_resources();
}
