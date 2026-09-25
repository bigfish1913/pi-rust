//! Commit-on-settle: persist a run's messages when they settle, not at run end.
//!
//! # Why this exists
//!
//! `tool_started` is the record that lets recovery say *which tool's side effect
//! is unknown* after a crash. It cannot be written from rpi's old model, where a
//! whole run's messages were persisted in one pass at the end: the reducer's
//! `validate_tool_start` requires `assistant_entry_id` to **already exist** with
//! a matching tool call at `tool_index`, and a tool runs long before the run
//! finishes. Native pi satisfies that requirement by committing the assistant
//! message when the step settles (`assistant.ready`), *then* running its tools.
//!
//! This module ports that ordering: an assistant message carrying tool calls is
//! committed as soon as it is final (`MessageEnd`), with a **reserved** entry id
//! that the matching `step_attempt` record names. Once the message is an entry,
//! its tools can be recorded before they run.
//!
//! # What is deliberately *not* committed early
//!
//! - **The final assistant message** (no tool calls): it ends the run and is
//!   persisted by the normal run-end pass, so `leaf_id`/`final_entry_id` and the
//!   `Completed` outcome keep their existing meaning.
//! - **A message from an attempt that will be retried**: the harness re-runs the
//!   whole loop on a retryable failure, so committing a failed attempt's message
//!   would leave a dead assistant message in history and put two assistant turns
//!   in a row in front of the provider. The retry decision is computable at
//!   settle time (`enabled && attempt < max && is_retryable_error`), and is
//!   evaluated here with the same inputs the retry loop uses.
//!
//! # Interaction with the compaction cut
//!
//! A message committed here is already an entry, so the run-end pass cannot
//! "skip" it via `post_compaction_cut`. That means messages summarized by a
//! mid-run compaction stay in the log. That matches native pi (its compaction
//! commits a summary entry and leaves the pre-summary entries in place; context
//! building starts after the last compaction via the cut-point scan) and is the
//! reason this is a deliberate trade rather than an oversight.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, StopReason};

use crate::agent_harness::AgentHarness;
use crate::session::types::{ProvisionedEntry, ProvisionedKind, StepKind, ToolReplay};

/// Whether a settled assistant message is part of the run's final history.
/// Mirrors the retry loop's own eligibility test so the two cannot disagree
/// about which attempt survives.
pub fn will_retry(
    message: &AssistantMessage,
    retry_enabled: bool,
    attempt: u32,
    max_retries: u32,
) -> bool {
    retry_enabled
        && attempt < max_retries
        && message.stop_reason == StopReason::Error
        && crate::compaction::is_retryable_assistant_error(message)
}

/// Whether a settled assistant message must be committed early.
///
/// Only messages that lead into tool execution need to be entries before the run
/// ends, because only they are referenced by `tool_started` records.
fn needs_early_commit(message: &AssistantMessage, retrying: bool) -> bool {
    !retrying
        && message
            .content
            .iter()
            .any(|block| matches!(block, rpi_ai::types::Content::ToolCall(_)))
}

/// What a settle observer needs to know about the run it is watching. Shared
/// with the run loop, which owns the retry counter.
pub struct SettleState {
    /// Index of the next loop message, in `new_messages` order. Advanced by
    /// `MessageStart` so a commit can name the index it persisted.
    seen: AtomicUsize,
    /// The attempt number the retry loop is currently on.
    attempt: AtomicU32,
    retry_enabled: bool,
    max_retries: u32,
    run_id: String,
    /// Per-tool replay policy, from the run's tool snapshot. A `ToolStarted`
    /// record has to carry it, and the settle observer sees only the event.
    tool_replays: BTreeMap<String, ToolReplay>,
    /// Indices already committed, so the run-end pass does not duplicate them.
    committed: Mutex<BTreeSet<usize>>,
    /// The assistant entry each settled message produced, by index.
    assistant_entries: Mutex<BTreeMap<usize, String>>,
    /// `tool_call_id` → (assistant entry id, ordinal within that message). Filled
    /// when a message carrying tool calls settles, read when each tool starts.
    pending_calls: Mutex<BTreeMap<String, (String, u32)>>,
    /// `tool_call_id` → reserved result entry id, so the tool result is committed
    /// under the id its `tool_started` record already named.
    reserved_results: Mutex<BTreeMap<String, String>>,
}

impl SettleState {
    pub fn new(
        run_id: impl Into<String>,
        retry_enabled: bool,
        max_retries: u32,
        tool_replays: BTreeMap<String, ToolReplay>,
    ) -> Self {
        Self {
            seen: AtomicUsize::new(0),
            attempt: AtomicU32::new(0),
            retry_enabled,
            max_retries,
            run_id: run_id.into(),
            tool_replays,
            committed: Mutex::new(BTreeSet::new()),
            assistant_entries: Mutex::new(BTreeMap::new()),
            pending_calls: Mutex::new(BTreeMap::new()),
            reserved_results: Mutex::new(BTreeMap::new()),
        }
    }

    /// Advance the attempt counter; called by the retry loop so the settle
    /// observer sees the same number the loop's next attempt will use.
    pub fn set_attempt(&self, attempt: u32) {
        self.attempt.store(attempt, Ordering::Release);
    }

    /// Indices already persisted by this observer.
    pub fn committed_indices(&self) -> BTreeSet<usize> {
        self.committed.lock().unwrap().clone()
    }

    pub fn committed_count(&self) -> usize {
        self.committed.lock().unwrap().len()
    }

    fn record_commit(&self, index: usize, entry_id: &str) {
        self.committed.lock().unwrap().insert(index);
        self.assistant_entries
            .lock()
            .unwrap()
            .insert(index, entry_id.to_string());
    }

    /// Remember each tool call of a freshly committed assistant message, keyed so
    /// `tool_execution_start` can record it against the right entry and ordinal.
    fn record_tool_calls(&self, entry_id: &str, message: &AssistantMessage) {
        let mut pending = self.pending_calls.lock().unwrap();
        for (ordinal, block) in message.content.iter().enumerate() {
            if let rpi_ai::types::Content::ToolCall(call) = block {
                pending.insert(call.id.clone(), (entry_id.to_string(), ordinal as u32));
            }
        }
    }
}

/// Emitter wrapper that commits settled messages to the session.
///
/// Sits *around* the frame recorder, so frames are still appended for every
/// delta before the message is committed — a crash between "settled" and
/// "committed" is then still recoverable from the frames.
pub struct SettlingEmitter {
    inner: Arc<dyn AgentEmitter>,
    state: Arc<SettleState>,
    harness: AgentHarness,
}

impl SettlingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        state: Arc<SettleState>,
        harness: AgentHarness,
    ) -> Self {
        Self {
            inner,
            state,
            harness,
        }
    }

    /// Commit one settled assistant message and record its `step_attempt`.
    async fn settle_assistant(&self, index: usize, message: &AssistantMessage) {
        let entry = ProvisionedEntry {
            id: self.harness.next_entry_id(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(message.clone())),
                terminate: None,
            },
        };
        let entry_id = entry.id.clone();
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
                self.state.record_tool_calls(&entry_id, message);
                // One assistant generation = one step attempt. The entry id is
                // the one just committed, so recovery can check whether the step
                // actually landed.
                if let Err(error) = self
                    .harness
                    .write_step_attempt_record(
                        &self.state.run_id,
                        StepKind::Assistant,
                        self.state.attempt.load(Ordering::Acquire) + 1,
                        &entry_id,
                        None,
                    )
                    .await
                {
                    tracing::warn!(%error, "could not record a settled assistant step");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit a settled assistant message");
            }
        }
    }

    /// Record a tool call that is about to run, and reserve the id its result
    /// will be committed under.
    ///
    /// Skipped with a warning when the call's assistant message was never
    /// committed: a `tool_started` record must reference an existing assistant
    /// entry (the reducer rejects a mismatch), so silently writing one would
    /// corrupt the log.
    async fn start_tool(&self, tool_call_id: &str, tool_name: &str, args: &serde_json::Value) {
        let Some((assistant_entry_id, tool_index)) = self
            .state
            .pending_calls
            .lock()
            .unwrap()
            .get(tool_call_id)
            .cloned()
        else {
            tracing::warn!(
                tool_call_id,
                "skipping a tool_started record: the assistant message carrying this call was not committed"
            );
            return;
        };
        let result_entry_id = self.harness.next_entry_id();
        self.state
            .reserved_results
            .lock()
            .unwrap()
            .insert(tool_call_id.to_string(), result_entry_id.clone());
        let replay = self
            .state
            .tool_replays
            .get(tool_name)
            .copied()
            .unwrap_or(ToolReplay::Never);
        if let Err(error) = self
            .harness
            .write_tool_started_record(
                &self.state.run_id,
                &assistant_entry_id,
                tool_index,
                tool_call_id,
                tool_name,
                args.clone(),
                &result_entry_id,
                replay,
            )
            .await
        {
            tracing::warn!(%error, tool_call_id, "could not record a tool_started frame");
        }
    }

    /// Commit a settled tool result under the id its `tool_started` named.
    async fn settle_tool_result(&self, index: usize, message: &rpi_ai::types::ToolResultMessage) {
        let Some(result_entry_id) = self
            .state
            .reserved_results
            .lock()
            .unwrap()
            .get(&message.tool_call_id)
            .cloned()
        else {
            // No reserved id (e.g. the call was never recorded): fall back to the
            // run-end pass, which mints an id normally.
            return;
        };
        let entry = ProvisionedEntry {
            id: result_entry_id,
            kind: ProvisionedKind::Message {
                message: AgentMessage::ToolResult(Box::new(message.clone())),
                terminate: None,
            },
        };
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.committed.lock().unwrap().insert(index);
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit a settled tool result");
            }
        }
    }

    fn handle(&self, event: &AgentEvent) -> Option<BoxFuture<'static, ()>> {
        match event {
            AgentEvent::MessageStart { .. } => {
                self.state.seen.fetch_add(1, Ordering::AcqRel);
                None
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(assistant),
            } => {
                // `seen` was advanced by this message's `MessageStart`; the index
                // of the message that just settled is one less.
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let retrying = will_retry(
                    assistant,
                    self.state.retry_enabled,
                    self.state.attempt.load(Ordering::Acquire),
                    self.state.max_retries,
                );
                if !needs_early_commit(assistant, retrying) {
                    return None;
                }
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let message = (**assistant).clone();
                Some(Box::pin(async move {
                    emitter.settle_assistant(index, &message).await;
                }))
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
            } => {
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let result = (**result).clone();
                Some(Box::pin(async move {
                    emitter.settle_tool_result(index, &result).await;
                }))
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let tool_call_id = tool_call_id.clone();
                let tool_name = tool_name.clone();
                let args = args.clone();
                Some(Box::pin(async move {
                    emitter.start_tool(&tool_call_id, &tool_name, &args).await;
                }))
            }
            _ => None,
        }
    }
}

impl AgentEmitter for SettlingEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let inner = Arc::clone(&self.inner);
        let settles = self.handle(&event);
        Box::pin(async move {
            // Commit *before* forwarding: a UI that renders the settled message
            // must not be able to observe it before it is durable.
            if let Some(settles) = settles {
                settles.await;
            }
            inner.emit(event).await;
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }
}
