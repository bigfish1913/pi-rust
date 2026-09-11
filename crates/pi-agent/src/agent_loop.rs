//! Mirrors `packages/agent/src/agent-loop.ts` — the provider-agnostic agent loop.
//!
//! Two public free functions, [`run_agent_loop`] (new prompt) and
//! [`run_agent_loop_continue`] (no new prompt), drive the outer follow-up loop
//! and inner steering+tool loop. The only LLM boundary is [`StreamFn`]
//! (sync return → `AssistantMessageEventStream`); everything else talks in
//! [`AgentMessage`].
//!
//! Critical invariants enforced here (plan §5):
//! - **Tool-execution ordering**: in a parallel batch, `ToolExecutionEnd` fires
//!   in *completion* order; tool-result `MessageStart`/`MessageEnd` fire later
//!   in *source/ordinal* order. Implemented by collecting completion signals
//!   into a queue, then walking the finalized vec by index for the result
//!   messages.
//! - **Truncate-fail**: `stop_reason == Length` → every tool call in the
//!   message fails-in-place with `is_error:true` and is *not* executed.
//! - **Late-update suppression**: `on_update` after `execute` resolves is a
//!   no-op via an `accepting_updates: Arc<AtomicBool>` flipped false on settle.
//!
//! The loop is fully testable without [`crate::Agent`] — `run_agent_loop` takes
//! plain `AgentContext`/`AgentLoopConfig` + an `AgentEmitter`.

use crate::agent_tool::AgentTool;
use crate::error::AgentError;
use crate::events::{AgentEmitter, AgentEvent};
use crate::hooks::AgentLoopConfig;
use crate::message::AgentMessage;
use crate::stream_fn::StreamFn;
use crate::types::{
    AfterToolCallContext, AgentContext, AgentToolResult, BeforeToolCallContext, ToolExecutionMode,
};

use rpi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, StopReason, ToolCall, ToolCallType,
    ToolResultMessage, ToolResultRole,
};
use rpi_ai::validate_tool_arguments;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The messages a single `run_agent_loop` invocation added (prompt + assistant
/// + tool results + injected steering/follow-ups). Returned to the caller.
pub type NewMessages = Vec<AgentMessage>;

/// Why a run ended. `Completed` is the normal exit; `Aborted`/`Failed` are set
/// when the terminal assistant message carried `Aborted`/`Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    Completed,
    Aborted,
    Failed,
}

impl LoopOutcome {
    fn from_stop(stop: StopReason) -> Self {
        match stop {
            StopReason::Aborted => LoopOutcome::Aborted,
            StopReason::Error => LoopOutcome::Failed,
            _ => LoopOutcome::Completed,
        }
    }
}

// ----------------------------------------------------------------------------
// Public entry points
// ----------------------------------------------------------------------------

/// Run an agent loop starting from a new prompt. Mirrors TS `runAgentLoop`.
///
/// Emits `agent_start`, `turn_start`, then `message_start`/`message_end` for
/// each prompt, then drives [`run_loop`]. Returns the messages produced.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt.clone(),
        messages: {
            let mut v = context.messages.clone();
            v.extend(prompts);
            v
        },
        tools: context.tools.clone(),
    };

    emit_event(&emit, AgentEvent::AgentStart).await;
    emit_event(&emit, AgentEvent::TurnStart).await;
    for prompt in &new_messages {
        emit_event(
            &emit,
            AgentEvent::MessageStart {
                message: prompt.clone(),
            },
        )
        .await;
        emit_event(
            &emit,
            AgentEvent::MessageEnd {
                message: prompt.clone(),
            },
        )
        .await;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        &config,
        &emit,
        &stream_fn,
    )
    .await?;
    Ok(new_messages)
}

/// Continue an agent loop from the existing context (no new prompt). Mirrors
/// TS `runAgentLoopContinue`. Errors if the context is empty or its last
/// message is an assistant message (the provider would reject that).
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError> {
    if context.messages.is_empty() {
        return Err(AgentError::State(
            "cannot continue: no messages in context".into(),
        ));
    }
    if context.messages.last().unwrap().is_assistant() {
        return Err(AgentError::State(
            "cannot continue from message role: assistant".into(),
        ));
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit_event(&emit, AgentEvent::AgentStart).await;
    emit_event(&emit, AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        &config,
        &emit,
        &stream_fn,
    )
    .await?;
    Ok(new_messages)
}

// ----------------------------------------------------------------------------
// Main loop — mirrors TS runLoop
// ----------------------------------------------------------------------------

/// A finalized tool call: the raw call, the merged result, and the error flag.
#[derive(Clone)]
struct FinalizedToolCall {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

/// A batch of executed tool calls: the per-call `ToolResultMessage`s (in source
/// order) and the early-terminate hint.
struct ExecutedToolBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
    stream_fn: &StreamFn,
) -> Result<LoopOutcome, AgentError> {
    let mut first_turn = true;
    // Check for steering messages at start (user may have typed while waiting).
    let mut pending_messages = drain_steering(config).await;

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would otherwise stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn {
                emit_event(emit, AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // Inject pending (steering/follow-up) messages before the next LLM call.
            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit_event(
                        emit,
                        AgentEvent::MessageStart {
                            message: message.clone(),
                        },
                    )
                    .await;
                    emit_event(
                        emit,
                        AgentEvent::MessageEnd {
                            message: message.clone(),
                        },
                    )
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // Stream the assistant response.
            let message =
                stream_assistant_response(current_context, config, emit, stream_fn).await?;
            new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let am = AgentMessage::Assistant(Box::new(message.clone()));
                emit_event(
                    emit,
                    AgentEvent::TurnEnd {
                        message: am,
                        tool_results: Vec::new(),
                    },
                )
                .await;
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::from_stop(message.stop_reason));
            }

            // Collect tool calls (in content order = source/ordinal order).
            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = if matches!(message.stop_reason, StopReason::Length) {
                    // Truncate-fail invariant: Length → fail ALL without executing.
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await?
                } else {
                    execute_tool_calls(current_context, &message, &tool_calls, config, emit).await?
                };
                tool_results.extend(batch.messages);
                has_more_tool_calls = !batch.terminate;

                for result in &tool_results {
                    let am = AgentMessage::ToolResult(Box::new(result.clone()));
                    current_context.messages.push(am.clone());
                    new_messages.push(am);
                }
            }

            if !tool_results.is_empty() {
                if let Some(upd) = after_tool_results(
                    config,
                    &message,
                    &tool_results,
                    current_context,
                    new_messages,
                )
                .await
                {
                    if let Some(ctx) = upd.context {
                        *current_context = ctx;
                    }
                }
            }

            let am = AgentMessage::Assistant(Box::new(message.clone()));
            emit_event(
                emit,
                AgentEvent::TurnEnd {
                    message: am,
                    tool_results: tool_results.clone(),
                },
            )
            .await;

            // prepareNextTurn: replace context if provided. (Model/thinking swaps
            // are owned by Agent; run_loop borrows config immutably for hook
            // stability. M2 tests exercise context replacement only.)
            if let Some(upd) = prepare_next_turn(
                config,
                &message,
                &tool_results,
                current_context,
                new_messages,
            )
            .await
            {
                if let Some(ctx) = upd.context {
                    *current_context = ctx;
                }
            }

            if should_stop_after_turn(
                config,
                &message,
                &tool_results,
                current_context,
                new_messages,
            )
            .await
            {
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::Completed);
            }

            if config.signal.is_cancelled() {
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::Aborted);
            }

            pending_messages = drain_steering(config).await;
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_ups = drain_follow_up(config).await;
        if !follow_ups.is_empty() {
            pending_messages = follow_ups;
            continue;
        }
        break;
    }

    emit_event(
        emit,
        AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        },
    )
    .await;
    Ok(LoopOutcome::Completed)
}

// ----------------------------------------------------------------------------
// streamAssistantResponse
// ----------------------------------------------------------------------------

/// Stream one assistant response, folding protocol events into the partial
/// message and emitting agent `Message*` events. Mirrors TS
/// `streamAssistantResponse`.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
    stream_fn: &StreamFn,
) -> Result<AssistantMessage, AgentError> {
    // Apply optional transform_context (AgentMessage[] → AgentMessage[]).
    let messages = if let Some(transform) = &config.transform_context {
        transform(context.messages.clone(), config.signal.clone()).await
    } else {
        context.messages.clone()
    };

    // convert_to_llm (AgentMessage[] → Message[]).
    let llm_messages = (config.convert_to_llm)(messages).await;

    let llm_context = rpi_ai::types::Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools: context.tools.iter().map(|t| t.schema().clone()).collect(),
    };

    // Resolve API key: getApiKey(provider) ?? config.api_key.
    let resolved_api_key = if let Some(get_key) = &config.get_api_key {
        get_key(&config.model.provider)
            .await
            .or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone()
    };

    let opts = config.to_stream_options(resolved_api_key);
    let mut response = stream_fn(&config.model, &llm_context, &opts);

    let mut added_partial = false;

    while let Some(event) = response.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                let am = AgentMessage::Assistant(Box::new((**partial).clone()));
                context.messages.push(am.clone());
                added_partial = true;
                emit_event(emit, AgentEvent::MessageStart { message: am }).await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                if added_partial {
                    let am = AgentMessage::Assistant(Box::new((**partial).clone()));
                    if let Some(last) = context.messages.last_mut() {
                        *last = am.clone();
                    }
                    emit_event(
                        emit,
                        AgentEvent::MessageUpdate {
                            message: am,
                            assistant_message_event: event.clone(),
                        },
                    )
                    .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message = response.result().await.map_err(|_| {
                    AgentError::Provider(
                        "assistant-message event stream ended without a terminal event".into(),
                    )
                })?;
                let am = AgentMessage::Assistant(Box::new(final_message.clone()));
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = am.clone();
                    }
                } else {
                    context.messages.push(am.clone());
                    emit_event(
                        emit,
                        AgentEvent::MessageStart {
                            message: am.clone(),
                        },
                    )
                    .await;
                }
                emit_event(emit, AgentEvent::MessageEnd { message: am }).await;
                return Ok(final_message);
            }
        }
    }

    // Stream ended without a terminal event — finalize from result() (TS has the
    // same fallback).
    let final_message = response.result().await.map_err(|_| {
        AgentError::Provider("assistant-message event stream ended without a terminal event".into())
    })?;
    let am = AgentMessage::Assistant(Box::new(final_message.clone()));
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = am.clone();
        }
    } else {
        context.messages.push(am.clone());
        emit_event(
            emit,
            AgentEvent::MessageStart {
                message: am.clone(),
            },
        )
        .await;
    }
    emit_event(emit, AgentEvent::MessageEnd { message: am }).await;
    Ok(final_message)
}

// ----------------------------------------------------------------------------
// Truncate-fail
// ----------------------------------------------------------------------------

/// Fail every tool call in a truncated message. Mirrors TS
/// `failToolCallsFromTruncatedMessage`. Each call gets a `tool_execution_start`
/// + `tool_execution_end` (is_error:true) + tool-result `MessageStart`/`End`,
/// but is *not* executed.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[ToolCall],
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        emit_event(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;
        let reason = format!(
            "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            tool_call.name
        );
        let result = create_error_tool_result(&reason);
        emit_event(
            emit,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                result: result.clone(),
                is_error: true,
            },
        )
        .await;
        let trm = create_tool_result_message(tool_call, &result, true);
        emit_tool_result_message(emit, &trm).await;
        messages.push(trm);
    }
    Ok(ExecutedToolBatch {
        messages,
        terminate: false,
    })
}

// ----------------------------------------------------------------------------
// Tool-call execution
// ----------------------------------------------------------------------------

/// Execute a batch of tool calls. Sequential if `config.tool_execution ==
/// Sequential` or any matched tool's `execution_mode()` is `Sequential`;
/// otherwise parallel. Mirrors TS `executeToolCalls`.
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    let has_sequential = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.schema().name == tc.name)
            .map(|t| t.execution_mode() == ToolExecutionMode::Sequential)
            .unwrap_or(false)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(current_context, assistant_message, tool_calls, config, emit)
            .await
    } else {
        execute_tool_calls_parallel(current_context, assistant_message, tool_calls, config, emit)
            .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        emit_event(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let finalized =
            run_one_tool_call(current_context, assistant_message, tool_call, config, emit).await?;

        emit_tool_execution_end(emit, &finalized).await;
        let trm =
            create_tool_result_message(&finalized.tool_call, &finalized.result, finalized.is_error);
        emit_tool_result_message(emit, &trm).await;
        finalized_calls.push(finalized);
        messages.push(trm);

        if config.signal.is_cancelled() {
            break;
        }
    }

    Ok(ExecutedToolBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

/// Parallel execution. Mirrors TS `executeToolCallsParallel`:
/// 1. Each call gets `tool_execution_start` + is prepared sequentially
///    (prepare may mutate args / block).
/// 2. Immediate outcomes (not-found / blocked / aborted / validation error)
///    emit `tool_execution_end` immediately.
/// 3. Ready calls run concurrently. **`tool_execution_end` is emitted in
///    COMPLETION order** — we drive all prepared futures with `join_set`-style
///    polling and emit as each resolves.
/// 4. After all settle, **tool-result `MessageStart`/`MessageEnd` are emitted in
///    SOURCE/ordinal order** — we walk the finalized vec by index.
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    // An entry per tool call: either finalized immediately, or a pending future.
    enum Entry {
        Done(FinalizedToolCall),
        Running(tokio::task::JoinHandle<FinalizedToolCall>),
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_event(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        match prepare_tool_call(current_context, assistant_message, tool_call, config).await {
            Prepared::Immediate { result, is_error } => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(emit, &finalized).await;
                entries.push(Entry::Done(finalized));
            }
            Prepared::Ready { tool, args } => {
                // Spawn the execute + finalize so it runs concurrently with peers.
                // The on_update closure captures an `Arc<AtomicBool>` gate so calls
                // made after `execute` resolves are no-ops (late-update suppression).
                let tc = tool_call.clone();
                let am = assistant_message.clone();
                let ctx = current_context.clone();
                let cfg = config.clone();
                let emit2 = Arc::clone(emit);
                let handle = tokio::spawn(async move {
                    let executed =
                        execute_prepared_tool_call(&tc, &tool, &args, &cfg, &emit2).await;
                    finalize_executed_tool_call(&ctx, &am, &tc, &args, executed, &cfg).await
                });
                entries.push(Entry::Running(handle));
            }
        }
        if config.signal.is_cancelled() {
            break;
        }
    }

    // Collect finalized outcomes into a slot per ordinal, emitting
    // `tool_execution_end` IN COMPLETION ORDER.
    let mut finalized_by_index: Vec<Option<FinalizedToolCall>> = vec![None; entries.len()];
    let mut pending: Vec<(usize, tokio::task::JoinHandle<FinalizedToolCall>)> = Vec::new();
    for (i, e) in entries.into_iter().enumerate() {
        match e {
            Entry::Done(f) => {
                finalized_by_index[i] = Some(f);
            }
            Entry::Running(h) => pending.push((i, h)),
        }
    }

    while !pending.is_empty() {
        if pending.len() == 1 {
            // Last one: just await it directly.
            let (i, h) = pending.remove(0);
            let finalized = h.await.unwrap_or_else(|_| FinalizedToolCall {
                tool_call: panicked_tool_call(),
                result: create_error_tool_result("tool task panicked"),
                is_error: true,
            });
            emit_tool_execution_end(emit, &finalized).await;
            finalized_by_index[i] = Some(finalized);
            break;
        }

        // Multiple pending: await the *first to complete* by racing them.
        // We poll each in turn until one is `is_finished()`, then resolve it and
        // keep the rest for the next loop iteration. `yield_now()` keeps this fair.
        let mut resolved: Option<(usize, FinalizedToolCall)> = None;
        let mut still_pending: Vec<(usize, tokio::task::JoinHandle<FinalizedToolCall>)> =
            Vec::with_capacity(pending.len());
        // Find any already-finished handle without awaiting.
        for (i, h) in pending.drain(..) {
            if resolved.is_none() && h.is_finished() {
                let finalized = h.await.unwrap_or_else(|_| FinalizedToolCall {
                    tool_call: panicked_tool_call(),
                    result: create_error_tool_result("tool task panicked"),
                    is_error: true,
                });
                resolved = Some((i, finalized));
            } else {
                still_pending.push((i, h));
            }
        }
        match resolved {
            Some((i, finalized)) => {
                emit_tool_execution_end(emit, &finalized).await;
                finalized_by_index[i] = Some(finalized);
                pending = still_pending;
            }
            None => {
                // None finished yet: race them with select_all. Build a future that
                // resolves when any handle completes, then push the rest back.
                pending = still_pending;
                race_one_and_collect(emit, &mut pending, &mut finalized_by_index).await;
            }
        }
    }

    // After all settled: emit tool-result MessageStart/MessageEnd IN SOURCE
    // (ordinal) ORDER.
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    for slot in finalized_by_index.into_iter() {
        let finalized = slot.expect("every tool call finalized");
        let trm =
            create_tool_result_message(&finalized.tool_call, &finalized.result, finalized.is_error);
        emit_tool_result_message(emit, &trm).await;
        messages.push(trm);
        finalized_calls.push(finalized);
    }

    Ok(ExecutedToolBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

/// Race the pending tool futures and, as each completes, emit its
/// `tool_execution_end` (completion order) and stash it into
/// `finalized_by_index`. Loops until `pending` is empty.
///
/// This uses `futures::future::select_all` to await the first completion, then
/// re-runs with the remainder — O(n²) but n is the tool-call count per turn
/// (typically small), and it preserves exact completion order for the
/// ordering invariant without a `JoinSet` borrow-dance.
async fn race_one_and_collect(
    emit: &Arc<dyn AgentEmitter>,
    pending: &mut Vec<(usize, tokio::task::JoinHandle<FinalizedToolCall>)>,
    finalized_by_index: &mut [Option<FinalizedToolCall>],
) {
    // Take ownership of the join handles and box them into a uniform future type
    // so `select_all` can race them. Each future resolves to its ordinal + the
    // finalized outcome; as each completes we emit `tool_execution_end` (THIS is
    // where completion order is honored) and stash the result by ordinal.
    let indexed: Vec<(usize, tokio::task::JoinHandle<FinalizedToolCall>)> = std::mem::take(pending);
    let mut boxed: Vec<
        std::pin::Pin<Box<dyn std::future::Future<Output = (usize, FinalizedToolCall)> + Send>>,
    > = Vec::with_capacity(indexed.len());
    for (i, h) in indexed {
        boxed.push(Box::pin(async move {
            let f = h.await.unwrap_or_else(|_| FinalizedToolCall {
                tool_call: panicked_tool_call(),
                result: create_error_tool_result("tool task panicked"),
                is_error: true,
            });
            (i, f)
        }));
    }

    while !boxed.is_empty() {
        // select_all returns (output, index_of_completed, remaining_futures).
        let (outcome, _idx, rest) = futures::future::select_all(boxed).await;
        boxed = rest;
        let (i, finalized) = outcome;
        emit_tool_execution_end(emit, &finalized).await;
        finalized_by_index[i] = Some(finalized);
    }
}

/// Shared core for the sequential path: prepare → execute → finalize.
async fn run_one_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<FinalizedToolCall, AgentError> {
    match prepare_tool_call(current_context, assistant_message, tool_call, config).await {
        Prepared::Immediate { result, is_error } => Ok(FinalizedToolCall {
            tool_call: tool_call.clone(),
            result,
            is_error,
        }),
        Prepared::Ready { tool, args } => {
            let executed = execute_prepared_tool_call(tool_call, &tool, &args, config, emit).await;
            Ok(finalize_executed_tool_call(
                current_context,
                assistant_message,
                tool_call,
                &args,
                executed,
                config,
            )
            .await)
        }
    }
}

/// Outcome of [`prepare_tool_call`].
enum Prepared {
    /// Resolved without executing (not-found / blocked / aborted / validation error).
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    /// Validated and ready to execute.
    Ready {
        tool: Arc<dyn AgentTool>,
        args: serde_json::Value,
    },
}

/// Find the tool, call `prepare_arguments`, validate args, run `before_tool_call`.
/// Mirrors TS `prepareToolCall`.
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
) -> Prepared {
    let tool = current_context
        .tools
        .iter()
        .find(|t| t.schema().name == tool_call.name)
        .cloned();
    let tool = match tool {
        Some(t) => t,
        None => {
            return Prepared::Immediate {
                result: create_error_tool_result(&format!("Tool {} not found", tool_call.name)),
                is_error: true,
            };
        }
    };

    // prepareArguments + schema validation.
    let prepared_args = match tool.prepare_arguments(tool_call.arguments.clone()) {
        Ok(v) => v,
        Err(e) => {
            return Prepared::Immediate {
                result: create_error_tool_result(&e.to_string()),
                is_error: true,
            };
        }
    };
    let mut prepared_tool_call = tool_call.clone();
    prepared_tool_call.arguments = prepared_args;

    let validated_args = match validate_tool_arguments(tool.schema(), &prepared_tool_call) {
        Ok(v) => v,
        Err(e) => {
            return Prepared::Immediate {
                result: create_error_tool_result(&e.to_string()),
                is_error: true,
            };
        }
    };

    // before_tool_call hook (may block + set terminate, may replace args).
    // TS hands the callback `args` by reference and lets JS mutate it in place;
    // Rust hands an immutable borrow, so a rewrite is signalled by
    // `BeforeToolCallResult::args`. The replacement is applied WITHOUT
    // re-validation, mirroring TS where the mutation lands after
    // `validateToolArguments` and is trusted.
    let mut validated_args = validated_args;
    if let Some(before) = &config.before_tool_call {
        let ctx = BeforeToolCallContext {
            assistant_message,
            tool_call: &prepared_tool_call,
            args: &validated_args,
            context: current_context,
        };
        let before_result = before(ctx, config.signal.clone()).await;
        if config.signal.is_cancelled() {
            return Prepared::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(br) = before_result {
            if let Some(replacement) = br.args {
                validated_args = replacement;
            }
            if br.block {
                let mut result = create_error_tool_result(
                    &br.reason
                        .unwrap_or_else(|| "Tool execution was blocked".to_string()),
                );
                if br.terminate {
                    result.terminate = true;
                }
                return Prepared::Immediate {
                    result,
                    is_error: true,
                };
            }
        }
    }

    if config.signal.is_cancelled() {
        return Prepared::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    Prepared::Ready {
        tool,
        args: validated_args,
    }
}

/// Run the tool's `execute` with late-update suppression. Mirrors TS
/// `executePreparedToolCall`.
async fn execute_prepared_tool_call(
    tool_call: &ToolCall,
    tool: &Arc<dyn AgentTool>,
    args: &serde_json::Value,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> ExecutedToolCallOutcome {
    // Gate for late updates: flipped false once execute resolves. on_update
    // checks it and returns early. This is the late-update-suppression invariant.
    let accepting_updates = Arc::new(AtomicBool::new(true));
    let tool_call_id = tool_call.id.clone();
    let tool_name = tool_call.name.clone();
    let args_clone = args.clone();
    let emit_clone = Arc::clone(emit);

    let on_update: Arc<dyn Fn(crate::types::ToolResultPartial) + Send + Sync> = {
        let gate = Arc::clone(&accepting_updates);
        Arc::new(move |partial: crate::types::ToolResultPartial| {
            if !gate.load(Ordering::SeqCst) {
                return;
            }
            // Cancellation during a cancelled batch: still emit for non-cancelled
            // runs; the gate above is the real suppression.
            let ev = AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args_clone.clone(),
                partial_result: Arc::new(partial),
            };
            // `try_emit` is the non-blocking sync surface, so `on_update` never
            // awaits (it's an `Arc<dyn Fn>`, not an async). Late-update
            // suppression + ordering: update events interleave correctly because
            // they share the collector's mutex / the broadcast's channel.
            emit_clone.try_emit(ev);
        })
    };

    let child_token = config.signal.child_token();
    match tool
        .execute(&tool_call.id, args.clone(), child_token, on_update)
        .await
    {
        Ok(result) => {
            accepting_updates.store(false, Ordering::SeqCst);
            ExecutedToolCallOutcome {
                result,
                is_error: false,
            }
        }
        Err(e) => {
            accepting_updates.store(false, Ordering::SeqCst);
            ExecutedToolCallOutcome {
                result: create_error_tool_result(&e.to_string()),
                is_error: true,
            }
        }
    }
}

/// Result of `execute` before `after_tool_call` overrides. Mirrors TS
/// `ExecutedToolCallOutcome`.
struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

/// Apply `after_tool_call` overrides. Mirrors TS `finalizeExecutedToolCall`.
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &serde_json::Value,
    executed: ExecutedToolCallOutcome,
    config: &AgentLoopConfig,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = &config.after_tool_call {
        let ctx = AfterToolCallContext {
            assistant_message,
            tool_call,
            args,
            result: &result,
            is_error,
            context: current_context,
        };
        match after(ctx, config.signal.clone()).await {
            Some(after_result) => {
                if let Some(c) = after_result.content {
                    result.content = c;
                }
                if let Some(d) = after_result.details {
                    result.details = d;
                }
                if let Some(u) = after_result.usage {
                    result.usage = Some(u);
                }
                if let Some(t) = after_result.terminate {
                    result.terminate = t;
                }
                if let Some(ie) = after_result.is_error {
                    is_error = ie;
                }
            }
            None => {}
        }
    }

    FinalizedToolCall {
        tool_call: tool_call.clone(),
        result,
        is_error,
    }
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// Early-terminate iff the batch is non-empty AND every result sets
/// `terminate == true`. Mirrors TS `shouldTerminateToolBatch`.
fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCall]) -> bool {
    !finalized_calls.is_empty() && finalized_calls.iter().all(|f| f.result.terminate)
}

/// Build an error `AgentToolResult` — text content, null details.
/// Mirrors TS `createErrorToolResult`.
fn create_error_tool_result(message: &str) -> AgentToolResult {
    AgentToolResult::error_text(message)
}

/// Build a `ToolResultMessage` from a finalized call. Mirrors TS
/// `createToolResultMessage`. Content is normalized to non-null via
/// `AgentToolResult::into_content` (TS guards `result.content ?? []`).
fn create_tool_result_message(
    tool_call: &ToolCall,
    result: &AgentToolResult,
    is_error: bool,
) -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: result.clone().into_content(),
        details: Some(result.details.clone()),
        usage: result.usage.clone(),
        added_tool_names: result.added_tool_names.clone(),
        is_error,
        timestamp: now_ms(),
    }
}

/// Emit `tool_execution_end` for a finalized call.
async fn emit_tool_execution_end(emit: &Arc<dyn AgentEmitter>, finalized: &FinalizedToolCall) {
    emit_event(
        emit,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        },
    )
    .await;
}

/// Emit `message_start` + `message_end` for a tool-result message. Tool-result
/// messages fire AFTER all `tool_execution_end`s, in source/ordinal order.
async fn emit_tool_result_message(emit: &Arc<dyn AgentEmitter>, trm: &ToolResultMessage) {
    let am = AgentMessage::ToolResult(Box::new(trm.clone()));
    emit_event(
        emit,
        AgentEvent::MessageStart {
            message: am.clone(),
        },
    )
    .await;
    emit_event(emit, AgentEvent::MessageEnd { message: am }).await;
}

/// Drain steering messages (empty vec if no hook).
async fn drain_steering(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    if let Some(hook) = &config.get_steering_messages {
        hook().await
    } else {
        Vec::new()
    }
}

/// Drain follow-up messages (empty vec if no hook).
async fn drain_follow_up(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    if let Some(hook) = &config.get_follow_up_messages {
        hook().await
    } else {
        Vec::new()
    }
}

/// Call `prepare_next_turn` if configured.
async fn prepare_next_turn(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> Option<crate::types::AgentLoopTurnUpdate> {
    if let Some(hook) = &config.prepare_next_turn {
        let ctx = crate::types::ShouldStopAfterTurnContext {
            message,
            tool_results,
            context,
            new_messages,
        };
        hook(ctx).await
    } else {
        None
    }
}

async fn after_tool_results(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> Option<crate::types::AgentLoopTurnUpdate> {
    let hook = config.after_tool_results.as_ref()?;
    let ctx = crate::types::ShouldStopAfterTurnContext {
        message,
        tool_results,
        context,
        new_messages,
    };
    hook(ctx).await
}

/// Call `should_stop_after_turn` if configured.
async fn should_stop_after_turn(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> bool {
    if let Some(hook) = &config.should_stop_after_turn {
        let ctx = crate::types::ShouldStopAfterTurnContext {
            message,
            tool_results,
            context,
            new_messages,
        };
        hook(ctx).await
    } else {
        false
    }
}

/// Emit one event via the emitter.
async fn emit_event(emit: &Arc<dyn AgentEmitter>, event: AgentEvent) {
    emit.emit(event).await;
}

/// Monotonic-ish ms timestamp. The loop only needs ordering + JSONL serializability,
/// not wall-clock accuracy. Uses an atomic counter so tests are deterministic.
fn now_ms() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static T: AtomicI64 = AtomicI64::new(1);
    T.fetch_add(1, Ordering::Relaxed)
}

/// A placeholder tool call for the panic-recovery path.
fn panicked_tool_call() -> ToolCall {
    ToolCall {
        kind: ToolCallType,
        id: "<panic>".to_string(),
        name: "<panic>".to_string(),
        arguments: serde_json::Value::Null,
        thought_signature: None,
        namespace: None,
    }
}
