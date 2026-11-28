//! Read-only DLL snapshots and explicit-root, per-incarnation private staging.
//! Neither a snapshot digest nor a staged file grants native code authority.
use super::{
    ordinary::{BackendError, BackendResult},
    AgentId,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAX_NATIVE_EXTENSION_ARTIFACTS: usize = 8;
pub const MAX_NATIVE_EXTENSION_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_NATIVE_EXTENSION_TOTAL_BYTES: usize = 128 * 1024 * 1024;

struct Artifact {
    source: PathBuf,
    bytes: Vec<u8>,
    sha256: String,
}

/// Pure reads only: no dlopen, registration, worker, staging or discovery.
/// Bytes, order and canonical source provenance are immutable after freeze.
/// Approvals must independently match this provenance/digest before loading.
pub struct NativeExtensionArtifacts {
    artifacts: Vec<Artifact>,
    identity: String,
}
impl std::fmt::Debug for NativeExtensionArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeExtensionArtifacts { .. }")
    }
}
impl NativeExtensionArtifacts {
    pub fn freeze(files: &[PathBuf]) -> BackendResult<Self> {
        Self::freeze_bounded(
            files,
            MAX_NATIVE_EXTENSION_BYTES,
            MAX_NATIVE_EXTENSION_TOTAL_BYTES,
        )
    }
    fn freeze_bounded(files: &[PathBuf], max_file: u64, max_total: usize) -> BackendResult<Self> {
        if files.len() > MAX_NATIVE_EXTENSION_ARTIFACTS {
            return Err(BackendError::Binding("too many explicit DLL artifacts"));
        }
        let mut paths = BTreeSet::new();
        let mut identities = BTreeSet::new();
        let mut opened = Vec::with_capacity(files.len());
        // Validate the entire bounded set first; never silently skip a file.
        for path in files {
            if !path.is_absolute() || !fs::metadata(path)?.is_file() {
                return Err(BackendError::Binding(
                    "DLL artifact must be an absolute regular file",
                ));
            }
            let source = fs::canonicalize(path)?;
            if source.to_str().is_none() || !paths.insert(source.clone()) {
                return Err(BackendError::Binding("duplicate or non-UTF8 DLL artifact"));
            }
            // A replacement FIFO cannot block freeze, and a final symlink
            // replacement cannot redirect this canonical file's open.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&source)?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > max_file
                || !identities.insert((metadata.dev(), metadata.ino()))
            {
                return Err(BackendError::Binding(
                    "invalid, oversized or aliased DLL artifact",
                ));
            }
            opened.push((source, file));
        }
        let mut total = 0usize;
        let mut artifacts = Vec::with_capacity(opened.len());
        for (source, file) in opened {
            let remaining = max_total - total;
            let limit = max_file.min(remaining as u64);
            let mut bytes = Vec::new();
            file.take(limit + 1).read_to_end(&mut bytes)?;
            if bytes.is_empty() || bytes.len() as u64 > limit {
                return Err(BackendError::Binding("DLL artifact byte budget exceeded"));
            }
            total += bytes.len();
            let sha256 = format!("{:x}", Sha256::digest(&bytes));
            artifacts.push(Artifact {
                source,
                bytes,
                sha256,
            });
        }
        let provenance: Vec<_> = artifacts.iter().map(|artifact|
            serde_json::json!({"path":artifact.source,"sha256":artifact.sha256,"bytes":artifact.bytes.len()})
        ).collect();
        let identity = format!(
            "extensions-v1:{:x}",
            Sha256::digest(
                serde_json::to_vec(&provenance)
                    .map_err(|_| BackendError::Binding("invalid DLL artifact identity"))?
            )
        );
        Ok(Self {
            artifacts,
            identity,
        })
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn provenance(&self) -> Vec<(PathBuf, String)> {
        self.artifacts
            .iter()
            .map(|artifact| (artifact.source.clone(), artifact.sha256.clone()))
            .collect()
    }
    /// Owned assembly only. Never uses ambient TMPDIR or a shared module path.
    /// The existing root must be explicit, private and owned by this uid. Each
    /// invocation (including the same Agent after reload) creates new inodes.
    /// Move the returned Arc into the SDK host_guard, not a Harness hook: the
    /// directory then survives late cleanup and remains retained on SDK failure.
    pub fn stage(&self, root: &Path, agent: AgentId) -> BackendResult<Arc<NativeStagedExtensions>> {
        if !root.is_absolute() {
            return Err(BackendError::Binding("DLL staging root must be absolute"));
        }
        // Strip lexical trailing separators/dots before lstat: on Unix,
        // lstat("symlink/") otherwise follows the final symlink as a directory.
        let root: PathBuf = root.components().collect();
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.is_dir()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(BackendError::Binding(
                "DLL staging root must be a private owned directory",
            ));
        }
        let root = fs::canonicalize(&root)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("rpi-host-{agent}-"))
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?;
        let mut paths = Vec::with_capacity(self.artifacts.len());
        for (index, artifact) in self.artifacts.iter().enumerate() {
            let path = directory.path().join(format!(
                "artifact-{index}.{}",
                std::env::consts::DLL_EXTENSION
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o400)
                .open(&path)?;
            file.write_all(&artifact.bytes)?;
            file.sync_all()?;
            drop(file);
            paths.push(path);
        }
        // Not a persistent manifest or a crash-recovery acknowledgement. Only
        // these exact complete files can now be passed to the owned loader.
        Ok(Arc::new(NativeStagedExtensions {
            paths,
            _directory: directory,
        }))
    }
}

/// Only owns its newly created staging directory, never source/user data or the
/// Host owner itself. SDK guard retention cannot create an owner/session cycle.
pub struct NativeStagedExtensions {
    paths: Vec<PathBuf>,
    _directory: tempfile::TempDir,
}
impl NativeStagedExtensions {
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}
impl std::fmt::Debug for NativeStagedExtensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeStagedExtensions { .. }")
    }
}

#[cfg(test)]
mod tests;
