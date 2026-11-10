//! Independent forwarding probe; stage 3 pauses AFTER the actual revision result.
//! Counters reset only between operations; all writes forward to the real backend.
use rpi_harness::{
    error::{SessionError, SessionResult},
    session::types::*,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;
pub struct Probe {
    pub revision_calls: AtomicUsize,
    pub lane_calls: AtomicUsize,
    pub pause_revision_at: AtomicUsize,
    pub fail_revision_at: AtomicUsize,
    pub fail_lane_at: AtomicUsize,
    pub inner: Arc<dyn SessionStorage>,
    pub entries: Mutex<HashMap<String, Option<Entry>>>,
    pub revision: AtomicU64,
    pub fail_reads: AtomicBool,
    pub fail_revision: AtomicBool,
    pub fail_lanes: AtomicBool,
    pub pause: AtomicBool,
    pub captured: Notify,
    pub release: Notify,
    pub reads: AtomicUsize,
    pub writes: AtomicUsize,
}
impl Probe {
    pub fn new(inner: Arc<dyn SessionStorage>) -> Arc<Self> {
        Arc::new(Self {
            revision_calls: AtomicUsize::new(0),
            lane_calls: AtomicUsize::new(0),
            pause_revision_at: AtomicUsize::new(0),
            fail_revision_at: AtomicUsize::new(0),
            fail_lane_at: AtomicUsize::new(0),
            inner,
            entries: Mutex::new(HashMap::new()),
            revision: AtomicU64::new(0),
            fail_reads: AtomicBool::new(false),
            fail_revision: AtomicBool::new(false),
            fail_lanes: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            captured: Notify::new(),
            release: Notify::new(),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        })
    }
    pub fn reset(&self) {
        self.revision_calls.store(0, Ordering::SeqCst);
        self.lane_calls.store(0, Ordering::SeqCst);
        self.writes.store(0, Ordering::SeqCst);
        self.reads.store(0, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl SessionStorage for Probe {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    fn is_ephemeral(&self) -> bool {
        self.inner.is_ephemeral()
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.create_lane(lane, at).await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.move_lane(lane, to).await
    }
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.append_progress(records).await
    }
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.read_progress(run_id).await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.prune_completed_progress().await
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.append_record(record).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.find_entries(query).await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.get_log(options).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_name(name).await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_label(id, label).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let captured = self.inner.get_entry(id).await;
        if self.pause.swap(false, Ordering::SeqCst) {
            self.captured.notify_one();
            self.release.notified().await;
        }
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(SessionError::storage("injected entry read failure"));
        }
        if let Some(entry) = self.entries.lock().unwrap().get(id).cloned() {
            return Ok(entry);
        }
        captured
    }
    async fn history_revision(&self) -> SessionResult<u64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let call = self.revision_calls.fetch_add(1, Ordering::SeqCst) + 1;
        // Capture the ACTUAL backend result before notifying, never a branch-query condition.
        let actual = self.inner.history_revision().await;
        if self.pause_revision_at.load(Ordering::SeqCst) == call {
            self.captured.notify_one();
            self.release.notified().await;
        }
        if self.fail_revision.load(Ordering::SeqCst)
            || self.fail_revision_at.load(Ordering::SeqCst) == call
        {
            return Err(SessionError::storage("injected revision failure"));
        }
        let revision = self.revision.load(Ordering::SeqCst);
        if revision != 0 {
            return Ok(revision);
        }
        actual
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let call = self.lane_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_lanes.load(Ordering::SeqCst)
            || self.fail_lane_at.load(Ordering::SeqCst) == call
        {
            return Err(SessionError::storage("injected lane failure"));
        }
        self.inner.get_lanes().await
    }
    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner
            .append_entry_if_revision(entry, lane, expected)
            .await
    }
    async fn append_record_if_revision(
        &self,
        record: LaneRecord,
        expected: u64,
    ) -> SessionResult<LaneRecord> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.append_record_if_revision(record, expected).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.inner
            .find_entries_on_branch(query, bounds, start)
            .await
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        self.inner.find_records(query).await
    }
    async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage> {
        self.inner.record_page(query).await
    }
    async fn drain(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.drain().await
    }
}
