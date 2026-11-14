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
//! - sandbox disabled (the default, opt-in): stay silent, do nothing;
//! - maintenance subcommand (`install`, `update`, `auth`, `dev`, ...): bypass
//!   and say so once;
//! - enabled but unavailable: warn and keep running (`auto`), or refuse to
//!   start (`required`);
//! - enabled and available: apply, or fall back with a warning when applying
//!   fails and `mode = "auto"`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rpi_sandbox::{
    compile_policy, probe, PolicyContext, PolicyOverrides, SandboxMode, SandboxPolicy,
    SandboxSupport,
};

/// Process-wide outcome recorded by [`prepare_from_argv`]. `None` in embedded
/// hosts and library tests that never call the CLI preflight.
static ACTIVE: OnceLock<SandboxStatus> = OnceLock::new();
static ACTIVE_POLICY: OnceLock<SandboxPolicy> = OnceLock::new();
/// Broker connection inherited from the supervisor (Linux, M2).
#[cfg(unix)]
static BROKER: OnceLock<rpi_sandbox::BrokerClient> = OnceLock::new();

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
        record_status(SandboxStatus {
            state: SandboxState::Bypassed,
            mode: SandboxMode::Auto,
            detail: reason.to_string(),
        });
        return Ok(Prepared::Bypassed { reason });
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let parsed = crate::args::parse_args(argv);
    let trusted = crate::session::resolve_project_trust(&parsed, &cwd);
    let policy = policy_for_posture(&parsed, &cwd, trusted);
    let _ = ACTIVE_POLICY.set(policy.clone());

    if !policy.enabled || policy.mode == SandboxMode::Off {
        record_status(SandboxStatus {
            state: SandboxState::Disabled,
            mode: policy.mode,
            detail: "disabled".to_string(),
        });
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
        SandboxSupport::Supported(backend) => {
            // The supervisor/broker is Linux-only for now (M4 covers Windows).
            #[cfg(target_os = "linux")]
            if let Err(error) = start_broker(&policy) {
                match policy.mode {
                    SandboxMode::Required => {
                        return Err(format!("cannot start the approval broker: {error}"))
                    }
                    _ => eprintln!("warning: {error}; approvals are unavailable"),
                }
            }
            let outcome = apply_backend(&policy)
                .map_err(|error| format!("failed to apply the filesystem sandbox: {error}"));
            match outcome {
                Ok(()) => {
                    record_status(SandboxStatus {
                        state: SandboxState::Applied,
                        mode: policy.mode,
                        detail: format!("{} (abi {})", backend.name(), abi_of(backend)),
                    });
                    Ok(Prepared::Applied)
                }
                Err(error) => match policy.mode {
                    SandboxMode::Required => Err(error),
                    _ => {
                        eprintln!("warning: {error}; running without a sandbox");
                        record_status(SandboxStatus {
                            state: SandboxState::Unavailable,
                            mode: policy.mode,
                            detail: error,
                        });
                        Ok(Prepared::Disabled)
                    }
                },
            }
        }
    }
}

fn abi_of(backend: rpi_sandbox::SandboxBackend) -> u32 {
    match backend {
        rpi_sandbox::SandboxBackend::Landlock { abi } => abi,
        _ => 0,
    }
}

fn record_status(status: SandboxStatus) {
    let _ = ACTIVE.set(status);
}

/// Fork the unconfined broker before the sandbox is applied.
///
/// The child keeps the sandboxed path (returning normally), the parent becomes
/// the supervisor: it serves approval requests, waits for the child and exits
/// with its status. Must run while the process is still single-threaded.
#[cfg(target_os = "linux")]
fn start_broker(policy: &SandboxPolicy) -> Result<(), String> {
    use std::io::BufReader;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;

    if policy.approval.enabled == Some(false) {
        return Ok(());
    }

    let mut fds = [0_i32; 2];
    // SAFETY: `fds` is a valid two-element array for socketpair(2).
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc < 0 {
        return Err(format!(
            "socketpair for the approval broker failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: fork(2) in a single-threaded process; the child only builds the
    // client and returns to the caller, the parent never returns.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        // SAFETY: both fds are owned by this process on the error path.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(format!(
            "fork for the approval broker failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    if pid == 0 {
        // Child: keep the agent end and continue into the sandboxed startup.
        // SAFETY: fds[0] belongs to the parent.
        unsafe { libc::close(fds[0]) };
        // SAFETY: fds[1] is a connected Unix stream owned by this process.
        let client = unsafe { rpi_sandbox::BrokerClient::from_raw_fd(fds[1]) }
            .map_err(|error| format!("cannot attach to the approval broker: {error}"))?;
        let _ = BROKER.set(client);
        return Ok(());
    }

    // Parent: become the supervisor. Ignore terminal signals so the TUI child
    // owns Ctrl+C / termination; exit with the child's status.
    // SAFETY: fds[1] belongs to the child; the signal dispositions are
    // process-wide and the supervisor has no UI.
    unsafe {
        libc::close(fds[1]);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    // SAFETY: fds[0] is a connected Unix stream owned by this process.
    let stream = unsafe { UnixStream::from_raw_fd(fds[0]) };
    let reader = match stream.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(error) => {
            eprintln!("warning: approval broker cannot read its socket: {error}");
            std::process::exit(1);
        }
    };
    let _ = rpi_sandbox::broker::serve(
        reader,
        stream,
        rpi_sandbox::BrokerConfig {
            audit: policy.audit_log.clone(),
        },
    );
    let mut status = 0;
    // SAFETY: `pid` is the child created above; `status` is a valid out pointer.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    };
    std::process::exit(code);
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
    let _ = ACTIVE.set(SandboxStatus {
        state: SandboxState::Unavailable,
        mode: policy.mode,
        detail: reason.clone(),
    });
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

    let overrides = args
        .map(policy_overrides)
        .unwrap_or_else(|| policy_overrides(&crate::args::Args::default()));
    compile_policy(
        global.sandbox.as_ref(),
        project
            .as_ref()
            .and_then(|settings| settings.sandbox.as_ref()),
        &overrides,
        &context,
        &auto_writable,
    )
}

fn policy_overrides(args: &crate::args::Args) -> PolicyOverrides {
    PolicyOverrides {
        enabled: args.sandbox.or_else(env_sandbox_override),
        mode: args.sandbox_mode,
        writable: args.allow_write.clone(),
        protected: Vec::new(),
        allow_file_writes: Vec::new(),
    }
}

fn env_sandbox_override() -> Option<bool> {
    let value = std::env::var("RPI_SANDBOX").ok()?;
    Some(!matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    ))
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

/// What the startup preflight actually did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxState {
    /// The sandbox is turned off by configuration or a CLI flag.
    Disabled,
    /// The process is confined by the kernel backend.
    Applied,
    /// A sandbox was requested but the platform/build cannot enforce it.
    Unavailable,
    /// A maintenance subcommand runs without a sandbox on purpose.
    Bypassed,
}

impl SandboxState {
    /// Stable lowercase label for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Applied => "applied",
            Self::Unavailable => "unavailable",
            Self::Bypassed => "bypassed",
        }
    }
}

/// Snapshot used by the TUI footer, the `/sandbox` panel and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStatus {
    /// What the preflight did.
    pub state: SandboxState,
    /// Configured failure behavior.
    pub mode: SandboxMode,
    /// Backend description (for example `landlock (abi 6)`) or the reason the
    /// sandbox is off.
    pub detail: String,
}

impl SandboxStatus {
    /// Whether the process is actually confined.
    pub fn enforced(&self) -> bool {
        self.state == SandboxState::Applied
    }

    /// Compact badge for the TUI footer. `None` means "do not render one".
    pub fn footer_label(&self) -> Option<String> {
        Some(match self.state {
            SandboxState::Applied => format!("🔒 {}", short_backend(&self.detail)),
            SandboxState::Bypassed => "⏸ sandbox bypassed".to_string(),
            SandboxState::Disabled => "🔓 sandbox off".to_string(),
            SandboxState::Unavailable => "🔓 sandbox unavailable".to_string(),
        })
    }

    /// Multi-line description for `/sandbox`.
    pub fn describe(&self) -> String {
        format!(
            "sandbox: {}\nmode: {}\nbackend: {}",
            self.state.as_str(),
            mode_label(self.mode),
            self.detail
        )
    }
}

fn short_backend(detail: &str) -> &str {
    detail.split(' ').next().unwrap_or(detail)
}

fn mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Off => "off",
        SandboxMode::Auto => "auto",
        SandboxMode::Required => "required",
    }
}

/// The status recorded by the CLI preflight, when this process ran it.
pub fn active_status() -> Option<&'static SandboxStatus> {
    ACTIVE.get()
}

/// The compiled policy recorded by the CLI preflight, when available.
pub fn active_policy() -> Option<&'static SandboxPolicy> {
    ACTIVE_POLICY.get()
}

/// The inherited broker connection, when the supervisor is running.
#[cfg(unix)]
pub fn broker_client() -> Option<&'static rpi_sandbox::BrokerClient> {
    BROKER.get()
}

/// Build a status snapshot: the recorded preflight outcome when present,
/// otherwise a best-effort recomputation for embedded callers.
pub fn status(cwd: &Path, project_trusted: bool) -> SandboxStatus {
    if let Some(status) = ACTIVE.get() {
        return status.clone();
    }
    let policy = effective_policy(cwd, project_trusted);
    let support = probe();
    let requested = policy.enabled && policy.mode != SandboxMode::Off;
    SandboxStatus {
        state: if !requested {
            SandboxState::Disabled
        } else if ENFORCEMENT_IMPLEMENTED && support.is_supported() {
            SandboxState::Applied
        } else {
            SandboxState::Unavailable
        },
        mode: policy.mode,
        detail: support.describe(),
    }
}

/// Full description for `/sandbox`, including the effective writable and
/// protected rules.
pub fn describe(cwd: &Path, project_trusted: bool) -> String {
    let status = status(cwd, project_trusted);
    let mut out = status.describe();
    let policy = ACTIVE_POLICY
        .get()
        .cloned()
        .unwrap_or_else(|| effective_policy(cwd, project_trusted));
    out.push_str("\n\nwritable:");
    for rule in &policy.writable {
        out.push_str(&format!("\n  {}", rule.path().display()));
    }
    if policy.protected.is_empty() {
        out.push_str("\n\nprotected: none");
    } else {
        out.push_str("\n\nprotected:");
        for rule in &policy.protected {
            out.push_str(&format!("\n  {}", rule.path().display()));
        }
    }
    let nested = policy.nested_protected();
    if !nested.is_empty() {
        out.push_str("\n\nnested (not kernel-enforced):");
        for (protected, writable) in nested {
            out.push_str(&format!(
                "\n  {} inside {}",
                protected.display(),
                writable.display()
            ));
        }
    }
    out.push_str(
        "\n\nRPI_SANDBOX=0 or --no-sandbox disables it; --sandbox-mode required fails closed.",
    );
    out
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

    #[test]
    fn footer_labels_cover_every_state() {
        let base = |state| SandboxStatus {
            state,
            mode: SandboxMode::Auto,
            detail: "landlock (abi 6)".to_string(),
        };
        assert_eq!(
            base(SandboxState::Applied).footer_label().as_deref(),
            Some("🔒 landlock")
        );
        assert_eq!(
            base(SandboxState::Disabled).footer_label().as_deref(),
            Some("🔓 sandbox off")
        );
        assert_eq!(
            base(SandboxState::Unavailable).footer_label().as_deref(),
            Some("🔓 sandbox unavailable")
        );
        assert_eq!(
            base(SandboxState::Bypassed).footer_label().as_deref(),
            Some("⏸ sandbox bypassed")
        );
        assert!(base(SandboxState::Applied).enforced());
        assert!(!base(SandboxState::Unavailable).enforced());
    }
}
