//! Run-local persistence barrier at the actual tool execution boundary.
//! UI ToolExecutionStart precedes preparation/authorization and cannot provide
//! either durable execution intent or the effective arguments used by recovery.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use tokio_util::sync::CancellationToken;

use crate::{agent_harness::AgentHarness, settle::SettleState};

pub(crate) struct ToolExecutionBarrier {
    pub(crate) tool: Arc<dyn AgentTool>,
    pub(crate) harness: AgentHarness,
    pub(crate) settle: Arc<SettleState>,
    pub(crate) write_error: Arc<Mutex<Option<String>>>,
    pub(crate) signal: CancellationToken,
}

#[async_trait]
impl AgentTool for ToolExecutionBarrier {
    fn schema(&self) -> &Tool {
        self.tool.schema()
    }
    fn label(&self) -> &str {
        self.tool.label()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.tool.execution_mode()
    }
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.tool.prepare_arguments(args)
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() || self.signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        if let Err(error) = self
            .settle
            .start_tool(
                &self.harness,
                tool_call_id,
                &self.tool.schema().name,
                &params,
            )
            .await
        {
            let message = error.to_string();
            self.write_error
                .lock()
                .unwrap()
                .get_or_insert(message.clone());
            // Cancel the RUN, not only this call's child token. The existing
            // batch machinery still joins already executing parallel peers.
            self.signal.cancel();
            tracing::warn!(%error, tool_call_id, "refusing execution without a durable tool checkpoint");
            return Err(AgentError::tool(message));
        }
        // Cancellation may arrive during the storage await. An acknowledged
        // marker is conservative intent, not proof execute was entered.
        if signal.is_cancelled() || self.signal.is_cancelled() {
            return Err(AgentError::Abort);
        }
        self.tool
            .execute(tool_call_id, params, signal, on_update)
            .await
    }
}
