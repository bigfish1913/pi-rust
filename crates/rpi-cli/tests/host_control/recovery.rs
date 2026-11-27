// Included by host_control.rs. Exercises the installed CLI entry point and real SQLite/JSONL.
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_recovers_authorized_members_without_replaying_and_retains_explicit_removal()
{
    use execution_wire::{AdministrativeAction as A, Command as C};
    use nix::sys::signal::Signal;
    let mut fixture = Fixture::new();
    let (dynamic, dynamic_path, mut profile) = administrative_profile(&fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", listener.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    let bytes = serde_json::to_vec(&profile).unwrap();
    std::fs::write(&dynamic_path, &bytes).unwrap();
    let fixed = AgentId::new();
    let fixed_path = fixture.root.path().join("fixed-profile.json");
    let fixed_session = fixture.root.path().join("fixed.jsonl");
    profile["agent"]["id"] = json!(fixed);
    profile["agent"]["session"] = json!({"id":"fixed", "path":fixed_session});
    std::fs::write(&fixed_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    std::fs::set_permissions(&fixed_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["agents"] = json!([fixed_path]);
    fixture.value["recovery"] = json!({"restore_on_start": true});
    fixture.value["administration"] = json!({"scope": scope, "profiles": [dynamic_path],
        "allowed_agents": [fixed, dynamic], "permissions": ["create", "restore", "close", "stop"]});
    fixture.save();

    let (mut child, ready) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 1);
    let refs = client
        .call(C::Profiles {
            after: None,
            limit: 2,
        })
        .await
        .unwrap();
    let profiles: Vec<execution_wire::ProfileRef> =
        serde_json::from_value(refs["profiles"].clone()).unwrap();
    let dynamic = profiles
        .iter()
        .find(|reference| json!(reference.agent_id) == json!(dynamic))
        .unwrap();
    let fixed = profiles
        .iter()
        .find(|reference| json!(reference.agent_id) == json!(fixed))
        .unwrap();
    let create = execution_intent(&client, scope, A::CreateProfile { profile: *dynamic });
    execution_succeeded(&client.call(execution_command(&create)).await.unwrap());
    let request_id = execution_wire::RequestId::new();
    let submission = client
        .call(C::Submit {
            agent_id: dynamic.agent_id,
            request_id,
            prompt: "unfinished-before-whole-host-restart".into(),
        })
        .await
        .unwrap();
    assert_eq!(submission["durable"], true);
    let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(execution_http_request(&mut stream)
        .await
        .contains("unfinished-before-whole-host-restart"));
    // The fixture observed a real request and withheld its response. Foreground
    // recovery runs only on a new explicit launch, after old ownership ends.
    drop(client);
    child.kill().await.unwrap();
    assert!(!child.wait().await.unwrap().success());
    drop(stream);
    std::fs::remove_file(fixture.socket()).unwrap();

    // Current authority and exact profile bytes are checked before any member
    // loads. Missing history also rejects the whole launch.
    let permissions = fixture.value["administration"]["permissions"].clone();
    fixture.value["administration"]["permissions"] = json!(["create", "close", "stop"]);
    fixture.save();
    assert_recovery_launch_rejected(&fixture).await;
    fixture.value["administration"]["permissions"] = permissions;
    fixture.save();
    let mut changed = bytes.clone();
    changed.push(b'\n');
    std::fs::write(&dynamic_path, changed).unwrap();
    assert_recovery_launch_rejected(&fixture).await;
    std::fs::write(&dynamic_path, bytes).unwrap();
    let history = fixture.root.path().join("dynamic.jsonl");
    let hidden = fixture.root.path().join("hidden.jsonl");
    std::fs::rename(&history, &hidden).unwrap();
    assert_recovery_launch_rejected(&fixture).await;
    assert!(!history.exists());
    std::fs::rename(hidden, &history).unwrap();

    let (child, next) = fixture.start().await;
    assert_ne!(ready["control"]["epoch"], next["control"]["epoch"]);
    let mut client = administrative_client(&fixture).await;
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 2);
    let answer = client
        .call(C::Result {
            agent_id: dynamic.agent_id,
            request_id,
            run_id: None,
            offset: 0,
            max_bytes: 4096,
        })
        .await
        .unwrap();
    assert_eq!(answer["result"]["state"], "unknown");
    let revision = execution_revision(&mut client, fixed.agent_id).await;
    let close = execution_intent(
        &client,
        scope,
        A::Close {
            agent_id: fixed.agent_id,
            revision,
        },
    );
    execution_succeeded(&client.call(execution_command(&close)).await.unwrap());
    let stop = execution_intent(&client, scope, A::Stop {});
    execution_succeeded(&client.call(execution_command(&stop)).await.unwrap());
    assert!(tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
        .status
        .success());
    assert!(!fixture.socket().exists());

    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 1);
    assert_eq!(
        client
            .call(C::Inspect {
                agent_id: fixed.agent_id
            })
            .await
            .unwrap()["state"],
        "closed"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
            .await
            .is_err()
    );
    drop(client);
    fixture.stop(child, Signal::SIGTERM).await;
}

async fn assert_recovery_launch_rejected(fixture: &Fixture) {
    let output = fixture.call("serve", &[]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("startup Agent profile was rejected"),
        "{stderr}"
    );
    assert!(!stderr.contains("administration-test-private-key"));
    assert!(!fixture.socket().exists());
}
