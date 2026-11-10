//! Prototype native durable wire adapter, not the complete M1 host integration.
//! The harness owns admission, persistence and deduplication. No extension fallback,
//! inferred run, JSON-supplied actor, automatic conflict retry, or extra journal.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use rpi_harness::agent_harness::{AgentHarness, WeakAgentHarness};
#[cfg(test)]
use rpi_harness::durable::protocol::request_budget;
use rpi_harness::durable::protocol::{
    self, cancellable_read, cancellation, fits, flat_request as tool_request, response_budget,
    valid_identity, MAX_ID_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, SCOPE_FIELDS,
};
pub use rpi_harness::durable::protocol::{parse_json, Failure};
use rpi_harness::durable::{DurableActor, DurableService, MAX_EXECUTION_PAGE_RECORDS};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

mod context;
pub(crate) mod experimental_epoch;
mod human;
mod notice;
pub(crate) use context::hook as context_hook;
pub(crate) use human::interactive_command;
pub(crate) use notice::lifecycle_notice;

pub(crate) const COMMAND_DESCRIPTION: &str =
    "Native durable workflow / explicit run view or guarded JSON mutation (v1 prototype)";

pub(crate) const MAX_IN_FLIGHT: usize = 8;

/// Pure capability metadata; enabling and routing are owned by the CLI host.
pub fn capabilities(enabled: bool) -> Value {
    json!({
        "version": 1,
        "profile": "native-durable-v1",
        "enabled": enabled,
        "commands": ["get_durable_state", "durable_mutate", "get_durable_request", "get_durable_runs"],
        "scope": "current_lane",
        "execution": "explicit_run",
        "executionPages": {
            "version": 1,
            "command": "get_durable_runs",
            "cursor": "scope_bound_exclusive",
            "maxRecords": MAX_EXECUTION_PAGE_RECORDS,
            "automatic": false
        },
        "scheduler": false,
        "modelContext": {
            "version": 1,
            "delivery": "per_request_ephemeral_user",
            "oversize": "explicit_summary",
            "failure": "unavailable_observation"
        },
        "prototype": true,
        "limits": {
            "inFlight": MAX_IN_FLIGHT,
            "requestBytes": MAX_REQUEST_BYTES,
            "responseBytes": MAX_RESPONSE_BYTES,
            "contextBytes": context::MAX_CONTEXT_BYTES,
            "idBytes": MAX_ID_BYTES,
            "receipts": 64,
            "recordBytes": 1024 * 1024
        }
    })
}

fn actor() -> Result<DurableActor, Failure> {
    use rpi_agent::tool_execution_scope::{execution_context, is_child_execution};
    if !is_child_execution() {
        return DurableActor::owner("native-cli").map_err(Into::into);
    }
    let context = execution_context().ok_or_else(|| {
        Failure::new(
            "permission_denied",
            "Trusted child durable identity is missing",
        )
    })?;
    let id = context
        .get("nativeDurableActor")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Failure::new(
                "permission_denied",
                "Trusted child durable identity is missing",
            )
        })?;
    DurableActor::child(id).map_err(|_| {
        Failure::new(
            "permission_denied",
            "Trusted child durable identity is invalid",
        )
    })
}
fn service(harness: &AgentHarness) -> Result<DurableService, Failure> {
    Ok(DurableService::bind(harness, actor()?))
}
/// Host cleanup after joining accepted RPC producers, not a wire action or an
/// outcome proof. Keep the owner alive while the core gate/backend barrier runs.
pub(crate) async fn drain(harness: &AgentHarness) -> Result<(), Failure> {
    service(harness)?.drain().await.map_err(Into::into)
}
/// Preserve validation/disabled precedence before trusted actor binding.
pub async fn rpc(harness: &AgentHarness, request: &Value, enabled: bool) -> Result<Value, Failure> {
    let request = protocol::decode_rpc(request, enabled)?;
    protocol::execute_rpc(&service(harness)?, request).await
}

fn agent_failure(failure: Failure) -> AgentError {
    AgentError::Tool(serde_json::to_string(&failure).expect("failure serializes"))
}

pub fn rpc_response(id: Value, kind: &str, result: Result<Value, Failure>) -> Value {
    // Do not echo an oversize ID even on request validation failure.
    let id = if fits(&id, MAX_ID_BYTES) {
        id
    } else {
        Value::Null
    };
    match result {
        Ok(value) => crate::rpc::protocol::response(Some(id), kind, Ok(Some(value))),
        Err(error) => {
            let mut response =
                crate::rpc::protocol::response(Some(id), kind, Err(error.message.clone()));
            response["errorDetails"] = json!(error);
            response
        }
    }
}

pub async fn command(harness: &AgentHarness, args: &str) -> Result<Value, Failure> {
    // Count raw whitespace too, just as the RPC raw-frame parser does.
    if args.len() > MAX_REQUEST_BYTES {
        return Err(Failure::new(
            "limit_exceeded",
            "Durable request exceeds 64 KiB",
        ));
    }
    let params = if args.trim().is_empty() {
        json!({})
    } else {
        parse_json(args)?
    };
    tool_request(&service(harness)?, params, None).await
}

// Account for JSON escaping and the text/details duplication in the actual
// agent result, not only the unwrapped durable value. No truncation after commit.
fn tool_result(value: Value) -> Result<AgentToolResult, Failure> {
    let text = serde_json::to_string(&value).expect("JSON value serializes");
    let request_id = value
        .get("outcome")
        .and_then(|outcome| outcome.get("receipt"))
        .and_then(|receipt| receipt.get("requestId"))
        .and_then(Value::as_str);
    response_budget(
        json!({"content":[{"type":"text","text":text}],"details":value}),
        request_id,
    )?;
    let mut result = AgentToolResult::text(text);
    result.details = value;
    Ok(result)
}

pub(crate) struct NativeDurableTool {
    binding: Arc<OnceLock<WeakAgentHarness>>,
    schema: Tool,
}
impl NativeDurableTool {
    pub fn new(binding: Arc<OnceLock<WeakAgentHarness>>) -> Self {
        Self {
            binding,
            schema: Tool {
                name: "durable".into(),
                description: "Prototype native session/lane durable workflow and manual queue. No scheduler or delegation. Default status; status with runId observes that explicit run. Mutations require an explicit requestId (never the provider call ID); expected may be omitted to sample the current scope once. Retry only identical guarded requests; status with requestId queries its receipt after uncertain commits (do not also pass runId). Child actors cannot start/reset/recover/forget_run. Core limits: 64 receipts, 1 MiB record; wire limits: 64 KiB request, 480 KiB response.".into(),
                parameters: Schema::new(json!({
                    "type":"object", "additionalProperties":false,
                    "properties":{
                        "action":{"type":"string", "enum":["status","start","checkpoint","pause","resume","complete","fail","reset","recover","forget_run","enqueue","queue","claim","finish","cancel"]},
                        "requestId":{"type":"string", "description":"Mutation identity, or receipt target for status (without runId).", "minLength":1, "maxLength":128, "pattern":"^[A-Za-z0-9._-]+$"},
                        "expected":{"type":"object", "description":"Complete scope from status; required for identical guarded retries."},
                        "runId":{"type":"string", "description":"Explicit run for status/recover/forget_run only."},
                        "name":{"type":"string"},
                        "steps":{"type":"array", "items":{"type":"object", "additionalProperties":false, "required":["id","title"], "properties":{"id":{"type":"string"},"title":{"type":"string"}}}},
                        "step":{"type":"string"},
                        "status":{"type":"string", "enum":["pending","in_progress","completed","failed"]},
                        "note":{"type":["string","null"]},
                        "reason":{"type":["string","null"]},
                        "text":{"type":"string"},
                        "item":{"type":"string"}
                    }
                })),
                constrained_sampling: None,
            },
        }
    }
}
#[async_trait]
impl AgentTool for NativeDurableTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "durable"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        cancellation(Some(&signal)).map_err(agent_failure)?;
        let harness = self
            .binding
            .get()
            .and_then(WeakAgentHarness::upgrade)
            .ok_or_else(|| {
                agent_failure(Failure::new(
                    "closed",
                    "Durable owner is not bound or is closed",
                ))
            })?;
        let service = service(&harness).map_err(agent_failure)?;
        let value = tool_request(&service, params, Some(&signal))
            .await
            .map_err(agent_failure)?;
        tool_result(value).map_err(agent_failure)
    }
}

#[cfg(test)]
#[path = "native_durable/tests.rs"]
mod tests;
