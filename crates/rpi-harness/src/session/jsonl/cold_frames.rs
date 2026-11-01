//! JSONL-private cold records. Canonical bytes stay on disk; query plans own
//! their source generation and never publish partial or shared mutable payloads.
use super::{codec::parse_mutation, errors::file_result};
use crate::{
    error::{SessionError, SessionResult},
    session::{
        state::SessionState,
        types::{
            AssistantFrameOp, AssistantFrameRecord, EntryOrder, LaneRecord, LogItem, LogOptions,
            RecordPage, RecordPageQuery, RecordQuery, SessionMutation,
        },
    },
};
use rpi_tools::env::ReadFile;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

struct Location {
    seq: u64,
    offset: u64,
    length: usize,
    scope: usize,
    digest: [u8; 32],
}
#[derive(Default)]
struct ColdFrames {
    reader: Option<Arc<dyn ReadFile>>,
    locations: Vec<Location>,
    scopes: Vec<(String, String)>,
    scope_ids: HashMap<(String, String), usize>,
}
#[derive(Clone, Default)]
pub(super) struct ResidentState {
    reducer: SessionState,
    cold: Arc<ColdFrames>,
}
impl Deref for ResidentState {
    type Target = SessionState;
    fn deref(&self) -> &SessionState {
        &self.reducer
    }
}
impl DerefMut for ResidentState {
    fn deref_mut(&mut self) -> &mut SessionState {
        &mut self.reducer
    }
}
impl ResidentState {
    pub fn defer(
        &mut self,
        frame: &AssistantFrameRecord,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        line: &str,
    ) -> SessionResult<()> {
        self.reducer.reserve_cold_frame(frame)?;
        let cold = Arc::get_mut(&mut self.cold)
            .ok_or_else(|| SessionError::storage("cold history already published"))?;
        cold.reader = Some(reader.clone());
        let scope = (frame.base.lane.clone(), frame.run_id.clone());
        let next = cold.scopes.len();
        let scope_id = *cold.scope_ids.entry(scope.clone()).or_insert_with(|| {
            cold.scopes.push(scope);
            next
        });
        cold.locations.push(Location {
            seq: frame.base.seq,
            offset,
            length: line.len(),
            scope: scope_id,
            digest: Sha256::digest(line.as_bytes()).into(),
        });
        Ok(())
    }
    pub fn record_plan(&self, query: &RecordQuery) -> SessionResult<RecordPlan> {
        // Let the existing reducer validate limit/filter semantics first.
        let hot = self.reducer.find_records(query)?;
        if query
            .record_type
            .is_some_and(|kind| kind != "assistant_frame")
            || query.operation_kind.is_some()
            || query.queue_kind.is_some()
            || query.target_entry_id.is_some()
            || query.tool_call_id.is_some()
            || query.pending_queue_only
        {
            return Ok(RecordPlan {
                selected: hot.into_iter().map(Selected::Hot).collect(),
                cold: self.cold.clone(),
            });
        }
        let oldest = query.order == Some(EntryOrder::OldestFirst);
        let mut cold: Box<dyn Iterator<Item = (usize, &Location)> + '_> = if oldest {
            Box::new(self.cold.locations.iter().enumerate())
        } else {
            Box::new(self.cold.locations.iter().enumerate().rev())
        };
        let mut selected: Vec<_> = hot.into_iter().map(Selected::Hot).collect();
        selected.extend(
            cold.by_ref()
                .filter(|(_, loc)| {
                    let (lane, run) = &self.cold.scopes[loc.scope];
                    query.lane.as_ref().is_none_or(|v| v == lane)
                        && query.run_id.as_ref().is_none_or(|v| v == run)
                        && query.after_seq.is_none_or(|v| loc.seq > v)
                })
                .take(query.limit.unwrap_or(usize::MAX))
                .map(|(index, _)| Selected::Cold(index)),
        );
        selected.sort_unstable_by_key(|item| item.seq(&self.cold, LaneRecord::seq));
        if !oldest {
            selected.reverse();
        }
        selected.truncate(query.limit.unwrap_or(usize::MAX));
        Ok(RecordPlan {
            selected,
            cold: self.cold.clone(),
        })
    }
    pub fn page_plan(&self, query: &RecordPageQuery) -> SessionResult<(RecordPlan, usize)> {
        let hot = self.reducer.record_page(query)?;
        let matching = |loc: &&Location| {
            self.cold.scopes[loc.scope].0 == query.lane
                && query.after_seq.is_none_or(|v| loc.seq > v)
                && query.before_seq.is_none_or(|v| loc.seq < v)
        };
        let total = hot.total + self.cold.locations.iter().filter(matching).count();
        let candidates: Box<dyn Iterator<Item = (usize, &Location)> + '_> = if query.from_end {
            Box::new(self.cold.locations.iter().enumerate().rev())
        } else {
            Box::new(self.cold.locations.iter().enumerate())
        };
        let mut selected: Vec<_> = hot.records.into_iter().map(Selected::Hot).collect();
        selected.extend(
            candidates
                .filter(|(_, loc)| matching(loc))
                .take(query.limit)
                .map(|(i, _)| Selected::Cold(i)),
        );
        selected.sort_unstable_by_key(|item| item.seq(&self.cold, LaneRecord::seq));
        if query.from_end {
            let start = selected.len().saturating_sub(query.limit);
            selected.drain(..start);
        } else {
            selected.truncate(query.limit);
        }
        Ok((
            RecordPlan {
                selected,
                cold: self.cold.clone(),
            },
            total,
        ))
    }
    pub fn log_plan(&self, options: &LogOptions) -> SessionResult<LogPlan> {
        let mut selected: Vec<_> = self
            .reducer
            .get_log(options)?
            .into_iter()
            .map(Selected::Hot)
            .collect();
        selected.extend(
            self.cold
                .locations
                .iter()
                .enumerate()
                .filter(|(_, loc)| options.after_seq.is_none_or(|v| loc.seq > v))
                .take(options.limit.unwrap_or(usize::MAX))
                .map(|(i, _)| Selected::Cold(i)),
        );
        selected.sort_unstable_by_key(|item| item.seq(&self.cold, LogItem::seq));
        selected.truncate(options.limit.unwrap_or(usize::MAX));
        Ok(LogPlan {
            selected,
            cold: self.cold.clone(),
        })
    }
}

enum Selected<T> {
    Hot(T),
    Cold(usize),
}
impl<T> Selected<T> {
    fn seq(&self, cold: &ColdFrames, hot: impl FnOnce(&T) -> u64) -> u64 {
        match self {
            Self::Hot(value) => hot(value),
            Self::Cold(index) => cold.locations[*index].seq,
        }
    }
}
pub(super) struct RecordPlan {
    selected: Vec<Selected<LaneRecord>>,
    cold: Arc<ColdFrames>,
}
pub(super) struct LogPlan {
    selected: Vec<Selected<LogItem>>,
    cold: Arc<ColdFrames>,
}
impl RecordPlan {
    pub async fn read(self) -> SessionResult<Vec<LaneRecord>> {
        let mut frames = self.cold.read_selected(&self.selected).await?;
        self.selected
            .into_iter()
            .map(|item| match item {
                Selected::Hot(value) => Ok(value),
                Selected::Cold(index) => frames
                    .remove(&index)
                    .ok_or_else(|| SessionError::storage("missing cold record")),
            })
            .collect()
    }
    pub async fn page(self, total: usize) -> SessionResult<RecordPage> {
        Ok(RecordPage {
            records: self.read().await?,
            total,
        })
    }
}
impl LogPlan {
    pub async fn read(self) -> SessionResult<Vec<LogItem>> {
        let mut frames = self.cold.read_selected(&self.selected).await?;
        self.selected
            .into_iter()
            .map(|item| match item {
                Selected::Hot(value) => Ok(value),
                Selected::Cold(index) => frames
                    .remove(&index)
                    .map(|record| LogItem::Record {
                        seq: record.seq(),
                        record,
                    })
                    .ok_or_else(|| SessionError::storage("missing cold log record")),
            })
            .collect()
    }
}
impl ColdFrames {
    async fn read_selected<T>(
        &self,
        selected: &[Selected<T>],
    ) -> SessionResult<HashMap<usize, LaneRecord>> {
        let mut indexes: Vec<_> = selected
            .iter()
            .filter_map(|item| match item {
                Selected::Cold(i) => Some(*i),
                _ => None,
            })
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        let mut records = HashMap::with_capacity(indexes.len());
        if indexes.is_empty() {
            return Ok(records);
        }
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| SessionError::storage("missing cold history reader"))?;
        let mut start = 0;
        while start < indexes.len() {
            let first = &self.locations[indexes[start]];
            let mut end = start + 1;
            let mut range_end = first.offset + first.length as u64;
            // Coalesce nearby rows, without an unbounded warm payload cache.
            while end < indexes.len() {
                let next = &self.locations[indexes[end]];
                let next_end = next.offset + next.length as u64;
                if next.offset.saturating_sub(range_end) > 8192
                    || next_end - first.offset > 256 * 1024
                {
                    break;
                }
                range_end = next_end;
                end += 1;
            }
            let length = usize::try_from(range_end - first.offset)
                .map_err(|_| SessionError::storage("cold history range overflow"))?;
            let bytes = file_result(
                reader.read_range(first.offset, length).await,
                "read cold history range",
            )?;
            if bytes.len() != length {
                return Err(SessionError::storage("short cold history read"));
            }
            for &index in &indexes[start..end] {
                let loc = &self.locations[index];
                let offset = (loc.offset - first.offset) as usize;
                let raw = &bytes[offset..offset + loc.length];
                if <[u8; 32]>::from(Sha256::digest(raw)) != loc.digest {
                    return Err(SessionError::invalid_entry(
                        "cold history source changed; reopen session",
                    ));
                }
                let line = std::str::from_utf8(raw)
                    .map_err(|_| SessionError::invalid_entry("invalid cold history UTF-8"))?;
                let mutation = parse_mutation(line)
                    .map_err(|e| SessionError::invalid_entry(format!("invalid cold frame: {e}")))?;
                let SessionMutation::Record {
                    record: LaneRecord::AssistantFrame(frame),
                } = mutation
                else {
                    return Err(SessionError::invalid_entry(
                        "cold history record identity mismatch",
                    ));
                };
                let (lane, run) = &self.scopes[loc.scope];
                if frame.base.seq != loc.seq
                    || &frame.base.lane != lane
                    || &frame.run_id != run
                    || frame.op == AssistantFrameOp::CommitStream
                {
                    return Err(SessionError::invalid_entry(
                        "cold history frame identity mismatch",
                    ));
                }
                records.insert(index, LaneRecord::AssistantFrame(frame));
            }
            start = end;
        }
        Ok(records)
    }
}

#[cfg(test)]
#[path = "cold_frames_tests.rs"]
mod tests;
