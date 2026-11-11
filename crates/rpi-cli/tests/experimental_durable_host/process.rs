//! #50-owned process guard derived from the #49 pure helper; old helper stays immutable.
use super::fixture::mutation;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const WAIT: Duration = Duration::from_secs(25);
const STDERR_LIMIT: usize = 16 * 1024;
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

impl Host {
    // Close ingress, consume stdout to EOF, then observe actual successful exit.
    // Drop still kills/reaps the owned child on any timeout/assertion unwind.
    pub fn finish(mut self, stop: bool) {
        if stop {
            assert_eq!(self.data("stop", json!({}))["stopped"], true);
        }
        let (success, stderr) = self.exit(false);
        assert!(success, "normal CLI exit failed: {stderr}");
    }
    pub fn reject(mut self) -> String {
        let (success, stderr) = self.exit(true);
        assert!(!success, "rejected startup exited successfully");
        assert!(!stderr.trim().is_empty(), "missing rejection diagnostic");
        stderr
    }
    fn exit(&mut self, rejecting: bool) -> (bool, String) {
        self.input.take();
        let deadline = Instant::now() + WAIT;
        loop {
            match self
                .output
                .as_ref()
                .unwrap()
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => {
                    let value: Value = serde_json::from_str(&line).expect("framed JSON stdout");
                    if rejecting {
                        assert_ne!(value["type"], "ready", "rejected startup reached READY");
                    }
                    assert!(
                        !matches!(
                            value["type"].as_str(),
                            Some("agent_start" | "tool_execution_start")
                        ),
                        "unexpected execution: {value}"
                    );
                    assert!(Instant::now() < deadline, "process failed to reach EOF");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(error) => panic!(
                    "process EOF timeout: {error}; {}",
                    String::from_utf8_lossy(&self.stderr.lock().unwrap())
                ),
            }
        }
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "process failed to exit");
            thread::sleep(Duration::from_millis(10));
        };
        self.output.take();
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        (
            status.success(),
            String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned(),
        )
    }
}
