//! Kernel-level checks for the Linux Landlock backend.
//!
//! Landlock cannot be removed once applied, so enforcement is exercised in a
//! child process: the parent spawns this same test binary with `--exact
//! landlock_child` and a temp directory to allow. The child applies the plan,
//! then asserts that allowed writes succeed, reads stay unrestricted, and
//! writes outside the allowlist fail with `EACCES`.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use rpi_sandbox::linux;
use rpi_sandbox::{Access, ApprovalSettings, PathRule, SandboxMode, SandboxPolicy};

fn policy(allowed: PathBuf) -> SandboxPolicy {
    SandboxPolicy {
        enabled: true,
        mode: SandboxMode::Auto,
        writable: vec![PathRule::Dir(allowed)],
        protected: Vec::new(),
        allow_file_writes: Vec::new(),
        approval: ApprovalSettings::default(),
        audit_log: None,
    }
}

/// Runs only inside the spawned child; a no-op in the normal test suite.
#[test]
fn landlock_child() {
    let Ok(root) = std::env::var("RPI_LANDLOCK_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let app_policy = policy(root.clone());

    let plan = linux::plan(&app_policy).expect("landlock plan");
    linux::apply(&plan).expect("apply landlock");

    // 1. Writes inside the allowlist succeed.
    std::fs::write(root.join("allowed.txt"), b"ok").expect("write inside allowlist");
    assert_eq!(
        std::fs::read_to_string(root.join("allowed.txt")).unwrap(),
        "ok"
    );

    // 2. Reads outside the allowlist stay unrestricted.
    std::fs::read_to_string("/etc/hosts").expect("read outside allowlist");

    // 3. Writes outside the allowlist are blocked with EACCES.
    let denied = std::env::temp_dir().join(format!("rpi-sandbox-denied-{}", std::process::id()));
    let error = std::fs::write(&denied, b"no").expect_err("write outside allowlist must fail");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "unexpected error: {error}"
    );

    // 4. The policy agrees with the kernel for the same paths.
    assert_eq!(
        app_policy.classify(&root.join("allowed.txt")),
        Access::Writable
    );
    assert!(!app_policy.is_writable(&denied));
}

#[test]
fn enforcement_blocks_writes_outside_the_allowlist() {
    if !rpi_sandbox::probe().is_supported() {
        eprintln!("landlock unavailable; skipping enforcement test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = std::env::current_exe().expect("test binary path");
    let status = Command::new(exe)
        .args(["--exact", "landlock_child", "--nocapture"])
        .env("RPI_LANDLOCK_TEST_ROOT", dir.path())
        .status()
        .expect("spawn child");
    assert!(status.success(), "child enforcement test failed: {status}");
}
