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

/// Remote (RPC/server) frontend: forwards approvals through the same
/// `UiDialogMailbox` the server already projects to connected clients, so a
/// `--connect` client sees the prompt with no protocol changes.
pub struct RemoteApproval {
    mailbox: rpi_extensions::UiDialogMailbox,
    next_id: AtomicU64,
}

static REMOTE: OnceLock<Arc<RemoteApproval>> = OnceLock::new();

/// Install the remote frontend for `--server` / `--mode rpc`.
pub fn install_remote(mailbox: rpi_extensions::UiDialogMailbox) {
    let _ = REMOTE.set(Arc::new(RemoteApproval {
        mailbox,
        next_id: AtomicU64::new(1),
    }));
}

fn remote() -> Option<&'static Arc<RemoteApproval>> {
    REMOTE.get()
}

/// Unified entry point used by the tool environment: local TUI first, then the
/// remote mailbox, then `None` (headless deny).
pub async fn ask(
    operation: WriteOp,
    path: &Path,
    raw_path: &str,
    bytes: Option<u64>,
) -> Option<ApprovalDecision> {
    if let Some(bridge) = bridge() {
        return Some(bridge.ask(operation, path, raw_path, bytes).await);
    }
    if let Some(remote) = remote() {
        return Some(remote.ask(operation, path, raw_path, bytes).await);
    }
    None
}

impl RemoteApproval {
    async fn ask(
        &self,
        operation: WriteOp,
        path: &Path,
        raw_path: &str,
        bytes: Option<u64>,
    ) -> ApprovalDecision {
        let id = format!(
            "sandbox:{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let tool_call_id = format!("sandbox-{}-{id}", std::process::id());
        let mut question = format!("Allow {} of {raw_path}?", operation.as_str());
        if let Some(bytes) = bytes {
            question.push_str(&format!("\n{bytes} bytes"));
        }
        let envelope = serde_json::json!({
            "op": "open",
            "requestId": id,
            "toolCallId": tool_call_id,
            "ui": {
                "purpose": "permission",
                "kind": "confirm",
                "header": "Filesystem sandbox",
                "question": question,
                "options": [
                    {"title": "Allow once", "permission": "allow_once"},
                    {"title": "Allow for this session", "permission": "allow_always"},
                    {"title": "Deny", "permission": "reject_once"}
                ]
            }
        });
        if let Err(_error) = self.mailbox.handle(envelope) {
            return ApprovalDecision::Deny;
        }
        let deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let status = match self.mailbox.poll(&id) {
                Ok(status) => status,
                Err(_) => return ApprovalDecision::Deny,
            };
            match status.get("status").and_then(|value| value.as_str()) {
                Some("answered") => {
                    let answer = status.get("answer").cloned().unwrap_or_default();
                    return decision_from_remote_answer(&answer);
                }
                Some("cancelled") => return ApprovalDecision::Deny,
                Some("pending") => {}
                _ => return ApprovalDecision::Deny,
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = self.mailbox.cancel(&id);
                return ApprovalDecision::Deny;
            }
        }
    }
}

fn decision_from_remote_answer(answer: &serde_json::Value) -> ApprovalDecision {
    if let Some(permission) = answer.get("permission").and_then(|value| value.as_str()) {
        return match permission {
            "allow_once" => ApprovalDecision::Allow {
                scope: ApprovalScope::Once,
                recursive: false,
            },
            "allow_always" => ApprovalDecision::Allow {
                scope: ApprovalScope::Session,
                recursive: false,
            },
            _ => ApprovalDecision::Deny,
        };
    }
    let values = answer.get("values").and_then(|value| value.as_array());
    let first = values
        .and_then(|values| values.first())
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if first.contains("once") && first.to_ascii_lowercase().contains("allow") {
        ApprovalDecision::Allow {
            scope: ApprovalScope::Once,
            recursive: false,
        }
    } else if first.to_ascii_lowercase().contains("session")
        || first.to_ascii_lowercase().contains("always")
    {
        ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            recursive: false,
        }
    } else {
        ApprovalDecision::Deny
    }
}

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
        "once" => ApprovalDecision::Allow {
            scope: ApprovalScope::Once,
            recursive: false,
        },
        "session" => ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            recursive: false,
        },
        "directory" => ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            recursive: true,
        },
        _ => ApprovalDecision::Deny,
    }
}
