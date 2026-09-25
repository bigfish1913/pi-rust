//! Stored-session working-directory recovery.
//!
//! Port of native Pi's `packages/coding-agent/src/core/session-cwd.ts`. A
//! session file records the cwd it was created in; when that directory has
//! since been deleted, restoring the session must either fall back to the
//! current cwd or fail with a clear, actionable message instead of silently
//! operating in a non-existent directory.

use std::fmt;
use std::path::{Path, PathBuf};

/// A session whose recorded working directory no longer exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    /// The session file that recorded the missing cwd, when known.
    pub session_file: Option<PathBuf>,
    /// The cwd stored in the session file (does not exist).
    pub session_cwd: PathBuf,
    /// The cwd the process is currently running in (the fallback).
    pub fallback_cwd: PathBuf,
}

/// Detect a missing session cwd. Returns `None` when there is no session file,
/// the stored cwd is empty, or the stored cwd still exists.
pub fn get_missing_session_cwd_issue(
    session_file: Option<&Path>,
    session_cwd: &Path,
    fallback_cwd: &Path,
) -> Option<SessionCwdIssue> {
    let session_file = session_file?;
    if session_cwd.as_os_str().is_empty() {
        return None;
    }
    if session_cwd.exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file.to_path_buf()),
        session_cwd: session_cwd.to_path_buf(),
        fallback_cwd: fallback_cwd.to_path_buf(),
    })
}

/// One-line error message (native `formatMissingSessionCwdError`).
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|p| format!("\nSession file: {}", p.display()))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd.display(),
        session_file,
        issue.fallback_cwd.display()
    )
}

/// Short prompt shown by the startup selector (native
/// `formatMissingSessionCwdPrompt`).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd.display(),
        issue.fallback_cwd.display()
    )
}

/// Error raised when a session's cwd cannot be resolved and no override was
/// supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

impl fmt::Display for MissingSessionCwdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_missing_session_cwd_error(&self.issue))
    }
}

impl std::error::Error for MissingSessionCwdError {}

/// Fail unless the session cwd exists (native `assertSessionCwdExists`).
pub fn assert_session_cwd_exists(
    session_file: Option<&Path>,
    session_cwd: &Path,
    fallback_cwd: &Path,
) -> Result<(), MissingSessionCwdError> {
    match get_missing_session_cwd_issue(session_file, session_cwd, fallback_cwd) {
        Some(issue) => Err(MissingSessionCwdError { issue }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn no_session_file_is_ok() {
        assert!(
            get_missing_session_cwd_issue(None, Path::new("/whatever"), Path::new("/cwd"))
                .is_none()
        );
    }

    #[test]
    fn existing_dir_is_ok() {
        let dir = std::env::temp_dir();
        assert!(get_missing_session_cwd_issue(Some(Path::new("/s.jsonl")), &dir, &dir).is_none());
    }

    #[test]
    fn empty_stored_cwd_is_ok() {
        assert!(get_missing_session_cwd_issue(
            Some(Path::new("/s.jsonl")),
            Path::new(""),
            Path::new("/cwd")
        )
        .is_none());
    }

    #[test]
    fn missing_dir_reports_issue() {
        let missing = PathBuf::from("/definitely/not/here/xyzzy-1234");
        let issue =
            get_missing_session_cwd_issue(Some(Path::new("/s.jsonl")), &missing, Path::new("/cwd"))
                .expect("issue");
        assert_eq!(issue.session_cwd, missing);
        assert!(format_missing_session_cwd_error(&issue).contains("does not exist"));
        assert!(format_missing_session_cwd_prompt(&issue).contains("continue in current cwd"));
        assert!(assert_session_cwd_exists(
            Some(Path::new("/s.jsonl")),
            &missing,
            Path::new("/cwd")
        )
        .is_err());
    }
}
