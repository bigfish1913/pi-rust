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
//! `bash` children do not call `FileSystem`. On Linux they are covered by a
//! small `LD_PRELOAD` shim built into this crate ([`child_barrier_env`]): it
//! interposes `open`/`open64`/`fopen`/`openat`/`truncate`/`unlink`/`rename`
//! and fails write-intent calls on the three protected paths with `EPERM`
//! *after the child has spawned*, for the child's whole lifetime. All other
//! writes pass through unchanged. [`check_bash_command`] additionally refuses
//! obvious write-intent commands before spawning so the user gets an
//! actionable error instead of a raw libc `EPERM`.
//!
//! This is an OS dynamic-loader barrier, not a kernel sandbox. Linux Landlock
//! is an allowlist (rules are OR-ed inside a layer) and cannot express "allow
//! all writes except these three files"; unprivileged mount namespaces with
//! read-only bind mounts are not available in every container. A child that
//! deliberately unsets `LD_PRELOAD` or issues raw syscalls bypasses the shim;
//! the PR body states that residual boundary explicitly.

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

/// The C shim source, embedded here so the guard and its interposition
/// contract are reviewable together. [`child_barrier_so`] compiles it once per
/// process with the host C compiler: that is the same compiler Python/tee/sed
/// children are linked against, so the interposed libc symbols match the
/// running system even for cross-target Rust builds where a C cross-compiler
/// is not configured.
#[cfg(target_os = "linux")]
const CHILD_BARRIER_C: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <linux/limits.h>

static int resolve_path(const char *path, char *out, size_t cap) {
    char buf[PATH_MAX];
    if (realpath(path, buf)) {
        snprintf(out, cap, "%s", buf);
        return 1;
    }
    /* Target does not exist yet (e.g. a to-be-created .lock): resolve the
       literal path against the process cwd so relative aliases still match. */
    if (path[0] == '/') {
        snprintf(out, cap, "%s", path);
    } else if (getcwd(buf, sizeof(buf))) {
        snprintf(out, cap, "%s/%s", buf, path);
    } else {
        snprintf(out, cap, "%s", path);
    }
    return 1;
}

static int deny(const char *path) {
    if (!path || !*path) return 0;
    const char *env = getenv("RPI_ACTIVE_SESSION_FILES");
    if (!env || !*env) return 0;
    char resolved[PATH_MAX];
    if (!resolve_path(path, resolved, sizeof(resolved))) return 0;
    size_t want = strlen(resolved);
    const char *p = env;
    while (*p) {
        size_t n = strcspn(p, ":");
        if (n == want && !memcmp(p, resolved, n)) {
            errno = EPERM;
            return 1;
        }
        p += n;
        if (*p == ':') p++;
    }
    return 0;
}
static int write_flags(int flags) {
    return (flags & (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND)) != 0
        || (flags & O_ACCMODE) != O_RDONLY;
}
static int write_mode(const char *mode) {
    return mode && (strchr(mode, 'w') || strchr(mode, 'a') || strchr(mode, '+'));
}

int open(const char *path, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny(path)) return -1;
    if (flags & O_CREAT) return real(path, flags, m);
    return real(path, flags);
}
int open64(const char *path, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "open64");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny(path)) return -1;
    if (flags & O_CREAT) return real(path, flags, m);
    return real(path, flags);
}
static int deny_at(int dirfd, const char *path) {
    if (!path || !*path || path[0] == '/' || dirfd == AT_FDCWD) return deny(path);
    /* A relative name paired with a non-cwd dirfd resolves against that fd.
       Recover its path from /proc so an obfuscated `os.open(..., dir_fd=d)`
       cannot slip past the cwd-based comparison. */
    char fdpath[64], base[PATH_MAX], joined[PATH_MAX * 2];
    snprintf(fdpath, sizeof(fdpath), "/proc/self/fd/%d", dirfd);
    ssize_t n = readlink(fdpath, base, sizeof(base) - 1);
    if (n <= 0) return deny(path);
    base[n] = 0;
    snprintf(joined, sizeof(joined), "%s/%s", base, path);
    return deny(joined);
}

int openat(int dirfd, const char *path, int flags, ...) {
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny_at(dirfd, path)) return -1;
    if (flags & O_CREAT) return real(dirfd, path, flags, m);
    return real(dirfd, path, flags);
}
int openat64(int dirfd, const char *path, int flags, ...) {
    static int (*real)(int, const char *, int, ...);
    if (!real) real = dlsym(RTLD_NEXT, "openat64");
    mode_t m = 0;
    if (flags & O_CREAT) {
        va_list ap; va_start(ap, flags); m = va_arg(ap, mode_t); va_end(ap);
    }
    if (write_flags(flags) && deny_at(dirfd, path)) return -1;
    if (flags & O_CREAT) return real(dirfd, path, flags, m);
    return real(dirfd, path, flags);
}
int creat(const char *path, mode_t mode) {
    static int (*real)(const char *, mode_t);
    if (!real) real = dlsym(RTLD_NEXT, "creat");
    if (deny(path)) return -1;
    return real(path, mode);
}
FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen");
    if (write_mode(mode) && deny(path)) return NULL;
    return real(path, mode);
}
FILE *fopen64(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "fopen64");
    if (write_mode(mode) && deny(path)) return NULL;
    return real(path, mode);
}
int truncate(const char *path, off_t length) {
    static int (*real)(const char *, off_t);
    if (!real) real = dlsym(RTLD_NEXT, "truncate");
    if (deny(path)) return -1;
    return real(path, length);
}
int unlink(const char *path) {
    static int (*real)(const char *);
    if (!real) real = dlsym(RTLD_NEXT, "unlink");
    if (deny(path)) return -1;
    return real(path);
}
int rename(const char *oldpath, const char *newpath) {
    static int (*real)(const char *, const char *);
    if (!real) real = dlsym(RTLD_NEXT, "rename");
    if (deny(oldpath) || deny(newpath)) return -1;
    return real(oldpath, newpath);
}
"#;

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
/// to the real libc symbol via `dlsym(RTLD_NEXT, ...)`. See
/// [`CHILD_BARRIER_C`] for the source.

/// Materialize (once per process) the embedded shared object and return its
/// path. Failures are returned to the caller so `Shell::exec` refuses the
/// command instead of silently spawning an unprotected child. A host C
/// compiler is required on Linux only when an active session is installed.
#[cfg(target_os = "linux")]
fn child_barrier_so() -> Result<PathBuf, String> {
    static SO: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    SO.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("rpi-active-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("guard shim dir: {e}"))?;
        let c = dir.join("guard.c");
        let so = dir.join("libguard.so");
        std::fs::write(&c, CHILD_BARRIER_C).map_err(|e| format!("guard shim source: {e}"))?;
        let out = std::process::Command::new("cc")
            .arg("-shared")
            .arg("-fPIC")
            .arg("-O2")
            .arg("-o")
            .arg(&so)
            .arg(&c)
            .arg("-ldl")
            .output()
            .map_err(|e| format!("guard shim compiler: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "guard shim compile failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
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
