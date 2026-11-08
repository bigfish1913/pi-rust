//! Provider-boundary observations are ephemeral, fresh, and explicitly opt-in.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OBSERVATION: &str = "Native durable observation";
const CONTEXT_TYPE: &str = "rpi.native-durable.context.v1";
const CONTROL_TYPE: &str = "rpi.durable.control.v1";

// Own every accepted socket and abort the task even if a host assertion unwinds.
struct ContextServer(tokio::task::JoinHandle<Vec<Value>>);
impl Drop for ContextServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Kept local so this module does not require changing the resource tests' API.
async fn context_request_body(socket: &mut tokio::net::TcpStream) -> Value {
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
    assert!(length <= 1024 * 1024 - end);
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "incomplete local HTTP body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[end..end + length]).unwrap()
}

fn context_server(fixture: &Fixture, count: usize) -> ContextServer {
    let listener =
        tokio::net::TcpListener::from_std(fixture.provider.try_clone().unwrap()).unwrap();
    ContextServer(tokio::spawn(async move {
        let mut requests = Vec::new();
        for turn in 0..count {
            let request = tokio::time::timeout(WAIT, async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = context_request_body(&mut socket).await;
                assert!(
                    request["tools"].is_null()
                        || request["tools"].as_array().is_some_and(Vec::is_empty),
                    "--no-tools still exposed model tools: {request}"
                );
                let frame = json!({"choices":[{"index":0,
                    "delta":{"content":format!("context response {turn}")},
                    "finish_reason":"stop"}]});
                let response = format!("data: {frame}\n\ndata: [DONE]\n\n");
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                ).as_bytes()).await.unwrap();
                request
            })
            .await
            .expect("controlled context provider timed out");
            requests.push(request);
        }
        requests
    }))
}

fn no_observation(value: &Value) {
    let encoded = value.to_string();
    assert!(
        !encoded.contains(OBSERVATION),
        "persisted/emitted observation: {value}"
    );
    assert!(
        !encoded.contains(CONTEXT_TYPE),
        "persisted/emitted context type: {value}"
    );
}

fn content_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("expected text content, got {other}"),
    }
}

fn control_entries(host: &mut Host) -> Value {
    host.data("get_session_branch", json!({"customTypes":[CONTROL_TYPE]}))["entries"].clone()
}

// Host::until deliberately rejects agent_start for the manual-only tests. A
// prompt can acknowledge before OR after its events: drain both the correlated
// ACK and settlement, permitting agent events but never tool execution/context.
fn context_prompt(host: &mut Host, turn: usize) {
    host.next_id += 1;
    let id = json!(format!("context-prompt-{}", host.next_id));
    let request = json!({"id":id,"type":"prompt","message":format!("context request {turn}")});
    let input = host.input.as_mut().unwrap();
    writeln!(input, "{request}").unwrap();
    input.flush().unwrap();
    let deadline = Instant::now() + WAIT;
    let (mut accepted, mut settled) = (false, false);
    while !accepted || !settled {
        let line = host
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "context prompt timed out/EOF ({error}); stderr: {}",
                    String::from_utf8_lossy(&host.stderr.lock().unwrap())
                )
            });
        let event: Value = serde_json::from_str(&line).expect("JSON native stdout");
        no_observation(&event);
        assert!(
            !event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("tool_execution")),
            "scripted text response executed a tool: {event}"
        );
        if event["type"] == "response" && event["id"] == id {
            assert_eq!(event["success"], true, "{event}");
            accepted = true;
        }
        if event["type"] == "agent_settled" {
            assert!(!settled, "duplicate settlement: {event}");
            assert_eq!(event["outcome"], "completed", "{event}");
            settled = true;
        }
        assert!(Instant::now() < deadline, "context prompt never settled");
    }
}

// Ordinary transcript entries still persist; model preparation must not change
// business control state, add control records, or leak custom context to history.
fn observed_prompt(host: &mut Host, turn: usize) -> Value {
    let before = host.state();
    let controls = control_entries(host);
    let history = host.data("get_messages", json!({}));
    no_observation(&history);
    assert_eq!(host.data("get_messages", json!({})), history);
    context_prompt(host, turn);
    let after = host.state();
    assert_eq!(after["state"], before["state"]);
    assert_eq!(after["executionControl"], before["executionControl"]);
    assert_eq!(
        after["scope"]["controlRevision"],
        before["scope"]["controlRevision"]
    );
    assert_eq!(
        control_entries(host),
        controls,
        "model request wrote durable records"
    );
    let persisted = host.data("get_messages", json!({}));
    no_observation(&persisted);
    let messages = persisted["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        history["messages"].as_array().unwrap().len() + 2
    );
    let user = &messages[messages.len() - 2];
    let assistant = &messages[messages.len() - 1];
    assert_eq!(user["role"], "user");
    assert_eq!(content_text(user), format!("context request {turn}"));
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(content_text(assistant), format!("context response {turn}"));
    no_observation(&host.data("get_session_branch", json!({"customTypes":[CONTROL_TYPE]})));
    before
}

fn assert_projection(request: &Value, state: &Value, active: bool) {
    let messages = request["messages"].as_array().expect("provider messages");
    let observations: Vec<_> = messages
        .iter()
        .filter(|message| content_text(message).contains(OBSERVATION))
        .collect();
    assert_eq!(observations.len(), usize::from(active), "{request}");
    // No cached copy from any earlier turn, including after reset and reopen.
    assert_eq!(
        request.to_string().matches(OBSERVATION).count(),
        usize::from(active)
    );
    if !active {
        assert!(!request.to_string().contains(CONTEXT_TYPE));
        return;
    }
    let message = observations[0];
    assert_eq!(
        message["role"], "user",
        "observation must not become system instructions"
    );
    assert_eq!(
        messages
            .iter()
            .find(|message| message["role"] != "system" && message["role"] != "developer"),
        Some(message),
        "observation must precede ordinary history"
    );
    let text = content_text(message);
    assert!(text.starts_with(OBSERVATION));
    assert!(
        text.len() <= 16 * 1024,
        "context exceeded its UTF-8 byte budget"
    );
    let (_, body) = text
        .split_once("```json\n")
        .expect("JSON fenced observation");
    let body = body.strip_suffix("\n```").expect("closed JSON fence");
    let payload: Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        payload,
        json!({
            "version":1,"availability":"available","detail":"full",
            "sessionId":state["scope"]["sessionId"],"lane":state["scope"]["lane"],
            "controlRevision":state["scope"]["controlRevision"],
            "workflow":state["state"]["workflow"],
            "queueView":"queued_and_claimed_only","openQueue":[],
            "queueCounts":{"queued":0,"claimed":0,"completed":0,"failed":0,"cancelled":0},
            "recoveryRequest":null
        })
    );
    for forbidden in [
        "scope",
        "bindingToken",
        "expected",
        "entryId",
        "leafId",
        "receipts",
    ] {
        assert!(
            !body.contains(&format!("\"{forbidden}\"")),
            "leaked {forbidden}: {body}"
        );
    }
}

fn assert_owned_jsonl_clean(path: &Path, prompts: usize, request_ids: &[&str]) {
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(
        !raw.contains(OBSERVATION),
        "context persisted in owned JSONL"
    );
    assert!(
        !raw.contains(CONTEXT_TYPE),
        "custom context persisted in owned JSONL"
    );
    let entries: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let messages: Vec<_> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .collect();
    assert_eq!(
        messages.len(),
        prompts * 2,
        "ordinary transcript did not persist"
    );
    let controls: Vec<_> = entries
        .iter()
        .filter(|entry| entry["customType"] == CONTROL_TYPE)
        .collect();
    assert_eq!(
        controls.len(),
        request_ids.len(),
        "unexpected business control records"
    );
    for (index, entry) in controls.iter().enumerate() {
        let receipts: Vec<_> = entry["data"]["receipts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|receipt| receipt["requestId"].as_str().unwrap())
            .collect();
        assert_eq!(
            receipts,
            request_ids[..=index],
            "unexpected or missing receipt IDs"
        );
    }
    for turn in 0..prompts {
        assert_eq!(
            content_text(&messages[turn * 2]["message"]),
            format!("context request {turn}")
        );
        assert_eq!(
            content_text(&messages[turn * 2 + 1]["message"]),
            format!("context response {turn}")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_context_is_fresh_ephemeral_and_restored_without_selected_tools() {
    let fixture = Fixture::new();
    let mut server = context_server(&fixture, 6);
    let mut host = fixture.start(
        true,
        &["--session-id", "native-durable-context", "--no-tools"],
    );
    assert_eq!(
        host.data("get_available_tools", json!({}))["activeTools"],
        json!([])
    );
    let mut snapshots = vec![observed_prompt(&mut host, 0)];
    assert!(snapshots[0]["state"]["workflow"].is_null());
    let start = host.mutate(
        "context-start",
        json!({"action":"start","name":"context workflow",
        "steps":[{"id":"s","title":"CONTEXT_OLD_MARKER"}]}),
    );
    snapshots.push(observed_prompt(&mut host, 1));
    assert_eq!(snapshots[1]["state"]["workflow"]["status"], "running");
    assert_eq!(
        snapshots[1]["state"]["workflow"]["steps"][0]["title"],
        "CONTEXT_OLD_MARKER"
    );
    host.mutate(
        "context-checkpoint",
        json!({"action":"checkpoint","step":"s",
        "status":"in_progress","note":"CONTEXT_NEW_MARKER"}),
    );
    host.mutate(
        "context-pause",
        json!({"action":"pause","reason":"WAIT_MARKER"}),
    );
    snapshots.push(observed_prompt(&mut host, 2));
    assert_eq!(snapshots[2]["state"]["workflow"]["status"], "paused");
    assert_eq!(
        snapshots[2]["state"]["workflow"]["steps"][0]["status"],
        "in_progress"
    );
    assert_eq!(
        snapshots[2]["state"]["workflow"]["steps"][0]["note"],
        "CONTEXT_NEW_MARKER"
    );
    assert_eq!(snapshots[2]["state"]["workflow"]["reason"], "WAIT_MARKER");
    host.mutate("context-reset", json!({"action":"reset"}));
    snapshots.push(observed_prompt(&mut host, 3));
    assert!(snapshots[3]["state"]["workflow"].is_null());
    let restored_start = host.mutate(
        "context-restored-start",
        json!({"action":"start",
        "name":"restored workflow","steps":[{"id":"s","title":"RESTORED_MARKER"}]}),
    );
    snapshots.push(observed_prompt(&mut host, 4));
    let saved = host.state();
    let saved_entries = control_entries(&mut host);
    assert_eq!(
        saved_entries.as_array().unwrap().len(),
        5,
        "only manual mutations may write control records"
    );
    let ids = [
        "context-start",
        "context-checkpoint",
        "context-pause",
        "context-reset",
        "context-restored-start",
    ];
    let receipts: Vec<_> = ids
        .iter()
        .map(|id| {
            host.data("get_durable_request", json!({"version":1,"requestId":id}))["receipt"].clone()
        })
        .collect();
    assert_eq!(receipts[0], start["receipt"]);
    assert_eq!(receipts[4], restored_start["receipt"]);
    for receipt in &receipts {
        assert!(receipt.is_object());
        assert_eq!(receipt["actor"], "native-cli");
    }
    for (index, entry) in saved_entries.as_array().unwrap().iter().enumerate() {
        assert_eq!(
            entry["data"]["receipts"].as_array().unwrap().len(),
            index + 1
        );
    }
    let history = host.data("get_messages", json!({}));
    let path = fixture.session_file();
    drop(host); // kill/reap: no graceful handshake can manufacture restored state
    assert_owned_jsonl_clean(&path, 5, &ids);
    let mut reopened = fixture.start(true, &["--session", path.to_str().unwrap(), "--no-tools"]);
    let restored = reopened.state();
    assert_eq!(restored["state"], saved["state"]);
    assert_eq!(restored["executionControl"], saved["executionControl"]);
    assert_eq!(
        restored["scope"]["controlRevision"],
        saved["scope"]["controlRevision"]
    );
    assert_eq!(
        reopened.data("get_messages", json!({}))["messages"],
        history["messages"]
    );
    assert_eq!(control_entries(&mut reopened), saved_entries);
    snapshots.push(observed_prompt(&mut reopened, 5));
    assert_eq!(
        snapshots[5]["state"]["workflow"]["steps"][0]["title"],
        "RESTORED_MARKER"
    );
    for (id, receipt) in ids.iter().zip(&receipts) {
        assert_eq!(
            reopened.data("get_durable_request", json!({"version":1,"requestId":id}))["receipt"],
            *receipt
        );
    }
    assert_eq!(control_entries(&mut reopened), saved_entries);
    drop(reopened);
    assert_owned_jsonl_clean(&path, 6, &ids);
    let requests = tokio::time::timeout(WAIT, &mut server.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 6);
    let system_messages = |request: &Value| -> Vec<Value> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system" || message["role"] == "developer")
            .cloned()
            .collect()
    };
    let system = system_messages(&requests[0]);
    assert!(
        !system.is_empty(),
        "fixture must exercise the ordinary system prompt"
    );
    for (turn, (request, snapshot)) in requests.iter().zip(&snapshots).enumerate() {
        assert_projection(request, snapshot, !matches!(turn, 0 | 3));
        assert_eq!(
            system_messages(request),
            system,
            "durable context mutated the system prompt"
        );
    }
    assert!(!requests[4].to_string().contains("CONTEXT_OLD_MARKER"));
    assert!(!requests[5].to_string().contains("CONTEXT_NEW_MARKER"));
    drop(server);
    fixture.assert_no_provider_calls();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_marker_added_after_start_never_becomes_provider_context() {
    let fixture = Fixture::new();
    let mut server = context_server(&fixture, 1);
    let mut host = fixture.start(
        false,
        &["--session-id", "legacy-context-disabled", "--no-tools"],
    );
    host.data(
        "append_entry",
        json!({"customType":"durable-state",
        "data":{"name":"LEGACY_NOT_NATIVE_CONTEXT"}}),
    );
    let legacy = host.data(
        "get_session_branch",
        json!({"customTypes":["durable-state"]}),
    );
    assert_eq!(legacy["count"], 1);
    assert_eq!(
        legacy["entries"][0]["data"]["name"],
        "LEGACY_NOT_NATIVE_CONTEXT"
    );
    assert_error(
        &host.request("get_durable_state", json!({"version":1})),
        "disabled",
    );
    let tools = host.data("get_available_tools", json!({}));
    assert!(!has_name(&tools, "tools", "durable"));
    assert_eq!(tools["activeTools"], json!([]));
    assert!(!has_name(
        &host.data("get_commands", json!({})),
        "commands",
        "durable"
    ));
    let before = host.data("get_messages", json!({}));
    assert_eq!(before["messages"], json!([]));
    context_prompt(&mut host, 0);
    let history = host.data("get_messages", json!({}));
    no_observation(&history);
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
    assert_eq!(content_text(&history["messages"][0]), "context request 0");
    assert_eq!(content_text(&history["messages"][1]), "context response 0");
    let after = host.data(
        "get_session_branch",
        json!({"customTypes":["durable-state"]}),
    );
    assert_eq!(
        after["entries"], legacy["entries"],
        "legacy marker was imported or rewritten"
    );
    assert_eq!(control_entries(&mut host), json!([]));
    assert_error(
        &host.request("get_durable_state", json!({"version":1})),
        "disabled",
    );
    let path = fixture.session_file();
    drop(host);
    assert_owned_jsonl_clean(&path, 1, &[]);
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("LEGACY_NOT_NATIVE_CONTEXT"));
    let requests = tokio::time::timeout(WAIT, &mut server.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_projection(&requests[0], &Value::Null, false);
    assert!(!requests[0]
        .to_string()
        .contains("LEGACY_NOT_NATIVE_CONTEXT"));
    drop(server);
    fixture.assert_no_provider_calls();
}
