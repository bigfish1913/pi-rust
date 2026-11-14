//! TUI bridge for filesystem-sandbox approval prompts.
//!
//! The sandboxed agent asks for an approval from a tool task; the interactive
//! TUI owns the dialog. Requests are queued here, the key loop picks them up
//! and opens a selector, and the answer comes back over a oneshot channel.
//! Headless runs never install the bridge, so `ask` is never called and the
//! tool reports the sandbox denial directly.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rpi_sandbox::{ApprovalDecision, ApprovalRequest, ApprovalScope, WriteOp};

/// How long a tool waits for the user before treating the request as denied.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// One request waiting for the TUI to render a dialog.
pub struct ApprovalJob {
    /// Presentation data.
    pub request: ApprovalRequest,
    /// Channel the dialog answers on.
    pub respond: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// Shared queue between tool tasks and the TUI key loop.
#[derive(Default)]
pub struct TuiApprovalBridge {
    pending: Mutex<VecDeque<ApprovalJob>>,
    next_id: AtomicU64,
}

impl TuiApprovalBridge {
    /// Create an empty bridge.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Ask the user. Returns [`ApprovalDecision::Deny`] on timeout or when the
    /// dialog is dismissed.
    pub async fn ask(
        &self,
        operation: WriteOp,
        path: &Path,
        raw_path: &str,
        bytes: Option<u64>,
    ) -> ApprovalDecision {
        let (respond, answer) = tokio::sync::oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = ApprovalRequest {
            id,
            operation,
            path: path.to_path_buf(),
            raw_path: raw_path.to_string(),
            bytes,
            options: ApprovalRequest::default_options(),
        };
        {
            let mut pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(_) => return ApprovalDecision::Deny,
            };
            pending.push_back(ApprovalJob { request, respond });
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, answer).await {
            Ok(Ok(decision)) => decision,
            _ => ApprovalDecision::Deny,
        }
    }

    /// Take the next request whose dialog has not been cancelled.
    pub fn take_pending(&self) -> Option<ApprovalJob> {
        let mut pending = self.pending.lock().ok()?;
        while let Some(job) = pending.pop_front() {
            if !job.respond.is_closed() {
                return Some(job);
            }
        }
        None
    }

    /// Deny every queued request (called when the TUI shuts down).
    pub fn cancel_all(&self) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        for job in pending.drain(..) {
            let _ = job.respond.send(ApprovalDecision::Deny);
        }
    }
}

static BRIDGE: OnceLock<Arc<TuiApprovalBridge>> = OnceLock::new();

/// Install (or reuse) the process-wide TUI bridge.
pub fn install() -> Arc<TuiApprovalBridge> {
    BRIDGE.get_or_init(TuiApprovalBridge::new).clone()
}

/// The installed bridge, if the interactive TUI is running.
pub fn bridge() -> Option<&'static Arc<TuiApprovalBridge>> {
    BRIDGE.get()
}

/// Translate a selector value into a decision.
pub fn decision_for(value: &str) -> ApprovalDecision {
    match value {
        "once" => ApprovalDecision::Allow(ApprovalScope::Once),
        "session" => ApprovalDecision::Allow(ApprovalScope::Session),
        _ => ApprovalDecision::Deny,
    }
}
