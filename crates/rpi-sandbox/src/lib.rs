//! Filesystem-only sandbox policy for `rpi`.
//!
//! The design keeps three concerns separate so they can ship incrementally:
//!
//! - [`policy`] is platform-neutral: rules, expansion, precedence, and the
//!   global/project/CLI merge. It never touches the kernel and is unit-tested
//!   on every host.
//! - [`probe`] reports what the current platform can enforce. Probing is
//!   read-only and does not sandbox the process.
//! - Enforcement backends (Linux Landlock, macOS Seatbelt, Windows restricted
//!   tokens) are added in later milestones. Until then `probe` still answers
//!   honestly and the CLI keeps running without a sandbox.
//!
//! See `docs/architecture/fs-sandbox.md` for the full design, including the
//! approval protocol and the rollout plan.

#![warn(missing_docs)]

pub mod policy;
pub mod probe;

pub use policy::{
    compile_policy, expand_configured_path, normalize_and_resolve, Access, ApprovalScope,
    ApprovalSettings, PathRule, PolicyContext, PolicyOverrides, SandboxMode, SandboxPolicy,
    SandboxSettings,
};
pub use probe::{probe, SandboxBackend, SandboxSupport};
