//! Host-integrated approval gate — runtime approval modes for embedded hosts.
//!
//! ## Why
//!
//! rpi's interactive permissions are extension-owned and read their rules at
//! session build time; an embedding desktop host needs to switch the policy at
//! runtime and receive a real approval prompt for the exact tool call. This
//! module provides the engine half:
//!
//! - [`ApprovalMode`] (`AlwaysAsk` / `Write` / `Yolo`) in a process-global
//!   atomic, settable at runtime by the host;
//! - a [`ApprovalHandler`] the host installs once — the gate calls it with the
//!   tool name/args/session id and honors an `Allow`/`Deny` decision;
//! - [`hook`], a `before_tool_call` hook that composes with the extension hook
//!   and blocks denied calls with a readable reason.
//!
//! When no handler is installed the gate is a no-op: a bare CLI keeps its
//! existing extension permissions behaviour and is never silently downgraded.
//!
//! Classification is intentionally conservative: only an explicit read-only
//! allowlist is auto-allowed; anything unknown is treated as mutating and asks
//! under `AlwaysAsk`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use rpi_agent::hooks::BeforeToolCall;
use rpi_agent::types::{BeforeToolCallContext, BeforeToolCallResult};
use rpi_harness::agent_harness::WeakAgentHarness;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Ask before any mutating tool (read-only tools never ask).
    AlwaysAsk,
    /// Auto-approve file writes; ask before commands / unknown tools.
    Write,
    /// Auto-approve everything (hard policy gates still apply elsewhere).
    Yolo,
}

impl ApprovalMode {
    pub fn as_wire(self) -> &'static str {
        match self {
            ApprovalMode::AlwaysAsk => "always-ask",
            ApprovalMode::Write => "write",
            ApprovalMode::Yolo => "yolo",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "always-ask" | "always_ask" | "ask" | "default" => Some(ApprovalMode::AlwaysAsk),
            "write" => Some(ApprovalMode::Write),
            "yolo" | "full" | "full-access" | "full_access" => Some(ApprovalMode::Yolo),
            _ => None,
        }
    }

    fn code(self) -> u8 {
        match self {
            ApprovalMode::AlwaysAsk => 0,
            ApprovalMode::Write => 1,
            ApprovalMode::Yolo => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => ApprovalMode::Write,
            2 => ApprovalMode::Yolo,
            _ => ApprovalMode::AlwaysAsk,
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(0);

/// Set the process-wide approval mode (host call; takes effect for the very
/// next tool call, including in-flight turns).
pub fn set_mode(mode: ApprovalMode) {
    MODE.store(mode.code(), Ordering::SeqCst);
}

pub fn mode() -> ApprovalMode {
    ApprovalMode::from_code(MODE.load(Ordering::SeqCst))
}

/// One approval question, with just enough context for a human to decide.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Allow,
    Deny { reason: Option<String> },
}

/// Host approval door (UI prompt). Must return a decision; an I/O error is
/// surfaced as a denial with the error text (never fail-open).
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request(&self, request: ApprovalRequest) -> ApprovalDecision;
}

static HANDLER: OnceLock<Arc<dyn ApprovalHandler>> = OnceLock::new();

pub fn set_handler(handler: Arc<dyn ApprovalHandler>) -> bool {
    HANDLER.set(handler).is_ok()
}

pub fn handler_installed() -> bool {
    HANDLER.get().is_some()
}

/// Read-only tools are always safe; keep this list explicit (unknown = mutating).
fn read_only_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read" | "ls" | "grep" | "find" | "web_search" | "web_fetch" | "docs" | "todo"
    )
}

fn write_tool(tool: &str) -> bool {
    matches!(
        tool,
        "write" | "edit" | "hashline_edit" | "ast_edit" | "apply_patch" | "multi_edit"
    )
}

/// Whether this call needs a human decision under `mode`.
fn needs_approval(mode: ApprovalMode, tool: &str) -> bool {
    match mode {
        ApprovalMode::Yolo => false,
        ApprovalMode::Write => !read_only_tool(tool) && !write_tool(tool),
        ApprovalMode::AlwaysAsk => !read_only_tool(tool),
    }
}

async fn session_id(binding: &OnceLock<WeakAgentHarness>) -> String {
    let Some(weak) = binding.get() else {
        return String::new();
    };
    let Some(harness) = weak.upgrade() else {
        return String::new();
    };
    harness
        .session()
        .get_metadata()
        .await
        .map(|metadata| metadata.id)
        .unwrap_or_default()
}

/// Evaluate one tool call against the current mode + host handler.
///
/// Owned inputs only: the `before_tool_call` future must be `'static`, so the
/// hook copies the tool name/args before awaiting.
///
/// Cancellation is propagated (reviewer blocker #4): once the handler door is
/// open, a stale UI prompt must not hold the tool turn forever — if `signal`
/// fires first the call fails **closed** (blocked), never open.
pub async fn evaluate(
    binding: &OnceLock<WeakAgentHarness>,
    tool: &str,
    args: &serde_json::Value,
    signal: &CancellationToken,
) -> Option<BeforeToolCallResult> {
    if !needs_approval(mode(), tool) {
        return None;
    }
    let Some(handler) = HANDLER.get() else {
        // Host never installed an approval door: leave behaviour to the
        // extension permissions path rather than inventing a denial.
        return None;
    };
    let request = ApprovalRequest {
        session_id: session_id(binding).await,
        tool: tool.to_owned(),
        args: args.clone(),
    };
    let decision = tokio::select! {
        biased;
        _ = signal.cancelled() => {
            return Some(BeforeToolCallResult {
                block: true,
                reason: Some(format!("approval for {tool} cancelled before a decision")),
                ..Default::default()
            });
        }
        decision = handler.request(request) => decision,
    };
    match decision {
        ApprovalDecision::Allow => None,
        ApprovalDecision::Deny { reason } => Some(BeforeToolCallResult {
            block: true,
            reason: Some(reason.unwrap_or_else(|| format!("User denied {tool}"))),
            ..Default::default()
        }),
    }
}

/// The `before_tool_call` gate. Compose it **after** any extension hook: an
/// extension veto still wins, and the host gate sees only calls that would
/// actually run.
pub fn hook(binding: Arc<OnceLock<WeakAgentHarness>>) -> BeforeToolCall {
    Arc::new(
        move |context: BeforeToolCallContext<'_>, signal: CancellationToken| {
            let binding = Arc::clone(&binding);
            let tool = context.tool_call.name.clone();
            let args = context.args.clone();
            Box::pin(async move {
                if signal.is_cancelled() {
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("tool call cancelled".into()),
                        ..Default::default()
                    });
                }
                evaluate(&binding, &tool, &args, &signal).await
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrip() {
        for mode in [
            ApprovalMode::AlwaysAsk,
            ApprovalMode::Write,
            ApprovalMode::Yolo,
        ] {
            assert_eq!(ApprovalMode::parse(mode.as_wire()), Some(mode));
        }
        assert_eq!(ApprovalMode::parse("bogus"), None);
    }

    #[test]
    fn classification_matches_policy() {
        assert!(!needs_approval(ApprovalMode::AlwaysAsk, "read"));
        assert!(needs_approval(ApprovalMode::AlwaysAsk, "bash"));
        assert!(needs_approval(ApprovalMode::AlwaysAsk, "write"));
        assert!(!needs_approval(ApprovalMode::Write, "write"));
        assert!(needs_approval(ApprovalMode::Write, "bash"));
        assert!(!needs_approval(ApprovalMode::Yolo, "bash"));
    }

    #[test]
    fn read_only_and_write_sets_are_explicit() {
        for tool in ["read", "ls", "grep", "find", "docs"] {
            assert!(read_only_tool(tool));
        }
        for tool in ["write", "edit", "hashline_edit", "ast_edit"] {
            assert!(write_tool(tool));
        }
        assert!(!read_only_tool("bash"));
        assert!(!write_tool("bash"));
    }

    struct NeverDecides;
    #[async_trait]
    impl ApprovalHandler for NeverDecides {
        async fn request(&self, _request: ApprovalRequest) -> ApprovalDecision {
            std::future::pending::<ApprovalDecision>().await
        }
    }

    struct AlwaysAllows;
    #[async_trait]
    impl ApprovalHandler for AlwaysAllows {
        async fn request(&self, _request: ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::Allow
        }
    }

    /// A cancelled tool signal must win over a handler that never answers:
    /// fail closed instead of holding the tool turn forever (reviewer #4).
    #[test]
    fn cancelled_signal_fails_closed_before_a_decision() {
        if HANDLER.get().is_none() {
            let _ = set_handler(Arc::new(NeverDecides));
        }
        set_mode(ApprovalMode::AlwaysAsk);
        let token = CancellationToken::new();
        token.cancel();
        let binding = OnceLock::new();
        let outcome =
            futures::executor::block_on(evaluate(&binding, "bash", &serde_json::json!({}), &token));
        let blocked = matches!(outcome, Some(result) if result.block);
        assert!(blocked, "cancelled signal must return a blocking result");

        // And an allowing handler is not consulted once the signal is up.
        let _ = set_handler(Arc::new(AlwaysAllows));
        let outcome =
            futures::executor::block_on(evaluate(&binding, "bash", &serde_json::json!({}), &token));
        assert!(matches!(outcome, Some(result) if result.block));
    }
}
