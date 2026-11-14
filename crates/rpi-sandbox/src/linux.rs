//! Linux Landlock backend.
//!
//! Enforcement is a two-step split so the interesting part stays testable:
//! [`build_plan`] turns a [`SandboxPolicy`] into access masks and rules without
//! touching the kernel, and [`apply`] installs that plan on the calling thread.
//! Landlock domains are inherited by children and cannot be widened, which is
//! why [`apply`] must run before any worker threads start (the CLI does this in
//! `main` before building the Tokio runtime).
//!
//! Only write-side access is handled: `WRITE_FILE`, `MAKE_*`, `REMOVE_*`,
//! `REFER` and `TRUNCATE`. Reads and execution stay unrestricted, matching the
//! documented "reads everywhere, writes on an allowlist" policy.
//!
//! Known kernel limitation: Landlock is additive. An allow rule on a directory
//! grants the whole subtree, so a `protected` path nested inside a writable
//! root cannot be subtracted by the kernel. [`SandboxPolicy::nested_protected`]
//! reports those pairs; the CLI warns (or fails in `required` mode) and the
//! in-process layer owns nested protection later.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::{PathRule, SandboxPolicy};

// Landlock access bits (`include/uapi/linux/landlock.h`).
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
/// ABI v2 (Linux 5.19): rename/link across directories.
const ACCESS_FS_REFER: u64 = 1 << 13;
/// ABI v3 (Linux 6.2): `truncate(2)` and `ftruncate(2)`.
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

/// The access bits handled on every kernel that supports Landlock.
const BASE_HANDLED: u64 = ACCESS_FS_WRITE_FILE
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_CHAR
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_BLOCK
    | ACCESS_FS_MAKE_SYM;

// Landlock syscall numbers are architecture-independent (generic syscall
// table) since the interface was added.
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// One allow rule: `path` and the rights granted beneath it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlockRule {
    /// Absolute path to allow.
    pub path: PathBuf,
    /// Granted access bits (a subset of the handled mask).
    pub access: u64,
}

/// A kernel-independent description of the ruleset to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlockPlan {
    /// Detected Landlock ABI version.
    pub abi: u32,
    /// Access bits the ruleset denies unless a rule grants them.
    pub handled: u64,
    /// Allow rules, sorted by path.
    pub rules: Vec<LandlockRule>,
    /// Configured roots that do not exist and were skipped.
    pub skipped: Vec<PathBuf>,
}

/// Query the Landlock ABI version.
pub fn abi() -> Result<u32, String> {
    crate::probe::landlock_abi()
}

/// Build the ruleset for `policy` against a known ABI.
///
/// The function performs no syscalls beyond `exists()` checks, which makes it
/// unit-testable on any Linux host.
pub fn build_plan(policy: &SandboxPolicy, abi: u32) -> LandlockPlan {
    let mut handled = BASE_HANDLED;
    if abi >= 2 {
        handled |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        handled |= ACCESS_FS_TRUNCATE;
    }

    let mut by_path: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut skipped = Vec::new();

    for rule in policy
        .writable
        .iter()
        .chain(policy.allow_file_writes.iter())
    {
        let (path, access) = match rule {
            PathRule::Dir(path) => (path, handled),
            // A file rule may only grant write/truncate; `MAKE_*`, `REMOVE_*`
            // and `REFER` are meaningless on a non-directory and are rejected
            // by the kernel with EINVAL.
            PathRule::File(path) => (path, handled & (ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE)),
        };
        if !path.exists() {
            skipped.push(path.clone());
            continue;
        }
        by_path
            .entry(path.clone())
            .and_modify(|existing| *existing |= access)
            .or_insert(access);
    }

    LandlockPlan {
        abi,
        handled,
        rules: by_path
            .into_iter()
            .map(|(path, access)| LandlockRule { path, access })
            .collect(),
        skipped,
    }
}

/// Build a plan using the kernel's reported ABI.
pub fn plan(policy: &SandboxPolicy) -> Result<LandlockPlan, String> {
    Ok(build_plan(policy, abi()?))
}

/// Install the plan on the calling thread (and every future child).
///
/// Must be called before the process creates worker threads: Landlock domains
/// are per-thread and inherited on clone/exec, so threads created earlier stay
/// unrestricted.
pub fn apply(plan: &LandlockPlan) -> Result<(), String> {
    let attr = RulesetAttr {
        handled_access_fs: plan.handled,
    };
    // SAFETY: `RulesetAttr` is `#[repr(C)]` and matches the kernel's
    // `struct landlock_ruleset_attr` for the single-field version we use; the
    // size argument is exactly `size_of::<RulesetAttr>()`.
    let ruleset_fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0_u32,
        )
    };
    if ruleset_fd < 0 {
        return Err(format!(
            "landlock_create_ruleset failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ruleset_fd = ruleset_fd as libc::c_int;
    let _ruleset = FdGuard(ruleset_fd);

    for rule in &plan.rules {
        let c_path = CString::new(rule.path.as_os_str().as_bytes())
            .map_err(|_| format!("path contains a NUL byte: {}", rule.path.display()))?;
        // SAFETY: `c_path` is a valid NUL-terminated string; `O_PATH` opens the
        // inode without requiring read permission, which is what Landlock's
        // PATH_BENEATH rule needs.
        let path_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if path_fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                // Raced with deletion; the root simply is not writable.
                continue;
            }
            return Err(format!(
                "cannot open {} for landlock: {error}",
                rule.path.display()
            ));
        }
        let path_fd = FdGuard(path_fd);
        let path_beneath = PathBeneathAttr {
            allowed_access: rule.access,
            parent_fd: path_fd.0,
        };
        // SAFETY: the ruleset fd is valid, the rule type is
        // LANDLOCK_RULE_PATH_BENEATH, and the attribute struct is `#[repr(C)]`
        // with the exact size the kernel expects.
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd,
                LANDLOCK_RULE_PATH_BENEATH,
                &path_beneath as *const PathBeneathAttr,
                0_u32,
            )
        };
        if rc < 0 {
            return Err(format!(
                "landlock_add_rule failed for {}: {}",
                rule.path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }

    // SAFETY: `prctl` with PR_SET_NO_NEW_PRIVS takes three integer arguments;
    // the kernel requires it before an unprivileged restrict_self.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc < 0 {
        return Err(format!(
            "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: restricts the calling thread to the ruleset above; flags must be
    // zero. The domain is inherited by children and cannot be widened.
    let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0_u32) };
    if rc < 0 {
        return Err(format!(
            "landlock_restrict_self failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

struct FdGuard(libc::c_int);

impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: the fd is owned by this guard and closed exactly once.
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApprovalSettings, PolicyContext, PolicyOverrides, SandboxMode, SandboxSettings};

    fn policy_for(settings: &SandboxSettings) -> SandboxPolicy {
        crate::compile_policy(
            Some(settings),
            None,
            &PolicyOverrides::default(),
            &PolicyContext {
                cwd: PathBuf::from("/tmp"),
                home: Some(PathBuf::from("/tmp")),
                temp: PathBuf::from("/tmp"),
            },
            &[],
        )
    }

    #[test]
    fn handled_mask_follows_abi() {
        let policy = policy_for(&SandboxSettings {
            enabled: Some(true),
            ..SandboxSettings::default()
        });

        let v1 = build_plan(&policy, 1);
        assert_eq!(v1.handled & ACCESS_FS_REFER, 0);
        assert_eq!(v1.handled & ACCESS_FS_TRUNCATE, 0);
        assert_ne!(v1.handled & ACCESS_FS_WRITE_FILE, 0);

        let v3 = build_plan(&policy, 3);
        assert_ne!(v3.handled & ACCESS_FS_REFER, 0);
        assert_ne!(v3.handled & ACCESS_FS_TRUNCATE, 0);
    }

    #[test]
    fn dir_rules_grant_the_handled_mask() {
        let policy = policy_for(&SandboxSettings {
            enabled: Some(true),
            ..SandboxSettings::default()
        });
        let plan = build_plan(&policy, 6);
        let tmp_rule = plan
            .rules
            .iter()
            .find(|rule| rule.path.as_path() == std::path::Path::new("/tmp"))
            .expect("/tmp rule");
        assert_eq!(tmp_rule.access, plan.handled);
    }

    #[test]
    fn file_rules_grant_only_write_and_truncate() {
        let policy = policy_for(&SandboxSettings {
            enabled: Some(true),
            allow_file_writes: vec!["/dev/null".to_string()],
            ..SandboxSettings::default()
        });
        let plan = build_plan(&policy, 6);
        let dev_null = plan
            .rules
            .iter()
            .find(|rule| rule.path.as_path() == std::path::Path::new("/dev/null"))
            .expect("/dev/null rule");
        assert_eq!(dev_null.access, ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE);
    }

    #[test]
    fn missing_roots_are_skipped() {
        let policy = policy_for(&SandboxSettings {
            enabled: Some(true),
            writable: vec!["/definitely/not/present/rpi-sandbox".to_string()],
            ..SandboxSettings::default()
        });
        let plan = build_plan(&policy, 6);
        assert!(plan
            .skipped
            .iter()
            .any(|path| path.as_path()
                == std::path::Path::new("/definitely/not/present/rpi-sandbox")));
        assert!(!plan.rules.iter().any(|rule| rule.path.as_path()
            == std::path::Path::new("/definitely/not/present/rpi-sandbox")));
    }

    #[test]
    fn duplicate_roots_are_merged() {
        let policy = SandboxPolicy {
            enabled: true,
            mode: SandboxMode::Auto,
            writable: vec![
                PathRule::Dir(PathBuf::from("/tmp")),
                PathRule::Dir(PathBuf::from("/tmp")),
            ],
            protected: Vec::new(),
            allow_file_writes: Vec::new(),
            approval: ApprovalSettings::default(),
            audit_log: None,
        };
        let plan = build_plan(&policy, 6);
        assert_eq!(
            plan.rules
                .iter()
                .filter(|rule| rule.path.as_path() == std::path::Path::new("/tmp"))
                .count(),
            1
        );
    }
}
