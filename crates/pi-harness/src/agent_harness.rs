//! Mirrors `packages/agent/src/harness/agent-harness.ts` — the `AgentHarness`
//! run loop, `AgentLane` trait, and `LaneHandle`.
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
//! - `AgentLane` trait + [`LaneHandle`] (non-main lanes delegate reads to a
//!   [`SessionTree`] view; the run loop is main-lane only for v1).
//!
//! Outcome mapping: `run_agent_loop` returns `Result<NewMessages, AgentError>`.
//! The terminal assistant message's `stop_reason` decides
//! `Completed`/`Aborted`/`Failed`; `Deferred` + a `deferred` handle produces
//! `RunOutcome::Suspended` (the operation stays open until `resume`). All
//! rejections are `Result<T, HarnessError>`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use rpi_ai::types::{
    AssistantMessage, Content, DeferredHandle, StopReason, ThinkingLevel, Usage, UserContent,
    UserMessage,
};
use rpi_ai::{Model, Provider as AiProvider};
use rpi_agent::message::AgentMessage;
use rpi_agent::{
    run_agent_loop, AgentContext, AgentEmitter, AgentLoopConfig, AgentTool, ConvertToLlm,
    StreamFn,
};

use crate::compaction::{
    compact, estimate_context_tokens, prepare_compaction, should_compact, CompactionLlmOptions,
};
use crate::error::{SessionError, SessionErrorCode};
use crate::events::{
    HarnessEvent, HarnessEventBus, RunEndEvent, RunEndOutcome, RunStartEvent,
};
use crate::messages::convert_to_llm as harness_convert_to_llm;
use crate::result::{HarnessError, HarnessResult, OperationKind};
use crate::session::context::{
    build_session_context, CustomEntryContextMessageProjector, SessionContextBuildOptions,
};
use crate::session::session::Session;
use crate::session::types::{
    BranchBounds, Entry, EntryOrder, EntryQuery, JsonValue, LaneRecord, OperationError,
    OperationFinishedRecord, OperationIntent, OperationOutcome, OperationStartedRecord,
    ProvisionedEntry, ProvisionedKind, RecordBase, RecordQuery, SessionTree, UsageCause,
    UsageRecord,
};
use crate::skills::format_skill_invocation;
use crate::prompt_templates::format_prompt_template_invocation;
use crate::system_prompt::compose_system_prompt;
use crate::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessStreamOptions, CompactionSettings,
    DrivingMode, HarnessTool, HarnessToolExecution, PromptTemplate, RetryPolicy, Skill,
};

// ===========================================================================
// Outcomes — mirror the TS RunOutcome / CompactionOutcome / NavigationOutcome
// unions (agent-harness.ts:89-103). Each carries the lane + terminal ids.
// ===========================================================================

/// `RunOutcome` (harness-level). Mirrors TS `RunOutcome`. `leaf_id`/`final_entry_id`
/// name the lane's leaf + final persisted entry; `final_message` is the terminal
/// assistant message. `Suspended` carries the provider deferred handle.
#[derive(Debug, Clone)]
pub enum HarnessRunOutcome {
    Completed { leaf_id: String, final_entry_id: String, final_message: rpi_ai::types::AssistantMessage },
    Aborted { leaf_id: String, final_entry_id: String, final_message: rpi_ai::types::AssistantMessage },
    Failed { leaf_id: String, error: OperationError, final_entry_id: Option<String>, final_message: Option<rpi_ai::types::AssistantMessage> },
    Suspended { leaf_id: String, final_entry_id: String, deferred: DeferredHandle },
}

/// `CompactionOutcome`. Mirrors TS `CompactionOutcome`.
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    Completed { leaf_id: String, entry: Entry },
    Declined { leaf_id: String },
    Aborted { leaf_id: String },
    Failed { leaf_id: String, error: OperationError },
}

/// `NavigationOutcome`. Mirrors TS `NavigationOutcome`. v1 only emits
/// `Declined` on a no-op navigation (the real branch navigation loop is
/// deferred).
#[derive(Debug, Clone)]
pub enum NavigationOutcome {
    Completed { new_leaf_id: Option<String>, summary_entry: Option<Entry> },
    Declined { leaf_id: Option<String> },
    Aborted { leaf_id: Option<String> },
    Failed { leaf_id: Option<String>, error: OperationError },
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

    async fn prompt_text(&self, text: &str, images: Vec<rpi_ai::types::ImageContent>) -> HarnessResult<RunResult>;
    async fn prompt_message(&self, message: AgentMessage) -> HarnessResult<RunResult>;
    async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> HarnessResult<RunResult>;
    async fn skill(&self, name: &str, additional_instructions: Option<&str>) -> HarnessResult<RunResult>;
    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult>;

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult>;
    async fn navigate_tree(&self, target_id: Option<&str>, summarize: bool, custom_instructions: Option<&str>, label: Option<&str>) -> HarnessResult<NavigationResult>;

    async fn abort(&self) -> HarnessResult<AbortResult>;

    async fn steer(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn follow_up(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn next_run(&self, message: AgentMessage) -> HarnessResult<QueueResult>;
    async fn cancel_queued(&self, entry_id: &str) -> HarnessResult<CancelQueuedResult>;
    async fn record_usage(&self, usage: Usage, entry_id: Option<&str>, details: Option<JsonValue>) -> HarnessResult<RecordUsageResult>;

    async fn wait_for_idle(&self) -> HarnessResult<()>;
    async fn run_when_idle(&self, callback: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>) -> HarnessResult<()>;

    async fn get_model(&self) -> HarnessResult<Model>;
    async fn set_model(&self, model: Model) -> HarnessResult<()>;
    async fn get_thinking_level(&self) -> HarnessResult<ThinkingLevel>;
    async fn set_thinking_level(&self, level: ThinkingLevel) -> HarnessResult<()>;
    async fn get_active_tools(&self) -> HarnessResult<Vec<String>>;
    async fn set_active_tools(&self, names: Vec<String>) -> HarnessResult<()>;

    fn session_view(&self) -> Arc<dyn SessionTree>;
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
    signal: CancellationToken,
    idle: Arc<tokio::sync::Notify>,
    kind: OperationKind,
}

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
    /// Optional emitter override; see [`AgentHarnessOptions::agent_emitter`].
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
    closed: bool,
    active_run: Option<ActiveRun>,
}

// ===========================================================================
// AgentHarness - the main lane. Drives run_agent_loop over the session
// branch path, persists entries/records, emits RunStart/RunEnd.
// ===========================================================================

/// The harness. Implements [`AgentLane`] for the `"main"` lane. Cheap to
/// [`Clone`] (all state is behind `Arc`); clones share the same session +
/// event bus + inner state.
#[derive(Clone)]
pub struct AgentHarness {
    session: Session,
    inner: Arc<Mutex<HarnessInner>>,
    bus: HarnessEventBus,
}

impl AgentHarness {
    /// Construct from options. Mirrors TS `AgentHarness.create` minus the
    /// restore path: if the session already has records (a prior operation),
    /// reject with [`HarnessError::Io`] (restore is not implemented in v1).
    pub async fn create(options: AgentHarnessOptions) -> HarnessResult<Self> {
        // TS `create`: `findRecords({ limit: 1 })` non-empty -> throw
        // `HarnessNotImplemented("create.restore")`. v1 surfaces this as
        // `HarnessError::Io`.
        let existing = options
            .session
            .find_records(&RecordQuery { limit: Some(1), ..Default::default() })
            .await
            .map_err(session_to_harness_err)?;
        if !existing.is_empty() {
            return Err(HarnessError::io(
                "create.restore is not implemented: session already has records",
            ));
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
            agent_emitter: options.agent_emitter,
            closed: false,
            active_run: None,
        };

        Ok(Self {
            session: options.session,
            inner: Arc::new(Mutex::new(inner)),
            bus: HarnessEventBus::new(),
        })
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

    /// Return a lane handle. `"main"` returns a clone of self (the main lane);
    /// any other lane returns a [`LaneHandle`] backed by the session's lane
    /// view. The lane must already exist in the session.
    pub fn lane(&self, name: &str) -> Arc<dyn AgentLane> {
        if name == "main" {
            Arc::new(self.clone())
        } else {
            Arc::new(LaneHandle {
                session: self.session.clone(),
                lane: name.to_string(),
                view: self.session.view(name),
                inner: Arc::clone(&self.inner),
                bus: self.bus.clone(),
            })
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
        let active = active_names.unwrap_or_else(|| {
            tools.iter().map(|t| t.tool.schema().name.clone()).collect()
        });
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

    // -- private helpers ----------------------------------------------------

    /// Reject if closed or if `main` already has an active operation. On
    /// success, installs the `ActiveRun` guard and returns `(run_id, signal,
    /// idle_notify)`.
    fn acquire_run(&self, kind: OperationKind) -> HarnessResult<(String, CancellationToken, Arc<tokio::sync::Notify>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(HarnessError::closed());
        }
        if let Some(active) = &inner.active_run {
            return Err(HarnessError::lane_busy(
                "main",
                active.run_id.clone(),
                active.kind,
                format!("Lane main already has an active {} operation", active.kind.as_str()),
            ));
        }
        let run_id = self.session.id_generator().next();
        let signal = CancellationToken::new();
        let idle = Arc::new(tokio::sync::Notify::new());
        inner.active_run = Some(ActiveRun {
            run_id: run_id.clone(),
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

    /// Resolve the provider for `model.provider`. Returns the first registered
    /// provider whose `id()` matches.
    fn resolve_provider(models: &[Arc<dyn AiProvider>], model: &Model) -> HarnessResult<Arc<dyn AiProvider>> {
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
    fn build_stream_fn(models: Vec<Arc<dyn AiProvider>>) -> HarnessResult<StreamFn> {
        // Resolve the provider lazily per call (the model may change between
        // turns via prepare_next_turn). If no provider matches, emit an Error
        // terminal event on a synthetic stream.
        let stream_fn = rpi_agent::stream_fn(move |model: &Model, ctx: &rpi_ai::types::Context, opts: &rpi_ai::SimpleStreamOptions| {
            let provider = models.iter().find(|p| p.id() == model.provider).cloned();
            match provider {
                Some(p) => {
                    let model = model.clone();
                    let ctx = ctx.clone();
                    let opts = opts.clone();
                    tokio::task::block_in_place(|| {
                        Handle::current().block_on(async move {
                            p.stream_simple(&model, &ctx, &opts).await
                        })
                    })
                }
                None => {
                    // No provider: synthesize an Error terminal stream.
                    let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
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
        });
        Ok(stream_fn)
    }

    /// Build the `ConvertToLlm` Arc for `AgentLoopConfig`: use the caller's
    /// override if provided, else wrap the harness-level `convert_to_llm`.
    fn build_convert_to_llm(
        to_provider_messages: Option<ConvertToLlm>,
    ) -> ConvertToLlm {
        to_provider_messages.unwrap_or_else(|| {
            Arc::new(|messages: Vec<AgentMessage>| {
                let out = harness_convert_to_llm(messages);
                Box::pin(async move { out })
            })
        })
    }

    /// Build the `SessionContextBuildOptions` from the entry projectors.
    fn build_opts(projectors: &BTreeMap<String, CustomEntryContextMessageProjector>) -> SessionContextBuildOptions {
        SessionContextBuildOptions {
            entry_transforms: Vec::new(),
            entry_projectors: projectors.clone(),
        }
    }

    /// Load the main-lane branch path, oldest-first. Returns an empty vec when
    /// the lane is empty (no leaf).
    async fn branch_path_oldest_first(&self) -> HarnessResult<Vec<Entry>> {
        let leaf = self
            .session
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
        let bounds = BranchBounds { start: Some(leaf_id), ..Default::default() };
        self.session
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
            agent_emitter: inner.agent_emitter.clone(),
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

    /// Compose the system prompt: base prompt + skills listing.
    fn compose_prompt(base: Option<&str>, resources: &AgentHarnessResources) -> String {
        let skills: &[Skill] = resources.skills.as_deref().unwrap_or(&[]);
        compose_system_prompt(base, skills)
    }

    /// Persist a `Compaction` entry and return the stamped `Entry`.
    async fn persist_compaction_entry(
        &self,
        result: &crate::compaction::CompactResult,
    ) -> HarnessResult<Entry> {
        let id = self.session.id_generator().next();
        let details = result.details.as_ref().map(|d| serde_json::to_value(d).unwrap_or(JsonValue::Null));
        let entry = ProvisionedEntry {
            id,
            kind: ProvisionedKind::Compaction {
                summary: result.summary.clone(),
                retained_tail: result.retained_tail.clone(),
                tokens_before: result.tokens_before,
                details,
                usage: result.usage.clone(),
            },
        };
        self.session
            .append_entry(entry, "main")
            .await
            .map_err(session_to_harness_err)
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
                lane: "main".to_string(),
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
                lane: "main".to_string(),
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
    ) -> HarnessResult<(Option<String>, Option<String>)> {
        let mut leaf_id = self
            .session
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let mut final_entry_id: Option<String> = leaf_id.clone();
        for msg in new_messages {
            let id = self
                .session
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
                final_entry_id: if final_id.is_empty() { None } else { Some(final_id) },
                final_message: None,
            },
            Some(am) => match am.stop_reason {
                StopReason::Error => HarnessRunOutcome::Failed {
                    leaf_id: leaf,
                    error: OperationError {
                        code: "error".into(),
                        message: am.error_message.clone().unwrap_or_default(),
                    },
                    final_entry_id: if final_id.is_empty() { None } else { Some(final_id) },
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
                            message: "Assistant returned Deferred stop reason without a handle".into(),
                        },
                        final_entry_id: if final_id.is_empty() { None } else { Some(final_id) },
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
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Run)?;

        // Snapshot the source leaf (before persisting prompts) for the
        // operation_started record.
        let source_leaf = self
            .session
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;

        // Emit RunStart.
        self.bus.emit(&HarnessEvent::RunStart(RunStartEvent {
            lane: "main".into(),
            run_id: run_id.clone(),
        }));

        // Write operation_started.
        let intent = OperationIntent::Run {
            original_prompt: prompts.clone(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        };
        self.write_operation_started(&run_id, source_leaf.clone(), intent)
            .await?;

        // Persist prompts (the user's input) BEFORE the run so they're durable.
        for msg in &prompts {
            self.session
                .append_message(msg.clone())
                .await
                .map_err(session_to_harness_err)?;
        }

        // Snapshot config.
        let snap = self.snapshot_config()?;
        if snap.models.is_empty() {
            self.release_run();
            let _ = self.write_operation_finished(
                &run_id,
                OperationOutcome::Failed,
                Some(OperationError {
                    code: "no_provider".into(),
                    message: "No models/providers configured".into(),
                }),
            ).await;
            let leaf = source_leaf.unwrap_or_default();
            self.bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
                lane: "main".into(),
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
        let build_opts = Self::build_opts(&snap.entry_projectors);

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
                            snap.models.first().cloned().ok_or_else(|| {
                                HarnessError::agent("No provider for compaction")
                            })
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
                                let _ = self.persist_compaction_entry(&result).await?;
                            }
                            Err(e) => {
                                // Compaction failed; abort the run.
                                self.release_run();
                                let leaf = self.session.get_leaf_id().await.ok().flatten().unwrap_or_default();
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
                                    lane: "main".into(),
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
        let build_opts = Self::build_opts(&snap.entry_projectors);
        let ctx = build_session_context(&path, &build_opts);
        let system_prompt = Self::compose_prompt(snap.system_prompt.as_deref(), &snap.resources);
        let tools = Self::active_tools(&snap.tools, &snap.active_tool_names);

        let agent_context = AgentContext {
            system_prompt,
            messages: ctx.messages,
            tools,
        };

        // Build the config.
        let convert = Self::build_convert_to_llm(snap.to_provider_messages);
        let stream_fn = Self::build_stream_fn(snap.models.clone())?;
        let config = AgentLoopConfig {
            model: snap.model.clone(),
            convert_to_llm: convert,
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            tool_execution: snap.tool_execution.to_agent_mode(),
            thinking_level: snap.thinking_level,
            api_key: None,
            timeout: snap.stream_options.timeout,
            max_retries: if snap.retry.enabled {
                Some(snap.retry.max_retries)
            } else {
                None
            },
            max_retry_delay: Some(std::time::Duration::from_millis(
                snap.retry.base_delay_ms,
            )),
            cache_retention: snap.stream_options.cache_retention.unwrap_or_default(),
            session_id: None,
            signal: signal.clone(),
        };

        // Emitter: use the caller's override if one was supplied (e.g. the TUI
        // installs a BroadcastEmitter so it can render AgentEvents live);
        // otherwise fall back to a collector that discards events (the harness
        // still surfaces RunStart/RunEnd via its own bus).
        let emitter: Arc<dyn AgentEmitter> = snap.agent_emitter.clone()
            .unwrap_or_else(|| Arc::new(rpi_agent::CollectorEmitter::default()));

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
        let result = run_agent_loop(Vec::new(), agent_context, config, emitter, stream_fn).await;

        // Persist new messages + derive outcome.
        let (leaf_id, _final_entry_id, outcome, op_outcome, op_error) = match result {
            Ok(new_messages) => {
                let (leaf, final_id) = self.persist_new_messages(&new_messages).await?;
                let outcome = Self::derive_outcome(&new_messages, leaf.clone(), final_id.clone());
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
                let leaf = self.session.get_leaf_id().await.ok().flatten().unwrap_or_default();
                let error = OperationError {
                    code: "agent_error".into(),
                    message: e.to_string(),
                };
                let outcome = HarnessRunOutcome::Failed {
                    leaf_id: leaf.clone(),
                    error: error.clone(),
                    final_entry_id: None,
                    final_message: None,
                };
                (Some(leaf), None, outcome, OperationOutcome::Failed, Some(error))
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
                lane: "main".into(),
                run_id: run_id.clone(),
                outcome: run_end_outcome,
                leaf_id: leaf_id.clone().unwrap_or_default(),
            }));
        }

        // Release the run guard.
        self.release_run();

        Ok(RunResult { run_id, outcome })
    }

    /// Explicit compaction. Drives `prepare_compaction` + `compact` and
    /// persists the result entry.
    async fn compact_core(&self, custom_instructions: Option<String>) -> HarnessResult<CompactionResult> {
        let (run_id, signal, _idle) = self.acquire_run(OperationKind::Compaction)?;

        let source_leaf = self
            .session
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
                                self.release_run();
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
                            let entry = self.persist_compaction_entry(&result).await?;
                            let leaf = entry.base().id.clone();
                            let _ = self
                                .write_operation_finished(&run_id, OperationOutcome::Completed, None)
                                .await;
                            CompactionOutcome::Completed { leaf_id: leaf, entry }
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
                        .write_operation_finished(&run_id, OperationOutcome::Failed, Some(err.clone()))
                        .await;
                    CompactionOutcome::Failed {
                        leaf_id: source_leaf.unwrap_or_default(),
                        error: err,
                    }
                }
            }
        };

        self.release_run();
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
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
}

// ===========================================================================
// AgentLane impl for AgentHarness (the main lane)
// ===========================================================================

#[async_trait::async_trait]
impl AgentLane for AgentHarness {
    fn name(&self) -> &str {
        "main"
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.session
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(&self, text: &str, images: Vec<rpi_ai::types::ImageContent>) -> HarnessResult<RunResult> {
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

    async fn skill(&self, name: &str, additional_instructions: Option<&str>) -> HarnessResult<RunResult> {
        // Scope the `std::sync::MutexGuard` so it is provably dropped before the
        // `.await` below (a !Send guard cannot cross an await point in a
        // `Send` future).
        let message = {
            let inner = self.inner.lock().unwrap();
            let skills: &[Skill] = inner.resources.skills.as_deref().unwrap_or(&[]);
            let skill = skills
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| HarnessError::unknown_skill(name, format!("No skill named '{name}'")))?;
            let invocation = format_skill_invocation(skill, additional_instructions);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn prompt_from_template(&self, name: &str, args: &[String]) -> HarnessResult<RunResult> {
        let message = {
            let inner = self.inner.lock().unwrap();
            let templates: &[PromptTemplate] = inner
                .resources
                .prompt_templates
                .as_deref()
                .unwrap_or(&[]);
            let template = templates
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| {
                    HarnessError::unknown_template(name, format!("No prompt template named '{name}'"))
                })?;
            let invocation = format_prompt_template_invocation(template, args);
            AgentMessage::User(UserMessage::new(invocation, now_ms()))
        };
        self.run_core(vec![message]).await
    }

    async fn compact(&self, custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        self.compact_core(custom_instructions.map(|s| s.to_string())).await
    }

    async fn navigate_tree(
        &self,
        _target_id: Option<&str>,
        _summarize: bool,
        _custom_instructions: Option<&str>,
        _label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        // v1: navigation is deferred (see docs/m5f-open-questions.md). Return
        // Declined with the current leaf.
        let leaf = self
            .session
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let run_id = self.session.id_generator().next();
        Ok(NavigationResult {
            run_id,
            outcome: NavigationOutcome::Declined { leaf_id: leaf },
        })
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        let (run_id, steer, follow_up) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(HarnessError::closed());
            }
            match &inner.active_run {
                Some(active) => {
                    active.signal.cancel();
                    (active.run_id.clone(), Vec::new(), Vec::new())
                }
                None => {
                    return Err(HarnessError::no_active_run(
                        "main",
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

    async fn steer(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        // v1: steering queue is deferred (see docs/m5f-open-questions.md).
        Err(HarnessError::no_active_run(
            "main",
            "Steering is not implemented in v1",
        ))
    }

    async fn follow_up(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        Err(HarnessError::no_active_run(
            "main",
            "Follow-up queue is not implemented in v1",
        ))
    }

    async fn next_run(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        Err(HarnessError::no_active_run(
            "main",
            "Next-run queue is not implemented in v1",
        ))
    }

    async fn cancel_queued(&self, _entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        Err(HarnessError::no_active_run(
            "main",
            "Queue cancellation is not implemented in v1",
        ))
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
                lane: "main".to_string(),
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
        self.session.view("main")
    }
}

// ===========================================================================
// LaneHandle - non-main lanes. Delegates reads to the session's SessionTree
// view; run ops reject with InvalidLane (the run loop is main-lane only in
// v1). Get/set accessors + record_usage + wait_for_idle delegate to the
// shared inner state (harness-wide config).
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

#[async_trait::async_trait]
impl AgentLane for LaneHandle {
    fn name(&self) -> &str {
        &self.lane
    }

    async fn get_leaf_id(&self) -> HarnessResult<Option<String>> {
        self.view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)
    }

    async fn prompt_text(&self, _text: &str, _images: Vec<rpi_ai::types::ImageContent>) -> HarnessResult<RunResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "The run loop is only available on the main lane in v1",
        ))
    }

    async fn prompt_message(&self, _message: AgentMessage) -> HarnessResult<RunResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "The run loop is only available on the main lane in v1",
        ))
    }

    async fn prompt_messages(&self, _messages: Vec<AgentMessage>) -> HarnessResult<RunResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "The run loop is only available on the main lane in v1",
        ))
    }

    async fn skill(&self, _name: &str, _additional_instructions: Option<&str>) -> HarnessResult<RunResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "Skills are only available on the main lane in v1",
        ))
    }

    async fn prompt_from_template(&self, _name: &str, _args: &[String]) -> HarnessResult<RunResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "Prompt templates are only available on the main lane in v1",
        ))
    }

    async fn compact(&self, _custom_instructions: Option<&str>) -> HarnessResult<CompactionResult> {
        Err(HarnessError::invalid_lane(
            self.lane.clone(),
            "non_main_lane",
            "Compaction is only available on the main lane in v1",
        ))
    }

    async fn navigate_tree(
        &self,
        _target_id: Option<&str>,
        _summarize: bool,
        _custom_instructions: Option<&str>,
        _label: Option<&str>,
    ) -> HarnessResult<NavigationResult> {
        let leaf = self
            .view
            .get_leaf_id()
            .await
            .map_err(session_to_harness_err)?;
        let run_id = self.session.id_generator().next();
        Ok(NavigationResult {
            run_id,
            outcome: NavigationOutcome::Declined { leaf_id: leaf },
        })
    }

    async fn abort(&self) -> HarnessResult<AbortResult> {
        Err(HarnessError::no_active_run(
            self.lane.clone(),
            "No active run on this lane",
        ))
    }

    async fn steer(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        Err(HarnessError::no_active_run(
            self.lane.clone(),
            "Steering is not implemented in v1",
        ))
    }

    async fn follow_up(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        Err(HarnessError::no_active_run(
            self.lane.clone(),
            "Follow-up queue is not implemented in v1",
        ))
    }

    async fn next_run(&self, _message: AgentMessage) -> HarnessResult<QueueResult> {
        Err(HarnessError::no_active_run(
            self.lane.clone(),
            "Next-run queue is not implemented in v1",
        ))
    }

    async fn cancel_queued(&self, _entry_id: &str) -> HarnessResult<CancelQueuedResult> {
        Err(HarnessError::no_active_run(
            self.lane.clone(),
            "Queue cancellation is not implemented in v1",
        ))
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
}

// ===========================================================================
// Free helpers
// ===========================================================================

/// Map a [`SessionError`] to a [`HarnessError`]. Session storage failures
/// surface as `Io`; invalid-lane errors surface as `InvalidLane`.
fn session_to_harness_err(e: SessionError) -> HarnessError {
    match e.code {
        SessionErrorCode::InvalidLane => {
            HarnessError::invalid_lane("main", "session", e.message)
        }
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