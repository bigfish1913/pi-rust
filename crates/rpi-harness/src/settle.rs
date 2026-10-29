//! Commit-on-settle: persist a run's messages when they settle, not at run end.
//!
//! # Why this exists
//!
//! `tool_started` is the record that lets recovery say *which tool's side effect
//! is unknown* after a crash. It cannot be written from rpi's old model, where a
//! whole run's messages were persisted in one pass at the end: the reducer's
//! `validate_tool_start` requires `assistant_entry_id` to **already exist** with
//! a matching tool call at `tool_index`, and a tool runs long before the run
//! finishes. Upstream satisfies that requirement by committing the assistant
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
//! mid-run compaction stay in the log. That matches upstream (its compaction
//! commits a summary entry and leaves the pre-summary entries in place; context
//! building starts after the last compaction via the cut-point scan) and is the
//! reason this is a deliberate trade rather than an oversight.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, StopReason};

use crate::agent_harness::AgentHarness;
use crate::session::types::{
    ProvisionedEntry, ProvisionedKind, RecordBase, ToolReplay, ToolStartedRecord,
};

static SETTLE_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();

fn settle_diagnostics_enabled() -> bool {
    *SETTLE_DIAGNOSTICS
        .get_or_init(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0"))
}

#[derive(Clone, Copy)]
enum SettlePhase {
    Emit,
    EmitHandle,
    SettleAwait,
    InnerEmit,
    PrepareCommit,
    AssistantEntryStep,
    ClearStream,
    ToolStarted,
    ToolResultEntry,
}

struct SettleDiagnostics {
    enabled: bool,
    counts: [AtomicU64; 9],
    elapsed_ns: [AtomicU64; 9],
}

impl SettleDiagnostics {
    fn new() -> Self {
        Self {
            enabled: settle_diagnostics_enabled(),
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            elapsed_ns: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn index(phase: SettlePhase) -> usize {
        match phase {
            SettlePhase::Emit => 0,
            SettlePhase::EmitHandle => 1,
            SettlePhase::SettleAwait => 2,
            SettlePhase::InnerEmit => 3,
            SettlePhase::PrepareCommit => 4,
            SettlePhase::AssistantEntryStep => 5,
            SettlePhase::ClearStream => 6,
            SettlePhase::ToolStarted => 7,
            SettlePhase::ToolResultEntry => 8,
        }
    }

    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record(&self, phase: SettlePhase, started: Option<Instant>) {
        if let Some(started) = started {
            let index = Self::index(phase);
            self.counts[index].fetch_add(1, Ordering::Relaxed);
            self.elapsed_ns[index].fetch_add(
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
    }

    fn report(&self, run_id: &str) {
        if !self.enabled {
            return;
        }
        let names = [
            "emit",
            "emit_handle",
            "settle_await",
            "inner_emit",
            "prepare_commit",
            "assistant_entry_step",
            "clear_stream",
            "tool_started",
            "tool_result_entry",
        ];
        let mut fields = String::new();
        for (index, name) in names.iter().enumerate() {
            use std::fmt::Write;
            let _ = write!(
                fields,
                " {}_calls={} {}_ns={}",
                name,
                self.counts[index].load(Ordering::Relaxed),
                name,
                self.elapsed_ns[index].load(Ordering::Relaxed),
            );
        }
        eprintln!("RPI_SETTLE_DIAGNOSTICS run_id={}{}", run_id, fields);
    }
}

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
    /// How many assistant messages have settled. The n-th settled assistant
    /// message is the recorder's stream n: both are assigned in event order, and
    /// only assistant messages create streams.
    assistant_settled: AtomicUsize,
    /// Indices already committed, so the run-end pass does not duplicate them.
    committed: Mutex<BTreeSet<usize>>,
    /// The entry each settled message produced, by loop-message index.
    assistant_entries: Mutex<BTreeMap<usize, String>>,
    /// Loop-message index to its run-wide frame stream. Used by every assistant
    /// commit path, including context boundaries and the final persistence pass.
    assistant_streams: Mutex<BTreeMap<usize, usize>>,
    /// Stable commit reservations, including externally queued messages and
    /// the first generated assistant of an accepted retry.
    reserved_entries: Mutex<BTreeMap<usize, String>>,
    injected_indices: Mutex<BTreeSet<usize>>,
    retry_result: Mutex<Option<String>>,
    /// `tool_call_id` → (assistant entry id, ordinal within that message). Filled
    /// when a message carrying tool calls settles, read when each tool starts.
    pending_calls: Mutex<BTreeMap<String, (String, u32)>>,
    /// `tool_call_id` → reserved result entry id, so the tool result is committed
    /// under the id its `tool_started` record already named.
    reserved_results: Mutex<BTreeMap<String, String>>,
    /// Queued messages handed to the loop for injection, in the order they will be
    /// emitted. MessageStart transfers each reservation to its loop-message
    /// index, so every role can commit at settle or fall back to run-end writes
    /// without changing the durable queue target's id.
    injected: Mutex<VecDeque<(String, AgentMessage)>>,
    /// A final assistant normally waits for run-end persistence. If a queued
    /// message continues this run, commit that assistant before the injection.
    uncommitted_assistant: Mutex<Option<(usize, usize, AssistantMessage)>>,
    /// The durable frame writer, installed after the run's emitter is created.
    /// Commit paths flush it before binding an assistant entry.
    frame_recorder: Mutex<Option<Arc<crate::frame_progress::FrameRecordingEmitter>>>,
    diagnostics: SettleDiagnostics,
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
            assistant_settled: AtomicUsize::new(0),
            committed: Mutex::new(BTreeSet::new()),
            assistant_entries: Mutex::new(BTreeMap::new()),
            assistant_streams: Mutex::new(BTreeMap::new()),
            reserved_entries: Mutex::new(BTreeMap::new()),
            injected_indices: Mutex::new(BTreeSet::new()),
            retry_result: Mutex::new(None),
            pending_calls: Mutex::new(BTreeMap::new()),
            reserved_results: Mutex::new(BTreeMap::new()),
            injected: Mutex::new(VecDeque::new()),
            uncommitted_assistant: Mutex::new(None),
            frame_recorder: Mutex::new(None),
            diagnostics: SettleDiagnostics::new(),
        }
    }

    pub(crate) fn set_frame_recorder(
        &self,
        recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
    ) {
        *self.frame_recorder.lock().unwrap() = Some(recorder);
    }

    async fn flush_frame_progress(&self) -> crate::result::HarnessResult<()> {
        let recorder = self.frame_recorder.lock().unwrap().clone();
        if let Some(recorder) = recorder {
            recorder
                .flush_with_reason(crate::frame_progress::FrameFlushReason::PrepareCommit)
                .await?;
        }
        Ok(())
    }

    /// Register a message the run is about to inject, with the entry id its
    /// `queue_enqueued` record reserved for it.
    pub fn push_injected(&self, entry_id: impl Into<String>, message: AgentMessage) {
        self.injected
            .lock()
            .unwrap()
            .push_back((entry_id.into(), message));
    }

    /// Start an attempt after its retry wait. Resetting before the wait would
    /// lose the previous attempt's committed indices if the wait is cancelled.
    pub fn set_attempt(&self, attempt: u32) {
        self.attempt.store(attempt, Ordering::Release);
        // Each invocation of the loop returns its own new_messages indices.
        // Frame stream ordinals, however, remain run-wide across retries.
        self.seen.store(0, Ordering::Release);
        self.committed.lock().unwrap().clear();
        self.assistant_entries.lock().unwrap().clear();
        self.assistant_streams.lock().unwrap().clear();
        self.reserved_entries.lock().unwrap().clear();
        self.injected_indices.lock().unwrap().clear();
        self.retry_result.lock().unwrap().take();
        self.uncommitted_assistant.lock().unwrap().take();
    }

    pub(crate) fn reserve_retry_result(&self, entry_id: String) {
        *self.retry_result.lock().unwrap() = Some(entry_id);
    }

    pub(crate) fn stream_at(&self, index: usize) -> Option<usize> {
        self.assistant_streams.lock().unwrap().get(&index).copied()
    }

    /// Reuse reservations on fallback writes as well as the normal settle path.
    pub(crate) fn reserve_entry_id(&self, index: usize, harness: &AgentHarness) -> String {
        self.reserved_entries
            .lock()
            .unwrap()
            .entry(index)
            .or_insert_with(|| harness.next_entry_id())
            .clone()
    }

    /// Indices already persisted by this observer.
    pub fn committed_indices(&self) -> BTreeSet<usize> {
        self.committed.lock().unwrap().clone()
    }

    pub fn committed_count(&self) -> usize {
        self.committed.lock().unwrap().len()
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.attempt.load(Ordering::Acquire)
    }

    pub(crate) fn entry_at(&self, index: usize) -> Option<String> {
        self.assistant_entries.lock().unwrap().get(&index).cloned()
    }

    async fn flush_prepare_commit(&self) -> crate::result::HarnessResult<()> {
        let prepare_started = self.diagnostics.start();
        let prepare_result = self.flush_frame_progress().await;
        self.diagnostics
            .record(SettlePhase::PrepareCommit, prepare_started);
        prepare_result
    }

    /// Flush frame progress before a commit whose binding is written together
    /// with the entry. The binding itself is appended by the caller so it can
    /// share the entry/step durable append.
    pub(crate) async fn prepare_commit_for_assistant(&self) -> crate::result::HarnessResult<()> {
        self.flush_prepare_commit().await
    }

    /// Make assistant persistence idempotent across a crash between the entry
    /// write and frame retirement. Non-assistant indices have no stream.
    pub(crate) async fn prepare_commit(
        &self,
        index: usize,
        entry_id: &str,
        harness: &AgentHarness,
    ) -> crate::result::HarnessResult<()> {
        self.flush_prepare_commit().await?;
        let stream_index = self.assistant_streams.lock().unwrap().get(&index).copied();
        if let Some(stream_index) = stream_index {
            crate::frame_progress::bind_stream_entry(
                harness.session(),
                harness.lane_name(),
                &self.run_id,
                stream_index,
                entry_id,
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) fn record_commit(&self, index: usize, entry_id: &str) {
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

impl Drop for SettleState {
    fn drop(&mut self) {
        self.diagnostics.report(&self.run_id);
    }
}

/// Emitter wrapper that commits settled messages to the session.
///
/// Sits *around* the frame recorder, so frames are still appended for every
/// delta before the message is committed — a crash between "settled" and
/// "committed" is then still recoverable from the frames.
pub struct SettlingEmitter {
    inner: Arc<dyn AgentEmitter>,
    /// The frame recorder, kept as a concrete handle so a committed message can
    /// retire its own stream. Forwarding still goes through `inner`.
    recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
    state: Arc<SettleState>,
    harness: AgentHarness,
}

impl SettlingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
        state: Arc<SettleState>,
        harness: AgentHarness,
    ) -> Self {
        Self {
            inner,
            recorder,
            state,
            harness,
        }
    }

    /// Commit one settled assistant message and record its `step_attempt`.
    async fn settle_assistant(
        &self,
        index: usize,
        stream_index: usize,
        message: &AssistantMessage,
    ) {
        let entry = ProvisionedEntry {
            id: self.state.reserve_entry_id(index, &self.harness),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(message.clone())),
                terminate: None,
            },
        };
        let entry_id = entry.id.clone();
        if let Err(error) = self.state.prepare_commit_for_assistant().await {
            tracing::warn!(%error, "could not bind a settled assistant stream");
            return;
        }
        let entry_started = self.state.diagnostics.start();
        let attempt = self.state.attempt.load(Ordering::Acquire) + 1;
        let entry_result = self
            .harness
            .append_bound_provisioned_with_step_attempt(
                entry,
                &self.state.run_id,
                attempt,
                stream_index,
            )
            .await;
        self.state
            .diagnostics
            .record(SettlePhase::AssistantEntryStep, entry_started);
        match entry_result {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
                self.state.record_tool_calls(&entry_id, message);
                // The message is an entry now, so its frames are no longer
                // progress. Retiring the stream is what stops a crash in a *later*
                // turn from replaying this one and duplicating it.
                let clear_started = self.state.diagnostics.start();
                let clear_result = self.recorder.clear_stream(stream_index).await;
                self.state
                    .diagnostics
                    .record(SettlePhase::ClearStream, clear_started);
                if let Err(error) = clear_result {
                    tracing::warn!(
                        stream_index,
                        %error,
                        "could not retire a committed assistant stream"
                    );
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
        let record = ToolStartedRecord {
            base: RecordBase {
                id: self.harness.next_entry_id(),
                seq: 0,
                // The store re-stamps the lane; it must still name a real one,
                // since the lane is how the store routes the append.
                lane: self.harness.lane_name().to_string(),
                timestamp: 0,
            },
            run_id: self.state.run_id.clone(),
            assistant_entry_id,
            tool_index,
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            effective_args: args.clone(),
            result_entry_id,
            replay,
        };
        let tool_started_started = self.state.diagnostics.start();
        let tool_started_result = self.harness.write_tool_started(record).await;
        self.state
            .diagnostics
            .record(SettlePhase::ToolStarted, tool_started_started);
        if let Err(error) = tool_started_result {
            tracing::warn!(%error, tool_call_id, "could not record a tool_started frame");
        }
    }

    /// Commit any injected message under the id its queue record reserved.
    ///
    /// Delivery reservations were matched by content at MessageStart, before
    /// role-based settlement, and remain available for fallback persistence.
    async fn settle_injected(&self, index: usize, message: &AgentMessage) {
        let entry_id = self.state.reserve_entry_id(index, &self.harness);
        let assistant = self.state.uncommitted_assistant.lock().unwrap().take();
        if let Some((assistant_index, stream_index, assistant)) = assistant {
            let already_committed = self
                .state
                .committed
                .lock()
                .unwrap()
                .contains(&assistant_index);
            if !already_committed {
                let assistant_id = self.state.reserve_entry_id(assistant_index, &self.harness);
                if let Err(error) = self
                    .state
                    .prepare_commit(assistant_index, &assistant_id, &self.harness)
                    .await
                {
                    tracing::warn!(%error, "could not bind the assistant before queue injection");
                    return;
                }
                let entry = ProvisionedEntry {
                    id: assistant_id.clone(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::Assistant(Box::new(assistant)),
                        terminate: None,
                    },
                };
                if let Err(error) = self.harness.append_provisioned(entry).await {
                    tracing::warn!(%error, "could not commit the assistant before queue injection");
                    return;
                }
                self.state.record_commit(assistant_index, &assistant_id);
            }
            if let Err(error) = self.recorder.clear_stream(stream_index).await {
                tracing::warn!(%error, "could not retire the assistant before queue injection");
            }
        }
        let entry = ProvisionedEntry {
            id: entry_id.clone(),
            kind: ProvisionedKind::Message {
                message: message.clone(),
                terminate: None,
            },
        };
        if let Err(error) = self
            .state
            .prepare_commit(index, &entry_id, &self.harness)
            .await
        {
            tracing::warn!(%error, "could not bind an injected assistant stream");
            return;
        }
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit an injected message");
            }
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
            id: result_entry_id.clone(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::ToolResult(Box::new(message.clone())),
                terminate: None,
            },
        };
        let result_entry_started = self.state.diagnostics.start();
        let result_entry_result = self.harness.append_provisioned(entry).await;
        self.state
            .diagnostics
            .record(SettlePhase::ToolResultEntry, result_entry_started);
        match result_entry_result {
            Ok(_) => {
                self.state.record_commit(index, &result_entry_id);
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit a settled tool result");
            }
        }
    }

    fn handle(&self, event: &AgentEvent) -> Option<BoxFuture<'static, ()>> {
        // Recognize queue deliveries before dispatching on role: a stored
        // assistant/tool-result is an injection, not a new provider generation
        // or a tool invocation produced by this run.
        if let AgentEvent::MessageEnd { message } = event {
            let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
            if self.state.injected_indices.lock().unwrap().contains(&index) {
                if matches!(message, AgentMessage::Assistant(_)) {
                    self.state.assistant_settled.fetch_add(1, Ordering::AcqRel);
                }
                let emitter = Self {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let message = message.clone();
                return Some(Box::pin(async move {
                    emitter.settle_injected(index, &message).await;
                }));
            }
        }
        match event {
            AgentEvent::MessageStart { message } => {
                let index = self.state.seen.fetch_add(1, Ordering::AcqRel);
                let injection = {
                    let mut pending = self.state.injected.lock().unwrap();
                    if pending.front().is_some_and(|(_, queued)| queued == message) {
                        pending.pop_front()
                    } else {
                        None
                    }
                };
                if let Some((entry_id, _)) = injection {
                    self.state
                        .reserved_entries
                        .lock()
                        .unwrap()
                        .insert(index, entry_id);
                    self.state.injected_indices.lock().unwrap().insert(index);
                }
                if matches!(message, AgentMessage::Assistant(_)) {
                    let stream_index = self.state.assistant_settled.load(Ordering::Acquire);
                    self.state
                        .assistant_streams
                        .lock()
                        .unwrap()
                        .insert(index, stream_index);
                    if !self.state.injected_indices.lock().unwrap().contains(&index) {
                        let retry_id = self.state.retry_result.lock().unwrap().take();
                        if let Some(entry_id) = retry_id {
                            self.state
                                .reserved_entries
                                .lock()
                                .unwrap()
                                .insert(index, entry_id.clone());
                            let state = Arc::clone(&self.state);
                            let harness = self.harness.clone();
                            // This binding also distinguishes a scheduled wait
                            // from a retry that has actually begun streaming.
                            return Some(Box::pin(async move {
                                if let Err(error) =
                                    state.prepare_commit(index, &entry_id, &harness).await
                                {
                                    tracing::warn!(%error, "could not bind a starting retry stream");
                                }
                            }));
                        }
                    }
                }
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
                let stream_index = self.state.assistant_settled.fetch_add(1, Ordering::AcqRel);
                self.state
                    .assistant_streams
                    .lock()
                    .unwrap()
                    .insert(index, stream_index);
                if !needs_early_commit(assistant, retrying) {
                    if !retrying {
                        *self.state.uncommitted_assistant.lock().unwrap() =
                            Some((index, stream_index, (**assistant).clone()));
                    }
                    return None;
                }
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                // Ordinal first, then increment: the message that just settled is
                // stream `ordinal`.
                let message = (**assistant).clone();
                Some(Box::pin(async move {
                    emitter
                        .settle_assistant(index, stream_index, &message)
                        .await;
                }))
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
            } => {
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
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
                    recorder: Arc::clone(&self.recorder),
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
            AgentEvent::AgentEnd { .. } => {
                // The final assistant message of a completed or aborted run is
                // persisted by `run_core` AFTER its `MessageEnd` broadcast, so
                // the log can still be missing the run's last turn here. Commit
                // it now, before `AgentEnd` itself is forwarded: every consumer
                // of the broadcast can then treat `AgentEnd` as the point where
                // the durable log is complete for this run (the TUI's footer
                // reconciliation is exactly such a consumer). Same fallback the
                // injected-delivery path uses, minus an injection to deliver.
                // `prepare_commit`/`append_provisioned` need await, so this runs
                // inside the future `emit` polls, not in `handle`.
                if self.state.uncommitted_assistant.lock().unwrap().is_none() {
                    return None;
                }
                let emitter = Self {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                Some(Box::pin(async move {
                    emitter.settle_final_assistant().await;
                }))
            }
            _ => None,
        }
    }

    /// Commit the run's final assistant message before `AgentEnd` is forwarded.
    /// Called with the lock NOT held: everything here awaits.
    async fn settle_final_assistant(&self) {
        let Some((assistant_index, stream_index, assistant)) =
            self.state.uncommitted_assistant.lock().unwrap().take()
        else {
            return;
        };
        let already_committed = self
            .state
            .committed
            .lock()
            .unwrap()
            .contains(&assistant_index);
        if already_committed {
            // The normal settle path beat us to it; only the stream's frames
            // still need retiring.
            if let Err(error) = self.recorder.clear_stream(stream_index).await {
                tracing::warn!(
                    stream_index,
                    %error,
                    "could not retire the final assistant before AgentEnd"
                );
            }
            return;
        }
        let assistant_id = self.state.reserve_entry_id(assistant_index, &self.harness);
        if let Err(error) = self
            .state
            .prepare_commit(assistant_index, &assistant_id, &self.harness)
            .await
        {
            // The message stays uncommitted, so `run_core`'s persistence pass
            // will still write it after the emitter returns.
            tracing::warn!(%error, "could not bind the final assistant before AgentEnd");
            return;
        }
        let entry = ProvisionedEntry {
            id: assistant_id.clone(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(assistant)),
                terminate: None,
            },
        };
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(assistant_index, &assistant_id);
            }
            Err(error) => {
                // Leave the message for `run_core`'s own persistence pass,
                // which runs after this emitter returns; the log is then
                // complete one step later than usual.
                tracing::warn!(
                    %error,
                    "could not commit the final assistant before AgentEnd"
                );
                return;
            }
        }
        if let Err(error) = self.recorder.clear_stream(stream_index).await {
            tracing::warn!(
                stream_index,
                %error,
                "could not retire the final assistant before AgentEnd"
            );
        }
    }
}

impl AgentEmitter for SettlingEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let inner = Arc::clone(&self.inner);
        let state = Arc::clone(&self.state);
        let emit_started = state.diagnostics.start();
        let handle_started = state.diagnostics.start();
        let settles = self.handle(&event);
        state
            .diagnostics
            .record(SettlePhase::EmitHandle, handle_started);
        Box::pin(async move {
            // Commit *before* forwarding: a UI that renders the settled message
            // must not be able to observe it before it is durable.
            let settle_started = state.diagnostics.start();
            if let Some(settles) = settles {
                settles.await;
            }
            state
                .diagnostics
                .record(SettlePhase::SettleAwait, settle_started);
            let inner_started = state.diagnostics.start();
            inner.emit(event).await;
            state
                .diagnostics
                .record(SettlePhase::InnerEmit, inner_started);
            state.diagnostics.record(SettlePhase::Emit, emit_started);
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }
}
