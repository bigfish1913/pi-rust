// Actual administrative CLI processes + the existing local HTTP/native fixtures.
fn cli_request_file(fixture: &Fixture, request: &Value) -> PathBuf {
    let path = fixture.root.path().join(format!(
        "intent-{}.json",
        request["request_id"].as_str().unwrap()
    ));
    std::fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}
async fn cli_request_result(
    fixture: &Fixture,
    operation: &str,
    path: &std::path::Path,
    code: i32,
) -> Value {
    let before = std::fs::read(path).unwrap();
    let output = fixture
        .call(operation, &["--request", path.to_str().unwrap()])
        .await;
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("administration-test-private-key"));
    assert_eq!(std::fs::read(path).unwrap(), before);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_administrative_cli_runs_http_reconciles_old_intents_and_handles_lost_stdout()
{
    use execution_wire::{AdministrativeAction as A, Command as C};
    let mut fixture = Fixture::new();
    let (agent, profile_path, mut profile) = administrative_profile(&fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let provider = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for round in 0..2 {
            let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
                .await
                .unwrap()
                .unwrap();
            let request = execution_http_request(&mut stream).await;
            assert!(request.contains("Bearer administration-test-private-key"));
            assert!(request.contains(&format!("cli-wire-{round}")));
            let reply = sse_completion();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["administration"] = json!({"scope":scope,"profiles":[profile_path],"allowed_agents":[agent],"permissions":["create","restore","close"]});
    fixture.save();
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    let discovered = fixture.data("profiles", &["--limit", "1"]).await;
    let reference: execution_wire::ProfileRef =
        serde_json::from_value(discovered["profiles"][0].clone()).unwrap();
    assert_eq!(discovered["scope"], json!(scope));
    let create = execution_intent(&client, scope, A::CreateProfile { profile: reference });
    let create_path = cli_request_file(&fixture, &json!(create));
    let created = cli_request_result(&fixture, "administrative-create", &create_path, 0).await;
    execution_succeeded(&created);
    assert_eq!(
        cli_request_result(&fixture, "administrative-create", &create_path, 0).await,
        created
    );
    let missing = execution_intent(&client, scope, create.action.clone());
    let missing_path = cli_request_file(&fixture, &json!(missing));
    assert_eq!(
        cli_request_result(&fixture, "administrative-receipt", &missing_path, 3).await["found"],
        false
    );
    let mut restored = None;
    for round in 0..2 {
        let request_id = execution_wire::RequestId::new();
        let submission_path = cli_request_file(
            &fixture,
            &json!({"agent_id": reference.agent_id, "request_id": request_id, "prompt": format!("cli-wire-{round}")}),
        );
        let admitted = cli_request_result(&fixture, "submit", &submission_path, 0).await;
        assert_eq!(admitted["durable"], true);
        tokio::time::timeout(DEADLINE, async {
            loop {
                if cli_request_result(&fixture, "receipt", &submission_path, 0).await["execution"]
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
        let revision = execution_revision(&mut client, reference.agent_id).await;
        let close = execution_intent(
            &client,
            scope,
            A::Close {
                agent_id: reference.agent_id,
                revision,
            },
        );
        let close_path = cli_request_file(&fixture, &json!(close));
        if round == 1 {
            // A broken stdout after remote commit must be unresolved, never an
            // invitation to generate another request. The original file survives.
            let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            drop(reader);
            let fd: std::os::fd::OwnedFd = writer.into();
            let output = tokio::time::timeout(
                DEADLINE,
                fixture
                    .command()
                    .args(["host", "administrative-close", "--config"])
                    .arg(&fixture.config)
                    .arg("--request")
                    .arg(&close_path)
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
        } else {
            execution_succeeded(
                &cli_request_result(&fixture, "administrative-close", &close_path, 0).await,
            );
        }
        let queried = cli_request_result(&fixture, "administrative-receipt", &close_path, 0).await;
        execution_succeeded(&queried);
        execution_succeeded(
            &cli_request_result(&fixture, "administrative-close", &close_path, 0).await,
        );
        assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 0);
        if round == 0 {
            let revision = execution_revision(&mut client, reference.agent_id).await;
            let restore = execution_intent(
                &client,
                scope,
                A::RestoreProfile {
                    profile: reference,
                    revision,
                },
            );
            let path = cli_request_file(&fixture, &json!(restore));
            let receipt = cli_request_result(&fixture, "administrative-restore", &path, 0).await;
            execution_succeeded(&receipt);
            restored = Some((path, receipt));
        }
    }
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let session = fixture.root.path().join("dynamic.jsonl");
    let bytes = std::fs::read(&session).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(
        text.contains("cli-wire-0")
            && text.contains("cli-wire-1")
            && text.contains("binary-profile-answer")
    );
    assert!(!text.contains("administration-test-private-key"));
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let (child, _) = fixture.start().await;
    let fresh = fixture.data("profiles", &[]).await;
    assert_ne!(fresh["profiles"][0]["version"], json!(reference.version));
    // A query process must not re-read serve-only profiles or reconstruct a new
    // request from this daemon's epoch/version, even if the file is now invalid.
    std::fs::write(profile_path, b"invalid-serve-only-profile").unwrap();
    assert_eq!(
        cli_request_result(&fixture, "administrative-create", &create_path, 0).await,
        created
    );
    assert_eq!(
        cli_request_result(&fixture, "administrative-receipt", &create_path, 0).await["receipt"],
        created["receipt"]
    );
    let (restore_path, receipt) = restored.unwrap();
    assert_eq!(
        cli_request_result(&fixture, "administrative-restore", &restore_path, 0).await,
        receipt
    );
    assert_eq!(fixture.data("status", &[]).await["resident"], 0);
    assert_eq!(std::fs::read(session).unwrap(), bytes);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_administrative_cli_rejects_unsafe_or_wrong_intents_before_connecting() {
    administrative_rejections_before_connecting("administrative-create").await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_cancel_cli_rejects_unsafe_or_unstable_targets_before_connecting() {
    administrative_rejections_before_connecting("administrative-cancel").await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_stop_cli_rejects_unsafe_or_incomplete_intents_before_connecting() {
    administrative_rejections_before_connecting("administrative-stop").await;
}
async fn administrative_rejections_before_connecting(operation: &str) {
    let cancel = operation == "administrative-cancel";
    let stop = operation == "administrative-stop";
    let fixture = Fixture::new();
    let listener = tokio::net::UnixListener::bind(fixture.socket()).unwrap();
    let mut original = json!({"scope":execution_wire::ControlScopeId::new(),"request_id":execution_wire::RequestId::new(),"epoch":execution_wire::Epoch::new(),"action":{"op":"create_profile","profile":{"agent_id":execution_wire::AgentId::new(),"version":execution_wire::ProfileVersion::new()}}});
    if cancel {
        original["action"] = json!({"op":"cancel_submitted","agent_id":execution_wire::AgentId::new(),"revision":1,"submitted_request_id":execution_wire::RequestId::new()});
    }
    if stop {
        original["action"] = json!({"op":"stop"});
    }
    let mut cases = vec![
        "action",
        "grant",
        "nil",
        "revision",
        "oversize",
        "malformed",
        "public",
        "symlink",
        "hardlink",
        "directory",
        "relative",
        "missing",
        "rewrite-epoch",
    ];
    if cancel {
        cases.extend([
            "nil-target",
            "missing-target",
            "bad-target",
            "hybrid-ticket",
            "overflow",
        ]);
    }
    if stop {
        cases.extend([
            "nil-request",
            "nil-epoch",
            "missing-id",
            "missing-epoch",
            "missing-scope",
            "hybrid-agent",
            "hybrid-ticket",
        ]);
    }
    for case in cases {
        let mut request = original.clone();
        let path = fixture.root.path().join(format!("bad-{case}.json"));
        match case {
            "action" if stop => {
                request["action"] =
                    json!({"op":"close","agent_id":execution_wire::AgentId::new(),"revision":1})
            }
            "action" => request["action"] = json!({"op":"stop"}),
            "grant" => request["grant"] = json!({"create":true}),
            "nil" => request["scope"] = json!(uuid::Uuid::nil().to_string()),
            "revision" if cancel => request["action"]["revision"] = json!(0),
            "revision" if stop => request["action"]["revision"] = json!(1),
            "nil-request" => request["request_id"] = json!(uuid::Uuid::nil().to_string()),
            "nil-epoch" => request["epoch"] = json!(uuid::Uuid::nil().to_string()),
            "missing-id" => {
                request.as_object_mut().unwrap().remove("request_id");
            }
            "missing-epoch" => {
                request.as_object_mut().unwrap().remove("epoch");
            }
            "missing-scope" => {
                request.as_object_mut().unwrap().remove("scope");
            }
            "hybrid-agent" => request["action"]["agent_id"] = json!(execution_wire::AgentId::new()),
            "revision" => {
                request["action"] = json!({"op":"restore_profile","profile":original["action"]["profile"],"revision":0})
            }
            "nil-target" => {
                request["action"]["submitted_request_id"] = json!(uuid::Uuid::nil().to_string())
            }
            "missing-target" => {
                request["action"]
                    .as_object_mut()
                    .unwrap()
                    .remove("submitted_request_id");
            }
            "bad-target" => request["action"]["submitted_request_id"] = json!(7),
            "hybrid-ticket" => {
                request["action"]["ticket_id"] = json!(execution_wire::TicketId::new())
            }
            "overflow" => request["action"]["revision"] = json!(i64::MAX as u64 + 1),
            _ => {}
        }
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut input = path.clone();
        match case {
            "oversize" => std::fs::write(&path, vec![b' '; 4097]).unwrap(),
            "malformed" => std::fs::write(&path, b"PRIVATE_BAD_JSON_MUST_NOT_ECHO").unwrap(),
            "public" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            "symlink" => {
                input = fixture.root.path().join("symlink.json");
                symlink(&path, &input).unwrap();
            }
            "hardlink" => {
                input = fixture.root.path().join("hardlink.json");
                std::fs::hard_link(&path, &input).unwrap();
            }
            "directory" => input = fixture.root.path().to_owned(),
            "relative" => input = PathBuf::from("relative.json"),
            "missing" => input = fixture.root.path().join("nonexistent.json"),
            _ => {}
        }
        let mut flags = vec!["--request", input.to_str().unwrap()];
        if case == "rewrite-epoch" {
            flags.extend(["--epoch", "ignored"]);
        }
        let output = fixture.call(operation, &flags).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_BAD_JSON_MUST_NOT_ECHO"));
        assert!(!fixture.directory().exists());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "invalid input reached IPC"
    );
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_administrative_cli_preserves_default_denial_and_unsupported_capabilities() {
    use execution_wire::AdministrativeAction as A;
    for administrative in [false, true] {
        let mut fixture = Fixture::new();
        let (agent, profile_path, _) = administrative_profile(&fixture);
        let scope = execution_wire::ControlScopeId::new();
        if administrative {
            fixture.value["administration"] = json!({"scope":scope,"profiles":[profile_path]});
            fixture.save();
        }
        let (child, _) = fixture.start().await;
        let client = administrative_client(&fixture).await;
        let reference = execution_wire::ProfileRef {
            agent_id: agent.to_string().try_into().unwrap(),
            version: execution_wire::ProfileVersion::new(),
        };
        for (operation, action) in [
            (
                "administrative-create",
                A::CreateProfile { profile: reference },
            ),
            (
                "administrative-restore",
                A::RestoreProfile {
                    profile: reference,
                    revision: 1,
                },
            ),
            (
                "administrative-close",
                A::Close {
                    agent_id: reference.agent_id,
                    revision: 1,
                },
            ),
            (
                "administrative-cancel",
                A::CancelSubmitted {
                    agent_id: reference.agent_id,
                    revision: 1,
                    submitted_request_id: execution_wire::RequestId::new(),
                },
            ),
            ("administrative-stop", A::Stop {}),
            ("administrative-receipt", A::Stop {}),
        ] {
            let intent = execution_intent(&client, scope, action);
            let path = cli_request_file(&fixture, &json!(intent));
            let output = fixture
                .call(operation, &["--request", path.to_str().unwrap()])
                .await;
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains(if administrative {
                "Unauthorized"
            } else {
                "UnsupportedCapability"
            }));
            assert!(!diagnostic.contains("administration-test-private-key"));
        }
        assert_eq!(fixture.data("status", &[]).await["registered"], 0);
        assert!(!fixture.root.path().join("dynamic.jsonl").exists());
        drop(client);
        fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
        fixture.no_resources();
    }
}
