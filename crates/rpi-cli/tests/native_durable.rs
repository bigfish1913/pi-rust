//! Real native RPC process coverage. All config, homes and JSONL sessions are
//! synthetic. Manual-access tests never prompt; resource acceptance uses only a
//! controlled local provider. No recovery or tool replay is requested.
#[path = "support/native_durable_context.rs"]
mod context;
#[path = "support/native_durable_pages.rs"]
mod pages;
#[path = "support/resource_fixture.rs"]
mod resource_fixture;
#[path = "support/native_durable_resources.rs"]
mod resources;
#[path = "support/native_durable_shutdown.rs"]
mod shutdown;
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

struct Fixture {
    root: tempfile::TempDir,
    provider: TcpListener,
}
impl Fixture {
    fn new() -> Self {
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
    fn command(&self, enabled: bool, extra: &[&str]) -> Command {
        let root = self.root.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
        command.current_dir(root.join("project")).env_clear();
        // Retain only OS execution plumbing, never inherited provider/config
        // overrides or credentials. Override all home/config paths below.
        for name in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TEMP",
            "TMP",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("APPDATA", root.join("config"))
            .env("LOCALAPPDATA", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .env("RPI_CODING_AGENT_SESSION_DIR", root.join("sessions"))
            .env("RPI_EXTENSIONS_DIR", root.join("agent/extensions"))
            .args([
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
        } else {
            command.arg("--no-native-durable");
        }
        command.args(extra);
        command
    }

    fn start(&self, enabled: bool, extra: &[&str]) -> Host {
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
        host
    }

    fn session_file(&self) -> PathBuf {
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

    fn assert_no_provider_calls(&self) {
        match self.provider.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("manual durable access contacted the provider: {other:?}"),
        }
    }
}

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<mpsc::Receiver<String>>,
    readers: Vec<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}
impl Host {
    fn spawn(command: &mut Command) -> Self {
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

    fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
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

    fn raw(&mut self, line: &str, id: &Value) -> Value {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
        self.until(|value| value["type"] == "response" && &value["id"] == id)
    }

    fn request(&mut self, kind: &str, mut fields: Value) -> Value {
        self.next_id += 1;
        let id = json!(format!("transport-{}", self.next_id));
        fields["type"] = json!(kind);
        fields["id"] = id.clone();
        self.raw(&fields.to_string(), &id)
    }

    fn data(&mut self, kind: &str, fields: Value) -> Value {
        let response = self.request(kind, fields);
        assert_eq!(response["success"], true, "{response}");
        response["data"].clone()
    }

    fn state(&mut self) -> Value {
        let data = self.data("get_durable_state", json!({"version":1}));
        assert_eq!(data["version"], 1, "{data}");
        let state = data["state"].clone();
        for field in ["scope", "state", "executionControl"] {
            assert!(state.get(field).is_some(), "missing {field}: {state}");
        }
        state
    }

    fn mutate(&mut self, request_id: &str, action: Value) -> Value {
        let scope = self.state()["scope"].clone();
        self.data("durable_mutate", mutation(request_id, scope, action))["outcome"].clone()
    }

    fn assert_idle(&mut self) {
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

fn mutation(request_id: &str, scope: Value, action: Value) -> Value {
    json!({"version":1,"requestId":request_id,"expected":scope,"action":action})
}
fn start_action() -> Value {
    json!({"action":"start","name":"synthetic workflow",
        "steps":[{"id":"verify","title":"Verify persistence"}]})
}
fn assert_error(response: &Value, code: &str) {
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["errorDetails"]["code"], code, "{response}");
    assert!(response.get("data").is_none(), "{response}");
}
fn has_name(data: &Value, field: &str, name: &str) -> bool {
    data[field]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == name || value["name"] == name)
}

#[test]
fn native_durable_can_be_disabled_with_the_transition_flag() {
    let fixture = Fixture::new();
    let mut host = fixture.start(false, &["--no-session", "--no-tools"]);
    for (kind, fields) in [
        ("get_durable_state", json!({"version":1})),
        (
            "get_durable_request",
            json!({"version":1,"requestId":"absent"}),
        ),
        (
            "durable_mutate",
            mutation("disabled-start", json!({}), start_action()),
        ),
    ] {
        assert_error(&host.request(kind, fields), "disabled");
    }
    let tools = host.data("get_available_tools", json!({}));
    assert!(!has_name(&tools, "tools", "durable"));
    let commands = host.data("get_commands", json!({}));
    assert!(!has_name(&commands, "commands", "durable"));
    host.assert_idle();
    drop(host);
    fixture.assert_no_provider_calls();
}

#[test]
fn guarded_mutations_deduplicate_queue_and_survive_process_death() {
    let fixture = Fixture::new();
    let mut host = fixture.start(
        true,
        &[
            "--session-id",
            "native-durable-lifecycle",
            "--tools",
            "durable",
        ],
    );
    let tools = host.data("get_available_tools", json!({}));
    assert_eq!(tools["tools"], json!(["durable"]), "{tools}");
    assert_eq!(tools["activeTools"], json!(["durable"]), "{tools}");
    let commands = host.data("get_commands", json!({}));
    assert!(has_name(&commands, "commands", "durable"));
    let initial = host.state();
    assert!(initial["state"]["workflow"].is_null(), "{initial}");
    let start = mutation("start-once", initial["scope"].clone(), start_action());
    let first = host.data("durable_mutate", start.clone())["outcome"].clone();
    assert_eq!(first["replayed"], false);
    assert_eq!(first["snapshot"]["state"]["workflow"]["status"], "running");
    let replay = host.data("durable_mutate", start)["outcome"].clone();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["receipt"], first["receipt"]);
    assert_eq!(replay["snapshot"], first["snapshot"]);
    assert_error(
        &host.request(
            "durable_mutate",
            mutation(
                "stale-enqueue",
                initial["scope"].clone(),
                json!({"action":"enqueue","text":"stale"}),
            ),
        ),
        "conflict",
    );
    let receipt = host.data(
        "get_durable_request",
        json!({"version":1,"requestId":"start-once"}),
    );
    assert_eq!(receipt["receipt"], first["receipt"]);
    assert_eq!(receipt["receipt"]["actor"], "native-cli");
    let slash_receipt = host.data("execute_command", json!({"command":"durable", "args":json!({"action":"status", "requestId":"start-once"}).to_string()}));
    assert_eq!(slash_receipt, receipt);

    let enqueue = host.mutate(
        "enqueue-once",
        json!({"action":"enqueue","text":"manual work"}),
    );
    let item = enqueue["snapshot"]["state"]["queue"][0]["id"].clone();
    assert!(item.is_string(), "{enqueue}");
    let claim = host.mutate("claim-once", json!({"action":"claim","item":item}));
    assert_eq!(claim["snapshot"]["state"]["queue"][0]["status"], "claimed");
    let finish = host.mutate(
        "finish-once",
        json!({"action":"finish","item":item,"status":"completed"}),
    );
    assert_eq!(
        finish["snapshot"]["state"]["queue"][0]["status"],
        "completed"
    );
    for custom_type in ["rpi.durable.control.v1", "rpi.durable.forged.v99"] {
        let response = host.request(
            "append_entry",
            json!({"customType":custom_type,"data":{"forged":true}}),
        );
        assert_eq!(response["success"], false, "{response}");
    }
    let saved = host.state();
    let slash = host.data("execute_command", json!({"command":"durable","args":""}));
    assert_eq!(slash["state"], saved);
    host.assert_idle();
    let session_id = host.data("get_state", json!({}))["sessionId"].clone();
    assert_eq!(session_id, "native-durable-lifecycle");
    let session_file = fixture.session_file();
    // No stop handshake: prove already-acknowledged writes survive process death.
    drop(host);
    let mut reopened = fixture.start(
        true,
        &[
            "--session",
            session_file.to_str().unwrap(),
            "--tools",
            "durable",
        ],
    );
    assert_eq!(
        reopened.data("get_state", json!({}))["sessionId"],
        session_id
    );
    let restored = reopened.state();
    assert_eq!(restored["state"], saved["state"]);
    assert_eq!(restored["executionControl"], saved["executionControl"]);
    assert_eq!(
        restored["scope"]["controlRevision"],
        saved["scope"]["controlRevision"]
    );
    // bindingToken is intentionally process-local; do not reuse an old guard
    // or issue a second start. Query committed receipts without replay instead.
    assert_eq!(
        reopened.data(
            "get_durable_request",
            json!({
                "version":1,"requestId":"start-once"
            })
        )["receipt"],
        first["receipt"]
    );
    assert_eq!(
        reopened.data(
            "get_durable_request",
            json!({
                "version":1,"requestId":"finish-once"
            })
        )["receipt"],
        finish["receipt"]
    );
    reopened.assert_idle();
    drop(reopened);
    fixture.assert_no_provider_calls();
}

#[test]
fn strict_wire_validation_rejects_duplicates_and_bad_guards_before_mutating() {
    let fixture = Fixture::new();
    let mut host = fixture.start(true, &["--no-session", "--no-tools"]);
    let before = host.state();
    let valid = mutation("strict-start", before["scope"].clone(), start_action());
    let mut bad = Vec::new();
    for version in [
        Value::Null,
        json!(0),
        json!(2),
        json!(1.0),
        json!("1"),
        json!(true),
    ] {
        let mut request = valid.clone();
        request["version"] = version;
        bad.push(request);
    }
    for missing in ["version", "requestId", "expected"] {
        let mut request = valid.clone();
        request.as_object_mut().unwrap().remove(missing);
        bad.push(request);
    }
    for missing in [
        "sessionId",
        "lane",
        "bindingToken",
        "leafId",
        "historyRevision",
        "entryId",
        "controlRevision",
    ] {
        let mut request = valid.clone();
        request["expected"].as_object_mut().unwrap().remove(missing);
        bad.push(request);
    }
    for field in ["actor", "__rpi", "unknown"] {
        let mut request = valid.clone();
        request[field] = json!({"spoofed":true});
        bad.push(request);
    }
    let mut unknown_guard = valid.clone();
    unknown_guard["expected"]["unknown"] = json!(true);
    bad.push(unknown_guard);
    let mut unknown_action = valid.clone();
    unknown_action["action"]["actor"] = json!("spoofed");
    bad.push(unknown_action);
    let mut unknown_step = valid.clone();
    unknown_step["action"]["steps"][0]["status"] = json!("completed");
    bad.push(unknown_step);
    for request in bad {
        assert_error(&host.request("durable_mutate", request), "invalid_request");
    }
    let scope = before["scope"].to_string();
    let duplicate_frames = [
        format!(
            r#"{{"type":"durable_mutate","id":"duplicate-top","version":1,"version":1,"requestId":"duplicate-top","expected":{scope},"action":{}}}"#,
            start_action()
        ),
        format!(
            r#"{{"type":"durable_mutate","id":"duplicate-action","version":1,"requestId":"duplicate-action","expected":{scope},"action":{{"action":"reset","action":"start","name":"bad","steps":[{{"id":"a","title":"A"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"durable_mutate","id":"duplicate-step","version":1,"requestId":"duplicate-step","expected":{scope},"action":{{"action":"start","name":"bad","steps":[{{"id":"a","\u0069d":"b","title":"A"}}]}}}}"#
        ),
    ];
    for (raw, id) in
        duplicate_frames
            .iter()
            .zip(["duplicate-top", "duplicate-action", "duplicate-step"])
    {
        assert_error(&host.raw(raw, &json!(id)), "invalid_request");
        assert!(
            host.data("get_durable_request", json!({"version":1,"requestId":id}))["receipt"]
                .is_null()
        );
    }
    for raw in [
        format!(
            "{}{}",
            " ".repeat(64 * 1024),
            json!({"id":"padded","type":"durable_mutate","version":1,"requestId":"padded","expected":before["scope"],"action":start_action()})
        ),
        format!(
            "{}{}",
            json!({"id":"padded","type":"get_durable_state","version":1}),
            " ".repeat(64 * 1024)
        ),
    ] {
        assert_error(&host.raw(&raw, &json!("padded")), "limit_exceeded");
    }
    assert_eq!(
        host.state(),
        before,
        "invalid frames must not append durable state"
    );
    // A subsequent valid frame still works; malformed input did not poison RPC.
    let outcome = host.data("durable_mutate", valid);
    assert_eq!(outcome["outcome"]["replayed"], false);
    host.assert_idle();
    drop(host);
    fixture.assert_no_provider_calls();
}

#[test]
fn tool_filters_do_not_disable_explicit_native_rpc() {
    for flags in [
        vec!["--no-tools"],
        vec!["--no-builtin-tools"],
        vec!["--tools", "durable", "--exclude-tools", "durable"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["--no-session"];
        args.extend(flags);
        let mut host = fixture.start(true, &args);
        let tools = host.data("get_available_tools", json!({}));
        assert!(!has_name(&tools, "tools", "durable"), "{tools}");
        assert!(!has_name(&tools, "activeTools", "durable"), "{tools}");
        let commands = host.data("get_commands", json!({}));
        let durable: Vec<_> = commands["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|command| command["name"] == "durable")
            .collect();
        assert_eq!(
            durable.len(),
            1,
            "native discovery is host opt-in, not model-tool selection"
        );
        assert_eq!(
            durable[0]["description"],
            "Native durable workflow / explicit run view or guarded JSON mutation (v1 prototype)"
        );
        let outcome = host.mutate("manual-start", start_action());
        assert_eq!(
            outcome["snapshot"]["state"]["workflow"]["name"],
            "synthetic workflow"
        );
        let state = host.state();
        assert_eq!(
            host.data("execute_command", json!({"command":"durable","args":""}))["state"],
            state
        );
        host.assert_idle();
        drop(host);
        fixture.assert_no_provider_calls();
    }
}

#[test]
fn native_command_discovery_preserves_json_execution_and_rejects_non_string_args() {
    let fixture = Fixture::new();
    let mut host = fixture.start(true, &["--no-session", "--no-tools"]);
    let before = host.state();
    let commands = host.data("get_commands", json!({}));
    assert_eq!(
        commands["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|command| command["name"] == "durable")
            .count(),
        1
    );
    for args in [
        Value::Null,
        json!(42),
        json!(false),
        json!({"action":"status"}),
        json!([]),
    ] {
        let response = host.request("execute_command", json!({"command":"/durable","args":args}));
        assert_eq!(response["success"], false, "{response}");
        assert_eq!(response["error"], "invalid command payload", "{response}");
        assert!(response.get("data").is_none());
        assert!(
            response.get("errorDetails").is_none(),
            "retain the legacy outer Request error contract"
        );
        assert_eq!(
            host.state(),
            before,
            "malformed args must not be silently treated as a status read"
        );
    }
    // Help belongs only to the human TUI; RPC remains the flat-JSON API.
    assert_error(
        &host.request(
            "execute_command",
            json!({"command":"durable","args":"help"}),
        ),
        "invalid_request",
    );
    let default_status = host.data("execute_command", json!({"command":"durable"}));
    assert_eq!(default_status["version"], 1);
    assert_eq!(default_status["state"], before);
    let payload = json!({"action":"enqueue","requestId":"discovery-spaces","text":"keep  界  spaces
inside"});
    let outcome = host.data(
        "execute_command",
        json!({"command":"/durable","args":payload.to_string()}),
    );
    assert_eq!(outcome["version"], 1);
    assert_eq!(
        outcome["outcome"]["snapshot"]["state"]["queue"][0]["text"],
        payload["text"]
    );
    assert_eq!(outcome["outcome"]["replayed"], false);
    let receipt = host.data("execute_command", json!({"command":"durable","args":json!({"action":"status","requestId":"discovery-spaces"}).to_string()}));
    assert_eq!(receipt["receipt"]["requestId"], "discovery-spaces");
    assert_eq!(
        receipt["receipt"]["entryId"],
        outcome["outcome"]["receipt"]["entryId"]
    );
    let after = host.state();
    assert_eq!(after, outcome["outcome"]["snapshot"]);
    let _ = host.data("get_commands", json!({}));
    assert_eq!(
        host.state(),
        after,
        "command discovery must not execute, recover or write"
    );
    host.assert_idle();
    drop(host);
    fixture.assert_no_provider_calls();
}
