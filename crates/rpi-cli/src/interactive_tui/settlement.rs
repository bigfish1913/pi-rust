//! Private host-task ownership for normal native-TUI shutdown, not control state.
//! Finished handles are reaped at registration/seal, preserving join failures.
//! Outstanding concurrency is not bounded. Dropping settlement detaches handles
//! rather than aborting producers, and does not guarantee cleanup will finish.
use futures::FutureExt;
use rpi_harness::agent_harness::AgentHarness;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::task::{JoinError, JoinHandle};

#[derive(Default)]
struct TaskState {
    sealed: bool,
    tasks: Vec<JoinHandle<()>>,
    had_join_failure: bool,
}

impl TaskState {
    // Poll only handles already reporting completion, never a producer future.
    // Preserve a handle if even this poll is Pending (e.g. unusual tracing).
    // Error payloads may have user-defined Drop: return them for destruction
    // outside the collector lock rather than running foreign Drop under it.
    fn reap_finished(&mut self) -> Vec<JoinError> {
        let mut errors = Vec::new();
        self.tasks.retain_mut(|task| {
            if !task.is_finished() {
                return true;
            }
            match tokio::task::unconstrained(task).now_or_never() {
                Some(Ok(())) => false,
                Some(Err(error)) => {
                    errors.push(error);
                    false
                }
                None => true,
            }
        });
        self.had_join_failure |= !errors.is_empty();
        errors
    }
}

/// One shutdown owner receives both unfinished handles and earlier join failures.
#[derive(Default)]
pub(super) struct SealedTasks {
    tasks: Vec<JoinHandle<()>>,
    had_join_failure: bool,
}

/// Clones share one admission fence; only the shutdown owner consumes its handles.
#[derive(Clone, Default)]
pub(super) struct SessionWriteTasks {
    enabled: bool,
    state: Arc<Mutex<TaskState>>,
}

impl SessionWriteTasks {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    pub(super) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        if !self.enabled {
            // Preserve legacy detached spawning, even after seal().
            tokio::spawn(future);
            return true;
        }
        let mut state = self.state.lock().unwrap();
        if state.sealed {
            return false;
        }
        let reaped_errors = state.reap_finished();
        // Tokio never polls inline: registration and the seal check are atomic
        // with respect to other collector clones, without holding a lock at await.
        state.tasks.push(tokio::spawn(future));
        drop(state);
        drop(reaped_errors);
        true
    }

    pub(super) fn seal(&self) -> SealedTasks {
        if !self.enabled {
            return SealedTasks::default();
        }
        let mut state = self.state.lock().unwrap();
        state.sealed = true;
        let reaped_errors = state.reap_finished();
        let batch = SealedTasks {
            tasks: std::mem::take(&mut state.tasks),
            had_join_failure: std::mem::take(&mut state.had_join_failure),
        };
        drop(state);
        drop(reaped_errors);
        batch
    }
}

/// Normal shutdown only: join host producers before idle and the core barrier.
/// The caller gates this on native enablement and owns any fixed diagnostic.
/// No cancellation worker is installed: the runtime must remain alive, and a
/// dropped root does not certify join, backend drain, or terminal delivery.
pub(super) async fn settle_native(harness: &AgentHarness, batch: SealedTasks) -> bool {
    let mut ok = !batch.had_join_failure;
    for task in batch.tasks {
        if task.await.is_err() {
            ok = false;
        }
    }
    if harness.lane("main").wait_for_idle().await.is_err() {
        ok = false;
    }
    if crate::native_durable::drain(harness).await.is_err() {
        ok = false;
    }
    ok
}

#[cfg(test)]
#[path = "../../../rpi-harness/tests/support/durable_control_probe.rs"]
mod probe;

#[cfg(test)]
mod tests {
    use super::{probe::Probe, *};
    use rpi_harness::{
        durable::{DurableActor, DurableMutation, DurableReceipt, DurableService},
        session::{
            memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
            types::{Entry, EntryQuery, RecordQuery, SessionMetadata},
            Session,
        },
        types::AgentHarnessOptions,
    };
    use serde_json::json;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::sync::Notify;

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(15), future)
            .await
            .expect("native settlement boundary timed out")
    }

    struct Fixture {
        harness: AgentHarness,
        probe: Arc<Probe>,
        producer_entered: Arc<Notify>,
        producer_release: Arc<Notify>,
        producer_done: Arc<Notify>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Also release on assertion failure/timeout; never strand an owned
            // core worker at the append or backend barrier.
            self.producer_release.notify_one();
            self.probe.release.notify_one();
            self.probe.release_drain.notify_one();
        }
    }

    impl Fixture {
        async fn new() -> Self {
            let storage = InMemorySessionStorage::new(
                SessionMetadata {
                    id: "synthetic-native-tui-settlement".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            );
            let probe = Probe::new(Arc::new(storage));
            let harness = AgentHarness::create(AgentHarnessOptions {
                session: Session::new(probe.clone(), None),
                allow_existing_session: true,
                ..Default::default()
            })
            .await
            .unwrap();
            Self {
                harness,
                probe,
                producer_entered: Arc::new(Notify::new()),
                producer_release: Arc::new(Notify::new()),
                producer_done: Arc::new(Notify::new()),
            }
        }

        fn service(&self) -> DurableService {
            DurableService::bind(&self.harness, DurableActor::owner("native-cli").unwrap())
        }

        async fn mutation(&self) -> DurableMutation {
            DurableMutation {
                request_id: "synthetic-settlement-start".into(),
                expected: self.service().snapshot().await.unwrap().scope,
                action: serde_json::from_value(json!({
                    "action": "start", "name": "synthetic settlement",
                    "steps": [{"id": "s", "title": "Persist"}]
                }))
                .unwrap(),
                run_id: None,
            }
        }

        fn producer(
            &self,
            mutation: DurableMutation,
            pause_before_admission: bool,
        ) -> impl Future<Output = ()> + Send + 'static {
            let harness = self.harness.clone();
            let entered = self.producer_entered.clone();
            let release = self.producer_release.clone();
            let done = self.producer_done.clone();
            async move {
                entered.notify_one();
                if pause_before_admission {
                    release.notified().await;
                }
                DurableService::bind(&harness, DurableActor::owner("native-cli").unwrap())
                    .mutate(mutation)
                    .await
                    .unwrap();
                done.notify_one();
            }
        }

        async fn exact_receipt(&self, mutation: DurableMutation) {
            let service = self.service();
            let receipt = service
                .request_outcome(&mutation.request_id)
                .await
                .unwrap()
                .expect("accepted producer must persist its exact request");
            assert_eq!(receipt.actor, "native-cli");
            assert_eq!(receipt.request_id, mutation.request_id);
            assert_eq!(receipt.expected, mutation.expected);
            assert_eq!(receipt.action, mutation.action);
            assert_eq!(receipt.run_id, mutation.run_id);
            assert_eq!(receipt.control_revision, 1);
            assert!(receipt.changed);
            let entries = self
                .harness
                .session()
                .storage()
                .find_entries(&EntryQuery {
                    entry_type: Some("custom"),
                    custom_type: Some("rpi.durable.control.v1".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            let Entry::Custom(entry) = &entries[0] else {
                panic!("expected control entry");
            };
            let persisted: DurableReceipt = serde_json::from_value(
                entry.data.as_ref().unwrap()["receipts"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            assert_eq!(persisted, receipt);
            let replay = service.mutate(mutation).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.receipt, receipt);
            assert_eq!(self.probe.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(self.probe.commits.load(Ordering::SeqCst), 1);
            // No provider/tool/operation or invented terminal records.
            assert!(self
                .harness
                .session()
                .storage()
                .find_records(&RecordQuery::default())
                .await
                .unwrap()
                .is_empty());
            assert!(self
                .harness
                .session()
                .storage()
                .find_entries(&EntryQuery::default())
                .await
                .unwrap()
                .iter()
                .all(|entry| matches!(entry, Entry::Custom(_))));
        }
    }

    #[tokio::test]
    async fn clones_seal_unpolled_late_work_and_join_pre_admission_producer() {
        bounded(async {
            let fixture = Fixture::new().await;
            let mutation = fixture.mutation().await;
            let tasks = SessionWriteTasks::new(true);
            let clone = tasks.clone();
            assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
            fixture.producer_entered.notified().await;
            let handles = clone.seal();
            assert_eq!(handles.tasks.len(), 1);
            assert!(tasks.seal().tasks.is_empty());
            let polls = Arc::new(AtomicUsize::new(0));
            let body = polls.clone();
            assert!(!tasks.spawn(async move {
                body.fetch_add(1, Ordering::SeqCst);
            }));
            tokio::task::yield_now().await;
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            fixture.probe.pause_drains.store(true, Ordering::SeqCst);
            let mut settle = Box::pin(settle_native(&fixture.harness, handles));
            assert!(futures::poll!(settle.as_mut()).is_pending());
            // #18 fences admitted core work, not an outer task which has not
            // called mutate yet. The host join must precede the core barrier.
            assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
            fixture.producer_release.notify_one();
            // Poll settlement concurrently with the separately paused drain.
            tokio::select! {
                _ = &mut settle => panic!("settlement bypassed paused backend"),
                _ = fixture.probe.drain_entered.notified() => {}
            }
            assert!(futures::poll!(settle.as_mut()).is_pending());
            fixture.probe.release_drain.notify_one();
            assert!(settle.await);
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
            fixture.exact_receipt(mutation).await;
        })
        .await;
    }

    #[tokio::test]
    async fn real_append_and_commit_ack_boundaries_precede_separate_backend_drain() {
        bounded(async {
            for mode in [3, 4] {
                let fixture = Fixture::new().await;
                let mutation = fixture.mutation().await;
                fixture.probe.mode.store(mode, Ordering::SeqCst);
                fixture.probe.pause_drains.store(true, Ordering::SeqCst);
                let tasks = SessionWriteTasks::new(true);
                assert!(tasks.spawn(fixture.producer(mutation.clone(), false)));
                if mode == 3 {
                    fixture.probe.entered.notified().await;
                } else {
                    fixture.probe.committed.notified().await;
                }
                // Verify storage itself, not just a probe notification: mode 3
                // has not appended; mode 4 committed but has not returned ACK.
                assert_eq!(
                    fixture
                        .probe
                        .inner
                        .find_entries(&EntryQuery {
                            custom_type: Some("rpi.durable.control.v1".into()),
                            ..Default::default()
                        })
                        .await
                        .unwrap()
                        .len(),
                    usize::from(mode == 4)
                );
                assert_eq!(
                    fixture.probe.commits.load(Ordering::SeqCst),
                    usize::from(mode == 4)
                );
                let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
                assert!(futures::poll!(settle.as_mut()).is_pending());
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
                fixture.probe.release.notify_one();
                tokio::select! {
                    _ = &mut settle => panic!("settlement bypassed paused backend"),
                    _ = fixture.probe.drain_entered.notified() => {}
                }
                assert!(futures::poll!(settle.as_mut()).is_pending());
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
                fixture.probe.release_drain.notify_one();
                assert!(settle.await);
                fixture.exact_receipt(mutation).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn producer_panic_does_not_skip_later_join_or_backend_drain() {
        bounded(async {
            let fixture = Fixture::new().await;
            let mutation = fixture.mutation().await;
            let tasks = SessionWriteTasks::new(true);
            let panicked = Arc::new(Notify::new());
            let started = panicked.clone();
            assert!(tasks.spawn(async move {
                started.notify_one();
                panic!("synthetic producer panic");
            }));
            assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
            panicked.notified().await;
            fixture.producer_entered.notified().await;
            fixture.probe.pause_drains.store(true, Ordering::SeqCst);
            let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
            assert!(futures::poll!(settle.as_mut()).is_pending());
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
            fixture.producer_release.notify_one();
            tokio::select! {
                _ = &mut settle => panic!("panic skipped later producer or drain"),
                _ = fixture.probe.drain_entered.notified() => {}
            }
            assert!(futures::poll!(settle.as_mut()).is_pending());
            fixture.probe.release_drain.notify_one();
            assert!(!settle.await);
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
            fixture.exact_receipt(mutation).await;
        })
        .await;
    }

    #[tokio::test]
    async fn disabled_collectors_keep_detached_spawning_even_after_seal() {
        bounded(async {
            for tasks in [SessionWriteTasks::default(), SessionWriteTasks::new(false)] {
                let fixture = Fixture::new().await;
                let mutation = fixture.mutation().await;
                assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
                fixture.producer_entered.notified().await;
                assert!(tasks.seal().tasks.is_empty());
                let completed = Arc::new(Notify::new());
                let done = completed.clone();
                assert!(tasks.spawn(async move {
                    done.notify_one();
                }));
                completed.notified().await;
                assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
                fixture.producer_release.notify_one();
                fixture.producer_done.notified().await;
                fixture.exact_receipt(mutation).await;
                // Collector operations never invoke native cleanup when off.
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn dropping_settlement_detaches_without_certifying_cleanup() {
        bounded(async {
            let fixture = Fixture::new().await;
            let mutation = fixture.mutation().await;
            let tasks = SessionWriteTasks::new(true);
            assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
            fixture.producer_entered.notified().await;
            let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
            assert!(futures::poll!(settle.as_mut()).is_pending());
            assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
            drop(settle);
            fixture.producer_release.notify_one();
            fixture.producer_done.notified().await;
            fixture.exact_receipt(mutation).await;
            // Only the detached producer's completion is certified here. No
            // automatically spawned cleanup, flush, or root-drop guarantee.
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn malformed_read_faulted_and_closed_live_cleanup_still_reaches_backend() {
        bounded(async {
            for closed in [false, true] {
                let fixture = Fixture::new().await;
                fixture
                    .harness
                    .session()
                    .append_custom_entry("rpi.durable.control.v1", Some(json!("malformed")))
                    .await
                    .unwrap();
                let before = serde_json::to_value(
                    fixture
                        .probe
                        .inner
                        .find_entries(&EntryQuery::default())
                        .await
                        .unwrap(),
                )
                .unwrap();
                let revision = fixture.probe.inner.history_revision().await.unwrap();
                fixture.probe.fail_reads.store(true, Ordering::SeqCst);
                fixture.probe.fail_revision.store(true, Ordering::SeqCst);
                if closed {
                    fixture.harness.close().await.unwrap();
                }
                fixture.probe.pause_drains.store(true, Ordering::SeqCst);
                let tasks = SessionWriteTasks::new(true);
                let mut settle = Box::pin(settle_native(&fixture.harness, tasks.seal()));
                tokio::select! {
                    _ = &mut settle => panic!("cleanup skipped paused backend"),
                    _ = fixture.probe.drain_entered.notified() => {}
                }
                assert!(futures::poll!(settle.as_mut()).is_pending());
                fixture.probe.release_drain.notify_one();
                // A closed live owner makes idle fail, not the core barrier;
                // the helper must report failure without skipping backend drain.
                assert_eq!(settle.await, !closed);
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
                assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
                assert_eq!(
                    fixture.probe.inner.history_revision().await.unwrap(),
                    revision
                );
                assert_eq!(
                    serde_json::to_value(
                        fixture
                            .probe
                            .inner
                            .find_entries(&EntryQuery::default())
                            .await
                            .unwrap()
                    )
                    .unwrap(),
                    before
                );
            }
        })
        .await;
    }

    async fn wait_for_finished(tasks: &SessionWriteTasks) {
        while !tasks
            .state
            .lock()
            .unwrap()
            .tasks
            .iter()
            .all(JoinHandle::is_finished)
        {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn completed_history_is_reaped_without_growing_per_submission() {
        bounded(async {
            let fixture = Fixture::new().await;
            let tasks = SessionWriteTasks::new(true);
            let completed = Arc::new(AtomicUsize::new(0));
            for _ in 0..256 {
                let completed = completed.clone();
                assert!(tasks.spawn(async move {
                    completed.fetch_add(1, Ordering::SeqCst);
                }));
                wait_for_finished(&tasks).await;
                assert_eq!(tasks.state.lock().unwrap().tasks.len(), 1);
            }
            assert_eq!(completed.load(Ordering::SeqCst), 256);
            let batch = tasks.seal();
            assert!(batch.tasks.is_empty());
            assert!(!batch.had_join_failure);
            assert!(settle_native(&fixture.harness, batch).await);
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.probe.attempts.load(Ordering::SeqCst), 0);
        })
        .await;
    }

    #[tokio::test]
    async fn reaped_panic_survives_successes_and_still_joins_live_writer() {
        bounded(async {
            let fixture = Fixture::new().await;
            let mutation = fixture.mutation().await;
            let tasks = SessionWriteTasks::new(true);
            assert!(tasks.spawn(async {
                panic!("synthetic reaped panic");
            }));
            wait_for_finished(&tasks).await;
            for _ in 0..4 {
                assert!(tasks.spawn(async {}));
                wait_for_finished(&tasks).await;
                let state = tasks.state.lock().unwrap();
                assert_eq!(state.tasks.len(), 1);
                assert!(state.had_join_failure);
            }
            assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
            fixture.producer_entered.notified().await;
            let batch = tasks.seal();
            assert!(batch.had_join_failure);
            assert_eq!(batch.tasks.len(), 1);
            fixture.probe.pause_drains.store(true, Ordering::SeqCst);
            let mut settle = Box::pin(settle_native(&fixture.harness, batch));
            assert!(futures::poll!(settle.as_mut()).is_pending());
            assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 0);
            fixture.producer_release.notify_one();
            tokio::select! {
                _ = &mut settle => panic!("reaped panic skipped writer or backend cleanup"),
                _ = fixture.probe.drain_entered.notified() => {}
            }
            fixture.probe.release_drain.notify_one();
            assert!(!settle.await);
            fixture.exact_receipt(mutation).await;
        })
        .await;
    }

    #[tokio::test]
    async fn depleted_budget_reaps_ready_results_but_preserves_unfinished_handles() {
        bounded(async {
            let fixture = Fixture::new().await;
            let mutation = fixture.mutation().await;
            let tasks = SessionWriteTasks::new(true);
            assert!(tasks.spawn(async {}));
            assert!(tasks.spawn(async {
                panic!("synthetic budget panic");
            }));
            wait_for_finished(&tasks).await;
            while tokio::task::coop::has_budget_remaining() {
                let mut consume = Box::pin(tokio::task::consume_budget());
                assert!(futures::poll!(consume.as_mut()).is_ready());
            }
            assert!(!tokio::task::coop::has_budget_remaining());
            // No scheduler yield: the new producer has not even been polled.
            assert!(tasks.spawn(fixture.producer(mutation.clone(), true)));
            assert!(!tokio::task::coop::has_budget_remaining());
            {
                let state = tasks.state.lock().unwrap();
                assert_eq!(state.tasks.len(), 1);
                assert!(!state.tasks[0].is_finished());
                assert!(state.had_join_failure);
            }
            let batch = tasks.seal();
            assert_eq!(batch.tasks.len(), 1);
            assert!(batch.had_join_failure);
            assert!(!tokio::task::coop::has_budget_remaining());
            fixture.producer_release.notify_one();
            assert!(!settle_native(&fixture.harness, batch).await);
            fixture.exact_receipt(mutation).await;
        })
        .await;
    }

    #[tokio::test]
    async fn reaped_panic_payload_is_destroyed_after_unlock_on_spawn_and_seal() {
        struct Payload {
            tasks: SessionWriteTasks,
            observed: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                let unlocked = self.tasks.state.try_lock().is_ok();
                self.observed
                    .store(if unlocked { 1 } else { 2 }, Ordering::SeqCst);
            }
        }
        bounded(async {
            for on_seal in [false, true] {
                let fixture = Fixture::new().await;
                let tasks = SessionWriteTasks::new(true);
                let observed = Arc::new(AtomicUsize::new(0));
                let payload = Payload {
                    tasks: tasks.clone(),
                    observed: observed.clone(),
                };
                assert!(tasks.spawn(async move {
                    std::panic::panic_any(payload);
                }));
                wait_for_finished(&tasks).await;
                assert_eq!(observed.load(Ordering::SeqCst), 0);
                if !on_seal {
                    assert!(tasks.spawn(async {}));
                    assert_eq!(observed.load(Ordering::SeqCst), 1);
                }
                let batch = tasks.seal();
                assert_eq!(observed.load(Ordering::SeqCst), 1);
                assert!(batch.had_join_failure);
                assert!(!settle_native(&fixture.harness, batch).await);
                assert_eq!(fixture.probe.drain_calls.load(Ordering::SeqCst), 1);
            }
        })
        .await;
    }
}

#[cfg(test)]
#[path = "settlement/experimental_durable_tests.rs"]
mod experimental_durable_tests;
