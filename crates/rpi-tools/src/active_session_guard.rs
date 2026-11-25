//! Protection for the session files the *currently active* harness owns.
//!
//! `rpi` writes its own transcript through the same [`OsExecutionEnv`] that
//! backs the built-in `write`/`edit`/`bash` tools. Without an explicit boundary,
//! a tool call (or an extension sharing that env) can truncate or replace the
//! live `<session>.jsonl`, its `.progress.jsonl` companion, or its `.lock`
//! while the harness is still appending to them. That produces a transcript the
//! running session can no longer trust and that no later repair can reconcile.
//!
//! This guard is deliberately narrow:
//!
//! * It is **default-on** for every [`OsExecutionEnv`] that has not been given
//!   a writer capability (see [`grant_writer`]).
//! * It protects **only** the three canonical paths of the *current* active
//!   session. Other `.jsonl` files, `--export` targets, and other sessions'
//!   files are untouched. `--no-session` (no active session) disables it.
//! * It compares canonicalized paths, so a relative path, a `..` component, or
//!   a symlink alias that resolves to a protected file is still rejected.
//! * Read operations are never blocked.
//!
//! `bash` children do not call `FileSystem`. [`check_bash_command`] rejects
//! obvious write-intent commands that name a protected path before spawn, on
//! every platform. On **Linux only**, a small `LD_PRELOAD` shim compiled at
//! build time and embedded in the binary ([`child_barrier_env`]) additionally
//! interposes `open`/`open64`/`fopen`/`openat`/`truncate`/`unlink`/`rename`
//! and fails write-intent calls with `EPERM` for the child's whole lifetime.
//! It needs no C compiler on the user's machine. All other writes pass through
//! unchanged.
//!
//! The child-process barrier is a dynamic-loader mechanism, not a kernel
//! sandbox: it only covers dynamically linked children on Linux, and a child
//! that unsets `LD_PRELOAD` or issues raw syscalls bypasses it. macOS
//! (`DYLD_INSERT_LIBRARIES` is restricted by SIP for protected binaries) and
//! Windows have no equivalent here, so on those platforms only the pre-flight
//! and tool-layer checks apply; an indirect child write that hides the path
//! from the command text is not stopped. The PR body states that boundary
//! explicitly; the general cross-platform sandbox is a separate feature.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{FileError, FileErrorCode};

/// The three paths that belong to one active session, canonicalized up front.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveSessionFiles {
    pub jsonl: PathBuf,
    pub progress: PathBuf,
    pub lock: PathBuf,
}

impl ActiveSessionFiles {
    /// Derive the companion paths from a session `.jsonl` path and canonicalize
    /// all three. Missing files are canonicalized via their nearest existing
    /// ancestor (the tails are appended literally), so a not-yet-created
    /// `.lock` is still comparable to a later `open(O_CREAT)`.
    pub fn derive(jsonl: &str) -> Self {
        Self {
            jsonl: canonicalize_lenient(jsonl),
            progress: canonicalize_lenient(&format!("{jsonl}.progress.jsonl")),
            lock: canonicalize_lenient(&format!("{jsonl}.lock")),
        }
    }

    fn contains(&self, canonical: &Path) -> bool {
        self.jsonl == canonical || self.progress == canonical || self.lock == canonical
    }
}

/// Canonicalize `path`, tolerating a missing final component. Symlinks in any
/// existing prefix are resolved; the non-existing suffix stays literal. This is
/// what makes a symlink alias written *after* the guard was installed still
/// match: the alias itself resolves to the protected path via `canonicalize`.
pub fn canonicalize_lenient(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut existing = p.as_path();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !existing.as_os_str().is_empty() {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        if let Some(name) = existing.file_name() {
            suffix.push(name.to_os_string());
        }
        match existing.parent() {
            Some(parent) if parent != existing => existing = parent,
            _ => break,
        }
    }
    p
}

/// Process-wide state. One `rpi` process serves one active session at a time;
/// the mutex keeps hot-swap (`/new`, `/fork`, `--session`) and concurrent tool
/// calls from observing a half-installed protection set.
#[derive(Default)]
struct GlobalGuard {
    active: Option<ActiveSessionFiles>,
    /// Data-pointer identities of `FileSystem` objects that hold the explicit
    /// harness writer capability, with one reference count per distinct grant.
    /// Multiple `JsonlSessionStorage` bindings can share one `Arc<dyn
    /// FileSystem>` (a fork's read-only source snapshot and its destination do);
    /// dropping one temporary binding must not revoke the surviving writer.
    writers: HashMap<usize, usize>,
    /// Incremented by [`clear_session_guard`] so a stale capability dropped
    /// after teardown cannot decrement a later grant that reused the same
    /// address.
    epoch: u64,
}

static GLOBAL: OnceLock<Mutex<GlobalGuard>> = OnceLock::new();

fn global() -> &'static Mutex<GlobalGuard> {
    GLOBAL.get_or_init(|| Mutex::new(GlobalGuard::default()))
}

/// Opaque token proving that this env instance is the harness-owned writer.
/// The `Arc` is shared by every clone of one storage binding; each independent
/// grant holds its own reference count and releases only its own grant.
pub struct WriterCapability {
    key: usize,
    epoch: u64,
}

impl Drop for WriterCapability {
    fn drop(&mut self) {
        let mut g = global().lock().unwrap_or_else(|p| p.into_inner());
        if g.epoch != self.epoch {
            return;
        }
        let Some(count) = g.writers.get_mut(&self.key) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            g.writers.remove(&self.key);
        }
    }
}

/// CLI-facing activation. `Some(path)` protects that session's three files;
/// `None` (`--no-session` / ephemeral) disables the guard entirely.
///
/// Writer grants are preserved across a session swap: the harness-owned
/// storage bindings survive and must keep append/repair/publish access.
pub fn activate_session_guard(jsonl_path: Option<&str>) {
    let mut g = global().lock().unwrap_or_else(|p| p.into_inner());
    g.active = jsonl_path.map(ActiveSessionFiles::derive);
}

/// Explicit teardown for shutdown/tests. Unlike `activate_session_guard(None)`,
/// this also revokes all live writer grants; the epoch keeps capabilities that
/// are dropped later from affecting a subsequent grant.
pub fn clear_session_guard() {
    let mut g = global().lock().unwrap_or_else(|p| p.into_inner());
    g.active = None;
    g.writers.clear();
    g.epoch = g.epoch.wrapping_add(1);
}

/// Read-only snapshot of the current protected paths (for tests/introspection).
pub fn active_files() -> Option<ActiveSessionFiles> {
    global()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .active
        .clone()
}

/// Grant the explicit writer capability to one `FileSystem` object. Called by
/// `JsonlSessionStorage` when it binds to an env.
pub fn grant_writer(fs: &dyn crate::env::FileSystem) -> Arc<WriterCapability> {
    let key = fs as *const dyn crate::env::FileSystem as *const () as usize;
    let mut g = global().lock().unwrap_or_else(|p| p.into_inner());
    *g.writers.entry(key).or_insert(0) += 1;
    let epoch = g.epoch;
    Arc::new(WriterCapability { key, epoch })
}

/// Whether `fs` currently holds the harness writer capability.
pub fn has_writer(fs: &dyn crate::env::FileSystem) -> bool {
    let key = fs as *const dyn crate::env::FileSystem as *const () as usize;
    global()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .writers
        .contains_key(&key)
}

/// Whether `canonical` is one of the current active session's three files.
pub fn is_protected(canonical: &Path) -> bool {
    global()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .active
        .as_ref()
        .is_some_and(|a| a.contains(canonical))
}

/// The one error surfaced for every denied mutation.
pub fn denied(path: &str) -> FileError {
    FileError::new(
        FileErrorCode::PermissionDenied,
        "cannot rewrite the active session log with a tool; tool calls, extensions, and bash cannot modify the live <session>.jsonl, .jsonl.progress.jsonl or .jsonl.lock. Reopen the session to change its history, or write to a different file",
    )
    .with_path(path)
}

/// Check one mutation target. `resolve` maps the tool-supplied path (relative,
/// `~`, `file://`, symlink alias, `..`) to an addressed absolute path; the
/// guard canonicalizes that result before comparing.
pub fn check_write(
    path: &str,
    resolve: impl FnOnce(&str) -> PathBuf,
    fs: &dyn crate::env::FileSystem,
) -> Result<(), FileError> {
    if has_writer(fs) {
        return Ok(());
    }
    let addressed = resolve(path);
    let canonical = canonicalize_lenient(&addressed.to_string_lossy());
    if is_protected(&canonical) {
        return Err(denied(path));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux child-process barrier (LD_PRELOAD)
// ---------------------------------------------------------------------------

/// Whether this build installs a child-process barrier. It is **true only on
/// Linux**: macOS limits `DYLD_INSERT_LIBRARIES` for protected binaries and
/// Windows has no equivalent, so elsewhere only the pre-flight command check
/// and the cross-platform tool-layer `FileSystem` guard apply. Exposed so
/// callers and tests can state the boundary without a `cfg` of their own.
pub const CHILD_BARRIER_AVAILABLE: bool = cfg!(target_os = "linux");

/// Environment variable read by the preload shim: colon-separated canonical
/// paths of the three protected files. Set only on the child's `Command`.
/// (A colon is unusual but legal in a filename; the tool-layer check and the
/// canonical `check_write` path guard remain the fallback for that case.)
#[cfg(target_os = "linux")]
pub const CHILD_BARRIER_ENV: &str = "RPI_ACTIVE_SESSION_FILES";

/// The shim interposes the libc entry points that a dynamically linked
/// interpreter (`python3 -c "open(path,'wb')"`, `tee`, `cp`, `sed -i`, shell
/// `>` redirection) uses to open a file for writing. On a match it sets
/// `errno=EPERM` and returns failure *before* the syscall, so the file is never
/// truncated or created. All other paths and all read-only opens are forwarded
/// to the real libc symbol via `dlsym(RTLD_NEXT, ...)`. The C source lives in
/// `guard_shim.c` next to this crate and is compiled by `build.rs` at build
/// time (not on the user's machine); see the module header for the boundary.

/// Materialize (once per process) the build-time shared object and return its
/// path. The bytes are embedded in the binary by `include_bytes!`, so a host C
/// compiler is **not** needed at runtime: only a writable temp dir is. A
/// write failure is returned to the caller so `Shell::exec` refuses the spawn
/// instead of silently running an unprotected child.
#[cfg(target_os = "linux")]
fn child_barrier_so() -> Result<PathBuf, String> {
    static SO: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    SO.get_or_init(|| {
        const SO_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libguard.so"));
        let dir = std::env::temp_dir().join(format!("rpi-active-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("guard shim dir: {e}"))?;
        let so = dir.join("libguard.so");
        std::fs::write(&so, SO_BYTES).map_err(|e| format!("guard shim install: {e}"))?;
        Ok(so)
    })
    .clone()
}

/// Compute the two env entries that install the child barrier. Returns `None`
/// when no active session is installed (`--no-session`), or an error when the
/// shim cannot be built — failing open would let a bash child truncate the live
/// transcript, so the caller must refuse the spawn.
#[cfg(target_os = "linux")]
pub fn child_barrier_env(
    existing_ld_preload: Option<&std::ffi::OsStr>,
) -> Result<Option<(std::ffi::OsString, std::ffi::OsString)>, String> {
    let Some(active) = active_files() else {
        return Ok(None);
    };
    let so = child_barrier_so()?;
    let mut ld = so.as_os_str().to_os_string();
    if let Some(existing) = existing_ld_preload {
        if !existing.is_empty() {
            ld.push(":");
            ld.push(existing);
        }
    }
    // Colon-separated canonical paths. `std::process::Command` rejects NUL
    // bytes in env values; the shim's realpath() resolves symlink aliases and
    // `..` at the syscall boundary itself.
    let mut paths = active.jsonl.as_os_str().to_os_string();
    for p in [&active.progress, &active.lock] {
        paths.push(":");
        paths.push(p.as_os_str());
    }
    Ok(Some((ld, paths)))
}

/// Tokens that indicate a command intends to write, replace, truncate or
/// delete a path. Used only to avoid blocking read-only commands such as
/// `cat <active>.jsonl`; the child barrier is the authoritative enforcement.
const WRITE_INTENT_TOKENS: &[&str] = &[
    "tee", "cp ", "mv ", "rm ", "dd ", "truncate", "sed -i", "perl -i", "wb", "'w'", "\"w\"", "w+",
    "install ", "patch ",
];

/// Redirections such as `> file` and `2> file` are write intent. `2>&1`,
/// `1>&2`, here-strings and comparison operators are not, even though they
/// contain `>`/`>`-adjacent characters.
fn has_output_redirection(command: &str) -> bool {
    let bytes = command.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'>' {
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] != b'&' && bytes[j] != b'>' {
            return true;
        }
    }
    false
}

fn has_write_intent(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    has_output_redirection(command) || WRITE_INTENT_TOKENS.iter().any(|t| lower.contains(t))
}

/// Extract path-like tokens from a shell command. Enough to catch a literal
/// absolute path, a relative path, or a quoted path embedded in `python3 -c`.
/// Shell variable assignments are skipped: `path=active.jsonl` is not itself
/// a write, while `$path` would be caught by the child barrier if opened.
fn command_path_tokens(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut assignment = false;
    for ch in command.chars() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '|' | ';' | '&' | '(' | ')' | '<' | '>' | '{' | '}' | '[' | ']' | ',' | '='
            )
        {
            if ch == '=' {
                assignment = true;
            }
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else if ch != '\'' && ch != '"' && ch != '`' {
            current.push(ch);
        }
        if ch.is_whitespace() {
            assignment = false;
        }
    }
    if !current.is_empty() && !assignment {
        out.push(current);
    }
    // The assignment flag only suppresses the *value* token; rediscover any
    // path-like tokens that follow it on the same command line.
    out.into_iter()
        .filter(|token| {
            !token.is_empty()
                && (token.contains('/') || token.ends_with(".jsonl") || token.ends_with(".lock"))
        })
        .collect()
}

/// Fast, explicit denial at the `Shell::exec` boundary for commands that name
/// an active session file. The preload shim remains the authoritative barrier
/// for spawned children; this check gives a clear, actionable error instead of
/// a low-level `EPERM` from an arbitrary descendant.
pub fn check_bash_command(command: &str, cwd: &Path) -> Result<(), FileError> {
    let Some(active) = active_files() else {
        return Ok(());
    };
    if !has_write_intent(command) {
        return Ok(());
    }
    let canonical_strings: Vec<String> = [
        active.jsonl.clone(),
        active.progress.clone(),
        active.lock.clone(),
    ]
    .iter()
    .map(|p| p.to_string_lossy().into_owned())
    .collect();
    // Literal embedded path (e.g. `open('/x/s.jsonl','wb')`).
    if canonical_strings.iter().any(|p| command.contains(p)) {
        return Err(denied("<bash command>"));
    }
    // Resolve path-like tokens: relative paths, `..`, and symlink aliases all
    // canonicalize back to the protected file.
    for token in command_path_tokens(command) {
        if !token.contains('/') && !token.ends_with(".jsonl") && !token.ends_with(".lock") {
            continue;
        }
        let addressed = if Path::new(&token).is_absolute() {
            PathBuf::from(&token)
        } else {
            cwd.join(&token)
        };
        let canonical = canonicalize_lenient(&addressed.to_string_lossy());
        if active.contains(&canonical) {
            return Err(denied(&token));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os_env::OsExecutionEnv;

    #[test]
    fn derive_and_lenient_canonicalize_share_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("s.jsonl");
        std::fs::write(&jsonl, b"{}").unwrap();
        let files = ActiveSessionFiles::derive(&jsonl.to_string_lossy());
        assert_eq!(files.jsonl, jsonl.canonicalize().unwrap());
        assert!(files.contains(&canonicalize_lenient(&jsonl.to_string_lossy())));
    }

    #[test]
    fn output_redirections_are_distinguished_from_fd_duplication() {
        assert!(has_write_intent("echo CORRUPT > active.jsonl"));
        assert!(has_write_intent("echo CORRUPT 2> active.jsonl"));
        assert!(!has_write_intent("cat active.jsonl 2>&1"));
        assert!(!has_write_intent("cat active.jsonl 1>&2"));
        // Append is write intent too (O_APPEND); the child barrier enforces it,
        // so the pre-flight check must not exempt `>>`.
        assert!(has_write_intent("cat active.jsonl >> /dev/null"));
    }

    /// Review PR #345 blocker 1: a normal durable-session bash command must
    /// not depend on a C compiler at runtime. Clear `PATH` around the shim
    /// installation so any `Command::new("cc")` would fail to spawn; the
    /// build-time embedded bytes must still materialize the .so. This calls
    /// `child_barrier_so` directly so it does not touch the process-global
    /// active-session state shared by the other tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn child_barrier_needs_no_runtime_compiler() {
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let result = child_barrier_so();
        match original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let so = result.expect("the shim must install without a compiler on PATH");
        assert!(
            so.exists(),
            "the build-time embedded shim must be materialized: {}",
            so.display()
        );
        assert!(
            std::fs::metadata(&so).unwrap().len() > 0,
            "materialized shim must not be empty"
        );
    }

    #[test]
    fn independent_writer_grants_share_a_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_path_buf()));
        let fs: Arc<dyn crate::env::FileSystem> = env.clone();
        clear_session_guard();
        let first = grant_writer(fs.as_ref());
        let second = grant_writer(fs.as_ref());
        assert!(has_writer(fs.as_ref()));
        drop(first);
        assert!(
            has_writer(fs.as_ref()),
            "dropping one binding must not revoke another binding sharing the env"
        );
        drop(second);
        assert!(!has_writer(fs.as_ref()));
        clear_session_guard();
    }
}
