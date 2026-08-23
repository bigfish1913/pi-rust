//! Mirrors `packages/ai/src/models.ts::Provider` — the trait every concrete
//! provider (faux, anthropic, …) implements. The agent loop never touches a
//! provider's wire format directly; it calls `stream_simple` and consumes the
//! returned [`AssistantMessageEventStream`].

use crate::event_stream::AssistantMessageEventStream;
use crate::model::Model;
use crate::types::{AssistantMessage, Context, ThinkingBudgets, ThinkingLevel};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cache-retention hint passed to a provider. Mirrors TS `CacheRetention`.
/// `Long` requests 1h ephemeral cache control on Anthropic (subject to compat);
/// `Short` requests the default ephemeral; `None` disables cache control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// Options for a "simple" (non-agent) stream call — what compaction / smoke
/// tests / bare provider usage pass. The agent loop wraps this with its richer
/// equivalent when it drives a real turn (M2).
#[derive(Debug, Clone)]
pub struct SimpleStreamOptions {
    /// API key. A provider may fall back to an env var when `None` (M3 anthropic
    /// reads `ANTHROPIC_API_KEY`); faux ignores it.
    pub api_key: Option<String>,
    /// Request timeout. `None` = provider default.
    pub timeout: Option<Duration>,
    /// Max retry attempts on retryable errors (408/409/429/>=500). `None` = 0.
    pub max_retries: Option<u32>,
    /// Cap on the total backoff between retries. `None` = 60s.
    pub max_retry_delay: Option<Duration>,
    /// Extra HTTP headers to send (merged over provider defaults).
    pub headers: Option<BTreeMap<String, String>>,
    /// Free-form provider metadata (request id, trace id, …). Anthropic reads
    /// `user_id` for abuse tracking; faux ignores it.
    pub metadata: Option<BTreeMap<String, String>>,
    /// Cache-retention hint for prompt caching.
    pub cache_retention: CacheRetention,
    /// Optional session id (faux uses it to simulate prompt-cache hits).
    pub session_id: Option<String>,
    /// Cancellation token. Providers must poll this between chunks and emit an
    /// `Error { reason: Aborted }` terminal event when cancelled. Defaults to a
    /// fresh un-cancelled token so `Default`-constructed options never abort.
    pub signal: CancellationToken,
    /// Extended-thinking level. `None`/`Off` → thinking disabled (provider may
    /// still emit `thinking: { type: "disabled" }` when the model reasons).
    /// Mirrors TS `SimpleStreamOptions.reasoning`.
    pub reasoning: Option<ThinkingLevel>,
    /// Optional caller override for the requested output-token ceiling. `None`
    /// lets the provider fit the thinking budget inside `Model::max_tokens`.
    /// Mirrors TS `SimpleStreamOptions.maxTokens`.
    pub max_tokens: Option<u64>,
    /// Sampling temperature. Only applied when the model supports it AND
    /// thinking is disabled (Anthropic rejects temperature under thinking).
    /// Mirrors TS `StreamOptions.temperature`.
    pub temperature: Option<f64>,
    /// Per-level reasoning token budgets for budget-based thinking models.
    /// Faux ignores this; anthropic uses it in `adjust_max_tokens_for_thinking`.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl Default for SimpleStreamOptions {
    fn default() -> Self {
        Self {
            api_key: None,
            timeout: None,
            max_retries: None,
            max_retry_delay: None,
            headers: None,
            metadata: None,
            cache_retention: CacheRetention::default(),
            session_id: None,
            signal: CancellationToken::new(),
            reasoning: None,
            max_tokens: None,
            temperature: None,
            thinking_budgets: None,
        }
    }
}

impl SimpleStreamOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_signal(mut self, signal: CancellationToken) -> Self {
        self.signal = signal;
        self
    }

    /// Resolve the effective reasoning level, defaulting to `Off` when unset.
    pub fn reasoning_level(&self) -> ThinkingLevel {
        self.reasoning.unwrap_or(ThinkingLevel::Off)
    }
}

/// The trait seam. The arrow `pi-agent → pi-ai` means the agent loop depends on
/// this trait alone — provider crates are pluggable behind it.
///
/// `stream_simple` returns synchronously (no `await`): it creates an
/// [`AssistantMessageEventStream`], spawns the producer task internally, and
/// hands back the consumer. Failures surface as `Error` events on the stream,
/// never as panics or `Err` from this method.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider id (e.g. `"anthropic"`, `"faux"`).
    fn id(&self) -> &str;

    /// Models this provider serves.
    fn models(&self) -> &[Model];

    /// Stream a single assistant turn against `ctx` on `model`. Returns the
    /// consumer end of an event stream; the producer task is already running.
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;
}

// ---------------------------------------------------------------------------
// ProviderHooks (B4) — a per-call sidecar hook fired inside the harness
// `StreamFn` closure BEFORE each `stream_simple` call.
//
// Why a sidecar trait (not an extension of `Provider`): `Provider` is not
// sealed (verified), so extending it directly is viable — but that would force
// EVERY `Provider` impl to stub no-op hooks, even faux/anthropic. A sidecar
// trait keeps `Provider` lean and lets only the host (harness) supply hooks;
// the harness captures `Option<Arc<dyn ProviderHooks>>` into its `StreamFn`
// closure and applies the patch per call. This mirrors pi's `beforeRequest`
// per-call semantics, NOT a run-once config-build patch (applying once per run
// is not equivalent — e.g. a hook that injects a per-request id or rotates a
// header must run before every provider call).
//
// The patch type is `SimpleStreamOptionsPatch` (against `SimpleStreamOptions`,
// the type actually passed to `stream_simple`) — NOT the harness-side
// `AgentHarnessStreamOptionsPatch` (which targets `AgentHarnessStreamOptions`,
// the pinned subset, and lives in rpi-harness): defining it here keeps rpi-ai
// free of any rpi-harness dep (cycle-free) and targets the live options the
// provider sees. The harness's existing `AgentHarnessStreamOptionsPatch` stays
// for harness config, a separate concern.
// ---------------------------------------------------------------------------

/// A per-call provider hook. `before_request` may patch the live
/// [`SimpleStreamOptions`] (e.g. inject/rotate headers, set metadata, override
/// timeout) before the harness calls `stream_simple`; `after_response` observes
/// the terminal assistant message after the stream resolves. Both are
/// `Option`-returning: `None` = no change / no observation.
///
/// Implementations MUST be infallible at the API boundary — a hook error is
/// swallowed (the run continues with the unpatched options), mirroring the
/// "hooks must not throw" contract from pi and the harness's other hooks
/// (`before_tool_call` etc.).
pub trait ProviderHooks: Send + Sync {
    /// Fired before each `stream_simple` call, with a snapshot of the model +
    /// context + opts the harness is about to pass. Return a patch that the
    /// harness applies to a clone of `opts` before calling the provider; return
    /// `None` to leave `opts` untouched.
    ///
    /// The hook receives a reference to the planned `opts`, not ownership — it
    /// must not retain the borrow (the closure clones before `await`). Runs on
    /// the blocking-bridge thread inside the harness `StreamFn` closure.
    fn before_request(
        &self,
        _model: &Model,
        _ctx: &Context,
        _opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        None
    }

    /// Fired after the stream resolves with the terminal assistant message.
    /// `message` is the final `AssistantMessage` the provider produced (the same
    /// one the loop emits as `MessageEnd`). `None` lets the default no-op impl
    /// apply. Runs on the blocking-bridge thread.
    fn after_response(&self, _model: &Model, _message: &AssistantMessage) {}
}

/// A no-op default so the harness can install a `ProviderHooks` slot cheaply
/// when none is supplied (uniform code path vs `Option<Arc<dyn>>`).
#[derive(Default)]
pub struct NoopProviderHooks;
impl ProviderHooks for NoopProviderHooks {}

/// A patch against [`SimpleStreamOptions`] returned by [`ProviderHooks::before_request`].
/// Mirrors TS `beforeRequest`'s per-request overrides but targets the live
/// `SimpleStreamOptions` (the type `stream_simple` receives), not the harness
/// pinned subset. `Option<Option<T>>` semantics: outer `None` = "leave this
/// field"; inner `Some(None)` on the headers/metadata maps = "delete this key".
///
/// This is *distinct* from the harness-side `AgentHarnessStreamOptionsPatch`
/// (which lives in rpi-harness and patches `AgentHarnessStreamOptions`). Both
/// exist; this one is the rpi-ai seam so provider hooks can be defined without
/// a rpi-harness cycle.
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptionsPatch {
    pub timeout: Option<Option<Duration>>,
    pub max_retries: Option<Option<u32>>,
    pub max_retry_delay: Option<Option<Duration>>,
    /// `None` = leave; `Some(map)` = merge, inner `None` deletes a key.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    /// `None` = leave; `Some(map)` = merge, inner `None` deletes a key.
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    pub cache_retention: Option<Option<CacheRetention>>,
    pub max_tokens: Option<Option<u64>>,
    pub temperature: Option<Option<f64>>,
    pub session_id: Option<Option<String>>,
}

impl SimpleStreamOptionsPatch {
    /// Apply this patch to `opts` in place. Mirrors the harness patch semantics:
    /// outer `None` = leave the field; an explicit value replaces it; `None`
    /// inside a map entry deletes that key.
    pub fn apply(&self, opts: &mut SimpleStreamOptions) {
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
                    Some(val) => map.insert(k.clone(), val.clone()),
                    None => map.remove(k),
                };
            }
            opts.headers = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(patch) = &self.metadata {
            let mut map = opts.metadata.take().unwrap_or_default();
            for (k, v) in patch {
                match v {
                    Some(val) => map.insert(k.clone(), val.clone()),
                    None => map.remove(k),
                };
            }
            opts.metadata = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(v) = self.cache_retention {
            opts.cache_retention = v.unwrap_or_default();
        }
        if let Some(v) = self.max_tokens {
            opts.max_tokens = v;
        }
        if let Some(v) = self.temperature {
            opts.temperature = v;
        }
        if let Some(v) = &self.session_id {
            opts.session_id = v.clone();
        }
    }
}
