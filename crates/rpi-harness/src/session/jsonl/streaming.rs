//! Bounded physical-line scanner and atomic span-copy repair. Offsets always
//! refer to the reader generation that validated them, never the published path.
use super::*;
use rpi_tools::env::ReadFile;
const CHUNK: usize = 1024 * 1024;

struct Scanner {
    reader: Arc<dyn ReadFile>,
    length: u64,
    fetched: u64,
    offset: u64,
    buffer: Vec<u8>,
    cursor: usize,
    hash: Sha256,
    hash_before_buffer: Sha256,
    policy: ReadPolicy,
}
impl Scanner {
    fn new(reader: Arc<dyn ReadFile>, length: u64) -> Self {
        Self {
            reader,
            length,
            fetched: 0,
            offset: 0,
            buffer: Vec::new(),
            cursor: 0,
            hash: Sha256::new(),
            hash_before_buffer: Sha256::new(),
            policy: ReadPolicy::Restore,
        }
    }
    async fn next(&mut self) -> SessionResult<Option<(u64, Vec<u8>, bool, bool)>> {
        let offset = self.offset;
        let mut line = Vec::new();
        loop {
            self.policy.check(0)?;
            if self.cursor < self.buffer.len() {
                let mut remainder = &self.buffer[self.cursor..];
                // The slice BufRead implementation uses the standard library's
                // optimized delimiter search without validating partial UTF-8.
                let consumed = std::io::BufRead::read_until(&mut remainder, b'\n', &mut line)
                    .expect("reading a byte slice is infallible");
                self.cursor += consumed;
                self.offset += consumed as u64;
                if line.last() == Some(&b'\n') {
                    line.pop();
                    return Ok(Some((offset, line, self.offset == self.length, true)));
                }
            }
            if self.fetched == self.length {
                return Ok((!line.is_empty()).then_some((offset, line, true, false)));
            }
            let count = (self.length - self.fetched).min(CHUNK as u64) as usize;
            self.buffer = file_result(
                self.reader.read_range(self.fetched, count).await,
                "scan session history",
            )?;
            self.policy.check(0)?;
            if self.buffer.len() != count {
                return Err(SessionError::storage("short session history scan"));
            }
            self.hash_before_buffer = self.hash.clone();
            self.hash.update(&self.buffer);
            self.fetched += count as u64;
            self.cursor = 0;
        }
    }

    async fn resume(&self, prefix: startup_checkpoint::Boundary) -> Option<Self> {
        if prefix.bytes < self.offset || prefix.bytes > self.length {
            return None;
        }
        // The initial header read may already contain some of the prefix or
        // even some of the suffix. Reuse those exact bytes and hash context.
        if prefix.bytes < self.fetched {
            let cursor = self.buffer.len() - (self.fetched - prefix.bytes) as usize;
            let prior = &self.buffer[..cursor];
            let mut hash = self.hash_before_buffer.clone();
            hash.update(prior);
            if prior.last() != Some(&b'\n') || hash.finalize() != prefix.digest {
                return None;
            }
            return Some(Self {
                reader: self.reader.clone(),
                length: self.length,
                fetched: self.fetched,
                offset: prefix.bytes,
                buffer: self.buffer.clone(),
                cursor,
                hash: self.hash.clone(),
                hash_before_buffer: self.hash_before_buffer.clone(),
                policy: self.policy,
            });
        }
        let mut hash = self.hash.clone();
        let mut last = self.buffer.last().copied();
        let mut offset = self.fetched;
        while offset < prefix.bytes {
            let count = (prefix.bytes - offset).min(CHUNK as u64) as usize;
            let bytes = self.reader.read_range(offset, count).await.ok()?;
            if bytes.len() != count {
                return None;
            }
            hash.update(&bytes);
            last = bytes.last().copied();
            offset += count as u64;
        }
        if last != Some(b'\n') || hash.clone().finalize() != prefix.digest {
            return None;
        }
        Some(Self {
            reader: self.reader.clone(),
            length: self.length,
            fetched: prefix.bytes,
            offset: prefix.bytes,
            buffer: Vec::new(),
            cursor: 0,
            hash_before_buffer: hash.clone(),
            hash,
            policy: self.policy,
        })
    }
}

async fn load_checkpoint(
    fs: &Arc<dyn FileSystem>,
    path: &str,
    scan: &Scanner,
) -> Option<(startup_checkpoint::Boundary, Scanner, ResidentState)> {
    let saved = startup_checkpoint::Checkpoint::open(fs, path).await?;
    let boundary = saved.boundary;
    let resumed = scan.resume(boundary).await?;
    let state = saved
        .image()
        .await?
        .bind(&scan.reader, scan.offset, boundary.bytes)?;
    Some((boundary, resumed, state))
}

async fn copy_range(
    fs: &Arc<dyn FileSystem>,
    target: &str,
    reader: &Arc<dyn ReadFile>,
    offset: u64,
    length: u64,
    hash: &mut Sha256,
) -> SessionResult<()> {
    let mut copied = 0;
    while copied < length {
        let count = (length - copied).min(CHUNK as u64) as usize;
        let bytes = file_result(
            reader.read_range(offset + copied, count).await,
            "copy session history span",
        )?;
        if bytes.len() != count {
            return Err(SessionError::storage("short session history span"));
        }
        hash.update(&bytes);
        file_result(
            fs.append_file(target, FileContent::Bytes(bytes), None)
                .await,
            "stage session history span",
        )?;
        copied += count as u64;
    }
    Ok(())
}
pub(super) async fn hash_reader(
    reader: &Arc<dyn ReadFile>,
    length: u64,
) -> SessionResult<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < length {
        let count = (length - offset).min(CHUNK as u64) as usize;
        let bytes = file_result(
            reader.read_range(offset, count).await,
            "hash session history",
        )?;
        if bytes.len() != count {
            return Err(SessionError::storage("short session history hash read"));
        }
        hash.update(&bytes);
        offset += count as u64;
    }
    Ok(hash.finalize())
}

impl JsonlSessionStorage {
    pub(super) async fn load_streaming(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        reader: Arc<dyn ReadFile>,
        length: u64,
        policy: ReadPolicy,
    ) -> SessionResult<Self> {
        let preview_source = (policy.repairs() || policy.defers_repairs())
            .then(|| preview::native_path(fs.as_ref(), path))
            .flatten()
            .and_then(|path| preview::Source::replay(path, reader.as_ref(), length));
        let scanner = || {
            let mut scan = Scanner::new(reader.clone(), length);
            scan.policy = policy;
            scan
        };
        let mut storage = if let Some(storage) = Self::load_streaming_impl(
            fs.clone(),
            path,
            clock.clone(),
            ids.clone(),
            scanner(),
            true,
        )
        .await?
        {
            storage
        } else {
            Self::load_streaming_impl(fs, path, clock, ids, scanner(), false)
                .await?
                .ok_or_else(|| {
                    SessionError::storage("unexpected fallback from full history replay")
                })?
        };
        if let Some(source) = preview_source.filter(|source| source.matches(reader.as_ref())) {
            *storage.preview_source.lock().unwrap() = Some(source);
        }
        if policy.repairs() {
            storage.commit_pending_repair().await?;
        }
        Ok(storage)
    }

    /// `None` restarts replay from the header because the suffix needs physical
    /// offsets from the verified prefix to preserve canonical recovery.
    async fn load_streaming_impl(
        fs: Arc<dyn FileSystem>,
        path: &str,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        mut scan: Scanner,
        allow_checkpoint: bool,
    ) -> SessionResult<Option<Self>> {
        let policy = scan.policy;
        policy.check(0)?;
        let reader = scan.reader.clone();
        let length = scan.length;
        let allow_repair = policy.repairs();
        let defer_repair = policy.defers_repairs();
        let strict = policy.strict();
        let Some((_, header_raw, _, header_newline)) = scan.next().await? else {
            return Err(invalid_file(path, 1, &"is missing a header"));
        };
        let header_line = std::str::from_utf8(&header_raw)
            .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
        if header_line.is_empty() {
            return Err(invalid_file(path, 1, &"is missing a header"));
        }
        if strict && !header_newline {
            return Err(SessionError::invalid_entry(
                "resource transition requires complete unrecovered history",
            ));
        }
        let header = parse_header(header_line).map_err(|e| invalid_file(path, 1, &e))?;
        if header.progress_format.is_some()
            && !file_result(
                fs.exists(&format!("{path}.progress.jsonl"), None).await,
                "check required progress companion",
            )?
        {
            return Err(SessionError::invalid_entry("required progress companion is missing; restore the complete session, not only its history"));
        }
        let info = file_result(fs.file_info(path, None).await, "read session metadata")?;
        let mut storage = Self::new(
            fs.clone(),
            metadata_from_header(&header, path, info.mtime_ms),
            clock,
            ids,
        );
        // A sibling staging file is never authoritative. Only a writable
        // restore may remove it, and ownership is acquired before mutation.
        if allow_repair {
            let staged_path = format!("{path}.tmp");
            if file_result(
                fs.exists(&staged_path, None).await,
                "check staged history recovery artifact",
            )? {
                storage.ensure_writer_lease().await?;
                file_result(
                    storage.fs.remove(&staged_path, false, true, None).await,
                    "remove stale staged history recovery artifact",
                )?;
            }
        }
        storage.progress.required = header.progress_format.is_some();
        let embedded =
            history_checkpoint(&header).map_err(|error| invalid_file(path, 1, &error))?;
        let mut cacheable = cfg!(unix)
            && defer_repair
            && header_newline
            && embedded.is_none()
            && length >= 4 * 1024 * 1024
            && fs.native_file_path(path).is_some()
            && info.kind == rpi_tools::env::FileKind::File;
        if let Some(checkpoint) = embedded {
            let resident = ResidentState::from_checkpoint(checkpoint.state)?;
            *storage.state.lock().await = resident;
        }
        let mut reused = None;
        if allow_checkpoint && cacheable {
            let phase =
                crate::session::diagnostics::RecoveryPhase::start("startup_checkpoint_load");
            if let Some((prefix, resumed, resident)) = load_checkpoint(&fs, path, &scan).await {
                reused = Some(prefix);
                scan = resumed;
                *storage.state.lock().await = resident;
            }
            phase.finish(0, reused.map_or(0, |prefix| prefix.lines));
        }
        let mut spans = vec![(0u64, header_raw.len() as u64)];
        let mut seen: HashMap<u64, (u64, usize)> = HashMap::new();
        let mut repair = false;
        let mut terminated = header_newline;
        let first_line = reused.map_or(1, |prefix| prefix.lines);
        let mut line_no = first_line;
        use crate::session::diagnostics::RecoveryAccumulator;
        let mut scan_time = RecoveryAccumulator::new("replay_scan");
        let mut probe_time = RecoveryAccumulator::new("replay_classify");
        let mut decode_time = RecoveryAccumulator::new("replay_decode");
        let mut apply_time = RecoveryAccumulator::new("replay_index_apply");
        while let Some((offset, raw, last, newline)) = {
            let started = scan_time.start();
            let row = scan.next().await?;
            scan_time.record(started);
            row
        } {
            line_no += 1;
            policy.check(line_no - 1)?;
            if reused.is_some() && !newline {
                return Ok(None);
            }
            terminated = newline;
            if strict && !newline {
                return Err(SessionError::invalid_entry(
                    "resource transition requires complete unrecovered history",
                ));
            }
            let classify_started = probe_time.start();
            let line = std::str::from_utf8(&raw)
                .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
            // Historical assistant frames do not need their discarded payloads
            // during cold startup. Recognize their indexed metadata directly;
            // duplicate/backward sequences still fall through to the canonical
            // codec so recovery keeps its exact-equivalence checks.
            let frame_metadata = super::super::cold_decode::parse_cold_frame(line);
            probe_time.record(classify_started);
            if let Some(metadata) = frame_metadata {
                let apply_started = apply_time.start();
                let mut state = storage.state.lock().await;
                if metadata.seq > state.sequence() {
                    if metadata.seq > state.next_sequence() {
                        state.resync_sequence(metadata.seq - 1);
                    }
                    let seq = metadata.seq;
                    if policy.keeps_frame_locations() {
                        state.apply_loaded_frame(metadata, &reader, offset, &raw)?;
                    } else {
                        // Preserve reducer identity/lane checks and clear
                        // semantics without allocating a retained location.
                        state.reserve_loaded_frame(metadata)?;
                    }
                    if reused.is_none() {
                        seen.insert(seq, (offset, raw.len()));
                        spans.push((offset, raw.len() as u64));
                    }
                    apply_time.record(apply_started);
                    continue;
                }
                apply_time.record(apply_started);
            }
            let decode_started = decode_time.start();
            let mutation = parse_mutation(line);
            decode_time.record(decode_started);
            match mutation {
                Ok(mutation) => {
                    let apply_started = apply_time.start();
                    let mut state = storage.state.lock().await;
                    if mutation.seq() <= state.sequence() {
                        if reused.is_some() {
                            return Ok(None);
                        }
                        let duplicate = if let Some(&(previous, count)) = seen.get(&mutation.seq())
                        {
                            let bytes = file_result(
                                reader.read_range(previous, count).await,
                                "compare duplicate session mutation",
                            )?;
                            let prior = std::str::from_utf8(&bytes)
                                .ok()
                                .and_then(|line| parse_mutation(line).ok());
                            prior.as_ref() == Some(&mutation)
                        } else {
                            false
                        };
                        if duplicate {
                            if strict {
                                return Err(SessionError::invalid_entry(
                                    "resource transition rejects duplicate history sequences",
                                ));
                            }
                            repair = true;
                            apply_time.record(apply_started);
                            continue;
                        }
                        return Err(invalid_file(
                            path,
                            line_no.try_into().unwrap_or(u32::MAX),
                            &format!("conflicting mutations share sequence {}", mutation.seq()),
                        ));
                    }
                    if mutation.seq() > state.next_sequence() {
                        state.resync_sequence(mutation.seq() - 1);
                    }
                    let seq = mutation.seq();
                    let mutation = chain_onto_lane_leaf(mutation, &state);
                    state
                        .apply_loaded(mutation, &reader, offset, &raw)
                        .map_err(|error| {
                            if error.code == SessionErrorCode::InvalidEntry {
                                invalid_file(path, line_no.try_into().unwrap_or(u32::MAX), &error)
                            } else {
                                error
                            }
                        })?;
                    if reused.is_none() {
                        seen.insert(seq, (offset, raw.len()));
                        spans.push((offset, raw.len() as u64));
                    }
                    apply_time.record(apply_started);
                }
                Err(error)
                    if !strict
                        && error.kind == JsonlDecodeErrorKind::Schema
                        && is_assistant_frame_append(line) =>
                {
                    if reused.is_some() {
                        return Ok(None);
                    }
                    cacheable = false;
                    tracing::warn!(line = line_no, "skipping undecodable assistant frame");
                    spans.push((offset, raw.len() as u64));
                }
                Err(error) if !strict && last && error.kind == JsonlDecodeErrorKind::Syntax => {
                    if reused.is_some() {
                        return Ok(None);
                    }
                    repair = true;
                    break;
                }
                Err(error) => {
                    return Err(invalid_file(
                        path,
                        line_no.try_into().unwrap_or(u32::MAX),
                        &error,
                    ))
                }
            }
        }
        let source_digest = scan.hash.clone().finalize();
        if cacheable && !repair && terminated {
            storage.startup_checkpoint = Some(startup_checkpoint::Update {
                boundary: startup_checkpoint::Boundary {
                    bytes: length,
                    lines: line_no,
                    digest: source_digest,
                },
                reused,
            });
        }
        *storage.loaded_history_digest.lock().unwrap() = Some(source_digest);
        if !storage.progress.required {
            *storage.legacy_history_digest.lock().unwrap() = Some(source_digest);
        }
        if allow_repair || defer_repair {
            *storage.pending_repair.lock().unwrap() = if repair {
                Some(PendingRepair::Streaming { reader, spans })
            } else if !terminated {
                let digest = (!storage.progress.required).then(|| {
                    scan.hash.update(b"\n");
                    scan.hash.finalize()
                });
                Some(PendingRepair::AppendNewline { digest })
            } else {
                None
            };
        }
        policy.check(line_no - 1)?;
        // These are disjoint accumulated timings, not per-row stderr events.
        // A checkpoint hit counts only replayed suffix rows in these phases.
        let rows = line_no - first_line;
        scan_time.finish(rows);
        probe_time.finish(rows);
        decode_time.finish(rows);
        apply_time.finish(rows);
        Ok(Some(storage))
    }

    pub(super) async fn commit_pending_repair_impl(
        &mut self,
        pending: PendingRepair,
    ) -> SessionResult<()> {
        let preview_source = self
            .preview_source
            .lock()
            .unwrap()
            .take()
            .filter(preview::Source::is_current);
        let layout = match pending {
            PendingRepair::Streaming { reader, spans } => {
                let layout = preview_source
                    .as_ref()
                    .filter(|source| source.matches(reader.as_ref()))
                    .and_then(|_| preview::RepairLayout::from_spans(&spans));
                let path = self.metadata.path.clone();
                let digest = Arc::new(std::sync::Mutex::new(None));
                let output = digest.clone();
                publish_file_atomically(&self.fs, &path, move |fs, temp| {
                    Box::pin(async move {
                        file_result(
                            fs.write_file(temp, FileContent::Bytes(Vec::new()), None)
                                .await,
                            "stage session repair",
                        )?;
                        let mut hash = Sha256::new();
                        for (offset, count) in spans {
                            copy_range(fs, temp, &reader, offset, count, &mut hash).await?;
                            file_result(
                                fs.append_file(temp, FileContent::Bytes(vec![b'\n']), None)
                                    .await,
                                "stage session repair newline",
                            )?;
                            hash.update(b"\n");
                        }
                        *output.lock().unwrap() = Some(hash.finalize());
                        Ok(())
                    })
                })
                .await?;
                if !self.progress.required {
                    *self.legacy_history_digest.lock().unwrap() = *digest.lock().unwrap();
                }
                layout
            }
            PendingRepair::Eager { bytes } => {
                let digest = Sha256::digest(&bytes);
                let path = self.metadata.path.clone();
                publish_file_atomically(&self.fs, &path, move |fs, temp| {
                    Box::pin(async move {
                        file_result(
                            fs.write_file(temp, FileContent::Bytes(bytes), None).await,
                            "stage prepared eager session repair",
                        )?;
                        Ok(())
                    })
                })
                .await?;
                if !self.progress.required {
                    *self.legacy_history_digest.lock().unwrap() = Some(digest);
                }
                None
            }
            PendingRepair::AppendNewline { digest } => {
                let layout = preview_source
                    .as_ref()
                    .and_then(preview::Source::final_newline_layout);
                file_result(
                    self.fs
                        .append_file(&self.metadata.path, FileContent::Text("\n".into()), None)
                        .await,
                    "repair unterminated session tail",
                )?;
                file_result(
                    self.fs.sync_file(&self.metadata.path, true, None).await,
                    "sync repaired unterminated session tail",
                )?;
                if let Some(digest) = digest {
                    *self.legacy_history_digest.lock().unwrap() = Some(digest);
                }
                layout
            }
        };
        if let Some((source, layout)) = preview_source.zip(layout) {
            if let Ok(Some(reader)) = self.fs.open_read_file(&self.metadata.path).await {
                let sequence = self.state.lock().await.sequence();
                *self.preview_source.lock().unwrap() =
                    source.after_repair(reader.as_ref(), layout, sequence);
            }
        }
        Ok(())
    }

    /// Returns false only for compatibility filesystems without stable ranged
    /// readers; their original eager upgrade implementation remains isolated.
    pub(super) async fn upgrade_streaming(&mut self, expected: [u8; 32]) -> SessionResult<bool> {
        let path = self.metadata.path.clone();
        let Some(reader) = file_result(
            self.fs.open_read_file(&path).await,
            "open progress upgrade history",
        )?
        else {
            return Ok(false);
        };
        let Some(length) = file_result(reader.byte_len().await, "stat progress upgrade history")?
        else {
            return Ok(false);
        };
        if hash_reader(&reader, length).await? != expected {
            return Err(SessionError::invalid_entry(
                "session history changed before progress upgrade; reopen session",
            ));
        }
        let mut scan = Scanner::new(reader.clone(), length);
        let Some((_, raw, _, newline)) = scan.next().await? else {
            return Err(SessionError::invalid_entry(
                "progress upgrade requires a complete header",
            ));
        };
        if !newline {
            return Err(SessionError::invalid_entry(
                "progress upgrade requires a complete header",
            ));
        }
        let header_line = std::str::from_utf8(&raw)
            .map_err(|_| SessionError::invalid_entry("invalid session UTF-8"))?;
        let header = parse_header(header_line).map_err(|e| invalid_file(&path, 1, &e))?;
        if header.id != self.metadata.id || header.progress_format.is_some() || header.version != 4
        {
            return Err(SessionError::invalid_entry(
                "session header changed before progress upgrade",
            ));
        }
        let mut value: serde_json::Value = serde_json::from_str(header_line)
            .map_err(|e| SessionError::invalid_entry(e.to_string()))?;
        value["progressFormat"] = super::super::progress::FORMAT.into();
        let upgraded = format!(
            "{}\n",
            serde_json::to_string(&value).map_err(|e| SessionError::storage(e.to_string()))?
        );
        let body_offset = raw.len() as u64 + 1;
        let upgraded_header_len = upgraded.len() as u64;
        let preview_source = self
            .preview_source
            .lock()
            .unwrap()
            .take()
            .filter(|source| source.matches(reader.as_ref()));
        let progress = super::super::progress::JsonlProgressStore::new(
            self.fs.clone(),
            &path,
            &header.id,
            self.clock.clone(),
        );
        progress.initialize_legacy_upgrade().await?;
        *self.append.lock().await = None;
        let history_path = path.clone();
        publish_file_atomically(&self.fs, &path, move |fs, temp| {
            Box::pin(async move {
                file_result(
                    fs.write_file(temp, FileContent::Text(upgraded), None).await,
                    "stage progress upgrade header",
                )?;
                copy_range(
                    fs,
                    temp,
                    &reader,
                    body_offset,
                    length - body_offset,
                    &mut Sha256::new(),
                )
                .await?;
                // Re-open the path too: hashing the old reader alone cannot detect
                // an atomic path replacement by a foreign writer.
                let current = file_result(
                    fs.open_read_file(&history_path).await,
                    "recheck history for progress upgrade",
                )?
                .ok_or_else(|| SessionError::storage("missing upgrade recheck reader"))?;
                let current_len =
                    file_result(current.byte_len().await, "stat upgrade recheck reader")?
                        .ok_or_else(|| SessionError::storage("missing upgrade recheck length"))?;
                if current_len != length || hash_reader(&current, current_len).await? != expected {
                    return Err(SessionError::invalid_entry(
                        "session history changed during progress upgrade; reopen session",
                    ));
                }
                Ok(())
            })
        })
        .await?;
        self.progress = progress;
        *self.legacy_history_digest.lock().unwrap() = None;
        if let Some(source) = preview_source {
            if let Ok(Some(reader)) = self.fs.open_read_file(&path).await {
                let sequence = self.state.lock().await.sequence();
                *self.preview_source.lock().unwrap() = source.after_header_rewrite(
                    reader.as_ref(),
                    sequence,
                    body_offset,
                    upgraded_header_len,
                );
                self.publish_preview().await;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod tests;
