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
    BranchBounds, Entry, EntryOrder, EntryQuery, ForkOptions, ForkPosition, LanePointer,
    LaneRecord, LogItem, LogOptions, OperationStartedRecord, RecordQuery, SessionMetadata,
    SessionMutation, SessionStats,
};

/// The in-memory mutation reducer. Mirrors TS `SessionState`. Owns the
/// append-only entry/record logs, the lane→leaf map, open operations per lane,
/// global name/labels facts, and running stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    sequence: u64,
    used_ids: std::collections::HashSet<String>,
    entries: Vec<Entry>,
    entries_by_id: HashMap<String, Entry>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>>,
    lanes: BTreeMap<String, Option<String>>,
    log: Vec<LogItem>,
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

                let is_message = matches!(entry, Entry::Message(_));
                self.sequence = seq;
                self.used_ids.insert(entry.id().to_string());
                self.entries.push(entry.clone());
                self.entries_by_id
                    .insert(entry.id().to_string(), entry.clone());
                if let Some(lane_name) = lane.as_deref() {
                    if let Some(slot) = self.lanes.get_mut(lane_name) {
                        *slot = Some(entry.id().to_string());
                    }
                }
                self.log.push(LogItem::Entry {
                    seq,
                    entry: entry.clone(),
                });
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
                self.records.push(record.clone());
                self.log.push(LogItem::Record {
                    seq,
                    record: record.clone(),
                });
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
                self.log.push(LogItem::Lane { seq, lane, leaf_id });
                Ok(ApplyOutcome::Lane)
            }
            SessionMutation::FactName { seq, name } => {
                self.sequence = seq;
                self.name = name.clone();
                self.log.push(LogItem::FactName { seq, name });
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
                self.log.push(LogItem::FactLabel {
                    seq,
                    target_id,
                    label,
                });
                Ok(ApplyOutcome::Fact)
            }
        }
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries_by_id.get(id)
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
        let path = self.walk_to_root_owned(Some(start), bounds)?;
        let mut results = Vec::new();
        match order {
            EntryOrder::OldestFirst => {
                for entry in path.into_iter().rev() {
                    let reached_bound = bounds.stop_at_id.as_deref() == Some(entry.id())
                        || bounds
                            .stop_at_type
                            .map(|t| t == entry.entry_type())
                            .unwrap_or(false);
                    if self.matches_entry_query(&entry, query) {
                        results.push(entry);
                    }
                    if reached_bound || query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
            EntryOrder::NewestFirst => {
                for entry in path {
                    if self.matches_entry_query(&entry, query) {
                        results.push(entry);
                    }
                    if query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    pub fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        validate_limit(query.limit)?;
        let order = query.order.unwrap_or_default();
        let mut results = Vec::new();
        let iter: Box<dyn Iterator<Item = &LaneRecord>> = match order {
            EntryOrder::OldestFirst => Box::new(self.records.iter()),
            EntryOrder::NewestFirst => Box::new(self.records.iter().rev()),
        };
        for record in iter {
            if !self.matches_record_query(record, query) {
                continue;
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

    pub fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        validate_limit(options.limit)?;
        let mut results = Vec::new();
        for item in &self.log {
            if let Some(after) = options.after_seq {
                if item.seq() <= after {
                    continue;
                }
            }
            results.push(item.clone());
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
                    let entry = self.get_entry(&sel).ok_or_else(|| {
                        SessionError::not_found(format!("Entry not found: {sel}"))
                    })?;
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

    fn walk_to_root_owned(
        &self,
        start: Option<&str>,
        bounds: &BranchBounds,
    ) -> SessionResult<Vec<Entry>> {
        let mut path = Vec::new();
        let mut next_id = start.map(|s| s.to_string());
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = next_id.take() {
            let current = self
                .entries_by_id
                .get(&id)
                .ok_or_else(|| SessionError::not_found(format!("Entry not found: {id}")))?
                .clone();
            if visited.contains(&current.id().to_string()) {
                return Err(SessionError::invalid_entry(format!(
                    "Session branch contains a cycle at {}",
                    current.id()
                )));
            }
            visited.insert(current.id().to_string());
            let reached = bounds.stop_at_id.as_deref() == Some(current.id())
                || bounds
                    .stop_at_type
                    .map(|t| t == current.entry_type())
                    .unwrap_or(false)
                || current.parent_id().is_none();
            path.push(current.clone());
            if reached {
                break;
            }
            next_id = current.parent_id().map(|s| s.to_string());
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
        type_ok && custom_ok && cursor_ok
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
        lane_ok && type_ok && run_ok && kind_ok && after_ok
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
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }
    pub(crate) fn lanes(&self) -> &BTreeMap<String, Option<String>> {
        &self.lanes
    }
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }
}
