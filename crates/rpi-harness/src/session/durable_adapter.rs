//! Adapter from the Pi-facing session model to the durable journal contract.
//!
//! This module deliberately performs no I/O. `Session` and `AgentHarness` own
//! the semantic API; `rpi-durable::storage` owns sequence assignment and the
//! actual append. Transcript and execution records keep distinct kind prefixes
//! even when a caller chooses to persist them through one backend.

use super::types::{Entry, LaneRecord, SessionMutation};
use crate::error::{SessionError, SessionResult};
use rpi_durable::runtime::DurableService as DurableJournalService;
use rpi_durable::storage::{DurableBackend, JournalFrame, JournalMutation};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

pub fn entry_mutation(entry: &Entry) -> SessionResult<JournalMutation> {
    Ok(JournalMutation {
        timestamp: entry.base().timestamp,
        kind: format!("session.entry.{}", entry.entry_type()),
        payload: serde_json::to_value(entry).map_err(|error| {
            SessionError::invalid_entry(format!("encode session entry: {error}"))
        })?,
    })
}

pub fn lane_record_mutation(record: &LaneRecord) -> SessionResult<JournalMutation> {
    Ok(JournalMutation {
        timestamp: record.base().timestamp,
        kind: format!("session.record.{}", record.record_type()),
        payload: serde_json::to_value(record).map_err(|error| {
            SessionError::invalid_entry(format!("encode session lane record: {error}"))
        })?,
    })
}

/// Convert the complete typed Session mutation log, including lane and fact
/// mutations, without changing the legacy Session JSONL shape inside payload.
pub fn session_mutation_to_journal_mutation(
    mutation: &SessionMutation,
) -> SessionResult<JournalMutation> {
    let (kind, payload) = match mutation {
        SessionMutation::Entry {
            seq,
            timestamp,
            lane,
            entry,
        } => {
            if entry.base().seq != *seq || entry.base().timestamp != *timestamp {
                return Err(SessionError::invalid_entry(
                    "session mutation entry metadata does not match its sequence or timestamp",
                ));
            }
            let mut payload = entry.to_flat_json();
            let object = payload.as_object_mut().ok_or_else(|| {
                SessionError::invalid_entry("session entry payload is not an object")
            })?;
            object.insert(
                "lane".into(),
                lane.as_ref()
                    .map_or(Value::Null, |lane| Value::String(lane.clone())),
            );
            ("session.mutation.entry", payload)
        }
        SessionMutation::Record { record } => (
            "session.mutation.record",
            serde_json::to_value(record).map_err(|error| {
                SessionError::invalid_entry(format!("encode session mutation record: {error}"))
            })?,
        ),
        SessionMutation::Lane { seq, lane, leaf_id } => (
            "session.mutation.lane",
            serde_json::json!({"seq": seq, "lane": lane, "leafId": leaf_id}),
        ),
        SessionMutation::FactName { seq, name } => (
            "session.mutation.fact.name",
            serde_json::json!({"seq": seq, "name": name}),
        ),
        SessionMutation::FactLabel {
            seq,
            target_id,
            label,
        } => (
            "session.mutation.fact.label",
            serde_json::json!({"seq": seq, "targetId": target_id, "label": label}),
        ),
    };
    Ok(JournalMutation {
        timestamp: mutation_timestamp(mutation),
        kind: kind.into(),
        payload,
    })
}

pub fn session_mutation_from_frame(frame: &JournalFrame) -> SessionResult<SessionMutation> {
    let object = frame.payload.as_object().ok_or_else(|| {
        SessionError::invalid_entry("durable session mutation payload is not an object")
    })?;
    let mutation = match frame.kind.as_str() {
        "session.mutation.entry" => {
            let lane = match object.get("lane") {
                Some(Value::Null) => None,
                Some(Value::String(lane)) => Some(lane.clone()),
                None => return Err(SessionError::invalid_entry("durable entry lane is missing")),
                _ => return Err(SessionError::invalid_entry("durable entry lane is invalid")),
            };
            let mut entry_object = object.clone();
            entry_object.remove("lane");
            let entry: Entry =
                serde_json::from_value(Value::Object(entry_object)).map_err(|error| {
                    SessionError::invalid_entry(format!(
                        "decode durable session mutation entry: {error}"
                    ))
                })?;
            SessionMutation::Entry {
                seq: entry.base().seq,
                timestamp: entry.base().timestamp,
                lane,
                entry,
            }
        }
        "session.mutation.record" => {
            let record: LaneRecord =
                serde_json::from_value(frame.payload.clone()).map_err(|error| {
                    SessionError::invalid_entry(format!(
                        "decode durable session mutation record: {error}"
                    ))
                })?;
            SessionMutation::Record { record }
        }
        "session.mutation.lane" => SessionMutation::Lane {
            seq: required_u64(object, "seq")?,
            lane: required_string(object, "lane")?,
            leaf_id: required_optional_string(object, "leafId")?,
        },
        "session.mutation.fact.name" => SessionMutation::FactName {
            seq: required_u64(object, "seq")?,
            name: required_optional_string(object, "name")?,
        },
        "session.mutation.fact.label" => SessionMutation::FactLabel {
            seq: required_u64(object, "seq")?,
            target_id: required_string(object, "targetId")?,
            label: required_optional_string(object, "label")?,
        },
        _ => {
            return Err(SessionError::invalid_entry(format!(
                "unsupported durable session mutation kind: {}",
                frame.kind
            )))
        }
    };
    if frame.timestamp != mutation_timestamp(&mutation) {
        return Err(SessionError::invalid_entry(
            "durable mutation timestamp does not match frame metadata",
        ));
    }
    Ok(mutation)
}

fn mutation_timestamp(mutation: &SessionMutation) -> i64 {
    match mutation {
        SessionMutation::Entry { timestamp, .. } => *timestamp,
        SessionMutation::Record { record } => record.base().timestamp,
        SessionMutation::Lane { .. }
        | SessionMutation::FactName { .. }
        | SessionMutation::FactLabel { .. } => 0,
    }
}

fn required_u64(object: &Map<String, Value>, key: &str) -> SessionResult<u64> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| SessionError::invalid_entry(format!("durable mutation {key} is invalid")))
}

fn required_string(object: &Map<String, Value>, key: &str) -> SessionResult<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SessionError::invalid_entry(format!("durable mutation {key} is invalid")))
}

fn required_optional_string(
    object: &Map<String, Value>,
    key: &str,
) -> SessionResult<Option<String>> {
    match object.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        None => Err(SessionError::invalid_entry(format!(
            "durable mutation {key} is missing"
        ))),
        _ => Err(SessionError::invalid_entry(format!(
            "durable mutation {key} is invalid"
        ))),
    }
}

/// Decode only frames produced by `entry_mutation`; the discriminator is
/// checked against the decoded value so a forged kind/payload pairing fails
/// closed rather than being silently projected into Session state.
pub fn entry_from_frame(frame: &JournalFrame) -> SessionResult<Entry> {
    let entry: Entry = serde_json::from_value(frame.payload.clone()).map_err(|error| {
        SessionError::invalid_entry(format!("decode durable session entry: {error}"))
    })?;
    let expected = format!("session.entry.{}", entry.entry_type());
    if frame.kind != expected {
        return Err(SessionError::invalid_entry(format!(
            "durable entry kind mismatch: expected {expected}, got {}",
            frame.kind
        )));
    }
    if frame.timestamp != entry.base().timestamp {
        return Err(SessionError::invalid_entry(
            "durable entry timestamp does not match frame metadata",
        ));
    }
    Ok(entry)
}

/// Decode only frames produced by `lane_record_mutation`, with the same
/// discriminator/payload binding as entry projection.
pub fn lane_record_from_frame(frame: &JournalFrame) -> SessionResult<LaneRecord> {
    let record: LaneRecord = serde_json::from_value(frame.payload.clone()).map_err(|error| {
        SessionError::invalid_entry(format!("decode durable session record: {error}"))
    })?;
    let expected = format!("session.record.{}", record.record_type());
    if frame.kind != expected {
        return Err(SessionError::invalid_entry(format!(
            "durable record kind mismatch: expected {expected}, got {}",
            frame.kind
        )));
    }
    if frame.timestamp != record.base().timestamp {
        return Err(SessionError::invalid_entry(
            "durable record timestamp does not match frame metadata",
        ));
    }
    Ok(record)
}

/// Project only the transcript namespace from a mixed durable journal. Other
/// host-owned frames remain opaque and are intentionally not interpreted here.
pub fn entries_from_frames(frames: &[JournalFrame]) -> SessionResult<Vec<Entry>> {
    frames
        .iter()
        .filter(|frame| frame.kind.starts_with("session.entry."))
        .map(entry_from_frame)
        .collect()
}

/// Project only the execution-record namespace from a mixed durable journal.
pub fn lane_records_from_frames(frames: &[JournalFrame]) -> SessionResult<Vec<LaneRecord>> {
    frames
        .iter()
        .filter(|frame| frame.kind.starts_with("session.record."))
        .map(lane_record_from_frame)
        .collect()
}

pub fn session_mutations_from_frames(
    frames: &[JournalFrame],
) -> SessionResult<Vec<SessionMutation>> {
    frames
        .iter()
        .filter(|frame| frame.kind.starts_with("session.mutation."))
        .map(session_mutation_from_frame)
        .collect()
}

/// Harness-owned facade over the durable writer.
///
/// This type intentionally accepts the harness Session model but delegates
/// sequence assignment, revision CAS and persistence to `rpi-durable`. It is
/// a migration seam for the typed compatibility stores; it does not replace
/// the existing `Session` API or create a second transcript.
pub struct DurableSessionWriter<B: DurableBackend> {
    service: DurableJournalService<B>,
    mutation_state: super::state::SessionState,
    session_id: Option<String>,
}

/// Object-safe, host-mediated capability that a `Session` may hold for an
/// explicit durable write path. The normal compatibility storage path does
/// not call this capability automatically; that avoids an unsafe dual-write
/// protocol while the typed backends are being migrated.
pub trait SessionDurableWriter: Send + Sync {
    /// Optional host-owned identity binding. `None` preserves the low-level
    /// backend-only constructor; a bound writer is checked against Session
    /// metadata before every facade operation.
    fn session_identity(&self) -> Option<String> {
        None
    }

    fn append_entry(&self, entry: &Entry) -> SessionResult<JournalFrame>;
    fn append_record(&self, record: &LaneRecord) -> SessionResult<JournalFrame>;
    fn append_record_and_entry(
        &self,
        record: &LaneRecord,
        entry: &Entry,
    ) -> SessionResult<Vec<JournalFrame>>;
    fn append_session_mutation(&self, mutation: &SessionMutation) -> SessionResult<JournalFrame>;
    fn load_session_mutations(&self) -> SessionResult<Vec<SessionMutation>>;
    fn load_frames(&self) -> SessionResult<Vec<JournalFrame>>;
    fn load_entries(&self) -> SessionResult<Vec<Entry>>;
    fn load_records(&self) -> SessionResult<Vec<LaneRecord>>;
    fn refresh(&self) -> SessionResult<Option<u64>>;
    fn expected_sequence(&self) -> SessionResult<Option<u64>>;
}

/// Thread-safe adapter used when the capability is shared by Session clones.
/// The durable service remains the sole owner of sequence assignment.
pub struct SharedDurableSessionWriter<B: DurableBackend> {
    writer: Mutex<DurableSessionWriter<B>>,
}

impl<B: DurableBackend> SharedDurableSessionWriter<B> {
    pub fn new(writer: DurableSessionWriter<B>) -> Arc<Self> {
        Arc::new(Self {
            writer: Mutex::new(writer),
        })
    }

    pub fn expected_sequence(&self) -> SessionResult<Option<u64>> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?;
        Ok(writer.expected_sequence())
    }

    pub fn load(&self) -> SessionResult<Vec<JournalFrame>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .load()
    }
}

impl<B: DurableBackend + Send> SessionDurableWriter for SharedDurableSessionWriter<B> {
    fn session_identity(&self) -> Option<String> {
        self.writer
            .lock()
            .ok()
            .and_then(|writer| writer.session_identity().map(str::to_owned))
    }

    fn append_entry(&self, entry: &Entry) -> SessionResult<JournalFrame> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .append_entry(entry)
    }

    fn append_record(&self, record: &LaneRecord) -> SessionResult<JournalFrame> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .append_record(record)
    }

    fn append_record_and_entry(
        &self,
        record: &LaneRecord,
        entry: &Entry,
    ) -> SessionResult<Vec<JournalFrame>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .append_record_and_entry(record, entry)
    }

    fn load_frames(&self) -> SessionResult<Vec<JournalFrame>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .load()
    }

    fn load_entries(&self) -> SessionResult<Vec<Entry>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .load_entries()
    }

    fn load_records(&self) -> SessionResult<Vec<LaneRecord>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .load_records()
    }

    fn append_session_mutation(&self, mutation: &SessionMutation) -> SessionResult<JournalFrame> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .append_session_mutation(mutation)
    }

    fn load_session_mutations(&self) -> SessionResult<Vec<SessionMutation>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .load_session_mutations()
    }

    fn refresh(&self) -> SessionResult<Option<u64>> {
        self.writer
            .lock()
            .map_err(|_| SessionError::storage("durable writer lock is poisoned"))?
            .refresh()
    }

    fn expected_sequence(&self) -> SessionResult<Option<u64>> {
        SharedDurableSessionWriter::expected_sequence(self)
    }
}

impl<B: DurableBackend> DurableSessionWriter<B> {
    pub fn open(backend: B) -> Result<Self, SessionError> {
        Self::open_with_identity(backend, None)
    }

    /// Open a writer explicitly bound to one host Session identity. The
    /// backend journal itself remains independent of SessionStorage.
    pub fn open_for_session(
        backend: B,
        session_id: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(SessionError::invalid_entry(
                "durable writer session identity must not be blank",
            ));
        }
        Self::open_with_identity(backend, Some(session_id))
    }

    fn open_with_identity(backend: B, session_id: Option<String>) -> Result<Self, SessionError> {
        let mut service = DurableJournalService::open(backend).map_err(storage_error)?;
        let state = replay_mutation_state(&service.load().map_err(storage_error)?)?;
        Ok(Self {
            service,
            mutation_state: state,
            session_id,
        })
    }

    pub fn session_identity(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn append_entry(&mut self, entry: &Entry) -> SessionResult<JournalFrame> {
        self.service
            .append(entry_mutation(entry)?)
            .map_err(storage_error)
    }

    pub fn append_record(&mut self, record: &LaneRecord) -> SessionResult<JournalFrame> {
        self.service
            .append(lane_record_mutation(record)?)
            .map_err(storage_error)
    }

    pub fn append_record_and_entry(
        &mut self,
        record: &LaneRecord,
        entry: &Entry,
    ) -> SessionResult<Vec<JournalFrame>> {
        self.service
            .append_batch(vec![lane_record_mutation(record)?, entry_mutation(entry)?])
            .map_err(storage_error)
    }

    pub fn append_session_mutation(
        &mut self,
        mutation: &SessionMutation,
    ) -> SessionResult<JournalFrame> {
        let mut candidate = self.mutation_state.clone();
        candidate.apply_mutation(mutation.clone())?;
        let frame = self
            .service
            .append(session_mutation_to_journal_mutation(mutation)?)
            .map_err(storage_error)?;
        self.mutation_state = candidate;
        Ok(frame)
    }

    pub fn append_session_mutation_batch(
        &mut self,
        mutations: &[SessionMutation],
    ) -> SessionResult<Vec<JournalFrame>> {
        let mut candidate = self.mutation_state.clone();
        for mutation in mutations {
            candidate.apply_mutation(mutation.clone())?;
        }
        let journal_mutations = mutations
            .iter()
            .map(session_mutation_to_journal_mutation)
            .collect::<SessionResult<Vec<_>>>()?;
        let frames = self
            .service
            .append_batch(journal_mutations)
            .map_err(storage_error)?;
        self.mutation_state = candidate;
        Ok(frames)
    }

    pub fn load_session_mutations(&mut self) -> SessionResult<Vec<SessionMutation>> {
        session_mutations_from_frames(&self.load()?)
    }

    pub fn load(&mut self) -> SessionResult<Vec<JournalFrame>> {
        self.service.load().map_err(storage_error)
    }

    pub fn load_entries(&mut self) -> SessionResult<Vec<Entry>> {
        entries_from_frames(&self.load()?)
    }

    pub fn load_records(&mut self) -> SessionResult<Vec<LaneRecord>> {
        lane_records_from_frames(&self.load()?)
    }

    pub fn refresh(&mut self) -> SessionResult<Option<u64>> {
        let sequence = self.service.refresh().map_err(storage_error)?;
        let state = replay_mutation_state(&self.service.load().map_err(storage_error)?)?;
        self.mutation_state = state;
        Ok(sequence)
    }

    pub fn expected_sequence(&self) -> Option<u64> {
        self.service.expected_sequence()
    }
}

fn replay_mutation_state(frames: &[JournalFrame]) -> SessionResult<super::state::SessionState> {
    let mutations = session_mutations_from_frames(frames)?;
    let mut state = super::state::SessionState::new();
    for mutation in mutations {
        state.apply_mutation(mutation)?;
    }
    Ok(state)
}

/// Opt-in mutation-log facade that replays the durable journal into the
/// existing SessionState reducer. The journal frame sequence remains owned by
/// the durable backend; the SessionMutation sequence carried inside payloads
/// remains the reducer's independent sequence domain. It is intentionally
/// separate from the default SessionStorage implementations: installing this
/// facade does not migrate old JSONL/SQLite files or enable dual writes.
pub struct DurableSessionMutationLog<B: DurableBackend> {
    writer: DurableSessionWriter<B>,
    state: super::state::SessionState,
}

impl<B: DurableBackend> DurableSessionMutationLog<B> {
    pub fn open(backend: B) -> SessionResult<Self> {
        let writer = DurableSessionWriter::open(backend)?;
        Self::from_writer(writer)
    }

    pub fn from_writer(mut writer: DurableSessionWriter<B>) -> SessionResult<Self> {
        let mutations = writer.load_session_mutations()?;
        let mut state = super::state::SessionState::new();
        for mutation in mutations {
            state.apply_mutation(mutation)?;
        }
        Ok(Self { writer, state })
    }

    pub fn append(&mut self, mutation: SessionMutation) -> SessionResult<JournalFrame> {
        let mut candidate = self.state.clone();
        candidate.apply_mutation(mutation.clone())?;
        let frame = self.writer.append_session_mutation(&mutation)?;
        self.state = candidate;
        Ok(frame)
    }

    pub fn append_batch(
        &mut self,
        mutations: &[SessionMutation],
    ) -> SessionResult<Vec<JournalFrame>> {
        let mut candidate = self.state.clone();
        for mutation in mutations {
            candidate.apply_mutation(mutation.clone())?;
        }
        let frames = self.writer.append_session_mutation_batch(mutations)?;
        self.state = candidate;
        Ok(frames)
    }

    pub fn state(&self) -> &super::state::SessionState {
        &self.state
    }

    pub fn load_frames(&mut self) -> SessionResult<Vec<JournalFrame>> {
        self.writer.load()
    }

    pub fn refresh(&mut self) -> SessionResult<Option<u64>> {
        let sequence = self.writer.refresh()?;
        let mutations = self.writer.load_session_mutations()?;
        let mut state = super::state::SessionState::new();
        for mutation in mutations {
            state.apply_mutation(mutation)?;
        }
        self.state = state;
        Ok(sequence)
    }

    pub fn expected_sequence(&self) -> Option<u64> {
        self.writer.expected_sequence()
    }

    pub fn writer(&self) -> &DurableSessionWriter<B> {
        &self.writer
    }
}

fn storage_error(error: rpi_durable::storage::BackendError) -> SessionError {
    match error {
        rpi_durable::storage::BackendError::Conflict { .. } => {
            SessionError::durable_conflict(error.to_string())
        }
        rpi_durable::storage::BackendError::Unsupported(_) => {
            SessionError::durable_unsupported(error.to_string())
        }
        error => SessionError::storage(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
    use crate::session::types::{Entry, EntryBase, LaneRecord, SessionMetadata};
    use crate::session::Session;
    use rpi_durable::storage::jsonl::JsonlBackend;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn durable_backend_error_mapping_preserves_typed_kinds() {
        assert_eq!(
            storage_error(rpi_durable::storage::BackendError::Conflict {
                expected: Some(1),
                actual: Some(2),
            })
            .code,
            crate::error::SessionErrorCode::DurableConflict
        );
        assert_eq!(
            storage_error(rpi_durable::storage::BackendError::Unsupported(
                "batch".into(),
            ))
            .code,
            crate::error::SessionErrorCode::DurableUnsupported
        );
    }

    #[test]
    fn transcript_and_execution_kinds_are_separate() {
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 10,
            },
            custom_type: "test".into(),
            data: None,
        });
        let entry = entry_mutation(&entry).unwrap();
        assert_eq!(entry.kind, "session.entry.custom");

        let record = LaneRecord::OperationStarted(crate::session::types::OperationStartedRecord {
            base: crate::session::types::RecordBase {
                id: "record".into(),
                seq: 1,
                lane: "main".into(),
                timestamp: 11,
            },
            source_leaf_id: None,
            intent: crate::session::types::OperationIntent::Navigation {
                target_id: None,
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        });
        let record = lane_record_mutation(&record).unwrap();
        assert_eq!(record.kind, "session.record.operation_started");
    }

    #[test]
    fn durable_frame_projection_requires_matching_discriminator() {
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 10,
            },
            custom_type: "test".into(),
            data: None,
        });
        let mutation = entry_mutation(&entry).unwrap();
        let frame = JournalFrame {
            sequence: 1,
            timestamp: mutation.timestamp,
            kind: mutation.kind,
            payload: mutation.payload,
        };
        assert_eq!(entry_from_frame(&frame).unwrap(), entry);

        let mut forged = frame.clone();
        forged.kind = "session.entry.message".into();
        assert!(entry_from_frame(&forged).is_err());

        let record = LaneRecord::OperationStarted(crate::session::types::OperationStartedRecord {
            base: crate::session::types::RecordBase {
                id: "record".into(),
                seq: 1,
                lane: "main".into(),
                timestamp: 11,
            },
            source_leaf_id: None,
            intent: crate::session::types::OperationIntent::Navigation {
                target_id: None,
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        });
        let record_mutation = lane_record_mutation(&record).unwrap();
        let record_frame = JournalFrame {
            sequence: 2,
            timestamp: record_mutation.timestamp,
            kind: record_mutation.kind,
            payload: record_mutation.payload,
        };
        let mixed = vec![frame, record_frame];
        assert_eq!(entries_from_frames(&mixed).unwrap(), vec![entry]);
        assert_eq!(lane_records_from_frames(&mixed).unwrap(), vec![record]);
    }

    #[test]
    fn complete_session_mutation_projection_roundtrips_lane_and_facts() {
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 10,
            },
            custom_type: "test".into(),
            data: None,
        });
        let mutations = vec![
            SessionMutation::Entry {
                seq: 1,
                timestamp: 10,
                lane: Some("main".into()),
                entry,
            },
            SessionMutation::Lane {
                seq: 2,
                lane: "main".into(),
                leaf_id: Some("entry".into()),
            },
            SessionMutation::FactName {
                seq: 3,
                name: Some("session name".into()),
            },
            SessionMutation::FactLabel {
                seq: 4,
                target_id: "entry".into(),
                label: None,
            },
            SessionMutation::Record {
                record: LaneRecord::OperationStarted(
                    crate::session::types::OperationStartedRecord {
                        base: crate::session::types::RecordBase {
                            id: "record".into(),
                            seq: 5,
                            lane: "main".into(),
                            timestamp: 11,
                        },
                        source_leaf_id: None,
                        intent: crate::session::types::OperationIntent::Navigation {
                            target_id: None,
                            summarize: false,
                            custom_instructions: None,
                            label: None,
                            summary_entry_id: None,
                        },
                    },
                ),
            },
        ];
        for mutation in mutations {
            let encoded = session_mutation_to_journal_mutation(&mutation).unwrap();
            let frame = JournalFrame {
                sequence: mutation.seq(),
                timestamp: encoded.timestamp,
                kind: encoded.kind,
                payload: encoded.payload,
            };
            assert_eq!(session_mutation_from_frame(&frame).unwrap(), mutation);
        }
    }

    #[test]
    fn mutation_projection_rejects_missing_nullable_fields_and_metadata_mismatch() {
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 10,
            },
            custom_type: "test".into(),
            data: None,
        });
        let encoded = session_mutation_to_journal_mutation(&SessionMutation::Entry {
            seq: 1,
            timestamp: 10,
            lane: None,
            entry,
        })
        .unwrap();

        let mut missing_lane = encoded.payload.as_object().unwrap().clone();
        missing_lane.remove("lane");
        let missing_lane = JournalFrame {
            sequence: 1,
            timestamp: encoded.timestamp,
            kind: encoded.kind.clone(),
            payload: Value::Object(missing_lane),
        };
        assert!(session_mutation_from_frame(&missing_lane).is_err());

        let mut wrong_timestamp = encoded.payload.clone();
        let wrong_timestamp = JournalFrame {
            sequence: 1,
            timestamp: 999,
            kind: encoded.kind,
            payload: std::mem::take(&mut wrong_timestamp),
        };
        assert!(session_mutation_from_frame(&wrong_timestamp).is_err());

        let lane = JournalFrame {
            sequence: 2,
            timestamp: 0,
            kind: "session.mutation.lane".into(),
            payload: serde_json::json!({"seq": 2, "lane": "main"}),
        };
        assert!(session_mutation_from_frame(&lane).is_err());
    }

    #[test]
    fn raw_load_preserves_unknown_host_owned_frames() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let mut service =
            rpi_durable::runtime::DurableService::open(JsonlBackend::open(&path).unwrap()).unwrap();
        service
            .append(JournalMutation {
                timestamp: 1,
                kind: "host.owned.future".into(),
                payload: serde_json::json!({"opaque": true}),
            })
            .unwrap();

        let mut writer = DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap();
        assert_eq!(writer.load().unwrap().len(), 1);
        assert!(entries_from_frames(&writer.load().unwrap())
            .unwrap()
            .is_empty());
        assert!(lane_records_from_frames(&writer.load().unwrap())
            .unwrap()
            .is_empty());
        assert!(session_mutations_from_frames(&writer.load().unwrap())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn mutation_log_replays_state_and_rejects_invalid_batch_before_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mutations.jsonl");
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 10,
            },
            custom_type: "test".into(),
            data: None,
        });
        let mutations = vec![
            SessionMutation::Entry {
                seq: 1,
                timestamp: 10,
                lane: Some("main".into()),
                entry,
            },
            SessionMutation::Lane {
                seq: 2,
                lane: "main".into(),
                leaf_id: Some("entry".into()),
            },
            SessionMutation::FactName {
                seq: 3,
                name: Some("session name".into()),
            },
            SessionMutation::FactLabel {
                seq: 4,
                target_id: "entry".into(),
                label: Some("important".into()),
            },
        ];
        let backend = JsonlBackend::open(&path).unwrap();
        let mut log = DurableSessionMutationLog::open(backend).unwrap();
        let frames = log.append_batch(&mutations).unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(log.state().get_entry("entry").unwrap().id(), "entry");
        assert_eq!(log.state().get_name(), Some("session name"));
        assert_eq!(log.state().get_label("entry"), Some("important"));
        drop(log);

        let mut reopened =
            DurableSessionMutationLog::open(JsonlBackend::open(&path).unwrap()).unwrap();
        assert_eq!(reopened.state().get_entry("entry").unwrap().id(), "entry");
        assert_eq!(reopened.state().get_lanes()[0].lane, "main");
        assert_eq!(reopened.state().get_name(), Some("session name"));
        assert_eq!(reopened.state().get_label("entry"), Some("important"));
        assert_eq!(reopened.load_frames().unwrap().len(), 4);

        let invalid = SessionMutation::Lane {
            seq: 6,
            lane: "main".into(),
            leaf_id: None,
        };
        assert!(reopened.append_batch(&[invalid]).is_err());
        assert_eq!(reopened.load_frames().unwrap().len(), 4);
    }

    #[test]
    fn mutation_log_refresh_replays_external_mutations_before_append() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut log = DurableSessionMutationLog::open(JsonlBackend::open(&path).unwrap()).unwrap();
        log.append(SessionMutation::FactName {
            seq: 1,
            name: Some("first".into()),
        })
        .unwrap();

        let mut external = DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap();
        external
            .append_session_mutation(&SessionMutation::FactName {
                seq: 2,
                name: Some("external".into()),
            })
            .unwrap();

        assert_eq!(log.refresh().unwrap(), Some(2));
        assert_eq!(log.state().get_name(), Some("external"));
        log.append(SessionMutation::FactName {
            seq: 3,
            name: Some("third".into()),
        })
        .unwrap();
        assert_eq!(log.state().get_name(), Some("third"));
    }

    #[test]
    fn session_explicit_durable_path_survives_jsonl_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SharedDurableSessionWriter::new(
            DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap(),
        );
        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "durable-session-test".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let session = Session::new_with_durable_writer(storage, None, writer.clone());
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 1,
                parent_id: None,
                timestamp: 12,
            },
            custom_type: "test".into(),
            data: None,
        });

        let committed = session.append_entry_to_durable(&entry).unwrap();
        assert_eq!(committed.sequence, 1);
        assert_eq!(writer.expected_sequence().unwrap(), Some(1));
        assert_eq!(session.durable_expected_sequence().unwrap(), Some(1));
        assert_eq!(session.refresh_durable_writer().unwrap(), Some(1));

        let mut external = DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap();
        assert_eq!(external.append_entry(&entry).unwrap().sequence, 2);

        let record = LaneRecord::OperationStarted(crate::session::types::OperationStartedRecord {
            base: crate::session::types::RecordBase {
                id: "record".into(),
                seq: 1,
                lane: "main".into(),
                timestamp: 13,
            },
            source_leaf_id: None,
            intent: crate::session::types::OperationIntent::Navigation {
                target_id: None,
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        });
        let conflict = session
            .append_record_and_entry_to_durable(&record, &entry)
            .unwrap_err();
        assert_eq!(
            conflict.code,
            crate::error::SessionErrorCode::DurableConflict
        );
        assert_eq!(session.refresh_durable_writer().unwrap(), Some(2));
        let batch = session
            .append_record_and_entry_to_durable(&record, &entry)
            .unwrap();
        assert_eq!(
            batch.iter().map(|frame| frame.sequence).collect::<Vec<_>>(),
            vec![3, 4]
        );
        let mutation = SessionMutation::FactName {
            seq: 1,
            name: Some("durable view".into()),
        };
        assert_eq!(
            session
                .append_session_mutation_to_durable(&mutation)
                .unwrap()
                .sequence,
            5
        );
        assert_eq!(session.load_durable_mutations().unwrap(), vec![mutation]);

        let mut reopened = DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap();
        assert_eq!(reopened.load().unwrap().len(), 5);
        assert_eq!(session.load_durable_frames().unwrap().len(), 5);
        assert_eq!(session.load_durable_entries().unwrap().len(), 3);
        assert_eq!(session.load_durable_records().unwrap().len(), 1);
    }

    #[test]
    fn bound_writer_rejects_a_different_session_identity() {
        let dir = tempdir().unwrap();
        let writer = SharedDurableSessionWriter::new(
            DurableSessionWriter::open_for_session(
                JsonlBackend::open(dir.path().join("session.jsonl")).unwrap(),
                "writer-session",
            )
            .unwrap(),
        );
        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "other-session".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let session = Session::new_with_durable_writer(storage, None, writer.clone());
        let error = session
            .append_session_mutation_to_durable(&SessionMutation::FactName {
                seq: 1,
                name: Some("must not write".into()),
            })
            .unwrap_err();
        assert_eq!(error.code, crate::error::SessionErrorCode::Storage);
        assert!(writer.load().unwrap().is_empty());
    }

    #[test]
    fn storage_rebind_clears_session_durable_capability() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SharedDurableSessionWriter::new(
            DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap(),
        );
        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "original-session".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let session = Session::new_with_durable_writer(storage, None, writer.clone());
        let replacement = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "replacement-session".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        session.set_storage(replacement);
        assert!(session.durable_writer().is_none());
        assert!(session
            .append_session_mutation_to_durable(&SessionMutation::FactName {
                seq: 1,
                name: Some("must not use old writer".into()),
            })
            .is_err());
        assert!(writer.load().unwrap().is_empty());
    }

    #[test]
    fn session_mutation_facade_rejects_reducer_invalid_mutation_before_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = SharedDurableSessionWriter::new(
            DurableSessionWriter::open(JsonlBackend::open(&path).unwrap()).unwrap(),
        );
        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "mutation-validation-session".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let session = Session::new_with_durable_writer(storage, None, writer.clone());
        let error = session
            .append_session_mutation_to_durable(&SessionMutation::FactName {
                seq: 2,
                name: Some("must not commit".into()),
            })
            .unwrap_err();
        assert_eq!(error.code, crate::error::SessionErrorCode::InvalidEntry);
        assert!(writer.load().unwrap().is_empty());
    }

    #[cfg(feature = "durable-sqlite")]
    #[test]
    fn session_explicit_durable_path_survives_sqlite_reopen() {
        use rpi_durable::storage::sqlite::SqliteBackend;

        let dir = tempdir().unwrap();
        let path = dir.path().join("session.sqlite");
        let writer = SharedDurableSessionWriter::new(
            DurableSessionWriter::open(SqliteBackend::open(&path).unwrap()).unwrap(),
        );
        let storage = Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "durable-sqlite-session-test".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        ));
        let session = Session::new_with_durable_writer(storage, None, writer);
        let record = LaneRecord::OperationStarted(crate::session::types::OperationStartedRecord {
            base: crate::session::types::RecordBase {
                id: "record".into(),
                seq: 1,
                lane: "main".into(),
                timestamp: 13,
            },
            source_leaf_id: None,
            intent: crate::session::types::OperationIntent::Navigation {
                target_id: None,
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        });

        let committed = session.append_record_to_durable(&record).unwrap();
        assert_eq!(committed.sequence, 1);

        let mut reopened = DurableSessionWriter::open(SqliteBackend::open(&path).unwrap()).unwrap();
        assert_eq!(reopened.load().unwrap(), vec![committed]);
    }

    #[test]
    fn writer_delegates_sequence_assignment_to_durable_storage() {
        let dir = tempdir().unwrap();
        let backend = JsonlBackend::open(dir.path().join("session.jsonl")).unwrap();
        let mut writer = DurableSessionWriter::open(backend).unwrap();
        let entry = Entry::Custom(crate::session::types::CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry".into(),
                seq: 41,
                parent_id: None,
                timestamp: 12,
            },
            custom_type: "test".into(),
            data: None,
        });
        let committed = writer.append_entry(&entry).unwrap();
        assert_eq!(committed.sequence, 1);
        assert_eq!(writer.expected_sequence(), Some(1));
    }
}
