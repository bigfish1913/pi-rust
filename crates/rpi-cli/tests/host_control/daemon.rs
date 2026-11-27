use super::*;
use execution_wire::{AdministrativeAction as Action, Command as Wire};
use nix::{
    sys::signal::{kill, Signal},
    unistd::{getsid, Pid},
};
use rpi_cli::host::client::{ConnectionConfig, HostClient};
use tokio::io::AsyncWriteExt;

struct DetachedGuard(Pid);
impl Drop for DetachedGuard {
    fn drop(&mut self) {
        let _ = kill(self.0, Signal::SIGTERM);
    }
}

async fn detached_start(fixture: &Fixture) -> Value {
    let output = fixture.call("serve", &[]).await;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(fixture.directory().join("supervisor.log")).unwrap_or_default()
    );
    assert!(output.stderr.is_empty());
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["event"], "host_ready");
    ready
}

fn ready_worker(ready: &Value) -> Pid {
    Pid::from_raw(ready["pid"].as_i64().unwrap() as i32)
}

async fn detached_stopped(fixture: &Fixture) {
    tokio::time::timeout(DEADLINE, async {
        while fixture.socket().exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

async fn detached_next_epoch(fixture: &Fixture, previous: &Value) -> HostClient {
    let config = ConnectionConfig {
        socket_path: fixture.socket(),
        host_id: serde_json::from_value(fixture.value["host_id"].clone()).unwrap(),
        token: serde_json::from_value(fixture.value["token"].clone()).unwrap(),
        timeout: Duration::from_secs(1),
    };
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(client) = HostClient::connect(config.clone()).await {
                if json!(client.welcome().epoch) != *previous {
                    return client;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "worker never restored: {}",
            std::fs::read_to_string(fixture.directory().join("supervisor.log")).unwrap_or_default()
        )
    })
}

#[tokio::test(flavor = "current_thread")]
async fn actual_daemon_returns_after_ready_cleans_descendants_and_restores_after_sigkill() {
    let mut fixture = Fixture::new();
    let (agent, profile_path, mut profile) = administrative_profile(&fixture);
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile["runtime"]["providers"]["fixture"]["baseUrl"] =
        json!(format!("http://{}/v1", provider.local_addr().unwrap()));
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    profile["agent"]["tools"] = json!(["bash"]);
    profile["approvedTools"] = json!(["bash"]);
    std::fs::write(&profile_path, profile.to_string()).unwrap();
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["mode"] = json!("daemon");
    fixture.value["recovery"] = json!({"restore_on_start": true, "restart_on_crash": true});
    fixture.value["administration"] = json!({"scope":scope, "profiles":[profile_path],
        "allowed_agents":[agent], "permissions":["create","restore","stop"]});
    fixture.save();

    let ready = detached_start(&fixture).await;
    let worker = ready_worker(&ready);
    let supervisor = getsid(Some(worker)).unwrap();
    let _guard = DetachedGuard(supervisor);
    assert_ne!(supervisor, worker);
    assert_ne!(getsid(None).unwrap(), supervisor);
    assert_eq!(
        std::fs::metadata(fixture.directory().join("supervisor.log"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!fixture.call("serve", &[]).await.status.success());
    let mut client = administrative_client(&fixture).await;
    let profiles = client
        .call(Wire::Profiles {
            after: None,
            limit: 1,
        })
        .await
        .unwrap();
    let profile = serde_json::from_value(profiles["profiles"][0].clone()).unwrap();
    let create = execution_intent(&client, scope, Action::CreateProfile { profile });
    execution_succeeded(&client.call(execution_command(&create)).await.unwrap());
    let agent_id = agent.to_string().try_into().unwrap();
    let request_id = execution_wire::RequestId::new();
    assert_eq!(
        client
            .call(Wire::Submit {
                agent_id,
                request_id,
                prompt: "daemon-tool-exercise".into()
            })
            .await
            .unwrap()["durable"],
        true
    );

    let accept = |provider: tokio::net::TcpListener| async move {
        let (stream, _) = tokio::time::timeout(DEADLINE, provider.accept())
            .await
            .unwrap()
            .unwrap();
        (stream, provider)
    };
    let (mut stream, provider) = accept(provider).await;
    assert!(execution_http_request(&mut stream)
        .await
        .contains("daemon-tool-exercise"));
    let arguments = json!({"command":"printf 'effect\\n' >> effect.txt; /bin/sleep 30 </dev/null >/dev/null 2>&1 & echo $! > child.pid"}).to_string();
    let call = json!({"index":0,"delta":{"role":"assistant","tool_calls":[
        {"index":0,"id":"daemon-custody","type":"function","function":{"name":"bash","arguments":arguments}}
    ]},"finish_reason":null});
    let end = json!({"index":0,"delta":{},"finish_reason":"tool_calls"});
    let reply = [call, end].into_iter().map(|choice| {
        let event = json!({"id":"daemon-fixture","model":"host-fixture","object":"chat.completion.chunk","created":1,"choices":[choice]});
        format!("data: {event}\n\n")
    }).collect::<String>() + "data: [DONE]\n\n";
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\n{reply}", reply.len()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
    let (mut stream, provider) = accept(provider).await;
    assert!(execution_http_request(&mut stream)
        .await
        .contains("daemon-custody"));
    let child = Pid::from_raw(
        std::fs::read_to_string(fixture.root.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    );
    assert_eq!(getsid(Some(child)).unwrap(), supervisor);
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("effect.txt")).unwrap(),
        "effect\n"
    );
    kill(worker, Signal::SIGKILL).unwrap();
    drop(stream);
    drop(client);
    let mut client = detached_next_epoch(&fixture, &ready["control"]["epoch"]).await;
    assert_eq!(client.call(Wire::Status {}).await.unwrap()["resident"], 1);
    assert_eq!(
        client
            .call(Wire::Result {
                agent_id,
                request_id,
                run_id: None,
                offset: 0,
                max_bytes: 4096
            })
            .await
            .unwrap()["result"]["state"],
        "unknown"
    );
    let status = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &child.to_string()])
        .output()
        .unwrap();
    let child_stat = String::from_utf8(status.stdout).unwrap();
    assert!(
        child_stat.trim().is_empty() || child_stat.trim_start().starts_with('Z'),
        "{child_stat}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), provider.accept())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("effect.txt")).unwrap(),
        "effect\n"
    );
    let stop = execution_intent(&client, scope, Action::Stop {});
    execution_succeeded(&client.call(execution_command(&stop)).await.unwrap());
    detached_stopped(&fixture).await;
    let next = detached_start(&fixture).await;
    let _guard = DetachedGuard(getsid(Some(ready_worker(&next))).unwrap());
    assert_eq!(fixture.data("status", &[]).await["resident"], 1);
    kill(ready_worker(&next), Signal::SIGTERM).unwrap();
    detached_stopped(&fixture).await;
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_daemon_worker_settles_when_supervisor_is_killed() {
    let mut fixture = Fixture::new();
    fixture.value["mode"] = json!("daemon");
    fixture.value["recovery"] = json!({"restart_on_crash": true});
    fixture.save();
    let ready = detached_start(&fixture).await;
    let worker = ready_worker(&ready);
    let supervisor = getsid(Some(worker)).unwrap();
    let _guard = DetachedGuard(supervisor);
    kill(supervisor, Signal::SIGKILL).unwrap();
    detached_stopped(&fixture).await;
    let next = detached_start(&fixture).await;
    let _guard = DetachedGuard(getsid(Some(ready_worker(&next))).unwrap());
    assert_ne!(ready["control"]["epoch"], next["control"]["epoch"]);
    kill(ready_worker(&next), Signal::SIGTERM).unwrap();
    detached_stopped(&fixture).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_daemon_does_not_publish_ready_on_startup_failure() {
    let mut fixture = Fixture::new();
    fixture.value["mode"] = json!("daemon");
    fixture.value["recovery"] = json!({"restart_on_crash": true});
    fixture.value["agents"] = json!([fixture.root.path().join("missing.json")]);
    fixture.save();
    let output = fixture.call("serve", &[]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!fixture.socket().exists());
    let log = std::fs::read_to_string(fixture.directory().join("supervisor.log")).unwrap();
    assert!(!log.contains("restarting"));
    assert!(!log.contains(fixture.value["token"].as_str().unwrap()));
}
