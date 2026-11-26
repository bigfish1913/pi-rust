//! Exercise two simultaneous native sessions through one actual Host process.
#![cfg(unix)]

#[path = "support/resource_fixture.rs"]
mod resource_fixture;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

struct Client(BufReader<UnixStream>);

impl Client {
    async fn open(socket: &Path, workspace: &Path) -> (Self, Value) {
        let (mut client, opened) = Self::handshake(socket, workspace).await;
        assert_eq!(opened["type"], "opened", "{opened}");
        client.until(|frame| frame["type"] == "ready").await;
        (client, opened)
    }

    async fn handshake(socket: &Path, workspace: &Path) -> (Self, Value) {
        let mut client = Self::connect(socket).await;
        client
            .send(json!({"version": 1, "workspace": workspace}))
            .await;
        let opened = client.next().await;
        (client, opened)
    }

    async fn connect(socket: &Path) -> Self {
        let stream = loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("could not connect to test Host: {error}"),
            }
        };
        Self(BufReader::new(stream))
    }

    async fn send(&mut self, frame: Value) {
        let line = format!("{frame}\n");
        self.0.get_mut().write_all(line.as_bytes()).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.0.read_line(&mut line).await.unwrap(),
            0,
            "Host disconnected"
        );
        serde_json::from_str(&line).unwrap()
    }

    async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        loop {
            let frame = self.next().await;
            if predicate(&frame) {
                return frame;
            }
        }
    }

    async fn history(&mut self) -> String {
        self.send(json!({"type":"get_messages", "id":"history"}))
            .await;
        self.until(|frame| frame["type"] == "response" && frame["id"] == "history")
            .await
            .to_string()
    }
}

async fn provider_request(socket: &mut TcpStream) -> String {
    let mut reader = BufReader::new(socket);
    let mut body_size = None;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                body_size = Some(value.trim().parse().unwrap());
            }
        }
    }
    let mut body = vec![0; body_size.unwrap()];
    reader.read_exact(&mut body).await.unwrap();
    String::from_utf8(body).unwrap()
}

async fn chunk(socket: &mut TcpStream, data: &str) {
    let frame = format!("{:x}\r\n{data}\r\n", data.len());
    socket.write_all(frame.as_bytes()).await.unwrap();
}

fn write_fixture(agent: &Path, address: std::net::SocketAddr) {
    std::fs::write(
        agent.join("settings.json"),
        json!({"defaultProvider":"fixture", "defaultModel":"fixture"}).to_string(),
    )
    .unwrap();
    std::fs::write(agent.join("models.json"), json!({"providers":{"fixture":{
        "api":"openai-completions", "baseUrl":format!("http://{address}/v1"), "apiKey":"fixture-only",
        "models":[{"id":"fixture", "contextWindow":32768, "maxTokens":256}]
    }}}).to_string()).unwrap();
}

fn spawn_daemon(
    root: &Path,
    agent: &Path,
    area: Option<&str>,
    sessions: &Path,
) -> tokio::process::Child {
    Command::new(env!("CARGO_BIN_EXE_rpi"))
        .arg("__rpi_local_host")
        .args(area)
        .current_dir(root)
        .env("HOME", root)
        .env("RPI_CODING_AGENT_DIR", agent)
        .env_remove("RPI_EXTENSIONS_DIR")
        .env("RPI_OFFLINE", "1")
        .env("RPI_CODING_AGENT_SESSION_DIR", sessions)
        .env(
            "RPI_PLUGIN_STUB_REGISTRATION_LOG",
            root.join("registrations.log"),
        )
        .env("RPI_PLUGIN_STUB_REQUIRE_FRESH_IMAGE", "1")
        .env("NO_PROXY", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn probe() {
    let root = tempfile::Builder::new()
        .prefix("rh-")
        .tempdir_in("/tmp")
        .unwrap();
    let agent = root.path().join("agent");
    let workspace = root.path().join("project");
    std::fs::create_dir(&agent).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = provider.local_addr().unwrap();
    write_fixture(&agent, address);

    let mut daemon = spawn_daemon(root.path(), &agent, None, &root.path().join("sessions"));

    let socket = agent.join("local-host-v1/rpc.sock");
    let ((mut a, a_opened), (mut b, b_opened)) = tokio::join!(
        Client::open(&socket, &workspace),
        Client::open(&socket, &workspace),
    );
    assert_eq!(a_opened["host_pid"].as_u64(), daemon.id().map(u64::from));
    assert_eq!(a_opened["host_pid"], b_opened["host_pid"]);
    for key in ["session_id", "agent_id"] {
        assert!(a_opened[key].is_string() && b_opened[key].is_string());
        assert_ne!(a_opened[key], b_opened[key]);
    }

    let (b_release, b_gate) = oneshot::channel();
    let serve = tokio::spawn(async move {
        let (mut first, _) = provider.accept().await.unwrap();
        let (mut second, _) = provider.accept().await.unwrap();
        let (first_body, second_body) =
            tokio::join!(provider_request(&mut first), provider_request(&mut second));
        let (mut a, mut b) = if first_body.contains("prompt-alpha") {
            assert!(second_body.contains("prompt-beta"));
            (first, second)
        } else {
            assert!(first_body.contains("prompt-beta") && second_body.contains("prompt-alpha"));
            (second, first)
        };
        let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        a.write_all(headers).await.unwrap();
        b.write_all(headers).await.unwrap();
        for (tag, socket) in [("alpha", &mut a), ("beta", &mut b)] {
            // Exceed the provider's batching threshold before waiting at the gate.
            let event = json!({"choices":[{"index":0,"delta":{
                "content":format!("{tag}-live {}", "text ".repeat(24))
            },"finish_reason":null}]});
            chunk(socket, &format!("data: {event}\n\n")).await;
        }
        b_gate.await.unwrap();
        chunk(&mut b, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" beta-finished\"},\"finish_reason\":\"stop\"}]}\n\n").await;
        chunk(&mut b, "data: [DONE]\n\n").await;
        b.write_all(b"0\r\n\r\n").await.unwrap();
    });

    let prompt =
        |name: &str| json!({"type":"prompt", "id":name, "content":format!("prompt-{name}")});
    tokio::join!(a.send(prompt("alpha")), b.send(prompt("beta")));
    let live =
        |frame: &Value| frame["type"] == "message_update" && frame["eventType"] == "text_delta";
    let (alpha, beta) = tokio::join!(a.until(live), b.until(live));
    assert!(alpha.to_string().contains("alpha-live"), "{alpha}");
    assert!(beta.to_string().contains("beta-live"), "{beta}");

    a.send(json!({"type":"abort", "id":"cancel-alpha"})).await;
    let settled = |frame: &Value| frame["type"] == "agent_settled";
    let aborted = a.until(settled).await;
    assert_eq!(aborted["aborted"], true, "{aborted}");
    let alpha_history = a.history().await;
    assert!(alpha_history.contains("prompt-alpha") && !alpha_history.contains("prompt-beta"));
    drop(a);

    b_release.send(()).unwrap();
    let completed = b.until(settled).await;
    assert_eq!(completed["outcome"], "completed", "{completed}");
    let beta_history = b.history().await;
    assert!(
        beta_history.contains("beta-finished") && !beta_history.contains("prompt-alpha"),
        "{beta_history}"
    );
    serve.await.unwrap();
    drop(b);
    daemon.kill().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_host_runs_independent_concurrent_sessions() {
    timeout(Duration::from_secs(45), probe())
        .await
        .expect("shared Host probe timed out");
}

fn journals(root: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut found = Vec::new();
    for project in std::fs::read_dir(root).unwrap() {
        let project = project.unwrap().path();
        if !project.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(project).unwrap() {
            let path = file.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy();
            if path.extension().is_some_and(|ext| ext == "jsonl") && !name.contains(".progress.") {
                let journal = std::fs::read_to_string(&path).unwrap();
                found.push((path, journal));
            }
        }
    }
    found
}

async fn complete(socket: &mut TcpStream) {
    let text = "area response ".repeat(8);
    let body = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]});
    sse_reply(socket, body).await;
}

async fn sse_reply(socket: &mut TcpStream, body: Value) {
    let headers = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    socket.write_all(headers).await.unwrap();
    chunk(socket, &format!("data: {body}\n\n")).await;
    chunk(socket, "data: [DONE]\n\n").await;
    socket.write_all(b"0\r\n\r\n").await.unwrap();
}

#[tokio::test]
async fn mini_flags_run_in_the_same_host_and_use_its_capacity() {
    timeout(Duration::from_secs(30), async {
        let root = tempfile::Builder::new().prefix("rh-mini-").tempdir_in("/tmp").unwrap();
        let agent = root.path().join("agent");
        let workspace = root.path().join("project");
        let sessions = root.path().join("sessions");
        for dir in [&agent, &workspace, &sessions] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(workspace.join("context.txt"), "attached context").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "workspace instructions").unwrap();
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        write_fixture(&agent, provider.local_addr().unwrap());
        std::fs::write(
            agent.join("host.json"),
            json!({"defaultArea":"work", "areas":{"work":{
                "provider":"fixture", "model":"fixture", "tools":["read"], "maxAgents":2
            }}}).to_string(),
        ).unwrap();
        // Mini must resolve through the running area's provider and model.
        std::fs::write(agent.join("settings.json"), "{}").unwrap();
        let mut daemon = spawn_daemon(root.path(), &agent, Some("work"), &sessions);
        let socket = agent.join("local-host-v1/area-work/rpc.sock");
        let spawn = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_rpi"))
                .args(args)
                .current_dir(&workspace)
                .env("HOME", root.path())
                .env("RPI_CODING_AGENT_DIR", &agent)
                .env("RPI_OFFLINE", "1")
                .env("NO_PROXY", "127.0.0.1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        };
        let mut a = spawn(&["--host", "--mini", "@context.txt", "prompt-alpha"]);
        a.stdin.take().unwrap().write_all(b"piped alpha ").await.unwrap();
        let mut b = spawn(&["--mini", "--host=work", "--tools", "read", "--no-context-files", "prompt-beta"]);
        drop(b.stdin.take());
        let (mut first, _) = provider.accept().await.unwrap();
        let (mut second, _) = provider.accept().await.unwrap();
        let (first_body, second_body) = tokio::join!(provider_request(&mut first), provider_request(&mut second));
        let (alpha, beta) = if first_body.contains("prompt-alpha") {
            (first_body, second_body)
        } else {
            (second_body, first_body)
        };
        assert!(alpha.contains("prompt-alpha") && alpha.contains("piped alpha") && alpha.contains("attached context"), "{alpha}");
        assert!(beta.contains("prompt-beta") && !beta.contains("prompt-alpha"), "{beta}");
        assert!(alpha.contains("workspace instructions"), "{alpha}");
        assert!(!beta.contains("workspace instructions"), "{beta}");
        let alpha = serde_json::from_str::<Value>(&alpha).unwrap();
        let beta = serde_json::from_str::<Value>(&beta).unwrap();
        let default_tools = alpha["tools"].as_array().expect("default Mini tools");
        for name in ["read", "bash"] {
            assert!(default_tools.iter().any(|tool| tool["function"]["name"] == name), "{alpha}");
        }
        assert_eq!(beta["tools"].as_array().unwrap().len(), 1, "{beta}");
        assert_eq!(beta["tools"][0]["function"]["name"], "read", "{beta}");
        let (_, reply) = Client::handshake(&socket, &workspace).await;
        assert_eq!(reply["type"], "error", "{reply}");
        assert!(reply["message"].as_str().unwrap().contains("maxAgents: 2"), "{reply}");
        let ((), ()) = tokio::join!(complete(&mut first), complete(&mut second));
        let (a, b) = tokio::join!(a.wait_with_output(), b.wait_with_output());
        for finished in [a.unwrap(), b.unwrap()] {
            assert!(finished.status.success(), "{}", String::from_utf8_lossy(&finished.stderr));
            assert_eq!(finished.stdout, format!("{}\n", "area response ".repeat(8)).into_bytes());
        }
        let mut cancelled = spawn(&["--host", "work", "--mini", "prompt-cancel"]);
        drop(cancelled.stdin.take());
        let (mut request, _) = provider.accept().await.unwrap();
        assert!(provider_request(&mut request).await.contains("prompt-cancel"));
        request.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        cancelled.kill().await.unwrap();
        match request.read(&mut [0]).await {
            Ok(0) => {},
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {},
            outcome => panic!("provider stream should close on cancellation: {outcome:?}"),
        }
        assert!(journals(&sessions).is_empty());
        assert!(!agent.join("local-host-v1/area-work/sessions").exists());
        let (session, opened) = Client::open(&socket, &workspace).await;
        assert_eq!(opened["host_pid"].as_u64(), daemon.id().map(u64::from));
        drop(session);
        daemon.kill().await.unwrap();
    }).await.expect("Host Mini probe timed out");
}

async fn area_probe() {
    let root = tempfile::Builder::new()
        .prefix("rh-areas-")
        .tempdir_in("/tmp")
        .unwrap();
    let agent = root.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = provider.local_addr().unwrap();
    write_fixture(&agent, address);
    std::fs::write(
        agent.join("host.json"),
        json!({"defaultArea":"alpha", "areas":{"alpha":{"tools":["read"]}}}).to_string(),
    )
    .unwrap();

    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let default_sessions = root.path().join("default-sessions");
    let mut alpha = spawn_daemon(root.path(), &agent, None, &default_sessions);
    let mut beta = spawn_daemon(root.path(), &agent, Some("beta"), &default_sessions);
    let mut default = spawn_daemon(root.path(), &agent, Some("default"), &default_sessions);

    let base = agent.join("local-host-v1");
    let alpha_socket = base.join("area-alpha/rpc.sock");
    let beta_socket = base.join("area-beta/rpc.sock");
    let default_socket = base.join("rpc.sock");
    let ((mut a, a_opened), (a2, a2_opened), (mut b, b_opened), (mut d, d_opened)) = tokio::join!(
        Client::open(&alpha_socket, &workspace),
        Client::open(&alpha_socket, &workspace),
        Client::open(&beta_socket, &workspace),
        Client::open(&default_socket, &workspace),
    );
    for (opened, daemon) in [
        (&a_opened, &alpha),
        (&a2_opened, &alpha),
        (&b_opened, &beta),
        (&d_opened, &default),
    ] {
        assert_eq!(opened["host_pid"].as_u64(), daemon.id().map(u64::from));
    }
    a.send(json!({"type":"get_state", "id":"area-tools"})).await;
    let state = a.until(|frame| frame["id"] == "area-tools").await;
    assert_eq!(state["result"]["activeTools"], json!(["read"]), "{state}");
    for key in ["session_id", "agent_id"] {
        let seen: std::collections::HashSet<_> = [&a_opened, &a2_opened, &b_opened, &d_opened]
            .into_iter()
            .map(|opened| opened[key].as_str().expect("session identity"))
            .collect();
        assert_eq!(seen.len(), 4, "{key} must be unique per session");
    }

    let prompts = ["prompt-alpha", "prompt-beta", "prompt-default"];
    let respond = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for _ in 0..prompts.len() {
            let (mut socket, _) = provider.accept().await.unwrap();
            let body = provider_request(&mut socket).await;
            complete(&mut socket).await;
            bodies.push(body);
        }
        bodies
    });
    let prompt = |name: &str| json!({"type":"prompt", "id":name, "content":name});
    tokio::join!(
        a.send(prompt("prompt-alpha")),
        b.send(prompt("prompt-beta")),
        d.send(prompt("prompt-default")),
    );
    let settled = |frame: &Value| frame["type"] == "agent_settled";
    let (alpha_done, beta_done, default_done) =
        tokio::join!(a.until(settled), b.until(settled), d.until(settled));
    for frame in [&alpha_done, &beta_done, &default_done] {
        assert_eq!(frame["outcome"], "completed", "{frame}");
    }
    let bodies = respond.await.unwrap();
    let joined = bodies.join("\n");
    assert!(joined.contains("prompt-alpha"), "{joined}");
    assert!(joined.contains("prompt-beta"), "{joined}");
    assert!(joined.contains("prompt-default"), "{joined}");

    for (dir, opened, prompt) in [
        (base.join("area-alpha/sessions"), &a_opened, prompts[0]),
        (base.join("area-beta/sessions"), &b_opened, prompts[1]),
        (default_sessions, &d_opened, prompts[2]),
    ] {
        let id = opened["session_id"].as_str().unwrap();
        let logs = journals(&dir);
        let (_, journal) = logs
            .iter()
            .find(|(path, _)| path.file_stem().unwrap().to_string_lossy().ends_with(id))
            .unwrap_or_else(|| panic!("session {id} is missing from {}", dir.display()));
        for candidate in prompts {
            assert_eq!(journal.contains(candidate), candidate == prompt);
        }
    }
    assert!(!base.join("area-default").exists());

    drop((a, a2, b, d));
    for daemon in [&mut alpha, &mut beta, &mut default] {
        daemon.kill().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn named_areas_isolate_hosts_and_sessions() {
    timeout(Duration::from_secs(60), area_probe())
        .await
        .expect("area Host probe timed out");
}

#[test]
fn invalid_host_config_is_rejected_before_starting_daemon() {
    let root = tempfile::tempdir().unwrap();
    for config in [
        r#"{"defaultArea":"../other"}"#,
        r#"{"areas":{"Work":{}}}"#,
        r#"{"areas":{"work":{"tools":[""]}}}"#,
        r#"{"areas":{"work":{"unknown":true}}}"#,
        r#"{"areas":{"work":{"maxAgents":0}}}"#,
        r#"{"areas":{"work":{"maxAgents":257}}}"#,
        r#"{"areas":{"work":{"idleTimeoutSeconds":-1}}}"#,
    ] {
        std::fs::write(root.path().join("host.json"), config).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .args(["--host", "work"])
            .env("RPI_CODING_AGENT_DIR", root.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid Host configuration"), "{stderr}");
        assert!(!root.path().join("local-host-v1").exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_rejects_new_agents_and_idle_timeout_waits_for_settlement() {
    timeout(Duration::from_secs(25), capacity_probe())
        .await
        .expect("capacity and idle timeout probe timed out");
}

async fn capacity_probe() {
    let root = tempfile::Builder::new()
        .prefix("rh-cap-")
        .tempdir_in("/tmp")
        .unwrap();
    let agent = root.path().join("agent");
    let workspace = root.path().join("project");
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&agent).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    write_fixture(&agent, provider.local_addr().unwrap());
    std::fs::write(
        agent.join("host.json"),
        r#"{"areas":{"default":{"maxAgents":1,"idleTimeoutSeconds":1}}}"#,
    )
    .unwrap();
    let mut daemon = spawn_daemon(root.path(), &agent, None, &sessions);
    let socket = agent.join("local-host-v1/rpc.sock");
    let (mut a, opened) = Client::open(&socket, &workspace).await;
    a.send(json!({"type":"prompt", "id":"slow", "content":"settle before expiration"}))
        .await;

    let mut model = {
        let (mut model, _) = provider.accept().await.unwrap();
        assert!(provider_request(&mut model)
            .await
            .contains("settle before expiration"));
        model
    };
    sleep(Duration::from_millis(1250)).await;
    let (_, rejected) = Client::handshake(&socket, &workspace).await;
    assert_eq!(rejected["type"], "error", "{rejected}");
    assert!(
        rejected["message"]
            .as_str()
            .unwrap()
            .contains("maxAgents: 1"),
        "{rejected}"
    );
    complete(&mut model).await;
    let settled = a.until(|frame| frame["type"] == "agent_settled").await;
    assert_eq!(settled["outcome"], "completed", "{settled}");

    let expired = a.until(|frame| frame["type"] == "host_idle_timeout").await;
    assert_eq!(expired["idleTimeoutSeconds"], 1, "{expired}");
    assert!(a.0.fill_buf().await.unwrap().is_empty());
    let (mut next, replacement) = loop {
        let (client, reply) = Client::handshake(&socket, &workspace).await;
        if reply["type"] == "opened" {
            break (client, reply);
        }
        assert!(
            reply["message"].as_str().unwrap().contains("maxAgents: 1"),
            "{reply}"
        );
        sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(replacement["host_pid"], opened["host_pid"]);
    assert_ne!(replacement["agent_id"], opened["agent_id"]);
    assert_ne!(replacement["session_id"], opened["session_id"]);
    assert!(journals(&sessions)
        .iter()
        .any(|(_, journal)| journal.contains("settle before expiration")));
    next.until(|frame| frame["type"] == "ready").await;
    next.send(json!({"type":"stop", "id":"stop"})).await;
    let stopped = next.until(|frame| frame["id"] == "stop").await;
    assert_eq!(stopped["success"], true, "{stopped}");
    next.0.read_to_end(&mut Vec::new()).await.unwrap();
    daemon.kill().await.unwrap();
}

async fn open_terminal(socket: &Path, workspace: &Path, regular: bool) -> Client {
    let mut client = Client::connect(socket).await;
    client
        .send(json!({
            "version": 1,
            "workspace": workspace,
            "terminal": {
                "regular": regular, "theme": null, "no_themes": true, "images": "None",
                "geometry": {"columns": 100, "rows": 30, "width": 0, "height": 0}
            }
        }))
        .await;
    let opened = client.next().await;
    assert_eq!(opened["type"], "terminal_opened", "{opened}");
    client
}

async fn terminal_key(client: &mut Client, code: &str, modifiers: u8) {
    let code = match code {
        "Enter" | "Esc" => json!(code),
        _ => json!({"Char": code}),
    };
    client
        .send(json!({"Event":{"Key":{
            "code":code,"modifiers":modifiers,"kind":"Press","state":0
        }}}))
        .await;
}

async fn terminal_submit(client: &mut Client, text: &str) {
    client.send(json!({"Event":{"Paste":text}})).await;
    terminal_key(client, "Enter", 0).await;
}

async fn terminal_text(client: &mut Client, needle: &str) -> String {
    let mut output = String::new();
    timeout(Duration::from_secs(8), async {
        loop {
            let frame = client.next().await;
            assert!(
                frame.get("Finished").is_none(),
                "closed while waiting for {needle:?}: {output}"
            );
            if let Some(text) = frame["Write"].as_str() {
                output.push_str(text);
                if output.contains(needle) {
                    break;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}: {output:?}"));
    output
}

#[tokio::test]
async fn terminal_attachment_runs_native_session_commands_in_the_host() {
    let extension = resource_fixture::Fixture::build().await;
    timeout(Duration::from_secs(30), async {
        let root = tempfile::Builder::new()
            .prefix("rh-tui-")
            .tempdir_in("/tmp")
            .unwrap();
        let agent = root.path().join("agent");
        let workspace = root.path().join("project");
        let sessions = root.path().join("sessions");
        let extensions = agent.join("extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        std::fs::copy(
            &extension.library,
            extensions.join(extension.library.file_name().unwrap()),
        )
        .unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        write_fixture(&agent, provider.local_addr().unwrap());
        std::fs::write(
            agent.join("host.json"),
            r#"{"areas":{"default":{"maxAgents":2}}}"#,
        )
        .unwrap();
        let mut daemon = spawn_daemon(root.path(), &agent, None, &sessions);
        let socket = agent.join("local-host-v1/rpc.sock");
        let (mut regular, mut fullscreen) = tokio::join!(
            open_terminal(&socket, &workspace, true),
            open_terminal(&socket, &workspace, false)
        );
        assert!(!terminal_text(&mut regular, "echo")
            .await
            .contains("\x1b[?1049h"));
        assert!(terminal_text(&mut fullscreen, "echo")
            .await
            .contains("\x1b[?1049h"));
        assert_eq!(
            extension
                .assert_registrations(&root.path().join("registrations.log"))
                .lines()
                .count(),
            2
        );
        let (_, rejected) = Client::handshake(&socket, &workspace).await;
        assert!(rejected["message"]
            .as_str()
            .unwrap()
            .contains("maxAgents: 2"));

        terminal_submit(&mut regular, "/name terminal-one").await;
        terminal_text(&mut regular, "Session renamed to").await;
        terminal_submit(&mut regular, "/reload").await;
        terminal_text(&mut regular, "Reloaded 1 plugin(s)").await;
        assert_eq!(
            extension
                .assert_registrations(&root.path().join("registrations.log"))
                .lines()
                .count(),
            3
        );
        terminal_submit(&mut regular, "run from the native TUI").await;
        let (mut upstream, _) = timeout(Duration::from_secs(8), provider.accept())
            .await
            .expect("timed out waiting for the first provider request")
            .unwrap();
        let request: Value = serde_json::from_str(&provider_request(&mut upstream).await).unwrap();
        assert!(request["messages"]
            .to_string()
            .contains("run from the native TUI"));
        let mut tools: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        tools.sort_unstable();
        assert_eq!(
            tools,
            ["bash", "docs", "durable", "echo", "edit", "read", "write"]
        );
        sse_reply(
            &mut upstream,
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":"host-echo","type":"function",
            "function":{"name":"echo","arguments":json!({"text":"TOOL_FROM_HOST"}).to_string()}
        }]},"finish_reason":"tool_calls"}]}),
        )
        .await;
        drop(upstream);
        let (mut upstream, _) = timeout(Duration::from_secs(8), provider.accept())
            .await
            .expect("timed out waiting for the tool result provider request")
            .unwrap();
        assert!(provider_request(&mut upstream)
            .await
            .contains("echo: TOOL_FROM_HOST"));
        complete(&mut upstream).await;
        // The fixture also transforms model text after a tool result.
        terminal_text(&mut regular, "AREA RESPONSE").await;
        terminal_submit(&mut regular, "/tree").await;
        terminal_text(&mut regular, "Session tree").await;
        terminal_key(&mut regular, "Esc", 0).await;
        terminal_submit(&mut regular, "/session").await;
        terminal_text(&mut regular, "Tab: all projects").await;
        terminal_key(&mut regular, "Esc", 0).await;
        terminal_submit(&mut regular, "/new").await;
        terminal_text(&mut regular, "Started new session").await;
        terminal_key(&mut regular, "d", 2).await;
        assert_eq!(
            regular.until(|frame| frame.get("Finished").is_some()).await["Finished"],
            0
        );

        terminal_submit(&mut fullscreen, "/name terminal-two").await;
        terminal_text(&mut fullscreen, "terminal-two").await;
        terminal_key(&mut fullscreen, "d", 2).await;
        assert_eq!(
            fullscreen
                .until(|frame| frame.get("Finished").is_some())
                .await["Finished"],
            0
        );
        let journals = journals(&sessions);
        let first = journals
            .iter()
            .find(|(_, text)| text.contains("run from the native TUI"))
            .unwrap();
        assert!(first.1.contains("terminal-one") && first.1.contains("operation_finished"));
        let second = journals
            .iter()
            .find(|(_, text)| text.contains("terminal-two"))
            .unwrap();
        assert_ne!(first.0, second.0);
        assert!(daemon.try_wait().unwrap().is_none());
        daemon.kill().await.unwrap();
    })
    .await
    .expect("native Host TUI probe timed out");
}
