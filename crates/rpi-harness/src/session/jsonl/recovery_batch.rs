//! Per-sweep reads of pinned recovery entries, in recovery record order.

use super::{read_range_bounded, restore_entry, JsonlPayload, Location};
use crate::{error::SessionResult, session::types::RecoveryEntryCheck};
use std::{ops::Range, sync::Arc};

const MAX_GAP: u64 = 8 * 1024;
const MAX_WINDOW: u64 = 256 * 1024;

fn cold_source(payload: &Option<JsonlPayload>) -> Option<&Location> {
    match payload {
        Some(JsonlPayload::Cold { source, .. }) => Some(source),
        _ => None,
    }
}

fn span(location: &Location) -> Option<Range<u64>> {
    Some(location.offset..location.offset.checked_add(location.length as u64)?)
}

fn window(first: &Location, following: &[Option<JsonlPayload>]) -> Option<(Range<u64>, usize)> {
    let mut range = span(first)?;
    let mut count = 1;
    for (index, payload) in following.iter().enumerate() {
        let Some(next) = cold_source(payload) else {
            continue;
        };
        if !Arc::ptr_eq(&first.reader, &next.reader) {
            break;
        }
        let Some(next) = span(next) else {
            break;
        };
        let combined = range.start.min(next.start)..range.end.max(next.end);
        // Recovery records can reference physical rows in either direction.
        let gap = next
            .start
            .max(range.start)
            .saturating_sub(next.end.min(range.end));
        if gap > MAX_GAP || combined.end - combined.start > MAX_WINDOW {
            break;
        }
        range = combined;
        count = index + 2;
    }
    (count > 1).then_some((range, count))
}

async fn settle_one(
    check: RecoveryEntryCheck<'_>,
    payload: Option<&JsonlPayload>,
) -> SessionResult<bool> {
    let Some(payload) = payload else {
        return Ok(false);
    };
    match check {
        RecoveryEntryCheck::DeferredWrite(_) => {
            if let JsonlPayload::Cold { .. } = payload {
                payload.read().await?;
            }
            Ok(true)
        }
        RecoveryEntryCheck::ToolResult { .. } => {
            Ok(check.matches_tool(payload.tool_result_identity().await?))
        }
    }
}

pub(in crate::session::jsonl) async fn read(
    checks: &[RecoveryEntryCheck<'_>],
    selected: &[Option<JsonlPayload>],
) -> SessionResult<Vec<bool>> {
    let mut settled = Vec::with_capacity(checks.len());
    while settled.len() < checks.len() {
        let index = settled.len();
        let group = cold_source(&selected[index]).and_then(|first| {
            window(first, &selected[index + 1..]).map(|(range, count)| (first, range, count))
        });
        let Some((first, range, count)) = group else {
            settled.push(settle_one(checks[index], selected[index].as_ref()).await?);
            continue;
        };

        let length = (range.end - range.start) as usize;
        let rows = &selected[index..index + count];
        let checks = &checks[index..index + count];
        let bytes = read_range_bounded(&first.reader, range.start, length).await;
        let Some(bytes) = bytes.ok().filter(|bytes| bytes.len() == length) else {
            // The range can fail because of a gap or a later row. Retry the
            // original reads in order so the first selected-row error wins.
            for (check, payload) in checks.iter().zip(rows) {
                settled.push(settle_one(*check, payload.as_ref()).await?);
            }
            continue;
        };

        for (check, payload) in checks.iter().zip(rows) {
            let Some(JsonlPayload::Cold {
                source,
                parent,
                live_nulls,
            }) = payload
            else {
                settled.push(settle_one(*check, payload.as_ref()).await?);
                continue;
            };
            let offset = (source.offset - range.start) as usize;
            let raw = &bytes[offset..offset + source.length];
            settled.push(match check {
                RecoveryEntryCheck::DeferredWrite(_) => {
                    restore_entry(source.decode(raw)?, parent, live_nulls)?;
                    true
                }
                RecoveryEntryCheck::ToolResult { .. } => {
                    check.matches_tool(source.decode_tool_result_identity(raw)?)
                }
            });
        }
    }
    Ok(settled)
}
