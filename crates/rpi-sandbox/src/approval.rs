//! Platform-neutral approval types.
//!
//! The frontend trait lives in `rpi-cli` (it needs async UI plumbing); this
//! module owns the data that crosses the broker boundary so the protocol can be
//! versioned independently of any UI.

use std::path::PathBuf;

/// One filesystem mutation an approval can cover.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum WriteOp {
    /// Create a new file.
    Create,
    /// Replace or extend an existing file.
    Overwrite,
    /// Append to an existing file.
    Append,
    /// Delete a file.
    Delete,
    /// Rename or move a file.
    Rename,
    /// Create a directory.
    Mkdir,
}

impl WriteOp {
    /// Stable lowercase label used in diagnostics and audit entries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Overwrite => "overwrite",
            Self::Append => "append",
            Self::Delete => "delete",
            Self::Rename => "rename",
            Self::Mkdir => "mkdir",
        }
    }
}

/// Scope of an approved exception.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalScope {
    /// Only the next matching operation.
    Once,
    /// Until the agent process exits.
    #[default]
    Session,
    /// Persisted for the project (frontends may map this to `Session` until
    /// project-level persistence lands).
    Project,
}

/// One approval request shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Stable request id for replay and multi-client arbitration.
    pub id: u64,
    /// Operation the tool wants to perform.
    pub operation: WriteOp,
    /// Absolute path after symlink resolution.
    pub path: PathBuf,
    /// Original user/model-supplied path, for display.
    pub raw_path: String,
    /// Byte count when the operation carries data.
    pub bytes: Option<u64>,
    /// Options the frontend may present.
    pub options: Vec<ApprovalOption>,
}

/// A choice a frontend can return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOption {
    /// Allow this operation only.
    AllowOnce,
    /// Allow for the rest of the process.
    AllowSession,
    /// Deny.
    Deny,
}

impl ApprovalRequest {
    /// Default option list: all three choices.
    pub fn default_options() -> Vec<ApprovalOption> {
        vec![
            ApprovalOption::AllowOnce,
            ApprovalOption::AllowSession,
            ApprovalOption::Deny,
        ]
    }
}

/// Decision returned by a frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow with the given scope. `recursive` covers the containing directory
    /// instead of only the exact path.
    Allow {
        /// Lifetime of the approval.
        scope: ApprovalScope,
        /// Whether the whole directory was approved.
        recursive: bool,
    },
    /// Deny the request.
    Deny,
}

impl ApprovalDecision {
    /// Whether the decision allows the operation.
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// Errors a frontend or broker can report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// No frontend is available (headless runs deny by default).
    Unavailable,
    /// The user denied the request.
    Denied,
    /// The request timed out.
    TimedOut,
    /// Transport or protocol failure.
    Transport(String),
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("no approval frontend is available"),
            Self::Denied => f.write_str("request denied"),
            Self::TimedOut => f.write_str("approval request timed out"),
            Self::Transport(message) => write!(f, "approval transport failed: {message}"),
        }
    }
}

impl std::error::Error for ApprovalError {}
