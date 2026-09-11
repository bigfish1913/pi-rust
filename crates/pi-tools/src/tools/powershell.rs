//! Optional Windows PowerShell tool, matching native Pi's `powershell` tool.
//! The command is passed through `-EncodedCommand` so quotes and newlines in
//! user input cannot be reinterpreted by the outer Bash transport.

use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::shell_output::{execute_shell_with_capture, ShellCaptureOptions};
use crate::tools::tool_context::ExecutionToolContext;

#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct PowerShellInput {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<f64>,
}

pub struct PowerShellTool {
    schema: Tool,
    env: Arc<dyn crate::env::ExecutionEnv>,
}

impl PowerShellTool {
    fn schema() -> Tool {
        Tool {
            name: "powershell".into(),
            description: "Execute a PowerShell command on Windows. Returns stdout and stderr."
                .into(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(schemars::schema_for!(PowerShellInput)).unwrap_or_default(),
            ),
            constrained_sampling: None,
        }
    }
}

pub fn create_powershell_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    Arc::new(PowerShellTool {
        schema: PowerShellTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "powershell"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if !cfg!(windows) {
            return Err(AgentError::Tool(
                "PowerShell tool is only available on Windows".into(),
            ));
        }
        let input: PowerShellInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("powershell input invalid: {e}")))?;
        let utf16: Vec<u8> = input
            .command
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let encoded = STANDARD.encode(utf16);
        let command =
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}");
        let capture = execute_shell_with_capture(
            &self.env,
            &command,
            ShellCaptureOptions {
                cwd: Some(self.env.cwd().to_path_buf()),
                env: None,
                inherit_env: true,
                timeout: input.timeout,
                cancel: Some(&signal),
                on_chunk: None,
                return_execution_errors: true,
            },
        )
        .await
        .map_err(|e| AgentError::Tool(e.to_string()))?;
        let mut output = capture.output;
        if capture.cancelled {
            return Err(AgentError::Tool("PowerShell command aborted".into()));
        }
        if let Some(error) = capture.execution_error {
            return Err(AgentError::Tool(error.to_string()));
        }
        if let Some(code) = capture.exit_code.filter(|code| *code != 0) {
            output.push_str(&format!("\n\nPowerShell exited with code {code}"));
            return Err(AgentError::Tool(output));
        }
        if output.trim().is_empty() {
            output = "(no output)".into();
        }
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            ..Default::default()
        })
    }
}
