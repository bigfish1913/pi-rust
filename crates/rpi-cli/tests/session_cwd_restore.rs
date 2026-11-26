//! End-to-end: restoring a session returns the agent to the directory the
//! session was created in.
//!
//! The launch path adopts the session's recorded cwd before any resource load,
//! so the observable contract lands in the provider request itself: the default
//! system prompt embeds `Current working directory: …`, and a run resumed from
//! a different directory must present the recorded directory there — not the
//! launch directory the shell happened to be in.
use std::sync::{Arc, Mutex};
use std::{process::Stdio, time::Duration};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

async fn serve_response(
    listener: &tokio::net::TcpListener,
    delta: Value,
    finish_reason: &str,
) -> Value {
    let (mut socket, _) = listener.accept().await.unwrap();
    let body = request_body(&mut socket).await;
    let chunk = json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]});
    let frame = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                frame.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.write_all(frame.as_bytes()).await.unwrap();
    body
}

async fn serve_one_request(listener: Arc<tokio::net::TcpListener>, bodies: Arc<Mutex<Vec<Value>>>) {
    let body = serve_response(&listener, json!({"content": "ok."}), "stop").await;
    bodies.lock().unwrap().push(body);
}

struct Fixture {
    root: tempfile::TempDir,
    agent: std::path::PathBuf,
    sessions: std::path::PathBuf,
    listener: Arc<tokio::net::TcpListener>,
}

async fn start_fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let sessions = root.path().join("sessions");
    let project = root.path().join("session-cwd-project");
    let launch = root.path().join("session-cwd-launch");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&launch).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({
            "providers": { "fixture": {
                "api": "openai-completions", "baseUrl": format!("http://{address}/v1"),
                "apiKey": "fixture-only", "models": [{ "id": "reasoner",
                "contextWindow": 131072, "maxTokens": 256 }]
            }}
        })
        .to_string(),
    )
    .unwrap();
    Fixture {
        root,
        agent,
        sessions,
        listener: Arc::new(listener),
    }
}

/// One print-mode run of the real binary.
async fn run_rpi(
    fixture: &Fixture,
    cwd: &std::path::Path,
    extra_args: &[&str],
    prompt: &str,
) -> (std::process::ExitStatus, String, String) {
    let out_path = cwd.join("stdout.log");
    let err_path = cwd.join("stderr.log");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"));
    command
        .current_dir(cwd)
        .env("RPI_CODING_AGENT_DIR", &fixture.agent)
        .env("RPI_CODING_AGENT_SESSION_DIR", &fixture.sessions)
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .args([
            "--offline",
            "--package-only",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--provider",
            "fixture",
            "--model",
            "reasoner",
            "--print",
            "--timeout",
            "15",
        ])
        .args(extra_args)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    if status.is_err() {
        panic!("run did not finish in time (cwd {})", cwd.display());
    }
    let status = status.unwrap().unwrap();
    let stdout = std::fs::read_to_string(&out_path).unwrap();
    let stderr = std::fs::read_to_string(&err_path).unwrap();
    (status, stdout, stderr)
}

#[tokio::test]
async fn mini_keeps_context_and_honors_an_explicit_tool_list_without_saving() {
    let fixture = start_fixture().await;
    let cwd = fixture.root.path().join("session-cwd-project");
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let listener = fixture.listener.clone();
    let collector = bodies.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            serve_one_request(listener.clone(), collector.clone()).await;
        }
    });

    let (status, stdout, stderr) = run_rpi(
        &fixture,
        &cwd,
        &[
            "--mini",
            "--system-prompt",
            "mini system",
            "--tools",
            "read,durable",
            "first",
            "second",
        ],
        "third",
    )
    .await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(stdout, "ok.\n");
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("the provider should receive all three prompts")
        .unwrap();

    let bodies = bodies.lock().unwrap();
    let request = &bodies[2];
    let messages = request["messages"].as_array().unwrap();
    let content: Vec<_> = messages
        .iter()
        .map(|message| message["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        content,
        ["mini system", "first", "ok.", "second", "ok.", "third"]
    );
    let tools = request["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "read");
    assert!(fixture.sessions.read_dir().unwrap().next().is_none());
    assert!(!cwd.join(".rpi/sessions").exists());
}

#[tokio::test]
async fn mini_runs_default_tools_and_compacts_context_without_saving() {
    let fixture = start_fixture().await;
    let cwd = fixture.root.path().join("session-cwd-project");
    std::fs::write(cwd.join("notes.txt"), "content returned by read").unwrap();
    std::fs::write(
        fixture.agent.join("settings.json"),
        json!({"compaction": {"reserveTokens": 130000, "keepRecentTokens": 1}}).to_string(),
    )
    .unwrap();

    let listener = fixture.listener.clone();
    let server = tokio::spawn(async move {
        let initial = serve_response(
            &listener,
            json!({"tool_calls": [{
                "index": 0, "id": "read-notes", "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"notes.txt\"}"}
            }]}),
            "tool_calls",
        )
        .await;
        let after_tool = serve_response(
            &listener,
            json!({"content": "old conversation detail ".repeat(500)}),
            "stop",
        )
        .await;
        let summary =
            serve_response(&listener, json!({"content": "compressed history"}), "stop").await;
        let after_compaction = serve_response(&listener, json!({"content": "ok."}), "stop").await;
        (initial, after_tool, summary, after_compaction)
    });

    let (status, stdout, stderr) = run_rpi(
        &fixture,
        &cwd,
        &["--mini", "inspect notes"],
        "next question",
    )
    .await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(stdout, "ok.\n");
    let (initial, after_tool, summary, after_compaction) = server.await.unwrap();
    let tools = initial["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "read"));
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "write"));
    assert!(!tools
        .iter()
        .any(|tool| tool["function"]["name"] == "durable"));
    assert!(after_tool["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "read-notes"
                && message["content"]
                    .to_string()
                    .contains("content returned by read")
        }));
    assert!(summary["messages"]
        .to_string()
        .contains("old conversation detail"));
    let messages = after_compaction["messages"].to_string();
    assert!(messages.contains("compressed history") && messages.contains("next question"));
    assert!(!messages.contains("old conversation detail"));
    assert!(fixture.sessions.read_dir().unwrap().next().is_none());
    assert!(!cwd.join(".rpi/sessions").exists());
}

#[tokio::test]
async fn mini_rpc_rejects_durable_mutation_without_saving() {
    let fixture = start_fixture().await;
    let cwd = fixture.root.path().join("session-cwd-project");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
        .current_dir(&cwd)
        .env("RPI_CODING_AGENT_DIR", &fixture.agent)
        .env("RPI_CODING_AGENT_SESSION_DIR", &fixture.sessions)
        .args([
            "--mini",
            "--native-durable",
            "--mode",
            "rpc",
            "--offline",
            "--no-extensions",
            "--provider",
            "fixture",
            "--model",
            "reasoner",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(
            br#"{"type":"durable_mutate","id":1,"version":1,"requestId":"mini-test","action":{"action":"start","name":"test","steps":[{"id":"only","title":"Only step"}]}}
"#,
        )
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("mini RPC should finish at EOF")
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let output_text = String::from_utf8(output.stdout).unwrap();
    let messages: Vec<Value> = output_text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages[0]["durable"]["enabled"], false, "{output_text}");
    let response = messages.iter().find(|message| message["id"] == 1).unwrap();
    assert_eq!(response["errorDetails"]["code"], "disabled", "{response}");
    assert!(fixture.sessions.read_dir().unwrap().next().is_none());
}

/// A session created in `project-a` and resumed (`-c`) from a different
/// directory must run in `project-a`: the provider request's system prompt
/// names the recorded directory, not the launch directory. Before the adoption
/// existed, the same launch presented the launch directory — the bug.
#[tokio::test]
async fn resuming_from_another_directory_runs_in_the_recorded_one() {
    let fixture = start_fixture().await;
    let project = fixture.root.path().join("session-cwd-project");
    let launch = fixture.root.path().join("session-cwd-launch");

    // Run 1 creates the session in the shared session root, recording the
    // project directory as its cwd.
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &[], "first").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(bodies.lock().unwrap().len(), 1);

    // Run 2 resumes the latest session from a different directory.
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &launch, &["--continue"], "second").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("ok."), "{stdout}\n{stderr}");

    // Adopting another project's directory switches tools, context files, and
    // extensions to that project; the user has to see it happen.
    assert!(
        stderr.contains("where it was created"),
        "the adoption must be announced: {stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("session-cwd-project") && stderr.contains("session-cwd-launch"),
        "the notice must name both directories: {stderr}"
    );

    let requests = bodies.lock().unwrap();
    assert_eq!(requests.len(), 1, "stderr: {stderr}");
    let request_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        request_text.contains("session-cwd-project"),
        "the resumed run must present the session's recorded directory: {request_text}"
    );
    assert!(
        !request_text.contains("session-cwd-launch"),
        "the launch directory must not leak into the resumed run: {request_text}"
    );
}

/// Resuming in the directory the session was created in adopts nothing, so
/// there is nothing to announce. rpi-web restores sessions repeatedly in the
/// project directory they belong to; an unconditional notice printed the same
/// line once per restored session, which reads as a broken restore.
#[tokio::test]
async fn resuming_in_the_recorded_directory_says_nothing() {
    let fixture = start_fixture().await;
    let project = fixture.root.path().join("session-cwd-project");

    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &[], "first").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");

    // Same directory, same session: no directory changed.
    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &["--continue"], "second").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("ok."), "{stdout}\n{stderr}");
    assert!(
        !stderr.contains("where it was created"),
        "a same-directory resume must not claim it moved: {stdout}\n{stderr}"
    );
    assert_eq!(bodies.lock().unwrap().len(), 1, "stderr: {stderr}");
}

/// A session whose recorded directory has since been deleted falls back to the
/// launch cwd, and says so on the run's stderr (headless modes have no
/// transcript to put a note in).
#[tokio::test]
async fn a_missing_recorded_directory_falls_back_and_says_so() {
    let fixture = start_fixture().await;
    let project = fixture.root.path().join("session-cwd-project");
    let launch = fixture.root.path().join("session-cwd-launch");

    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &project, &[], "first").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");

    // The recorded directory disappears before the resume.
    std::fs::remove_dir_all(&project).unwrap();

    let bodies = Arc::new(Mutex::new(Vec::new()));
    {
        let listener = fixture.listener.clone();
        let collector = Arc::clone(&bodies);
        tokio::spawn(serve_one_request(listener, collector));
    }
    let (status, stdout, stderr) = run_rpi(&fixture, &launch, &["--continue"], "second").await;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("no longer exists"),
        "the fallback must be announced: {stdout}\n{stderr}"
    );

    let requests = bodies.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request_text = serde_json::to_string(&requests[0]).unwrap();
    assert!(
        request_text.contains("session-cwd-launch"),
        "the run must fall back to the launch directory: {request_text}"
    );
}
