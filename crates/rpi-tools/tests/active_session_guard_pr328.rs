//! PR A regression probes: active-session file write guard.
//!
//! Five discriminating invariants, named for the semantic axes they pin:
//!  1. `bash_child_write_to_active_session_is_refused`
//!  2. `ordinary_jsonl_and_export_targets_remain_writable`
//!  3. `no_session_does_not_block_any_write`
//!  4. `relative_path_and_symlink_aliases_are_refused`
//!  5. `spawn_after_obfuscated_child_write_is_refused` (OS-level barrier)
//!
//! They use only the public `OsExecutionEnv` + `ExecutionEnv` API. On the
//! unfixed head a temporary no-op shim keeps them compiling so SKILL §2's
//! fail-on-old is an **assertion** failure, not `E0425/E0599`.
//!
//! The `LD_PRELOAD` shim and symlink probes are POSIX-only; that matches the
//! Linux OS barrier. The tool-layer checks and the no-overblocking probes stay
//! cross-platform so Windows `--all-targets` still compiles.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use rpi_tools::env::{FileContent, ShellExecOptions};
use rpi_tools::{FileSystem, OsExecutionEnv, Shell};

/// Integration tests share one process; the guard is process-global.
static PROBE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn probe_lock() -> std::sync::MutexGuard<'static, ()> {
    PROBE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn absolute(p: &Path) -> String {
    p.canonicalize()
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn install_active(jsonl: Option<&Path>) {
    let path = jsonl.map(absolute);
    // Fixed head: process-wide protection. Unfixed head: `#[cfg(test)]` no-op
    // shim, so every "must be refused" assertion below fails at runtime.
    rpi_tools::active_session_guard::activate_session_guard(path.as_deref());
}

fn clear() {
    rpi_tools::active_session_guard::activate_session_guard(None);
}

fn dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(name).tempdir().unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn bash_child_write_to_active_session_is_refused() {
    let _g = probe_lock();
    let tmp = dir("pr328-bash-");
    let session = tmp.path().join("active.jsonl");
    let progress = tmp.path().join("active.jsonl.progress.jsonl");
    let lock = tmp.path().join("active.jsonl.lock");
    for f in [&session, &progress, &lock] {
        std::fs::write(f, b"original\n").unwrap();
    }
    install_active(Some(&session));

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    for (label, file) in [
        ("jsonl", &session),
        ("progress", &progress),
        ("lock", &lock),
    ] {
        let before = std::fs::read(file).unwrap();
        let cmd = format!(
            "python3 -c \"open('{}','wb').write(b'CORRUPT')\"",
            absolute(file)
        );
        let result = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
        assert!(
            result.is_err(),
            "{label}: bash child rewrite of an active session file must be refused"
        );
        assert_eq!(
            before,
            std::fs::read(file).unwrap(),
            "{label}: refused command must not touch the file"
        );
    }

    // Built-in write/edit and any extension sharing this env go through the
    // same `FileSystem` entry point: refuse with a clear, actionable error.
    for (label, file) in [
        ("jsonl", &session),
        ("progress", &progress),
        ("lock", &lock),
    ] {
        let before = std::fs::read(file).unwrap();
        let result = rt.block_on(async {
            env.write_file(
                &absolute(file),
                FileContent::Bytes(b"CORRUPT".to_vec()),
                None,
            )
            .await
        });
        assert!(
            result.is_err(),
            "{label}: write_file must refuse the active session file"
        );
        assert_eq!(
            before,
            std::fs::read(file).unwrap(),
            "{label}: no bytes changed"
        );
        let err = result.err().unwrap();
        assert_eq!(
            err.code,
            rpi_tools::error::FileErrorCode::PermissionDenied,
            "{label}: actionable denial"
        );
        assert!(
            err.message.contains("active session"),
            "{label}: message explains the guard"
        );
    }
    clear();
}

/// A command whose text contains no write-intent token at all, so only the
/// child-process `LD_PRELOAD` barrier can stop it. Pins the requirement that
/// protection survives the spawn (`python3 -c "open(...,'wb')"` obfuscated as
/// `getattr(os, 'op'+'en')`). A `dir_fd` relative variant is included.
#[cfg(target_os = "linux")]
#[test]
fn spawn_after_obfuscated_child_write_is_refused() {
    let _g = probe_lock();
    let tmp = dir("pr328-spawn-");
    let session = tmp.path().join("active.jsonl");
    std::fs::write(&session, b"header\n").unwrap();
    install_active(Some(&session));

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    let abs = absolute(&session);
    let cwd = absolute(tmp.path());
    for (label, cmd) in [
        (
            "absolute",
            format!(
                "python3 -c \"import os; fd=getattr(os,'op'+'en')('{abs}', os.O_WRONLY|os.O_TRUNC)\""
            ),
        ),
        (
            "dir-fd-relative",
            format!(
                "python3 -c \"import os; d=getattr(os,'op'+'en')('{cwd}', os.O_RDONLY|os.O_DIRECTORY); fd=getattr(os,'op'+'en')('active.jsonl', os.O_WRONLY|os.O_TRUNC, dir_fd=d)\""
            ),
        ),
    ] {
        assert!(
            !cmd.contains("open(") && !cmd.contains("wb") && !cmd.contains('>'),
            "{label}: probe must not be caught by the textual pre-check"
        );
        let out = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
        assert!(
            out.is_err() || out.as_ref().is_ok_and(|o| o.exit_code != 0),
            "{label}: child must fail to write: {out:?}"
        );
        assert_eq!(
            b"header\n".to_vec(),
            std::fs::read(&session).unwrap(),
            "{label}: active session file unchanged"
        );
    }
    clear();
}

#[test]
fn ordinary_jsonl_and_export_targets_remain_writable() {
    let _g = probe_lock();
    let tmp = dir("pr328-ordinary-");
    let session = tmp.path().join("active.jsonl");
    std::fs::write(&session, b"header\n").unwrap();
    install_active(Some(&session));

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    let other = tmp.path().join("other.jsonl");
    let export = tmp.path().join("export.jsonl");
    let export_md = tmp.path().join("export.md");
    for target in [&other, &export, &export_md] {
        let result = rt.block_on(async {
            env.write_file(&absolute(target), FileContent::Bytes(b"OK".to_vec()), None)
                .await
        });
        assert!(
            result.is_ok(),
            "{}: unrelated target must stay writable: {result:?}",
            target.display()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"OK");
    }
    // Reads of the active session file are never blocked by the guard.
    let read = rt.block_on(async { env.read_binary_file(&absolute(&session), None).await });
    assert_eq!(read.unwrap(), b"header\n");
    // A read-only bash command with a `>`-like token (`2>&1`) must not be
    // mistaken for write intent. The OS child barrier also allows reads.
    let cmd = format!(
        "python3 -c \"open('{}','rb').read()\" 2>&1",
        absolute(&session)
    );
    let out = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
    assert!(
        out.is_ok(),
        "read-only bash command must not be blocked: {out:?}"
    );
    // A bash child writing an unrelated (not active) file is likewise allowed.
    let cmd = format!(
        "python3 -c \"open('{}','wb').write(b'CHILD')\"",
        absolute(&other)
    );
    let out = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
    assert!(
        out.is_ok(),
        "bash child writing an ordinary .jsonl must be allowed: {out:?}"
    );
    assert_eq!(std::fs::read(&other).unwrap(), b"CHILD");
    assert_eq!(std::fs::read(&session).unwrap(), b"header\n");
    clear();
}

#[test]
fn no_session_does_not_block_any_write() {
    let _g = probe_lock();
    let tmp = dir("pr328-nosession-");
    let candidate = tmp.path().join("no-active-session.jsonl");
    std::fs::write(&candidate, b"header\n").unwrap();
    install_active(None);

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    let result = rt.block_on(async {
        env.write_file(
            &absolute(&candidate),
            FileContent::Bytes(b"OK".to_vec()),
            None,
        )
        .await
    });
    assert!(
        result.is_ok(),
        "--no-session must not block any write: {result:?}"
    );
    assert_eq!(std::fs::read(&candidate).unwrap(), b"OK");
    let cmd = format!(
        "python3 -c \"open('{}','wb').write(b'CHILD')\"",
        absolute(&candidate)
    );
    let out = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
    assert!(
        out.is_ok(),
        "--no-session bash child must be allowed: {out:?}"
    );
    assert_eq!(std::fs::read(&candidate).unwrap(), b"CHILD");
}

#[test]
fn relative_path_and_dotdot_aliases_are_refused() {
    let _g = probe_lock();
    let tmp = dir("pr328-alias-");
    let nested = tmp.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let session = tmp.path().join("active.jsonl");
    std::fs::write(&session, b"header\n").unwrap();
    install_active(Some(&session));

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    let before = std::fs::read(&session).unwrap();
    for (label, supplied) in [
        ("relative", "active.jsonl".to_string()),
        ("dot-relative", "./nested/../active.jsonl".to_string()),
    ] {
        let result = rt.block_on(async {
            env.write_file(&supplied, FileContent::Bytes(b"CORRUPT".to_vec()), None)
                .await
        });
        assert!(
            result.is_err(),
            "{label}: alias to the active session file must be refused"
        );
        assert_eq!(
            before,
            std::fs::read(&session).unwrap(),
            "{label}: target unchanged"
        );
    }
    clear();
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_aliases_are_refused() {
    let _g = probe_lock();
    let tmp = dir("pr328-symlink-");
    let nested = tmp.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let session = tmp.path().join("active.jsonl");
    std::fs::write(&session, b"header\n").unwrap();
    install_active(Some(&session));

    let alias = tmp.path().join("alias.jsonl");
    std::os::unix::fs::symlink(&session, &alias).unwrap();
    let nested_alias = nested.join("up.jsonl");
    std::os::unix::fs::symlink(Path::new("../active.jsonl"), &nested_alias).unwrap();

    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let rt = runtime();
    let before = std::fs::read(&session).unwrap();
    for (label, supplied) in [
        ("symlink", "alias.jsonl".to_string()),
        ("nested-symlink", "nested/up.jsonl".to_string()),
    ] {
        let result = rt.block_on(async {
            env.write_file(&supplied, FileContent::Bytes(b"CORRUPT".to_vec()), None)
                .await
        });
        assert!(
            result.is_err(),
            "{label}: alias to the active session file must be refused"
        );
        assert_eq!(
            before,
            std::fs::read(&session).unwrap(),
            "{label}: target unchanged"
        );
    }
    // The same aliases passed through a bash child are refused too.
    for (label, supplied) in [
        ("relative", "active.jsonl".to_string()),
        ("symlink", "alias.jsonl".to_string()),
    ] {
        let cmd = format!("python3 -c \"open('{supplied}','wb').write(b'CORRUPT')\"");
        let result = rt.block_on(async { env.exec(&cmd, ShellExecOptions::default()).await });
        assert!(
            result.is_err(),
            "{label}: bash alias to active session must be refused"
        );
        assert_eq!(
            before,
            std::fs::read(&session).unwrap(),
            "{label}: target unchanged"
        );
    }
    clear();
}
