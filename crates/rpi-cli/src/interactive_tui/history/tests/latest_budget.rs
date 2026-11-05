//! The `Latest` window must be bounded at the query boundary.
//!
//! `LATEST_ENTRY_BUDGET` is documented as a *memory* guard, and that claim only
//! holds if the backend is asked for a bounded window. With `limit: None` the
//! jsonl backend walks the whole branch, builds a payload reference for every
//! entry and hydrates all of them before `latest_page` slices the tail off — a
//! 100k-entry session allocates 100k entries and the constant bounds nothing.
//! The same read also has to stay one walk: `branch_entry_page` re-walks the
//! branch per entry, which is why `latest_page` exists at all.
//!
//! The probe wraps the real tree in a pass-through `SessionTree` that records
//! the queries `latest_page` issues, then asserts on the *query shape*. A
//! recorder that answers exactly what the inner tree answered cannot change
//! behaviour, so a failure is a fact about the query, not about the double.

use super::*;
use rpi_harness::session::types::{
    BranchBounds, Entry, EntryAncestry, EntryQuery, MessageSummaryPage, MessageSummaryQuery,
    MessageWindowPage, MessageWindowQuery, SessionStats, SessionTree, TranscriptPage,
    TranscriptPageQuery,
};

/// Queries `latest_page` issued, in call order.
type Queries = Arc<Mutex<Vec<EntryQuery>>>;

/// Pass-through `SessionTree` that records every `find_entries_on_branch` query.
struct RecordingTree {
    inner: Arc<dyn SessionTree>,
    queries: Queries,
}

#[async_trait::async_trait]
impl SessionTree for RecordingTree {
    async fn get_leaf_id(&self) -> rpi_harness::error::SessionResult<Option<String>> {
        self.inner.get_leaf_id().await
    }
    async fn find_entries(
        &self,
        query: &EntryQuery,
    ) -> rpi_harness::error::SessionResult<Vec<Entry>> {
        self.inner.find_entries(query).await
    }
    async fn find_entry(
        &self,
        query: &EntryQuery,
    ) -> rpi_harness::error::SessionResult<Option<Entry>> {
        self.inner.find_entry(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> rpi_harness::error::SessionResult<Vec<Entry>> {
        self.queries.lock().unwrap().push(query.clone());
        self.inner.find_entries_on_branch(query, bounds).await
    }
    async fn branch_ancestry(
        &self,
        bounds: &BranchBounds,
    ) -> rpi_harness::error::SessionResult<Vec<EntryAncestry>> {
        self.inner.branch_ancestry(bounds).await
    }
    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> rpi_harness::error::SessionResult<Option<Entry>> {
        self.inner.find_entry_on_branch(query, bounds).await
    }
    async fn transcript_page(
        &self,
        query: &TranscriptPageQuery,
    ) -> rpi_harness::error::SessionResult<Option<TranscriptPage>> {
        self.inner.transcript_page(query).await
    }
    async fn message_summaries(
        &self,
        query: &MessageSummaryQuery,
    ) -> rpi_harness::error::SessionResult<MessageSummaryPage> {
        self.inner.message_summaries(query).await
    }
    async fn message_window(
        &self,
        query: &MessageWindowQuery,
    ) -> rpi_harness::error::SessionResult<MessageWindowPage> {
        self.inner.message_window(query).await
    }
    async fn get_entry(&self, id: &str) -> rpi_harness::error::SessionResult<Option<Entry>> {
        self.inner.get_entry(id).await
    }
    async fn get_stats(&self) -> rpi_harness::error::SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
    async fn get_name(&self) -> rpi_harness::error::SessionResult<Option<String>> {
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> rpi_harness::error::SessionResult<()> {
        self.inner.set_name(name).await
    }
    async fn get_label(
        &self,
        target_id: &str,
    ) -> rpi_harness::error::SessionResult<Option<String>> {
        self.inner.get_label(target_id).await
    }
    async fn set_label(
        &self,
        target_id: &str,
        label: Option<&str>,
    ) -> rpi_harness::error::SessionResult<()> {
        self.inner.set_label(target_id, label).await
    }
    async fn append_message(
        &self,
        message: rpi_agent::message::AgentMessage,
    ) -> rpi_harness::error::SessionResult<String> {
        self.inner.append_message(message).await
    }
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> rpi_harness::error::SessionResult<String> {
        self.inner.append_custom_entry(custom_type, data).await
    }
}

/// Build `count` user messages and return a recording tree over them.
async fn recording_fixture(count: usize) -> (Arc<dyn SessionTree>, Queries) {
    let (storage, _state, _tui) = fixture(count).await;
    let tree: Arc<dyn SessionTree> = Session::new(storage, None).view("main");
    let queries = Queries::default();
    let recording: Arc<dyn SessionTree> = Arc::new(RecordingTree {
        inner: tree,
        queries: queries.clone(),
    });
    (recording, queries)
}

/// The advertised budget must reach the backend as a bound.
///
/// `LATEST_ENTRY_BUDGET` bounds nothing while the query carries `limit: None`:
/// the backend materialises the whole branch first and `latest_page` discards
/// everything but the tail afterwards.
#[tokio::test]
async fn latest_page_passes_the_budget_as_a_backend_limit() {
    let (tree, queries) = recording_fixture(123).await;
    let page = latest_page(&tree, 50).await.expect("latest page loads");
    let recorded = queries.lock().unwrap().clone();
    assert!(
        !recorded.is_empty(),
        "the window must come from a bounded read, not from slicing a full branch"
    );
    assert_eq!(
        recorded[0].limit,
        Some(50),
        "the budget has to reach the backend as a limit, otherwise it is applied \
         after the whole branch has already been materialised"
    );
    assert_eq!(
        recorded[0].order,
        Some(rpi_harness::session::types::EntryOrder::NewestFirst),
        "a bounded window must be read newest-first: oldest-first + limit returns \
         the START of the conversation, which is the opposite of the latest page"
    );
    // Truthfulness of `has_more_before` must not cost a full second walk: any
    // follow-up read is bounded to a single row and anchored at the window edge.
    for probe in &recorded[1..] {
        assert_eq!(
            probe.limit,
            Some(1),
            "a follow-up read may only peek one row past the window: {probe:?}"
        );
        assert!(
            probe.cursor.is_some(),
            "a follow-up read must be anchored at the window edge, not re-walk the \
             branch: {probe:?}"
        );
    }
    // The bounded read still has to produce the chronological tail.
    assert_eq!(page.entries.len(), 50);
    assert_eq!(
        page.entries.first().map(|entry| entry.id()),
        Some("entry-073")
    );
    assert_eq!(
        page.entries.last().map(|entry| entry.id()),
        Some("entry-122")
    );
    assert!(page.has_more_before, "73 older entries remain reachable");
    assert_eq!(page.last_id.as_deref(), Some("entry-122"));
    assert_eq!(page.next_before_id.as_deref(), Some("entry-073"));
}

/// A budget at or above the branch length is still exactly one bounded read,
/// and it must not claim unread history that does not exist.
#[tokio::test]
async fn latest_page_reports_no_hidden_history_when_the_branch_fits() {
    let (tree, queries) = recording_fixture(10).await;
    let page = latest_page(&tree, 50).await.expect("latest page loads");
    let recorded = queries.lock().unwrap().clone();
    assert_eq!(recorded[0].limit, Some(50));
    assert_eq!(
        recorded.len(),
        1,
        "a window that already contains the whole branch must not need a peek"
    );
    assert_eq!(page.entries.len(), 10);
    assert!(!page.has_more_before, "the whole branch is on screen");
    assert_eq!(page.first_id.as_deref(), Some("entry-000"));
    assert_eq!(page.last_id.as_deref(), Some("entry-009"));
    assert!(page.next_before_id.is_none());
}

/// An empty session must not issue a read for a leaf it does not have, and must
/// still produce a well-formed empty page.
#[tokio::test]
async fn latest_page_on_an_empty_session_is_an_empty_page() {
    let (tree, queries) = recording_fixture(0).await;
    let page = latest_page(&tree, 50).await.expect("latest page loads");
    assert!(page.entries.is_empty());
    assert!(!page.has_more_before);
    assert!(page.next_before_id.is_none());
    assert!(page.leaf_id.is_none());
    let recorded = queries.lock().unwrap().clone();
    assert!(
        recorded.is_empty() || recorded.iter().all(|query| query.limit == Some(50)),
        "no unbounded read may happen for an empty session: {recorded:?}"
    );
}

/// Boundary sweep on the same axis as the budget: the window size must be the
/// only thing that changes. A one-entry budget still shows the newest entry and
/// still reports everything older as reachable; a budget one short of the branch
/// must not claim hidden history.
#[tokio::test]
async fn latest_page_window_sweep_keeps_the_tail_and_more_before_truthful() {
    let (tree, queries) = recording_fixture(20).await;
    for (budget, expected_first, expected_more) in [
        (1usize, "entry-019", true),
        (2, "entry-018", true),
        (19, "entry-001", true),
        (20, "entry-000", false),
        (21, "entry-000", false),
    ] {
        let page = latest_page(&tree, budget).await.expect("latest page loads");
        assert_eq!(
            page.entries.first().map(|entry| entry.id()),
            Some(expected_first),
            "budget {budget}: the newest entry must never be dropped"
        );
        assert_eq!(
            page.entries.last().map(|entry| entry.id()),
            Some("entry-019"),
            "budget {budget}: the tail must always be present"
        );
        assert_eq!(
            page.has_more_before, expected_more,
            "budget {budget}: has_more_before must be truthful"
        );
        assert_eq!(
            page.next_before_id.is_some(),
            expected_more,
            "budget {budget}: the older cursor exists exactly when history remains"
        );
        // Every read stays bounded by the budget plus at most one peek row.
        for query in queries.lock().unwrap().iter() {
            assert!(
                query.limit.unwrap_or(usize::MAX) <= budget.max(1),
                "budget {budget}: unbounded read {query:?}"
            );
        }
    }
}

/// A zero budget must still be a positive, bounded request rather than panicking
/// or asking the backend for `limit: 0` (which is an invalid query).
#[tokio::test]
async fn latest_page_clamps_a_zero_budget_to_one_entry() {
    let (tree, queries) = recording_fixture(5).await;
    let page = latest_page(&tree, 0).await.expect("latest page loads");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].id(), "entry-004");
    assert!(page.has_more_before);
    let recorded = queries.lock().unwrap().clone();
    assert!(recorded
        .iter()
        .all(|query| query.limit.is_some_and(|l| l > 0)));
}

/// The memory claim, stated as a fact about how much the view materialises.
///
/// The reviewer's objection was that `LATEST_ENTRY_BUDGET` bounded nothing: a
/// 100k-entry session still decoded every entry, then threw all but the tail
/// away. Asserting on `page.entries.len()` cannot see that — the old code also
/// returned a `budget`-sized slice, it just materialised the branch first. So the
/// assertion is on how many rows the view ever *asks* the backend for: on a
/// branch 15x the budget, the total requested across every read must stay inside
/// the budget plus a single peek row.
#[tokio::test]
async fn latest_page_never_asks_for_more_than_the_budget() {
    const ENTRIES: usize = 600;
    const BUDGET: usize = 40;
    let (tree, queries) = recording_fixture(ENTRIES).await;
    let page = latest_page(&tree, BUDGET).await.expect("latest page loads");
    let recorded = queries.lock().unwrap().clone();
    let requested: usize = recorded
        .iter()
        .map(|query| query.limit.unwrap_or(ENTRIES))
        .sum();
    assert!(
        requested <= BUDGET + 1,
        "a {ENTRIES}-entry branch must not be materialised to show {BUDGET} rows:          {} read(s) requesting {requested} rows total: {recorded:?}",
        recorded.len()
    );
    // The window still has to be the tail, chronological, with truthful cursors.
    assert_eq!(page.entries.len(), BUDGET);
    assert!(page.has_more_before);
    assert_eq!(
        page.entries.first().map(|entry| entry.id()),
        Some("entry-560")
    );
    assert_eq!(
        page.entries.last().map(|entry| entry.id()),
        Some("entry-599")
    );
    assert_eq!(page.next_before_id.as_deref(), Some("entry-560"));
    assert!(page.next_after_id.is_none());
    assert!(!page.has_more_after);
}
