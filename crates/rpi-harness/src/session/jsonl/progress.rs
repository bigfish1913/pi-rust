//! A checkpoint plus incremental frame payloads; never rewrites session history.
use super::errors::file_result;
use crate::error::{SessionError, SessionResult};
use crate::session::{
    memory::Clock,
    progress::ProgressState,
    types::{AssistantFrameOp, AssistantFrameRecord},
};
use rpi_tools::env::{FileContent, FileSystem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) const FORMAT: &str = "assistant-progress-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: String,
    session_id: String,
    state: ProgressState,
}
#[derive(Default)]
struct State {
    progress: Option<ProgressState>,
    poisoned: bool,
    length: u64,
    digest: Sha256,
}
#[derive(Clone)]
pub(super) struct JsonlProgressStore {
    fs: Arc<dyn FileSystem>,
    path: String,
    session_id: String,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
    pub required: bool,
}
impl JsonlProgressStore {
    pub fn new(
        fs: Arc<dyn FileSystem>,
        session_path: &str,
        session_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            fs,
            path: format!("{session_path}.progress.jsonl"),
            session_id: session_id.into(),
            clock,
            state: Arc::new(Mutex::new(State::default())),
            required: true,
        }
    }
    /// Publish before the primary header advertises this required companion.
    pub async fn initialize(&self) -> SessionResult<()> {
        let mut state = self.state.lock().await;
        let progress = ProgressState::default();
        let bytes = self.checkpoint(&progress)?;
        let length = bytes.len() as u64;
        let mut digest = Sha256::new();
        digest.update(&bytes);
        file_result(
            self.fs
                .write_file(&self.path, FileContent::Bytes(bytes), None)
                .await,
            "initialize progress companion",
        )?;
        state.length = length;
        state.digest = digest;
        state.progress = Some(progress);
        Ok(())
    }
    /// For explicit legacy upgrade only. Never adopt or overwrite undeclared
    /// payloads. A matching pristine checkpoint is the sole retryable remnant
    /// of an interrupted two-file publication. This deliberately does not call
    /// `load`, whose repair/normalization behavior could modify foreign data.
    pub async fn initialize_legacy_upgrade(&self) -> SessionResult<()> {
        if file_result(
            self.fs.exists(&self.path, None).await,
            "check legacy progress companion",
        )? {
            let bytes = file_result(
                self.fs.read_binary_file(&self.path, None).await,
                "read legacy progress companion",
            )?;
            // The normal reader requires exactly one JSONL checkpoint line.
            // Whole-document JSON would also accept pretty printing or blank
            // trailing lines that become unreadable after publishing the header.
            if !bytes.ends_with(b"\n") || bytes[..bytes.len() - 1].contains(&b'\n') {
                return Err(SessionError::invalid_entry(
                    "undeclared progress companion is not a single checkpoint line",
                ));
            }
            let mut checkpoint: Checkpoint = serde_json::from_slice(&bytes).map_err(|e| {
                SessionError::invalid_entry(format!(
                    "undeclared progress companion is not an empty checkpoint: {e}"
                ))
            })?;
            if checkpoint.format != FORMAT
                || checkpoint.session_id != self.session_id
                || checkpoint.state.sequence != 0
                || !checkpoint.state.frames.is_empty()
            {
                return Err(SessionError::invalid_entry(
                    "undeclared progress companion is not a matching empty checkpoint",
                ));
            }
            checkpoint.state.validate()?;
            return Ok(());
        }
        let bytes = self.checkpoint(&ProgressState::default())?;
        super::storage::publish_file_atomically(&self.fs, &self.path, |fs, temporary| {
            Box::pin(async move {
                file_result(
                    fs.write_file(temporary, FileContent::Bytes(bytes), None)
                        .await,
                    "stage legacy progress companion",
                )
            })
        })
        .await
    }

    async fn load(&self, state: &mut State) -> SessionResult<()> {
        self.load_with_repair(state, true).await
    }
    pub async fn require_idle_without_repair(&self) -> SessionResult<()> {
        let mut state = self.state.lock().await;
        self.load_with_repair(&mut state, false).await?;
        if !state.progress.as_ref().unwrap().frames.is_empty() {
            return Err(SessionError::invalid_entry(
                "resource transition rejects pending native progress",
            ));
        }
        Ok(())
    }
    async fn load_with_repair(&self, state: &mut State, repair: bool) -> SessionResult<()> {
        if state.poisoned {
            return Err(SessionError::storage(
                "progress append is uncertain; reopen session",
            ));
        }
        if state.progress.is_some() {
            return Ok(());
        }
        if !self.required {
            // A history-only artifact declares no independently stored progress.
            state.progress = Some(ProgressState::default());
            return Ok(());
        }
        let text = match self.fs.read_text_file(&self.path, None).await {
            Ok(text) => text,
            Err(error) => return file_result(Err(error), "read required progress companion"),
        };
        let mut lines = text.split_inclusive('\n');
        let header = lines
            .next()
            .ok_or_else(|| SessionError::invalid_entry("empty progress checkpoint"))?;
        if !header.ends_with('\n') {
            return Err(SessionError::invalid_entry(
                "incomplete progress checkpoint",
            ));
        }
        let mut checkpoint: Checkpoint = serde_json::from_str(header).map_err(|error| {
            SessionError::invalid_entry(format!("invalid progress checkpoint: {error}"))
        })?;
        if checkpoint.format != FORMAT || checkpoint.session_id != self.session_id {
            return Err(SessionError::invalid_entry(
                "progress format/session identity mismatch",
            ));
        }
        checkpoint.state.validate()?;
        let mut progress = checkpoint.state;
        let mut valid_bytes = header.len();
        let mut normalize = false;
        let remainder: Vec<_> = lines.collect();
        for (index, line) in remainder.iter().enumerate() {
            let record: AssistantFrameRecord = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(error)
                    if (error.is_eof() || error.is_syntax())
                        && index + 1 == remainder.len()
                        && !line.ends_with('\n') =>
                {
                    if !repair {
                        return Err(SessionError::invalid_entry(
                            "resource transition rejects torn progress tail",
                        ));
                    }
                    file_result(
                        self.fs
                            .truncate_file(&self.path, valid_bytes as u64, None)
                            .await,
                        "repair torn progress tail",
                    )?;
                    break;
                }
                Err(error) => {
                    return Err(SessionError::invalid_entry(format!(
                        "invalid progress record: {error}"
                    )))
                }
            };
            if progress.sequence.checked_add(1) != Some(record.base.seq) {
                return Err(SessionError::invalid_entry(
                    "nonconsecutive progress sequence",
                ));
            }
            progress.prepare(vec![record.clone()], || record.base.timestamp)?;
            progress.apply(&[record]);
            valid_bytes += line.len();
            normalize |= !line.ends_with('\n');
        }
        // A complete JSON value without its final newline must not be followed
        // by concatenated JSON on the next append. Publish a canonical checkpoint.
        if normalize {
            if !repair {
                return Err(SessionError::invalid_entry(
                    "resource transition rejects incomplete progress newline",
                ));
            }
            self.publish(self.checkpoint(&progress)?, state).await?;
        } else {
            state.length = valid_bytes as u64;
            state.digest = Sha256::new();
            state.digest.update(&text.as_bytes()[..valid_bytes]);
        }
        state.progress = Some(progress);
        Ok(())
    }
    fn checkpoint(&self, progress: &ProgressState) -> SessionResult<Vec<u8>> {
        #[derive(Serialize)]
        struct BorrowedCheckpoint<'a> {
            format: &'a str,
            session_id: &'a str,
            state: &'a ProgressState,
        }
        let mut bytes = serde_json::to_vec(&BorrowedCheckpoint {
            format: FORMAT,
            session_id: &self.session_id,
            state: progress,
        })
        .map_err(|error| SessionError::storage(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    async fn publish(&self, bytes: Vec<u8>, state: &mut State) -> SessionResult<()> {
        let length = bytes.len() as u64;
        let mut digest = Sha256::new();
        digest.update(&bytes);
        let temporary = format!("{}.tmp", self.path);
        file_result(
            self.fs
                .write_file(&temporary, FileContent::Bytes(bytes), None)
                .await,
            "stage progress checkpoint",
        )?;
        if let Err(error) = self.fs.rename_file(&temporary, &self.path, None).await {
            // Rename may have succeeded before its error was reported. Never append
            // using the stale in-memory projection in that uncertain state.
            state.poisoned = true;
            return file_result(Err(error), "publish progress checkpoint");
        }
        state.length = length;
        state.digest = digest;
        Ok(())
    }
    pub async fn append(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        if !self.required {
            return Err(SessionError::invalid_payload(
                "history-only JSONL must be imported or forked before recording progress",
            ));
        }
        let mut state = self.state.clone().lock_owned().await;
        let store = self.clone();
        // The owner survives cancellation of the caller, including during rename.
        tokio::spawn(async move {
            store.load(&mut state).await?;
            if records.is_empty() {
                return Ok(Vec::new());
            }
            let prepared = state
                .progress
                .as_ref()
                .unwrap()
                .prepare(records, || store.clock.now_ms())?;
            if prepared
                .iter()
                .any(|record| record.op != AssistantFrameOp::Append)
            {
                let candidate = state.progress.as_ref().unwrap().project(&prepared);
                store
                    .publish(store.checkpoint(&candidate)?, &mut state)
                    .await?;
                state.progress = Some(candidate);
            } else {
                let mut bytes = Vec::new();
                for record in &prepared {
                    serde_json::to_writer(&mut bytes, record)
                        .map_err(|error| SessionError::storage(error.to_string()))?;
                    bytes.push(b'\n');
                }
                let before = file_result(
                    store.fs.file_info(&store.path, None).await,
                    "stat progress append",
                )?
                .size;
                if before != state.length {
                    state.poisoned = true;
                    return Err(SessionError::storage(
                        "progress length changed externally; reopen session",
                    ));
                }
                let after = before
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| SessionError::storage("progress length overflow"))?;
                // Open per batch: no cached handle can target a retired inode.
                let mut handle = file_result(
                    store.fs.open_append_file(&store.path).await,
                    "open progress append",
                )?;
                let result = match handle.as_mut() {
                    Some(handle) => handle.append(&bytes).await,
                    None => {
                        store
                            .fs
                            .append_file(&store.path, FileContent::Bytes(bytes.clone()), None)
                            .await
                    }
                };
                drop(handle);
                if let Err(error) = result {
                    let rolled_back = match store.fs.read_binary_file(&store.path, None).await {
                        Ok(current)
                            if current.len() >= before as usize
                                && current.len() - before as usize <= bytes.len()
                                && Sha256::digest(&current[..before as usize])
                                    == state.digest.clone().finalize()
                                && current[before as usize..]
                                    == bytes[..current.len() - before as usize] =>
                        {
                            store
                                .fs
                                .truncate_file(&store.path, before, None)
                                .await
                                .is_ok()
                        }
                        _ => false,
                    };
                    if !rolled_back {
                        state.poisoned = true;
                    }
                    return file_result(Err(error), "append progress payloads");
                }
                state.digest.update(&bytes);
                state.length = after;
                state.progress.as_mut().unwrap().apply(&prepared);
            }
            Ok(prepared)
        })
        .await
        .map_err(|error| SessionError::storage(format!("progress owner failed: {error}")))?
    }
    pub async fn prune_completed(
        &self,
        completed: crate::session::progress::CompletedProgressRuns,
    ) -> SessionResult<usize> {
        let mut state = self.state.clone().lock_owned().await;
        let store = self.clone();
        tokio::spawn(async move {
            store.load(&mut state).await?;
            let current = state.progress.as_ref().unwrap();
            let Some(candidate) = current.project_completed(&completed) else {
                return Ok(0);
            };
            let removed = current.frames.len() - candidate.frames.len();
            store
                .publish(store.checkpoint(&candidate)?, &mut state)
                .await?;
            state.progress = Some(candidate);
            Ok(removed)
        })
        .await
        .map_err(|error| SessionError::storage(format!("progress cleanup owner failed: {error}")))?
    }

    pub async fn read(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        let mut state = self.state.clone().lock_owned().await;
        let store = self.clone();
        let run_id = run_id.to_owned();
        // Initial loading may repair a torn tail or normalize a checkpoint.
        // Its filesystem writes need the same cancellation-safe ownership as append.
        tokio::spawn(async move {
            store.load(&mut state).await?;
            Ok(state.progress.as_ref().unwrap().read(&run_id))
        })
        .await
        .map_err(|error| SessionError::storage(format!("progress read owner failed: {error}")))?
    }
    pub async fn drain(&self) {
        let _guard = self.state.lock().await;
    }
}
