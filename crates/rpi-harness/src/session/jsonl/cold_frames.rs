//! JSONL-private payload references. Query plans own only selected sources; no
//! warm cache and no published plan holds or clones the resident history index.
use super::{codec::parse_mutation, errors::file_result};
use crate::{
    error::{SessionError, SessionResult},
    session::{
        state::{matches_entry_payload, ApplyOutcome, IndexedState, StoredLogItem},
        types::*,
    },
};
use rpi_tools::env::{AppendReceipt, ReadFile};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct Location {
    reader: Arc<dyn ReadFile>,
    seq: u64,
    offset: u64,
    length: usize,
    digest: [u8; 32],
    id: String,
    lane: String,
    run: String,
}
impl Location {
    fn new(reader: Arc<dyn ReadFile>, offset: u64, raw: &[u8], mutation: &SessionMutation) -> Self {
        let (id, lane, run) = match mutation {
            SessionMutation::Entry { entry, .. } => {
                (entry.id().into(), String::new(), String::new())
            }
            SessionMutation::Record { record } => (
                record.id().into(),
                record.lane().into(),
                record.run_id().unwrap_or_default().into(),
            ),
            _ => (String::new(), String::new(), String::new()),
        };
        Self {
            reader,
            seq: mutation.seq(),
            offset,
            length: raw.len(),
            digest: Sha256::digest(raw).into(),
            id,
            lane,
            run,
        }
    }
    async fn read(&self) -> SessionResult<SessionMutation> {
        let raw = file_result(
            self.reader.read_range(self.offset, self.length).await,
            "read cold history range",
        )?;
        self.decode(&raw)
    }
    fn decode(&self, raw: &[u8]) -> SessionResult<SessionMutation> {
        if raw.len() != self.length {
            return Err(SessionError::storage("short cold history read"));
        }
        if <[u8; 32]>::from(Sha256::digest(raw)) != self.digest {
            return Err(SessionError::invalid_entry(
                "cold history source changed; reopen session",
            ));
        }
        let line = std::str::from_utf8(raw)
            .map_err(|_| SessionError::invalid_entry("invalid cold history UTF-8"))?;
        let mutation = parse_mutation(line)
            .map_err(|e| SessionError::invalid_entry(format!("invalid cold history: {e}")))?;
        let identity = match &mutation {
            SessionMutation::Entry { entry, .. } => entry.id() == self.id,
            SessionMutation::Record { record } => {
                record.id() == self.id
                    && record.lane() == self.lane
                    && record.run_id().unwrap_or_default() == self.run
            }
            _ => false,
        };
        if mutation.seq() != self.seq || !identity {
            return Err(SessionError::invalid_entry(
                "cold history identity mismatch",
            ));
        }
        Ok(mutation)
    }
}
/// Live entries can distinguish `Some(Null)` from `None`, whereas the legacy
/// message decoder intentionally cannot. Keep only typed presence corrections,
/// never message bodies or raw JSON, until this live generation is dropped.
#[derive(Clone, Copy)]
enum MessageNull {
    ToolDetails,
    DeferredData,
}
impl MessageNull {
    fn capture(message: &rpi_agent::message::AgentMessage) -> Option<Self> {
        use rpi_agent::message::AgentMessage;
        match message {
            AgentMessage::ToolResult(message)
                if message.details.as_ref().is_some_and(|v| v.is_null()) =>
            {
                Some(Self::ToolDetails)
            }
            AgentMessage::Assistant(message)
                if message
                    .deferred
                    .as_ref()
                    .and_then(|d| d.data.as_ref())
                    .is_some_and(|v| v.is_null()) =>
            {
                Some(Self::DeferredData)
            }
            _ => None,
        }
    }
    fn apply(self, message: &mut rpi_agent::message::AgentMessage) {
        use rpi_agent::message::AgentMessage;
        match (self, message) {
            (Self::ToolDetails, AgentMessage::ToolResult(message)) => {
                message.details = Some(serde_json::Value::Null);
            }
            (Self::DeferredData, AgentMessage::Assistant(message)) => {
                if let Some(deferred) = &mut message.deferred {
                    deferred.data = Some(serde_json::Value::Null);
                }
            }
            _ => {}
        }
    }
}
#[derive(Clone, Default)]
pub(super) struct LiveNulls {
    message: Option<MessageNull>,
    retained_tail: Vec<(usize, MessageNull)>,
}
impl LiveNulls {
    fn capture(entry: &Entry) -> Self {
        match entry {
            Entry::Message(entry) => Self {
                message: MessageNull::capture(&entry.message),
                ..Self::default()
            },
            Entry::Compaction(entry) => Self {
                retained_tail: entry
                    .retained_tail
                    .iter()
                    .enumerate()
                    .filter_map(|(i, message)| {
                        MessageNull::capture(message).map(|correction| (i, correction))
                    })
                    .collect(),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
    fn apply(&self, entry: &mut Entry) {
        match entry {
            Entry::Message(entry) => {
                if let Some(correction) = self.message {
                    correction.apply(&mut entry.message);
                }
            }
            Entry::Compaction(entry) => {
                for &(index, correction) in &self.retained_tail {
                    if let Some(message) = entry.retained_tail.get_mut(index) {
                        correction.apply(message);
                    }
                }
            }
            _ => {}
        }
    }
}
#[derive(Clone)]
pub(super) enum JsonlPayload {
    Resident(Arc<Entry>),
    Cold {
        source: Arc<Location>,
        parent: Option<String>,
        live_nulls: LiveNulls,
    },
}
impl JsonlPayload {
    pub async fn read(&self) -> SessionResult<Entry> {
        match self {
            Self::Resident(entry) => Ok((**entry).clone()),
            Self::Cold {
                source,
                parent,
                live_nulls,
            } => {
                let SessionMutation::Entry { mut entry, .. } = source.read().await? else {
                    return Err(SessionError::invalid_entry("cold entry identity mismatch"));
                };
                entry.base_mut().parent_id = parent.clone();
                live_nulls.apply(&mut entry);
                Ok(entry)
            }
        }
    }
}
#[derive(Clone, Default)]
struct ColdFrames {
    locations: Vec<Arc<Location>>,
}
#[derive(Clone)]
pub(super) struct ResidentState {
    reducer: IndexedState<JsonlPayload>,
    cold: ColdFrames,
    // Only the currently durable batch, consumed by apply_mutation. Custom
    // appenders without receipts simply leave real payloads resident.
    pending: HashMap<u64, Arc<Location>>,
}
impl Default for ResidentState {
    fn default() -> Self {
        Self {
            reducer: IndexedState::new(),
            cold: ColdFrames::default(),
            pending: HashMap::new(),
        }
    }
}
impl Deref for ResidentState {
    type Target = IndexedState<JsonlPayload>;
    fn deref(&self) -> &Self::Target {
        &self.reducer
    }
}
impl DerefMut for ResidentState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reducer
    }
}
impl ResidentState {
    #[cfg(test)]
    pub(super) fn assert_cold_shared(&self, ids: &[String]) -> Arc<dyn ReadFile> {
        assert!(
            self.pending.is_empty(),
            "acknowledged appends must consume pending locations"
        );
        let mut reader = None;
        for id in ids {
            let Some(JsonlPayload::Cold { source, .. }) = self.entry_ref(id) else {
                panic!("entry {id} must retain only a cold payload");
            };
            if let Some(first) = &reader {
                assert!(
                    Arc::ptr_eq(first, &source.reader),
                    "one generation must share one reader Arc"
                );
            } else {
                reader = Some(source.reader.clone());
            }
        }
        reader.expect("nonempty entry ids")
    }

    pub fn apply_mutation(&mut self, mutation: SessionMutation) -> SessionResult<ApplyOutcome> {
        let source = self.pending.remove(&mutation.seq());
        self.apply_source(mutation, source)
    }
    fn apply_source(
        &mut self,
        mutation: SessionMutation,
        source: Option<Arc<Location>>,
    ) -> SessionResult<ApplyOutcome> {
        if let (
            Some(source),
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            },
        ) = (&source, &mutation)
        {
            if frame.op != AssistantFrameOp::CommitStream {
                self.reducer.reserve_cold_frame(frame)?;
                self.cold.locations.push(source.clone());
                return Ok(ApplyOutcome::Record(LaneRecord::AssistantFrame(
                    frame.clone(),
                )));
            }
        }
        self.reducer
            .apply_mutation(mutation, |entry| match &source {
                Some(source) => JsonlPayload::Cold {
                    source: source.clone(),
                    parent: entry.parent_id().map(str::to_owned),
                    live_nulls: LiveNulls::capture(entry),
                },
                None => JsonlPayload::Resident(Arc::new(entry.clone())),
            })
    }
    pub fn apply_loaded(
        &mut self,
        mutation: SessionMutation,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        raw: &[u8],
    ) -> SessionResult<()> {
        let source = Arc::new(Location::new(reader.clone(), offset, raw, &mutation));
        self.apply_source(mutation, Some(source)).map(|_| ())
    }
    pub fn bind_append(&mut self, receipt: AppendReceipt, bytes: &[u8]) {
        let mut offset = receipt.offset;
        for raw in bytes.split_inclusive(|byte| *byte == b'\n') {
            if let Ok(line) = std::str::from_utf8(raw) {
                if let Ok(mutation) = parse_mutation(line) {
                    if matches!(
                        mutation,
                        SessionMutation::Entry { .. } | SessionMutation::Record { .. }
                    ) {
                        let source = Arc::new(Location::new(
                            receipt.reader.clone(),
                            offset,
                            raw,
                            &mutation,
                        ));
                        // Candidate batches have already applied entries; replace
                        // only their payload slots, never replay their mutations.
                        if let SessionMutation::Entry { entry, .. } = &mutation {
                            if let Some(index) = self.reducer.entry_index(entry.id()) {
                                let parent =
                                    self.reducer.entry_meta(index).unwrap().parent_id.clone();
                                // The decoded mutation has already lost message
                                // Some(Null). Preserve the actual candidate's
                                // live semantics instead of inspecting that row.
                                let live_nulls = match self.reducer.payload(index).unwrap() {
                                    JsonlPayload::Resident(entry) => LiveNulls::capture(entry),
                                    JsonlPayload::Cold { live_nulls, .. } => live_nulls.clone(),
                                };
                                *self.reducer.payload_mut(index).unwrap() = JsonlPayload::Cold {
                                    source,
                                    parent,
                                    live_nulls,
                                };
                                offset += raw.len() as u64;
                                continue;
                            }
                        }
                        self.pending.insert(mutation.seq(), source);
                    }
                }
            }
            offset += raw.len() as u64;
        }
    }
    #[cfg(test)]
    pub fn defer(
        &mut self,
        frame: &AssistantFrameRecord,
        reader: &Arc<dyn ReadFile>,
        offset: u64,
        line: &str,
    ) -> SessionResult<()> {
        self.apply_loaded(
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame.clone()),
            },
            reader,
            offset,
            line.as_bytes(),
        )
    }
    pub fn entry_ref(&self, id: &str) -> Option<JsonlPayload> {
        self.reducer
            .entry_index(id)
            .and_then(|i| self.reducer.payload(i))
            .cloned()
    }
    fn refs(&self, indices: impl IntoIterator<Item = usize>) -> Vec<JsonlPayload> {
        indices
            .into_iter()
            .map(|i| self.reducer.payload(i).unwrap().clone())
            .collect()
    }
    pub fn entry_plan(
        &self,
        query: &EntryQuery,
        branch: Option<(&BranchBounds, &str)>,
    ) -> SessionResult<EntryPlan> {
        let mut indices = self.reducer.select_entry_indices(query, branch)?;
        if query.tool_call_id.is_none() && query.outbox_key.is_none() {
            indices.truncate(query.limit.unwrap_or(usize::MAX));
        }
        Ok(EntryPlan {
            selected: self.refs(indices),
            query: query.clone(),
        })
    }
    pub fn context_plan(&self, bounds: &BranchBounds, start: &str) -> SessionResult<ContextPlan> {
        let query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        };
        let path = self
            .reducer
            .select_entry_indices(&query, Some((bounds, start)))?;
        let mut facts = crate::session::context::SessionContext::default();
        facts.thinking_level = "off".into();
        let mut thinking = false;
        for &i in path.iter().rev() {
            let meta = self.reducer.entry_meta(i).unwrap();
            if !thinking {
                if let Some(level) = &meta.thinking_level {
                    facts.thinking_level = level.clone();
                    thinking = true;
                }
            }
            if facts.model.is_none() {
                facts.model = meta.model_change.clone().or_else(|| {
                    Some((
                        meta.assistant_provider.clone()?,
                        meta.assistant_model.clone()?,
                    ))
                });
            }
            if facts.active_tool_names.is_none() {
                facts.active_tool_names = meta.active_tools.clone();
            }
        }
        let first = path
            .iter()
            .rposition(|&i| self.reducer.entry_meta(i).unwrap().kind == "compaction")
            .unwrap_or(0);
        Ok(ContextPlan {
            selected: self.refs(path.into_iter().skip(first)),
            facts,
        })
    }
    pub fn summary_plan(
        &self,
        lane: &str,
        query: &MessageSummaryQuery,
    ) -> SessionResult<SummaryPlan> {
        let selected = self.reducer.select_message_summaries(lane, query)?;
        Ok(SummaryPlan {
            selected: self.refs(selected.indices),
            has_more: selected.has_more,
            leaf_id: selected
                .leaf
                .map(|i| self.reducer.entry_meta(i).unwrap().id.clone()),
        })
    }
    pub fn window_plan(&self, lane: &str, query: &MessageWindowQuery) -> SessionResult<WindowPlan> {
        let selected = self.reducer.select_message_window(lane, query)?;
        Ok(WindowPlan {
            selected: self.refs(selected.indices),
            before: selected.has_more_before,
            after: selected.has_more_after,
            leaf_id: selected
                .leaf
                .map(|i| self.reducer.entry_meta(i).unwrap().id.clone()),
        })
    }
    pub fn record_plan(&self, query: &RecordQuery) -> SessionResult<RecordPlan> {
        let hot = self.reducer.find_records(query)?;
        let mut selected: Vec<_> = hot.into_iter().map(Selected::Hot).collect();
        if !query
            .record_type
            .is_some_and(|kind| kind != "assistant_frame")
            && query.operation_kind.is_none()
            && query.queue_kind.is_none()
            && query.target_entry_id.is_none()
            && query.tool_call_id.is_none()
            && !query.pending_queue_only
        {
            let oldest = query.order == Some(EntryOrder::OldestFirst);
            let iter: Box<dyn Iterator<Item = &Arc<Location>> + '_> = if oldest {
                Box::new(self.cold.locations.iter())
            } else {
                Box::new(self.cold.locations.iter().rev())
            };
            selected.extend(
                iter.filter(|loc| {
                    query.lane.as_ref().is_none_or(|v| v == &loc.lane)
                        && query.run_id.as_ref().is_none_or(|v| v == &loc.run)
                        && query.after_seq.is_none_or(|v| loc.seq > v)
                })
                .take(query.limit.unwrap_or(usize::MAX))
                .cloned()
                .map(Selected::Cold),
            );
        }
        selected.sort_unstable_by_key(|item| item.seq(LaneRecord::seq));
        if query.order != Some(EntryOrder::OldestFirst) {
            selected.reverse();
        }
        selected.truncate(query.limit.unwrap_or(usize::MAX));
        Ok(RecordPlan { selected })
    }
    pub fn page_plan(&self, query: &RecordPageQuery) -> SessionResult<(RecordPlan, usize)> {
        let hot = self.reducer.record_page(query)?;
        let matching = |loc: &&Arc<Location>| {
            loc.lane == query.lane
                && query.after_seq.is_none_or(|v| loc.seq > v)
                && query.before_seq.is_none_or(|v| loc.seq < v)
        };
        let total = hot.total + self.cold.locations.iter().filter(matching).count();
        let mut selected: Vec<_> = hot.records.into_iter().map(Selected::Hot).collect();
        let iter: Box<dyn Iterator<Item = &Arc<Location>> + '_> = if query.from_end {
            Box::new(self.cold.locations.iter().rev())
        } else {
            Box::new(self.cold.locations.iter())
        };
        selected.extend(
            iter.filter(matching)
                .take(query.limit)
                .cloned()
                .map(Selected::Cold),
        );
        selected.sort_unstable_by_key(|item| item.seq(LaneRecord::seq));
        if query.from_end {
            selected.drain(..selected.len().saturating_sub(query.limit));
        } else {
            selected.truncate(query.limit);
        }
        Ok((RecordPlan { selected }, total))
    }
    pub fn log_plan(&self, options: &LogOptions) -> SessionResult<LogPlan> {
        let mut selected = Vec::new();
        for item in self.reducer.log_items(options)? {
            selected.push(match item {
                StoredLogItem::Entry(i) => LogSelected::Entry(
                    self.reducer.entry_meta(*i).unwrap().seq,
                    self.reducer.payload(*i).unwrap().clone(),
                ),
                StoredLogItem::Record(i) => {
                    let record = self.reducer.records()[*i].clone();
                    LogSelected::Hot(LogItem::Record {
                        seq: record.seq(),
                        record,
                    })
                }
                StoredLogItem::Other(item) => LogSelected::Hot((**item).clone()),
            });
        }
        selected.extend(
            self.cold
                .locations
                .iter()
                .filter(|loc| options.after_seq.is_none_or(|v| loc.seq > v))
                .take(options.limit.unwrap_or(usize::MAX))
                .cloned()
                .map(LogSelected::Cold),
        );
        selected.sort_unstable_by_key(LogSelected::seq);
        selected.truncate(options.limit.unwrap_or(usize::MAX));
        Ok(LogPlan { selected })
    }
    pub fn fork_plan(&self, options: &ForkOptions) -> SessionResult<ForkPlan> {
        let (indices, lanes) = match options {
            ForkOptions::Tree => (
                (0..self.reducer.metadata().len()).collect::<Vec<_>>(),
                self.reducer.get_lanes(),
            ),
            ForkOptions::Branch { entry_id, position } => {
                let selected = entry_id.clone().or(self.reducer.require_lane("main")?);
                let target = if let Some(id) = selected {
                    let i = self
                        .reducer
                        .entry_index(&id)
                        .ok_or_else(|| SessionError::not_found(format!("Entry not found: {id}")))?;
                    let meta = self.reducer.entry_meta(i).unwrap();
                    if meta.kind != "message" {
                        return Err(SessionError::invalid_fork_target(format!(
                            "Fork target is not a message entry: {id}"
                        )));
                    }
                    match position.unwrap_or(if entry_id.is_some() {
                        ForkPosition::Before
                    } else {
                        ForkPosition::At
                    }) {
                        ForkPosition::At => Some(id),
                        ForkPosition::Before => meta.parent_id.clone(),
                    }
                } else {
                    None
                };
                let mut indices = self
                    .reducer
                    .walk_to_root(target.as_deref(), &BranchBounds::default())?;
                indices.reverse();
                (
                    indices,
                    vec![LanePointer {
                        lane: "main".into(),
                        leaf_id: target,
                    }],
                )
            }
        };
        let mut labels = Vec::new();
        for &i in &indices {
            let id = &self.reducer.entry_meta(i).unwrap().id;
            if let Some(label) = self.reducer.get_label(id) {
                labels.push((id.clone(), label.to_owned()));
            }
        }
        Ok(ForkPlan {
            entries: self.refs(indices),
            lanes,
            labels,
            name: self.reducer.get_name().map(str::to_owned),
        })
    }
}
pub(super) struct ForkPlan {
    pub entries: Vec<JsonlPayload>,
    pub lanes: Vec<LanePointer>,
    pub labels: Vec<(String, String)>,
    pub name: Option<String>,
}
pub(super) struct EntryPlan {
    selected: Vec<JsonlPayload>,
    query: EntryQuery,
}
impl EntryPlan {
    pub async fn read(self) -> SessionResult<Vec<Entry>> {
        let mut entries = Vec::new();
        for payload in self.selected {
            let entry = payload.read().await?;
            if matches_entry_payload(&entry, &self.query) {
                entries.push(entry);
                if self.query.limit.is_some_and(|limit| entries.len() >= limit) {
                    break;
                }
            }
        }
        Ok(entries)
    }
}
pub(super) struct ContextPlan {
    selected: Vec<JsonlPayload>,
    facts: crate::session::context::SessionContext,
}
impl ContextPlan {
    pub async fn read(mut self) -> SessionResult<crate::session::context::SessionContext> {
        let mut entries = Vec::with_capacity(self.selected.len());
        for payload in self.selected {
            entries.push(payload.read().await?);
        }
        self.facts.messages =
            crate::session::context::build_session_context_owned(entries, &Default::default())
                .messages;
        Ok(self.facts)
    }
}
pub(super) struct SummaryPlan {
    selected: Vec<JsonlPayload>,
    has_more: bool,
    leaf_id: Option<String>,
}
impl SummaryPlan {
    pub async fn read(self) -> SessionResult<MessageSummaryPage> {
        let mut messages = Vec::new();
        for payload in self.selected {
            let entry = payload.read().await?;
            let Some(rpi_agent::message::AgentMessage::User(user)) = entry.as_message() else {
                return Err(SessionError::invalid_entry("user index invariant"));
            };
            let (preview, has_images) = crate::session::message_preview::preview(&user.content);
            messages.push(MessageSummary {
                entry_id: entry.id().into(),
                role: "user".into(),
                preview,
                timestamp: user.timestamp,
                has_images,
            });
        }
        let first_id = messages.first().map(|m| m.entry_id.clone());
        let last_id = messages.last().map(|m| m.entry_id.clone());
        Ok(MessageSummaryPage {
            filtered: true,
            role: "user".into(),
            next_before_id: self.has_more.then(|| first_id.clone()).flatten(),
            first_id,
            last_id,
            messages,
            has_more: self.has_more,
            leaf_id: self.leaf_id,
        })
    }
}
pub(super) struct WindowPlan {
    selected: Vec<JsonlPayload>,
    before: bool,
    after: bool,
    leaf_id: Option<String>,
}
impl WindowPlan {
    pub async fn read(self) -> SessionResult<MessageWindowPage> {
        let mut entries = Vec::new();
        for payload in self.selected {
            entries.push(payload.read().await?);
        }
        let first_id = entries.first().map(|e| e.id().to_owned());
        let last_id = entries.last().map(|e| e.id().to_owned());
        Ok(MessageWindowPage {
            next_before_id: self.before.then(|| first_id.clone()).flatten(),
            next_after_id: self.after.then(|| last_id.clone()).flatten(),
            first_id,
            last_id,
            entries,
            has_more_before: self.before,
            has_more_after: self.after,
            leaf_id: self.leaf_id,
        })
    }
}
enum Selected<T> {
    Hot(T),
    Cold(Arc<Location>),
}
impl<T> Selected<T> {
    fn seq(&self, hot: impl FnOnce(&T) -> u64) -> u64 {
        match self {
            Self::Hot(v) => hot(v),
            Self::Cold(loc) => loc.seq,
        }
    }
}
pub(super) struct RecordPlan {
    selected: Vec<Selected<LaneRecord>>,
}
enum LogSelected {
    Hot(LogItem),
    Entry(u64, JsonlPayload),
    Cold(Arc<Location>),
}
impl LogSelected {
    fn seq(&self) -> u64 {
        match self {
            Self::Hot(item) => item.seq(),
            Self::Entry(seq, _) => *seq,
            Self::Cold(loc) => loc.seq,
        }
    }
}
pub(super) struct LogPlan {
    selected: Vec<LogSelected>,
}
// Coalesce selected frame ranges from the same reader generation, without a
// retained cache. A large individual row is permitted; combined reads are bounded.
async fn read_frames(locations: Vec<Arc<Location>>) -> SessionResult<HashMap<u64, LaneRecord>> {
    let mut locations = locations;
    locations
        .sort_unstable_by_key(|loc| (Arc::as_ptr(&loc.reader) as *const () as usize, loc.offset));
    let mut records = HashMap::new();
    let mut start = 0;
    while start < locations.len() {
        let first = &locations[start];
        let mut end = start + 1;
        let mut range_end = first.offset + first.length as u64;
        while end < locations.len() {
            let next = &locations[end];
            let next_end = next.offset + next.length as u64;
            if !Arc::ptr_eq(&first.reader, &next.reader)
                || next.offset.saturating_sub(range_end) > 8192
                || next_end.saturating_sub(first.offset) > 256 * 1024
            {
                break;
            }
            range_end = next_end;
            end += 1;
        }
        let length = usize::try_from(range_end - first.offset)
            .map_err(|_| SessionError::storage("cold history range overflow"))?;
        let bytes = file_result(
            first.reader.read_range(first.offset, length).await,
            "read cold history range",
        )?;
        if bytes.len() != length {
            return Err(SessionError::storage("short cold history read"));
        }
        for loc in &locations[start..end] {
            let offset = (loc.offset - first.offset) as usize;
            let SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            } = loc.decode(&bytes[offset..offset + loc.length])?
            else {
                return Err(SessionError::invalid_entry(
                    "cold history frame identity mismatch",
                ));
            };
            if frame.op == AssistantFrameOp::CommitStream {
                return Err(SessionError::invalid_entry(
                    "cold history frame identity mismatch",
                ));
            }
            records.insert(loc.seq, LaneRecord::AssistantFrame(frame));
        }
        start = end;
    }
    Ok(records)
}
impl RecordPlan {
    pub async fn read(self) -> SessionResult<Vec<LaneRecord>> {
        let mut frames = read_frames(
            self.selected
                .iter()
                .filter_map(|item| {
                    if let Selected::Cold(loc) = item {
                        Some(loc.clone())
                    } else {
                        None
                    }
                })
                .collect(),
        )
        .await?;
        self.selected
            .into_iter()
            .map(|item| match item {
                Selected::Hot(record) => Ok(record),
                Selected::Cold(loc) => frames
                    .remove(&loc.seq)
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
        let mut frames = read_frames(
            self.selected
                .iter()
                .filter_map(|item| {
                    if let LogSelected::Cold(loc) = item {
                        Some(loc.clone())
                    } else {
                        None
                    }
                })
                .collect(),
        )
        .await?;
        let mut result = Vec::new();
        for item in self.selected {
            result.push(match item {
                LogSelected::Hot(item) => item,
                LogSelected::Entry(seq, payload) => LogItem::Entry {
                    seq,
                    entry: payload.read().await?,
                },
                LogSelected::Cold(loc) => LogItem::Record {
                    seq: loc.seq,
                    record: frames
                        .remove(&loc.seq)
                        .ok_or_else(|| SessionError::storage("missing cold record"))?,
                },
            });
        }
        Ok(result)
    }
}
#[cfg(test)]
#[path = "cold_frames_tests.rs"]
mod tests;
