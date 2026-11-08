//! Real current-SDK resource boundaries for the opt-in native durable host.
use super::*;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn owned_library(fixture: &Fixture, built: &resource_fixture::Fixture) -> PathBuf {
    let directory = fixture.root.path().join("lib");
    std::fs::create_dir_all(&directory).unwrap();
    let library = directory.join(built.library.file_name().unwrap());
    std::fs::copy(&built.library, &library).unwrap();
    library
}

fn ready(command: &mut Command, enabled: bool) -> (Host, Value) {
    let mut host = Host::spawn(command);
    let event = host.until(|v| v["type"] == "ready");
    assert_eq!(event["durable"]["enabled"], enabled, "{event}");
    (host, event)
}

// Read to EOF with one overall deadline, never accepting a transient READY.
// Host owns bounded stderr and kills/reaps even when an assertion unwinds.
fn rejected(command: &mut Command, expected: &str) -> String {
    let mut host = Host::spawn(command);
    let deadline = Instant::now() + WAIT;
    loop {
        match host
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(line) => {
                let event: Value = serde_json::from_str(&line).expect("JSON startup stdout");
                assert_ne!(
                    event["type"], "ready",
                    "rejected startup reached READY: {event}"
                );
                assert!(
                    Instant::now() < deadline,
                    "startup rejection never reached EOF"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!(
                "startup rejection timed out: {error}; {}",
                String::from_utf8_lossy(&host.stderr.lock().unwrap())
            ),
        }
    }
    let status = loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "rejected child did not exit");
        thread::sleep(Duration::from_millis(10));
    };
    for reader in host.readers.drain(..) {
        reader.join().unwrap();
    }
    let stderr = String::from_utf8_lossy(&host.stderr.lock().unwrap()).into_owned();
    assert!(!status.success(), "rejection exited successfully: {stderr}");
    assert!(stderr.contains(expected), "expected {expected:?}: {stderr}");
    stderr
}

// Called only after hosts have exited: include every primary/companion
// journal file and its bytes, not just the transcript or a lossy parsed
// projection. The sibling .lock record is deliberately excluded: a successful
// native open and a fail-closed admission attempt may advance its persisted
// lease fence without changing journal/artifact bytes.
fn session_bytes(fixture: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if path.is_file() {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
            {
                return;
            }
            files.insert(path.to_path_buf(), std::fs::read(path).unwrap());
        } else if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                walk(&entry.unwrap().path(), files);
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(&fixture.root.path().join("sessions"), &mut files);
    assert!(!files.is_empty());
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_durable_refuses_real_extension_tool_and_command_even_when_filtered() {
    let built = resource_fixture::Fixture::build().await;
    for command_case in [false, true] {
        let fixture = Fixture::new();
        let library = owned_library(&fixture, &built);
        let log = fixture.root.path().join("registrations.log");
        let configure = |enabled| {
            let mut cmd = fixture.command(
                enabled,
                &[
                    "--extension",
                    library.to_str().unwrap(),
                    "--approve",
                    "--no-session",
                ],
            );
            cmd.env("RPI_PLUGIN_STUB_REGISTRATION_LOG", &log);
            if command_case {
                cmd.env("RPI_PLUGIN_STUB_COMMAND_NAME", "durable");
            } else {
                cmd.env("RPI_PLUGIN_STUB_TOOL_NAME", "durable");
            }
            cmd
        };
        let (mut legacy, event) = ready(&mut configure(false), false);
        assert_eq!(event["resourceState"]["enabled"], false, "{event}");
        if command_case {
            assert!(has_name(
                &legacy.data("get_commands", json!({})),
                "commands",
                "durable"
            ));
            assert_eq!(
                legacy.data("execute_command", json!({"command":"durable","args":""})),
                json!({"fixture":true})
            );
        } else {
            assert!(has_name(
                &legacy.data("get_available_tools", json!({})),
                "tools",
                "durable"
            ));
        }
        drop(legacy);
        assert_eq!(built.assert_registrations(&log).lines().count(), 1);
        for (index, filter) in [
            &[][..],
            &["--no-tools"][..],
            &["--exclude-tools", "durable"][..],
        ]
        .into_iter()
        .enumerate()
        {
            rejected(
                configure(true).args(filter),
                "--native-durable conflicts with an extension durable tool or /durable command",
            );
            assert_eq!(built.assert_registrations(&log).lines().count(), index + 2);
        }
        fixture.assert_no_provider_calls();
    }
}

// Abort on every unwind/timeout path as well as normal completion; this task
// owns all accepted sockets. No external endpoint or inherited credential exists.
struct LocalServer(tokio::task::JoinHandle<()>);
impl Drop for LocalServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn request_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "incomplete local HTTP headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 1024 * 1024, "oversized local request");
        if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let length: usize = String::from_utf8_lossy(&bytes[..end])
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("local request content-length");
    assert!(end + length <= 1024 * 1024);
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "incomplete local HTTP body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_resources_execute_bound_native_durable_persist_and_reject_changed_dll() {
    let built = resource_fixture::Fixture::build().await;
    let fixture = Fixture::new();
    let library = owned_library(&fixture, &built);
    let log = fixture.root.path().join("registrations.log");
    let config = fixture.root.path().join("project/exact.json");
    std::fs::write(
        &config,
        json!({"version":1,"extensions":[library],
        "tools":{"mode":"only","names":["durable","echo"]}})
        .to_string(),
    )
    .unwrap();
    let configure = |session_flag: &str, session: &str| {
        let mut command = fixture.command(
            true,
            &[
                "--approve",
                "--resource-config",
                config.to_str().unwrap(),
                session_flag,
                session,
            ],
        );
        command.env("RPI_PLUGIN_STUB_REGISTRATION_LOG", &log);
        command
    };
    let listener =
        tokio::net::TcpListener::from_std(fixture.provider.try_clone().unwrap()).unwrap();
    let mut server = LocalServer(tokio::spawn(async move {
        tokio::time::timeout(WAIT, async move {
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = request_body(&mut socket).await;
                let mut tools: Vec<_> = request["tools"].as_array().unwrap().iter()
                    .map(|tool| tool["function"]["name"].as_str().unwrap()).collect();
                tools.sort_unstable();
                assert_eq!(tools, ["durable", "echo"], "{request}");
                let results: Vec<_> = request["messages"].as_array().unwrap().iter()
                    .filter(|message| message["role"] == "tool").collect();
                let frame = if turn == 0 {
                    assert!(results.is_empty(), "{request}");
                    assert!(!request.to_string().contains("Native durable observation"));
                    let args = json!({"action":"start","requestId":"exact-start","name":"exact",
                        "steps":[{"id":"verify","title":"Verify exact binding"}]}).to_string();
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,
                        "id":"native-start","type":"function","function":{"name":"durable","arguments":args}}]},
                        "finish_reason":"tool_calls"}]})
                } else {
                    assert_eq!(results.len(), 1, "{request}");
                    // A tool mutation refreshes the next request's observation,
                    // even inside the same agent run and under exact selection.
                    let observations: Vec<_> = request["messages"].as_array().unwrap().iter()
                        .filter_map(|message| {
                            let text = match &message["content"] {
                                Value::String(text) => text.clone(),
                                Value::Array(parts) => parts.iter()
                                    .filter_map(|part| part["text"].as_str())
                                    .collect::<Vec<_>>().join(""),
                                _ => return None,
                            };
                            text.starts_with("Native durable observation").then_some((message, text))
                        }).collect();
                    assert_eq!(observations.len(), 1, "{request}");
                    assert_eq!(observations[0].0["role"], "user");
                    let body = observations[0].1
                        .split_once("```json\n").unwrap().1.strip_suffix("\n```").unwrap();
                    let projection: Value = serde_json::from_str(body).unwrap();
                    assert_eq!(projection["workflow"]["name"], "exact");
                    assert_eq!(projection["controlRevision"], 1);
                    assert_eq!(results[0]["tool_call_id"], "native-start");
                    let result: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
                    assert_eq!(result["outcome"]["replayed"], false, "{result}");
                    assert_eq!(result["outcome"]["snapshot"]["state"]["workflow"]["name"], "exact", "{result}");
                    assert_eq!(result["outcome"]["snapshot"]["scope"]["controlRevision"], 1, "{result}");
                    json!({"choices":[{"index":0,"delta":{"content":"fixture complete"},"finish_reason":"stop"}]})
                };
                let response = format!("data: {frame}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        }).await.expect("controlled local provider timed out");
    }));
    let (mut host, event) = ready(&mut configure("--session-id", "native-durable-exact"), true);
    assert_eq!(event["resourceState"]["enabled"], true, "{event}");
    let resources = host.data("get_resource_state", json!({"version":1}));
    assert_eq!(resources["binding"], event["resourceState"]["binding"]);
    let fingerprint = resources["binding"]["fingerprint"].as_str().unwrap();
    assert_eq!(fingerprint.len(), 64);
    let initial = host.data("get_durable_state", json!({"version":1}))["state"].clone();
    assert_eq!(resources["sessionId"], "native-durable-exact");
    assert_eq!(resources["sessionId"], initial["scope"]["sessionId"]);
    assert_eq!(
        event["resourceState"]["sessionId"],
        initial["scope"]["sessionId"]
    );
    let tools = host.data("get_available_tools", json!({}));
    assert!(has_name(&tools, "tools", "durable") && has_name(&tools, "tools", "echo"));
    assert_eq!(tools["activeTools"], json!(["durable", "echo"]));
    let accepted = host.request(
        "prompt",
        json!({"message":"Start the exact fixture workflow once."}),
    );
    assert_eq!(accepted["success"], true, "{accepted}");
    // Unlike the parent's manual-only until(), an intentional agent run may
    // emit agent_start and tool_execution_start before settlement.
    let deadline = Instant::now() + WAIT;
    loop {
        let line = host
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("exact native agent did not settle");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["type"] == "agent_settled" {
            assert_eq!(event["outcome"], "completed", "{event}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "exact native agent never settled"
        );
    }
    tokio::time::timeout(WAIT, &mut server.0)
        .await
        .unwrap()
        .unwrap();
    let saved = host.data("get_durable_state", json!({"version":1}))["state"].clone();
    assert_eq!(saved["scope"]["controlRevision"], 1);
    assert_eq!(saved["state"]["workflow"]["name"], "exact");
    let receipt = host.data(
        "get_durable_request",
        json!({"version":1,"requestId":"exact-start"}),
    );
    assert!(receipt["receipt"].is_object(), "{receipt}");
    let records = host.data(
        "get_session_branch",
        json!({"customTypes":["rpi.durable.control.v1"]}),
    );
    assert_eq!(records["count"], 1, "{records}");
    assert_eq!(
        records["entries"][0]["data"]["receipts"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{records}"
    );
    drop(host);
    let path = fixture.session_file();
    let before_reopen = session_bytes(&fixture);
    let (mut reopened, event) = ready(&mut configure("--session", path.to_str().unwrap()), true);
    assert_eq!(
        event["resourceState"]["binding"]["fingerprint"],
        fingerprint
    );
    let restored = reopened.data("get_durable_state", json!({"version":1}))["state"].clone();
    assert_eq!(restored["state"], saved["state"]);
    assert_eq!(restored["scope"]["controlRevision"], 1);
    assert_eq!(
        reopened.data(
            "get_durable_request",
            json!({"version":1,"requestId":"exact-start"})
        ),
        receipt
    );
    drop(reopened);
    assert_eq!(
        session_bytes(&fixture),
        before_reopen,
        "read-only reopen changed session bytes"
    );
    let registrations = built.assert_registrations(&log);
    assert_eq!(registrations.lines().count(), 2);
    // Overlay bytes preserve DLL loadability if fingerprint preflight regresses.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&library)
        .unwrap()
        .write_all(b"\0")
        .unwrap();
    let before_rejection = session_bytes(&fixture);
    rejected(
        &mut configure("--session", path.to_str().unwrap()),
        "session resource artifact inventory changed",
    );
    assert_eq!(
        built.assert_registrations(&log),
        registrations,
        "tampered DLL registered before rejection"
    );
    assert_eq!(
        session_bytes(&fixture),
        before_rejection,
        "rejected fingerprint check wrote session files"
    );
    // The server has served exactly two requests and closed its listener. Any
    // unexpected connection on reopen/rejection remains in this owned backlog.
    assert!(
        matches!(fixture.provider.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn exact_resource_tool_filters_preserve_explicit_manual_native_control() {
    for (selection, expected_tools) in [
        (json!({"mode":"none"}), json!([])),
        (json!({"mode":"only","names":["read"]}), json!(["read"])),
    ] {
        let fixture = Fixture::new();
        let config = fixture.root.path().join("project/exact.json");
        std::fs::write(&config, json!({"version":1,"tools":selection}).to_string()).unwrap();
        let (mut host, event) = ready(
            &mut fixture.command(
                true,
                &[
                    "--approve",
                    "--resource-config",
                    config.to_str().unwrap(),
                    "--no-session",
                ],
            ),
            true,
        );
        assert_eq!(event["resourceState"]["enabled"], true, "{event}");
        let tools = host.data("get_available_tools", json!({}));
        // Exact selection freezes model exposure, not the host's inventory.
        assert!(has_name(&tools, "tools", "durable"), "{tools}");
        assert!(!has_name(&tools, "activeTools", "durable"), "{tools}");
        assert_eq!(tools["activeTools"], expected_tools, "{tools}");
        assert!(has_name(
            &host.data("get_commands", json!({})),
            "commands",
            "durable"
        ));
        let outcome = host.mutate("exact-manual-start", start_action());
        assert_eq!(outcome["snapshot"]["scope"]["controlRevision"], 1);
        assert_eq!(
            outcome["snapshot"]["state"]["workflow"]["name"],
            "synthetic workflow"
        );
        host.assert_idle();
        drop(host);
        fixture.assert_no_provider_calls();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_durable_journals_require_explicit_import_and_remain_legacy_without_flag() {
    for kind in ["durable-state", "rpi-durable-runtime"] {
        let fixture = Fixture::new();
        let mut legacy = fixture.start(false, &["--session-id", "legacy-durable", "--no-tools"]);
        legacy.data("append_entry", json!({"customType":kind,"data":{}}));
        let entry = legacy.data("get_session_branch", json!({"customTypes":[kind]}));
        assert_eq!(entry["count"], 1, "{entry}");
        drop(legacy);
        let path = fixture.session_file();
        let before = session_bytes(&fixture);
        let error = rejected(
            &mut fixture.command(
                true,
                &[
                    "--session",
                    path.to_str().unwrap(),
                    "--no-extensions",
                    "--no-skills",
                    "--no-context-files",
                    "--no-prompt-templates",
                    "--no-tools",
                ],
            ),
            "legacy durable journal requires explicit native import",
        );
        assert!(error.contains("explicit native import"), "{error}");
        assert_eq!(
            session_bytes(&fixture),
            before,
            "legacy rejection modified session bytes"
        );
        let mut reopened =
            fixture.start(false, &["--session", path.to_str().unwrap(), "--no-tools"]);
        assert_eq!(
            reopened.data("get_session_branch", json!({"customTypes":[kind]})),
            entry
        );
        let disabled = reopened.request("get_durable_state", json!({"version":1}));
        assert_error(&disabled, "disabled");
        drop(reopened);
        assert_eq!(
            session_bytes(&fixture),
            before,
            "legacy resume imported or rewrote the journal"
        );
        fixture.assert_no_provider_calls();
    }
}
