//! Real isolated JSONL process shutdown; unlike the process-death coverage,
//! these tests never kill a child to establish successful persistence.
use super::*;

fn send_without_waiting(host: &mut Host, kind: &str, mut fields: Value) -> Value {
    host.next_id += 1;
    let id = json!(format!("shutdown-transport-{}", host.next_id));
    fields["type"] = json!(kind);
    fields["id"] = id.clone();
    let input = host.input.as_mut().unwrap();
    writeln!(input, "{fields}").unwrap();
    input.flush().unwrap();
    id
}

fn collect_and_wait_for_normal_exit(host: &mut Host) -> Vec<Value> {
    collect_and_wait_for_exit(host, false)
}

fn collect_and_wait_for_exit(host: &mut Host, allow_agent_events: bool) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    let mut frames = Vec::new();
    loop {
        match host
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(line) => {
                let frame: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("non-JSON shutdown output ({error}): {line}"));
                if !allow_agent_events {
                    assert!(
                        !matches!(
                            frame["type"].as_str(),
                            Some("agent_start" | "tool_execution_start")
                        ),
                        "manual shutdown started execution: {frame}"
                    );
                }
                frames.push(frame);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!(
                "shutdown stdout did not close: {error}; stderr: {}",
                String::from_utf8_lossy(&host.stderr.lock().unwrap())
            ),
        }
        assert!(Instant::now() < deadline, "shutdown output did not settle");
    }
    // stdout EOF need not coincide exactly with OS process reaping. No kill,
    // fixed delay, or Host::drop is used as evidence of successful shutdown.
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(0),
                "shutdown failed: {status}; stderr: {}",
                String::from_utf8_lossy(&host.stderr.lock().unwrap())
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "RPC child did not exit after stdout EOF"
        );
        thread::yield_now();
    }
    frames
}

fn graceful_shutdown(stop: bool) {
    let fixture = Fixture::new();
    let mut host = fixture.start(
        true,
        &[
            "--session-id",
            "native-durable-graceful-shutdown",
            "--no-tools",
        ],
    );
    let initial = host.state();
    let request_id = if stop {
        "graceful-stop-start"
    } else {
        "graceful-eof-start"
    };
    let mutation_id = send_without_waiting(
        &mut host,
        "durable_mutate",
        mutation(request_id, initial["scope"].clone(), start_action()),
    );
    // Do not wait for mutation response/commit before closing the input side or
    // dispatching stop. The accepted command must be joined by normal shutdown.
    let stop_id = if stop {
        Some(send_without_waiting(&mut host, "stop", json!({})))
    } else {
        host.input.take();
        None
    };
    let frames = collect_and_wait_for_normal_exit(&mut host);
    let response = frames
        .iter()
        .find(|frame| frame["type"] == "response" && frame["id"] == mutation_id)
        .expect("accepted mutation response must flush before process exit");
    assert_eq!(response["success"], true, "{response}");
    let outcome = response["data"]["outcome"].clone();
    assert_eq!(outcome["replayed"], false);
    let receipt = outcome["receipt"].clone();
    assert_eq!(receipt["requestId"], request_id);
    assert_eq!(receipt["actor"], "native-cli");
    assert_eq!(
        outcome["snapshot"]["state"]["workflow"]["status"],
        "running"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "session_entry_committed"
                && frame["customType"] == "rpi.durable.control.v1"
                && frame["entryId"] == receipt["entryId"])
            .count(),
        1,
        "receipt commit invalidation must flush: {frames:?}"
    );
    if let Some(stop_id) = stop_id {
        let ack = frames
            .iter()
            .find(|frame| frame["type"] == "response" && frame["id"] == stop_id)
            .expect("stop acknowledgement must flush");
        assert_eq!(ack["success"], true, "{ack}");
        assert_eq!(ack["data"]["stopped"], true, "{ack}");
    }
    let session_file = fixture.session_file();
    let bytes = std::fs::read(&session_file).unwrap();
    // Child has already exited successfully; Host::drop remains failure cleanup
    // only, retaining its existing kill/reap safety net without changing it.
    drop(host);
    fixture.assert_no_provider_calls();
    let mut reopened = fixture.start(
        true,
        &["--session", session_file.to_str().unwrap(), "--no-tools"],
    );
    let saved = reopened.state();
    assert_eq!(saved["state"], outcome["snapshot"]["state"]);
    assert_eq!(
        saved["executionControl"],
        outcome["snapshot"]["executionControl"]
    );
    assert_eq!(
        saved["scope"]["controlRevision"],
        outcome["snapshot"]["scope"]["controlRevision"]
    );
    assert_eq!(saved["scope"]["entryId"], receipt["entryId"]);
    assert_eq!(
        reopened.data(
            "get_durable_request",
            json!({"version":1,"requestId":request_id})
        )["receipt"],
        receipt
    );
    reopened.assert_idle();
    reopened.input.take();
    let reopened_frames = collect_and_wait_for_normal_exit(&mut reopened);
    assert!(
        !reopened_frames
            .iter()
            .any(|frame| frame["type"] == "session_entry_committed"),
        "read-only reopen appended a fact: {reopened_frames:?}"
    );
    assert_eq!(
        std::fs::read(&session_file).unwrap(),
        bytes,
        "read-only reopen/shutdown changed journal bytes"
    );
    drop(reopened);
    fixture.assert_no_provider_calls();
}

#[test]
fn native_stop_reconciles_an_active_process_prompt_before_exit() {
    let fixture = Fixture::new();
    let provider = fixture.provider.try_clone().unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let provider_task = thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let (mut stream, _) = loop {
            match provider.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "provider request did not arrive");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("provider accept failed: {error}"),
            }
        };
        accepted_tx.send(()).unwrap();
        thread::sleep(Duration::from_secs(1));
        let body = r#"data: {"choices":[{"delta":{"content":"late"},"finish_reason":null}]}

data: [DONE]

"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
    });

    let mut host = fixture.start(
        true,
        &[
            "--session-id",
            "native-durable-active-process-stop",
            "--no-tools",
        ],
    );
    let prompt_id =
        send_without_waiting(&mut host, "prompt", json!({"message":"hold this prompt"}));
    accepted_rx.recv_timeout(WAIT).unwrap();
    let stop_id = send_without_waiting(&mut host, "stop", json!({}));
    let frames = collect_and_wait_for_exit(&mut host, true);
    let prompt_response = frames
        .iter()
        .find(|frame| frame["type"] == "response" && frame["id"] == prompt_id)
        .expect("active prompt must receive one terminal response");
    assert_eq!(prompt_response["success"], true, "{prompt_response}");
    assert!(frames.iter().any(|frame| {
        frame["type"] == "agent_settled"
            && (frame["outcome"] == "aborted" || frame["outcome"] == "failed")
    }));
    let stop_response = frames
        .iter()
        .find(|frame| frame["type"] == "response" && frame["id"] == stop_id)
        .expect("native stop acknowledgement must flush");
    assert_eq!(stop_response["success"], true, "{stop_response}");
    provider_task.join().unwrap();
}

#[test]
fn native_eof_flushes_accepted_mutation_receipt_and_reopens_without_writes() {
    graceful_shutdown(false);
}
#[test]
fn native_stop_flushes_accepted_mutation_receipt_and_ack_then_reopens_without_writes() {
    graceful_shutdown(true);
}
