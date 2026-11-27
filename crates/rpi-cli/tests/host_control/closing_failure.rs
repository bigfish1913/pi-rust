// Actual binary + gated HTTP + real SQLite failure; no management DLL.
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_close_failure_retains_owner_and_shutdown_retries_only_on_explicit_signal() {
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
        json!({"scope":scope,"allowed_agents":[agent],"permissions":["close"]});
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
            .contains("binary-close-failure"));
        arrived.send(()).unwrap();
        // No response headers: native close must abort the run independently.
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
    let (mut child, ready) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    let input = execution_wire::RequestId::new();
    client
        .call(C::Submit {
            agent_id: agent,
            request_id: input,
            prompt: "binary-close-failure".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(DEADLINE, arrival)
        .await
        .unwrap()
        .unwrap();
    let revision = execution_revision(&mut client, agent).await;
    let intent = execution_intent(
        &client,
        scope,
        A::Close {
            agent_id: agent,
            revision,
        },
    );
    let original = cli_request_file(&fixture, &json!(intent));
    let original_bytes = std::fs::read(&original).unwrap();
    let db = rusqlite::Connection::open(fixture.directory().join("agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_binary_closed BEFORE UPDATE ON agents WHEN NEW.resident=0 BEGIN SELECT RAISE(ABORT,'injected binary Closed failure'); END;").unwrap();
    let failed = cli_request_result(&fixture, "administrative-close", &original, 3).await;
    assert_eq!(failed["receipt"]["status"]["state"], "uncertain");
    assert_eq!(failed["receipt"]["request"], json!(intent));
    assert_eq!(
        cli_request_result(&fixture, "administrative-receipt", &original, 3).await["receipt"],
        failed["receipt"]
    );
    assert_eq!(
        cli_request_result(&fixture, "administrative-close", &original, 3).await,
        failed
    );
    let status = client.call(C::Status {}).await.unwrap();
    assert_eq!(status["owners"], 1);
    assert_eq!(status["resident"], 1);
    assert_eq!(status["stopping"], false);
    assert!(child.try_wait().unwrap().is_none());
    let session = fixture.root.path().join("dynamic.jsonl");
    let bytes = std::fs::read(&session).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"aborted\""));
    assert_eq!(text.matches("rpi.host.admission.v1").count(), 1);
    assert!(!text.contains("administration-test-private-key"));
    let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
    // Retained owner/resident custody is not a claim that a failed terminal
    // catalog publication can keep an already-released physical writer held.
    let writer = rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    drop(writer);
    let mut diagnostics = BufReader::new(child.stderr.take().unwrap()).lines();
    let pid = nix::unistd::Pid::from_raw(child.id().unwrap() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let diagnostic = tokio::time::timeout(DEADLINE, diagnostics.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        diagnostic.contains("retains unsettled owners"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("injected binary"));
    assert!(!diagnostic.contains(fixture.value["token"].as_str().unwrap()));
    assert!(child.try_wait().unwrap().is_none());
    assert!(fixture.socket().exists());
    let status = client.call(C::Status {}).await.unwrap();
    assert_eq!(status["stopping"], true);
    assert_eq!(status["owners"], 1);
    assert_eq!(status["resident"], 1);
    assert_eq!(
        cli_request_result(&fixture, "administrative-receipt", &original, 3).await["receipt"],
        failed["receipt"]
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), diagnostics.next_line())
            .await
            .is_err(),
        "failed shutdown must not spin/retry itself"
    );
    db.execute_batch("DROP TRIGGER reject_binary_closed")
        .unwrap();
    drop(db);
    // Removing the fault is NOT permission to replay an administrative intent
    // or automatically retry shutdown. The next explicit signal owns retry.
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(client.call(C::Status {}).await.unwrap()["owners"], 1);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.socket().exists());
    assert_eq!(std::fs::read(&session).unwrap(), bytes);
    let (next, next_ready) = fixture.start().await;
    assert_ne!(ready["control"]["epoch"], next_ready["control"]["epoch"]);
    let mut peer = administrative_client(&fixture).await;
    assert_eq!(
        cli_request_result(&fixture, "administrative-receipt", &original, 3).await["receipt"],
        failed["receipt"]
    );
    assert_eq!(
        cli_request_result(&fixture, "administrative-close", &original, 3).await,
        failed
    );
    let status = peer.call(C::Status {}).await.unwrap();
    assert_eq!(status["resident"], 1);
    assert_eq!(status["owners"], 1);
    assert_eq!(status["stopping"], false);
    assert_eq!(
        peer.call(C::Receipt {
            agent_id: agent,
            request_id: input
        })
        .await
        .unwrap()["execution"]["finished"],
        true
    );
    assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
    assert_eq!(std::fs::read(&session).unwrap(), bytes);
    release.send(()).unwrap();
    provider.await.unwrap();
    drop(diagnostics);
    fixture.stop(next, nix::sys::signal::Signal::SIGTERM).await;
    fixture.no_resources();
}
