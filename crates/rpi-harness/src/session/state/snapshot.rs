//! Preserve the existing SQLite snapshot format while storing payloads once.
use super::*;
use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct};

struct EntryIndex<'a>(&'a SessionState);
impl Serialize for EntryIndex<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.entries_by_id.len()))?;
        for (id, index) in &self.0.entries_by_id {
            map.serialize_entry(id, &self.0.entries[*index])?;
        }
        map.end()
    }
}

#[derive(Serialize)]
enum PayloadLog<'a> {
    Entry { seq: u64, entry: &'a Entry },
    Record { seq: u64, record: &'a LaneRecord },
}
struct LogSnapshot<'a>(&'a SessionState);
impl Serialize for LogSnapshot<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.log.len()))?;
        for item in &self.0.log {
            match item {
                StoredLogItem::Entry(index) => {
                    let entry = &self.0.entries[*index];
                    seq.serialize_element(&PayloadLog::Entry {
                        seq: entry.seq(),
                        entry,
                    })?;
                }
                StoredLogItem::Record(index) => {
                    let record = &self.0.records[*index];
                    seq.serialize_element(&PayloadLog::Record {
                        seq: record.seq(),
                        record,
                    })?;
                }
                StoredLogItem::Other(item) => seq.serialize_element(item)?,
            }
        }
        seq.end()
    }
}

impl Serialize for SessionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SessionState", 11)?;
        state.serialize_field("sequence", &self.sequence)?;
        state.serialize_field("used_ids", &self.used_ids)?;
        state.serialize_field("entries", &self.entries)?;
        state.serialize_field("entries_by_id", &EntryIndex(self))?;
        state.serialize_field("records", &self.records)?;
        state.serialize_field("open_operations_by_lane", &self.open_operations_by_lane)?;
        state.serialize_field("lanes", &self.lanes)?;
        state.serialize_field("log", &LogSnapshot(self))?;
        state.serialize_field("stats", &self.stats)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("labels", &self.labels)?;
        state.end()
    }
}

#[derive(Deserialize)]
struct Snapshot {
    sequence: u64,
    used_ids: std::collections::HashSet<String>,
    entries: Vec<Entry>,
    // The legacy entries_by_id payloads are redundant. Rebuild the index from
    // canonical entries instead of deserializing a second set of messages.
    records: Vec<LaneRecord>,
    open_operations_by_lane: HashMap<String, BTreeMap<u64, OperationStartedRecord>>,
    lanes: BTreeMap<String, Option<String>>,
    log: Vec<LogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
}

impl<'de> Deserialize<'de> for SessionState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let snapshot = Snapshot::deserialize(deserializer)?;
        let entries_by_id: HashMap<_, _> = snapshot
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id().to_owned(), index))
            .collect();
        let records_by_seq: HashMap<_, _> = snapshot
            .records
            .iter()
            .enumerate()
            .map(|(index, record)| (record.seq(), index))
            .collect();
        let mut log = Vec::with_capacity(snapshot.log.len());
        for item in snapshot.log {
            log.push(match item {
                LogItem::Entry { seq, entry } => {
                    let index = entries_by_id.get(entry.id()).copied().ok_or_else(|| {
                        D::Error::custom("snapshot log references a missing entry")
                    })?;
                    if seq != snapshot.entries[index].seq() || entry != snapshot.entries[index] {
                        return Err(D::Error::custom(
                            "snapshot entry log differs from its payload",
                        ));
                    }
                    StoredLogItem::Entry(index)
                }
                LogItem::Record { seq, record } => {
                    let index = records_by_seq.get(&seq).copied().ok_or_else(|| {
                        D::Error::custom("snapshot log references a missing record")
                    })?;
                    if record != snapshot.records[index] {
                        return Err(D::Error::custom(
                            "snapshot record log differs from its payload",
                        ));
                    }
                    StoredLogItem::Record(index)
                }
                other => StoredLogItem::Other(Box::new(other)),
            });
        }
        // Rebuild the derived role index. Reject malformed parent graphs rather
        // than allowing an optimized ancestry walk to loop or cross a branch.
        if entries_by_id.len() != snapshot.entries.len() {
            return Err(D::Error::custom("snapshot has duplicate entry IDs"));
        }
        let mut nearest_user = Vec::with_capacity(snapshot.entries.len());
        for (index, entry) in snapshot.entries.iter().enumerate() {
            let previous = match entry.parent_id() {
                Some(id) => {
                    let parent = entries_by_id
                        .get(id)
                        .filter(|parent| **parent < index)
                        .ok_or_else(|| {
                            D::Error::custom("snapshot has a missing or forward parent")
                        })?;
                    nearest_user[*parent]
                }
                None => None,
            };
            nearest_user.push(
                if matches!(
                    entry.as_message(),
                    Some(rpi_agent::message::AgentMessage::User(_))
                ) {
                    Some(index)
                } else {
                    previous
                },
            );
        }
        Ok(Self {
            nearest_user,
            sequence: snapshot.sequence,
            used_ids: snapshot.used_ids,
            entries: snapshot.entries,
            entries_by_id,
            records: snapshot.records,
            open_operations_by_lane: snapshot.open_operations_by_lane,
            lanes: snapshot.lanes,
            log,
            stats: snapshot.stats,
            name: snapshot.name,
            labels: snapshot.labels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{
        AssistantFrameOp, AssistantFrameRecord, EntryBase, MessageEntry, RecordBase,
    };

    fn state() -> SessionState {
        let mut state = SessionState::new();
        for index in 0..2 {
            let entry = Entry::Message(MessageEntry {
                base: EntryBase {
                    entry_type: "message".into(),
                    id: format!("e{index}"),
                    seq: index + 1,
                    parent_id: (index > 0).then(|| "e0".into()),
                    timestamp: 42,
                },
                message: rpi_ai::UserMessage::new("history 🐈".repeat(8), 42).into(),
                terminate: None,
            });
            state
                .apply_mutation(SessionMutation::Entry {
                    seq: index + 1,
                    timestamp: 42,
                    lane: Some("main".into()),
                    entry,
                })
                .unwrap();
        }
        state
            .apply_mutation(SessionMutation::Record {
                record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                    base: RecordBase {
                        id: "frame".into(),
                        seq: 3,
                        lane: "main".into(),
                        timestamp: 42,
                    },
                    run_id: "run".into(),
                    stream_index: 0,
                    op: AssistantFrameOp::ClearRun,
                    frame: None,
                }),
            })
            .unwrap();
        state
            .apply_mutation(SessionMutation::FactName {
                seq: 4,
                name: Some("name".into()),
            })
            .unwrap();
        state
    }

    #[test]
    fn legacy_snapshot_shape_roundtrips_indexes_log_cursors_and_defensive_reads() {
        let state = state();
        let legacy = serde_json::to_value(&state).unwrap();
        assert_eq!(legacy["entries_by_id"]["e0"], legacy["entries"][0]);
        assert_eq!(legacy["log"][0]["Entry"]["entry"], legacy["entries"][0]);
        assert_eq!(legacy["log"][2]["Record"]["record"], legacy["records"][0]);
        let restored: SessionState = serde_json::from_value(legacy.clone()).unwrap();
        let mut restored_json = serde_json::to_value(&restored).unwrap();
        let mut legacy = legacy;
        for value in [&mut restored_json, &mut legacy] {
            value["used_ids"]
                .as_array_mut()
                .unwrap()
                .sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        }
        assert_eq!(restored_json, legacy);
        assert_eq!(
            restored
                .get_log(&LogOptions {
                    after_seq: Some(2),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            2
        );
        let mut entries = restored
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
                "e1",
            )
            .unwrap();
        assert_eq!(
            entries.iter().map(Entry::id).collect::<Vec<_>>(),
            ["e0", "e1"]
        );
        entries[0].base_mut().timestamp = 999;
        assert_eq!(restored.get_entry("e0").unwrap().base().timestamp, 42);
    }

    #[test]
    fn contradictory_snapshot_log_payloads_are_rejected() {
        let mut legacy = serde_json::to_value(state()).unwrap();
        legacy["log"][0]["Entry"]["entry"]["timestamp"] = serde_json::json!(999);
        assert!(serde_json::from_value::<SessionState>(legacy).is_err());
    }
}
