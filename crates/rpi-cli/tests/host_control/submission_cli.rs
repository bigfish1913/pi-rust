// Actual CLI clients with a gated native HTTP request; not management-DLL E2E.
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_submission_cli_admits_before_completion_and_reconciles_without_replay() {
    use tokio::io::AsyncWriteExt;
    let mut fixture = Fixture::new();
    let (agent, profile_path, mut profile) = administrative_profile(&fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    fixture.value["agents"] = json!([profile_path]);
    fixture.save();
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let body = execution_http_request(&mut stream).await;
        assert!(body.contains("Bearer administration-test-private-key"));
        assert!(body.contains("submission-cli-wire"));
        arrived.send(()).unwrap();
        released.await.unwrap();
        let reply = sse_completion();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let (child, _) = fixture.start().await;
    let original = json!({"agent_id":agent,"request_id":execution_wire::RequestId::new(),"prompt":"submission-cli-wire"});
    let path = cli_request_file(&fixture, &original);
    // Exit zero is durable custody even though the real Provider cannot finish.
    let admitted = cli_request_result(&fixture, "submit", &path, 0).await;
    assert_eq!(admitted["durable"], true);
    tokio::time::timeout(DEADLINE, arrival)
        .await
        .unwrap()
        .unwrap();
    let running = cli_request_result(&fixture, "receipt", &path, 0).await;
    assert_eq!(running["execution"]["state"], "started");
    assert_eq!(running["execution"]["finished"], false);
    assert_eq!(running["receipt"], admitted["receipt"]);
    assert_eq!(
        cli_request_result(&fixture, "submit", &path, 0).await,
        admitted
    );
    // A real lost stdout for an identical duplicate remains unresolved locally.
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    let fd: std::os::fd::OwnedFd = writer.into();
    let output = tokio::time::timeout(
        DEADLINE,
        fixture
            .command()
            .args(["host", "submit", "--config"])
            .arg(&fixture.config)
            .arg("--request")
            .arg(&path)
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
    let mut conflict = original.clone();
    conflict["prompt"] = json!("different-input-must-not-run");
    let conflict_path = fixture.root.path().join("conflict.json");
    std::fs::write(&conflict_path, serde_json::to_vec(&conflict).unwrap()).unwrap();
    std::fs::set_permissions(&conflict_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let rejected = fixture
        .call("submit", &["--request", conflict_path.to_str().unwrap()])
        .await;
    assert_eq!(rejected.status.code(), Some(1));
    // Existing ordinary BackendError::Admission maps to InvalidRequest, not the
    // separate administrative journal's Conflict code. Preserve that contract.
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("InvalidRequest"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    // A lookup using wrong original content must not claim successful custody.
    let wrong = fixture
        .call("receipt", &["--request", conflict_path.to_str().unwrap()])
        .await;
    assert_eq!(wrong.status.code(), Some(3));
    assert!(wrong.stdout.is_empty());
    let mut missing = original.clone();
    missing["request_id"] = json!(execution_wire::RequestId::new());
    let missing_path = cli_request_file(&fixture, &missing);
    assert_eq!(
        cli_request_result(&fixture, "receipt", &missing_path, 3).await["found"],
        false
    );
    release.send(()).unwrap();
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let finished = tokio::time::timeout(DEADLINE, async {
        loop {
            let value = cli_request_result(&fixture, "receipt", &path, 0).await;
            if value["execution"]["finished"] == true {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished["receipt"], admitted["receipt"]);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let session = fixture.root.path().join("dynamic.jsonl");
    let before = std::fs::read(&session).unwrap();
    let text = std::str::from_utf8(&before).unwrap();
    assert!(text.contains("binary-profile-answer"));
    assert!(
        !text.contains("different-input-must-not-run")
            && !text.contains("administration-test-private-key")
    );
    assert_eq!(text.matches("rpi.host.admission.v1").count(), 1);
    let (child, _) = fixture.start().await;
    assert_eq!(
        cli_request_result(&fixture, "receipt", &path, 0).await,
        finished
    );
    assert_eq!(
        cli_request_result(&fixture, "submit", &path, 0).await,
        admitted
    );
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    assert_eq!(std::fs::read(session).unwrap(), before);
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_submission_cli_rejects_bad_original_files_without_any_connection() {
    let fixture = Fixture::new();
    let listener = tokio::net::UnixListener::bind(fixture.socket()).unwrap();
    let original = json!({"agent_id":execution_wire::AgentId::new(),"request_id":execution_wire::RequestId::new(),"prompt":"original-private-prompt-must-not-echo"});
    for case in [
        "empty",
        "whitespace",
        "utf8-budget",
        "native-budget",
        "unknown",
        "nil",
        "missing-prompt",
        "malformed",
        "file-budget",
        "public",
        "symlink",
        "relative",
        "missing-file",
        "prompt-flag",
    ] {
        let mut value = original.clone();
        match case {
            "empty" => value["prompt"] = json!(""),
            "whitespace" => value["prompt"] = json!(" \n"),
            "utf8-budget" => {
                value["prompt"] =
                    json!("你".repeat(execution_wire::MAX_SUBMIT_PROMPT_BYTES / 3 + 1))
            }
            "native-budget" => value["prompt"] = json!("\u{1}x".repeat(110_000)),
            "unknown" => value["grant"] = json!({"tools":["all"]}),
            "nil" => value["request_id"] = json!(uuid::Uuid::nil().to_string()),
            "missing-prompt" => {
                value.as_object_mut().unwrap().remove("prompt");
            }
            _ => {}
        }
        let path = fixture.root.path().join(format!("bad-{case}.json"));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut input = path.clone();
        match case {
            "malformed" => std::fs::write(&path, b"original-private-prompt-must-not-echo").unwrap(),
            "file-budget" => {
                std::fs::write(&path, vec![b' '; execution_wire::MAX_FRAME_BYTES + 1]).unwrap()
            }
            "public" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            "symlink" => {
                input = fixture.root.path().join("alias");
                symlink(&path, &input).unwrap();
            }
            "relative" => input = PathBuf::from("relative.json"),
            "missing-file" => input = fixture.root.path().join("absent.json"),
            _ => {}
        }
        for operation in ["submit", "receipt"] {
            let mut flags = vec!["--request", input.to_str().unwrap()];
            if case == "prompt-flag" {
                flags.extend(["--prompt", "must-not-echo"]);
            }
            let output = fixture.call(operation, &flags).await;
            assert_eq!(
                output.status.code(),
                Some(2),
                "{case}/{operation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr)
                .contains("original-private-prompt-must-not-echo"));
            assert!(!fixture.directory().exists());
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    fixture.no_resources();
}
