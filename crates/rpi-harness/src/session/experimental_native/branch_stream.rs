//! Bounded branch selection: retain only matching sequence keys,
//! but validate the entire selected metadata path even after filling the page.
//! This bounds retained selection state, NOT traversal work or SQLite I/O/RSS.
use super::*;
use std::collections::VecDeque;

fn maximum(q: &EntryQuery) -> Result<usize> {
    let maximum = q
        .limit
        .ok_or_else(|| SessionError::invalid_query("bounded limit required"))?;
    limit(Some(maximum))?;
    Ok(maximum)
}

fn walk(
    db: &Connection,
    identity: &Identity,
    start: &str,
    bounds: &BranchBounds,
    mut visit: impl FnMut(&str, &Meta, bool),
) -> Result<()> {
    let mut next = Some(start.to_owned());
    let mut child_seq = None;
    while let Some(id) = next {
        let (seq, meta) = entry_at(db, identity, &id)?
            .ok_or_else(|| SessionError::not_found("branch entry missing"))?;
        // Every committed entry parent is older. Strict descent rejects cycles
        // and forward/self links without retaining a depth-sized visited set.
        // Do this even for nonmatches and after the result limit is filled.
        engine::ensure(
            seq > 0 && meta.seq == seq && child_seq.is_none_or(|child| seq < child),
            "branch parent sequence must strictly decrease",
        )?;
        let Effect::Entry {
            id: actual, parent, ..
        } = &meta.effect
        else {
            return Err("invalid parent metadata".into());
        };
        engine::ensure(actual == &id, "branch entry identity mismatch")?;
        let stop = bounds.stop_at_id.as_deref() == Some(id.as_str())
            || bounds
                .stop_at_type
                .is_some_and(|t| meta.selection.entry_type.as_deref() == Some(t));
        visit(&id, &meta, stop);
        if stop {
            break;
        }
        next = parent.clone();
        child_seq = Some(seq);
    }
    Ok(())
}

fn keys(
    db: &Connection,
    identity: &Identity,
    q: &EntryQuery,
    start: &str,
    bounds: &BranchBounds,
) -> Result<Vec<u64>> {
    let maximum = maximum(q)?;
    if q.order == Some(EntryOrder::OldestFirst) {
        return Err(SessionError::invalid_query("newest-first branch selection required").into());
    }
    let mut selected = Vec::new();
    walk(db, identity, start, bounds, |id, meta, _| {
        if selected.len() < maximum && meta.selection.entry_matches(id, meta.seq, q) {
            selected.push(meta.seq);
        }
    })?;
    Ok(selected)
}

fn oldest_keys(
    db: &Connection,
    identity: &Identity,
    q: &EntryQuery,
    start: &str,
    bounds: &BranchBounds,
) -> Result<VecDeque<u64>> {
    let maximum = maximum(q)?;
    if q.order != Some(EntryOrder::OldestFirst) {
        return Err(SessionError::invalid_query("oldest-first branch selection required").into());
    }
    let mut selected = VecDeque::new();
    walk(db, identity, start, bounds, |id, meta, stop| {
        // Preserve the existing oldest-first inclusive-bound behavior: when
        // the reverse walk reaches an explicit stop, ONLY that node remains
        // a candidate, but all query filters/cursor still apply to that node.
        if stop {
            selected.clear();
        }
        if meta.selection.entry_matches(id, meta.seq, q) {
            if selected.len() == maximum {
                selected.pop_back();
            }
            selected.push_front(meta.seq);
        }
    })?;
    Ok(selected)
}

fn materialize(
    db: &Connection,
    identity: &Identity,
    keys: impl IntoIterator<Item = u64>,
) -> Result<Vec<Entry>> {
    keys.into_iter()
        .map(|seq| super::super::queries::entry(db, identity, seq))
        .collect()
}

pub(super) fn entries(
    db: &Connection,
    identity: &Identity,
    q: &EntryQuery,
    start: &str,
    bounds: &BranchBounds,
) -> Result<Vec<Entry>> {
    // Materialize only after all traversed metadata has passed validation, so
    // a damaged older path never becomes a successful partial result.
    if q.order == Some(EntryOrder::OldestFirst) {
        materialize(db, identity, oldest_keys(db, identity, q, start, bounds)?)
    } else {
        materialize(db, identity, keys(db, identity, q, start, bounds)?)
    }
}

#[cfg(test)]
#[path = "branch_oldest_tests.rs"]
mod oldest_tests;
#[cfg(test)]
#[path = "branch_stream_tests.rs"]
mod tests;
