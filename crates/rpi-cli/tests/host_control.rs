//! Required-artifact tests: Cargo builds the actual rpi binary. No fixture skip,
//! installed executable, global resources or production service. One test uses
//! a local HTTP Provider for startup-profile + IPC execution, without a management DLL.
#![cfg(unix)]
use rpi_cli::host::{
    AgentId, AgentMode, AgentSpec, ExecutionMode, HostCatalog, HostConfig, LaunchSpec, SessionRef,
    TimeoutOverrides,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::{symlink, MetadataExt, PermissionsExt},
    path::PathBuf,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
const DEADLINE: Duration = Duration::from_secs(15);

#[test]
fn actual_binary_top_level_help_describes_authenticated_not_read_only_host_ipc() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
        .arg("--help")
        .env_clear()
        .env("HOME", root.path())
        .env("RPI_CODING_AGENT_DIR", root.path())
        .env("NO_COLOR", "1")
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("host <command>"));
    assert!(help.contains("authenticated IPC"));
    assert!(!help.contains("read-only IPC"));
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_rejects_invalid_closed_profiles_before_session_or_agent_creation() {
    for case in [
        "fuzzy-model",
        "missing-key",
        "command-key",
        "required-header",
        "bad-proxy",
        "oversized-session-id",
        "oversized-tool-list",
        "resource-default",
        "resource-dll",
        "resource-skills",
        "resource-tools",
        "resource-relative",
        "resource-package",
    ] {
        let mut fixture = Fixture::new();
        let workspace = fixture.root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let mut value = json!({
            "agent": {
                "id": AgentId::new(), "name": "preflight-rejection", "cwd": workspace,
                "session": {"id": "preflight-session", "path": workspace.join("new.jsonl")},
                "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []
            },
            "runtime": {"providers": {"fixture": {
                "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
                "apiKey": "$PREFLIGHT_KEY",
                "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]
            }}},
            "providerValues": {"PREFLIGHT_KEY": "preflight-private-key", "NO_PROXY": "*"},
            "toolEnvironment": {"HOME": workspace, "PATH": "/usr/bin:/bin"},
            "shell": "/bin/sh", "thinking": "off"
        });
        match case {
            "fuzzy-model" => value["agent"]["model"]["model"] = json!("host"),
            "missing-key" => value["providerValues"] = json!({"NO_PROXY": "*"}),
            "command-key" => {
                value["runtime"]["providers"]["fixture"]["apiKey"] =
                    json!("!must-not-execute-preflight-command");
            }
            "required-header" => {
                value["runtime"]["providers"]["fixture"]["headers"] =
                    json!({"x-required": "$UNSET_REQUIRED_HOST_HEADER"});
            }
            "bad-proxy" => {
                value["providerValues"]["NO_PROXY"] = json!("");
                value["providerValues"]["HTTP_PROXY"] =
                    json!("socks5://preflight-private-proxy.invalid");
            }
            "oversized-session-id" => value["agent"]["session"]["id"] = json!("s".repeat(257)),
            "oversized-tool-list" => {
                value["agent"]["tools"] = json!(vec!["read"; 257]);
                value["approvedTools"] = json!(["read"]);
            }
            "resource-default" => {
                value["resources"] = json!({"version":1,"tools":{"mode":"default"}})
            }
            "resource-dll" => {
                value["resources"] = json!({"version":1,"tools":{"mode":"none"},"extensions":["must-not-load.dylib"]})
            }
            "resource-skills" => {
                value["resources"] =
                    json!({"version":1,"tools":{"mode":"none"},"skills":["must-not-discover"]})
            }
            "resource-tools" => {
                value["resources"] = json!({"version":1,"tools":{"mode":"only","names":["read"]}})
            }
            "resource-relative" => {
                value["resources"] =
                    json!({"version":1,"tools":{"mode":"none"},"instructions":["relative.md"]})
            }
            "resource-package" => {
                value["resources"] = json!({"version":1,"tools":{"mode":"none"},"main_package":{"name":"must-not-resolve"}})
            }
            _ => unreachable!(),
        }
        let profile = fixture.root.path().join("p.json");
        std::fs::write(&profile, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).unwrap();
        fixture.value["agents"] = json!([profile]);
        fixture.save();
        let output = fixture.call("serve", &[]).await;
        assert!(!output.status.success(), "{case} unexpectedly accepted");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("host_ready"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("preflight-private-key"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("preflight-private-proxy"));
        assert!(!fixture.socket().exists(), "{case} published a listener");
        assert_eq!(
            std::fs::read_dir(&workspace).unwrap().count(),
            0,
            "{case} created a Session, companion or writer artifact before rejection"
        );
        assert!(!fixture.root.path().join("unused-home").exists());
        let catalog = HostCatalog::open(fixture.directory(), HostConfig::default())
            .await
            .unwrap();
        assert_eq!(catalog.status().await.unwrap().registered, 0, "{case}");
        catalog.shutdown().await.unwrap();
    }
}

include!("host_control/administration.rs");
include!("host_control/administrative_execution.rs");
include!("host_control/administrative_cli.rs");
include!("host_control/submission_cli.rs");
include!("host_control/cancellation.rs");
include!("host_control/stopping.rs");
include!("host_control/closing_failure.rs");
include!("host_control/instruction_resources.rs");
include!("host_control/launch_policy.rs");
#[path = "host_control/native_hooks.rs"]
mod native_hooks;

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
    value: Value,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new().prefix("hb").tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = root.path().join("c.json");
        let value = json!({"directory":root.path().join("d"), "socket_path":root.path().join("s"), "host_id":uuid::Uuid::new_v4().to_string(), "token":uuid::Uuid::new_v4().simple().to_string()});
        let fixture = Self {
            root,
            config,
            value,
        };
        fixture.save();
        fixture
    }
    fn save(&self) {
        std::fs::write(&self.config, self.value.to_string()).unwrap();
        std::fs::set_permissions(&self.config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn directory(&self) -> PathBuf {
        PathBuf::from(self.value["directory"].as_str().unwrap())
    }
    fn socket(&self) -> PathBuf {
        PathBuf::from(self.value["socket_path"].as_str().unwrap())
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
        command
            .env_clear()
            .env("HOME", self.root.path().join("unused-home"))
            .env("RPI_EXTENSIONS_DIR", self.root.path().join("must-not-load"))
            .env("RPI_SESSION_ID", "ambient-session-must-not-bind")
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }
    async fn call(&self, operation: &str, flags: &[&str]) -> Output {
        tokio::time::timeout(
            DEADLINE,
            self.command()
                .args(["host", operation, "--config"])
                .arg(&self.config)
                .args(flags)
                .output(),
        )
        .await
        .unwrap()
        .unwrap()
    }
    async fn data(&self, operation: &str, flags: &[&str]) -> Value {
        let output = self.call(operation, flags).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    async fn start(&self) -> (Child, Value) {
        let mut child = self
            .command()
            .args(["host", "serve", "--config"])
            .arg(&self.config)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(DEADLINE, lines.next_line())
            .await
            .unwrap()
            .unwrap();
        let Some(line) = line else {
            panic!(
                "daemon exited before ready: {:?}",
                child.wait_with_output().await.unwrap()
            );
        };
        assert!(!line.contains(self.value["token"].as_str().unwrap()));
        let ready: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["event"], "host_ready");
        let mut expected = json!(["status", "list", "inspect", "receipts", "results", "submit"]);
        if self
            .value
            .get("administration")
            .is_some_and(|policy| !policy.is_null())
        {
            expected.as_array_mut().unwrap().extend([
                json!("profiles"),
                json!("administrative_receipts"),
                json!("administrative_create"),
                json!("administrative_restore"),
                json!("administrative_close"),
                json!("administrative_cancel"),
                json!("administrative_stop"),
            ]);
        }
        assert_eq!(ready["control"]["capabilities"], expected);
        (child, ready)
    }
    async fn stop(&self, child: Child, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
            signal,
        )
        .unwrap();
        let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!self.socket().exists());
    }
    fn no_resources(&self) {
        assert!(!self.root.path().join("unused-home").exists());
        assert!(!self.root.path().join(".rpi").exists());
        assert!(!self.root.path().join("sessions").exists());
    }
}

#[tokio::test]
async fn actual_binary_queries_exact_catalog_restarts_with_new_epoch_and_settles_signals() {
    let fixture = Fixture::new();
    let catalog = HostCatalog::open(fixture.directory(), HostConfig::default())
        .await
        .unwrap();
    let ordinary = AgentSpec {
        id: AgentId::new(),
        name: "binary-ordinary".into(),
        parent_id: None,
        execution: ExecutionMode::InProcess,
        mode: AgentMode::Persistent,
        session: Some(SessionRef {
            id: "not-auto-restored".into(),
            path: fixture.root.path().join("sessions/exact.jsonl"),
        }),
        launch: LaunchSpec {
            cwd: fixture.root.path().to_path_buf(),
            model: None,
            tools: vec![],
            resource_profile: None,
        },
        timeouts: TimeoutOverrides::default(),
    };
    catalog.register(ordinary.clone()).await.unwrap();
    let mut mini = ordinary.clone();
    mini.id = AgentId::new();
    mini.name = "metadata-only-mini".into();
    mini.mode = AgentMode::Mini;
    mini.session = None;
    catalog.register(mini.clone()).await.unwrap();
    catalog.shutdown().await.unwrap();
    let (child, ready) = fixture.start().await;
    let status = fixture.data("status", &[]).await;
    assert_eq!(status["registered"], 2);
    assert_eq!(status["resident"], 0);
    assert_eq!(status["owners"], 0);
    let page = fixture.data("list", &["--limit", "1"]).await;
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = fixture
        .data("list", &["--limit", "1", "--after", cursor])
        .await;
    assert_ne!(page["agents"][0]["id"], next["agents"][0]["id"]);
    assert!(next["next_cursor"].is_null());
    let inspected = fixture
        .data("inspect", &["--agent", &ordinary.id.to_string()])
        .await;
    assert_eq!(inspected["spec"], serde_json::to_value(&ordinary).unwrap());
    assert_eq!(
        fixture
            .data("inspect", &["--agent", &mini.id.to_string()])
            .await["state"],
        "lost"
    );
    let inode = std::fs::metadata(fixture.socket()).unwrap().ino();
    let duplicate = fixture.call("serve", &[]).await;
    assert!(!duplicate.status.success());
    assert_eq!(std::fs::metadata(fixture.socket()).unwrap().ino(), inode);
    assert_eq!(fixture.data("status", &[]).await["registered"], 2);
    fixture.no_resources();
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    let (child, reopened) = fixture.start().await;
    assert_ne!(ready["control"]["epoch"], reopened["control"]["epoch"]);
    assert_eq!(ready["control"]["host_id"], reopened["control"]["host_id"]);
    assert_eq!(fixture.data("status", &[]).await["registered"], 2);
    fixture.stop(child, nix::sys::signal::Signal::SIGINT).await;
    fixture.no_resources();
}

#[tokio::test]
async fn actual_binary_rejects_unsafe_and_unknown_configs_before_creating_host_artifacts() {
    let mut fixture = Fixture::new();
    std::fs::set_permissions(&fixture.config, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!fixture.call("serve", &[]).await.status.success());
    assert!(!fixture.directory().exists());
    assert!(!fixture.socket().exists());
    fixture.save();
    let destination = fixture.root.path().join("real.json");
    std::fs::rename(&fixture.config, &destination).unwrap();
    symlink(&destination, &fixture.config).unwrap();
    assert!(!fixture.call("serve", &[]).await.status.success());
    std::fs::remove_file(&fixture.config).unwrap();
    std::fs::hard_link(&destination, &fixture.config).unwrap();
    assert!(!fixture.call("serve", &[]).await.status.success());
    std::fs::remove_file(&destination).unwrap();
    fixture.value["prompt"] = json!("must-not-be-a-prompt");
    fixture.save();
    let output = fixture.call("serve", &[]).await;
    assert!(!output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(fixture.value["token"].as_str().unwrap())
    );
    assert!(!fixture.directory().exists());
    assert!(!fixture.socket().exists());
    fixture.no_resources();
}

#[tokio::test]
async fn actual_binary_queries_and_invalid_commands_never_start_or_initialize_a_host() {
    let fixture = Fixture::new();
    assert!(!fixture.call("status", &[]).await.status.success());
    for (operation, flags) in [
        ("restore", vec![]),
        ("serve", vec!["--approve"]),
        ("list", vec!["--limit", "0"]),
        ("inspect", vec![]),
    ] {
        assert_eq!(fixture.call(operation, &flags).await.status.code(), Some(2));
    }
    assert!(!fixture.directory().exists());
    assert!(!fixture.socket().exists());
    fixture.no_resources();
    let output = fixture
        .command()
        .args(["host", "--help"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("receipt") && help.contains("submit") && help.contains("agents"));
    let output = fixture.command().arg("--help").output().await.unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("host <command>"));
    fixture.no_resources();
}

#[tokio::test]
async fn actual_binary_does_not_echo_wrong_tokens_and_failed_auth_does_not_stop_the_host() {
    let mut fixture = Fixture::new();
    let (child, _) = fixture.start().await;
    let token = fixture.value["token"].clone();
    fixture.value["token"] = json!(uuid::Uuid::new_v4().simple().to_string());
    fixture.save();
    let output = fixture.call("status", &[]).await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(token.as_str().unwrap()));
    assert!(!stderr.contains(fixture.value["token"].as_str().unwrap()));
    fixture.value["token"] = token;
    fixture.save();
    assert_eq!(fixture.data("status", &[]).await["registered"], 0);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
}

#[tokio::test]
async fn actual_binary_readiness_output_failure_still_releases_catalog_and_endpoint() {
    for close_stderr in [false, true] {
        let fixture = Fixture::new();
        // Close the receiver before exec: deterministic EPIPE without racing the
        // first write. Use spawn, since Tokio Command::output overrides stdout.
        let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(reader);
        let stderr = if close_stderr {
            Stdio::from(std::os::fd::OwnedFd::from(writer.try_clone().unwrap()))
        } else {
            Stdio::piped()
        };
        let child = fixture
            .command()
            .args(["host", "serve", "--config"])
            .arg(&fixture.config)
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(writer)))
            .stderr(stderr)
            .spawn()
            .unwrap();
        let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !close_stderr {
            assert!(stderr.contains("Host readiness output failed"));
        }
        assert!(!stderr.contains("panicked"));
        assert!(!fixture.socket().exists());
        let catalog = HostCatalog::open(fixture.directory(), HostConfig::default())
            .await
            .unwrap();
        catalog.shutdown().await.unwrap();
        fixture.no_resources();
    }
}

fn sse_completion() -> String {
    let delta = json!({"role":"assistant","content":"binary-profile-answer"});
    let chunks = [
        json!({"id":"fixture-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"fixture-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    ];
    chunks
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 1024 * 1024);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let size: usize = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
        })
        .unwrap();
    assert!(size <= 1024 * 1024);
    while bytes.len() < header_end + size {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    headers
}

#[tokio::test]
async fn actual_binary_loads_startup_profile_and_executes_ipc_submit_end_to_end() {
    use rpi_cli::host::{
        client::{ConnectionConfig, HostClient},
        protocol::{AgentId as ProtocolAgentId, AuthToken, Command, HostId, RequestId},
    };
    let mut fixture = Fixture::new();
    let workspace = fixture.root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let agent = uuid::Uuid::new_v4().to_string();
    let session_path = workspace.join("profile.jsonl");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let headers = read_http_request(&mut stream).await;
        assert!(headers.contains("Bearer binary-private-key"));
        let reply = sse_completion();
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len())
                    .as_bytes(),
            )
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    });
    let profile = fixture.root.path().join("p.json");
    std::fs::write(
        &profile,
        json!({
            "agent": {
                "id": agent,
                "name": "binary-profile",
                "cwd": workspace.to_string_lossy(),
                "session": {"id": "binary-profile-session", "path": session_path.to_string_lossy()},
                "model": {"provider": "fixture", "model": "host-fixture"},
                "tools": []
            },
            "runtime": {"providers": {"fixture": {
                "api": "openai-completions",
                "baseUrl": format!("{url}/v1"),
                "apiKey": "$BINARY_KEY",
                "models": [{"id": "host-fixture", "contextWindow": 32768, "maxTokens": 1024}]
            }}},
            "providerValues": {"BINARY_KEY": "binary-private-key", "NO_PROXY": "*"},
            "toolEnvironment": {"HOME": workspace.to_string_lossy(), "PATH": "/usr/bin:/bin"},
            "shell": "/bin/sh",
            "approvedTools": [],
            "thinking": "off",
            "timeoutMs": 10000
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).unwrap();
    fixture.value["agents"] = json!([profile]);
    fixture.save();

    let (child, _ready) = fixture.start().await;
    let status = fixture.data("status", &[]).await;
    assert_eq!(status["registered"], 1);
    assert_eq!(status["resident"], 1);
    assert_eq!(status["owners"], 1);
    let inspected = fixture.data("inspect", &["--agent", &agent]).await;
    assert_eq!(inspected["state"], "idle");
    assert_eq!(
        inspected["spec"]["launch"]["cwd"],
        workspace.to_string_lossy().as_ref()
    );

    let connection = ConnectionConfig {
        socket_path: fixture.socket(),
        host_id: HostId::try_from(fixture.value["host_id"].as_str().unwrap().to_string()).unwrap(),
        token: AuthToken::new(fixture.value["token"].as_str().unwrap().to_string()).unwrap(),
        timeout: DEADLINE,
    };
    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id: ProtocolAgentId = agent.clone().try_into().unwrap();
    let request_id = RequestId::new();
    let submitted = ipc
        .call(Command::Submit {
            agent_id,
            request_id,
            prompt: "binary durable prompt".into(),
        })
        .await
        .unwrap();
    assert_eq!(submitted["durable"], true);
    assert_eq!(submitted["receipt"]["entrySeq"], 1);
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let status = ipc
            .call(Command::Receipt {
                agent_id,
                request_id,
            })
            .await
            .unwrap();
        if status["execution"]["finished"] == true {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(&session_path).unwrap();
    assert!(text.contains("binary-profile-answer"));
    assert!(!text.contains("binary-private-key"));
    ipc.detach();
    drop(ipc);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;

    // Restart from the same explicit profile: the Agent is reused, the receipt
    // persists, and the identical submit is idempotent without another run.
    let (child, _ready) = fixture.start().await;
    let mut ipc = HostClient::connect(connection).await.unwrap();
    let persisted = ipc
        .call(Command::Receipt {
            agent_id,
            request_id,
        })
        .await
        .unwrap();
    assert_eq!(persisted["found"], true);
    assert_eq!(persisted["execution"]["finished"], true);
    let replay = ipc
        .call(Command::Submit {
            agent_id,
            request_id,
            prompt: "binary durable prompt".into(),
        })
        .await
        .unwrap();
    assert_eq!(replay["receipt"]["entrySeq"], 1);
    assert_eq!(fixture.data("status", &[]).await["registered"], 1);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    fixture.no_resources();
}

#[tokio::test]
async fn actual_binary_kill_after_admission_restores_receipt_without_provider_replay() {
    use rpi_cli::host::{
        client::{ConnectionConfig, HostClient},
        protocol::{AuthToken, Command as HostCommand, HostId, RequestId},
    };
    let mut fixture = Fixture::new();
    let workspace = fixture.root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let session = workspace.join("crash.jsonl");
    let agent = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let headers = read_http_request(&mut stream).await;
        assert!(headers.contains("Bearer crash-private-key"));
        // Hold the request without replying. SIGKILL will interrupt the native
        // run after durable admission/start but before a Provider result.
        (listener, stream)
    });
    let profile = fixture.root.path().join("crash-profile.json");
    std::fs::write(
        &profile,
        json!({
            "agent": {
                "id": agent, "name": "crash-agent", "cwd": workspace,
                "session": {"id": "crash-session", "path": session},
                "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []
            },
            "runtime": {"providers": {"fixture": {
                "api": "openai-completions", "baseUrl": url, "apiKey": "$CRASH_KEY",
                "models": [{"id": "host-fixture", "contextWindow": 32768, "maxTokens": 1024}]
            }}},
            "providerValues": {"CRASH_KEY": "crash-private-key", "NO_PROXY": "*"},
            "toolEnvironment": {"HOME": workspace, "PATH": "/usr/bin:/bin"},
            "shell": "/bin/sh", "approvedTools": [], "thinking": "off", "timeoutMs": 10000
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).unwrap();
    fixture.value["agents"] = json!([profile]);
    fixture.save();
    let connection = ConnectionConfig {
        socket_path: fixture.socket(),
        host_id: HostId::try_from(fixture.value["host_id"].as_str().unwrap().to_string()).unwrap(),
        token: AuthToken::new(fixture.value["token"].as_str().unwrap().to_string()).unwrap(),
        timeout: DEADLINE,
    };
    let (mut child, first_ready) = fixture.start().await;
    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id = agent.try_into().unwrap();
    let request_id = RequestId::new();
    let submit = HostCommand::Submit {
        agent_id,
        request_id,
        prompt: "custody before crash".into(),
    };
    let receipt = HostCommand::Receipt {
        agent_id,
        request_id,
    };
    let admitted = ipc.call(submit.clone()).await.unwrap();
    assert_eq!(admitted["durable"], true);
    let (listener, stream) = tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let started = ipc.call(receipt.clone()).await.unwrap();
    assert_eq!(started["execution"]["state"], "started");
    assert_eq!(started["execution"]["finished"], false);
    child.kill().await.unwrap();
    drop(stream);
    drop(ipc);
    assert!(
        fixture.socket().exists(),
        "SIGKILL cannot run listener cleanup"
    );
    // An explicit operator cleanup in this private test directory, not daemon
    // stale-socket auto-unlink. The killed process has been reaped above.
    std::fs::remove_file(fixture.socket()).unwrap();
    let (child, second_ready) = fixture.start().await;
    assert_ne!(
        first_ready["control"]["epoch"],
        second_ready["control"]["epoch"]
    );
    let mut ipc = HostClient::connect(connection).await.unwrap();
    let recovered = ipc.call(receipt).await.unwrap();
    assert_eq!(recovered["receipt"], admitted["receipt"]);
    assert_eq!(
        recovered["execution"]["runId"],
        started["execution"]["runId"]
    );
    let replay = ipc.call(submit).await.unwrap();
    assert_eq!(replay["receipt"], admitted["receipt"]);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "restart or duplicate submit replayed the Provider request"
    );
    let text = std::fs::read_to_string(session).unwrap();
    assert_eq!(text.matches("rpi.host.admission.v1").count(), 1);
    assert!(!text.contains("crash-private-key"));
    fixture.no_resources();
}
