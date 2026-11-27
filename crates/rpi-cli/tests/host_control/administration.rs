// Included by host_control.rs, sharing its required actual-binary fixture.
fn administrative_profile(fixture: &Fixture) -> (AgentId, PathBuf, Value) {
    let root = fixture.root.path().canonicalize().unwrap();
    let agent = AgentId::new();
    let path = root.join("dynamic-profile.json");
    let profile = json!({
        "agent": {"id": agent, "name": "binary-dynamic", "cwd": root,
            "session": {"id": "binary-dynamic", "path": root.join("dynamic.jsonl")},
            "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []},
        "runtime": {"providers": {"fixture": {"api": "openai-completions",
            "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "administration-test-private-key",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]}}},
        "toolEnvironment": {"HOME": root, "PATH": "/usr/bin:/bin"}, "shell": "/bin/sh", "thinking": "off"
    });
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (agent, path, profile)
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_configures_dynamic_profiles_without_autoload_or_legacy_capabilities() {
    let mut fixture = Fixture::new();
    let (agent, path, _) = administrative_profile(&fixture);
    fixture.value["administration"] = json!({"scope": rpi_cli::host::ControlScopeId::new(), "profiles": [path], "allowed_agents": [agent], "permissions": ["create", "restore", "cancel", "close", "stop"], "max_jobs": 2});
    fixture.save();
    for signal in [
        nix::sys::signal::Signal::SIGTERM,
        nix::sys::signal::Signal::SIGINT,
    ] {
        let (child, _) = fixture.start().await;
        let mut client = administrative_client(&fixture).await;
        let profiles = client
            .call(rpi_cli::host::protocol::Command::Profiles {
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        assert_eq!(profiles["scope"], fixture.value["administration"]["scope"]);
        assert_eq!(profiles["profiles"].as_array().unwrap().len(), 1);
        assert_eq!(profiles["profiles"][0]["agent_id"], json!(agent));
        assert_eq!(profiles["profiles"][0].as_object().unwrap().len(), 2);
        assert!(profiles["next_cursor"].is_null());
        assert!(!profiles
            .to_string()
            .contains("administration-test-private-key"));
        drop(client);
        let status = fixture.data("status", &[]).await;
        assert_eq!(status["registered"], 0);
        assert_eq!(status["resident"], 0);
        assert!(!fixture.root.path().join("dynamic.jsonl").exists());
        assert!(!fixture
            .root
            .path()
            .join("dynamic.jsonl.progress.jsonl")
            .exists());
        // Query clients must not reread any profile or reconstitute authority.
        let hidden = fixture.root.path().join("profile-hidden-during-query.json");
        std::fs::rename(&path, &hidden).unwrap();
        assert_eq!(fixture.data("status", &[]).await["registered"], 0);
        std::fs::rename(hidden, &path).unwrap();
        fixture.stop(child, signal).await;
    }
    fixture.no_resources();
}

async fn administrative_client(fixture: &Fixture) -> rpi_cli::host::client::HostClient {
    use rpi_cli::host::{
        client::{ConnectionConfig, HostClient},
        protocol::AuthToken,
    };
    HostClient::connect(ConnectionConfig {
        socket_path: fixture.socket(),
        host_id: serde_json::from_value(fixture.value["host_id"].clone()).unwrap(),
        token: AuthToken::new(fixture.value["token"].as_str().unwrap().into()).unwrap(),
        timeout: DEADLINE,
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_reads_interrupted_administrative_history_without_taking_fresh_custody() {
    use rpi_cli::host::{
        lifecycle::{LifecycleAction, LifecycleReceipt, LifecycleRequest, LifecycleState},
        protocol as wire, ControlScopeId, RequestId,
    };
    let mut fixture = Fixture::new();
    let (agent, path, _) = administrative_profile(&fixture);
    let scope = ControlScopeId::new();
    fixture.value["administration"] = json!({"scope": scope, "profiles": [path], "allowed_agents": [agent], "permissions": ["create"]});
    fixture.save();
    // Seed a real pending intent, not a fabricated success. The actual binary
    // must classify it Interrupted and only reconcile, never load or replay it.
    let catalog = HostCatalog::open(fixture.directory(), HostConfig::default())
        .await
        .unwrap();
    let intent = LifecycleRequest {
        scope,
        request_id: RequestId::new(),
        epoch: catalog.epoch(),
        action: LifecycleAction::Create { agent_id: agent },
    };
    assert!(
        catalog
            .prepare_lifecycle(intent.clone())
            .await
            .unwrap()
            .fresh
    );
    catalog.shutdown().await.unwrap();
    let request_file = cli_request_file(&fixture, &json!(intent));
    for signal in [
        nix::sys::signal::Signal::SIGTERM,
        nix::sys::signal::Signal::SIGINT,
    ] {
        let (child, _) = fixture.start().await;
        let mut client = administrative_client(&fixture).await;
        let cli_receipt =
            cli_request_result(&fixture, "administrative-receipt", &request_file, 3).await;
        assert_eq!(cli_receipt["receipt"]["status"]["state"], "interrupted");
        let query = wire::Command::AdministrativeReceipt {
            request: serde_json::from_value(json!(intent)).unwrap(),
        };
        let data = client.call(query).await.unwrap();
        assert_eq!(data["found"], true);
        let receipt: LifecycleReceipt = serde_json::from_value(data["receipt"].clone()).unwrap();
        assert_eq!(receipt.request, intent);
        assert_eq!(receipt.status, LifecycleState::Interrupted {});
        assert_ne!(client.welcome().epoch.to_string(), intent.epoch.to_string());
        assert_eq!(
            client.call(wire::Command::Status {}).await.unwrap()["registered"],
            0
        );
        assert!(!fixture.root.path().join("dynamic.jsonl").exists());
        drop(client);
        fixture.stop(child, signal).await;
    }
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_rejects_bad_private_administration_before_host_or_session_artifacts() {
    for case in [
        "scope",
        "zero-jobs",
        "too-many-jobs",
        "unknown-permission",
        "duplicate-permission",
        "duplicate-agent",
        "unknown-agent",
        "relative-profile",
        "unknown-field",
        "peer-grant",
        "profile-public",
        "model",
        "cross-startup-duplicate",
        "profile-budget",
        "agent-budget",
    ] {
        let mut fixture = Fixture::new();
        let (agent, path, mut profile) = administrative_profile(&fixture);
        let mut policy = json!({"scope": rpi_cli::host::ControlScopeId::new(), "profiles": [path], "allowed_agents": [agent], "permissions": ["create"]});
        match case {
            "scope" => policy["scope"] = json!("invalid-scope"),
            "zero-jobs" => policy["max_jobs"] = json!(0),
            "too-many-jobs" => policy["max_jobs"] = json!(65),
            "unknown-permission" => policy["permissions"] = json!(["approve-everything"]),
            "duplicate-permission" => policy["permissions"] = json!(["create", "create"]),
            "duplicate-agent" => policy["allowed_agents"] = json!([agent, agent]),
            "unknown-agent" => policy["allowed_agents"] = json!([AgentId::new()]),
            "relative-profile" => policy["profiles"] = json!(["relative-profile.json"]),
            "unknown-field" => policy["allowEverything"] = json!(true),
            "peer-grant" => policy["grant"] = json!({"scope": policy["scope"], "stop": true}),
            "profile-public" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            "model" => {
                profile["agent"]["model"]["model"] = json!("not-an-exact-model");
                std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
            }
            "cross-startup-duplicate" => fixture.value["agents"] = json!([path]),
            "profile-budget" => policy["profiles"] = json!(vec!["/missing-profile"; 1025]),
            "agent-budget" => {
                policy["allowed_agents"] =
                    json!((0..1025).map(|_| AgentId::new()).collect::<Vec<_>>())
            }
            _ => unreachable!(),
        }
        fixture.value["administration"] = policy;
        fixture.save();
        let output = fixture.call("serve", &[]).await;
        assert!(!output.status.success(), "{case}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("host_ready"),
            "{case}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("administration-test-private-key"),
            "{case}"
        );
        assert!(
            !fixture.directory().exists(),
            "{case} created a catalog before full preflight"
        );
        assert!(!fixture.socket().exists(), "{case}");
        assert!(
            !fixture.root.path().join("dynamic.jsonl").exists(),
            "{case}"
        );
        fixture.no_resources();
    }
}
