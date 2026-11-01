//! A file-generation reader; unlike append handles it never follows a rename.
use crate::{
    env::ReadFile,
    error::{io_to_file_error, FileError, FileErrorCode},
};
use async_trait::async_trait;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(super) struct OsReadFile {
    file: Arc<Mutex<File>>,
    path: PathBuf,
}
impl OsReadFile {
    pub(super) async fn open(path: PathBuf) -> Result<Self, FileError> {
        let addressed = path.clone();
        let file = tokio::task::spawn_blocking(move || {
            // std's Windows default shares read/write/delete, permitting the
            // journal's existing atomic replacement while readers keep a view.
            OpenOptions::new().read(true).open(addressed)
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&path.to_string_lossy())))?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            path,
        })
    }
    async fn read(&self, range: Option<(u64, usize)>) -> Result<Vec<u8>, FileError> {
        let shared = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = shared
                .lock()
                .map_err(|_| std::io::Error::other("reader mutex poisoned"))?;
            let mut bytes = Vec::new();
            match range {
                Some((offset, length)) => {
                    file.seek(SeekFrom::Start(offset))?;
                    bytes.resize(length, 0);
                    file.read_exact(&mut bytes)?;
                }
                None => {
                    file.seek(SeekFrom::Start(0))?;
                    file.read_to_end(&mut bytes)?;
                }
            }
            Ok(bytes)
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&self.path.to_string_lossy())))
    }
}
fn join_error(error: tokio::task::JoinError) -> FileError {
    FileError::new(FileErrorCode::Unknown, error.to_string()).with_source(Box::new(error))
}
#[async_trait]
impl ReadFile for OsReadFile {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        self.read(None).await
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.read(Some((offset, length))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_handle_keeps_its_generation_after_native_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        std::fs::write(&path, b"old-generation").unwrap();
        let reader = OsReadFile::open(path.clone()).await.unwrap();
        let old = dir.path().join("old.jsonl");
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, b"new-generation").unwrap();
        assert_eq!(reader.read_all().await.unwrap(), b"old-generation");
        let (left, right) = tokio::join!(reader.read_range(0, 3), reader.read_range(4, 10));
        assert_eq!(left.unwrap(), b"old");
        assert_eq!(right.unwrap(), b"generation");
        assert!(reader.read_range(10, 100).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new-generation");
    }
}
