//! Mirrors `packages/ai/src/models.ts::Provider` — the trait every concrete
//! provider (faux, anthropic, …) implements. The agent loop never touches a
//! provider's wire format directly; it calls `stream_simple` and consumes the
//! returned [`AssistantMessageEventStream`].

use crate::event_stream::AssistantMessageEventStream;
use crate::model::Model;
use crate::types::{Context, ThinkingBudgets, ThinkingLevel};
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
