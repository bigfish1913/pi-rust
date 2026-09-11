//! Mirrors `packages/agent/src/harness/tools/bash.ts` — the built-in `bash`
//! `AgentTool`. Drives `execute_shell_with_capture` with a 100ms-throttled
//! `on_update`, and converts ALL failures (non-zero exit, timeout, abort,
//! executionError) into `Err(AgentError)` — the agent loop encodes those into an
//! error `ToolResultMessage`.
//!
//! Throttle mirrors the TS `BASH_UPDATE_THROTTLE_MS`: at most one `on_update` per
//! 100ms window; a final flush after capture resolves. `on_chunk` invocations
//! from the capture layer mark the throttle dirty + schedule a flush.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::{ExecutionError, ExecutionErrorCode};
use crate::shell_output::{
    execute_shell_with_capture, ShellCaptureOptions, ShellCaptureProgress, ShellCaptureResult,
};
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{format_size, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Throttle window for bash `on_update` flushes. Mirrors `BASH_UPDATE_THROTTLE_MS`.
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// Input for the bash tool. Mirrors TS `BashToolInput`. `timeout` is in seconds.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct BashInput {
    /// Bash command to execute.
    pub command: String,
    /// Timeout in seconds (optional, no default timeout).
    #[serde(default)]
    pub timeout: Option<f64>,
}

/// Structured details for the bash tool. Mirrors TS `BashToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BashToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Options for `create_bash_tool`. Mirrors TS `BashToolOptions`.
#[derive(Debug, Clone, Default)]
pub struct BashToolOptions {
    /// Prepended to the command (separated by `\n`) before execution.
    pub command_prefix: Option<String>,
    /// Fallback timeout in seconds applied when the model doesn't pass one.
    /// pi has no default, but a model that forgets `timeout` can hang the run
    /// forever (reported as "卡住") — rpi injects this at the harness build
    /// site. A model-supplied timeout still wins.
    pub default_timeout: Option<f64>,
}

/// The built-in `bash` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view
/// suffices — bash does not touch the mutation queue).
pub struct BashTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
    options: BashToolOptions,
}

impl BashTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(BashInput);
        Tool {
            name: "bash".to_string(),
            description: format!(
                "Execute a bash command in the current working directory. Returns stdout and \
                 stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {kb}KB \
                 (whichever is hit first). If truncated, full output is saved to a temp file. \
                 Optionally provide a timeout in seconds.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(params).unwrap_or_default(),
            ),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        }
    }
}

/// Build the `bash` tool from a context. Mirrors TS `createBashTool`.
pub fn create_bash_tool(
    context: &ExecutionToolContext,
    options: Option<BashToolOptions>,
) -> Arc<dyn AgentTool> {
    Arc::new(BashTool {
        schema: BashTool::schema(),
        env: context.env().clone(),
        options: options.unwrap_or_default(),
    })
}

#[async_trait]
impl AgentTool for BashTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "bash"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: BashInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("bash input invalid: {e}")))?;
        validate_timeout(input.timeout)?;
        let command = match &self.options.command_prefix {
            Some(p) => format!("{p}\n{}", input.command),
            None => input.command,
        };
        let timeout = input.timeout.or(self.options.default_timeout);

        // Initial empty update (mirrors TS `onUpdate?.({ content: [], details: undefined })`).
        emit_update(&on_update, String::new(), None, None);

        // The throttle state. Shared between the on_chunk callback (which marks
        // dirty + schedules a flush) and the post-capture final flush.
        let throttle = Arc::new(Mutex::new(ThrottleState {
            dirty: false,
            last_flush: Instant::now(),
            latest_output: String::new(),
            latest_truncation: None,
            latest_full_path: None,
        }));

        let throttle_for_cb = throttle.clone();
        let on_update_for_cb = on_update.clone();
        let on_chunk: Box<dyn FnMut(&str, &dyn Fn() -> ShellCaptureProgress) + Send> = Box::new(
            move |_chunk, get_progress: &dyn Fn() -> ShellCaptureProgress| {
                let progress = get_progress();
                // Snapshot the fields we need from the progress before touching
                // the async mutex — the on_chunk callback runs on whatever thread
                // the env's stdout/stderr callback is invoked from, which may be
                // a runtime worker. Use try_lock so we never block (and never
                // panic on a held lock); a missed flush is fine — the final
                // post-capture flush always emits the terminal state.
                let new_output = progress.output.clone();
                let new_truncation = if progress.truncation.truncated {
                    Some(progress.truncation.clone())
                } else {
                    None
                };
                let new_full_path = progress
                    .full_output_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned());
                let mut st = match throttle_for_cb.try_lock() {
                    Ok(g) => g,
                    Err(_) => return, // busy — skip this update; final flush covers it
                };
                st.latest_output = new_output;
                st.latest_truncation = new_truncation;
                st.latest_full_path = new_full_path;
                st.dirty = true;
                let elapsed = st.last_flush.elapsed();
                if elapsed >= Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
                    flush_now(&mut st, &on_update_for_cb);
                }
            },
        );

        let cwd = self.env.cwd().to_path_buf();
        let env = self.env.clone();
        let capture_opts = ShellCaptureOptions {
            cwd: Some(cwd),
            env: None,
            inherit_env: true,
            timeout,
            cancel: Some(&signal),
            on_chunk: Some(on_chunk),
            return_execution_errors: true,
        };

        // execute_shell_with_capture wants `&Arc<dyn ExecutionEnv>`.
        let capture = execute_shell_with_capture(&env, &command, capture_opts)
            .await
            .map_err(exec_err_to_agent)?;

        // Final flush + emit the terminal output via on_update.
        {
            let mut st = throttle.lock().await;
            st.latest_output = capture.output.clone();
            st.latest_truncation = if capture.truncated {
                Some(capture.truncation.clone())
            } else {
                None
            };
            st.latest_full_path = capture
                .full_output_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            st.dirty = true;
            flush_now(&mut st, &on_update);
        }

        // Build the terminal output text + details, applying truncation notices.
        let (output_text, details) = build_bash_output(&capture);
        let details_value = serde_json::to_value(details).unwrap_or(serde_json::Value::Null);

        // Resolution: cancelled → aborted; timeout → timeout; other exec error;
        // non-zero exit → Tool error. All become Err(AgentError).
        let append_status = |status: String| -> String {
            if output_text.is_empty() {
                status
            } else {
                format!("{output_text}\n\n{status}")
            }
        };
        if capture.cancelled {
            return Err(AgentError::Tool(append_status("Command aborted".into())));
        }
        if let Some(ee) = &capture.execution_error {
            if ee.code == ExecutionErrorCode::Timeout {
                return Err(AgentError::Tool(append_status(format!(
                    "Command timed out after {timeout:?} seconds"
                ))));
            }
            return Err(AgentError::Tool(ee.to_string()));
        }
        if let Some(code) = capture.exit_code {
            if code != 0 {
                return Err(AgentError::Tool(append_status(format!(
                    "Command exited with code {code}"
                ))));
            }
        }
        let text = if output_text.is_empty() {
            "(no output)".to_string()
        } else {
            output_text
        };
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(text)],
            details: details_value,
            ..Default::default()
        })
    }
}

/// Throttle state, shared between the on_chunk callback and the final flush.
struct ThrottleState {
    dirty: bool,
    last_flush: Instant,
    latest_output: String,
    latest_truncation: Option<TruncationResult>,
    latest_full_path: Option<String>,
}

/// Flush a dirty throttle state by emitting an `on_update`. Mirrors
/// `emitOutputUpdate`.
fn flush_now(st: &mut ThrottleState, on_update: &Arc<dyn Fn(ToolResultPartial) + Send + Sync>) {
    if !st.dirty {
        return;
    }
    st.dirty = false;
    st.last_flush = Instant::now();
    emit_update(
        on_update,
        st.latest_output.clone(),
        st.latest_truncation.clone(),
        st.latest_full_path.clone(),
    );
}

/// Emit a single `on_update` payload built from the given capture snapshot.
/// Mirrors the TS `onUpdate({ content:[{text:progress.output}], details:{...} })`.
fn emit_update(
    on_update: &Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    output: String,
    truncation: Option<TruncationResult>,
    full_output_path: Option<String>,
) {
    let details = match (&truncation, &full_output_path) {
        (None, None) => serde_json::Value::Null,
        _ => {
            let details = BashToolDetails {
                truncation,
                full_output_path,
            };
            serde_json::to_value(details).unwrap_or(serde_json::Value::Null)
        }
    };
    on_update(AgentToolResult {
        content: vec![TextContentOrImage::text(output)],
        details,
        ..Default::default()
    });
}

/// Build the terminal `(output_text, details)` from a capture, appending the
/// truncation continuation notice (mirrors the TS block).
fn build_bash_output(capture: &ShellCaptureResult) -> (String, BashToolDetails) {
    let mut output_text = capture.output.clone();
    let details = if capture.truncated {
        let t = &capture.truncation;
        let start_line = t.total_lines.saturating_sub(t.output_lines) + 1;
        let end_line = t.total_lines;
        let path_str = capture
            .full_output_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        if t.last_line_partial {
            let last_line_size = format_size(capture.last_line_bytes);
            output_text.push_str(&format!(
                "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {}]",
                format_size(t.output_bytes),
                path_str.as_deref().unwrap_or("?")
            ));
        } else if matches!(
            t.truncated_by,
            Some(crate::truncate::TruncationLimit::Lines)
        ) {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {}]",
                t.total_lines,
                path_str.as_deref().unwrap_or("?")
            ));
        } else {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {}]",
                t.total_lines,
                format_size(DEFAULT_MAX_BYTES),
                path_str.as_deref().unwrap_or("?")
            ));
        }
        BashToolDetails {
            truncation: Some(capture.truncation.clone()),
            full_output_path: path_str,
        }
    } else {
        BashToolDetails {
            truncation: None,
            full_output_path: None,
        }
    };
    (output_text, details)
}

/// Validate a timeout (seconds). Mirrors `validateTimeout`.
fn validate_timeout(timeout: Option<f64>) -> Result<(), AgentError> {
    let Some(secs) = timeout else {
        return Ok(());
    };
    if !secs.is_finite() || secs <= 0.0 {
        return Err(AgentError::Tool(
            "Invalid timeout: must be a finite number of seconds".into(),
        ));
    }
    let max = 2_147_483_647.0 / 1000.0;
    if secs > max {
        return Err(AgentError::Tool(format!(
            "Invalid timeout: maximum is {max} seconds"
        )));
    }
    Ok(())
}

fn exec_err_to_agent(e: ExecutionError) -> AgentError {
    AgentError::Tool(e.to_string())
}
