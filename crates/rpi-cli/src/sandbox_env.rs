//! Sandbox-aware execution environment.
//!
//! Wraps [`OsExecutionEnv`] so a write that the kernel denies can be approved
//! interactively and then executed by the unconfined broker. Reads, listing and
//! shell execution delegate unchanged: the kernel already allows them.
//!
//! Without an approval frontend (headless runs, `mode = "required"` before the
//! TUI starts, or `approval.enabled = false`) the original permission error is
//! returned, so behavior stays "deny by default".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_sandbox::{ApprovalDecision, SandboxPolicy, WriteBroker, WriteOp};
use rpi_tools::env::{AppendFile, ReadFile, WriterLease};
use rpi_tools::{
    ExecutionEnv, FileContent, FileError, FileErrorCode, FileInfo, FileSystem, MutatingEnv,
    OsExecutionEnv, Shell, ShellExecOptions, ShellOutput,
};
use tokio_util::sync::CancellationToken;

/// Wrapper that adds broker-backed approvals on top of the OS environment.
pub struct SandboxedEnv {
    inner: Arc<dyn ExecutionEnv>,
    inner_mut: Arc<dyn MutatingEnv>,
    policy: Arc<SandboxPolicy>,
    broker: Arc<dyn WriteBroker>,
}

/// Wrap `base` for sandboxed tool execution.
pub fn wrap(
    base: Arc<OsExecutionEnv>,
    policy: Arc<SandboxPolicy>,
    broker: Arc<dyn WriteBroker>,
) -> (Arc<dyn ExecutionEnv>, Arc<dyn MutatingEnv>) {
    let env: Arc<SandboxedEnv> = Arc::new(SandboxedEnv {
        inner: base.clone(),
        inner_mut: base,
        policy,
        broker,
    });
    let exec: Arc<dyn ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn MutatingEnv> = env;
    (exec, mut_env)
}

/// Build the tool environment for a session, wrapping the OS environment when
/// an enforcement backend and broker are active.
pub fn for_session(base: Arc<OsExecutionEnv>) -> (Arc<dyn ExecutionEnv>, Arc<dyn MutatingEnv>) {
    #[cfg(unix)]
    let broker = crate::sandbox::broker_client()
        .map(|client| Arc::new(rpi_sandbox::broker::SharedBroker(client)) as Arc<dyn WriteBroker>);
    #[cfg(not(unix))]
    let broker: Option<Arc<dyn WriteBroker>> = None;

    match (broker, crate::sandbox::active_policy()) {
        (Some(broker), Some(policy)) => wrap(base, Arc::new(policy.clone()), broker),
        _ => {
            let exec: Arc<dyn ExecutionEnv> = base.clone();
            let mut_env: Arc<dyn MutatingEnv> = base;
            (exec, mut_env)
        }
    }
}

impl SandboxedEnv {
    fn denied(&self, raw: &str, detail: &str) -> FileError {
        FileError::new(
            FileErrorCode::PermissionDenied,
            format!(
                "{detail} (path: {raw}). Approve the write interactively or add it to sandbox.writable."
            ),
        )
        .with_path(raw)
    }

    /// Resolve `raw` and report whether the kernel already allows the write.
    async fn resolve(&self, raw: &str) -> Result<(PathBuf, bool), FileError> {
        let abs = self.inner.absolute_path(raw, None).await?;
        let writable = self.policy.is_writable(&abs);
        Ok((abs, !writable))
    }

    /// Ask for approval for `abs` (already resolved) and record it with the
    /// broker. Returns an error when the user denies or no frontend exists.
    async fn authorize(
        &self,
        raw: &str,
        abs: &Path,
        operations: &[WriteOp],
        bytes: Option<u64>,
    ) -> Result<(), FileError> {
        let operation = operations.first().copied().unwrap_or(WriteOp::Overwrite);
        // An existing session/directory approval short-circuits the prompt.
        if self.broker.is_approved(abs, operation) {
            return Ok(());
        }
        let Some(decision) = crate::sandbox_approval::ask(operation, abs, raw, bytes).await else {
            return Err(self.denied(
                raw,
                "the filesystem sandbox blocked this write and no approval prompt is available",
            ));
        };
        match decision {
            ApprovalDecision::Allow { scope, recursive } => {
                // A directory approval is recorded on the containing directory
                // so siblings are covered too.
                let approve_path = if recursive {
                    abs.parent().unwrap_or(abs).to_path_buf()
                } else {
                    abs.to_path_buf()
                };
                self.broker
                    .approve(&approve_path, operations, scope, recursive)
                    .map_err(|error| {
                        self.denied(raw, &format!("approval could not be recorded: {error}"))
                    })
            }
            ApprovalDecision::Deny => Err(self.denied(raw, "the user denied this write")),
        }
    }

    fn broker_error(&self, raw: &str, error: String) -> FileError {
        self.denied(raw, &error)
    }
}

#[async_trait]
impl FileSystem for SandboxedEnv {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }

    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(path, cancel).await
    }

    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.join_path(parts, cancel).await
    }

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        self.inner.read_text_file(path, cancel).await
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        self.inner.read_text_lines(path, max_lines, cancel).await
    }

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path, cancel).await
    }

    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (abs, needs_approval) = self.resolve(path).await?;
        if !needs_approval {
            return self.inner.write_file(path, content, cancel).await;
        }
        self.authorize(
            path,
            &abs,
            &[WriteOp::Create, WriteOp::Overwrite],
            Some(content.as_bytes().len() as u64),
        )
        .await?;
        self.broker
            .write(&abs, content.as_bytes())
            .map_err(|error| self.broker_error(path, error))
    }

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (abs, needs_approval) = self.resolve(path).await?;
        if !needs_approval {
            return self.inner.append_file(path, content, cancel).await;
        }
        self.authorize(
            path,
            &abs,
            &[WriteOp::Append, WriteOp::Create],
            Some(content.as_bytes().len() as u64),
        )
        .await?;
        self.broker
            .append(&abs, content.as_bytes())
            .map_err(|error| self.broker_error(path, error))
    }

    async fn open_append_file(&self, path: &str) -> Result<Option<Box<dyn AppendFile>>, FileError> {
        if self
            .policy
            .is_writable(&self.inner.absolute_path(path, None).await?)
        {
            return self.inner.open_append_file(path).await;
        }
        // Out-of-policy journals fall back to the per-call append path, which
        // goes through the approval/broker flow.
        Ok(None)
    }

    async fn try_acquire_writer_lease(
        &self,
        path: &str,
    ) -> Result<Option<Box<dyn WriterLease>>, FileError> {
        self.inner.try_acquire_writer_lease(path).await
    }

    async fn sync_file(
        &self,
        path: &str,
        data_only: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.sync_file(path, data_only, cancel).await
    }

    async fn sync_directory(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.sync_directory(path, cancel).await
    }

    async fn open_read_file(&self, path: &str) -> Result<Option<Arc<dyn ReadFile>>, FileError> {
        self.inner.open_read_file(path).await
    }

    async fn truncate_file(
        &self,
        path: &str,
        len: u64,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (abs, needs_approval) = self.resolve(path).await?;
        if !needs_approval {
            return self.inner.truncate_file(path, len, cancel).await;
        }
        self.authorize(path, &abs, &[WriteOp::Overwrite], None)
            .await?;
        self.broker
            .truncate(&abs, len)
            .map_err(|error| self.broker_error(path, error))
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (source_abs, source_needs) = self.resolve(source).await?;
        let (dest_abs, dest_needs) = self.resolve(dest).await?;
        if !source_needs && !dest_needs {
            return self.inner.rename_file(source, dest, cancel).await;
        }
        if source_needs {
            self.authorize(source, &source_abs, &[WriteOp::Rename], None)
                .await?;
        }
        if dest_needs {
            self.authorize(dest, &dest_abs, &[WriteOp::Create], None)
                .await?;
        }
        self.broker
            .rename(&source_abs, &dest_abs)
            .map_err(|error| self.broker_error(source, error))
    }

    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        self.inner.file_info(path, cancel).await
    }

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path, cancel).await
    }

    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.canonical_path(path, cancel).await
    }

    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError> {
        self.inner.exists(path, cancel).await
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (abs, needs_approval) = self.resolve(path).await?;
        if !needs_approval {
            return self.inner.create_dir(path, recursive, cancel).await;
        }
        self.authorize(path, &abs, &[WriteOp::Mkdir], None).await?;
        self.broker
            .mkdir(&abs)
            .map_err(|error| self.broker_error(path, error))
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let (abs, needs_approval) = self.resolve(path).await?;
        if !needs_approval {
            return self.inner.remove(path, recursive, force, cancel).await;
        }
        self.authorize(path, &abs, &[WriteOp::Delete], None).await?;
        self.broker
            .delete(&abs)
            .map_err(|error| self.broker_error(path, error))
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(prefix, cancel).await
    }

    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_file(prefix, suffix, cancel).await
    }

    async fn cleanup(&self) {
        FileSystem::cleanup(&*self.inner).await;
    }
}

#[async_trait]
impl Shell for SandboxedEnv {
    async fn exec<'a>(
        &'a self,
        command: &str,
        options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, rpi_tools::ExecutionError> {
        self.inner.exec(command, options).await
    }

    async fn cleanup(&self) {
        Shell::cleanup(&*self.inner).await;
    }
}

#[async_trait]
impl ExecutionEnv for SandboxedEnv {}

impl MutatingEnv for SandboxedEnv {
    fn as_env(&self) -> &dyn ExecutionEnv {
        self
    }

    fn mutation_registry(&self) -> &rpi_tools::MutationQueueRegistry {
        self.inner_mut.mutation_registry()
    }
}
