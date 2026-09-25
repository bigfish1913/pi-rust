//! Mirrors `packages/agent/src/harness/agent-harness.ts` — the `AgentHarness`
//! run loop and `AgentLane` trait.
//!
//! **Plan §2.3 refinement #2:** the TS `AgentHarness` is a stub shell (every
//! operation rejects with `HarnessNotImplemented`). The Rust port implements
//! the *real* run loop on top of [`rpi_agent::run_agent_loop`]. The TS file is
//! treated as the **type contract only**: the option shape, the `AgentLane`
//! interface, the outcome unions, and the defensive-copy contract (setters
//! clone inputs, getters return clones — mirroring TS `[...]`/`{...}`).
//!
//! What the v1 port implements (the rest is documented as deferred in
//! `docs/m5f-open-questions.md`):
//! - [`AgentHarness::create`] — reject when the session already has records
//!   (TS `create.restore` is not implemented either; we surface restore as a
//!   `HarnessError::Io` rejection rather than a panic).
//! - `prompt` (text + message overloads), `skill`, `prompt_from_template` —
//!   drive `run_agent_loop` over the lane's branch path, persist each new
//!   `AgentMessage` via the [`Session`] facade, emit `RunStart`/`RunEnd` on the
//!   [`HarnessEventBus`], and write `operation_started`/`operation_finished`
//!   `LaneRecord`s. Pre-run compaction is evaluated (`should_compact` →
//!   `prepare_compaction` + `compact`) and the resulting `Compaction` entry is
//!   persisted before the run proceeds.
//! - `compact` (explicit), `abort`, `record_usage`, `wait_for_idle`,
//!   `run_when_idle`, and the full set of cloned get/set accessors.
//! - `AgentLane` trait + lane-bound runners. Existing session lanes execute
//!   against their own branch while sharing harness configuration and events.
//!
//! Outcome mapping: `run_agent_loop` returns `Result<NewMessages, AgentError>`.
//! The terminal assistant message's `stop_reason` decides
//! `Completed`/`Aborted`/`Failed`; `Deferred` + a `deferred` handle produces
//! `RunOutcome::Suspended` (the operation stays open until `resume`). All
//! rejections are `Result<T, HarnessError>`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use rpi_agent::message::AgentMessage;
use rpi_agent::{
    run_agent_loop, run_agent_loop_from_assistant, AfterToolCall, AfterToolResults, AgentContext,
    AgentEmitter, AgentEvent, AgentLoopConfig, AgentTool, BeforeToolCall, ConvertToLlm, QueueMode,
    ShouldStopAfterTurnContext, StreamFn, TransformContext,
};
use rpi_ai::types::{
    AssistantMessage, Content, DeferredHandle, StopReason, ThinkingLevel, Usage, UserContent,
    UserMessage,
};
use rpi_ai::{Model, Provider as AiProvider};

use crate::compaction::{
    compact, estimate_context_tokens, prepare_compaction, should_compact, CompactionLlmOptions,
};
use crate::error::{SessionError, SessionErrorCode};
use crate::events::{HarnessEvent, HarnessEventBus, RunEndEvent, RunEndOutcome, RunStartEvent};
use crate::messages::{
    convert_to_llm as harness_convert_to_llm, create_compaction_summary_message,
};
use crate::prompt_templates::format_prompt_template_invocation;
use crate::result::{HarnessError, HarnessResult, OperationKind};
use crate::session::context::{
    build_session_context, ContextEntryTransform, CustomEntryContextMessageProjector,
    SessionContextBuildOptions,
};
use crate::session::session::Session;
use crate::session::types::{
    BranchBounds, CompactionReason, Entry, EntryOrder, EntryQuery, JsonValue, LaneRecord,
    OperationError, OperationFinishedRecord, OperationIntent, OperationOutcome,
    OperationStartedRecord, ProvisionedEntry, ProvisionedKind, QueueCancelledRecord,
    QueueEnqueuedRecord, QueueKind, RecordBase, RecordQuery, RetryPendingRecord, SessionStats,
    SessionTree, StepAttemptRecord, StepKind, ToolReplay, ToolStartedRecord, UsageCause,
    UsageRecord, WriteDeferredRecord,
};
use crate::skills::format_skill_invocation;
use crate::system_prompt::compose_system_prompt;
use crate::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessStreamOptions, CompactionSettings,
    DrivingMode, HarnessTool, HarnessToolExecution, PromptTemplate, RetryPolicy, Skill,
};
use crate::watcher::{watch_listener, HarnessWatcher};

// ===========================================================================
// Outcomes — mirror the TS RunOutcome / CompactionOutcome / NavigationOutcome
// unions (agent-harness.ts:89-103). Each carries the lane + terminal ids.
// ===========================================================================

/// `RunOutcome` (harness-level). Mirrors TS `RunOutcome`. `leaf_id`/`final_entry_id`
/// name the lane's leaf + final persisted entry; `final_message` is the terminal
/// assistant message. `Suspended` carries the provider deferred handle.
#[derive(Debug, Clone)]
pub enum HarnessRunOutcome {
    Completed {
        leaf_id: String,
        final_entry_id: String,
        final_message: rpi_ai::types::AssistantMessage,
    },
    Aborted {
        leaf_id: String,
        final_entry_id: String,
        final_message: rpi_ai::types::AssistantMessage,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
        final_entry_id: Option<String>,
        final_message: Option<rpi_ai::types::AssistantMessage>,
    },
    Suspended {
        leaf_id: String,
        final_entry_id: String,
        deferred: DeferredHandle,
    },
}

/// `CompactionOutcome`. Mirrors TS `CompactionOutcome`.
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    Completed {
        leaf_id: String,
        entry: Entry,
    },
    Declined {
        leaf_id: String,
    },
    Aborted {
        leaf_id: String,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
    },
}

/// `NavigationOutcome`. Mirrors TS `NavigationOutcome`. v1 only emits
/// `Declined` on a no-op navigation (the real branch navigation loop is
/// deferred).
#[derive(Debug, Clone)]
pub enum NavigationOutcome {
    Completed {
        new_leaf_id: Option<String>,
        summary_entry: Option<Entry>,
    },
    Declined {
        leaf_id: Option<String>,
    },
    Aborted {
        leaf_id: Option<String>,
    },
    Failed {
        leaf_id: Option<String>,
        error: OperationError,
    },
}

/// Result of a `prompt`/`skill`/`prompt_from_template` call. Mirrors TS `RunResult`
/// = `Result<{ runId } & RunOutcome, RunRejected>`.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub run_id: String,
    pub outcome: HarnessRunOutcome,
}

/// Result of an explicit `compact`. Mirrors TS `CompactionResult`.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub run_id: String,
    pub outcome: CompactionOutcome,
}

/// Result of `navigate_tree`. Mirrors TS `NavigationResult`.
#[derive(Debug, Clone)]
pub struct NavigationResult {
    pub run_id: String,
    pub outcome: NavigationOutcome,
}

/// Result of `steer`/`follow_up`/`next_run`. Mirrors TS `QueueResult`.
#[derive(Debug, Clone)]
pub struct QueueResult {
    pub entry_id: String,
}

/// Result of `abort`. Mirrors TS `AbortResult`.
#[derive(Debug, Clone)]
pub struct AbortResult {
    pub run_id: String,
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

/// Result of `record_usage`. Mirrors TS `RecordUsageResult` (`Result<void, Closed>`).
#[derive(Debug, Clone, Default)]
pub struct RecordUsageResult;

/// Result of `cancel_queued`. Mirrors TS `CancelQueuedResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelQueuedOutcome {
    Cancelled,
    AlreadyConsumed,
    AlreadyCleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelQueuedResult {
    pub outcome: CancelQueuedOutcome,
}

/// A snapshot of the queued (not-yet-consumed) user messages on one lane,
/// split by queue kind. Mirrors native pi's `getSteeringMessages()` /
/// `getFollowUpMessages()` — the payload the pending-messages display renders
/// (`Steering: <text>` / `Follow-up: <text>`) and the `app.message.dequeue`
/// action restores into the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuedMessages {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

impl QueuedMessages {
    /// True when nothing is queued on the lane.
    pub fn is_empty(&self) -> bool {
        self.steering.is_empty() && self.follow_up.is_empty()
    }
}

/// The display text of a queued message. Steer / follow-up entries are always
/// user content, so this is the text the user typed (image blocks are ignored,
/// matching the single-line pending row).
fn queued_message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => match &user.content {
            rpi_ai::types::UserContent::Text(text) => text.clone(),
            rpi_ai::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    rpi_ai::types::Content::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        _ => String::new(),
    }
}

// ===========================================================================
// Deferred-operation + inspection read models (TS `runtime/` surface). These
// back the `AgentHarness` methods `drive`/`resume`/`inspectExecution`/
// `getResult`/`snapshot`, which the coding-agent product layer consumes.
// ===========================================================================

/// Terminal/disposition state after driving a durable operation.
#[derive(Debug, Clone)]
pub enum DriveOutcome {
    /// The operation ran to a terminal run outcome.
    Completed(Box<RunResult>),
    /// The operation parked again on a fresh deferred handle.
    Suspended(DeferredHandle),
    /// There was nothing pending to drive on this lane.
    Idle,
}

/// Result of `drive(operationId)`. Mirrors the TS runtime drive result.
#[derive(Debug, Clone)]
pub struct DriveResult {
    pub operation_id: String,
    pub outcome: DriveOutcome,
}

/// The active-operation half of a [`LaneSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveOperationSnapshot {
    pub run_id: String,
    pub lane: String,
    pub kind: OperationKind,
}

/// A point-in-time read model of one lane. Mirrors the TS lane snapshot the
/// runtime/TUI renders from: the leaf pointer, the session name, rolled-up
/// stats, and (when a run is in flight) the active operation.
#[derive(Debug, Clone, Default)]
pub struct LaneSnapshot {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub name: Option<String>,
    pub stats: Option<SessionStats>,
    pub active: Option<ActiveOperationSnapshot>,
    /// Labels keyed by target entry id (mirrors the TS `labels` fact map).
    pub labels: std::collections::BTreeMap<String, String>,
}

/// A tool-execution record surfaced by `inspect_execution`. Unlike the live
/// `ToolExecutionComponent` (a UI concern), this is the durable view: which
/// entry persisted the call, and whether a result landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionInfo {
    pub tool_call_id: String,
    pub tool_name: String,
    /// The entry id the tool call was persisted under, when known.
    pub entry_id: Option<String>,
    /// The entry id of the matching tool result, when one exists.
    pub result_entry_id: Option<String>,
}

// ===========================================================================
// AgentLane trait — the per-lane surface (TS AgentLane interface, lines 271-303)
// ===========================================================================

/// Per-lane operations. Mirrors TS `AgentLane`. [`AgentHarness`] (the main lane)
/// implements the run loop; [`LaneHandle`] (non-main lanes) delegate reads to a
/// [`SessionTree`] view and reject run ops with [`HarnessError::InvalidLane`].
#[async_trait::async_trait]
pub trait AgentLane: Send + Sync {
    fn name(&self) -> &str;
    async fn get_leaf_id(&self) -> HarnessResult<Option<String>>;

    /// Whether recovery left an interrupted run that should be continued.
    ///
    /// The caller (the TUI) decides *when* to continue: driving a run has to
    /// happen somewhere the output is rendered, and the harness cannot know that.
    async fn has_pending_resume(&self) -> bool;

    /// Continue the interrupted run, if there is one to continue.
    ///
    /// `Ok(None)` means there was nothing to resume; the caller should carry on
    /// with a normal prompt.
    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>>;

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult>;
    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult>;
    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult>;
    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult>;
    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult>;

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult>;
    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        summarize: bool,
        custom_instructions: Option<&str>,
        label: Option<&str>,
    ) -> HarnessResult<NavigationResult>;

    async fn abort(&self) -> HarnessResult<AbortResult>;

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult>;

    /// Snapshot the lane's queued (not-yet-consumed) steering + follow-up user
    /// messages. Read-only; drives the pending-messages display. Mirrors native
    /// pi's `getSteeringMessages()` / `getFollowUpMessages()`.
    async fn queued_messages(&self) -> HarnessResult<QueuedMessages>;

    /// Remove and return every queued steering + follow-up message on the lane.
    /// Mirrors native pi's `clearQueue()` (the `app.message.dequeue` action
    /// restores the returned text into the editor).
    async fn clear_queue(&self) -> HarnessResult<QueuedMessages>;
    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult>;

    async fn wait_for_idle(&self) -> HarnessResult<()>;
    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()>;

    async fn get_model(&self) -> HarnessResult<Model>;
    async fn set_model(&self, model: Model) -> HarnessResult<()>;
    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel>;
    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()>;
    async fn get_active_tools(&self) -> HarnessResult<Vec<String>>;
    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()>;

    fn session_view(&self) -> Arc<dyn SessionTree>;

    // -- durable-session read/write surface (TS `AgentLane` additions).
    //    Non-generic so the trait stays dyn-compatible; `watch` lives on the
    //    concrete `AgentHarness` because it is generic over the callback. ----

    async fn get_tip_id(&self) -> HarnessResult<Option<String>>;
    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>>;
    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>>;
    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>>;
    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String>;
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String>;
    async fn get_name(&self) -> HarnessResult<Option<String>>;
    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()>;
    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>>;
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()>;
    async fn get_stats(&self) -> HarnessResult<SessionStats>;
    async fn snapshot(&self) -> HarnessResult<LaneSnapshot>;
    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>>;
    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>>;
    async fn accept(&self, entry_id: &str) -> HarnessResult<bool>;
    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool>;
    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult>;
    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult>;
}

// ===========================================================================
// HarnessInner - the shared mutable state behind the harness (model,
// thinking level, active tools, providers, run guard). Guarded by a Mutex so
// the AgentHarness (main lane) and LaneHandle (non-main) can share it.
// Defensive-copy: setters Clone, getters return clones.
// ===========================================================================

/// In-flight run guard. Held while a run/compaction operation is driving the
/// loop on `main`. The `signal` is a child of the harness root token; cancel it
/// to abort. `idle` fires when the guard is released.
struct ActiveRun {
    run_id: String,
    lane: String,
    signal: CancellationToken,
    idle: Arc<tokio::sync::Notify>,
    kind: OperationKind,
}

/// A queued user message keeps its public entry id until the agent loop drains
/// it. The loop only needs the message, while the harness API also needs the id
/// for cancellation and UI feedback.
#[derive(Clone)]
struct QueuedMessage {
    entry_id: String,
    lane: String,
    message: AgentMessage,
}

struct MessageQueue {
    mode: QueueMode,
    pending: VecDeque<QueuedMessage>,
}

type SharedMessageQueue = Arc<Mutex<MessageQueue>>;

impl MessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            pending: VecDeque::new(),
        }
    }

    /// Take every queued item for `lane`, keeping each item's id.
    ///
    /// The id matters: it is what the durable `queue_enqueued` record named, so
    /// the caller can record the consumption against it. Dropping the ids here
    /// (as this used to) is what would make a consumed message come back to life
    /// on the next start.
    fn drain_for_lane(&mut self, lane: &str) -> Vec<QueuedMessage> {
        let items: Vec<QueuedMessage> = match self.mode {
            QueueMode::All => {
                let mut selected = Vec::new();
                let mut retained = VecDeque::new();
                for item in self.pending.drain(..) {
                    if item.lane == lane {
                        selected.push(item);
                    } else {
                        retained.push_back(item);
                    }
                }
                self.pending = retained;
                selected
            }
            QueueMode::OneAtATime => {
                let Some(index) = self.pending.iter().position(|item| item.lane == lane) else {
                    return Vec::new();
                };
                self.pending.remove(index).into_iter().collect()
            }
        };
        items
    }

    /// Remove and return every queued item for `lane`, regardless of
    /// [`QueueMode`]. Used by the `app.message.dequeue` action (restore all
    /// queued messages into the editor), which also cancels them durably.
    fn take_items_for_lane(&mut self, lane: &str) -> Vec<QueuedMessage> {
        let mut selected = Vec::new();
        let mut retained = VecDeque::new();
        for item in self.pending.drain(..) {
            if item.lane == lane {
                selected.push(item);
            } else {
                retained.push_back(item);
            }
        }
        self.pending = retained;
        selected
    }

    fn remove(&mut self, entry_id: &str) -> bool {
        let before = self.pending.len();
        self.pending.retain(|item| item.entry_id != entry_id);
        self.pending.len() != before
    }

    /// The queued message texts for `lane`, in queue order, without touching
    /// the queue. Used by the pending-messages display.
    fn peek_for_lane(&self, lane: &str) -> Vec<String> {
        self.pending
            .iter()
            .filter(|item| item.lane == lane)
            .map(|item| queued_message_text(&item.message))
            .collect()
    }
}

/// Releases the in-memory active-run slot on every exit path, including an
/// early `?` caused by session I/O or configuration errors.
struct ActiveRunLease<'a> {
    harness: &'a AgentHarness,
}

impl Drop for ActiveRunLease<'_> {
    fn drop(&mut self) {
        self.harness.release_run();
    }
}

/// A run that recovery repaired and that can be continued without the user
/// restating their request.
///
/// Native pi resumes the interrupted operation in place (`driveOperation`
/// re-enters at `state.at`). rpi's run has no durable re-entry point, so the
/// equivalent is to start a run with **no new prompt**: the branch already
/// ends with the resolved tool results, so the next thing the loop does is
/// exactly what the interrupted run would have done next — ask the provider
/// to continue.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingResume {
    /// The run that just died. Names the notice and the log entry.
    pub interrupted_run_id: String,
    /// The run the whole resume chain starts from. Every resumed run points back
    /// at this one, so the attempt count is a property of the chain rather than
    /// of whichever run happens to have died last.
    pub chain_origin_run_id: String,
    /// How many resumes this chain has already had. Bounds a run that keeps dying
    /// from restarting the process's work forever.
    pub attempt: u32,
}

/// How many times one interrupted run may be resumed automatically.
///
/// Without a bound, a run that crashes deterministically would be re-driven on
/// every startup: an unbounded loop that spends tokens and repeats side
/// effects while the user only ever sees a crash.
pub const MAX_RESUME_ATTEMPTS: u32 = 3;

/// Shared mutable state. Cloned out (field-by-field) at the start of each
/// operation so the loop sees a consistent snapshot; setters replace fields
/// in place.
struct HarnessInner {
    model: Model,
    thinking_level: ThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<HarnessTool>,
    resources: AgentHarnessResources,
    stream_options: AgentHarnessStreamOptions,
    retry: RetryPolicy,
    compaction: CompactionSettings,
    /// v1 leaves the harness in `Automatic` driving mode; the field is kept so
    /// the contract mirrors TS `AgentHarnessOptions.drive`.
    #[allow(dead_code)]
    driving: DrivingMode,
    tool_execution: HarnessToolExecution,
    system_prompt: Option<String>,
    /// Provider registry; the harness resolves by `model.provider`.
    models: Vec<Arc<dyn AiProvider>>,
    /// Optional caller-supplied converter; if `None` the harness-level
    /// `convert_to_llm` is used.
    to_provider_messages: Option<ConvertToLlm>,
    entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
    /// Caller-supplied context-entry transforms (B3b). Forwarded into
    /// `SessionContextBuildOptions.entry_transforms` at the `build_opts` site.
    entry_transforms: Vec<ContextEntryTransform>,
    /// Optional emitter override; see [`AgentHarnessOptions::agent_emitter`].
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
    /// Set by `create` when recovery repaired a run that can be continued, and
    /// consumed by [`AgentHarness::resume_pending`]. See [`PendingResume`].
    pending_resume: Option<PendingResume>,
    // ---- B3b: the three exists-but-`None` AgentLoopConfig hooks -----------
    before_tool_call: Option<BeforeToolCall>,
    after_tool_call: Option<AfterToolCall>,
    transform_context: Option<TransformContext>,
    /// B4: per-call provider hooks fired inside the `StreamFn` closure.
    provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    closed: bool,
    active_run: Option<ActiveRun>,
    steering_queue: SharedMessageQueue,
    follow_up_queue: SharedMessageQueue,
    next_run_queue: SharedMessageQueue,
}

// ===========================================================================
// AgentHarness - a lane-bound runner. Drives run_agent_loop over its session
// branch path, persists entries/records, emits RunStart/RunEnd.
// ===========================================================================

/// The harness created by [`AgentHarness::create`] is bound to `"main"`.
/// [`AgentHarness::lane`] creates another lane-bound runner. Cheap to clone;
/// clones share the same session, event bus, and inner state.
#[derive(Clone)]
pub struct AgentHarness {
    session: Session,
    inner: Arc<Mutex<HarnessInner>>,
    bus: HarnessEventBus,
    lane: String,
}

impl AgentHarness {
    /// Construct from options. Mirrors TS `AgentHarness.create`: when the
    /// session already has records (a prior operation) the default is to
    /// reject (TS throws `HarnessNotImplemented("create.restore")`). The
    /// restore path (`--continue`/`--resume`/`--session`) sets
    /// `allow_existing_session` so the harness loads the existing transcript
    /// and continues appending.
    pub async fn create(options: AgentHarnessOptions) -> HarnessResult<Self> {
        if !options.allow_existing_session {
            let existing = options
                .session
                .find_records(&RecordQuery {
                    limit: Some(1),
                    ..Default::default()
                })
                .await
                .map_err(session_to_harness_err)?;
            if !existing.is_empty() {
                return Err(HarnessError::io(
                    "create.restore is not implemented: session already has records",
                ));
            }
        }

        // Recovery repaired whatever the previous process left behind. If that
        // repair left the branch mid-loop (a tool result as the tip), the run can
        // be continued instead of waiting for the user to restate the request.
        // Anything still queued when the previous process died is restored here.
        // For a fresh session this is empty, and for the non-restore path there is
        // nothing to read.
        let rebuilt_queue = if options.allow_existing_session {
            Self::rebuild_queues(&options.session, "main").await
        } else {
            Vec::new()
        };

        let mut pending_resume = None;
        if options.allow_existing_session {
            let interrupted = options
                .session
                .find_open_operations("main", Some(1))
                .await
                .map_err(session_to_harness_err)?
                .first()
                .cloned();
            // A run that died waiting to retry is continued too, and its failed
            // attempt is not salvaged. Decided before recovery so the salvage and
            // the resume gate agree.
            let died_awaiting_retry = match &interrupted {
                Some(dead) => Self::died_awaiting_retry(&options.session, &dead.base.id).await,
                None => false,
            };
            Self::finish_interrupted_operation(
                &options.session,
                "main",
                &options.tools,
                &CancellationToken::new(),
                !died_awaiting_retry,
            )
            .await?;
            if let Some(dead) = interrupted {
                let died_before_any_output =
                    Self::tip_is_user_message(&options.session, "main").await;
                if died_awaiting_retry
                    || died_before_any_output
                    || Self::lane_is_resumable(&options.session, "main").await
                {
                    let (chain_origin_run_id, attempt) = Self::resume_chain(&dead);
                    pending_resume = Some(PendingResume {
                        interrupted_run_id: dead.base.id.clone(),
                        chain_origin_run_id,
                        attempt,
                    });
                }
            }
        }

        let inner = HarnessInner {
            model: options.model,
            thinking_level: options.thinking_level,
            active_tool_names: options.active_tool_names,
            tools: options.tools,
            resources: options.resources,
            stream_options: options.stream_options,
            retry: options.retry,
            compaction: options.compaction,
            driving: options.drive,
            tool_execution: options.tool_execution,
            system_prompt: options.system_prompt,
            models: options.models,
            to_provider_messages: options.to_provider_messages,
            entry_projectors: options.entry_projectors,
            entry_transforms: options.entry_transforms,
            agent_emitter: options.agent_emitter,
            pending_resume,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            transform_context: options.transform_context,
            provider_hooks: options.provider_hooks,
            closed: false,
            active_run: None,
            steering_queue: Arc::new(Mutex::new(MessageQueue::new(options.steering_mode))),
            follow_up_queue: Arc::new(Mutex::new(MessageQueue::new(options.follow_up_mode))),
            next_run_queue: Arc::new(Mutex::new(MessageQueue::new(QueueMode::All))),
        };
        for (kind, item) in rebuilt_queue {
            let queue = match kind {
                QueueKind::Steer => &inner.steering_queue,
                QueueKind::FollowUp => &inner.follow_up_queue,
                QueueKind::NextRun => &inner.next_run_queue,
            };
            queue.lock().unwrap().pending.push_back(item);
        }

        Ok(Self {
            session: options.session,
            inner: Arc::new(Mutex::new(inner)),
            bus: HarnessEventBus::new(),
            lane: "main".to_string(),
        })
    }

    /// Whether the run died waiting to retry a retryable failure.
    ///
    /// Reads the durable `retry_pending` record: the frames cannot answer this
    /// (a failed attempt leaves no terminal frame, so its reduced message looks
    /// merely `pending`), and a failed attempt leaves no other trace because a
    /// `step_attempt` is only written when a message commits.
    ///
    /// "Waiting" means the attempt it scheduled never landed: once that attempt's
    /// entry exists, the retry succeeded and there is nothing to redo.
    async fn died_awaiting_retry(session: &Session, run_id: &str) -> bool {
        let records = match session
            .find_records(&RecordQuery {
                record_type: Some("retry_pending"),
                run_id: Some(run_id.to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            Ok(records) => records,
            Err(_) => return false,
        };
        let entries = match session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        let landed: std::collections::BTreeSet<&str> =
            entries.iter().map(|entry| entry.id()).collect();
        records.into_iter().any(|record| match record {
            LaneRecord::RetryPending(pending) => !landed.contains(pending.result_entry_id.as_str()),
            _ => false,
        })
    }

    /// Whether the lane's branch ends in a state a run can continue from.
    ///
    /// True when the last entry is a tool result: a run that reaches its tool
    /// results always goes on to ask the provider for the next assistant message,
    /// so a tool result as the tip means the run died before it could. A tool that
    /// set `terminate` legitimately ends the run there, so it is excluded.
    async fn lane_is_resumable(session: &Session, lane: &str) -> bool {
        // Lane-scoped branch read (the session-wide one is hardcoded to "main").
        let entries = match session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        match entries.last() {
            Some(Entry::Message(message)) => {
                matches!(message.message, AgentMessage::ToolResult(_))
                    && message.terminate != Some(true)
            }
            _ => false,
        }
    }

    /// The resume chain a run belongs to: `(origin run id, resumes so far)`.
    ///
    /// Read from the run's own durable intent. The count has to be durable,
    /// because the crash that makes it matter also destroys any in-memory counter
    /// that could have held it — and a run that crashes deterministically would
    /// otherwise be re-driven on every startup, spending tokens and repeating side
    /// effects while the user only ever sees a crash.
    fn resume_chain(started: &OperationStartedRecord) -> (String, u32) {
        match &started.intent {
            OperationIntent::Run {
                resume_data: Some(data),
                ..
            } => {
                let origin = data
                    .get("resumedFrom")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| started.base.id.clone());
                let prior = data
                    .get("resumeAttempt")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as u32;
                (origin, prior)
            }
            // A run with no resume data is the chain's origin.
            _ => (started.base.id.clone(), 0),
        }
    }

    /// The run recovery repaired and that is waiting to be continued, if any.
    pub fn pending_resume(&self) -> Option<PendingResume> {
        self.inner
            .lock()
            .unwrap()
            .pending_resume
            .clone()
            .filter(|pending| pending.attempt < MAX_RESUME_ATTEMPTS)
    }

    /// Continue the interrupted run, if recovery left one to continue.
    ///
    /// Returns `Ok(None)` when there is nothing to resume, when the resume budget
    /// is exhausted, or when the branch is not in a continuable state — in all of
    /// those cases the caller should simply carry on with a normal prompt.
    ///
    /// This is a *product-visible* action: it spends tokens and runs tools without
    /// the user asking a question, which is why it is bounded and why it announces
    /// itself in the transcript.
    pub async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        let Some(pending) = self.inner.lock().unwrap().pending_resume.take() else {
            return Ok(None);
        };
        if pending.attempt >= MAX_RESUME_ATTEMPTS {
            tracing::warn!(
                run_id = %pending.interrupted_run_id,
                attempts = pending.attempt,
                "not resuming the interrupted run again: its resume budget is exhausted"
            );
            self.record_resume_notice(&format!(
                "The interrupted run was not resumed again: it has already been resumed {} times, \
                 and resuming it again could repeat its side effects without making progress.",
                pending.attempt
            ))
            .await;
            return Ok(None);
        }
        // The one hard requirement is that the continuation is a *valid request*:
        // a trailing assistant message is rejected by providers (and by
        // `run_agent_loop_continue`), so that case must not be resumed even if a
        // pending resume was recorded. Whether a resume is *offered* in the first
        // place is decided in `create`; this is the last-line check.
        if Self::tip_is_assistant(&self.session, &self.lane).await {
            tracing::warn!(
                lane = %self.lane,
                "not continuing the interrupted run: the branch ends at an assistant message"
            );
            return Ok(None);
        }
        self.record_resume_notice(&format!(
            "Continuing the run that was interrupted (resume {} of {}).",
            pending.attempt + 1,
            MAX_RESUME_ATTEMPTS
        ))
        .await;
        // An empty prompt is exactly a continuation: the loop's next action is the
        // provider call the interrupted run never reached.
        self.run_core(Vec::new()).await.map(Some)
    }

    /// Record the user-visible "why is a run starting by itself" notice.
    async fn record_resume_notice(&self, text: &str) {
        let notice = crate::messages::create_custom_message(
            "runResume",
            UserContent::Text(text.to_string()),
            true,
            None,
            now_ms(),
        );
        if let Err(error) = self.session.view(&self.lane).append_message(notice).await {
            tracing::warn!(
                lane = %self.lane,
                %error,
                "could not record the run-resume notice"
            );
        }
    }
    /// The event bus. Callers register `RunStart`/`RunEnd` listeners here.
    pub fn events(&self) -> &HarnessEventBus {
        &self.bus
    }

    /// The session facade (for direct entry/record queries the harness API
    /// doesn't surface).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Return a runner bound to a session lane. All lanes share configuration,
    /// cancellation, and the event bus, while persistence and context use the
    /// selected lane's branch. The lane must already exist in the session.
    pub fn lane(&self, name: &str) -> Arc<dyn AgentLane> {
        Arc::new(Self {
            session: self.session.clone(),
            inner: Arc::clone(&self.inner),
            bus: self.bus.clone(),
            lane: name.to_string(),
        })
    }

    /// The harness's bound lane name ("main" for a top-level harness).
    pub fn lane_name(&self) -> &str {
        &self.lane
    }

    // -- durable-session read/write surface (TS `AgentHarness` getTipId /
    //    findEntries / appendMessage / …). Each delegates to the lane-scoped
    //    `SessionTree` view so non-main lanes behave identically. -----------

    /// The lane-scoped session view backing the read/write surface below.
    pub fn view(&self) -> Arc<dyn SessionTree> {
        self.session.view(&self.lane)
    }

    /// `getTipId()` — the lane's current leaf entry id. Mirrors TS
    /// `AgentHarness.getTipId`.
    pub async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        self.view()
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntries(query)` — query the lane's entries. Mirrors TS
    /// `AgentHarness.findEntries`.
    pub async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        self.view()
            .find_entries(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntry(query)` — first matching entry on the lane. Mirrors TS
    /// `AgentHarness.findEntry`.
    pub async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        self.view()
            .find_entry(query)
            .await
            .map_err(session_to_harness_err)
    }

    /// `findEntriesOnBranch(query, bounds)` — branch path of the lane.
    pub async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> HarnessResult<Vec<Entry>> {
        self.view()
            .find_entries_on_branch(query, bounds)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getEntry(id)` — look up a single entry by id. Mirrors TS
    /// `AgentHarness.getEntry`.
    pub async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        self.view()
            .get_entry(id)
            .await
            .map_err(session_to_harness_err)
    }

    /// `appendMessage(message)` — persist an out-of-band message entry on the
    /// lane without running the agent. Mirrors TS `AgentHarness.appendMessage`.
    pub async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        self.view()
            .append_message(message)
            .await
            .map_err(session_to_harness_err)
    }

    /// `appendCustomEntry(customType, data?)` — persist a custom entry on the
    /// lane. Mirrors TS `AgentHarness.appendCustomEntry`.
    pub async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        self.view()
            .append_custom_entry(custom_type, data)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getName()` — the session name. Mirrors TS `AgentHarness.getName`.
    pub async fn get_name(&self) -> HarnessResult<Option<String>> {
        self.view().get_name().await.map_err(session_to_harness_err)
    }

    /// `setName(name)` — set/clear the session name. Mirrors TS
    /// `AgentHarness.setName`.
    pub async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        self.view()
            .set_name(name)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getLabel(targetId)` — the label attached to an entry, when set.
    pub async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        self.view()
            .get_label(target_id)
            .await
            .map_err(session_to_harness_err)
    }

    /// `setLabel(targetId, label)` — attach/clear an entry label.
    pub async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        self.view()
            .set_label(target_id, label)
            .await
            .map_err(session_to_harness_err)
    }

    /// `getStats()` — rolled-up token/cost/message counters.
    pub async fn get_stats(&self) -> HarnessResult<SessionStats> {
        self.view()
            .get_stats()
            .await
            .map_err(session_to_harness_err)
    }

    /// `snapshot()` — the lane read model (leaf, name, stats, active op).
    /// Mirrors the TS runtime lane snapshot. `labels` folds in every labelled
    /// entry the lane can see.
    pub async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        let view = self.view();
        let leaf_id = view.get_leaf_id().await.map_err(session_to_harness_err)?;
        let stats = view.get_stats().await.ok();
        let name = view.get_name().await.ok().flatten();
        let active = self.active_operation_snapshot();
        let labels = self.lane_labels(&view).await;
        Ok(LaneSnapshot {
            lane: self.lane.clone(),
            leaf_id,
            name,
            stats,
            active,
            labels,
        })
    }

    /// `watchSession()` — watch this lane's event stream. Mirrors TS
    /// `AgentHarness.watchSession`: the listener receives every event on the
    /// harness bus. The returned [`HarnessWatcher`] unsubscribes on drop.
    pub fn watch_session<F>(&self, callback: F) -> HarnessWatcher
    where
        F: Fn(&HarnessEvent) + Send + Sync + 'static,
    {
        let mut handle = self.bus.watch(|| ());
        handle.start(watch_listener(callback));
        HarnessWatcher::new(handle)
    }

    /// `watch()` — harness-level alias of [`Self::watch_session`]. Mirrors the
    /// TS `AgentHarness.watch` entry point.
    pub fn watch<F>(&self, callback: F) -> HarnessWatcher
    where
        F: Fn(&HarnessEvent) + Send + Sync + 'static,
    {
        self.watch_session(callback)
    }

    /// `inspectExecution(toolCallId)` — locate the persisted tool-call entry
    /// (and its matching result, when present) for a tool call id. Mirrors the
    /// TS runtime `inspectExecution` read model.
    pub async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        for entry in self.all_lane_entries().await? {
            let Entry::Message(me) = &entry else {
                continue;
            };
            match &me.message {
                AgentMessage::Assistant(a) => {
                    for block in &a.content {
                        if let Content::ToolCall(tc) = block {
                            if tc.id == tool_call_id {
                                return Ok(Some(ToolExecutionInfo {
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    entry_id: Some(entry.id().to_string()),
                                    result_entry_id: None,
                                }));
                            }
                        }
                    }
                }
                AgentMessage::ToolResult(tr) => {
                    if tr.tool_call_id == tool_call_id {
                        return Ok(Some(ToolExecutionInfo {
                            tool_call_id: tr.tool_call_id.clone(),
                            tool_name: tr.tool_name.clone(),
                            entry_id: None,
                            result_entry_id: Some(entry.id().to_string()),
                        }));
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// `getResult(runId)` — the terminal result of a completed run, read back
    /// from the durable `operation_finished` record. Mirrors TS
    /// `AgentHarness.getResult`.
    pub async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        let records = self
            .session
            .find_records(&RecordQuery {
                run_id: Some(run_id.to_string()),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        for record in records {
            let LaneRecord::OperationFinished(fin) = &record else {
                continue;
            };
            let leaf_id = self
                .view()
                .get_leaf_id()
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            let final_message = self.last_assistant_message().await.unwrap_or_else(|| {
                rpi_ai::types::AssistantMessage::empty(rpi_ai::Api::AnthropicMessages, "", "", 0)
            });
            let outcome = match fin.outcome {
                OperationOutcome::Completed => HarnessRunOutcome::Completed {
                    final_entry_id: leaf_id.clone(),
                    leaf_id,
                    final_message,
                },
                OperationOutcome::Aborted => HarnessRunOutcome::Aborted {
                    final_entry_id: leaf_id.clone(),
                    leaf_id,
                    final_message,
                },
                OperationOutcome::Failed => HarnessRunOutcome::Failed {
                    leaf_id: leaf_id.clone(),
                    error: fin.error.clone().unwrap_or_else(|| OperationError {
                        code: "failed".into(),
                        message: "run failed".into(),
                    }),
                    final_entry_id: Some(leaf_id),
                    final_message: Some(final_message),
                },
                // `Declined` is a no-op outcome — it never produced a result.
                OperationOutcome::Declined => continue,
            };
            return Ok(Some(RunResult {
                run_id: run_id.to_string(),
                outcome,
            }));
        }
        Ok(None)
    }

    /// `accept(entryId)` — commit a pending/steering entry as a durable lane
    /// message. Returns `true` when the entry existed and was accepted.
    /// Mirrors the TS runtime accept step.
    pub async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        let Some(entry) = self.get_entry(entry_id).await? else {
            return Ok(false);
        };
        let Entry::Message(me) = entry else {
            return Ok(false);
        };
        self.append_message(me.message).await?;
        Ok(true)
    }

    /// `requestAbort(runId)` — abort the named run when it is the active one.
    /// Returns `true` when the abort was delivered. Mirrors the TS runtime
    /// `requestAbort`.
    pub async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        let matches_active = {
            let inner = self.inner.lock().unwrap();
            inner
                .active_run
                .as_ref()
                .map(|a| a.run_id == run_id)
                .unwrap_or(false)
        };
        if !matches_active {
            return Ok(false);
        }
        AgentLane::abort(self).await?;
        Ok(true)
    }

    /// `drive(operationId)` — drive a parked deferred operation to its next
    /// disposition. When no suspended operation matches, reports
    /// [`DriveOutcome::Idle`]. Mirrors the TS runtime `drive` step.
    pub async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        let Some(deferred) = self.find_deferred_handle(operation_id).await? else {
            return Ok(DriveResult {
                operation_id: operation_id.to_string(),
                outcome: DriveOutcome::Idle,
            });
        };
        let result = self.resume_deferred(deferred).await?;
        Ok(DriveResult {
            operation_id: operation_id.to_string(),
            outcome: match result.outcome {
                HarnessRunOutcome::Suspended { deferred, .. } => DriveOutcome::Suspended(deferred),
                _ => DriveOutcome::Completed(Box::new(result)),
            },
        })
    }

    /// `resume(suspendedId)` — resume a parked deferred operation.
    /// `suspended_id` is either the deferred handle id or the entry id of the
    /// suspended assistant message. Mirrors the TS `resume` operation.
    pub async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        let Some(deferred) = self.find_deferred_handle(suspended_id).await? else {
            return Err(HarnessError::NothingToResume {
                lane: self.lane.clone(),
                message: format!("no suspended operation matches '{suspended_id}'"),
            });
        };
        self.resume_deferred(deferred).await
    }

    // -- internal helpers for the surface above ------------------------------

    fn active_operation_snapshot(&self) -> Option<ActiveOperationSnapshot> {
        let inner = self.inner.lock().unwrap();
        inner.active_run.as_ref().map(|a| ActiveOperationSnapshot {
            run_id: a.run_id.clone(),
            lane: a.lane.clone(),
            kind: a.kind,
        })
    }

    /// Every entry visible to the lane's view (session-wide, oldest first).
    async fn all_lane_entries(&self) -> HarnessResult<Vec<Entry>> {
        self.view()
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)
    }

    async fn lane_labels(
        &self,
        view: &Arc<dyn SessionTree>,
    ) -> std::collections::BTreeMap<String, String> {
        let mut labels = std::collections::BTreeMap::new();
        if let Ok(entries) = view
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            for entry in entries {
                if let Ok(Some(label)) = view.get_label(entry.id()).await {
                    labels.insert(entry.id().to_string(), label);
                }
            }
        }
        labels
    }

    async fn last_assistant_message(&self) -> Option<rpi_ai::types::AssistantMessage> {
        let entries = self.all_lane_entries().await.ok()?;
        let mut last = None;
        for entry in entries {
            if let Entry::Message(me) = entry {
                if let AgentMessage::Assistant(a) = me.message {
                    last = Some(*a);
                }
            }
        }
        last
    }

    /// Find a parked deferred handle by deferred-id or by the entry id of the
    /// suspended assistant message that carries it.
    async fn find_deferred_handle(
        &self,
        suspended_id: &str,
    ) -> HarnessResult<Option<DeferredHandle>> {
        for entry in self.all_lane_entries().await? {
            let Entry::Message(me) = &entry else {
                continue;
            };
            let AgentMessage::Assistant(a) = &me.message else {
                continue;
            };
            let Some(deferred) = &a.deferred else {
                continue;
            };
            if deferred.id == suspended_id || entry.id() == suspended_id {
                return Ok(Some(deferred.clone()));
            }
        }
        Ok(None)
    }

    /// Drive a reconstructed deferred handle to a terminal outcome. Kept
    /// separate so `drive`/`resume` share one path.
    ///
    /// Mirrors native pi's `readDeferredSourceHandle` + `publishResponse`: poll
    /// the provider, record what it returned, and then either stay suspended (the
    /// provider handed back another handle) or carry on with the run. "Carry on"
    /// means the whole reason this is not a plain continuation: a polled assistant
    /// message arrives from outside the loop, so its tool calls have **not** been
    /// executed, and the next provider request would be rejected while they are
    /// unanswered — hence the run is entered *from that assistant*.
    async fn resume_deferred(&self, deferred: DeferredHandle) -> HarnessResult<RunResult> {
        let snap = self.snapshot_config()?;
        // The operation the previous process left parked, if any: it stays open
        // while suspended, so it is the lane's open operation.
        let parked_run_id = self
            .session
            .find_open_operations(&self.lane, Some(1))
            .await
            .map_err(session_to_harness_err)?
            .first()
            .map(|operation| operation.base.id.clone());

        let provider = snap
            .models
            .iter()
            .find(|provider| provider.id() == deferred.provider)
            .cloned()
            .ok_or_else(|| {
                HarnessError::agent(format!(
                    "no provider '{}' is registered for the deferred handle the run parked on",
                    deferred.provider
                ))
            })?;
        let Some(capability) = provider.deferred() else {
            // Not silently "finished": a provider that cannot poll cannot answer,
            // and pretending otherwise would record a result nobody produced.
            return Err(HarnessError::not_implemented(
                "resume_deferred: this provider has no long-poll continuation",
            ));
        };
        let model = provider
            .models()
            .iter()
            .find(|model| model.id == deferred.model_id)
            .cloned()
            .unwrap_or_else(|| snap.model.clone());
        let options = self.deferred_stream_options(&snap);

        let message = capability
            .stream_deferred(&model, &deferred, &options)
            .await
            .result()
            .await
            .map_err(|error| {
                HarnessError::agent(format!("the deferred poll produced no message: {error}"))
            })?;

        // Record the polled message: it is real history (the model wrote it), and
        // a *new* handle it carries has to be discoverable, which is what
        // `find_deferred_handle` reads.
        let entry_id = self
            .session
            .view(&self.lane)
            .append_message(AgentMessage::Assistant(Box::new(message.clone())))
            .await
            .map_err(session_to_harness_err)?;
        let leaf_id = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?
            .unwrap_or_else(|| entry_id.clone());

        // Still not ready: stay suspended. The operation stays open, so the next
        // start (or the next `drive`) polls the new handle in turn.
        if message.stop_reason == StopReason::Deferred {
            return Ok(RunResult {
                run_id: parked_run_id.unwrap_or_default(),
                outcome: HarnessRunOutcome::Suspended {
                    leaf_id,
                    final_entry_id: entry_id,
                    deferred: message.deferred.clone().unwrap_or(deferred),
                },
            });
        }

        // Ready: the parked operation is done — the poll answered it.
        if let Some(run_id) = &parked_run_id {
            self.write_operation_finished(run_id, OperationOutcome::Completed, None)
                .await?;
        }

        // Anything left to do? A polled message that carries tool calls needs them
        // executed before the next provider request.
        let has_tool_calls = message
            .content
            .iter()
            .any(|block| matches!(block, Content::ToolCall(_)));
        if has_tool_calls {
            return self.run_core_from_assistant().await;
        }

        Ok(RunResult {
            run_id: parked_run_id.unwrap_or_default(),
            outcome: HarnessRunOutcome::Completed {
                leaf_id,
                final_entry_id: entry_id,
                final_message: message,
            },
        })
    }

    /// The options a deferred poll runs with. Built from the harness snapshot so a
    /// poll sees the same timeout/thinking settings as an ordinary request; it
    /// gets a fresh cancellation token because no run is in flight while parked.
    fn deferred_stream_options(&self, snap: &ConfigSnapshot) -> rpi_ai::SimpleStreamOptions {
        rpi_ai::SimpleStreamOptions {
            timeout: snap.stream_options.timeout,
            session_id: None,
            signal: CancellationToken::new(),
            reasoning: match snap.thinking_level {
                ThinkingLevel::Off => None,
                other => Some(other),
            },
            ..Default::default()
        }
    }

    // -- harness-level config accessors (TS `AgentHarness` class surface, not
    //    on the `AgentLane` interface). Defensive-copy: setters Clone/move in,
    //    getters return clones. ---------------------------------------------

    /// `getTools()`. Mirrors TS `AgentHarness.getTools` — returns a defensive
    /// clone of the tool registry.
    pub async fn get_tools(&self) -> HarnessResult<Vec<HarnessTool>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.tools.clone())
    }

    /// `setTools(tools, activeNames?)`. Mirrors TS `AgentHarness.setTools` —
    /// replaces the registry and (when `active_names` is `None`) resets
    /// `active_tool_names` to every tool's schema name. Inputs are moved in
    /// (Rust ownership already isolates them from the caller).
    pub async fn set_tools(
        &self,
        tools: Vec<HarnessTool>,
        active_names: Option<Vec<String>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        let active = active_names
            .unwrap_or_else(|| tools.iter().map(|t| t.tool.schema().name.clone()).collect());
        inner.tools = tools;
        inner.active_tool_names = active;
        Ok(())
    }

    /// `close()`. Mirrors TS `AgentHarness.close` — flips the closed flag so
    /// every subsequent operation rejects with [`HarnessError::Closed`]. An
    /// in-flight run is not interrupted (its guard holds its own signal); the
    /// flag gates new operations.
    pub async fn close(&self) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        Ok(())
    }

    /// Swap the harness's durable session facade (TUI `/session` hot-switch).
    /// `run_core` re-reads the session on every run (branch path, leaf id,
    /// context build), so replacing `inner.session` makes the next run
    /// continue in the new session file — no harness rebuild, no lane/event
    /// wiring churn. The caller is responsible for aborting any in-flight run
    /// first.
    pub async fn set_session(&self, session: Session) -> HarnessResult<()> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        drop(inner);
        // Swap the durable backing on the shared facade — every lane handle /
        // config snapshot observes the new storage immediately.
        self.session.set_storage(session.storage());
        Ok(())
    }

    /// `getResources()`. Mirrors TS `AgentHarness.getResources`
    /// (`agent-harness.ts:460-464`) — returns a defensive clone of the
    /// resources (skills + prompt-templates) the harness was built with.
    /// Callers (e.g. the interactive TUI's `/`-autocomplete, which lists
    /// prompt-template names alongside built-in slash commands) read the
    /// discovered set through this accessor without touching the lock-held
    /// inner directly.
    pub async fn get_resources(&self) -> HarnessResult<AgentHarnessResources> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.resources.clone())
    }

    /// `getSystemPrompt()`. Mirrors TS `AgentHarness.getSystemPrompt` — a
    /// defensive clone of the composed base system prompt the harness was built
    /// with (base + append + context; the `<available_skills>` listing is added
    /// at run time from the resource skills, so this returns the *base* prompt
    /// the plugin's `runtime_action(GetSystemPrompt)` sees). B5a: backs the
    /// [`rpi_extensions::RuntimeActionHost::get_system_prompt`] action.
    pub async fn get_system_prompt(&self) -> HarnessResult<Option<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.system_prompt.clone())
    }

    // ---- B5d: reload mutation setters -------------------------------------
    //
    // `/reload` re-runs skill/prompt/context/SYSTEM discovery and must push the
    // rebuilt state into a *live* harness without rebuilding it (rebuilding
    // would tear down the session/lane/event wiring). These setters mirror
    // `set_tools`/`set_session`: take the lock, reject if closed, assign, drop.
    // The next `run_core` snapshots the new values via `snapshot_config`, so a
    // mutation here takes effect on the NEXT run — exactly the contract pi's
    // `/reload` offers (in-flight runs finish on the old config; the reloaded
    // resources apply to the subsequent turn).
    //
    // All five are `pub async` (uniform with the existing setters) even though
    // the bodies are sync; the `async` keeps the call sites uniform with
    // `set_tools`/`set_session` and leaves room for future emission (e.g. a
    // `ResourcesChanged` harness event) without an API break.

    /// `setSystemPrompt(prompt)`. Replace the composed base system prompt the
    /// harness was built with. B5d `/reload` calls this after recomposing
    /// (base + context + append); the next run's `snapshot_config` picks it up.
    pub async fn set_system_prompt(&self, prompt: Option<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.system_prompt = prompt;
        Ok(())
    }

    /// `setResources(resources)`. Replace the skills + prompt-templates the
    /// harness advertises (TUI `/context` listing, `<available_skills>` gate).
    pub async fn set_resources(&self, resources: AgentHarnessResources) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.resources = resources;
        Ok(())
    }
    /// `setAgentEmitter(emitter)`. Replace the emitter override (B3a's
    /// `ExtensionEmitter` fans `AgentEvent`s to plugin `on()` handlers). On
    /// `/reload` the old emitter is dropped (unsubscribing from the broadcast)
    /// and a fresh one over the reloaded registry takes its place.
    pub async fn set_agent_emitter(
        &self,
        emitter: Option<Arc<dyn AgentEmitter>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.agent_emitter = emitter;
        Ok(())
    }

    /// `setModels(models)`. Replace the provider registry. B5d `/reload`
    /// rebuilds `vec![gateway] + PluggableProvider::from_session(&new_session)`
    /// and installs it here so the next run resolves provider plugins from the
    /// reloaded registry (the old `ActionBridge`/`ExtensionSession` are
    /// invalidated; their `PluggableProvider`s reject calls via the staleness
    /// guard).
    pub async fn set_models(&self, models: Vec<Arc<dyn AiProvider>>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.models = models;
        Ok(())
    }

    /// `setProviderHooks(hooks)`. Replace the per-call provider hooks (B4's
    /// `ExtensionProviderHooks` fires `before_request`/`after_response` inside
    /// the `StreamFn` closure). `/reload` builds a fresh hooks adapter over the
    /// reloaded registry snapshot.
    pub async fn set_provider_hooks(
        &self,
        hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    ) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.provider_hooks = hooks;
        Ok(())
    }

    // -- private helpers ----------------------------------------------------

    /// Reject if closed or if a lane already has an active operation. On
    /// success, installs the `ActiveRun` guard and returns `(run_id, signal,
    /// idle_notify)`.
    fn acquire_run(
        &self,
        kind: OperationKind,
    ) -> HarnessResult<(String, CancellationToken, Arc<tokio::sync::Notify>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if let Some(active) = &inner.active_run {
            return Err(HarnessError::lane_busy(
                self.lane.clone(),
                active.run_id.clone(),
                active.kind,
                format!(
                    "Lane {} already has an active {} operation",
                    self.lane,
                    active.kind.as_str()
                ),
            ));
        }
        let run_id = self.session.id_generator().next();
        let signal = CancellationToken::new();
        let idle = Arc::new(tokio::sync::Notify::new());
        inner.active_run = Some(ActiveRun {
            run_id: run_id.clone(),
            lane: self.lane.clone(),
            signal: signal.clone(),
            idle: Arc::clone(&idle),
            kind,
        });
        Ok((run_id, signal, idle))
    }

    /// Release the active-run guard and notify idle waiters.
    fn release_run(&self) {
        let idle = {
            let mut inner = self.inner.lock().unwrap();
            inner.active_run.take().map(|a| a.idle)
        };
        if let Some(idle) = idle {
            idle.notify_waiters();
        }
    }

    /// Enqueue a message into a steering/follow-up queue, **requiring an active run**.
    ///
    /// This implementation is now **unused** after aligning with native pi's
    /// design: `steer()` and `follow_up()` now enqueue unconditionally,
    /// mirroring the TS Agent class and avoiding the race between `activeRun`
    /// clearing and the TUI status check. The function is kept for reference
    /// and for potential future queue kinds that may need the active-run guard.
    #[allow(dead_code)]
    /// Provisioned JSON for an entry — the shape a queue record stores as its
    /// `target`. Built through the reducer's own helper so the recorded intent
    /// and the eventual commit cannot drift.
    fn provisioned_target(entry: &ProvisionedEntry) -> JsonValue {
        let placeholder = crate::session::types::provisioned_into_entry(entry.clone(), 0, None, 0);
        crate::session::reducer::entry_provisioned_json(&placeholder)
    }

    /// Recover the queued message from a recorded `target`.
    ///
    /// The target is the entry's flat JSON minus the storage-assigned fields, so
    /// those three are put back with placeholders before deserializing.
    fn message_from_target(target: &JsonValue) -> Option<AgentMessage> {
        let mut object = target.as_object()?.clone();
        object.insert("seq".to_string(), JsonValue::from(0));
        object.insert("parentId".to_string(), JsonValue::Null);
        object.insert("timestamp".to_string(), JsonValue::from(0));
        let entry: Entry = serde_json::from_value(JsonValue::Object(object)).ok()?;
        match crate::session::types::provisioned_from_entry(&entry).kind {
            ProvisionedKind::Message { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The entry id a queue record's `target` names.
    fn target_entry_id(target: &JsonValue) -> Option<&str> {
        target.get("id").and_then(|value| value.as_str())
    }

    /// Enqueue a message and record the intent durably.
    ///
    /// The record is what lets a queued message survive a crash: the in-memory
    /// queue is rebuilt from these records on the next start. Without it the
    /// queue is purely process-local, so a steer typed while the agent works is
    /// simply lost.
    async fn enqueue_message(
        &self,
        message: AgentMessage,
        kind: QueueKind,
    ) -> HarnessResult<QueueResult> {
        let (entry_id, queue, run_id) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            let active = inner.active_run.as_ref().filter(|a| a.lane == self.lane);
            let queue = match kind {
                QueueKind::Steer => Arc::clone(&inner.steering_queue),
                QueueKind::FollowUp => Arc::clone(&inner.follow_up_queue),
                QueueKind::NextRun => Arc::clone(&inner.next_run_queue),
            };
            // The run the item belongs to. A cancellation must carry the same run
            // id (the reducer matches on it), so it is recorded with the item.
            let run_id = active.map(|active| active.run_id.clone());
            (self.session.id_generator().next(), queue, run_id)
        };
        queue.lock().unwrap().pending.push_back(QueuedMessage {
            entry_id: entry_id.clone(),
            lane: self.lane.clone(),
            message: message.clone(),
        });
        self.write_queue_enqueued(kind, run_id.as_deref(), &entry_id, &message)
            .await?;
        Ok(QueueResult { entry_id })
    }

    /// Record that `entry_id` is queued.
    async fn write_queue_enqueued(
        &self,
        kind: QueueKind,
        run_id: Option<&str>,
        entry_id: &str,
        message: &AgentMessage,
    ) -> HarnessResult<()> {
        let target = Self::provisioned_target(&ProvisionedEntry {
            id: entry_id.to_string(),
            kind: ProvisionedKind::Message {
                message: message.clone(),
                terminate: None,
            },
        });
        self.session
            .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
                base: self.record_base(),
                queue: kind,
                run_id: run_id.map(str::to_string),
                target,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Record that a queued item is no longer queued.
    ///
    /// `run_id` must be the value the matching enqueue carried: the reducer
    /// matches a cancellation against its enqueue on `run_id`, so using the
    /// *current* run (or `None`) would be rejected as log corruption whenever the
    /// item was queued under a different run.
    async fn write_queue_cancelled(
        &self,
        entry_id: &str,
        run_id: Option<&str>,
    ) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::QueueCancelled(QueueCancelledRecord {
                base: self.record_base(),
                run_id: run_id.map(str::to_string),
                entry_id: entry_id.to_string(),
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// The enqueue record for `entry_id`, if this lane has one.
    ///
    /// A cancellation is only written when the enqueue exists: writing one
    /// without it is exactly the "cancellation has no pending matching enqueue"
    /// corruption the reducer checks for.
    async fn find_enqueue(&self, entry_id: &str) -> Option<QueueEnqueuedRecord> {
        let records = self
            .session
            .find_records(&RecordQuery {
                lane: Some(self.lane.clone()),
                record_type: Some("queue_enqueued"),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .ok()?;
        records.into_iter().find_map(|record| match record {
            LaneRecord::QueueEnqueued(enqueued)
                if Self::target_entry_id(&enqueued.target) == Some(entry_id) =>
            {
                Some(enqueued)
            }
            _ => None,
        })
    }

    /// Take every queued item for this lane the way the loop's injection
    /// callbacks need: record the consumption, and hand the reserved entry id to
    /// the settle observer so the injected message is committed when it settles
    /// rather than surviving only until the run ends.
    async fn drain_injected(
        &self,
        queue: SharedMessageQueue,
        settle: &Arc<crate::settle::SettleState>,
    ) -> Vec<AgentMessage> {
        let items = queue.lock().unwrap().drain_for_lane(&self.lane);
        let mut messages = Vec::with_capacity(items.len());
        for item in items {
            if let Some(enqueued) = self.find_enqueue(&item.entry_id).await {
                if let Err(error) = self
                    .write_queue_cancelled(&item.entry_id, enqueued.run_id.as_deref())
                    .await
                {
                    tracing::warn!(
                        entry_id = %item.entry_id,
                        %error,
                        "could not record a queue consumption"
                    );
                }
            }
            settle.push_injected(item.entry_id.clone(), item.message.clone());
            messages.push(item.message);
        }
        messages
    }

    /// Take every queued item for this lane, recording each consumption.
    async fn drain_queue(&self, kind: QueueKind) -> Vec<AgentMessage> {
        let queue = {
            let inner = self.inner.lock().unwrap();
            match kind {
                QueueKind::Steer => Arc::clone(&inner.steering_queue),
                QueueKind::FollowUp => Arc::clone(&inner.follow_up_queue),
                QueueKind::NextRun => Arc::clone(&inner.next_run_queue),
            }
        };
        self.drain_shared_queue(queue).await
    }

    /// Consumption is recorded *before* the message can become an entry, because
    /// the reducer requires the cancelled target to not exist yet.
    async fn drain_shared_queue(&self, queue: SharedMessageQueue) -> Vec<AgentMessage> {
        let items = queue.lock().unwrap().drain_for_lane(&self.lane);
        let mut messages = Vec::with_capacity(items.len());
        for item in items {
            if let Some(enqueued) = self.find_enqueue(&item.entry_id).await {
                if let Err(error) = self
                    .write_queue_cancelled(&item.entry_id, enqueued.run_id.as_deref())
                    .await
                {
                    tracing::warn!(
                        entry_id = %item.entry_id,
                        %error,
                        "could not record a queue consumption"
                    );
                }
            }
            messages.push(item.message);
        }
        messages
    }

    /// Rebuild the in-memory queues from the durable records.
    ///
    /// An item is still queued when its enqueue has no matching cancellation.
    /// Reconstructed in recorded order, so a restart delivers what was pending in
    /// the order the user wrote it.
    async fn rebuild_queues(session: &Session, lane: &str) -> Vec<(QueueKind, QueuedMessage)> {
        let records = match session
            .find_records(&RecordQuery {
                lane: Some(lane.to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
        {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(lane, %error, "could not read the queue records");
                return Vec::new();
            }
        };
        let mut cancelled: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut pending: Vec<QueueEnqueuedRecord> = Vec::new();
        for record in records {
            match record {
                LaneRecord::QueueEnqueued(enqueued) => pending.push(enqueued),
                LaneRecord::QueueCancelled(cancelled_record) => {
                    cancelled.insert(cancelled_record.entry_id);
                }
                _ => {}
            }
        }
        pending
            .into_iter()
            .filter_map(|enqueued| {
                let entry_id = Self::target_entry_id(&enqueued.target)?;
                if cancelled.contains(entry_id) {
                    return None;
                }
                let message = Self::message_from_target(&enqueued.target)?;
                Some((
                    enqueued.queue,
                    QueuedMessage {
                        entry_id: entry_id.to_string(),
                        lane: lane.to_string(),
                        message,
                    },
                ))
            })
            .collect()
    }

    /// A record base for this lane. The store re-stamps `seq`/`lane`/`timestamp`,
    /// but the lane must still name a real one: it is how the store routes the
    /// append.
    fn record_base(&self) -> RecordBase {
        RecordBase {
            id: self.session.id_generator().next(),
            seq: 0,
            lane: self.lane.clone(),
            timestamp: 0,
        }
    }

    /// Turn an interrupted run's committed frames into the exact sequence of
    /// messages to append: each partial assistant message followed by one result
    /// per tool call whose result never landed.
    ///
    /// Each unresolved call is either **replayed** (when both the recorded policy
    /// and the current tool declaration say `Safe`) or recorded as an
    /// unknown-outcome stand-in. Never appending a result at all is not an
    /// option: an assistant message whose tool calls have no results is an
    /// invalid request for every provider.
    ///
    /// Mirrors native pi's `recoverToolInvocation` (`drive/tools.ts`).
    async fn resolve_interrupted_run(
        session: &Session,
        lane: &str,
        run_id: &str,
        tools: &[HarnessTool],
        signal: &CancellationToken,
        salvage_failed_attempts: bool,
    ) -> HarnessResult<Vec<AgentMessage>> {
        // The `tool_started` records hold what a replay needs and the frames do
        // not: the arguments the call was made with, and the policy declared at
        // the time. The in-memory copy died with the process.
        let records = session
            .find_records(&RecordQuery {
                record_type: Some("tool_started"),
                run_id: Some(run_id.to_string()),
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(session_to_harness_err)?;
        let mut recorded: std::collections::BTreeMap<String, ToolStartedRecord> =
            std::collections::BTreeMap::new();
        for record in records {
            if let LaneRecord::ToolStarted(frame) = record {
                recorded.insert(frame.tool_call_id.clone(), frame);
            }
        }

        let mut messages = Vec::new();
        let mut resolved: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // (1) Streams that never became entries: salvage them as interrupted
        //     assistant messages, each followed by its unresolved tool results.
        for partial in crate::frame_progress::replay_run_frames(session, run_id).await? {
            // When the run is about to be retried, none of its uncommitted
            // streams is worth keeping: they are the failed attempts of the retry
            // chain, and recording one would leave a dead assistant message in the
            // transcript as if it were history.
            //
            // The check is deliberately run-level rather than per-partial. It
            // *cannot* be per-partial: frames carry no terminal frame, so
            // `reduce_frames` yields `stop_reason: Pending` and the reduced
            // message never looks like the error it actually was.
            if !salvage_failed_attempts {
                continue;
            }
            let unresolved =
                crate::frame_progress::unknown_tool_outcomes(session, lane, &partial).await?;
            let timestamp = partial.timestamp;
            messages.push(AgentMessage::Assistant(Box::new(
                crate::frame_progress::interrupted_message(partial),
            )));
            for outcome in unresolved {
                resolved.insert(outcome.tool_call_id.clone());
                messages.push(
                    Self::resolve_tool_call(tools, signal, &recorded, &outcome, timestamp).await,
                );
            }
        }

        // (2) A *committed* assistant message whose tools never finished.
        //
        // Commit-on-settle commits an assistant message as soon as it settles and
        // retires that stream, so its calls are not in (1) — but they still need
        // results, or the next request carries an assistant message with tool
        // calls and nothing answering them. This also covers the window between
        // the message settling and its first tool starting, where no
        // `tool_started` record exists yet.
        //
        // Appending at the tip is order-correct: the loop runs a whole tool batch
        // before the next assistant message, so the only assistant whose calls can
        // be outstanding is the last one.
        if let Some(partial) = Self::tip_assistant(session, lane).await {
            let unresolved =
                crate::frame_progress::unknown_tool_outcomes(session, lane, &partial).await?;
            let timestamp = partial.timestamp;
            for outcome in unresolved {
                if resolved.insert(outcome.tool_call_id.clone()) {
                    messages.push(
                        Self::resolve_tool_call(tools, signal, &recorded, &outcome, timestamp)
                            .await,
                    );
                }
            }
        }
        Ok(messages)
    }

    /// Whether the branch tip is a user message the run died on.
    ///
    /// That is the `starting` crash: the operation opened, the prompt was
    /// persisted, and the process died before the first provider call. Nothing
    /// was produced, so the tip is still the prompt — and the run should simply
    /// continue, which is what native pi does by re-entering its `starting` state.
    async fn tip_is_user_message(session: &Session, lane: &str) -> bool {
        let entries = match session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        matches!(
            entries.last(),
            Some(Entry::Message(message)) if matches!(message.message, AgentMessage::User(_))
        )
    }

    /// Whether the branch tip is an assistant message.
    async fn tip_is_assistant(session: &Session, lane: &str) -> bool {
        let entries = match session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
        {
            Ok(entries) => entries,
            Err(_) => return true,
        };
        matches!(
            entries.last(),
            Some(Entry::Message(message)) if matches!(message.message, AgentMessage::Assistant(_))
        )
    }

    /// The assistant message at the branch tip, when the tip is one.
    ///
    /// A committed assistant message sitting at the tip is how a run that died
    /// during its tools looks once commit-on-settle has persisted the message.
    async fn tip_assistant(session: &Session, lane: &str) -> Option<AssistantMessage> {
        let entries = session
            .view(lane)
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .ok()?;
        match entries.last() {
            Some(Entry::Message(message)) => match &message.message {
                AgentMessage::Assistant(assistant) => Some((**assistant).clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Resolve one unresolved tool call: replay it when that is safe, otherwise
    /// record that its outcome is unknown.
    async fn resolve_tool_call(
        tools: &[HarnessTool],
        signal: &CancellationToken,
        recorded: &std::collections::BTreeMap<String, ToolStartedRecord>,
        outcome: &crate::frame_progress::UnknownToolOutcome,
        timestamp: i64,
    ) -> AgentMessage {
        let frame = recorded.get(&outcome.tool_call_id);
        let tool = tools
            .iter()
            .find(|tool| tool.tool.schema().name == outcome.tool_name);
        // Dual confirmation, exactly like native: the call must have been
        // recorded as safe *and* the tool must still declare itself safe.
        // Either alone would replay calls that are not safe to repeat.
        let may_replay = frame.is_some_and(|frame| frame.replay == ToolReplay::Safe)
            && tool.is_some_and(|tool| tool.replay == crate::types::ToolReplay::Safe);
        if !may_replay {
            return crate::frame_progress::interrupted_tool_result(outcome, timestamp);
        }
        let (Some(frame), Some(tool)) = (frame, tool) else {
            return crate::frame_progress::interrupted_tool_result(outcome, timestamp);
        };

        let replayed = tool
            .tool
            .execute(
                &outcome.tool_call_id,
                frame.effective_args.clone(),
                signal.child_token(),
                Arc::new(|_| {}),
            )
            .await
            .map_err(|error| error.to_string());
        match replayed {
            Ok(result) => {
                // The tool's own output first, then the marker: mirrors native pi's
                // `interruptedOutcome`, which appends its marker after the content
                // the tool had already produced, and keeps the result readable —
                // the reader sees the output before the note about it.
                let mut content = result.clone().into_content();
                content.push(Content::text(crate::tool_recovery::REPLAYED_TOOL_RESULT));
                AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
                    role: rpi_ai::types::ToolResultRole,
                    tool_call_id: outcome.tool_call_id.clone(),
                    tool_name: outcome.tool_name.clone(),
                    content,
                    details: Some(result.details.clone()),
                    usage: result.usage.clone(),
                    added_tool_names: result.added_tool_names.clone(),
                    is_error: false,
                    timestamp,
                }))
            }
            Err(error) => AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
                role: rpi_ai::types::ToolResultRole,
                tool_call_id: outcome.tool_call_id.clone(),
                tool_name: outcome.tool_name.clone(),
                content: vec![Content::text(format!(
                    "{}\n\nThe re-run failed: {error}",
                    crate::tool_recovery::REPLAYED_TOOL_RESULT
                ))],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp,
            })),
        }
    }

    /// Settle an operation left open by a process that died, repairing what it
    /// left behind.
    ///
    /// Leaves the lane with no open operation — otherwise every future operation
    /// on it would fail permanently. When the repair leaves the branch
    /// continuable, `create` records a [`PendingResume`] so the run can be picked
    /// up again; the settlement itself deliberately does not start any work.
    async fn finish_interrupted_operation(
        session: &Session,
        lane: &str,
        tools: &[HarnessTool],
        signal: &CancellationToken,
        salvage_failed_attempts: bool,
    ) -> HarnessResult<()> {
        let open = session
            .find_open_operations(lane, Some(2))
            .await
            .map_err(session_to_harness_err)?;
        if open.len() > 1 {
            return Err(HarnessError::io(format!(
                "Lane {lane} has multiple open operations; the session log is corrupted"
            )));
        }
        let Some(operation) = open.first() else {
            return Ok(());
        };
        let run_id = operation.base.id.clone();

        session
            .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                base: RecordBase {
                    id: session.id_generator().next(),
                    seq: 0,
                    lane: lane.to_string(),
                    timestamp: 0,
                },
                run_id: run_id.clone(),
                outcome: OperationOutcome::Aborted,
                error: Some(OperationError {
                    code: "interrupted".to_string(),
                    message: "Operation was interrupted before it could finish".to_string(),
                }),
            }))
            .await
            .map_err(session_to_harness_err)?;

        // The run never got to persist its messages, so replay whatever
        // assistant frames committed before the process died and record them as
        // interrupted. Without this the whole run's output is lost and only the
        // user's prompt remains — see `docs/llm-repetition-forensics.md` §十一.
        // Mirrors native pi's `recoverAssistantGeneration`.
        //
        // Every unresolved tool call gets a result: a real one when the call may
        // be replayed, otherwise the unknown-outcome stand-in. That pairing is
        // required, not cosmetic — a crash most often lands *during* tool
        // execution, so the committed partial usually carries tool calls, and an
        // assistant message whose tool calls have no results is rejected by every
        // provider on the next request.
        match Self::resolve_interrupted_run(
            session,
            lane,
            &run_id,
            tools,
            signal,
            salvage_failed_attempts,
        )
        .await
        {
            Ok(salvaged) => {
                for message in salvaged {
                    if let Err(error) = session.view(lane).append_message(message).await {
                        tracing::warn!(
                            lane,
                            %error,
                            "could not persist a salvaged assistant message after an interrupted run"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    lane,
                    %error,
                    "could not salvage assistant frames for an interrupted run"
                );
            }
        }
        Ok(())
    }

    /// Resolve the provider for `model.provider`. Returns the first registered
    /// provider whose `id()` matches.
    fn resolve_provider(
        models: &[Arc<dyn AiProvider>],
        model: &Model,
    ) -> HarnessResult<Arc<dyn AiProvider>> {
        models
            .iter()
            .find(|p| p.id() == model.provider)
            .cloned()
            .ok_or_else(|| {
                HarnessError::agent(format!(
                    "No provider registered for model provider '{}'",
                    model.provider
                ))
            })
    }

    /// Build a `StreamFn` that bridges the sync-return contract to the async
    /// `Provider::stream_simple`. Mirrors `examples/minimal`'s `make_stream_fn`:
    /// `block_in_place` + `Handle::current().block_on`.
    ///
    /// B4: the closure also captures `provider_hooks`; before each
    /// `stream_simple` it fires `before_request` and applies the returned
    /// `SimpleStreamOptionsPatch` to a clone of `opts`, so an extension's
    /// `before_provider_request`/`before_provider_headers` `on()` handlers can
    /// patch the LIVE per-call options (not a run-once config-build — that
    /// would not be equivalent to pi's `beforeRequest`, which fires before
    /// every provider call). `after_response` observation is wired on the
    /// emitter side (B3+) where the terminal `MessageEnd` is visible without
    /// stealing the stream from the loop; the per-call patch is the
    /// load-bearing B4 deliverable here.
    fn build_stream_fn(
        models: Vec<Arc<dyn AiProvider>>,
        provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
        stream_options: AgentHarnessStreamOptions,
    ) -> HarnessResult<StreamFn> {
        // Resolve the provider lazily per call (the model may change between
        // turns via prepare_next_turn). If no provider matches, emit an Error
        // terminal event on a synthetic stream.
        let stream_fn = rpi_agent::stream_fn(
            move |model: &Model,
                  ctx: &rpi_ai::types::Context,
                  opts: &rpi_ai::SimpleStreamOptions| {
                let provider = models.iter().find(|p| p.id() == model.provider).cloned();
                match provider {
                    Some(p) => {
                        let model = model.clone();
                        let ctx = ctx.clone();
                        // B4 [merge_into]: the loop builds `opts` via
                        // `AgentLoopConfig::to_stream_options`, which DROPS the
                        // harness's pinned `headers`/`metadata` (AgentLoopConfig
                        // has no such fields → to_stream_options sets them None).
                        // Fold the pinned `AgentHarnessStreamOptions` into the
                        // live opts here, at the per-call site, so headers/metadata
                        // reach the provider — then layer the per-call
                        // `ProviderHooks::before_request` patch on top.
                        let mut opts = stream_options.merge_into(opts.clone());
                        if let Some(hooks) = &provider_hooks {
                            if let Some(patch) = hooks.before_request(&model, &ctx, &opts) {
                                patch.apply(&mut opts);
                            }
                        }
                        tokio::task::block_in_place(|| {
                            Handle::current()
                                .block_on(async move { p.stream_simple(&model, &ctx, &opts).await })
                        })
                    }
                    None => {
                        // No provider: synthesize an Error terminal stream.
                        let (mut producer, stream) =
                            rpi_ai::event_stream::create_assistant_message_event_stream();
                        let msg = AssistantMessage::terminal(
                            model.api.clone(),
                            &model.provider,
                            &model.id,
                            StopReason::Error,
                            format!("No provider registered for '{}'", model.provider),
                            0,
                        );
                        let _ = producer.push(rpi_ai::types::AssistantMessageEvent::Error {
                            reason: rpi_ai::types::ErrorReason::Error,
                            error: msg,
                        });
                        stream
                    }
                }
            },
        );
        Ok(stream_fn)
    }

    /// Build the `ConvertToLlm` Arc for `AgentLoopConfig`: use the caller's
    /// override if provided, else wrap the harness-level `convert_to_llm`.
    fn build_convert_to_llm(to_provider_messages: Option<ConvertToLlm>) -> ConvertToLlm {
        to_provider_messages.unwrap_or_else(|| {
            Arc::new(|messages: Vec<AgentMessage>| {
                let out = harness_convert_to_llm(messages);
                Box::pin(async move { out })
            })
        })
    }

    /// Build the `SessionContextBuildOptions` from the entry projectors + the
    /// caller-supplied context-entry transforms (B3b). Previously
    /// `entry_transforms` was hardcoded to `Vec::new()`; the options field now
    /// flows through so a host (pi-cli, via rpi-extensions) can inject transforms.
    fn build_opts(
        projectors: &BTreeMap<String, CustomEntryContextMessageProjector>,
        entry_transforms: &[ContextEntryTransform],
    ) -> SessionContextBuildOptions {
        SessionContextBuildOptions {
            entry_transforms: entry_transforms.to_vec(),
            entry_projectors: projectors.clone(),
        }
    }

    /// Load this lane's branch path, oldest-first. Returns an empty vec when
    /// the lane is empty (no leaf).
    async fn branch_path_oldest_first(&self) -> HarnessResult<Vec<Entry>> {
        let leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let Some(leaf_id) = leaf else {
            return Ok(Vec::new());
        };
        let query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        };
        let bounds = BranchBounds {
            start: Some(leaf_id),
            ..Default::default()
        };
        self.session
            .view(&self.lane)
            .find_entries_on_branch(&query, &bounds)
            .await
            .map_err(session_to_harness_err)
    }

    /// Snapshot the inner config fields needed to build an `AgentLoopConfig`.
    fn snapshot_config(&self) -> HarnessResult<ConfigSnapshot> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(ConfigSnapshot {
            model: inner.model.clone(),
            thinking_level: inner.thinking_level,
            active_tool_names: inner.active_tool_names.clone(),
            tools: inner.tools.clone(),
            system_prompt: inner.system_prompt.clone(),
            resources: inner.resources.clone(),
            stream_options: inner.stream_options.clone(),
            retry: inner.retry.clone(),
            compaction: inner.compaction,
            tool_execution: inner.tool_execution,
            models: inner.models.clone(),
            to_provider_messages: inner.to_provider_messages.clone(),
            entry_projectors: inner.entry_projectors.clone(),
            entry_transforms: inner.entry_transforms.clone(),
            agent_emitter: inner.agent_emitter.clone(),
            before_tool_call: inner.before_tool_call.clone(),
            after_tool_call: inner.after_tool_call.clone(),
            transform_context: inner.transform_context.clone(),
            provider_hooks: inner.provider_hooks.clone(),
            steering_queue: Arc::clone(&inner.steering_queue),
            follow_up_queue: Arc::clone(&inner.follow_up_queue),
        })
    }

    /// Build the agent tools vec, filtered by `active_tool_names`. An empty
    /// `active_tool_names` means "all tools active" (mirrors TS default).
    fn active_tools(tools: &[HarnessTool], active: &[String]) -> Vec<Arc<dyn AgentTool>> {
        if active.is_empty() {
            return tools.iter().map(|t| Arc::clone(&t.tool)).collect();
        }
        tools
            .iter()
            .filter(|t| active.contains(&t.tool.schema().name))
            .map(|t| Arc::clone(&t.tool))
            .collect()
    }

    /// Compose the system prompt: base prompt + skills listing. The base prompt
    /// the CLI hands the harness already carries the `append` + `context`
    /// sections (folded in by `session.rs` via `compose_system_prompt` before
    /// `AgentHarness::create`), so here the harness only appends the
    /// `<available_skills>` listing — the one section it owns itself.
    ///
    /// **Read-tool gate** (pi `skills.ts:335-336`): the skills listing is
    /// injected only when the `read` tool is in `active_tool_names`. The model
    /// needs `read` to load a skill's full file on invocation; without it, the
    /// listing would advertise skills the model cannot act on, so pi suppresses
    /// it. The `disable_model_invocation` filter is applied inside
    /// `format_skills_for_system_prompt`.
    fn compose_prompt(
        base: Option<&str>,
        resources: &AgentHarnessResources,
        active_tool_names: &[String],
    ) -> String {
        let all_skills: &[Skill] = resources.skills.as_deref().unwrap_or(&[]);
        let skills: &[Skill] = if active_tool_names.iter().any(|n| n == "read") {
            all_skills
        } else {
            &[]
        };
        compose_system_prompt(base, skills, None, None)
    }

    /// Reserve an entry id. Exposed so a settle observer can name an entry in a
    /// `step_attempt` record *before* committing it, which is what makes the
    /// record's `result_entry_id` checkable at recovery.
    pub(crate) fn next_entry_id(&self) -> String {
        self.session.id_generator().next()
    }

    /// Write a `tool_started` record on this harness's lane.
    ///
    /// This is the record that lets recovery name the tool whose side effect is
    /// unknown after a crash. It is only writable *after* the assistant message
    /// carrying the call is an entry, which is what commit-on-settle buys.
    /// Takes a fully-formed record rather than its fields: the caller already
    /// assembles the parts (assistant entry, ordinal, reserved result id), and a
    /// nine-parameter signature would only split one construction across two
    /// places.
    pub(crate) async fn write_tool_started(&self, record: ToolStartedRecord) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::ToolStarted(record))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Commit a provisioned entry on this harness's lane.
    pub(crate) async fn append_provisioned(&self, entry: ProvisionedEntry) -> HarnessResult<Entry> {
        self.session
            .append_entry(entry, &self.lane)
            .await
            .map_err(session_to_harness_err)
    }

    /// Record that another attempt is scheduled for `run_id`.
    ///
    /// Written before the backoff wait, so a crash during the wait is
    /// recognisable as "this run was about to retry" rather than looking like an
    /// interrupted run. The reserved id is that attempt's assistant entry: once it
    /// lands, the retry succeeded and nothing is pending.
    async fn write_retry_pending(
        &self,
        run_id: &str,
        attempt: u32,
        result_entry_id: &str,
    ) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::RetryPending(RetryPendingRecord {
                base: self.record_base(),
                run_id: run_id.to_string(),
                attempt,
                result_entry_id: result_entry_id.to_string(),
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Write a `step_attempt` record on this harness's lane.
    pub(crate) async fn write_step_attempt_record(
        &self,
        run_id: &str,
        step: StepKind,
        attempt: u32,
        result_entry_id: &str,
        compaction_reason: Option<CompactionReason>,
    ) -> HarnessResult<()> {
        self.write_step_attempt(run_id, step, attempt, result_entry_id, compaction_reason)
            .await
    }

    /// Persist a `Compaction` entry and return the stamped `Entry`.
    ///
    /// Writes the durable intent records around the commit:
    ///
    /// - `write_deferred` *before* the append, naming the entry this step means
    ///   to commit. A compaction's content is known up front, so its target JSON
    ///   is exact — which is what `validate_exact_provisioned_entry` demands
    ///   (it deep-compares the committed entry against the recorded intent).
    /// - `step_attempt` *after* the append, so `result_entry_id` names the entry
    ///   that actually landed. `validate_attempt_result` accepts an absent entry,
    ///   but a step record whose id never resolves is worse than none: recovery
    ///   would appear to have information it cannot check.
    async fn persist_compaction_entry(
        &self,
        run_id: &str,
        result: &crate::compaction::CompactResult,
        reason: CompactionReason,
    ) -> HarnessResult<Entry> {
        let id = self.session.id_generator().next();
        let details = result
            .details
            .as_ref()
            .map(|d| serde_json::to_value(d).unwrap_or(JsonValue::Null));
        let entry = ProvisionedEntry {
            id: id.clone(),
            kind: ProvisionedKind::Compaction {
                summary: result.summary.clone(),
                retained_tail: result.retained_tail.clone(),
                tokens_before: result.tokens_before,
                details,
                usage: result.usage.clone(),
            },
        };

        self.write_write_deferred(run_id, &entry).await?;
        let committed = self
            .session
            .append_entry(entry, &self.lane)
            .await
            .map_err(session_to_harness_err)?;
        // A compaction is a step series of its own, so its first attempt is 1
        // (the reducer resets the series when `step` changes).
        self.write_step_attempt(run_id, StepKind::Compaction, 1, &id, Some(reason))
            .await?;
        Ok(committed)
    }

    /// Record a `write_deferred`: an entry whose content is already known but
    /// which has not been committed yet.
    ///
    /// `target` is the entry's *provisioned* JSON (its flat serialization minus
    /// the storage-assigned `seq`/`parentId`/`timestamp`), built through the same
    /// helper the reducer compares against, so the recorded intent and the
    /// eventual commit cannot drift.
    async fn write_write_deferred(
        &self,
        run_id: &str,
        entry: &ProvisionedEntry,
    ) -> HarnessResult<()> {
        let placeholder = crate::session::types::provisioned_into_entry(entry.clone(), 0, None, 0);
        let target = crate::session::reducer::entry_provisioned_json(&placeholder);
        self.session
            .append_record(LaneRecord::WriteDeferred(WriteDeferredRecord {
                base: RecordBase {
                    id: self.session.id_generator().next(),
                    seq: 0,
                    lane: self.lane.clone(),
                    timestamp: 0,
                },
                run_id: run_id.to_string(),
                target,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Record a `step_attempt`: one durable step of a run, naming the entry that
    /// will (or did) hold its result.
    async fn write_step_attempt(
        &self,
        run_id: &str,
        step: StepKind,
        attempt: u32,
        result_entry_id: &str,
        compaction_reason: Option<CompactionReason>,
    ) -> HarnessResult<()> {
        self.session
            .append_record(LaneRecord::StepAttempt(StepAttemptRecord {
                base: RecordBase {
                    id: self.session.id_generator().next(),
                    seq: 0,
                    lane: self.lane.clone(),
                    timestamp: 0,
                },
                run_id: run_id.to_string(),
                step,
                attempt,
                result_entry_id: result_entry_id.to_string(),
                compaction_reason,
            }))
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Write an `operation_started` record. Returns the stamped record (its
    /// `base.id` is the run_id the caller generated; `seq`/`lane`/`timestamp`
    /// are overwritten by storage).
    async fn write_operation_started(
        &self,
        run_id: &str,
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    ) -> HarnessResult<OperationStartedRecord> {
        let record = LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: run_id.to_string(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            source_leaf_id,
            intent,
        });
        let stamped = self
            .session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        match stamped {
            LaneRecord::OperationStarted(s) => Ok(s),
            _ => unreachable!("append_record(OperationStarted) stamps into OperationStarted"),
        }
    }

    /// Write an `operation_finished` record.
    async fn write_operation_finished(
        &self,
        run_id: &str,
        outcome: OperationOutcome,
        error: Option<OperationError>,
    ) -> HarnessResult<()> {
        let record = LaneRecord::OperationFinished(OperationFinishedRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            run_id: run_id.to_string(),
            outcome,
            error,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(())
    }

    /// Persist each `AgentMessage` produced by the loop. The prompts were
    /// already persisted before the run, and `run_agent_loop` is called with
    /// an empty prompts vec (see `run_core`), so `new_messages` contains ONLY
    /// loop-produced messages — persist every one of them. Returns
    /// `(leaf_id, final_entry_id)`.
    async fn persist_new_messages(
        &self,
        new_messages: &[AgentMessage],
        start_index: usize,
        committed: &std::collections::BTreeSet<usize>,
    ) -> HarnessResult<(Option<String>, Option<String>)> {
        // `start_index` is where `new_messages[0]` sits in the loop's full message
        // sequence, so a settle-committed index lines up with slice offsets.
        // `committed` holds the indices the settle observer already persisted.
        let mut leaf_id = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let mut final_entry_id: Option<String> = leaf_id.clone();
        for (offset, msg) in new_messages.iter().enumerate() {
            // A message the settle observer already committed is durable; writing
            // it again would duplicate it in the branch.
            if committed.contains(&(start_index + offset)) {
                continue;
            }
            let id = self
                .session
                .view(&self.lane)
                .append_message(msg.clone())
                .await
                .map_err(session_to_harness_err)?;
            leaf_id = Some(id.clone());
            final_entry_id = Some(id);
        }
        Ok((leaf_id, final_entry_id))
    }

    /// Derive the harness `RunOutcome` from the terminal assistant message.
    fn derive_outcome(
        new_messages: &[AgentMessage],
        leaf_id: Option<String>,
        final_entry_id: Option<String>,
    ) -> HarnessRunOutcome {
        let leaf = leaf_id.unwrap_or_default();
        let final_id = final_entry_id.unwrap_or_default();
        // Find the last assistant message.
        let last_assistant = new_messages.iter().rev().find_map(|m| match m {
            AgentMessage::Assistant(a) => Some(a.as_ref()),
            _ => None,
        });
        match last_assistant {
            None => HarnessRunOutcome::Failed {
                leaf_id: leaf,
                error: OperationError {
                    code: "no_assistant".into(),
                    message: "Run ended without producing an assistant message".into(),
                },
                final_entry_id: if final_id.is_empty() {
                    None
                } else {
                    Some(final_id)
                },
                final_message: None,
            },
            Some(am) => match am.stop_reason {
                StopReason::Error => HarnessRunOutcome::Failed {
                    leaf_id: leaf,
                    error: OperationError {
                        code: "error".into(),
                        message: am.error_message.clone().unwrap_or_default(),
                    },
                    final_entry_id: if final_id.is_empty() {
                        None
                    } else {
                        Some(final_id)
                    },
                    final_message: Some(am.clone()),
                },
                StopReason::Aborted => HarnessRunOutcome::Aborted {
                    leaf_id: leaf,
                    final_entry_id: final_id,
                    final_message: am.clone(),
                },
                StopReason::Deferred => match &am.deferred {
                    Some(handle) => HarnessRunOutcome::Suspended {
                        leaf_id: leaf,
                        final_entry_id: final_id,
                        deferred: handle.clone(),
                    },
                    None => HarnessRunOutcome::Failed {
                        leaf_id: leaf,
                        error: OperationError {
                            code: "deferred_no_handle".into(),
                            message: "Assistant returned Deferred stop reason without a handle"
                                .into(),
                        },
                        final_entry_id: if final_id.is_empty() {
                            None
                        } else {
                            Some(final_id)
                        },
                        final_message: Some(am.clone()),
                    },
                },
                _ => HarnessRunOutcome::Completed {
                    leaf_id: leaf,
                    final_entry_id: final_id,
                    final_message: am.clone(),
                },
            },
        }
    }

    /// The core run loop shared by all prompt overloads + skill + template.
    async fn run_core(&self, prompts: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        self.run_core_with_entry(prompts, false).await
    }

    /// Continue a run from the assistant message at the branch tip.
    ///
    /// The deferred case: that message came from a poll, so its tool calls have
    /// not run. Entering the loop *from* it executes them before the next
    /// provider request instead of sending unanswered tool calls.
    async fn run_core_from_assistant(&self) -> HarnessResult<RunResult> {
        self.run_core_with_entry(Vec::new(), true).await
    }

    async fn run_core_with_entry(
        &self,
        prompts: Vec<AgentMessage>,
        from_assistant: bool,
    ) -> HarnessResult<RunResult> {
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Run)?;

        // `nextRun` messages are consumed when a new run starts, before the
        // caller's prompt. This matches pi's ordering and keeps them durable
        // through the normal prompt persistence path below.
        // The in-memory queue is drained through the recording path, so the
        // consumed items are durably un-queued rather than reappearing next start.
        let queued = self.drain_queue(QueueKind::NextRun).await;
        let prompts: Vec<AgentMessage> = queued.into_iter().chain(prompts).collect();
        let _run_lease = ActiveRunLease { harness: self };
        // Recovery runs before the source leaf is snapshotted, so the salvaged
        // messages are part of the base history this run builds on.
        let recovery_tools = self.get_tools().await?;
        // `true`: a run starting here retries through its own retry loop, so a
        // leftover failed attempt is settled the ordinary way.
        Self::finish_interrupted_operation(
            &self.session,
            &self.lane,
            &recovery_tools,
            &signal,
            true,
        )
        .await?;

        // Snapshot the source leaf (before persisting prompts) for the
        // operation_started record.
        let source_leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;

        // Emit RunStart.
        self.bus.emit(&HarnessEvent::RunStart(RunStartEvent {
            lane: self.lane.clone(),
            run_id: run_id.clone(),
        }));

        // Write operation_started.
        // A resumed run records the chain it belongs to. `resume_data` was an
        // unused placeholder until now; the attempt budget has to be durable,
        // because the crash that makes it matter also destroys every in-memory
        // counter that could have held it.
        let resume_data = self
            .inner
            .lock()
            .unwrap()
            .pending_resume
            .clone()
            .filter(|pending| pending.attempt < MAX_RESUME_ATTEMPTS)
            .map(|pending| {
                let mut data = std::collections::BTreeMap::new();
                data.insert(
                    "resumedFrom".to_string(),
                    JsonValue::String(pending.chain_origin_run_id),
                );
                data.insert(
                    "resumeAttempt".to_string(),
                    JsonValue::from(pending.attempt + 1),
                );
                data
            });
        let intent = OperationIntent::Run {
            original_prompt: prompts.clone(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data,
        };
        self.write_operation_started(&run_id, source_leaf.clone(), intent)
            .await?;

        // Persist prompts (the user's input) BEFORE the run so they're durable.
        for msg in &prompts {
            self.session
                .view(&self.lane)
                .append_message(msg.clone())
                .await
                .map_err(session_to_harness_err)?;
        }

        // Snapshot config.
        let snap = self.snapshot_config()?;
        if snap.models.is_empty() {
            let _ = self
                .write_operation_finished(
                    &run_id,
                    OperationOutcome::Failed,
                    Some(OperationError {
                        code: "no_provider".into(),
                        message: "No models/providers configured".into(),
                    }),
                )
                .await;
            let leaf = source_leaf.unwrap_or_default();
            self.bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
                lane: self.lane.clone(),
                run_id: run_id.clone(),
                outcome: RunEndOutcome::Failed,
                leaf_id: leaf.clone(),
            }));
            return Ok(RunResult {
                run_id,
                outcome: HarnessRunOutcome::Failed {
                    leaf_id: leaf,
                    error: OperationError {
                        code: "no_provider".into(),
                        message: "No models/providers configured".into(),
                    },
                    final_entry_id: None,
                    final_message: None,
                },
            });
        }

        // Build the branch path (now includes the just-persisted prompts).
        let path = self.branch_path_oldest_first().await?;
        let build_opts = Self::build_opts(&snap.entry_projectors, &snap.entry_transforms);

        // Pre-run compaction evaluation.
        if snap.compaction.enabled && snap.model.context_window > 0 {
            let ctx = build_session_context(&path, &build_opts);
            let ctx_tokens = estimate_context_tokens(&ctx.messages).tokens;
            let window = snap.model.context_window as i64;
            if should_compact(ctx_tokens, window, &snap.compaction) {
                if let Ok(Some(prep)) = prepare_compaction(&path, snap.compaction) {
                    let compaction_provider = Self::resolve_provider(&snap.models, &snap.model)
                        .or_else(|_| {
                            // Fall back to first provider if the model's
                            // provider isn't registered but others are.
                            snap.models
                                .first()
                                .cloned()
                                .ok_or_else(|| HarnessError::agent("No provider for compaction"))
                        });
                    if let Ok(provider) = compaction_provider {
                        let llm_opts = CompactionLlmOptions {
                            provider,
                            model: snap.model.clone(),
                            api_key: None,
                            signal: signal.clone(),
                            thinking_level: Some(snap.thinking_level),
                            retry: if snap.retry.enabled {
                                Some(snap.retry.clone())
                            } else {
                                None
                            },
                            custom_instructions: None,
                        };
                        match compact(&prep, &llm_opts).await {
                            Ok(result) => {
                                let _ = self
                                    .persist_compaction_entry(
                                        &run_id,
                                        &result,
                                        // The pre-run check fires when the context
                                        // crosses the configured threshold, i.e.
                                        // proactively, not in reaction to a provider
                                        // overflow error.
                                        CompactionReason::Threshold,
                                    )
                                    .await?;
                            }
                            Err(e) => {
                                // Compaction failed; abort the run.
                                let leaf = self
                                    .session
                                    .view(&self.lane)
                                    .get_leaf_id()
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default();
                                let err = OperationError {
                                    code: "compaction_failed".into(),
                                    message: e.message.clone(),
                                };
                                let _ = self
                                    .write_operation_finished(
                                        &run_id,
                                        OperationOutcome::Failed,
                                        Some(err.clone()),
                                    )
                                    .await;
                                self.bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
                                    lane: self.lane.clone(),
                                    run_id: run_id.clone(),
                                    outcome: RunEndOutcome::Failed,
                                    leaf_id: leaf.clone(),
                                }));
                                return Ok(RunResult {
                                    run_id,
                                    outcome: HarnessRunOutcome::Failed {
                                        leaf_id: leaf,
                                        error: err,
                                        final_entry_id: None,
                                        final_message: None,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        // Rebuild path after potential compaction.
        let path = self.branch_path_oldest_first().await?;
        let build_opts = Self::build_opts(&snap.entry_projectors, &snap.entry_transforms);
        let ctx = build_session_context(&path, &build_opts);
        let system_prompt = Self::compose_prompt(
            snap.system_prompt.as_deref(),
            &snap.resources,
            &snap.active_tool_names,
        );
        let tools = Self::active_tools(&snap.tools, &snap.active_tool_names);

        let agent_context = AgentContext {
            system_prompt,
            messages: ctx.messages,
            tools,
        };

        // Build the config.
        let convert = Self::build_convert_to_llm(snap.to_provider_messages);
        let stream_fn = Self::build_stream_fn(
            snap.models.clone(),
            snap.provider_hooks.clone(),
            snap.stream_options.clone(),
        )?;
        let post_compaction_cut = Arc::new(AtomicUsize::new(0));
        // Run-level turn ceiling shared with the post-loop notice below. The
        // loop's only other exit is "the model stopped asking for tools", which
        // nothing bounds — one observed session ran 112 turns / 867s in a single
        // run (`docs/llm-repetition-forensics.md` §二). Native pi has no such
        // ceiling either, so this is **off unless** `RPI_MAX_TURNS_PER_RUN=<n>`
        // asks for it; when off the hook stays `None` and the loop is unchanged.
        let run_budget = Arc::new(Mutex::new(crate::run_budget::RunBudget::from_env()));
        let should_stop_after_turn: Option<rpi_agent::ShouldStopAfterTurn> =
            if run_budget.lock().unwrap().is_enabled() {
                let budget = Arc::clone(&run_budget);
                Some(Arc::new(
                    move |_ctx: ShouldStopAfterTurnContext<'_>| -> BoxFuture<'static, bool> {
                        let budget = Arc::clone(&budget);
                        Box::pin(async move { budget.lock().unwrap().observe_turn().is_some() })
                    },
                ))
            } else {
                None
            };

        let retry_policy = snap.retry.clone();
        let mut retry_attempt = 0u32;
        // Commit-on-settle: persist an assistant message carrying tool calls as
        // soon as it is final, so the tools it calls can be recorded before they
        // run (`tool_started` requires the assistant entry to exist).
        let settle_state = Arc::new(crate::settle::SettleState::new(
            run_id.clone(),
            retry_policy.enabled,
            retry_policy.max_retries,
            snap.tools
                .iter()
                .map(|tool| {
                    // `HarnessTool` carries the harness-level policy; a record
                    // carries the persisted one. They are separate types with the
                    // same two cases, so map explicitly rather than relying on
                    // them staying in sync.
                    let replay = match tool.replay {
                        crate::types::ToolReplay::Never => ToolReplay::Never,
                        crate::types::ToolReplay::Safe => ToolReplay::Safe,
                    };
                    (tool.tool.schema().name.clone(), replay)
                })
                .collect(),
        ));
        let config = AgentLoopConfig {
            model: snap.model.clone(),
            convert_to_llm: convert,
            transform_context: snap.transform_context.clone(),
            get_api_key: None,
            should_stop_after_turn,
            prepare_next_turn: None,
            after_tool_results: {
                let harness = self.clone();
                let enabled = snap.compaction.enabled;
                let settings = snap.compaction;
                let model = snap.model.clone();
                let provider = snap
                    .models
                    .iter()
                    .find(|p| p.id() == model.provider)
                    .cloned();
                let compaction_signal = signal.clone();
                let compaction_run_id = run_id.clone();
                if enabled && model.context_window > 0 && provider.is_some() {
                    let cut_for_hook = Arc::clone(&post_compaction_cut);
                    Some(Arc::new(move |ctx: ShouldStopAfterTurnContext<'_>| -> BoxFuture<'static, Option<rpi_agent::AgentLoopTurnUpdate>> {
                        let harness = harness.clone();
                        let model = model.clone();
                        let provider = provider.clone().expect("checked above");
                        let cut = Arc::clone(&cut_for_hook);
                        let compaction_signal = compaction_signal.clone();
                        let compaction_run_id = compaction_run_id.clone();
                        let context = ctx.context.clone();
                        let new_messages_len = ctx.new_messages.len();
                        Box::pin(async move {
                            let tokens = estimate_context_tokens(&context.messages).tokens;
                            if !should_compact(tokens, model.context_window as i64, &settings) {
                                return None;
                            }
                            let entries: Vec<Entry> = context.messages.iter().enumerate().map(|(i, message)| Entry::Message(crate::session::types::MessageEntry {
                                base: crate::session::types::EntryBase { entry_type: "message".into(), id: format!("runtime-{i}"), seq: i as u64 + 1, parent_id: None, timestamp: i as i64 },
                                message: message.clone(), terminate: None,
                            })).collect();
                            let prep = match prepare_compaction(&entries, settings) { Ok(Some(p)) => p, _ => return None };
                            let opts = CompactionLlmOptions { provider, model: model.clone(), api_key: None, signal: compaction_signal, thinking_level: None, retry: None, custom_instructions: None };
                            let result = match compact(&prep, &opts).await { Ok(r) => r, Err(_) => return None };
                            // Mid-loop compaction fires because the turn pushed the
                            // context over the threshold.
                            if harness.persist_compaction_entry(&compaction_run_id, &result, CompactionReason::Threshold).await.is_err() { return None; }
                            cut.store(new_messages_len, Ordering::Release);
                            let mut messages = Vec::with_capacity(result.retained_tail.len() + 1);
                            messages.push(create_compaction_summary_message(&result.summary, result.tokens_before, 0));
                            messages.extend(result.retained_tail);
                            Some(rpi_agent::AgentLoopTurnUpdate { context: Some(AgentContext { system_prompt: context.system_prompt.clone(), messages, tools: context.tools.clone() }), model: None, thinking_level: None })
                        })
                    }) as AfterToolResults)
                } else {
                    None
                }
            },
            get_steering_messages: Some({
                let queue = Arc::clone(&snap.steering_queue);
                let harness = self.clone();
                let settle = Arc::clone(&settle_state);
                Arc::new(move || {
                    let harness = harness.clone();
                    let queue = Arc::clone(&queue);
                    let settle = Arc::clone(&settle);
                    Box::pin(async move { harness.drain_injected(queue, &settle).await })
                })
            }),
            get_follow_up_messages: Some({
                let queue = Arc::clone(&snap.follow_up_queue);
                let harness = self.clone();
                let settle = Arc::clone(&settle_state);
                Arc::new(move || {
                    let harness = harness.clone();
                    let queue = Arc::clone(&queue);
                    let settle = Arc::clone(&settle);
                    Box::pin(async move { harness.drain_injected(queue, &settle).await })
                })
            }),
            before_tool_call: snap.before_tool_call.clone(),
            after_tool_call: snap.after_tool_call.clone(),
            tool_execution: snap.tool_execution.to_agent_mode(),
            thinking_level: snap.thinking_level,
            api_key: None,
            timeout: snap.stream_options.timeout,
            // Pi's `retry.maxRetries` is an assistant-level retry budget. The
            // provider/SDK retry budget is intentionally independent and
            // defaults to zero, so transient failures are retried by the
            // harness exactly once per agent attempt.
            max_retries: None,
            max_retry_delay: Some(std::time::Duration::from_millis(
                snap.retry.max_agent_delay_ms,
            )),
            cache_retention: snap.stream_options.cache_retention.unwrap_or_default(),
            session_id: None,
            signal: signal.clone(),
        };

        // Emitter: use the caller's override if one was supplied (e.g. the TUI
        // installs a BroadcastEmitter so it can render AgentEvents live);
        // otherwise fall back to a collector that discards events (the harness
        // still surfaces RunStart/RunEnd via its own bus).
        let base_emitter: Arc<dyn AgentEmitter> = snap
            .agent_emitter
            .clone()
            .unwrap_or_else(|| Arc::new(rpi_agent::CollectorEmitter::default()));
        // Durable progress: every streamed assistant frame is appended to the
        // session as it arrives, so a crash mid-run leaves a committed prefix
        // that `salvage_run_frames` can replay instead of losing the whole run.
        // Mirrors native pi's `openFrameProgress`. See
        // `docs/llm-repetition-forensics.md` §十一.
        let frame_recorder = Arc::new(crate::frame_progress::FrameRecordingEmitter::new(
            base_emitter,
            self.session.clone(),
            self.lane.clone(),
            run_id.clone(),
        ));
        let emitter: Arc<dyn AgentEmitter> = frame_recorder.clone();

        // Drive the loop. We pass an EMPTY prompts vec to `run_agent_loop`
        // (NOT `prompts`): the prompts were already persisted to the session
        // and are present in `agent_context.messages` via the branch-path
        // build above. Passing them again would extend `current_context` a
        // second time and double-count them in the provider context.
        //
        // Consequence for persistence: `run_agent_loop` seeds `new_messages`
        // from `prompts.clone()` (empty here), so the returned `new_messages`
        // contains ONLY the loop-produced messages (assistant / tool-result /
        // assistant-final) — NO prompt prefix. `persist_new_messages` must
        // therefore persist ALL of them (skip 0). The old `skip(prompts_len)`
        // was a leftover from a design that passed `prompts` in; it wrongly
        // dropped the first real assistant message.
        let emitter: Arc<dyn AgentEmitter> = Arc::new(crate::settle::SettlingEmitter::new(
            Arc::clone(&emitter),
            Arc::clone(&frame_recorder),
            Arc::clone(&settle_state),
            self.clone(),
        ));
        let result = loop {
            // Entering from an assistant runs that message's tools as its very
            // first action, which is exactly why it is never retried (see below).
            let attempt_result = if from_assistant {
                run_agent_loop_from_assistant(
                    agent_context.clone(),
                    config.clone(),
                    Arc::clone(&emitter),
                    Arc::clone(&stream_fn),
                )
                .await
            } else {
                run_agent_loop(
                    Vec::new(),
                    agent_context.clone(),
                    config.clone(),
                    Arc::clone(&emitter),
                    Arc::clone(&stream_fn),
                )
                .await
            };

            let retry_error = attempt_result
                .as_ref()
                .ok()
                .and_then(|messages| {
                    messages.iter().rev().find_map(|message| match message {
                        AgentMessage::Assistant(assistant) => Some(assistant.as_ref()),
                        _ => None,
                    })
                })
                .filter(|assistant| {
                    assistant.stop_reason == StopReason::Error
                        && crate::compaction::is_retryable_assistant_error(assistant)
                })
                .map(|assistant| {
                    assistant
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Provider request failed.".to_string())
                });
            if !retry_policy.enabled
                || retry_attempt >= retry_policy.max_retries
                || retry_error.is_none()
                // A from-assistant attempt already ran the polled message's tools,
                // so retrying would run them a second time. One attempt only.
                || from_assistant
            {
                break attempt_result;
            }

            retry_attempt += 1;
            settle_state.set_attempt(retry_attempt);
            // Record the retry before waiting, so a crash during the wait is
            // recognised as "this run was about to retry" rather than looking like
            // an interrupted run. The reserved id is that attempt's assistant
            // entry: if it ever lands, the retry succeeded and nothing is pending.
            let retry_result_id = self.next_entry_id();
            if let Err(error) = self
                .write_retry_pending(&run_id, retry_attempt + 1, &retry_result_id)
                .await
            {
                tracing::warn!(%error, "could not record a scheduled retry");
            }
            let delay_ms = retry_policy
                .base_delay_ms
                .saturating_mul(1u64 << retry_attempt.saturating_sub(1))
                .min(retry_policy.max_agent_delay_ms);
            emitter
                .emit(AgentEvent::RetryScheduled {
                    attempt: retry_attempt,
                    max_retries: retry_policy.max_retries,
                    delay_ms,
                    error: retry_error.expect("retry eligibility requires an error"),
                })
                .await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                _ = signal.cancelled() => break attempt_result,
            }
        };

        // Persist new messages + derive outcome.
        let (leaf_id, _final_entry_id, outcome, op_outcome, op_error) = match result {
            Ok(new_messages) => {
                let cut = post_compaction_cut.load(Ordering::Acquire);
                let start = cut.min(new_messages.len());
                let committed = settle_state.committed_indices();
                let (leaf, final_id) = self
                    .persist_new_messages(&new_messages[start..], start, &committed)
                    .await?;
                // A budget stop ends the run "Completed" (the loop's normal exit
                // path), which would otherwise look like a finished task. Record
                // *why* it stopped so the user sees it in the transcript and the
                // next turn's context knows the work was cut short.
                // Bind the reason before awaiting: the `MutexGuard` is not
                // `Send`, and holding it across `append_message` would make the
                // whole run future non-`Send`.
                let budget_stop = run_budget.lock().unwrap().stop();
                if let Some(stop) = budget_stop {
                    let notice = crate::messages::create_custom_message(
                        "runBudget",
                        UserContent::Text(stop.message()),
                        true,
                        None,
                        now_ms(),
                    );
                    if let Err(error) = self.session.view(&self.lane).append_message(notice).await {
                        tracing::warn!(
                            lane = %self.lane,
                            %error,
                            "could not record the run-budget stop notice"
                        );
                    }
                }
                let outcome = Self::derive_outcome(&new_messages, leaf.clone(), final_id.clone());
                // The run's messages are durable now, so its frames are no
                // longer needed as progress. Retiring them here (rather than at
                // each `MessageEnd`) is what keeps a *failed retry attempt*'s
                // frames from being salvaged as if they were real history.
                if let Err(error) = frame_recorder.clear().await {
                    tracing::warn!(lane = %self.lane, %error, "could not clear assistant frames");
                }
                let op_outcome = match &outcome {
                    HarnessRunOutcome::Completed { .. } => OperationOutcome::Completed,
                    HarnessRunOutcome::Aborted { .. } => OperationOutcome::Aborted,
                    HarnessRunOutcome::Failed { .. } => OperationOutcome::Failed,
                    HarnessRunOutcome::Suspended { .. } => {
                        // Suspended: the operation stays open (no
                        // operation_finished). We'll skip writing it below.
                        OperationOutcome::Completed // placeholder
                    }
                };
                let op_error = match &outcome {
                    HarnessRunOutcome::Failed { error, .. } => Some(error.clone()),
                    _ => None,
                };
                (leaf, final_id, outcome, op_outcome, op_error)
            }
            Err(e) => {
                // Nothing is persisted on this path, so replay whatever frames
                // committed and record them as interrupted instead of throwing
                // away everything the model produced. Mirrors native pi's
                // `recoverAssistantGeneration`. The salvaged sequence pairs each
                // partial with synthetic error results for its unresolved tool
                // calls, so the transcript remains a valid request.
                let salvaged = match Self::resolve_interrupted_run(
                    &self.session,
                    &self.lane,
                    frame_recorder.run_id(),
                    &snap.tools,
                    &signal,
                    // The retry loop just finished; anything left is settled.
                    true,
                )
                .await
                {
                    Ok(salvaged) => salvaged,
                    Err(salvage_error) => {
                        tracing::warn!(
                            lane = %self.lane,
                            %salvage_error,
                            "could not salvage assistant frames after a failed run"
                        );
                        Vec::new()
                    }
                };
                let leaf = self
                    .session
                    .view(&self.lane)
                    .get_leaf_id()
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let mut leaf_after_salvage = leaf.clone();
                for message in salvaged {
                    match self.session.view(&self.lane).append_message(message).await {
                        Ok(id) => leaf_after_salvage = id,
                        Err(append_error) => {
                            tracing::warn!(
                                lane = %self.lane,
                                %append_error,
                                "could not persist a salvaged assistant message"
                            );
                        }
                    }
                }
                let error = OperationError {
                    code: "agent_error".into(),
                    message: e.to_string(),
                };
                let outcome = HarnessRunOutcome::Failed {
                    leaf_id: leaf_after_salvage.clone(),
                    error: error.clone(),
                    final_entry_id: None,
                    final_message: None,
                };
                (
                    Some(leaf_after_salvage),
                    None,
                    outcome,
                    OperationOutcome::Failed,
                    Some(error),
                )
            }
        };

        let is_suspended = matches!(outcome, HarnessRunOutcome::Suspended { .. });

        // Write operation_finished (skip for Suspended - operation stays open).
        if !is_suspended {
            let _ = self
                .write_operation_finished(&run_id, op_outcome, op_error)
                .await;
        }

        // Emit RunEnd (skip for Suspended - a suspended run never emits run_end).
        if let Some(run_end_outcome) = match &outcome {
            HarnessRunOutcome::Completed { .. } => Some(RunEndOutcome::Completed),
            HarnessRunOutcome::Aborted { .. } => Some(RunEndOutcome::Aborted),
            HarnessRunOutcome::Failed { .. } => Some(RunEndOutcome::Failed),
            HarnessRunOutcome::Suspended { .. } => None,
        } {
            self.bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
                lane: self.lane.clone(),
                run_id: run_id.clone(),
                outcome: run_end_outcome,
                leaf_id: leaf_id.clone().unwrap_or_default(),
            }));
        }

        Ok(RunResult { run_id, outcome })
    }

    /// Explicit compaction. Drives `prepare_compaction` + `compact` and
    /// persists the result entry.
    async fn compact_core(
        &self,
        custom_instructions: Option<String>,
    ) -> HarnessResult<CompactionResult> {
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Compaction)?;
        let _run_lease = ActiveRunLease { harness: self };
        let recovery_tools = self.get_tools().await?;
        // `true`: a run starting here retries through its own retry loop, so a
        // leftover failed attempt is settled the ordinary way.
        Self::finish_interrupted_operation(
            &self.session,
            &self.lane,
            &recovery_tools,
            &signal,
            true,
        )
        .await?;

        let source_leaf = self
            .session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;

        // Write operation_started.
        let result_entry_id = self.session.id_generator().next();
        let intent = OperationIntent::Compaction {
            custom_instructions: custom_instructions.clone(),
            result_entry_id: result_entry_id.clone(),
        };
        self.write_operation_started(&run_id, source_leaf.clone(), intent)
            .await?;

        let snap = self.snapshot_config()?;
        let path = self.branch_path_oldest_first().await?;

        let outcome = if path.is_empty() {
            CompactionOutcome::Declined {
                leaf_id: source_leaf.unwrap_or_default(),
            }
        } else {
            match prepare_compaction(&path, snap.compaction) {
                Ok(None) => CompactionOutcome::Declined {
                    leaf_id: source_leaf.unwrap_or_default(),
                },
                Ok(Some(prep)) => {
                    let provider = match Self::resolve_provider(&snap.models, &snap.model) {
                        Ok(p) => p,
                        Err(_) => match snap.models.first() {
                            Some(p) => Arc::clone(p),
                            None => {
                                let err = OperationError {
                                    code: "no_provider".into(),
                                    message: "No provider for compaction".into(),
                                };
                                let _ = self
                                    .write_operation_finished(
                                        &run_id,
                                        OperationOutcome::Failed,
                                        Some(err.clone()),
                                    )
                                    .await;
                                return Ok(CompactionResult {
                                    run_id,
                                    outcome: CompactionOutcome::Failed {
                                        leaf_id: source_leaf.unwrap_or_default(),
                                        error: err,
                                    },
                                });
                            }
                        },
                    };
                    let llm_opts = CompactionLlmOptions {
                        provider,
                        model: snap.model.clone(),
                        api_key: None,
                        signal: signal.clone(),
                        thinking_level: Some(snap.thinking_level),
                        retry: if snap.retry.enabled {
                            Some(snap.retry.clone())
                        } else {
                            None
                        },
                        custom_instructions: custom_instructions.clone(),
                    };
                    match compact(&prep, &llm_opts).await {
                        Ok(result) => {
                            let entry = self
                                .persist_compaction_entry(
                                    &run_id,
                                    &result,
                                    // `/compact` and the compaction command run when
                                    // the user asks for them.
                                    CompactionReason::Manual,
                                )
                                .await?;
                            let leaf = entry.base().id.clone();
                            let _ = self
                                .write_operation_finished(
                                    &run_id,
                                    OperationOutcome::Completed,
                                    None,
                                )
                                .await;
                            CompactionOutcome::Completed {
                                leaf_id: leaf,
                                entry,
                            }
                        }
                        Err(e) => {
                            let err = OperationError {
                                code: "compaction_failed".into(),
                                message: e.message.clone(),
                            };
                            let _ = self
                                .write_operation_finished(
                                    &run_id,
                                    OperationOutcome::Failed,
                                    Some(err.clone()),
                                )
                                .await;
                            CompactionOutcome::Failed {
                                leaf_id: source_leaf.unwrap_or_default(),
                                error: err,
                            }
                        }
                    }
                }
                Err(e) => {
                    let err = OperationError {
                        code: "prepare_failed".into(),
                        message: e.message.clone(),
                    };
                    let _ = self
                        .write_operation_finished(
                            &run_id,
                            OperationOutcome::Failed,
                            Some(err.clone()),
                        )
                        .await;
                    CompactionOutcome::Failed {
                        leaf_id: source_leaf.unwrap_or_default(),
                        error: err,
                    }
                }
            }
        };

        Ok(CompactionResult { run_id, outcome })
    }
}

/// Config snapshot cloned out of `HarnessInner` at the start of an operation.
struct ConfigSnapshot {
    model: Model,
    thinking_level: ThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<HarnessTool>,
    system_prompt: Option<String>,
    resources: AgentHarnessResources,
    stream_options: AgentHarnessStreamOptions,
    retry: RetryPolicy,
    compaction: CompactionSettings,
    tool_execution: HarnessToolExecution,
    models: Vec<Arc<dyn AiProvider>>,
    to_provider_messages: Option<ConvertToLlm>,
    entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
    entry_transforms: Vec<ContextEntryTransform>,
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
    before_tool_call: Option<BeforeToolCall>,
    after_tool_call: Option<AfterToolCall>,
    transform_context: Option<TransformContext>,
    provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
    steering_queue: SharedMessageQueue,
    follow_up_queue: SharedMessageQueue,
}

// ===========================================================================
// AgentLane impl for AgentHarness (the main lane)
// ===========================================================================

#[async_trait::async_trait]
impl AgentLane for AgentHarness {
    async fn has_pending_resume(&self) -> bool {
        self.pending_resume().is_some()
    }

    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        AgentHarness::resume_pending(self).await
    }

    fn name(&self) -> &str {
        &self.lane
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.session
            .view(&self.lane)
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult> {
        let content = if images.is_empty() {
            UserContent::Text(text.to_string())
        } else {
            let mut blocks: Vec<Content> = Vec::with_capacity(images.len() + 1);
            blocks.push(Content::text(text));
            for img in images {
                blocks.push(Content::Image(img));
            }
            UserContent::Blocks(blocks)
        };
        let message = AgentMessage::User(UserMessage::new(content, now_ms()));
        self.run_core(vec![message]).await
    }

    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult> {
        self.run_core(vec![message]).await
    }

    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        self.run_core(messages).await
    }

    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult> {
        // Scope the `std::sync::MutexGuard` so it is provably dropped before the
        // `.await` below (a !Send guard cannot cross an await point in a
        // `Send` future).
        let message = {
            let inner = self.inner.lock().unwrap();
            let skills: &[Skill] = inner.resources.skills.as_deref().unwrap_or(&[]);
            let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
                HarnessError::unknown_skill(name, format!("No skill named '{name}'"))
            })?;
            let invocation = format_skill_invocation(skill, additional_instructions);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult> {
        let message = {
            let inner = self.inner.lock().unwrap();
            let templates: &[PromptTemplate] =
                inner.resources.prompt_templates.as_deref().unwrap_or(&[]);
            let template = templates.iter().find(|t| t.name == name).ok_or_else(|| {
                HarnessError::unknown_template(name, format!("No prompt template named '{name}'"))
            })?;
            let invocation = format_prompt_template_invocation(template, args);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        self.compact_core(custom_instructions.map(|s| s.to_string()))
            .await
    }

    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        _summarize: bool,
        _custom_instructions: Option<&str>,
        _label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        let run_id = self.session.id_generator().next();
        let target = target_id.filter(|id| !id.is_empty());
        match self.session.move_lane(&self.lane, target).await {
            Ok(()) => {
                let leaf = self
                    .session
                    .view(&self.lane)
                    .get_leaf_id()
                    .await
                    .map_err(session_to_harness_err)?;
                Ok(NavigationResult {
                    run_id,
                    outcome: NavigationOutcome::Completed {
                        new_leaf_id: leaf,
                        summary_entry: None,
                    },
                })
            }
            Err(error) => Ok(NavigationResult {
                run_id,
                outcome: NavigationOutcome::Failed {
                    leaf_id: self
                        .session
                        .view(&self.lane)
                        .get_leaf_id()
                        .await
                        .ok()
                        .flatten(),
                    error: OperationError {
                        code: "navigation_failed".into(),
                        message: error.to_string(),
                    },
                },
            }),
        }
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        let (run_id, steer, follow_up) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            match &inner.active_run {
                Some(active) if active.lane == self.lane => {
                    active.signal.cancel();
                    (active.run_id.clone(), Vec::new(), Vec::new())
                }
                Some(_) => {
                    return Err(HarnessError::no_active_run(
                        &self.lane,
                        "No active run to abort on this lane",
                    ));
                }
                None => {
                    return Err(HarnessError::no_active_run(
                        &self.lane,
                        "No active run to abort",
                    ));
                }
            }
        };
        Ok(AbortResult {
            run_id,
            steer,
            follow_up,
        })
    }

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        // Mirror native pi's steering queue: enqueue unconditionally, even when
        // the run has ended. The loop drains via `getSteeringMessages` at each
        // tool-batch boundary, and a message that lands after the run finishes
        // stays queued for the next explicit run. This avoids the race between
        // `activeRun` clearing and the TUI's status check that used to send
        // messages to `next_run_queue` (which never auto-wakes).
        self.enqueue_message(message, QueueKind::Steer).await
    }

    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        // Same as steer: unconditional enqueue, drained by `getFollowUpMessages`
        // after the loop would otherwise stop.
        self.enqueue_message(message, QueueKind::FollowUp).await
    }

    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.enqueue_message(message, QueueKind::NextRun).await
    }

    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        // Scoped: the guard must not live into the awaits below (it is not
        // `Send`, and holding it across one makes the whole run future non-Send).
        let removed = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.steering_queue.lock().unwrap().remove(entry_id)
                || inner.follow_up_queue.lock().unwrap().remove(entry_id)
                || inner.next_run_queue.lock().unwrap().remove(entry_id)
        };
        if removed {
            // Record it, or the item would come back on the next start. The run id
            // comes from the enqueue, not from the current state: the reducer
            // matches them, and an item queued during a run is often cancelled
            // after that run has ended.
            match self.find_enqueue(entry_id).await {
                Some(enqueued) => {
                    if let Err(error) = self
                        .write_queue_cancelled(entry_id, enqueued.run_id.as_deref())
                        .await
                    {
                        tracing::warn!(
                            entry_id,
                            %error,
                            "could not record a queue cancellation"
                        );
                    }
                }
                None => tracing::warn!(
                    entry_id,
                    "not recording a queue cancellation: the item has no enqueue record"
                ),
            }
        }
        Ok(CancelQueuedResult {
            outcome: if removed {
                CancelQueuedOutcome::Cancelled
            } else {
                CancelQueuedOutcome::AlreadyConsumed
            },
        })
    }

    async fn queued_messages(&self) -> HarnessResult<QueuedMessages> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        let steering = inner
            .steering_queue
            .lock()
            .unwrap()
            .peek_for_lane(&self.lane);
        let follow_up = inner
            .follow_up_queue
            .lock()
            .unwrap()
            .peek_for_lane(&self.lane);
        Ok(QueuedMessages {
            steering,
            follow_up,
        })
    }

    async fn clear_queue(&self) -> HarnessResult<QueuedMessages> {
        let (steering_items, follow_up_items) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            // Bound to locals: the guards are temporaries of the tuple
            // expression otherwise, and they must not outlive this block.
            let steering = inner
                .steering_queue
                .lock()
                .unwrap()
                .take_items_for_lane(&self.lane);
            let follow_up = inner
                .follow_up_queue
                .lock()
                .unwrap()
                .take_items_for_lane(&self.lane);
            (steering, follow_up)
        };
        let mut steering = Vec::with_capacity(steering_items.len());
        let mut follow_up = Vec::with_capacity(follow_up_items.len());
        // Un-queuing is a cancellation: without the record these messages would
        // come back on the next start, even though they are in the editor now.
        for (items, out) in [
            (steering_items, &mut steering),
            (follow_up_items, &mut follow_up),
        ] {
            for item in items {
                if let Some(enqueued) = self.find_enqueue(&item.entry_id).await {
                    if let Err(error) = self
                        .write_queue_cancelled(&item.entry_id, enqueued.run_id.as_deref())
                        .await
                    {
                        tracing::warn!(
                            entry_id = %item.entry_id,
                            %error,
                            "could not record a queue clear"
                        );
                    }
                }
                out.push(queued_message_text(&item.message));
            }
        }
        Ok(QueuedMessages {
            steering,
            follow_up,
        })
    }

    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult> {
        // Scope the !Send `std::sync::MutexGuard` before the `.await`.
        let run_id = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.as_ref().map(|a| a.run_id.clone())
        };

        let record = LaneRecord::Usage(UsageRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            usage,
            cause: UsageCause::Assistant,
            run_id,
            entry_id: entry_id.map(|s| s.to_string()),
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
            details,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(RecordUsageResult)
    }

    async fn wait_for_idle(&self) -> HarnessResult<()> {
        let notify = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            match &inner.active_run {
                Some(active) => Some(Arc::clone(&active.idle)),
                None => None,
            }
        };
        if let Some(notify) = notify {
            notify.notified().await;
        }
        Ok(())
    }

    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()> {
        let is_idle = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.is_none()
        };
        if is_idle {
            callback().await;
        } else {
            self.wait_for_idle().await?;
            callback().await;
        }
        Ok(())
    }

    async fn get_model(&self) -> HarnessResult<Model> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.model.clone())
    }

    async fn set_model(&self, model: Model) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.model = model;
        Ok(())
    }

    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.thinking_level)
    }

    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.thinking_level = level;
        Ok(())
    }

    async fn get_active_tools(&self) -> HarnessResult<Vec<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.active_tool_names.clone())
    }

    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.active_tool_names = names;
        Ok(())
    }

    fn session_view(&self) -> Arc<dyn SessionTree> {
        self.session.view(&self.lane)
    }

    async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        AgentHarness::get_tip_id(self).await
    }

    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        AgentHarness::find_entries(self, query).await
    }

    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        AgentHarness::find_entry(self, query).await
    }

    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        AgentHarness::get_entry(self, id).await
    }

    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        AgentHarness::append_message(self, message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        AgentHarness::append_custom_entry(self, custom_type, data).await
    }

    async fn get_name(&self) -> HarnessResult<Option<String>> {
        AgentHarness::get_name(self).await
    }

    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        AgentHarness::set_name(self, name).await
    }

    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        AgentHarness::get_label(self, target_id).await
    }

    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        AgentHarness::set_label(self, target_id, label).await
    }

    async fn get_stats(&self) -> HarnessResult<SessionStats> {
        AgentHarness::get_stats(self).await
    }

    async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        AgentHarness::snapshot(self).await
    }

    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        AgentHarness::inspect_execution(self, tool_call_id).await
    }

    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        AgentHarness::get_result(self, run_id).await
    }

    async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        AgentHarness::accept(self, entry_id).await
    }

    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        AgentHarness::request_abort(self, run_id).await
    }

    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        AgentHarness::drive(self, operation_id).await
    }

    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        AgentHarness::resume(self, suspended_id).await
    }
}

// ===========================================================================
// LaneHandle - legacy read-oriented compatibility wrapper. New callers should
// use `AgentHarness::lane`, which returns a fully executable lane runner.
// ===========================================================================

/// A non-main lane handle. Cheap to clone (shares the harness inner state).
#[derive(Clone)]
pub struct LaneHandle {
    session: Session,
    lane: String,
    view: Arc<dyn SessionTree>,
    inner: Arc<Mutex<HarnessInner>>,
    /// Kept so non-main lanes can register listeners/watch the bus once
    /// `watch`/`events` are surfaced for non-main lanes; v1 leaves it unused.
    #[allow(dead_code)]
    bus: HarnessEventBus,
}

impl LaneHandle {
    fn runner(&self) -> AgentHarness {
        AgentHarness {
            session: self.session.clone(),
            inner: Arc::clone(&self.inner),
            bus: self.bus.clone(),
            lane: self.lane.clone(),
        }
    }
}

#[async_trait::async_trait]
impl AgentLane for LaneHandle {
    async fn has_pending_resume(&self) -> bool {
        self.runner().pending_resume().is_some()
    }

    async fn resume_pending(&self) -> HarnessResult<Option<RunResult>> {
        self.runner().resume_pending().await
    }

    fn name(&self) -> &str {
        &self.lane
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(
        &self,
        text: &str,
        images: Vec<rpi_ai::types::ImageContent>,
    ) -> HarnessResult<RunResult> {
        self.runner().prompt_text(text, images).await
    }

    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult> {
        self.runner().prompt_message(message).await
    }

    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        self.runner().prompt_messages(messages).await
    }

    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> HarnessResult<RunResult> {
        self.runner().skill(name, additional_instructions).await
    }

    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult> {
        self.runner().prompt_from_template(name, args).await
    }

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        self.runner().compact(custom_instructions).await
    }

    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        summarize: bool,
        custom_instructions: Option<&str>,
        label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        self.runner()
            .navigate_tree(target_id, summarize, custom_instructions, label)
            .await
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        self.runner().abort().await
    }

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().steer(message).await
    }

    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().follow_up(message).await
    }

    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult> {
        self.runner().next_run(message).await
    }

    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        self.runner().cancel_queued(entry_id).await
    }

    async fn queued_messages(&self) -> HarnessResult<QueuedMessages> {
        self.runner().queued_messages().await
    }

    async fn clear_queue(&self) -> HarnessResult<QueuedMessages> {
        self.runner().clear_queue().await
    }

    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<&str>,
        details: Option<JsonValue>,
    ) -> HarnessResult<RecordUsageResult> {
        // Scope the !Send `std::sync::MutexGuard` before the `.await`.
        let run_id = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.as_ref().map(|a| a.run_id.clone())
        };

        let record = LaneRecord::Usage(UsageRecord {
            base: RecordBase {
                id: self.session.id_generator().next(),
                seq: 0,
                lane: self.lane.clone(),
                timestamp: 0,
            },
            usage,
            cause: UsageCause::Assistant,
            run_id,
            entry_id: entry_id.map(|s| s.to_string()),
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
            details,
        });
        self.session
            .append_record(record)
            .await
            .map_err(session_to_harness_err)?;
        Ok(RecordUsageResult)
    }

    async fn wait_for_idle(&self) -> HarnessResult<()> {
        let notify = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            match &inner.active_run {
                Some(active) => Some(Arc::clone(&active.idle)),
                None => None,
            }
        };
        if let Some(notify) = notify {
            notify.notified().await;
        }
        Ok(())
    }

    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> HarnessResult<()> {
        let is_idle = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            inner.active_run.is_none()
        };
        if is_idle {
            callback().await;
        } else {
            self.wait_for_idle().await?;
            callback().await;
        }
        Ok(())
    }

    async fn get_model(&self) -> HarnessResult<Model> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.model.clone())
    }

    async fn set_model(&self, model: Model) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.model = model;
        Ok(())
    }

    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.thinking_level)
    }

    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.thinking_level = level;
        Ok(())
    }

    async fn get_active_tools(&self) -> HarnessResult<Vec<String>> {
        let inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        Ok(inner.active_tool_names.clone())
    }

    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        inner.active_tool_names = names;
        Ok(())
    }

    fn session_view(&self) -> Arc<dyn SessionTree> {
        Arc::clone(&self.view)
    }

    async fn get_tip_id(&self) -> HarnessResult<Option<String>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn find_entries(&self, query: &EntryQuery) -> HarnessResult<Vec<Entry>> {
        self.view
            .find_entries(query)
            .await
            .map_err(session_to_harness_err)
    }

    async fn find_entry(&self, query: &EntryQuery) -> HarnessResult<Option<Entry>> {
        self.view
            .find_entry(query)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_entry(&self, id: &str) -> HarnessResult<Option<Entry>> {
        self.view
            .get_entry(id)
            .await
            .map_err(session_to_harness_err)
    }

    async fn append_message(&self, message: AgentMessage) -> HarnessResult<String> {
        self.view
            .append_message(message)
            .await
            .map_err(session_to_harness_err)
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<JsonValue>,
    ) -> HarnessResult<String> {
        self.view
            .append_custom_entry(custom_type, data)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_name(&self) -> HarnessResult<Option<String>> {
        self.view.get_name().await.map_err(session_to_harness_err)
    }

    async fn set_name(&self, name: Option<&str>) -> HarnessResult<()> {
        self.view
            .set_name(name)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_label(&self, target_id: &str) -> HarnessResult<Option<String>> {
        self.view
            .get_label(target_id)
            .await
            .map_err(session_to_harness_err)
    }

    async fn set_label(&self, target_id: &str, label: Option<&str>) -> HarnessResult<()> {
        self.view
            .set_label(target_id, label)
            .await
            .map_err(session_to_harness_err)
    }

    async fn get_stats(&self) -> HarnessResult<SessionStats> {
        self.view.get_stats().await.map_err(session_to_harness_err)
    }

    async fn snapshot(&self) -> HarnessResult<LaneSnapshot> {
        self.runner().snapshot().await
    }

    async fn inspect_execution(
        &self,
        tool_call_id: &str,
    ) -> HarnessResult<Option<ToolExecutionInfo>> {
        self.runner().inspect_execution(tool_call_id).await
    }

    async fn get_result(&self, run_id: &str) -> HarnessResult<Option<RunResult>> {
        self.runner().get_result(run_id).await
    }

    async fn accept(&self, entry_id: &str) -> HarnessResult<bool> {
        self.runner().accept(entry_id).await
    }

    async fn request_abort(&self, run_id: &str) -> HarnessResult<bool> {
        self.runner().request_abort(run_id).await
    }

    async fn drive(&self, operation_id: &str) -> HarnessResult<DriveResult> {
        self.runner().drive(operation_id).await
    }

    async fn resume(&self, suspended_id: &str) -> HarnessResult<RunResult> {
        self.runner().resume(suspended_id).await
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use rpi_ai::types::UserMessage;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage::new(text, 0))
    }

    #[test]
    fn queue_mode_one_at_a_time_preserves_entry_order() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "main".into(),
            message: user("second"),
        });
        assert_eq!(queue.drain_for_lane("main").len(), 1);
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn queue_cancel_removes_only_the_requested_entry() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "main".into(),
            message: user("second"),
        });
        assert!(queue.remove("a"));
        assert!(!queue.remove("missing"));
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn queue_drain_isolated_by_lane() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "main-1".into(),
            lane: "main".into(),
            message: user("main"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "side-1".into(),
            lane: "side".into(),
            message: user("side"),
        });
        assert_eq!(queue.drain_for_lane("main").len(), 1);
        assert_eq!(
            queue.pending.front().map(|item| item.entry_id.as_str()),
            Some("side-1")
        );
        assert_eq!(queue.drain_for_lane("side").len(), 1);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn queue_peek_reports_lane_messages_without_consuming() {
        let mut queue = MessageQueue::new(QueueMode::All);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "side".into(),
            message: user("other"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "c".into(),
            lane: "main".into(),
            message: user("second"),
        });
        assert_eq!(queue.peek_for_lane("main"), vec!["first", "second"]);
        // Peeking must not change the queue.
        assert_eq!(queue.pending.len(), 3);
    }

    #[test]
    fn queue_take_for_lane_drains_every_lane_entry() {
        let mut queue = MessageQueue::new(QueueMode::OneAtATime);
        queue.pending.push_back(QueuedMessage {
            entry_id: "a".into(),
            lane: "main".into(),
            message: user("first"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "b".into(),
            lane: "side".into(),
            message: user("other"),
        });
        queue.pending.push_back(QueuedMessage {
            entry_id: "c".into(),
            lane: "main".into(),
            message: user("second"),
        });
        // Unlike `drain_for_lane` (which honours `QueueMode`), the dequeue
        // action clears the lane completely regardless of mode. It returns items
        // (ids included) so the caller can record each cancellation.
        let taken: Vec<String> = queue
            .take_items_for_lane("main")
            .into_iter()
            .map(|item| queued_message_text(&item.message))
            .collect();
        assert_eq!(taken, vec!["first", "second"]);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].entry_id, "b");
    }
}

// ===========================================================================
// Free helpers
// ===========================================================================

/// Map a [`SessionError`] to a [`HarnessError`]. Session storage failures
/// surface as `Io`; invalid-lane errors surface as `InvalidLane`.
fn session_to_harness_err(e: SessionError) -> HarnessError {
    match e.code {
        SessionErrorCode::InvalidLane => HarnessError::invalid_lane("main", "session", e.message),
        _ => HarnessError::io(e.message),
    }
}

/// Current time in unix milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod session_switch_tests {
    use super::*;
    use crate::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use crate::session::memory::{InMemorySessionStorage, SystemClock};
    use crate::session::session::DefaultIdGenerator;
    use crate::session::types::SessionMetadata;
    use rpi_ai::{Api, Model};
    use rpi_tools::{FileSystem, OsExecutionEnv};

    fn opts() -> AgentHarnessOptions {
        AgentHarnessOptions {
            model: Model::new("m", "m", Api::Faux, "faux", "http://x"),
            thinking_level: Default::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: Default::default(),
            retry: RetryPolicy::default(),
            compaction: CompactionSettings::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session: Session::new(
                Arc::new(InMemorySessionStorage::new(
                    SessionMetadata {
                        id: "a".into(),
                        created_at: 0,
                        parent_session_id: None,
                    },
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )),
                None,
            ),
            allow_existing_session: true,
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
        }
    }

    #[tokio::test]
    async fn create_closes_interrupted_operation_when_restoring() {
        let options = opts();
        options
            .session
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: RecordBase {
                    id: "interrupted-run".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: Vec::new(),
                    initial_messages: Vec::new(),
                    system_prompt_override: None,
                    resume_data: None,
                },
            }))
            .await
            .unwrap();

        let harness = AgentHarness::create(options).await.unwrap();

        assert!(harness
            .session()
            .find_open_operations("main", Some(1))
            .await
            .unwrap()
            .is_empty());
        let records = harness
            .session()
            .find_records(&RecordQuery {
                record_type: Some("operation_finished"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            records.as_slice(),
            [LaneRecord::OperationFinished(finished)]
                if finished.run_id == "interrupted-run"
                    && finished.outcome == OperationOutcome::Aborted
        ));
    }

    /// `set_session` swaps the durable backing: the harness's `session()`
    /// facade (shared with lane handles) then reads the NEW storage's entries
    /// — the TUI `/session` hot-switch path.
    #[tokio::test]
    async fn set_session_swaps_durable_backing() {
        let h = AgentHarness::create(opts()).await.unwrap();
        // Build two JSONL sessions with one user message each.
        let tmp =
            std::env::temp_dir().join(format!("rpi-set-session-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_string_lossy().into_owned();
        let env = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));
        let fs: Arc<dyn FileSystem> = env.clone();
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs,
            sessions_root: tmp.join("sessions").to_string_lossy().into_owned(),
            clock: Arc::new(SystemClock),
            ids: Arc::new(DefaultIdGenerator::new()),
        });
        let s1 = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-one".into()),
                parent_session_id: None,
                cwd: cwd.clone(),
                metadata: None,
            })
            .await
            .unwrap();
        let s2 = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some("sess-two".into()),
                parent_session_id: None,
                cwd,
                metadata: None,
            })
            .await
            .unwrap();
        let sess1 = Session::new(Arc::new(s1), None);
        let sess2 = Session::new(Arc::new(s2), None);
        sess1
            .append_message(rpi_agent::AgentMessage::User(
                rpi_ai::types::UserMessage::new(String::from("alpha"), 1),
            ))
            .await
            .unwrap();
        sess2
            .append_message(rpi_agent::AgentMessage::User(
                rpi_ai::types::UserMessage::new(String::from("beta"), 2),
            ))
            .await
            .unwrap();

        h.set_session(sess1).await.unwrap();
        let e1 = h
            .session()
            .view("main")
            .find_entries(&EntryQuery {
                entry_type: None,
                custom_type: None,
                order: None,
                limit: None,
                cursor: None,
            })
            .await
            .unwrap();
        assert_eq!(e1.len(), 1);

        h.set_session(sess2).await.unwrap();
        let e2 = h
            .session()
            .view("main")
            .find_entries(&EntryQuery {
                entry_type: None,
                custom_type: None,
                order: None,
                limit: None,
                cursor: None,
            })
            .await
            .unwrap();
        assert_eq!(e2.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn non_main_lane_runner_persists_prompt_on_its_branch() {
        let h = AgentHarness::create(opts()).await.unwrap();
        h.session().create_lane("side", None).await.unwrap();
        let side = h.lane("side");

        let result = side.prompt_text("side branch", Vec::new()).await.unwrap();
        assert!(matches!(result.outcome, HarnessRunOutcome::Failed { .. }));

        let entries = side
            .session_view()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Message(message)
                    if matches!(
                        &message.message,
                        AgentMessage::User(user)
                            if matches!(&user.content, UserContent::Text(text) if text == "side branch")
                    )
            )
        }));
    }
}
