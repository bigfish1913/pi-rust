use rpi_harness::{
    error::{SessionError, SessionResult},
    session::types::*,
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::Notify;

pub struct Probe {
    pub inner: Arc<dyn SessionStorage>,
    pub mode: AtomicUsize,
    pub fail_reads: AtomicBool,
    pub fail_revision: AtomicBool,
    pub fail_lanes: AtomicBool,
    pub index_scan_override: Mutex<Option<Vec<LaneRecord>>>,
    pub pause_legacy_source: AtomicBool,
    pub legacy_source_captured: Notify,
    pub release_legacy_source: Notify,
    pub legacy_source_override: Mutex<Option<Vec<Entry>>>,
    pub pause_queue_reads: AtomicBool,
    pub queue_read_entered: Notify,
    pub release_queue_read: Notify,
    pub pause_branch_reads: AtomicBool,
    pub branch_read_entered: Notify,
    pub release_branch_read: Notify,
    pub pause_index_scans: AtomicBool,
    pub index_scan_captured: Notify,
    pub release_index_scan: Notify,
    pub fail_pages: AtomicBool,
    pub pause_execution_pages: AtomicBool,
    pub execution_page_captured: Notify,
    pub release_execution_page: Notify,
    pub execution_page_override: Mutex<Option<RecordPage>>,
    pub attempts: AtomicUsize,
    pub commits: AtomicUsize,
    pub entered: Notify,
    pub committed: Notify,
    pub release: Notify,
    pub drain_calls: AtomicUsize,
    pub pause_drains: AtomicBool,
    pub drain_entered: Notify,
    pub release_drain: Notify,
}
impl Probe {
    pub fn new(inner: Arc<dyn SessionStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            mode: AtomicUsize::new(0),
            fail_reads: AtomicBool::new(false),
            fail_revision: AtomicBool::new(false),
            fail_lanes: AtomicBool::new(false),
            index_scan_override: Mutex::new(None),
            pause_legacy_source: AtomicBool::new(false),
            legacy_source_captured: Notify::new(),
            release_legacy_source: Notify::new(),
            legacy_source_override: Mutex::new(None),
            pause_queue_reads: AtomicBool::new(false),
            queue_read_entered: Notify::new(),
            release_queue_read: Notify::new(),
            pause_branch_reads: AtomicBool::new(false),
            branch_read_entered: Notify::new(),
            release_branch_read: Notify::new(),
            pause_index_scans: AtomicBool::new(false),
            index_scan_captured: Notify::new(),
            release_index_scan: Notify::new(),
            fail_pages: AtomicBool::new(false),
            pause_execution_pages: AtomicBool::new(false),
            execution_page_captured: Notify::new(),
            release_execution_page: Notify::new(),
            execution_page_override: Mutex::new(None),
            attempts: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
            entered: Notify::new(),
            committed: Notify::new(),
            release: Notify::new(),
            drain_calls: AtomicUsize::new(0),
            pause_drains: AtomicBool::new(false),
            drain_entered: Notify::new(),
            release_drain: Notify::new(),
        })
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
    fn writer_lease_held(&self) -> bool {
        self.inner.writer_lease_held()
    }
    fn release_writer_lease(&self) {
        self.inner.release_writer_lease();
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        if self.fail_lanes.load(Ordering::SeqCst) {
            return Err(SessionError::storage("injected lane pointer read failure"));
        }
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.inner.create_lane(lane, at).await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.inner.move_lane(lane, to).await
    }
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.append_progress(records).await
    }
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.read_progress(run_id).await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.inner.prune_completed_progress().await
    }
    async fn history_revision(&self) -> SessionResult<u64> {
        if self.fail_revision.load(Ordering::SeqCst) {
            return Err(SessionError::storage("injected revision failure"));
        }
        self.inner.history_revision().await
    }
    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.swap(0, Ordering::SeqCst);
        self.entered.notify_one();
        if mode == 1 {
            return Err(SessionError::storage("before commit"));
        }
        if mode == 3 {
            self.release.notified().await;
        }
        let result = self
            .inner
            .append_entry_if_revision(entry, lane, expected)
            .await?;
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.committed.notify_one();
        if mode == 4 {
            self.release.notified().await;
        }
        if mode == 2 {
            return Err(SessionError::storage("lost commit ACK"));
        }
        Ok(result)
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.inner.append_record(record).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.inner.get_entry(id).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.find_entries(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(SessionError::storage("injected branch read failure"));
        }
        if query.custom_type.as_deref() == Some("rpi.durable.control.v1")
            && self.pause_branch_reads.swap(false, Ordering::SeqCst)
        {
            self.branch_read_entered.notify_one();
            self.release_branch_read.notified().await;
        }
        let entries = self
            .inner
            .find_entries_on_branch(query, bounds, start)
            .await?;
        // Test-only post-capture pause for the explicit legacy source query.
        // All flags default off, preserving every prior probe user.
        if query.entry_type == Some("custom")
            && query.custom_type.is_none()
            && query.limit == Some(257)
        {
            if self.pause_legacy_source.swap(false, Ordering::SeqCst) {
                self.legacy_source_captured.notify_one();
                self.release_legacy_source.notified().await;
            }
            if let Some(overridden) = self.legacy_source_override.lock().unwrap().take() {
                return Ok(overridden);
            }
        }
        Ok(entries)
    }

    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        if query.record_type == Some("queue_enqueued")
            && self.pause_queue_reads.swap(false, Ordering::SeqCst)
        {
            self.queue_read_entered.notify_one();
            self.release_queue_read.notified().await;
        }
        let records = self.inner.find_records(query).await?;
        // Capture the actual bounded index scan before pausing. Other reads,
        // including explicit per-run lookup, do not enter this test-only hook.
        if query.record_type == Some("operation_started")
            && query.run_id.is_none()
            && query.limit == Some(65)
        {
            if self.pause_index_scans.swap(false, Ordering::SeqCst) {
                self.index_scan_captured.notify_one();
                self.release_index_scan.notified().await;
            }
            // Test-only, one-shot malformed backend response. The real inner
            // scan has already occurred; do not alter facts or per-run lookups.
            if let Some(overridden) = self.index_scan_override.lock().unwrap().take() {
                return Ok(overridden);
            }
        }
        Ok(records)
    }
    async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage> {
        let execution_page = query.limit == 65 && query.from_end && query.after_seq.is_none();
        if execution_page && self.fail_pages.load(Ordering::SeqCst) {
            return Err(SessionError::storage(
                "injected execution page read failure",
            ));
        }
        let page = self.inner.record_page(query).await?;
        if execution_page {
            if self.pause_execution_pages.swap(false, Ordering::SeqCst) {
                self.execution_page_captured.notify_one();
                self.release_execution_page.notified().await;
            }
            if let Some(overridden) = self.execution_page_override.lock().unwrap().take() {
                return Ok(overridden);
            }
        }
        Ok(page)
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
        self.inner.set_name(name).await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.inner.set_label(id, label).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
    async fn drain(&self) {
        self.drain_calls.fetch_add(1, Ordering::SeqCst);
        self.drain_entered.notify_one();
        if self.pause_drains.swap(false, Ordering::SeqCst) {
            self.release_drain.notified().await;
        }
        self.inner.drain().await
    }
}
