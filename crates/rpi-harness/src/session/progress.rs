//! Transient frame payloads, independent of the immutable session history.
use std::collections::{HashMap, HashSet};

use super::types::{AssistantFrameOp, AssistantFrameRecord};
use crate::error::{SessionError, SessionResult};
use serde::{Deserialize, Serialize};

pub(crate) type CompletedProgressRuns = HashMap<String, HashSet<String>>;

fn is_completed(frame: &AssistantFrameRecord, completed: &CompletedProgressRuns) -> bool {
    completed
        .get(&frame.base.lane)
        .is_some_and(|runs| runs.contains(&frame.run_id))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgressState {
    pub sequence: u64,
    pub frames: Vec<AssistantFrameRecord>,
    #[serde(skip)]
    ids: HashSet<String>,
}

impl ProgressState {
    pub fn at_sequence(sequence: u64) -> Self {
        Self {
            sequence,
            ..Default::default()
        }
    }
    /// Validate the whole batch without cloning the accumulated payloads.
    pub fn prepare(
        &self,
        records: Vec<AssistantFrameRecord>,
        mut now: impl FnMut() -> i64,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        let mut ids = HashSet::with_capacity(records.len());
        for record in &records {
            if record.base.id.is_empty() || record.base.lane.is_empty() || record.run_id.is_empty()
            {
                return Err(SessionError::invalid_payload(
                    "progress identity must not be empty",
                ));
            }
            if self.ids.contains(&record.base.id) || !ids.insert(&record.base.id) {
                return Err(SessionError::invalid_payload("duplicate progress id"));
            }
            match record.op {
                AssistantFrameOp::Append if record.frame.is_some() => {}
                AssistantFrameOp::ClearRun | AssistantFrameOp::ClearStream
                    if record.frame.is_none() => {}
                _ => {
                    return Err(SessionError::invalid_payload(
                        "invalid progress operation; bindings belong to history",
                    ))
                }
            }
        }
        let mut sequence = self.sequence;
        records
            .into_iter()
            .map(|mut record| {
                sequence = sequence
                    .checked_add(1)
                    .ok_or_else(|| SessionError::storage("progress sequence overflow"))?;
                record.base.seq = sequence;
                record.base.timestamp = now();
                Ok(record)
            })
            .collect()
    }

    pub fn apply(&mut self, records: &[AssistantFrameRecord]) {
        for record in records {
            self.sequence = record.base.seq;
            match record.op {
                AssistantFrameOp::Append => {
                    self.ids.insert(record.base.id.clone());
                    self.frames.push(record.clone());
                }
                AssistantFrameOp::ClearStream | AssistantFrameOp::ClearRun => {
                    let ids = &mut self.ids;
                    self.frames.retain(|frame| {
                        let keep = frame.run_id != record.run_id
                            || frame.base.lane != record.base.lane
                            || (record.op == AssistantFrameOp::ClearStream
                                && frame.stream_index != record.stream_index);
                        if !keep {
                            ids.remove(&frame.base.id);
                        }
                        keep
                    });
                    if self.frames.capacity() > self.frames.len().saturating_mul(2) {
                        self.frames.shrink_to_fit();
                    }
                    if self.ids.capacity() > self.ids.len().saturating_mul(2) {
                        self.ids.shrink_to_fit();
                    }
                }
                AssistantFrameOp::CommitStream => unreachable!("validated progress batch"),
            }
        }
    }

    /// Build the checkpoint candidate without cloning payloads being retired.
    pub fn project(&self, records: &[AssistantFrameRecord]) -> Self {
        let frames: Vec<_> = self
            .frames
            .iter()
            .filter(|frame| {
                !records.iter().any(|record| {
                    frame.run_id == record.run_id
                        && frame.base.lane == record.base.lane
                        && (record.op == AssistantFrameOp::ClearRun
                            || (record.op == AssistantFrameOp::ClearStream
                                && frame.stream_index == record.stream_index))
                })
            })
            .cloned()
            .collect();
        let ids = frames.iter().map(|frame| frame.base.id.clone()).collect();
        let mut projected = Self {
            sequence: self.sequence,
            frames,
            ids,
        };
        projected.apply(records);
        projected
    }

    pub fn prune_completed(&mut self, completed: &CompletedProgressRuns) -> usize {
        let before = self.frames.len();
        let ids = &mut self.ids;
        self.frames.retain(|frame| {
            if is_completed(frame, completed) {
                ids.remove(&frame.base.id);
                false
            } else {
                true
            }
        });
        if self.frames.capacity() > self.frames.len().saturating_mul(2) {
            self.frames.shrink_to_fit();
        }
        if self.ids.capacity() > self.ids.len().saturating_mul(2) {
            self.ids.shrink_to_fit();
        }
        before - self.frames.len()
    }

    /// No candidate or payload clones when there is nothing to reclaim.
    pub fn project_completed(&self, completed: &CompletedProgressRuns) -> Option<Self> {
        if !self
            .frames
            .iter()
            .any(|frame| is_completed(frame, completed))
        {
            return None;
        }
        let frames: Vec<_> = self
            .frames
            .iter()
            .filter(|frame| !is_completed(frame, completed))
            .cloned()
            .collect();
        let ids = frames.iter().map(|frame| frame.base.id.clone()).collect();
        Some(Self {
            sequence: self.sequence,
            frames,
            ids,
        })
    }

    pub fn read(&self, run_id: &str) -> Vec<AssistantFrameRecord> {
        self.frames
            .iter()
            .filter(|frame| frame.run_id == run_id)
            .cloned()
            .collect()
    }

    pub fn validate(&mut self) -> SessionResult<()> {
        let mut previous = 0;
        let mut ids = HashSet::new();
        for frame in &self.frames {
            if frame.op != AssistantFrameOp::Append
                || frame.frame.is_none()
                || frame.base.seq <= previous
                || frame.base.seq > self.sequence
                || frame.base.id.is_empty()
                || frame.base.lane.is_empty()
                || frame.run_id.is_empty()
                || !ids.insert(&frame.base.id)
            {
                return Err(SessionError::invalid_entry("invalid progress checkpoint"));
            }
            previous = frame.base.seq;
        }
        self.ids = ids.into_iter().cloned().collect();
        Ok(())
    }
}
