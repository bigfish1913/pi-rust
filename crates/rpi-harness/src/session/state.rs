//! Mirrors `packages/agent/src/harness/session/state.ts` — the in-memory
//! mutation reducer that enforces the recoverable-session invariants.
//!
//! Critical invariants enforced here (plan §5):
//! - **Consecutive seq**: `seq == state.sequence + 1` or `invalid_entry`.
//!   Recovery from a torn tail relies on a global monotonic sequence.
//! - **Write-once ids**: entry/record ids are unique forever (`already_exists`
//!   on reuse).
//! - **Lane-leaf chaining**: an entry with `lane: Some(l)` must have
//!   `entry.parent_id == lanes[l]` (the lane's current leaf). Otherwise the
//!   branch would fork silently.
//! - **Fork-without-lane**: the fork path passes `lane: None`, so chaining is
//!   *not* checked — the forked entries arrive with their original `parent_id`
//!   chain intact, and a subsequent `Lane { lane, leaf_id }` mutation re-points
//!   the lane.
//! - **At-most-one open op per lane**: a 2nd `operation_started` while one is
//!   open is *not* rejected here (state allows the map to hold multiple, to
//!   support the "aborting while suspended" snapshot path); the **reducer**
//!   (`reducer.rs`) flags `multiple_open_operations` as corruption, and
//!   recovery's `find_open_operations(limit: 2)` interprets `>=2` as corruption.
//!
//! Note: the TS `applyMutation` allows multiple open operations to coexist in
//! `openOperationsByLane` (it only deletes on `operation_finished`). The
//! "at-most-one" rule is a *reducer-level* corruption check plus a harness-level
//! precondition, not a state-level hard reject. This port mirrors that exactly.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::error::{SessionError, SessionResult};
use crate::session::types::{
    AssistantFrameOp, BranchBounds, Entry, EntryOrder, EntryQuery, ForkOptions, ForkPosition,
    LanePointer, LaneRecord, LogItem, LogOptions, OperationStartedRecord, RecordQuery,
    SessionMetadata, SessionMutation, SessionStats,
};

#[cfg(test)]
mod record_page_tests;
mod snapshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum StoredLogItem {
    Entry(usize),
    Record(usize),
    Other(Box<LogItem>),
}

/// The in-memory mutation reducer. Mirrors TS `SessionState`. Owns the
/// append-only entry/record logs, the lane→leaf map, open operations per lane,
/// global name/labels facts, and running stats.
#[derive(Debug, Clone)]
pub struct SessionState {
    sequence: u64,
    used_ids: std::collections::HashSet<String>,
    entries: Vec<Entry>,
    entries_by_id: HashMap<String, usize>,
    // Rebuilt by replay, including imported/forked entries; never persisted separately.
    nearest_user: Vec<Option<usize>>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>>,
    lanes: BTreeMap<String, Option<String>>,
    log: Vec<StoredLogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// Borrow record payloads; copy only terminal scope metadata once per cleanup.
    pub(crate) fn completed_progress_runs(&self) -> super::progress::CompletedProgressRuns {
        let mut completed = super::progress::CompletedProgressRuns::new();
        for record in &self.records {
            if let LaneRecord::OperationFinished(finished) = record {
                completed
                    .entry(finished.base.lane.clone())
                    .or_default()
                    .insert(finished.run_id.clone());
            }
        }
        completed
    }

    /// New state with a single `main` lane pointing at no leaf (mirrors TS
    /// `lanes = new Map([["main", null]])`).
    pub fn new() -> Self {
        let mut lanes = BTreeMap::new();
        lanes.insert("main".to_string(), None);
        Self {
            sequence: 0,
            used_ids: std::collections::HashSet::new(),
            entries: Vec::new(),
            entries_by_id: HashMap::new(),
            nearest_user: Vec::new(),
            records: Vec::new(),
            open_operations_by_lane: HashMap::new(),
            lanes,
            log: Vec::new(),
            stats: SessionStats::default(),
            name: None,
            labels: HashMap::new(),
        }
    }

    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    /// The highest applied mutation seq (0 before any mutation). The JSONL
    /// loader uses this to detect a duplicated/late append, where a mutation
    /// arrives with an already-consumed seq.
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    /// JSONL-only cold frame reservation. Its immutable payload/log slot lives
    /// in the file-backed wrapper, but IDs and sequence remain write-once.
    pub(crate) fn reserve_cold_frame(
        &mut self,
        frame: &crate::session::types::AssistantFrameRecord,
    ) -> SessionResult<()> {
        if frame.base.seq != self.sequence + 1 {
            return Err(SessionError::invalid_entry(
                "non-consecutive cold frame sequence",
            ));
        }
        if !self.lanes.contains_key(&frame.base.lane) {
            return Err(SessionError::invalid_entry(format!(
                "Invalid session mutation: references missing lane {}",
                frame.base.lane
            )));
        }
        self.validate_unused_id(&frame.base.id)?;
        self.sequence = frame.base.seq;
        self.used_ids.insert(frame.base.id.clone());
        Ok(())
    }

    /// Validate the entire cancellation batch before any reducer mutation.
    pub(crate) fn validate_queue_cancellations(&self, records: &[LaneRecord]) -> SessionResult<()> {
        let mut ids = std::collections::HashSet::new();
        let mut targets = std::collections::HashSet::new();
        for record in records {
            let LaneRecord::QueueCancelled(cancel) = record else {
                return Err(SessionError::storage(
                    "cancellation batch contains a non-cancellation",
                ));
            };
            self.require_lane(&cancel.base.lane)?;
            self.validate_unused_id(&cancel.base.id)?;
            if !ids.insert(cancel.base.id.as_str()) || !targets.insert(cancel.entry_id.as_str()) {
                return Err(SessionError::storage("duplicate queue cancellation"));
            }
            let enqueue = self.find_records(&RecordQuery {
                lane: Some(cancel.base.lane.clone()),
                target_entry_id: Some(cancel.entry_id.clone()),
                pending_queue_only: true,
                limit: Some(1),
                ..Default::default()
            })?;
            if !matches!(enqueue.first(), Some(LaneRecord::QueueEnqueued(enqueued)) if enqueued.run_id == cancel.run_id)
            {
                return Err(SessionError::storage(
                    "queue cancellation has no pending matching enqueue",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn check_revision(&self, expected: u64) -> SessionResult<()> {
        if self.sequence != expected {
            return Err(SessionError::storage(
                "branch write conflict: history changed during conditional write",
            ));
        }
        Ok(())
    }

    /// Advance the seq counter to `seq` (no-op when already at or past it).
    /// The JSONL loader calls this when a divergent writer's counter leaves a
    /// gap, so the next mutation is accepted instead of refused.
    pub(crate) fn resync_sequence(&mut self, seq: u64) {
        if seq > self.sequence {
            self.sequence = seq;
        }
    }

    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes
            .iter()
            .map(|(lane, leaf_id)| LanePointer {
                lane: lane.clone(),
                leaf_id: leaf_id.clone(),
            })
            .collect()
    }

    /// Look up a lane's leaf, or `invalid_lane` if the lane doesn't exist.
    pub fn require_lane(&self, lane: &str) -> SessionResult<Option<String>> {
        self.lanes
            .get(lane)
            .cloned()
            .ok_or_else(|| SessionError::invalid_lane(format!("Lane not found: {lane}")))
    }

    /// `already_exists` if the lane already exists. Mirrors `validateNewLane`.
    pub fn validate_new_lane(&self, lane: &str) -> SessionResult<()> {
        if self.lanes.contains_key(lane) {
            return Err(SessionError::already_exists(format!(
                "Lane already exists: {lane}"
            )));
        }
        Ok(())
    }

    /// `not_found` if `target_id` is set and unknown. Mirrors `validateTarget`.
    pub fn validate_target(&self, target_id: Option<&str>) -> SessionResult<()> {
        if let Some(id) = target_id {
            if !self.entries_by_id.contains_key(id) {
                return Err(SessionError::not_found(format!("Entry not found: {id}")));
            }
        }
        Ok(())
    }

    /// `already_exists` if the id was already used. Mirrors `validateUnusedId`.
    pub fn validate_unused_id(&self, id: &str) -> SessionResult<()> {
        if self.used_ids.contains(id) {
            return Err(SessionError::already_exists(format!(
                "Session id already exists: {id}"
            )));
        }
        Ok(())
    }

    /// Apply a mutation, enforcing every invariant. Mirrors TS `applyMutation`.
    /// The `Entry` arm carries a full [`Entry`] whose `seq`/`parent_id`/`timestamp`
    /// are already stamped by the storage layer (TS shape); the state *validates*
    /// lane-leaf chaining against `entry.parent_id`, it does not set it.
    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> SessionResult<ApplyOutcome> {
        let seq = mutation.seq();
        if seq != self.sequence + 1 {
            return Err(SessionError::invalid_entry(format!(
                "Invalid session mutation: has non-consecutive seq {seq}"
            )));
        }

        match mutation {
            SessionMutation::Entry {
                seq, lane, entry, ..
            } => {
                self.validate_unused_id(entry.id())?;
                // Lane chaining — only when a lane is named (fork passes None).
                if let Some(lane_name) = lane.as_deref() {
                    let leaf = self.lanes.get(lane_name).cloned().ok_or_else(|| {
                        SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing lane {lane_name}"
                        ))
                    })?;
                    // `entry.parent_id` must equal the lane's current leaf.
                    if entry.parent_id().map(|s| s.to_string()) != leaf {
                        return Err(SessionError::invalid_entry(
                            "Invalid session mutation: does not chain to the lane leaf".to_string(),
                        ));
                    }
                }
                // Parent existence (when set).
                if let Some(parent) = entry.parent_id() {
                    if !self.entries_by_id.contains_key(parent) {
                        return Err(SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing parent {parent}"
                        )));
                    }
                }

                let nearest_user = if matches!(
                    entry.as_message(),
                    Some(rpi_agent::message::AgentMessage::User(_))
                ) {
                    Some(self.entries.len())
                } else {
                    entry
                        .parent_id()
                        .and_then(|id| self.entries_by_id.get(id))
                        .and_then(|index| self.nearest_user[*index])
                };
                self.nearest_user.push(nearest_user);
                let is_message = matches!(entry, Entry::Message(_));
                self.sequence = seq;
                self.used_ids.insert(entry.id().to_string());
                self.entries_by_id
                    .insert(entry.id().to_string(), self.entries.len());
                if let Some(lane_name) = lane.as_deref() {
                    if let Some(slot) = self.lanes.get_mut(lane_name) {
                        *slot = Some(entry.id().to_string());
                    }
                }
                self.log.push(StoredLogItem::Entry(self.entries.len()));
                self.entries.push(entry.clone());
                if is_message {
                    self.stats.message_count += 1;
                }
                Ok(ApplyOutcome::Entry(entry))
            }
            SessionMutation::Record { record } => {
                let lane = record.lane().to_string();
                if !self.lanes.contains_key(&lane) {
                    return Err(SessionError::invalid_entry(format!(
                        "Invalid session mutation: references missing lane {lane}"
                    )));
                }
                self.validate_unused_id(record.id())?;
                let seq = record.seq();
                self.sequence = seq;
                self.used_ids.insert(record.id().to_string());
                match &record {
                    LaneRecord::OperationStarted(started) => {
                        self.open_operations_by_lane
                            .entry(lane.clone())
                            .or_default()
                            .insert(seq, started.clone());
                    }
                    LaneRecord::OperationFinished(finished) => {
                        if let Some(open) = self.open_operations_by_lane.get_mut(&lane) {
                            // Remove by run_id (matching the operation_started id).
                            open.retain(|_, started| started.base.id != finished.run_id);
                            if open.is_empty() {
                                self.open_operations_by_lane.remove(&lane);
                            }
                        }
                    }
                    _ => {}
                }
                if let LaneRecord::Usage(usage_rec) = &record {
                    self.stats.cached_tokens += usage_rec.usage.cache_read;
                    self.stats.uncached_tokens +=
                        usage_rec.usage.input + usage_rec.usage.cache_write;
                    self.stats.total_tokens += usage_rec.usage.total_tokens;
                    self.stats.cost_total += usage_rec.usage.cost.total;
                }
                self.log.push(StoredLogItem::Record(self.records.len()));
                self.records.push(record.clone());
                if let LaneRecord::AssistantFrame(frame) = &record {
                    match frame.op {
                        AssistantFrameOp::ClearStream => {
                            self.retire_frames(&frame.run_id, Some(frame.stream_index));
                        }
                        AssistantFrameOp::ClearRun => self.retire_frames(&frame.run_id, None),
                        _ => {}
                    }
                }
                Ok(ApplyOutcome::Record(record))
            }
            SessionMutation::Lane { seq, lane, leaf_id } => {
                if let Some(leaf) = leaf_id.as_deref() {
                    if !self.entries_by_id.contains_key(leaf) {
                        return Err(SessionError::invalid_entry(format!(
                            "Invalid session mutation: references missing lane target {leaf}"
                        )));
                    }
                }
                self.sequence = seq;
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.log.push(StoredLogItem::Other(Box::new(LogItem::Lane {
                    seq,
                    lane,
                    leaf_id,
                })));
                Ok(ApplyOutcome::Lane)
            }
            SessionMutation::FactName { seq, name } => {
                self.sequence = seq;
                self.name = name.clone();
                self.log
                    .push(StoredLogItem::Other(Box::new(LogItem::FactName {
                        seq,
                        name,
                    })));
                Ok(ApplyOutcome::Fact)
            }
            SessionMutation::FactLabel {
                seq,
                target_id,
                label,
            } => {
                if !self.entries_by_id.contains_key(&target_id) {
                    return Err(SessionError::invalid_entry(format!(
                        "Invalid session mutation: references missing label target {target_id}"
                    )));
                }
                self.sequence = seq;
                match &label {
                    Some(l) => {
                        self.labels.insert(target_id.clone(), l.clone());
                    }
                    None => {
                        self.labels.remove(&target_id);
                    }
                }
                self.log
                    .push(StoredLogItem::Other(Box::new(LogItem::FactLabel {
                        seq,
                        target_id,
                        label,
                    })));
                Ok(ApplyOutcome::Fact)
            }
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries_by_id
            .get(id)
            .map(|index| &self.entries[*index])
    }

    pub fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let mut results = Vec::new();
        let iter: Box<dyn Iterator<Item = &Entry>> = match order {
            EntryOrder::OldestFirst => Box::new(self.entries.iter()),
            EntryOrder::NewestFirst => Box::new(self.entries.iter().rev()),
        };
        for entry in iter {
            if !self.matches_entry_query(entry, query) {
                continue;
            }
            results.push(entry.clone());
            if let Some(limit) = query.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let path = self.walk_to_root(Some(start), bounds)?;
        let mut results = Vec::new();
        match order {
            EntryOrder::OldestFirst => {
                for entry in path.into_iter().rev() {
                    let reached_bound = bounds.stop_at_id.as_deref() == Some(entry.id())
                        || bounds
                            .stop_at_type
                            .map(|t| t == entry.entry_type())
                            .unwrap_or(false);
                    if self.matches_entry_query(entry, query) {
                        results.push(entry.clone());
                    }
                    if reached_bound || query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
            EntryOrder::NewestFirst => {
                for entry in path {
                    if self.matches_entry_query(entry, query) {
                        results.push(entry.clone());
                    }
                    if query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    /// Project a validated branch without allocating owned copies for entries
    /// that fall before the latest compaction boundary.
    pub(crate) fn context_on_branch(
        &self,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<crate::session::context::SessionContext> {
        let diagnostics =
            std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0");
        let started = diagnostics.then(std::time::Instant::now);
        let mut path = self.walk_to_root(Some(start), bounds)?;
        path.reverse();
        if bounds.stop_at_id.is_some() || bounds.stop_at_type.is_some() {
            if let Some(end) = path.iter().position(|entry| {
                bounds.stop_at_id.as_deref() == Some(entry.id())
                    || bounds
                        .stop_at_type
                        .map(|kind| kind == entry.entry_type())
                        .unwrap_or(false)
            }) {
                path.truncate(end + 1);
            }
        }
        if let Some(started) = started {
            eprintln!(
                "RPI_CONTEXT_DIAGNOSTICS path_load_ns={} path_entries={}",
                started.elapsed().as_nanos().min(u64::MAX as u128),
                path.len(),
            );
        }
        Ok(crate::session::context::build_session_context_from_borrowed(&path))
    }

    /// Drop the frames a clear has retired: one per streamed delta, each a
    /// parsed JSON value, and never read again because recovery skips what a
    /// clear covers. Left in place they outweigh the transcript several times
    /// over. The session file keeps them, and a reload retires them here too.
    /// Their IDs stay in `used_ids`: the file still holds them, so they stay taken.
    fn retire_frames(&mut self, run_id: &str, stream_index: Option<usize>) {
        let retired = |record: &LaneRecord| {
            matches!(record, LaneRecord::AssistantFrame(frame)
                if frame.op == AssistantFrameOp::Append
                    && frame.run_id == run_id
                    && stream_index.is_none_or(|index| frame.stream_index == index))
        };
        // Everything before the first retired frame keeps its index.
        let Some(first) = self.records.iter().position(retired) else {
            return;
        };
        let mut moved = vec![None; self.records.len() - first];
        let mut kept = first;
        let tail = self.records.split_off(first);
        for (offset, record) in tail.into_iter().enumerate() {
            if !retired(&record) {
                moved[offset] = Some(kept);
                kept += 1;
                self.records.push(record);
            }
        }
        // The log lists records in order, so only its tail can name a moved one.
        let log_tail = self
            .log
            .iter()
            .rposition(|item| matches!(item, StoredLogItem::Record(index) if *index < first))
            .map_or(0, |at| at + 1);
        let mut position = 0;
        self.log.retain_mut(|item| {
            position += 1;
            if position <= log_tail {
                return true;
            }
            let StoredLogItem::Record(index) = item else {
                return true;
            };
            match moved[*index - first] {
                Some(now) => {
                    *index = now;
                    true
                }
                None => false,
            }
        });
    }

    pub fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let mut results = Vec::new();
        // Borrow cancellation identities, not historical message payloads.
        let cancelled: std::collections::HashSet<(&str, &str)> = if query.pending_queue_only {
            self.records
                .iter()
                .filter_map(|record| match record {
                    LaneRecord::QueueCancelled(cancel) => {
                        Some((cancel.base.lane.as_str(), cancel.entry_id.as_str()))
                    }
                    _ => None,
                })
                .collect()
        } else {
            Default::default()
        };
        let iter: Box<dyn Iterator<Item = &LaneRecord>> = match order {
            EntryOrder::OldestFirst => Box::new(self.records.iter()),
            EntryOrder::NewestFirst => Box::new(self.records.iter().rev()),
        };
        for record in iter {
            if !self.matches_record_query(record, query) {
                continue;
            }
            if query.pending_queue_only {
                let LaneRecord::QueueEnqueued(enqueued) = record else {
                    continue;
                };
                let Some(id) = enqueued
                    .target
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if self.entries_by_id.contains_key(id) || cancelled.contains(&(record.lane(), id)) {
                    continue;
                }
            }
            results.push(record.clone());
            if let Some(limit) = query.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn record_page(
        &self,
        query: &crate::session::types::RecordPageQuery,
    ) -> SessionResult<crate::session::types::RecordPage> {
        validate_limit(Some(query.limit))?;
        let matches = |record: &&LaneRecord| {
            record.lane() == query.lane
                && query.after_seq.is_none_or(|seq| record.seq() > seq)
                && query.before_seq.is_none_or(|seq| record.seq() < seq)
        };
        let total = self.records.iter().filter(matches).count();
        let mut records: Vec<_> = if query.from_end {
            self.records
                .iter()
                .rev()
                .filter(matches)
                .take(query.limit)
                .cloned()
                .collect()
        } else {
            self.records
                .iter()
                .filter(matches)
                .take(query.limit)
                .cloned()
                .collect()
        };
        if query.from_end {
            records.reverse();
        }
        Ok(crate::session::types::RecordPage { records, total })
    }

    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        validate_limit(limit)?;
        let mut open: Vec<OperationStartedRecord> = self
            .open_operations_by_lane
            .get(lane)
            .map(|m| m.values().rev().cloned().collect())
            .unwrap_or_default();
        if let Some(limit) = limit {
            open.truncate(limit);
        }
        Ok(open)
    }

    pub(crate) fn has_open_operation_except(
        &self,
        lane: &str,
        except_run_id: Option<&str>,
    ) -> bool {
        self.open_operations_by_lane
            .get(lane)
            .map(|open| {
                open.values()
                    .any(|started| Some(started.base.id.as_str()) != except_run_id)
            })
            .unwrap_or(false)
    }

    pub fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        validate_limit(options.limit)?;
        let mut results = Vec::new();
        for item in &self.log {
            let seq = match item {
                StoredLogItem::Entry(index) => self.entries[*index].seq(),
                StoredLogItem::Record(index) => self.records[*index].seq(),
                StoredLogItem::Other(item) => item.seq(),
            };
            if let Some(after) = options.after_seq {
                if seq <= after {
                    continue;
                }
            }
            results.push(match item {
                StoredLogItem::Entry(index) => LogItem::Entry {
                    seq,
                    entry: self.entries[*index].clone(),
                },
                StoredLogItem::Record(index) => LogItem::Record {
                    seq,
                    record: self.records[*index].clone(),
                },
                StoredLogItem::Other(item) => (**item).clone(),
            });
            if let Some(limit) = options.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|s| s.as_str())
    }

    pub fn get_stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Build the fork mutation list. Mirrors TS `createForkMutations`. The
    /// forked session replays these from `seq=1`. Forked entries keep their
    /// original `id` + `parent_id` chain and pass `lane: None` so lane-leaf
    /// chaining is NOT re-checked (invariant §7).
    pub fn create_fork_mutations(
        &self,
        options: &ForkOptions,
    ) -> SessionResult<Vec<SessionMutation>> {
        let (copied_entries, fork_lanes): (Vec<Entry>, Vec<LanePointer>) = match options {
            ForkOptions::Tree => {
                let entries = self.find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })?;
                let lanes = self.get_lanes();
                (entries, lanes)
            }
            ForkOptions::Branch { entry_id, position } => {
                let selected = match entry_id {
                    Some(id) => Some(id.clone()),
                    None => self.require_lane("main")?,
                };
                let target_id: Option<String> = if let Some(sel) = selected {
                    let mut entry = self.get_entry(&sel).ok_or_else(|| {
                        SessionError::not_found(format!("Entry not found: {sel}"))
                    })?;
                    // An implicit tip (`entry_id: None`) is often a state entry:
                    // a session's newest entry is frequently `model_change` /
                    // `thinking_level_change`, and pi still forks such a session.
                    // Fall back to the nearest message ancestor so `/fork` keeps
                    // working after a `/model` switch. An **explicit** target
                    // that is not a message still errors — the caller asked for
                    // that entry by id.
                    if entry_id.is_none() && !matches!(entry, Entry::Message(_)) {
                        let mut cursor = entry.parent_id().map(str::to_string);
                        let mut steps = 0usize;
                        while let Some(parent) = cursor {
                            entry = self.get_entry(&parent).ok_or_else(|| {
                                SessionError::not_found(format!("Entry not found: {parent}"))
                            })?;
                            if matches!(entry, Entry::Message(_)) {
                                break;
                            }
                            cursor = entry.parent_id().map(str::to_string);
                            steps += 1;
                            if steps > 10_000 {
                                return Err(SessionError::invalid_fork_target(format!(
                                    "Fork target has no message ancestor: {sel}"
                                )));
                            }
                        }
                    }
                    if !matches!(entry, Entry::Message(_)) {
                        return Err(SessionError::invalid_fork_target(format!(
                            "Fork target is not a message entry: {sel}"
                        )));
                    }
                    let position = position.unwrap_or(match entry_id {
                        Some(_) => ForkPosition::Before,
                        None => ForkPosition::At,
                    });
                    match position {
                        ForkPosition::At => Some(entry.id().to_string()),
                        ForkPosition::Before => entry.parent_id().map(|s| s.to_string()),
                    }
                } else {
                    None
                };
                let copied = if let Some(target) = &target_id {
                    self.find_entries_on_branch(
                        &EntryQuery {
                            order: Some(EntryOrder::OldestFirst),
                            ..Default::default()
                        },
                        &BranchBounds::default(),
                        target,
                    )?
                } else {
                    Vec::new()
                };
                let lanes = vec![LanePointer {
                    lane: "main".into(),
                    leaf_id: target_id,
                }];
                (copied, lanes)
            }
        };

        let mut mutations = Vec::new();
        let mut sequence = 1u64;
        for source_entry in &copied_entries {
            // Re-stamp seq (timestamp preserved from source). Fork path: the
            // entry keeps its original id/parent_id; `lane: None` bypasses the
            // lane-leaf chaining check.
            let mut forked = source_entry.clone();
            forked.base_mut().seq = sequence;
            mutations.push(SessionMutation::Entry {
                seq: sequence,
                timestamp: source_entry.base().timestamp,
                lane: None,
                entry: forked,
            });
            sequence += 1;
        }
        for pointer in &fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane: pointer.lane.clone(),
                leaf_id: pointer.leaf_id.clone(),
            });
            sequence += 1;
        }
        if let Some(name) = &self.name {
            mutations.push(SessionMutation::FactName {
                seq: sequence,
                name: Some(name.clone()),
            });
            sequence += 1;
        }
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(entry.id()) {
                mutations.push(SessionMutation::FactLabel {
                    seq: sequence,
                    target_id: entry.id().to_string(),
                    label: Some(label.clone()),
                });
                sequence += 1;
            }
        }
        Ok(mutations)
    }

    // ---- private helpers ----

    fn walk_to_root(
        &self,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<&Entry>> {
        let mut path = Vec::new();
        let mut next_id = start;
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = next_id.take() {
            let current = self
                .get_entry(id)
                .ok_or_else(|| SessionError::not_found(format!("Entry not found: {id}")))?;
            if !visited.insert(current.id()) {
                return Err(SessionError::invalid_entry(format!(
                    "Session branch contains a cycle at {}",
                    current.id()
                )));
            }
            let reached = bounds.stop_at_id.as_deref() == Some(current.id())
                || bounds
                    .stop_at_type
                    .map(|t| t == current.entry_type())
                    .unwrap_or(false)
                || current.parent_id().is_none();
            path.push(current);
            if reached {
                break;
            }
            next_id = current.parent_id();
        }
        Ok(path)
    }

    fn matches_entry_query(&self, entry: &Entry, query: &EntryQuery) -> bool {
        let type_ok = query
            .entry_type
            .map(|t| t == entry.entry_type())
            .unwrap_or(true);
        let custom_ok = query
            .custom_type
            .as_deref()
            .map(|ct| {
                if let Entry::Custom(c) = entry {
                    c.custom_type == ct
                } else {
                    false
                }
            })
            .unwrap_or(true);
        let cursor_ok = query
            .cursor
            .map(|c| {
                let after = c.after_seq;
                match query.order.unwrap_or_default() {
                    EntryOrder::OldestFirst => entry.seq() > after,
                    EntryOrder::NewestFirst => entry.seq() < after,
                }
            })
            .unwrap_or(true);
        let identity_ok = query.entry_id.as_deref().is_none_or(|id| entry.id() == id);
        let outbox_ok = query.outbox_key.as_deref().is_none_or(|key| match entry {
            Entry::Message(message) => match &message.message {
                rpi_agent::AgentMessage::Custom(custom) => {
                    custom
                        .data
                        .get("__rpiOutbox")
                        .and_then(|value| value.get("key"))
                        .and_then(serde_json::Value::as_str)
                        == Some(key)
                }
                _ => false,
            },
            _ => false,
        });
        let tool_ok = query.tool_call_id.as_deref().is_none_or(|id| {
            match entry {
            Entry::Message(entry) => match &entry.message {
                rpi_agent::AgentMessage::Assistant(message) => message.content.iter().any(|block| {
                    matches!(block, rpi_ai::types::Content::ToolCall(call) if call.id == id)
                }),
                rpi_agent::AgentMessage::ToolResult(result) => result.tool_call_id == id,
                _ => false,
            },
            _ => false,
        }
        });
        let boundary_ok = !query.message_boundary_only
            || matches!(entry,
            Entry::Message(entry) if matches!(entry.message, rpi_agent::AgentMessage::Assistant(_) | rpi_agent::AgentMessage::User(_)));
        type_ok && custom_ok && cursor_ok && identity_ok && outbox_ok && tool_ok && boundary_ok
    }

    fn matches_record_query(&self, record: &LaneRecord, query: &RecordQuery) -> bool {
        let lane_ok = query
            .lane
            .as_deref()
            .map(|l| record.lane() == l)
            .unwrap_or(true);
        let type_ok = query
            .record_type
            .map(|t| t == record.record_type())
            .unwrap_or(true);
        let run_ok = query
            .run_id
            .as_deref()
            .map(|run| match record {
                LaneRecord::OperationStarted(s) => s.base.id == run,
                _ => record.run_id().map(|r| r == run).unwrap_or(false),
            })
            .unwrap_or(true);
        let kind_ok = query
            .operation_kind
            .map(|k| {
                matches!(record,
                    LaneRecord::OperationStarted(s) if s.intent.kind() == k)
            })
            .unwrap_or(true);
        let after_ok = query
            .after_seq
            .map(|after| record.seq() > after)
            .unwrap_or(true);
        let queue_ok = query.queue_kind.is_none_or(
            |kind| matches!(record, LaneRecord::QueueEnqueued(enqueued) if enqueued.queue == kind),
        );
        let target_ok = query.target_entry_id.as_deref().is_none_or(|id| {
            matches!(record, LaneRecord::QueueEnqueued(enqueued) if enqueued.target.get("id").and_then(serde_json::Value::as_str) == Some(id))
        });
        let tool_ok = query.tool_call_id.as_deref().is_none_or(
            |id| matches!(record, LaneRecord::ToolStarted(started) if started.tool_call_id == id),
        );
        lane_ok && type_ok && run_ok && kind_ok && after_ok && queue_ok && target_ok && tool_ok
    }
}

/// What `apply_mutation` returned — mirrors the TS pattern where each mutation
/// variant updates different state.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    Entry(Entry),
    Record(LaneRecord),
    Lane,
    Fact,
}

/// Outcome of a fork: the metadata to stamp on the new session.
#[derive(Debug, Clone)]
pub struct ForkResult {
    pub metadata: SessionMetadata,
    pub mutations: Vec<SessionMutation>,
}

// ---- helpers ----

fn validate_limit(limit: Option<usize>) -> SessionResult<()> {
    if let Some(l) = limit {
        if l == 0 {
            return Err(SessionError::invalid_query(
                "limit must be a positive integer",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)] // state introspection helpers for upcoming session_state tests
impl SessionState {
    pub(crate) fn lanes(&self) -> &BTreeMap<String, Option<String>> {
        &self.lanes
    }
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

mod message_queries;
