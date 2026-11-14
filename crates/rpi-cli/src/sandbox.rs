//! Startup wiring for the filesystem sandbox.
//!
//! Linux enforcement (M1) happens in `main` before the Tokio runtime is built:
//! Landlock domains are per-thread and inherited by children, so worker threads
//! created earlier would stay unrestricted. [`prepare_from_argv`] parses just
//! enough of the command line to resolve the cwd, project trust and session
//! directory, compiles the policy, and applies the platform backend.
//!
//! Behavior matrix:
//!
//! - sandbox disabled (the default): stay silent, do nothing;
//! - maintenance subcommand (`install`, `update`, `auth`, `dev`, ...): bypass
//!   and say so once;
//! - enabled but unavailable: warn and keep running (`auto`), or refuse to
//!   start (`required`);
//! - enabled and available: apply, or fall back with a warning when applying
//!   fails and `mode = "auto"`.

use std::path::{Path, PathBuf};

use rpi_sandbox::{
    compile_policy, probe, PolicyContext, PolicyOverrides, SandboxMode, SandboxPolicy,
    SandboxSupport,
};

/// Whether this build has a working enforcement backend.
pub const ENFORCEMENT_IMPLEMENTED: bool = cfg!(target_os = "linux");

/// Outcome of the pre-runtime sandbox preparation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// The process is now confined.
    Applied,
    /// The sandbox is disabled or `mode = "off"`.
    Disabled,
    /// A maintenance command was detected; the process runs unconfined.
    Bypassed {
        /// Why the sandbox was skipped.
        reason: &'static str,
    },
}

/// Subcommands that legitimately write outside the workspace. They are invoked
/// by the user, never by the model, and the model cannot spawn an unconfined
/// `rpi` because the sandbox domain is inherited by children.
fn bypass_reason(argv: &[String]) -> Option<&'static str> {
    const MAINTENANCE: &[&str] = &[
        "install",
        "uninstall",
        "auth",
        "update",
        "extension",
        "package",
        "resource",
        "events",
        "session",
        "dev",
        "dev-local",
        "__rpi_dev_cleanup",
    ];
    let first = argv.first()?.as_str();
    if MAINTENANCE.contains(&first) {
        Some("maintenance command")
    } else {
        None
    }
}

/// Prepare and apply the sandbox before the async runtime starts.
///
/// Errors are fatal: `mode = "required"` cannot be honored, or applying a
/// supported backend failed. In `auto` mode every failure degrades to a warning
/// and an unconfined process.
pub fn prepare_from_argv(argv: &[String]) -> Result<Prepared, String> {
    if let Some(reason) = bypass_reason(argv) {
        if sandbox_requested_in_settings() {
            eprintln!("sandbox: bypassed ({reason})");
        }
        return Ok(Prepared::Bypassed { reason });
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let parsed = crate::args::parse_args(argv);
    let trusted = crate::session::resolve_project_trust(&parsed, &cwd);
    let policy = policy_for_posture(&parsed, &cwd, trusted);

    if !policy.enabled || policy.mode == SandboxMode::Off {
        return Ok(Prepared::Disabled);
    }

    // Landlock cannot subtract a protected subtree from an allowed parent.
    // Surface it now instead of silently claiming full protection.
    for (protected, writable) in policy.nested_protected() {
        let detail = format!(
            "protected path {} is inside writable root {}; the kernel cannot exclude a subtree",
            protected.display(),
            writable.display()
        );
        if policy.mode == SandboxMode::Required {
            return Err(format!("sandbox is required but {detail}"));
        }
        eprintln!("warning: sandbox: {detail}");
    }

    let support = probe();
    if !ENFORCEMENT_IMPLEMENTED {
        return fallback(
            &policy,
            support
                .backend()
                .map(|backend| format!("{} backend is not implemented yet", backend.name()))
                .unwrap_or_else(|| support.reason().unwrap_or("no usable backend").to_string()),
        );
    }

    match support {
        SandboxSupport::Unsupported { reason } => fallback(&policy, reason),
        SandboxSupport::Supported(_) => {
            let outcome = apply_backend(&policy)
                .map_err(|error| format!("failed to apply the filesystem sandbox: {error}"));
            match outcome {
                Ok(()) => Ok(Prepared::Applied),
                Err(error) => match policy.mode {
                    SandboxMode::Required => Err(error),
                    _ => {
                        eprintln!("warning: {error}; running without a sandbox");
                        Ok(Prepared::Disabled)
                    }
                },
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_backend(policy: &SandboxPolicy) -> Result<(), String> {
    let plan = rpi_sandbox::linux::plan(policy)?;
    rpi_sandbox::linux::apply(&plan)
}

#[cfg(not(target_os = "linux"))]
fn apply_backend(_policy: &SandboxPolicy) -> Result<(), String> {
    Err("no enforcement backend on this platform".to_string())
}

fn fallback(policy: &SandboxPolicy, reason: String) -> Result<Prepared, String> {
    match policy.mode {
        SandboxMode::Required => Err(format!(
            "sandbox is required but cannot be enforced: {reason}"
        )),
        _ => {
            eprintln!(
                "warning: sandbox enabled but unavailable: {reason}; running without a sandbox"
            );
            Ok(Prepared::Disabled)
        }
    }
}

fn sandbox_requested_in_settings() -> bool {
    crate::settings::load_settings()
        .ok()
        .and_then(|settings| settings.sandbox)
        .and_then(|sandbox| sandbox.enabled)
        .unwrap_or(false)
}

/// Compile the effective policy without command-line context.
pub fn effective_policy(cwd: &Path, project_trusted: bool) -> SandboxPolicy {
    policy(cwd, project_trusted, None)
}

/// Compile the effective policy for a parsed command line, including
/// `--session-dir`.
fn policy_for_posture(
    args: &crate::args::Args,
    cwd: &Path,
    project_trusted: bool,
) -> SandboxPolicy {
    policy(cwd, project_trusted, Some(args))
}

fn policy(cwd: &Path, project_trusted: bool, args: Option<&crate::args::Args>) -> SandboxPolicy {
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
    if let Some(session_dir) = effective_session_dir(&global, args, cwd) {
        auto_writable.push(session_dir);
    }
    // Runtime bookkeeping lives in the agent directory and must stay writable
    // for startup to work; individual files are allowlisted rather than the
    // directory so a model cannot install plugins or rewrite policy.
    for file in runtime_agent_files(&global) {
        auto_writable.push(file);
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

fn runtime_agent_files(settings: &crate::settings::Settings) -> Vec<PathBuf> {
    let _ = settings;
    let Ok(agent_dir) = crate::config::agent_dir() else {
        return Vec::new();
    };
    vec![
        agent_dir.join("update-check.json"),
        agent_dir.join(".resource-lease"),
    ]
}

fn effective_session_dir(
    settings: &crate::settings::Settings,
    args: Option<&crate::args::Args>,
    cwd: &Path,
) -> Option<PathBuf> {
    let resolve = |dir: &Path| {
        if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            cwd.join(dir)
        }
    };
    if let Some(dir) = args.and_then(|args| args.session_dir.as_ref()) {
        return Some(resolve(dir));
    }
    if let Some(dir) = settings.session_dir.as_ref() {
        return Some(resolve(dir));
    }
    std::env::var_os("RPI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .map(|dir| resolve(&dir))
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

/// Startup check for status reporting and diagnostics.
pub fn startup_check(cwd: &Path, project_trusted: bool) -> Result<Option<String>, String> {
    let policy = effective_policy(cwd, project_trusted);
    startup_message(&policy, &probe())
}

/// Snapshot used by future UI surfaces (banner, footer, `/sandbox`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStatus {
    /// Whether the user asked for a sandbox.
    pub enabled: bool,
    /// Whether this build can enforce it.
    pub enforced: bool,
    /// Human-readable backend description, or the reason it is unavailable.
    pub detail: String,
}

/// Build a status snapshot for display.
pub fn status(cwd: &Path, project_trusted: bool) -> SandboxStatus {
    let policy = effective_policy(cwd, project_trusted);
    let support = probe();
    let requested = policy.enabled && policy.mode != SandboxMode::Off;
    SandboxStatus {
        enabled: requested,
        enforced: requested && ENFORCEMENT_IMPLEMENTED && support.is_supported(),
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

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn maintenance_commands_bypass() {
        for command in [
            "install",
            "uninstall",
            "auth",
            "update",
            "extension",
            "package",
            "resource",
            "events",
            "session",
            "dev",
            "dev-local",
        ] {
            assert_eq!(
                bypass_reason(&argv(&[command])),
                Some("maintenance command"),
                "{command} must bypass"
            );
        }
        assert_eq!(bypass_reason(&argv(&["-p", "hello"])), None);
        assert_eq!(bypass_reason(&[]), None);
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

    #[test]
    fn linux_builds_advertise_enforcement() {
        assert_eq!(ENFORCEMENT_IMPLEMENTED, cfg!(target_os = "linux"));
    }
}
