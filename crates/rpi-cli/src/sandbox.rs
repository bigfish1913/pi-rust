//! Startup wiring for the filesystem sandbox (M0).
//!
//! M0 only models the policy, reads the `sandbox` settings block, and probes
//! the platform. Nothing is enforced yet, so this module's job is to turn the
//! policy plus the probe result into an honest startup message:
//!
//! - sandbox disabled (the default): stay silent;
//! - enabled but the platform cannot enforce it: warn and keep running, or
//!   refuse to start when `mode` is `required`;
//! - enabled on a supported platform: warn that enforcement is not implemented
//!   yet instead of pretending the process is sandboxed.
//!
//! M1 replaces [`ENFORCEMENT_IMPLEMENTED`] with the Linux Landlock backend and
//! applies it before the Tokio runtime starts; the merge and messaging rules
//! below stay the same.

use std::path::{Path, PathBuf};

use rpi_sandbox::{
    compile_policy, probe, PolicyContext, PolicyOverrides, SandboxMode, SandboxPolicy,
    SandboxSupport,
};

/// Whether the current build can actually enforce a sandbox.
///
/// Deliberately `false` until the Landlock backend lands (M1). Keeping this as
/// a value instead of a comment means the startup messages and tests cannot
/// drift into claiming enforcement that does not exist.
pub const ENFORCEMENT_IMPLEMENTED: bool = false;

/// Compile the effective policy for `cwd`.
///
/// Global settings are always read; project settings contribute only when the
/// project is trusted, and even then only through the tightening rules in
/// [`compile_policy`] (a project can add `protected` entries but cannot add
/// writable roots or disable the sandbox).
pub fn effective_policy(cwd: &Path, project_trusted: bool) -> SandboxPolicy {
    let global = crate::settings::load_settings().unwrap_or_default();
    let project = if project_trusted {
        crate::settings::load_active_project_settings(cwd)
            .ok()
            .flatten()
            .map(|(_, settings)| settings)
    } else {
        None
    };

    let context = PolicyContext::from_env(cwd.to_path_buf());
    let mut auto_writable: Vec<PathBuf> = vec![cwd.to_path_buf()];
    if let Some(session_dir) = effective_session_dir(&global, cwd) {
        auto_writable.push(session_dir);
    }

    compile_policy(
        global.sandbox.as_ref(),
        project
            .as_ref()
            .and_then(|settings| settings.sandbox.as_ref()),
        &PolicyOverrides::default(),
        &context,
        &auto_writable,
    )
}

fn effective_session_dir(settings: &crate::settings::Settings, cwd: &Path) -> Option<PathBuf> {
    if let Some(dir) = settings.session_dir.as_ref() {
        return Some(if dir.is_absolute() {
            dir.clone()
        } else {
            cwd.join(dir)
        });
    }
    std::env::var_os("RPI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .map(|dir| {
            if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            }
        })
}

/// Turn a policy and probe result into the startup action.
///
/// `Ok(None)` means "say nothing"; `Ok(Some(message))` is a warning printed to
/// stderr; `Err(message)` aborts startup because `mode = "required"` cannot be
/// honored.
pub fn startup_message(
    policy: &SandboxPolicy,
    support: &SandboxSupport,
) -> Result<Option<String>, String> {
    if !policy.enabled || policy.mode == SandboxMode::Off {
        return Ok(None);
    }

    let unsupported = |reason: &str| match policy.mode {
        SandboxMode::Required => Err(format!(
            "sandbox is required but cannot be enforced: {reason}"
        )),
        _ => Ok(Some(format!(
            "warning: sandbox enabled but unavailable: {reason}; running without a sandbox"
        ))),
    };

    match support {
        SandboxSupport::Unsupported { reason } => unsupported(reason),
        SandboxSupport::Supported(backend) if !ENFORCEMENT_IMPLEMENTED => {
            let detail = format!(
                "{} is detectable but enforcement is not implemented yet",
                backend.name()
            );
            unsupported(&detail)
        }
        SandboxSupport::Supported(_) => Ok(None),
    }
}

/// Startup check used by the CLI: compile the policy, probe the platform, and
/// return a warning or a fatal error.
pub fn startup_check(cwd: &Path, project_trusted: bool) -> Result<Option<String>, String> {
    let policy = effective_policy(cwd, project_trusted);
    startup_message(&policy, &probe())
}

/// Snapshot used by future UI surfaces (banner, footer, `/sandbox`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStatus {
    /// Whether the user asked for a sandbox.
    pub enabled: bool,
    /// Whether this build enforces it.
    pub enforced: bool,
    /// Human-readable backend description, or the reason it is unavailable.
    pub detail: String,
}

/// Build a status snapshot for display.
pub fn status(cwd: &Path, project_trusted: bool) -> SandboxStatus {
    let policy = effective_policy(cwd, project_trusted);
    let support = probe();
    SandboxStatus {
        enabled: policy.enabled && policy.mode != SandboxMode::Off,
        enforced: policy.enabled && policy.mode != SandboxMode::Off && ENFORCEMENT_IMPLEMENTED,
        detail: support.describe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_sandbox::SandboxBackend;

    fn policy(enabled: bool, mode: SandboxMode) -> SandboxPolicy {
        SandboxPolicy {
            enabled,
            mode,
            writable: Vec::new(),
            protected: Vec::new(),
            allow_file_writes: Vec::new(),
            approval: rpi_sandbox::ApprovalSettings::default(),
            audit_log: None,
        }
    }

    #[test]
    fn disabled_policy_is_silent() {
        assert_eq!(
            startup_message(&policy(false, SandboxMode::Auto), &probe()).unwrap(),
            None
        );
    }

    #[test]
    fn off_mode_is_silent_even_when_enabled() {
        assert_eq!(
            startup_message(&policy(true, SandboxMode::Off), &probe()).unwrap(),
            None
        );
    }

    #[test]
    fn auto_mode_warns_when_unavailable() {
        let support = SandboxSupport::Unsupported {
            reason: "no backend".to_string(),
        };
        let message = startup_message(&policy(true, SandboxMode::Auto), &support)
            .unwrap()
            .expect("warning");
        assert!(message.contains("no backend"));
        assert!(message.contains("without a sandbox"));
    }

    #[test]
    fn required_mode_fails_when_unavailable() {
        let support = SandboxSupport::Unsupported {
            reason: "no backend".to_string(),
        };
        let error = startup_message(&policy(true, SandboxMode::Required), &support).unwrap_err();
        assert!(error.contains("required"));
    }

    #[test]
    fn supported_but_unimplemented_warns() {
        if ENFORCEMENT_IMPLEMENTED {
            return;
        }
        let support = SandboxSupport::Supported(SandboxBackend::Landlock { abi: 6 });
        let message = startup_message(&policy(true, SandboxMode::Auto), &support)
            .unwrap()
            .expect("warning");
        assert!(message.contains("not implemented"));
    }

    #[test]
    fn required_fails_while_unimplemented() {
        if ENFORCEMENT_IMPLEMENTED {
            return;
        }
        let support = SandboxSupport::Supported(SandboxBackend::Landlock { abi: 6 });
        assert!(startup_message(&policy(true, SandboxMode::Required), &support).is_err());
    }
}
