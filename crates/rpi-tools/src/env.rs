//! Mirrors `packages/agent/src/harness/types.ts::ExecutionEnv` — the trait every
//! tool and test-double implements. `ExecutionEnv = FileSystem + Shell`.
//!
//! Contract (plan §5.13): **every method must never panic.** All failures are
//! encoded in `Result<_, FileError>` / `Result<_, ExecutionError>`. Paths may be
//! absolute or relative to [`FileSystem::cwd`]; returned paths are "addressed"
//! (symlink-canonicalized only by [`FileSystem::canonical_path`]).
//!
//! The TS `AbortSignal` becomes `&tokio_util::sync::CancellationToken` on every
//! method. Backends SHOULD check `is_cancelled()` at entry and honor cooperative
//! cancellation during blocking ops via `tokio::select!`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutionError, FileError};

/// `file` | `directory` | `symlink`. Mirrors TS `FileKind`.
///
/// Backends detect via `symlink_metadata` (lstat) — symlinks are NOT followed
/// automatically by any `FileSystem` method except `canonical_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Mirrors TS `FileInfo`. `path` is the absolute addressed (resolved) path —
/// NOT symlink-canonicalized. `mtime_ms` is ms since Unix epoch.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
}

/// Write/append payload. Mirrors the TS `string | Uint8Array` union.
#[derive(Debug, Clone)]
pub enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl FileContent {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            FileContent::Text(s) => s.as_bytes(),
            FileContent::Bytes(b) => b,
        }
    }
}

impl From<String> for FileContent {
    fn from(s: String) -> Self {
        FileContent::Text(s)
    }
}

impl From<&str> for FileContent {
    fn from(s: &str) -> Self {
        FileContent::Text(s.to_string())
    }
}

impl From<Vec<u8>> for FileContent {
    fn from(b: Vec<u8>) -> Self {
        FileContent::Bytes(b)
    }
}

/// Result of `Shell::exec`. Mirrors the TS inline return type
/// `{ stdout, stderr, exitCode }`. Signal kills collapse to `exit_code: 0`
/// (matching the TS `code ?? 0` rule).
#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Mirrors TS `ShellExecOptions` (note: `timeout` is in **seconds**, matching
/// the TS contract). `on_stdout` / `on_stderr` fire per chunk, interleaved in
/// arrival order; the bash-tool wraps these with throttling + capture.
pub struct ShellExecOptions<'a> {
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    /// Default `true`. When `false`, ONLY `env` is used (no process env, no
    /// shell base env).
    pub inherit_env: bool,
    /// Seconds. `None` = no timeout.
    pub timeout: Option<f64>,
    pub cancel: Option<&'a CancellationToken>,
    pub on_stdout: Option<Box<dyn FnMut(&str) + Send + 'a>>,
    pub on_stderr: Option<Box<dyn FnMut(&str) + Send + 'a>>,
}

impl<'a> Default for ShellExecOptions<'a> {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            inherit_env: true,
            timeout: None,
            cancel: None,
            on_stdout: None,
            on_stderr: None,
        }
    }
}

impl<'a> ShellExecOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Mirrors TS `FileSystem`. All methods async, never panic.
///
/// `cancel: Option<&CancellationToken>` is the Rust analogue of the optional
/// `abortSignal`; backends SHOULD check it at entry (returning
/// `FileError::aborted()`) and honor cooperative cancel during long ops.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// The environment's working directory (absolute).
    fn cwd(&self) -> &Path;

    /// Resolve `path` (relative-to-cwd, `~`, `file://`, absolute) to an absolute
    /// addressed path. Pure — no FS access, no abort. Mirrors TS `absolutePath`.
    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Join path components. Pure. Mirrors TS `joinPath`.
    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError>;

    /// Stream-read up to `max_lines` lines (split on `\n`; `max_lines <= 0`
    /// → empty). Mirrors TS `readTextLines{maxLines}`.
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError>;

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError>;

    /// Creates parent directories (recursive) automatically. Mirrors TS
    /// `writeFile`.
    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Uses `symlink_metadata` (lstat — symlinks not followed). Mirrors TS
    /// `fileInfo`.
    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError>;

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError>;

    /// Follows symlinks (`canonicalize`). Mirrors TS `canonicalPath`. Backends
    /// may return `not_found` or `not_supported` for missing/unsupported paths;
    /// callers (mutation queue) fall back to absolute path on those codes.
    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// `false` ONLY for `not_found`; other errors propagate. Mirrors TS
    /// `exists` (which delegates to `fileInfo`, NOT `access(F_OK)`).
    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError>;

    /// Default `recursive=true`. Mirrors TS `createDir{recursive}`.
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Defaults `recursive=false`, `force=false`. Mirrors TS `remove`.
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Default prefix `"tmp-"`. Mirrors TS `createTempDir{prefix}`.
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Mirrors TS `createTempFile{prefix,suffix}` (defaults empty). Note: the
    /// TS impl creates an isolated temp **dir** per file; backends may simplify
    /// to a single file in the OS temp dir (document any divergence).
    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Best-effort, never returns `Err`. Mirrors TS `cleanup` (kills tracked
    /// child processes, etc.).
    async fn cleanup(&self);
}

/// Mirrors TS `Shell`.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Execute `command` in a bash shell. The TS env resolves a bash binary
    /// (git-bash on Windows, `/bin/bash` on POSIX, `/bin/sh` fallback on POSIX).
    /// There is NO cmd.exe fallback on Windows — failure to find bash is
    /// `ExecutionError::ShellUnavailable`.
    async fn exec<'a>(
        &'a self,
        command: &str,
        options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError>;

    /// Best-effort, never returns `Err`. Mirrors TS `Shell.cleanup`.
    async fn cleanup(&self);
}

/// Mirrors TS `ExecutionEnv extends FileSystem, Shell`. A marker combining both
/// capabilities — tools receive `Arc<dyn ExecutionEnv>` (or a concrete env) and
/// use `FileSystem` + `Shell` methods interchangeably.
#[async_trait]
pub trait ExecutionEnv: FileSystem + Shell {}

/// Check `cancel` at a method entry point, returning `FileError::aborted()`
/// when set. Mirrors the TS `abortResult<T>(signal, path?)` helper used at the
/// top of every `FileSystem` method.
pub fn check_cancel_file(
    cancel: Option<&CancellationToken>,
    path: Option<&str>,
) -> Result<(), FileError> {
    if let Some(t) = cancel {
        if t.is_cancelled() {
            let mut err = FileError::aborted();
            if let Some(p) = path {
                err = err.with_path(p);
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Same as [`check_cancel_file`] but for `Shell`-side (returns
/// `ExecutionError::aborted()`).
pub fn check_cancel_exec(cancel: Option<&CancellationToken>) -> Result<(), ExecutionError> {
    if let Some(t) = cancel {
        if t.is_cancelled() {
            return Err(ExecutionError::aborted());
        }
    }
    Ok(())
}
