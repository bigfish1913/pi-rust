//! Private bounded record view, not a SessionStorage/writer or persisted cache.
use super::{NativeRunError, MAX_NATIVE_RECORDS, MAX_NATIVE_RECORD_BYTES};
use crate::{
    agent_harness::NativeExecutionStorage,
    durable::{experimental_ledger::source::SourceError, validate_run_id},
    error::{SessionError, SessionResult},
    session::types::*,
};
use std::{
    collections::HashSet,
    io::{self, Write},
    sync::Arc,
};
const PAGE: usize = 256;

pub(super) struct HistoricalRecords {
    storage: Arc<dyn SessionStorage>,
    lane: String,
    leaf: Option<String>,
    revision: u64,
    generation: u64,
    records: Vec<LaneRecord>,
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_NATIVE_RECORD_BYTES.saturating_sub(self.0) {
            return Err(io::Error::other("native record serialization budget"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn capture(
    storage: Arc<dyn SessionStorage>,
    lane: &str,
    leaf: Option<&str>,
    revision: u64,
    generation: u64,
) -> Result<HistoricalRecords, NativeRunError> {
    let mut records = Vec::new();
    let mut after = None;
    let mut remaining = None;
    let mut ids = HashSet::new();
    let mut bytes = Counter(0);
    loop {
        let page = storage
            .record_page(&RecordPageQuery {
                lane: lane.into(),
                after_seq: after,
                // None is exactly the unbounded upper limit when R == u64::MAX.
                before_seq: revision.checked_add(1),
                limit: PAGE,
                from_end: false,
            })
            .await
            .map_err(|_| SourceError::Read)?;
        if page.total > MAX_NATIVE_RECORDS.saturating_sub(records.len()) {
            return Err(SourceError::Limit.into());
        }
        if remaining.is_some_and(|expected| expected != page.total)
            || page.records.len() != page.total.min(PAGE)
        {
            return Err(NativeRunError::Records);
        }
        remaining = Some(page.total - page.records.len());
        for record in page.records {
            if record.lane() != lane
                || record.seq() == 0
                || record.seq() > revision
                || after.is_some_and(|seq| record.seq() <= seq)
                || validate_run_id(record.id()).is_err()
                || !ids.insert(record.id().to_owned())
            {
                return Err(NativeRunError::Records);
            }
            serde_json::to_writer(&mut bytes, &record).map_err(|_| SourceError::Limit)?;
            after = Some(record.seq());
            records.push(record);
        }
        if remaining == Some(0) {
            break;
        }
    }
    Ok(HistoricalRecords {
        storage,
        lane: lane.into(),
        leaf: leaf.map(str::to_owned),
        revision,
        generation,
        records,
    })
}
fn unsupported<T>() -> SessionResult<T> {
    Err(SessionError::storage(
        "unsupported historical native proof query",
    ))
}
#[async_trait::async_trait]
impl NativeExecutionStorage for HistoricalRecords {
    async fn history_identity(&self) -> SessionResult<crate::session::types::HistoryIdentity> {
        Ok(crate::session::types::HistoryIdentity {
            revision: self.revision,
            generation: self.generation,
        })
    }
    async fn find_records(&self, q: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        // This deliberately supports only the native proof's current contract.
        // New proof query shapes must be implemented, never silently weakened.
        if q.lane.as_deref() != Some(&self.lane)
            || q.run_id.is_none()
            || !matches!(
                q.record_type,
                Some("operation_started" | "operation_finished" | "tool_started")
            )
            || !matches!(q.limit, Some(1..=65))
            || q.operation_kind.is_some()
            || q.queue_kind.is_some()
            || q.tool_call_id.is_some()
            || q.target_entry_id.is_some()
            || q.pending_queue_only
            || q.after_seq.is_some()
        {
            return unsupported();
        }
        let matches = |r: &&LaneRecord| {
            let run = if matches!(r, LaneRecord::OperationStarted(_)) {
                Some(r.id())
            } else {
                r.run_id()
            };
            Some(r.record_type()) == q.record_type && run == q.run_id.as_deref()
        };
        let limit = q.limit.unwrap();
        Ok(match q.order.unwrap_or_default() {
            EntryOrder::OldestFirst => self
                .records
                .iter()
                .filter(matches)
                .take(limit)
                .cloned()
                .collect(),
            EntryOrder::NewestFirst => self
                .records
                .iter()
                .rev()
                .filter(matches)
                .take(limit)
                .cloned()
                .collect(),
        })
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        if self.leaf.as_deref() != Some(start)
            || query.limit != Some(1)
            || bounds.start.is_some()
            || bounds.stop_at_id.is_some()
            || bounds.stop_at_type.is_some()
        {
            return unsupported();
        }
        let entries = self
            .storage
            .find_entries_on_branch(query, bounds, start)
            .await?;
        if entries.len() > 1
            || entries
                .iter()
                .any(|e| e.seq() == 0 || e.seq() > self.revision)
        {
            return unsupported();
        }
        Ok(entries)
    }
}
