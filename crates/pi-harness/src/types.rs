//! Mirrors `packages/agent/src/harness/types.ts` (harness-level surface only).
//!
//! The TS `types.ts` is split: a `FileSystem`/`Shell`/`ExecutionEnv` section
//! (ported in M4 to `pi-tools/src/env.rs`) and a harness-configuration section
//! (`Skill`, `PromptTemplate`, `AgentHarnessResources`, `AgentHarnessTool`,
//! `AgentHarnessStreamOptions` + `Patch`, `AgentHarnessOptions`, `DrivingMode`).
//! This module mirrors the latter; it re-exports the M4 env types via `rpi_tools`
//! where the harness needs them.
//!
//! `Result`/`ok`/`err`/`getOrThrow`/`toError` from the same TS file are dropped —
//! Rust has `Result` natively and `?`/`.map_err` replace the helpers.

use std::collections::BTreeMap;
use std::sync::Arc;

use rpi_ai::{CacheRetention, Model, Provider, SimpleStreamOptions, ThinkingLevel};
use rpi_agent::{AgentTool, ConvertToLlm, QueueMode};
use serde::{Deserialize, Serialize};

use crate::session::context::CustomEntryContextMessageProjector;
use crate::session::Session;

/// A skill loaded from a `SKILL.md` file or provided by an application. Mirrors
/// TS `Skill`.
///
/// `name`/`description`/`file_path` are inserted into the system prompt in an
/// XML-formatted block (see `skills.rs::format_skills_for_system_prompt`).
/// `disable_model_invocation` excludes the skill from model-visible listings
/// while still allowing explicit application invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    /// Absolute path to the skill file. Used for model-visible location and
    /// resolving relative references in invocations.
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_model_invocation: Option<bool>,
}

/// A prompt template that can be formatted into a prompt for explicit
/// invocation. Mirrors TS `PromptTemplate`. Argument placeholders are
/// substituted by `prompt_templates.rs::substitute_args`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content: String,
}

/// Resources made available to explicit invocation methods and system-prompt
/// callbacks. Mirrors TS `AgentHarnessResources`. Both fields clone-on-get per
/// the defensive-copy contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentHarnessResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_templates: Option<Vec<PromptTemplate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Skill>>,
}

impl AgentHarnessResources {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// A tool definition executed by the harness with an application-defined
/// context. Mirrors TS `AgentHarnessTool` = `Omit<AgentTool, "execute"> & {
/// execute(..., context) }`.
///
/// The Rust port stays an `AgentTool` at the schema level (the harness stores
/// `Arc<dyn AgentTool>` for the loop) and layers a separate
/// `AgentHarnessToolContextSource` + plumbing in `agent_harness.rs` that injects
/// the per-turn context before delegating `execute`. This type captures the
/// harness-specific *replay* flag the TS union adds (`replay?: "never"|"safe"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolReplay {
    Never,
    #[default]
    Safe,
}

/// A harness tool = an `AgentTool` plus a replay policy. The harness registry
/// stores `HarnessTool` and projects the `AgentTool` to the loop.
#[derive(Clone)]
pub struct HarnessTool {
    pub tool: Arc<dyn AgentTool>,
    pub replay: ToolReplay,
}

impl HarnessTool {
    pub fn new(tool: Arc<dyn AgentTool>) -> Self {
        Self { tool, replay: ToolReplay::default() }
    }
    pub fn with_replay(mut self, replay: ToolReplay) -> Self {
        self.replay = replay;
        self
    }
}

/// Convert a `HarnessTool` replay policy to the session record variant.
impl From<ToolReplay> for crate::session::types::ToolReplay {
    fn from(r: ToolReplay) -> Self {
        match r {
            ToolReplay::Never => crate::session::types::ToolReplay::Never,
            ToolReplay::Safe => crate::session::types::ToolReplay::Safe,
        }
    }
}

/// Static tool context or zero-argument provider resolved for each turn
/// snapshot. Mirrors TS `AgentHarnessToolContextSource`.
pub enum AgentHarnessToolContextSource<T: Send + Sync + 'static> {
    Static(T),
    Provider(Arc<dyn Fn() -> futures::future::BoxFuture<'static, T> + Send + Sync>),
}

/// Curated provider request options owned by the harness and snapshotted per
/// turn. Mirrors TS `AgentHarnessStreamOptions`. Subset of `SimpleStreamOptions`
/// the harness lets the caller pin; the harness fills the rest (api key, signal,
/// reasoning, thinking_budgets, session_id) per turn.
#[derive(Debug, Clone, Default)]
pub struct AgentHarnessStreamOptions {
    pub timeout: Option<std::time::Duration>,
    pub max_retries: Option<u32>,
    pub max_retry_delay: Option<std::time::Duration>,
    pub headers: Option<BTreeMap<String, String>>,
    pub metadata: Option<BTreeMap<String, String>>,
    pub cache_retention: Option<CacheRetention>,
}

/// The harness's pinned subset folded into a real [`SimpleStreamOptions`] per
/// turn. `Option<CacheRetention>` defaults when unset so the caller only pins
/// the fields they care about.
impl AgentHarnessStreamOptions {
    /// Merge these pinned options into a base `SimpleStreamOptions`, with this
    /// set taking precedence on `Some` fields.
    pub fn merge_into(&self, mut base: SimpleStreamOptions) -> SimpleStreamOptions {
        if self.timeout.is_some() {
            base.timeout = self.timeout;
        }
        if self.max_retries.is_some() {
            base.max_retries = self.max_retries;
        }
        if self.max_retry_delay.is_some() {
            base.max_retry_delay = self.max_retry_delay;
        }
        // headers / metadata merge (overlay keys), per the TS Patch semantics.
        if let Some(h) = &self.headers {
            let mut merged = base.headers.unwrap_or_default();
            for (k, v) in h {
                merged.insert(k.clone(), v.clone());
            }
            base.headers = Some(merged);
        }
        if let Some(m) = &self.metadata {
            let mut merged = base.metadata.unwrap_or_default();
            for (k, v) in m {
                merged.insert(k.clone(), v.clone());
            }
            base.metadata = Some(merged);
        }
        if let Some(c) = self.cache_retention {
            base.cache_retention = c;
        }
        base
    }
}

/// Per-request stream-option patch returned by provider hooks. Mirrors TS
/// `AgentHarnessStreamOptionsPatch` — `None` means "leave unchanged", an empty
/// `Some(map)` means "clear".
#[derive(Debug, Clone, Default)]
pub struct AgentHarnessStreamOptionsPatch {
    pub timeout: Option<Option<std::time::Duration>>,
    pub max_retries: Option<Option<u32>>,
    pub max_retry_delay: Option<Option<std::time::Duration>>,
    /// `None` = leave; `Some(map)` = merge, with inner `None` deleting keys.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub metadata: Option<BTreeMap<String, Option<serde_json::Value>>>,
    pub cache_retention: Option<Option<CacheRetention>>,
}

impl AgentHarnessStreamOptionsPatch {
    /// Apply this patch to `opts` in place. Mirrors the TS apply semantics:
    /// `undefined` value deletes a key, explicit empty patch clears nothing.
    pub fn apply(&self, opts: &mut AgentHarnessStreamOptions) {
        if let Some(v) = self.timeout {
            opts.timeout = v;
        }
        if let Some(v) = self.max_retries {
            opts.max_retries = v;
        }
        if let Some(v) = self.max_retry_delay {
            opts.max_retry_delay = v;
        }
        if let Some(patch) = &self.headers {
            let mut map = opts.headers.take().unwrap_or_default();
            for (k, v) in patch {
                match v {
                    Some(val) => {
                        map.insert(k.clone(), val.clone());
                    }
                    None => {
                        map.remove(k);
                    }
                }
            }
            opts.headers = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(patch) = &self.metadata {
            let mut map = opts.metadata.take().unwrap_or_default();
            for (k, v) in patch {
                match v {
                    Some(val) => {
                        map.insert(k.clone(), val.to_string());
                    }
                    None => {
                        map.remove(k);
                    }
                }
            }
            opts.metadata = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(v) = self.cache_retention {
            opts.cache_retention = v;
        }
    }
}

/// Retry policy owned by the harness. Mirrors TS `RetryPolicy` (a thin config
/// the harness folds into `SimpleStreamOptions.max_retries`/`max_retry_delay`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { enabled: false, max_retries: 0, base_delay_ms: 1000 }
    }
}

/// Compaction thresholds + retention. Mirrors TS `CompactionSettings`. Defaults
/// `{ enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000 }` — the
/// canonical [`DEFAULT_COMPACTION_SETTINGS`] constant lives here so it is
/// available before the `compaction` module (M5d) is built; that module
/// re-exports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
}

/// Default compaction settings used by the harness. Mirrors TS
/// `DEFAULT_COMPACTION_SETTINGS`.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

/// How the harness drives the loop. Mirrors TS `drive` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrivingMode {
    /// Harness drives the loop to completion automatically.
    #[default]
    Automatic,
    /// Caller peeks/executes actions one at a time (`peek_action`/`execute_action`).
    Manual,
}

/// How a batch of tool calls executes. Mirrors TS `toolExecution` option; maps
/// 1:1 to `rpi_agent::ToolExecutionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessToolExecution {
    #[default]
    Parallel,
    Sequential,
}

impl HarnessToolExecution {
    pub fn to_agent_mode(self) -> rpi_agent::ToolExecutionMode {
        match self {
            HarnessToolExecution::Parallel => rpi_agent::ToolExecutionMode::Parallel,
            HarnessToolExecution::Sequential => rpi_agent::ToolExecutionMode::Sequential,
        }
    }
}

/// Re-export of `rpi_tools::ExecutionEnv` so harness consumers can reach the env
/// trait through `rpi_harness::types::ExecutionEnv` (mirroring how `types.ts`
/// declares `ExecutionEnv` itself).
pub use rpi_tools::ExecutionEnv;

/// Re-export the operation-kind enum so callers referencing it via the harness
/// root see the TS-equivalent name.
pub use crate::result::OperationKind as HarnessOperationKind;

/// `Usage` re-export for harness-record helpers.
pub use rpi_ai::types::Usage as HarnessUsage;

/// Reserved container for the `AgentHarnessOptions` config. Populated in M5f
/// when the `AgentHarness` is implemented; declared now so downstream modules
/// can reference the shape. Mirrors TS `AgentHarnessOptions`.
pub struct AgentHarnessOptions {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub active_tool_names: Vec<String>,
    pub tools: Vec<HarnessTool>,
    pub system_prompt: Option<String>,
    pub resources: AgentHarnessResources,
    pub stream_options: AgentHarnessStreamOptions,
    pub retry: RetryPolicy,
    pub compaction: CompactionSettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: HarnessToolExecution,
    pub drive: DrivingMode,

    // ---- M5f additions (the pieces the real run loop needs) ---------------

    /// The durable session the harness drives. Required: every run persists
    /// entries/records through this facade (mirrors TS `options.session`).
    pub session: Session,

    /// Provider registry keyed by `model.provider`. The harness resolves which
    /// provider serves the configured `model` (and any compaction model) and
    /// calls `provider.stream_simple` inside the `StreamFn` it builds. Mirrors
    /// TS `options.models`.
    pub models: Vec<Arc<dyn Provider>>,

    /// The agent-level `convert_to_llm` (AgentMessage[] → Message[]). The
    /// harness installs the harness-level converter (which projects custom-role
    /// messages into user text) by default; callers may override. Mirrors the
    /// TS wiring where the harness composes its converter into `AgentLoopConfig`.
    pub to_provider_messages: Option<ConvertToLlm>,

    /// Optional entry projectors folded into the per-turn context build. Maps a
    /// `custom` entry's `custom_type` → the messages to splice into the context.
    /// Mirrors TS `entryProjectors`.
    pub entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,

    /// Optional emitter override. When `Some`, the harness passes this emitter
    /// to `run_agent_loop` so callers (e.g. the TUI) can observe `AgentEvent`s
    /// live as a run unfolds. `None` (the default) preserves the discard
    /// behavior — the harness only surfaces `RunStart`/`RunEnd` on its own bus.
    pub agent_emitter: Option<Arc<dyn rpi_agent::AgentEmitter>>,
}

impl Default for AgentHarnessOptions {
    fn default() -> Self {
        Self {
            model: Model::new(
                "faux".to_string(),
                "Faux".to_string(),
                rpi_ai::Api::AnthropicMessages,
                "faux".to_string(),
                "https://example.test".to_string(),
            ),
            thinking_level: ThinkingLevel::default(),
            active_tool_names: Vec::new(),
            tools: Vec::new(),
            system_prompt: None,
            resources: AgentHarnessResources::default(),
            stream_options: AgentHarnessStreamOptions::default(),
            retry: RetryPolicy::default(),
            compaction: CompactionSettings::default(),
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            tool_execution: HarnessToolExecution::default(),
            drive: DrivingMode::default(),
            session: Session::new(
                Arc::new(crate::session::memory::InMemorySessionStorage::new(
                    crate::session::types::SessionMetadata {
                        id: "default".into(),
                        created_at: 0,
                        parent_session_id: None,
                    },
                    Arc::new(crate::session::memory::SystemClock),
                    Arc::new(crate::session::session::DefaultIdGenerator::new()),
                )),
                None,
            ),
            models: Vec::new(),
            to_provider_messages: None,
            entry_projectors: BTreeMap::new(),
            agent_emitter: None,
        }
    }
}
