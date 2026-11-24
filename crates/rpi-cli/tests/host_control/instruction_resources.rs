// Actual binary + HTTP with instruction resources; no DLL or partial loader.
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_persists_frozen_instructions_before_provider_and_reopens_exact_inventory() {
    use rpi_cli::host::{
        client::{ConnectionConfig, HostClient},
        protocol::{AgentId as WireAgent, AuthToken, Command, HostId, RequestId},
    };
    let mut fixture = Fixture::new();
    let workspace = fixture.root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let instruction = fixture.root.path().join("explicit.md");
    let original = "only explicit frozen {{keep_this}} instruction";
    std::fs::write(&instruction, original).unwrap();
    std::fs::write(
        workspace.join("AGENTS.md"),
        "ambient-context-must-not-appear",
    )
    .unwrap();
    let session_path = workspace.join("instruction.jsonl");
    let agent = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let observed_session = session_path.clone();
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = execution_http_request(&mut stream).await;
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        let system = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .unwrap();
        assert!(system["content"].as_str().unwrap().contains(original));
        assert!(!body.to_string().contains("ambient-context-must-not-appear"));
        assert!(!body.to_string().contains("changed-after-freeze"));
        let entries: Vec<Value> = std::fs::read_to_string(&observed_session)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let binding = entries
            .iter()
            .position(|entry| {
                entry.pointer("/data/key") == Some(&json!("rpi.resource-selection.v1"))
            })
            .unwrap();
        let admission = entries
            .iter()
            .position(|entry| entry["customType"] == "rpi.host.admission.v1")
            .unwrap();
        // Assert native-value provenance is already present, not a late write
        // after the Provider has consumed the prompt.
        assert_eq!(
            entries[binding]["data"]["value"]["inventory"]["skills"],
            json!([])
        );
        assert_eq!(
            entries[binding]["data"]["value"]["inventory"]["extensions"],
            json!([])
        );
        assert!(
            entries[binding]["data"]["value"]["inventory"]["toolSchemasSha256"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
        );
        assert!(binding < admission);
        let reply = sse_completion();
        use tokio::io::AsyncWriteExt;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        // Keep the endpoint alive to detect accidental Provider work on reopen.
        tokio::select! {
            _ = listener.accept() => panic!("unexpected Provider request during reopen/replay"),
            result = finished => result.unwrap(),
        }
    });
    let profile = fixture.root.path().join("resources.json");
    std::fs::write(&profile, json!({
        "agent":{"id":agent,"name":"instruction-agent","cwd":workspace,"session":{"id":"instruction-session","path":session_path},"model":{"provider":"fixture","model":"host-fixture"},"tools":[]},
        "runtime":{"providers":{"fixture":{"api":"openai-completions","baseUrl":url,"apiKey":"instruction-private-key","models":[{"id":"host-fixture","contextWindow":32768,"maxTokens":1024}]}}},
        "providerValues":{"NO_PROXY":"*"},"toolEnvironment":{"HOME":workspace,"PATH":"/usr/bin:/bin"},"shell":"/bin/sh","thinking":"off","timeoutMs":10000,
        "resources":{"version":1,"tools":{"mode":"none"},"instructions":[instruction]}
    }).to_string()).unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).unwrap();
    fixture.value["agents"] = json!([profile]);
    fixture.save();
    let (child, _) = fixture.start().await;
    let initial = std::fs::read_to_string(&session_path).unwrap();
    assert_eq!(initial.matches("rpi.resource-selection.v1").count(), 1);
    assert!(!initial.contains("instruction-private-key"));
    let inspected = fixture.data("inspect", &["--agent", &agent]).await;
    let identity = inspected["spec"]["launch"]["resource_profile"].clone();
    assert!(identity.as_str().unwrap().starts_with("instructions-v1:"));
    // The running owner keeps frozen text, never rereads this replacement.
    std::fs::write(&instruction, "changed-after-freeze").unwrap();
    let connection = ConnectionConfig {
        socket_path: fixture.socket(),
        host_id: HostId::try_from(fixture.value["host_id"].as_str().unwrap().to_string()).unwrap(),
        token: AuthToken::new(fixture.value["token"].as_str().unwrap().to_string()).unwrap(),
        timeout: DEADLINE,
    };
    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id: WireAgent = agent.clone().try_into().unwrap();
    let request_id = RequestId::new();
    assert_eq!(
        ipc.call(Command::Submit {
            agent_id,
            request_id,
            prompt: "use frozen instructions".into()
        })
        .await
        .unwrap()["durable"],
        true
    );
    tokio::time::timeout(DEADLINE, async {
        loop {
            let receipt = ipc
                .call(Command::Receipt {
                    agent_id,
                    request_id,
                })
                .await
                .unwrap();
            if receipt["execution"]["finished"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    ipc.detach();
    drop(ipc);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let before = std::fs::read(&session_path).unwrap();
    // A newly frozen different declaration cannot replace saved authority.
    let rejected = fixture.call("serve", &[]).await;
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("host_ready"));
    assert_eq!(std::fs::read(&session_path).unwrap(), before);
    assert!(!fixture.socket().exists());
    std::fs::write(&instruction, original).unwrap();
    let (child, _) = fixture.start().await;
    assert_eq!(
        fixture.data("inspect", &["--agent", &agent]).await["spec"]["launch"]["resource_profile"],
        identity
    );
    assert_eq!(
        std::fs::read_to_string(&session_path)
            .unwrap()
            .matches("rpi.resource-selection.v1")
            .count(),
        1
    );
    let mut ipc = HostClient::connect(connection).await.unwrap();
    let replay = ipc
        .call(Command::Submit {
            agent_id,
            request_id,
            prompt: "use frozen instructions".into(),
        })
        .await
        .unwrap();
    assert_eq!(replay["durable"], true);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    done.send(()).unwrap();
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
}
