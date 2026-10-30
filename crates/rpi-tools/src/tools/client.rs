//! Text-buffer and shell tools supplied by a client, without local filesystem access.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ShellOutput;
use crate::truncate::{truncate_tail, TruncationOptions, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Client-owned text buffers and shell execution. Paths are passed through unchanged.
///
/// Implementations must honor cancellation and the supplied shell timeout, and
/// report failures as strings. No filesystem emulation or protocol types are required.
#[async_trait]
pub trait ClientToolBackend: Send + Sync {
    async fn read_text(&self, path: &str, cancel: CancellationToken) -> Result<String, String>;
    async fn write_text(
        &self,
        path: &str,
        content: &str,
        cancel: CancellationToken,
    ) -> Result<(), String>;
    async fn exec(
        &self,
        command: &str,
        timeout: Option<f64>,
        cancel: CancellationToken,
    ) -> Result<ShellOutput, String>;
}

#[derive(Deserialize, JsonSchema)]
struct ReadInput {
    /// Client text-buffer path (passed through unchanged).
    path: String,
    /// First line to read, 1-indexed; defaults to 1.
    offset: Option<usize>,
    /// Positive line limit; defaults to 2000 and is capped at 2000.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteInput {
    /// Client text-buffer path (passed through unchanged).
    path: String,
    /// Complete replacement text.
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    command: String,
    /// Timeout in seconds, greater than zero and at most 600; defaults to 120.
    timeout: Option<f64>,
}

#[derive(Clone, Copy)]
enum Kind {
    Read,
    Write,
    Bash,
}

struct ClientTool {
    backend: Arc<dyn ClientToolBackend>,
    kind: Kind,
    schema: Tool,
}

fn create<T: JsonSchema>(
    backend: Arc<dyn ClientToolBackend>,
    kind: Kind,
    name: &str,
    description: &str,
) -> Arc<dyn AgentTool> {
    Arc::new(ClientTool {
        backend,
        kind,
        schema: Tool {
            name: name.into(),
            description: description.into(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default(),
            ),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        },
    })
}

/// Create the model-facing `read` tool for client text buffers (not images).
pub fn create_client_read_tool(backend: Arc<dyn ClientToolBackend>) -> Arc<dyn AgentTool> {
    create::<ReadInput>(backend, Kind::Read, "read", "Read a client text buffer, not the local filesystem. Text only; no images. Optional 1-indexed offset (default 1) and positive limit (default/max 2000 lines). Output is bounded to 50KB with UTF-8-safe truncation.")
}

/// Create the model-facing `write` tool for client text buffers.
pub fn create_client_write_tool(backend: Arc<dyn ClientToolBackend>) -> Arc<dyn AgentTool> {
    create::<WriteInput>(backend, Kind::Write, "write", "Write replacement content to a client text buffer, not the local filesystem. The client performs the write and reports any failure.")
}

/// Create the model-facing `bash` tool for the client's shell.
pub fn create_client_bash_tool(backend: Arc<dyn ClientToolBackend>) -> Arc<dyn AgentTool> {
    create::<BashInput>(backend, Kind::Bash, "bash", "Execute a command in the client shell, not the local shell. Returns stdout and stderr, bounded to the last 2000 lines or 50KB. Timeout is in seconds (default 120, positive, maximum 600). Nonzero exit status is an error.")
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), AgentError> {
    if cancel.is_cancelled() {
        Err(AgentError::Abort)
    } else {
        Ok(())
    }
}

fn parse<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, AgentError> {
    serde_json::from_value(params).map_err(|e| AgentError::Validation(e.to_string()))
}

/// Preserve line endings and return a partial long line rather than dropping it.
fn read_slice(text: &str, offset: usize, limit: usize) -> (String, bool) {
    let mut lines = text.split_inclusive('\n').skip(offset - 1);
    let mut output = String::new();
    for _ in 0..limit {
        let Some(line) = lines.next() else {
            return (output, false);
        };
        let remaining = DEFAULT_MAX_BYTES - output.len();
        if line.len() > remaining {
            let mut end = remaining;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            output.push_str(&line[..end]);
            return (output, true);
        }
        output.push_str(line);
    }
    (output, lines.next().is_some())
}

#[async_trait]
impl AgentTool for ClientTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        &self.schema.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        check_cancel(&signal)?;
        match self.kind {
            Kind::Read => {
                let input: ReadInput = parse(params)?;
                let offset = input.offset.unwrap_or(1);
                let limit = input.limit.unwrap_or(DEFAULT_MAX_LINES);
                if offset == 0 || limit == 0 {
                    return Err(AgentError::Validation(
                        "offset and limit must be positive".into(),
                    ));
                }
                let text = self
                    .backend
                    .read_text(&input.path, signal.clone())
                    .await
                    .map_err(AgentError::Tool)?;
                check_cancel(&signal)?;
                let (text, truncated) = read_slice(&text, offset, limit.min(DEFAULT_MAX_LINES));
                let mut result = AgentToolResult::text(text);
                result.details = serde_json::json!({"truncated": truncated, "offset": offset});
                Ok(result)
            }
            Kind::Write => {
                let input: WriteInput = parse(params)?;
                self.backend
                    .write_text(&input.path, &input.content, signal.clone())
                    .await
                    .map_err(AgentError::Tool)?;
                check_cancel(&signal)?;
                Ok(AgentToolResult::text(format!(
                    "Successfully wrote to {}",
                    input.path
                )))
            }
            Kind::Bash => {
                let input: BashInput = parse(params)?;
                let timeout = input.timeout.unwrap_or(120.0);
                if !timeout.is_finite() || timeout <= 0.0 || timeout > 600.0 {
                    return Err(AgentError::Validation(
                        "timeout must be finite, positive, and at most 600 seconds".into(),
                    ));
                }
                let output = self
                    .backend
                    .exec(&input.command, Some(timeout), signal.clone())
                    .await
                    .map_err(AgentError::Tool)?;
                check_cancel(&signal)?;
                let combined = match (output.stdout.is_empty(), output.stderr.is_empty()) {
                    (true, _) => output.stderr,
                    (_, true) => output.stdout,
                    _ => format!("{}\n{}", output.stdout, output.stderr),
                };
                // Reserve room for the status message so error output is also bounded.
                let status = format!("Command exited with code {}", output.exit_code);
                let failed = output.exit_code != 0;
                let truncation = truncate_tail(
                    &combined,
                    TruncationOptions {
                        max_lines: Some(DEFAULT_MAX_LINES - usize::from(failed)),
                        max_bytes: Some(
                            DEFAULT_MAX_BYTES - if failed { status.len() + 1 } else { 0 },
                        ),
                    },
                );
                if failed {
                    return Err(AgentError::Tool(format!(
                        "{}\n{status}",
                        truncation.content
                    )));
                }
                let mut result = AgentToolResult::text(truncation.content);
                result.details = serde_json::json!({"exit_code": output.exit_code, "truncated": truncation.truncated});
                Ok(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::types::TextContentOrImage;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeClient {
        buffers: Mutex<HashMap<String, String>>,
        calls: Mutex<Vec<String>>,
        shell: Mutex<Option<ShellOutput>>,
        write_error: Option<String>,
    }

    #[async_trait]
    impl ClientToolBackend for FakeClient {
        async fn read_text(&self, path: &str, cancel: CancellationToken) -> Result<String, String> {
            assert!(!cancel.is_cancelled());
            self.calls.lock().unwrap().push(format!("read:{path}"));
            self.buffers
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or("client buffer missing".into())
        }

        async fn write_text(
            &self,
            path: &str,
            content: &str,
            cancel: CancellationToken,
        ) -> Result<(), String> {
            assert!(!cancel.is_cancelled());
            self.calls.lock().unwrap().push(format!("write:{path}"));
            if let Some(error) = &self.write_error {
                return Err(error.clone());
            }
            self.buffers
                .lock()
                .unwrap()
                .insert(path.into(), content.into());
            Ok(())
        }

        async fn exec(
            &self,
            command: &str,
            timeout: Option<f64>,
            cancel: CancellationToken,
        ) -> Result<ShellOutput, String> {
            assert!(!cancel.is_cancelled());
            self.calls
                .lock()
                .unwrap()
                .push(format!("exec:{command}:{timeout:?}"));
            self.shell
                .lock()
                .unwrap()
                .clone()
                .ok_or("client shell unavailable".into())
        }
    }

    async fn run(
        tool: &Arc<dyn AgentTool>,
        params: serde_json::Value,
    ) -> Result<AgentToolResult, AgentError> {
        tool.execute("test", params, CancellationToken::new(), Arc::new(|_| {}))
            .await
    }

    fn text(result: &AgentToolResult) -> &str {
        match &result.content[..] {
            [TextContentOrImage::Text(text)] => &text.text,
            _ => panic!("expected a single text block"),
        }
    }

    #[tokio::test]
    async fn model_facing_read_uses_client_buffer_and_write_changes_it() {
        let backend = Arc::new(FakeClient::default());
        // This is a real repository path, but only the client buffer is consulted.
        backend
            .buffers
            .lock()
            .unwrap()
            .insert("Cargo.toml".into(), "unsaved client text".into());
        let read = create_client_read_tool(backend.clone());
        let write = create_client_write_tool(backend.clone());
        assert_eq!(read.schema().name, "read");
        assert_eq!(write.schema().name, "write");
        assert_eq!(
            text(&run(&read, json!({"path":"Cargo.toml"})).await.unwrap()),
            "unsaved client text"
        );
        run(
            &write,
            json!({"path":"Cargo.toml", "content":"actual replacement 🦀"}),
        )
        .await
        .unwrap();
        assert_eq!(
            text(&run(&read, json!({"path":"Cargo.toml"})).await.unwrap()),
            "actual replacement 🦀"
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            ["read:Cargo.toml", "write:Cargo.toml", "read:Cargo.toml"]
        );
    }

    #[tokio::test]
    async fn read_slicing_defaults_and_utf8_bounds() {
        let backend = Arc::new(FakeClient::default());
        backend
            .buffers
            .lock()
            .unwrap()
            .insert("buffer".into(), "first\r\n第二\nthird".into());
        let read = create_client_read_tool(backend.clone());
        assert_eq!(
            text(
                &run(&read, json!({"path":"buffer", "offset":2, "limit":1}))
                    .await
                    .unwrap()
            ),
            "第二\n"
        );
        assert_eq!(
            text(
                &run(&read, json!({"path":"buffer", "offset":99}))
                    .await
                    .unwrap()
            ),
            ""
        );
        backend
            .buffers
            .lock()
            .unwrap()
            .insert("buffer".into(), "🦀".repeat(DEFAULT_MAX_BYTES));
        let result = run(&read, json!({"path":"buffer"})).await.unwrap();
        assert_eq!(text(&result).len(), DEFAULT_MAX_BYTES);
        assert!(text(&result).chars().all(|c| c == '🦀'));
        assert_eq!(result.details["truncated"], true);
        backend
            .buffers
            .lock()
            .unwrap()
            .insert("buffer".into(), "line\n".repeat(3000));
        for params in [
            json!({"path":"buffer"}),
            json!({"path":"buffer", "limit":9000}),
        ] {
            let result = run(&read, params).await.unwrap();
            assert_eq!(text(&result).lines().count(), DEFAULT_MAX_LINES);
            assert_eq!(result.details["truncated"], true);
        }
        // A byte cap falling inside a multi-byte character must round down.
        let (bounded, truncated) =
            read_slice(&format!("a{}", "🦀".repeat(DEFAULT_MAX_BYTES)), 1, 1);
        assert_eq!(bounded.len(), DEFAULT_MAX_BYTES - 3);
        assert!(truncated);
    }

    #[tokio::test]
    async fn invalid_arguments_never_call_backend() {
        let backend = Arc::new(FakeClient::default());
        let read = create_client_read_tool(backend.clone());
        for params in [
            json!({}),
            json!({"path":3}),
            json!({"path":"x", "offset":0}),
            json!({"path":"x", "offset":-1}),
            json!({"path":"x", "limit":0}),
            json!({"path":"x", "limit":1.5}),
        ] {
            assert!(matches!(
                run(&read, params).await,
                Err(AgentError::Validation(_))
            ));
        }
        let bash = create_client_bash_tool(backend.clone());
        for timeout in [0.0, -1.0, 600.1] {
            assert!(matches!(
                run(&bash, json!({"command":"x", "timeout":timeout})).await,
                Err(AgentError::Validation(_))
            ));
        }
        let write = create_client_write_tool(backend.clone());
        assert!(matches!(
            run(&write, json!({"path":"x"})).await,
            Err(AgentError::Validation(_))
        ));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shell_output_exit_status_timeout_and_bounds() {
        let backend = Arc::new(FakeClient::default());
        *backend.shell.lock().unwrap() = Some(ShellOutput {
            stdout: "client stdout".into(),
            stderr: "client stderr".into(),
            exit_code: 0,
        });
        let bash = create_client_bash_tool(backend.clone());
        assert_eq!(bash.schema().name, "bash");
        let result = run(&bash, json!({"command":"client-command"}))
            .await
            .unwrap();
        assert_eq!(text(&result), "client stdout\nclient stderr");
        assert_eq!(result.details["exit_code"], 0);
        assert_eq!(
            backend.calls.lock().unwrap()[0],
            "exec:client-command:Some(120.0)"
        );
        *backend.shell.lock().unwrap() = Some(ShellOutput {
            stdout: "🦀\n".repeat(30000),
            stderr: "failure".into(),
            exit_code: 7,
        });
        let error = run(&bash, json!({"command":"fail", "timeout":600}))
            .await
            .unwrap_err();
        let AgentError::Tool(message) = error else {
            panic!("expected tool error")
        };
        assert!(message.contains("failure"));
        assert!(message.ends_with("Command exited with code 7"));
        assert!(message.len() <= DEFAULT_MAX_BYTES);
        assert!(message.lines().count() <= DEFAULT_MAX_LINES);
        assert_eq!(backend.calls.lock().unwrap()[1], "exec:fail:Some(600.0)");
    }

    #[tokio::test]
    async fn backend_errors_are_tool_errors() {
        let backend = Arc::new(FakeClient::default());
        let read = create_client_read_tool(backend.clone());
        assert!(
            matches!(run(&read, json!({"path":"missing"})).await, Err(AgentError::Tool(message)) if message == "client buffer missing")
        );
        let bash = create_client_bash_tool(backend);
        assert!(
            matches!(run(&bash, json!({"command":"missing"})).await, Err(AgentError::Tool(message)) if message == "client shell unavailable")
        );
        let failing_write = Arc::new(FakeClient {
            write_error: Some("client write denied".into()),
            ..Default::default()
        });
        let write = create_client_write_tool(failing_write.clone());
        assert!(
            matches!(run(&write, json!({"path":"x", "content":"y"})).await, Err(AgentError::Tool(message)) if message == "client write denied")
        );
        assert!(failing_write.buffers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancellation_prevents_all_backend_calls() {
        let backend = Arc::new(FakeClient::default());
        for (tool, params) in [
            (
                create_client_read_tool(backend.clone()),
                json!({"path":"x"}),
            ),
            (
                create_client_write_tool(backend.clone()),
                json!({"path":"x", "content":"y"}),
            ),
            (
                create_client_bash_tool(backend.clone()),
                json!({"command":"x"}),
            ),
        ] {
            let cancel = CancellationToken::new();
            cancel.cancel();
            assert!(matches!(
                tool.execute("test", params, cancel, Arc::new(|_| {})).await,
                Err(AgentError::Abort)
            ));
        }
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
