//! Independent synthetic process fixture; does not import the old test module.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(25);
const STDERR_LIMIT: usize = 16 * 1024;

pub struct Fixture {
    pub root: tempfile::TempDir,
    provider: TcpListener,
}
impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in ["agent", "home", "project", "sessions", "config", "data"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        // A local tripwire, not an HTTP server: even an accidental provider
        // connection is observable without contacting an external service.
        let provider = TcpListener::bind("127.0.0.1:0").unwrap();
        provider.set_nonblocking(true).unwrap();
        std::fs::write(
            root.path().join("agent/models.json"),
            json!({"providers":{"fixture":{
                "api":"openai-completions",
                "baseUrl":format!("http://{}/v1", provider.local_addr().unwrap()),
                "apiKey":"synthetic-test-key",
                "models":[{"id":"test","contextWindow":131072,"maxTokens":256}]
            }}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join("agent/auth.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("agent/settings.json"),
            r#"{"retry":{"enabled":false}}"#,
        )
        .unwrap();
        Self { root, provider }
    }

    // Resource tests supply their own explicit selection/discovery policy.
    pub fn base_command(&self) -> Command {
        let root = self.root.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
        command.current_dir(root.join("project")).env_clear();
        // Retain only OS execution plumbing, never inherited provider/config
        // overrides or credentials. Override all home/config paths below.
        for name in ["PATH", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("TEMP", root.join("data"))
            .env("TMP", root.join("data"))
            .env("TMPDIR", root.join("data"))
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("APPDATA", root.join("config"))
            .env("LOCALAPPDATA", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .env("RPI_CODING_AGENT_SESSION_DIR", root.join("sessions"))
            .env("RPI_EXTENSIONS_DIR", root.join("agent/extensions"));
        command
    }

    pub fn command(&self, enabled: bool, extra: &[&str]) -> Command {
        let mut command = self.base_command();
        command.args([
            "--offline",
            "--provider",
            "fixture",
            "--model",
            "test",
            "--mode",
            "rpc",
        ]);
        if enabled {
            command.arg("--native-durable");
        }
        command.args(extra);
        command
    }

    pub fn start(&self, enabled: bool, extra: &[&str]) -> (Host, Value) {
        let mut command = self.command(
            enabled,
            &[
                "--package-only",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--no-prompt-templates",
            ],
        );
        let mut host = Host::spawn(command.args(extra));
        let ready = host.until(|v| v["type"] == "ready");
        assert_eq!(ready["durable"]["enabled"], enabled, "{ready}");
        assert_eq!(ready["durable"]["version"], 1, "{ready}");
        assert_eq!(ready["durable"]["prototype"], true, "{ready}");
        (host, ready)
    }

    pub fn session_file(&self) -> PathBuf {
        fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, files);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .ends_with(".progress.jsonl")
                {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        collect(&self.root.path().join("sessions"), &mut files);
        assert_eq!(files.len(), 1, "test-owned JSONL sessions: {files:?}");
        files.pop().unwrap()
    }

    pub fn assert_no_provider_calls(&self) {
        match self.provider.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("manual durable access contacted the provider: {other:?}"),
        }
    }
}

pub struct Host {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<mpsc::Receiver<String>>,
    readers: Vec<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}
impl Host {
    pub fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut error_pipe = child.stderr.take().unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        let output_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let tail = Arc::clone(&stderr);
        let error_reader = thread::spawn(move || {
            let mut chunk = [0; 4096];
            while let Ok(count) = error_pipe.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let mut bytes = tail.lock().unwrap();
                bytes.extend_from_slice(&chunk[..count]);
                let excess = bytes.len().saturating_sub(STDERR_LIMIT);
                bytes.drain(..excess);
            }
        });
        Self {
            child,
            input: Some(input),
            output: Some(receiver),
            readers: vec![output_reader, error_reader],
            stderr,
            next_id: 0,
        }
    }

    pub fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let result = self
                .output
                .as_ref()
                .unwrap()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()));
            let line = result.unwrap_or_else(|error| {
                panic!(
                    "native RPC response timeout/EOF ({error}); stderr: {}",
                    String::from_utf8_lossy(&self.stderr.lock().unwrap())
                )
            });
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("non-JSON native stdout ({error}): {line}"));
            assert!(
                !matches!(
                    value["type"].as_str(),
                    Some("agent_start" | "tool_execution_start")
                ),
                "durable access must not start an agent or tool: {value}"
            );
            if predicate(&value) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "native RPC emitted events without a response"
            );
        }
    }

    pub fn raw(&mut self, line: &str, id: &Value) -> Value {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        self.until(|value| value["type"] == "response" && &value["id"] == id)
    }

    pub fn request(&mut self, kind: &str, mut fields: Value) -> Value {
        self.next_id += 1;
        let id = json!(format!("transport-{}", self.next_id));
        fields["type"] = json!(kind);
        fields["id"] = id.clone();
        self.raw(&fields.to_string(), &id)
    }

    pub fn data(&mut self, kind: &str, fields: Value) -> Value {
        let response = self.request(kind, fields);
        assert_eq!(response["success"], true, "{response}");
        response["data"].clone()
    }

    pub fn state(&mut self) -> Value {
        let data = self.data("get_durable_state", json!({"version":1}));
        assert_eq!(data["version"], 1, "{data}");
        let state = data["state"].clone();
        for field in ["scope", "state", "executionControl"] {
            assert!(state.get(field).is_some(), "missing {field}: {state}");
        }
        state
    }

    pub fn mutate(&mut self, request_id: &str, action: Value) -> Value {
        let scope = self.state()["scope"].clone();
        self.data("durable_mutate", mutation(request_id, scope, action))["outcome"].clone()
    }

    pub fn assert_idle(&mut self) {
        let state = self.data("get_state", json!({}));
        assert!(state["activeOperation"].is_null(), "{state}");
        let messages = self.data("get_messages", json!({}));
        assert_eq!(messages["messages"], json!([]), "{messages}");
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        // Closing the receiver also releases a reader blocked on its bounded
        // channel. Always kill/reap, including assertion and timeout paths.
        self.input.take();
        self.output.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

pub fn mutation(request_id: &str, scope: Value, action: Value) -> Value {
    json!({"version":1,"requestId":request_id,"expected":scope,"action":action})
}
pub fn start_action() -> Value {
    json!({"action":"start","name":"synthetic workflow",
        "steps":[{"id":"verify","title":"Verify persistence"}]})
}
pub fn assert_error(response: &Value, code: &str) {
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["errorDetails"]["code"], code, "{response}");
    assert!(response.get("data").is_none(), "{response}");
}

impl Fixture {
    pub fn raw_rejection(&self, args: &[&str], usage: bool) -> String {
        let mut command = self.base_command();
        command.args(args);
        self.reject_command(&mut command, usage)
    }
    pub fn rejection(&self, enabled: bool, args: &[&str], usage: bool) -> String {
        let mut command = self.command(
            enabled,
            &[
                "--package-only",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--no-prompt-templates",
                "--no-tools",
            ],
        );
        command.args(args);
        self.reject_command(&mut command, usage)
    }
    fn reject_command(&self, command: &mut Command, usage: bool) -> String {
        let mut host = Host::spawn(command);
        host.input.take();
        let deadline = Instant::now() + WAIT;
        // Drain concurrently with exit polling: even a noisy rejection cannot
        // fill the bounded stdout channel and deadlock the child.
        let check_line = |line: String| {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                assert_ne!(
                    value["type"], "ready",
                    "rejected CLI emitted ready: {value}"
                );
            }
        };
        let status = loop {
            for line in host.output.as_ref().unwrap().try_iter() {
                check_line(line);
            }
            if let Some(status) = host.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "rejected CLI did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        loop {
            match host
                .output
                .as_ref()
                .unwrap()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => check_line(line),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(error) => panic!("rejection pipe did not close: {error}"),
            }
        }
        host.output.take();
        for reader in host.readers.drain(..) {
            reader.join().unwrap();
        }
        let stderr = String::from_utf8_lossy(&host.stderr.lock().unwrap()).into_owned();
        assert!(!status.success(), "unexpected CLI success: {stderr}");
        if usage {
            assert_eq!(status.code(), Some(2), "{stderr}");
        }
        assert!(!stderr.trim().is_empty(), "missing rejection diagnostic");
        self.assert_no_provider_calls();
        stderr
    }
    pub fn assert_no_sessions(&self) {
        fn files(dir: &Path) -> usize {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|e| {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        files(&p)
                    } else {
                        1
                    }
                })
                .sum()
        }
        assert_eq!(files(&self.root.path().join("sessions")), 0);
    }
    /// Opaque legacy fixture, built solely with synthetic storage APIs.
    pub fn synthetic_legacy(&self, path: &Path) {
        use rpi_harness::session::{
            jsonl::{JsonlSessionStorage, JsonlV4Header},
            types::{ProvisionedEntry, ProvisionedKind},
            DefaultIdGenerator, Session, SystemClock,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(WAIT, async {
                    let fs = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(self.root.path().into()));
                    let storage = JsonlSessionStorage::create(
                        fs,
                        path.to_str().unwrap(),
                        JsonlV4Header::new(
                            "synthetic-legacy".into(),
                            1,
                            self.root
                                .path()
                                .join("project")
                                .to_string_lossy()
                                .into_owned(),
                            None,
                            None,
                            None,
                        ),
                        Arc::new(SystemClock),
                        Arc::new(DefaultIdGenerator::new()),
                    )
                    .await
                    .unwrap();
                    let session = Session::new(Arc::new(storage), None);
                    session
                        .append_entry(
                            ProvisionedEntry {
                                id: "synthetic-legacy-entry".into(),
                                kind: ProvisionedKind::Custom {
                                    custom_type: "durable-state".into(),
                                    data: Some(json!({"synthetic":true})),
                                },
                            },
                            "main",
                        )
                        .await
                        .unwrap();
                })
                .await
                .expect("synthetic legacy fixture timeout");
            });
    }
}
