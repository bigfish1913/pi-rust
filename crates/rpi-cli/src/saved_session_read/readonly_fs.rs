//! Restricted native-owner filesystem: exactly two pinned regular inputs, no
//! writer, creation, directory enumeration, settings, environment or shell.
use async_trait::async_trait;
use rpi_tools::{
    env::ReadFile, FileContent, FileError, FileErrorCode, FileInfo, FileKind, FileSystem,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
const READ_LIMIT: usize = 64 * 1024 * 1024;
struct Budget {
    remaining: usize,
    deadline: Instant,
    hashes: BTreeMap<PathBuf, [u8; 32]>,
}
fn failure(message: impl Into<String>) -> FileError {
    FileError::new(FileErrorCode::Invalid, message.into())
}
fn unsupported() -> FileError {
    FileError::new(
        FileErrorCode::NotSupported,
        "saved reader has no write/directory capability",
    )
}
fn io(error: std::io::Error) -> FileError {
    rpi_tools::io_to_file_error(error, None)
}
struct Input {
    file: Mutex<std::fs::File>,
    path: PathBuf,
    length: usize,
    budget: Arc<Mutex<Budget>>,
}
impl Input {
    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > self.length as u64)
        {
            return Err(failure("saved read invalid source range"));
        }
        {
            let mut budget = self.budget.lock().expect("saved read budget");
            if Instant::now() >= budget.deadline {
                return Err(failure("saved read processing deadline exceeded"));
            }
            budget.remaining = budget
                .remaining
                .checked_sub(length)
                .ok_or_else(|| failure("saved read total byte limit exceeded"))?;
        }
        let mut file = self.file.lock().expect("saved input");
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut bytes = vec![0; length];
        for chunk in bytes.chunks_mut(64 * 1024) {
            if Instant::now() >= self.budget.lock().expect("saved read budget").deadline {
                return Err(failure("saved read processing deadline exceeded"));
            }
            file.read_exact(chunk).map_err(io)?;
        }
        Ok(bytes)
    }
}
#[async_trait]
impl ReadFile for Input {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        let bytes = self.read(0, self.length)?;
        self.budget
            .lock()
            .expect("saved read budget")
            .hashes
            .insert(self.path.clone(), Sha256::digest(&bytes).into());
        Ok(bytes)
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.read(offset, length)
    }
}
pub(super) struct ReadonlyFs {
    cwd: PathBuf,
    inputs: BTreeMap<PathBuf, Arc<Input>>,
    budget: Arc<Mutex<Budget>>,
}
impl ReadonlyFs {
    pub(super) fn new(history: &Path, deadline: Instant) -> Result<Self, String> {
        let root = history
            .parent()
            .ok_or("saved session has no parent directory")?
            .to_owned();
        let progress = PathBuf::from(format!("{}.progress.jsonl", history.display()));
        let budget = Arc::new(Mutex::new(Budget {
            remaining: READ_LIMIT,
            deadline,
            hashes: BTreeMap::new(),
        }));
        let mut inputs = BTreeMap::new();
        for path in [history.to_owned(), progress] {
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && path != history => continue,
                Err(e) => return Err(e.to_string()),
            };
            if !meta.file_type().is_file()
                || std::fs::canonicalize(&path).map_err(|e| e.to_string())? != path
            {
                return Err("saved reader refuses aliased/nonregular companion input".into());
            }
            let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let length = file.metadata().map_err(|e| e.to_string())?.len();
            if length > READ_LIMIT as u64 {
                return Err("saved read total byte limit exceeded".into());
            }
            inputs.insert(
                path.clone(),
                Arc::new(Input {
                    file: Mutex::new(file),
                    path,
                    length: length as usize,
                    budget: budget.clone(),
                }),
            );
        }
        Ok(Self {
            cwd: root,
            inputs,
            budget,
        })
    }
    pub(super) fn fingerprint(&self) -> String {
        let budget = self.budget.lock().expect("saved read budget");
        let mut hash = Sha256::new();
        for (path, digest) in &budget.hashes {
            let path = path.to_string_lossy();
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(digest);
        }
        format!("{:x}", hash.finalize())
    }
    fn input(&self, path: &str) -> Result<&Arc<Input>, FileError> {
        self.inputs
            .get(Path::new(path))
            .ok_or_else(|| failure("saved reader input is missing or outside its fixed sources"))
    }
}
#[async_trait]
impl FileSystem for ReadonlyFs {
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    async fn absolute_path(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(unsupported())
        }
    }
    async fn join_path(
        &self,
        _: &[&str],
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn read_text_file(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        String::from_utf8(self.input(path)?.read_all().await?)
            .map_err(|_| failure("invalid saved input UTF-8"))
    }
    async fn read_text_lines(
        &self,
        _: &str,
        _: Option<usize>,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        Err(unsupported())
    }
    async fn read_binary_file(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.input(path)?.read_all().await
    }
    async fn open_read_file(&self, path: &str) -> Result<Option<Arc<dyn ReadFile>>, FileError> {
        Ok(Some(self.input(path)?.clone()))
    }
    async fn write_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn append_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn rename_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn file_info(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        let input = self.input(path)?;
        let meta = input
            .file
            .lock()
            .expect("saved input")
            .metadata()
            .map_err(io)?;
        Ok(FileInfo {
            name: input
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path: input.path.clone(),
            kind: FileKind::File,
            size: meta.len(),
            mtime_ms: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(0),
        })
    }
    async fn list_dir(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        Err(unsupported())
    }
    async fn canonical_path(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Ok(self.input(path)?.path.clone())
    }
    async fn exists(&self, path: &str, _: Option<&CancellationToken>) -> Result<bool, FileError> {
        Ok(self.inputs.contains_key(Path::new(path)))
    }
    async fn create_dir(
        &self,
        _: &str,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn remove(
        &self,
        _: &str,
        _: bool,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn create_temp_dir(
        &self,
        _: Option<&str>,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn create_temp_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn cleanup(&self) {}
}
