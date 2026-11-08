//! Append-only JSONL durable backend.
//!
//! This backend provides ordering and revision checks. Cooperating backend
//! instances serialize reads and appends through a sidecar file lock next to
//! the journal, so the head check and sequence allocation form one CAS
//! boundary. It intentionally does not claim fsync/power-loss atomicity;
//! callers that need that guarantee must add an explicit durability policy
//! around the backend.

use super::backend::{
    next_frame, next_frames, BackendError, DurableBackend, JournalFrame, JournalMutation,
};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct JsonlBackend {
    path: PathBuf,
}

impl JsonlBackend {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, BackendError> {
        let backend = Self { path: path.into() };
        if let Some(parent) = backend.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&backend.path)?;
        Ok(backend)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }

    fn open_lock(&self) -> Result<File, BackendError> {
        Ok(OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.lock_path())?)
    }

    fn read_frames_unlocked(&self) -> Result<Vec<JournalFrame>, BackendError> {
        let file = File::open(&self.path)?;
        let mut frames: Vec<JournalFrame> = Vec::new();
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let frame: JournalFrame = serde_json::from_str(&line)
                .map_err(|error| BackendError::Invalid(format!("line {}: {error}", line_no + 1)))?;
            frame.validate()?;
            if let Some(previous) = frames.last() {
                let expected = previous
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| BackendError::Invalid("journal sequence overflow".into()))?;
                if expected != frame.sequence {
                    return Err(BackendError::Invalid(
                        "journal sequence is not contiguous".into(),
                    ));
                }
            }
            frames.push(frame);
        }
        Ok(frames)
    }

    fn read_frames(&self) -> Result<Vec<JournalFrame>, BackendError> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        let result = self.read_frames_unlocked();
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(frames), Ok(())) => Ok(frames),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }
}

impl DurableBackend for JsonlBackend {
    fn load(&mut self) -> Result<Vec<JournalFrame>, BackendError> {
        self.read_frames()
    }

    fn append(
        &mut self,
        mutation: JournalMutation,
        expected_sequence: Option<u64>,
    ) -> Result<JournalFrame, BackendError> {
        // The lock covers the head read, CAS check, sequence allocation and
        // append. Without this, two processes can both observe the same head
        // and publish duplicate journal sequences.
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| {
            let current = self
                .read_frames_unlocked()?
                .last()
                .map(|frame| frame.sequence);
            let frame = next_frame(current, mutation, expected_sequence)?;

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            serde_json::to_writer(&mut file, &frame)?;
            file.write_all(b"\n")?;
            file.flush()?;
            Ok(frame)
        })();
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(frame), Ok(())) => Ok(frame),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    fn append_batch(
        &mut self,
        mutations: Vec<JournalMutation>,
        expected_sequence: Option<u64>,
    ) -> Result<Vec<JournalFrame>, BackendError> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| {
            let current = self
                .read_frames_unlocked()?
                .last()
                .map(|frame| frame.sequence);
            let frames = next_frames(current, mutations, expected_sequence)?;
            if frames.is_empty() {
                return Ok(frames);
            }

            // Encode the complete logical batch before opening the append
            // handle. This prevents validation/serialization failures from
            // publishing a prefix. It is still not a power-loss atomicity
            // guarantee.
            let mut encoded = Vec::new();
            for frame in &frames {
                serde_json::to_writer(&mut encoded, frame)?;
                encoded.push(b'\n');
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(&encoded)?;
            file.flush()?;
            Ok(frames)
        })();
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(frames), Ok(())) => Ok(frames),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    fn head(&mut self) -> Result<Option<u64>, BackendError> {
        Ok(self.read_frames()?.last().map(|frame| frame.sequence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mutation(sequence: u64) -> JournalMutation {
        JournalMutation {
            timestamp: sequence as i64,
            kind: "test".into(),
            payload: serde_json::json!({"sequence": sequence}),
        }
    }

    #[test]
    fn batch_validates_before_publishing_and_assigns_contiguous_sequences() {
        let dir = tempdir().unwrap();
        let mut backend = JsonlBackend::open(dir.path().join("journal.jsonl")).unwrap();
        let frames = backend
            .append_batch(vec![mutation(1), mutation(2)], None)
            .unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let invalid = JournalMutation {
            timestamp: 3,
            kind: "bad\nkind".into(),
            payload: serde_json::json!({}),
        };
        assert!(backend
            .append_batch(vec![mutation(3), invalid], Some(2))
            .is_err());
        assert_eq!(backend.head().unwrap(), Some(2));
    }

    #[test]
    fn appends_with_revision_cas_and_assigns_sequences() {
        let dir = tempdir().unwrap();
        let mut backend = JsonlBackend::open(dir.path().join("journal.jsonl")).unwrap();
        assert_eq!(backend.append(mutation(1), None).unwrap().sequence, 1);
        assert_eq!(backend.append(mutation(2), Some(1)).unwrap().sequence, 2);
        assert_eq!(backend.head().unwrap(), Some(2));
        assert!(matches!(
            backend.append(mutation(3), Some(0)),
            Err(BackendError::Conflict { .. })
        ));
    }

    #[test]
    fn concurrent_empty_writers_cas_without_duplicate_sequences() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let left = JsonlBackend::open(&path).unwrap();
        let right = JsonlBackend::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let left_barrier = std::sync::Arc::clone(&barrier);
        let left = std::thread::spawn(move || {
            let mut backend = left;
            left_barrier.wait();
            backend.append(mutation(1), None)
        });
        let right_barrier = std::sync::Arc::clone(&barrier);
        let right = std::thread::spawn(move || {
            let mut backend = right;
            right_barrier.wait();
            backend.append(mutation(2), None)
        });

        let results = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(BackendError::Conflict { .. })))
                .count(),
            1
        );
        let mut reopened = JsonlBackend::open(&path).unwrap();
        assert_eq!(
            reopened
                .load()
                .unwrap()
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }
}
