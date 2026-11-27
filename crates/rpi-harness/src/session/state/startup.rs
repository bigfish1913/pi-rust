//! Portable reducer image for native JSONL startup. Payloads are supplied by
//! the journal backend. Rebuild lookup indexes and check graph/log structure
//! before installing the image.
use super::*;
use std::collections::HashSet;

#[derive(Deserialize, Serialize)]
pub(crate) struct StartupIndex<P> {
    sequence: u64,
    used_ids: HashSet<String>,
    entries: Vec<P>,
    metadata: Vec<EntryMeta>,
    records: Vec<LaneRecord>,
    lanes: BTreeMap<String, Option<String>>,
    lane_generations: BTreeMap<String, u64>,
    log: Vec<StoredLogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
}

impl<P: Clone> IndexedState<P> {
    pub(crate) fn startup_image<Q>(
        &self,
        mut image_payload: impl FnMut(&P) -> Option<Q>,
    ) -> Option<StartupIndex<Q>> {
        Some(StartupIndex {
            entries: self
                .entries
                .iter()
                .map(&mut image_payload)
                .collect::<Option<_>>()?,
            sequence: self.sequence,
            used_ids: self.used_ids.clone(),
            metadata: self.metadata.clone(),
            records: self.records.clone(),
            lanes: self.lanes.clone(),
            lane_generations: self.lane_generations.clone(),
            log: self.log.clone(),
            stats: self.stats.clone(),
            name: self.name.clone(),
            labels: self.labels.clone(),
        })
    }
}

impl<P> StartupIndex<P> {
    pub(crate) fn restore<Q: Clone>(
        self,
        mut rebind: impl FnMut(P, &EntryMeta) -> Option<Q>,
    ) -> Option<IndexedState<Q>> {
        if self.metadata.len() != self.entries.len() || !self.lanes.contains_key("main") {
            return None;
        }
        let mut entries_by_id = HashMap::with_capacity(self.entries.len());
        let mut nearest_user = Vec::with_capacity(self.entries.len());
        let mut entries = Vec::with_capacity(self.entries.len());
        for (payload, meta) in self.entries.into_iter().zip(&self.metadata) {
            let index = entries.len();
            let prior = match meta.parent_id() {
                Some(parent) => nearest_user[*entries_by_id.get(parent)?],
                None => None,
            };
            if !self.used_ids.contains(&meta.id)
                || entries_by_id.insert(meta.id.clone(), index).is_some()
            {
                return None;
            }
            nearest_user.push(if meta.is_user() { Some(index) } else { prior });
            entries.push(rebind(payload, meta)?);
        }
        if self.lanes.len() != self.lane_generations.len()
            || self.lanes.iter().any(|(lane, leaf)| {
                self.lane_generations
                    .get(lane)
                    .is_none_or(|&generation| generation > self.sequence)
                    || leaf
                        .as_ref()
                        .is_some_and(|id| !entries_by_id.contains_key(id))
            })
            || !self.labels.keys().all(|id| entries_by_id.contains_key(id))
        {
            return None;
        }

        let mut next_entry = 0;
        let mut next_record = 0;
        let mut last_seq = 0;
        for item in &self.log {
            let seq = match item {
                StoredLogItem::Entry(index) if *index == next_entry => {
                    next_entry += 1;
                    self.metadata.get(*index)?.seq
                }
                StoredLogItem::Record(index) if *index == next_record => {
                    next_record += 1;
                    self.records.get(*index)?.seq()
                }
                StoredLogItem::Other(item)
                    if !matches!(
                        item.as_ref(),
                        LogItem::Entry { .. } | LogItem::Record { .. }
                    ) =>
                {
                    item.seq()
                }
                _ => return None,
            };
            if seq <= last_seq || seq > self.sequence {
                return None;
            }
            last_seq = seq;
        }
        if next_entry != entries.len() || next_record != self.records.len() {
            return None;
        }

        let mut open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>> =
            HashMap::new();
        let mut hot_append_count = 0;
        let mut record_ids = HashSet::new();
        for record in &self.records {
            if !self.lanes.contains_key(record.lane())
                || !self.used_ids.contains(record.id())
                || entries_by_id.contains_key(record.id())
                || !record_ids.insert(record.id())
            {
                return None;
            }
            match record {
                LaneRecord::OperationStarted(started) => {
                    open_operations_by_lane
                        .entry(started.base.lane.clone())
                        .or_default()
                        .insert(started.base.seq, started.clone());
                }
                LaneRecord::OperationFinished(finished) => {
                    if let Some(open) = open_operations_by_lane.get_mut(&finished.base.lane) {
                        open.retain(|_, started| started.base.id != finished.run_id);
                        if open.is_empty() {
                            open_operations_by_lane.remove(&finished.base.lane);
                        }
                    }
                }
                LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::Append => {
                    hot_append_count += 1;
                }
                _ => {}
            }
        }
        Some(IndexedState {
            sequence: self.sequence,
            used_ids: self.used_ids,
            entries,
            metadata: self.metadata,
            entries_by_id,
            nearest_user,
            records: self.records,
            hot_append_count,
            #[cfg(test)]
            retire_record_checks: 0,
            open_operations_by_lane,
            lanes: self.lanes,
            lane_generations: self.lane_generations,
            log: self.log,
            stats: self.stats,
            name: self.name,
            labels: self.labels,
        })
    }
}
