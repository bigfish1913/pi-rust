//! Cooperative serialization and prospective branch limits, before publication.
use super::{ledger, Loaded};
use crate::{
    durable::DurableError,
    session::types::{CustomEntry, Entry, EntryBase},
};
use serde::Serialize;
use std::io::{self, Write};

struct Counter {
    count: usize,
    limit: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.count) {
            return Err(io::Error::other(
                "experimental serialization budget exceeded",
            ));
        }
        self.count += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_len(value: &impl Serialize, limit: usize) -> Result<usize, DurableError> {
    let mut counter = Counter { count: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| DurableError::LimitExceeded)?;
    Ok(counter.count)
}

/// Count the complete serialization before allocating its output buffer.
pub(super) fn bounded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, DurableError> {
    let length = encoded_len(value, limit)?;
    let mut output = Vec::with_capacity(length);
    serde_json::to_writer(&mut output, value).map_err(|_| super::invalid())?;
    if output.len() != length {
        return Err(super::invalid());
    }
    Ok(output)
}

fn add(old: usize, more: usize, limit: usize) -> Result<(), DurableError> {
    old.checked_add(more)
        .filter(|sum| *sum <= limit)
        .map(|_| ())
        .ok_or(DurableError::LimitExceeded)
}

/// Run AFTER exact replay detection: a journal at its limit is still readable
/// and an identical current-anchor request still replays without appending.
pub(super) fn append_counts(loaded: &Loaded) -> Result<(), DurableError> {
    use ledger::{chain::MAX_CHAIN_RECORDS, source::*};
    add(loaded.source.inspected_entries, 1, MAX_PATH_ENTRIES)?;
    // Count unrelated custom entries too, not only control records.
    add(
        loaded.source.inspected_custom_entries,
        1,
        MAX_CUSTOM_ENTRIES,
    )?;
    add(loaded.source.records.len(), 1, MAX_CHAIN_RECORDS)?;
    loaded
        .snapshot
        .scope
        .history_revision
        .checked_add(1)
        .ok_or(DurableError::LimitExceeded)?;
    Ok(())
}

pub(super) fn prospective(
    loaded: &Loaded,
    record: &ledger::Record,
    data: serde_json::Value,
) -> Result<serde_json::Value, DurableError> {
    append_counts(loaded)?;
    let mut nodes = record.nodes.len();
    for item in &loaded.source.records {
        nodes = nodes
            .checked_add(item.candidate.record().nodes.len())
            .ok_or(DurableError::LimitExceeded)?;
    }
    if nodes > ledger::chain::MAX_CHAIN_NODES {
        return Err(DurableError::LimitExceeded);
    }
    // Count the serialized Entry, not just its data. The extremal integer widths
    // dominate any honest assigned seq <= captured R+1 and any i64 timestamp.
    let envelope = Entry::Custom(CustomEntry {
        base: EntryBase {
            entry_type: "custom".into(),
            id: record.receipt.entry_id.clone(),
            seq: u64::MAX,
            parent_id: loaded.snapshot.scope.leaf_id.clone(),
            timestamp: i64::MIN,
        },
        custom_type: ledger::CUSTOM_TYPE.into(),
        data: Some(data),
    });
    let remaining = ledger::source::MAX_BRANCH_BYTES
        .checked_sub(loaded.source.encoded_bytes)
        .ok_or(DurableError::LimitExceeded)?;
    encoded_len(&envelope, remaining)?;
    match envelope {
        Entry::Custom(entry) => entry.data.ok_or_else(super::invalid),
        _ => Err(super::invalid()),
    }
}
