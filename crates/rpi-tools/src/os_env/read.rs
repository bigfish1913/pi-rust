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
        Ok(Self::from_shared(Arc::new(Mutex::new(file)), path))
    }
    /// Share the append handle itself, not a second open of its pathname.
    /// Reads and writes must hold this mutex for all cursor operations.
    pub(super) fn from_shared(file: Arc<Mutex<File>>, path: PathBuf) -> Self {
        Self { file, path }
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
    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        let shared = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let file = shared
                .lock()
                .map_err(|_| std::io::Error::other("reader mutex poisoned"))?;
            file.metadata().map(|metadata| Some(metadata.len()))
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&self.path.to_string_lossy())))
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
        assert_eq!(reader.byte_len().await.unwrap(), Some(14));
        let old = dir.path().join("old.jsonl");
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(14));
        assert_eq!(reader.read_all().await.unwrap(), b"old-generation");
        let (left, right) = tokio::join!(reader.read_range(0, 3), reader.read_range(4, 10));
        assert_eq!(left.unwrap(), b"old");
        assert_eq!(right.unwrap(), b"generation");
        assert!(reader.read_range(10, 100).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        // Length tracks the opened generation, not a cached opening length or
        // the path now naming another file.
        OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(3));
        assert_eq!(reader.read_all().await.unwrap(), b"old");
        assert!(reader.read_range(0, 4).await.is_err());
    }
}
