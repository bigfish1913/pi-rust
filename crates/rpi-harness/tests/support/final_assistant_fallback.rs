//! Final entry ACK refusal must not invalidate an already-bound native ID.
use super::*;
use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::{DefaultIdGenerator, SystemClock};
use rpi_tools::{FileSystem, OsExecutionEnv};

#[derive(Default)]
struct LiveEvents(Mutex<Vec<AgentEvent>>);

impl AgentEmitter for LiveEvents {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.0.lock().unwrap().push(event);
        Box::pin(async {})
    }

    fn try_emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct Fixture {
    harness: AgentHarness,
    storage: Arc<ProbedStorage>,
    live: Arc<LiveEvents>,
    ended: Arc<AtomicUsize>,
    fs: Arc<dyn FileSystem>,
    path: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(volatile: bool, refusals: usize, refuse_binding: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let path = dir
            .path()
            .join("final.jsonl")
            .to_string_lossy()
            .into_owned();
        let inner = JsonlSessionStorage::create(
            fs.clone(),
            &path,
            JsonlV4Header::new("final-fallback".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let storage = Arc::new(ProbedStorage::new(Arc::new(inner)));
        storage.pause.store(false, Ordering::SeqCst);
        *storage.fail_assistants.lock().unwrap() = refusals;
        storage.fail_binding.store(refuse_binding, Ordering::SeqCst);
        let session = Session::new(storage.clone(), None);
        let session = if volatile {
            session.with_volatile_progress()
        } else {
            session
        };
        // Exactly one response: persistence fallback must never call a model.
        let provider = FauxProvider::new(FauxScript::new().with_text("finished once"));
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            session,
            ..Default::default()
        })
        .await
        .unwrap();
        let live = Arc::new(LiveEvents::default());
        harness.set_agent_emitter(Some(live.clone())).await.unwrap();
        let ended = Arc::new(AtomicUsize::new(0));
        let counter = ended.clone();
        harness
            .events()
            .on_lifetime::<rpi_harness::events::RunEndEvent, _>(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        Self {
            harness,
            storage,
            live,
            ended,
            fs,
            path,
            _dir: dir,
        }
    }

    fn final_live_id(&self) -> String {
        let events = self.live.0.lock().unwrap();
        let ids = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(_),
                    message_id,
                }
                | AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                    message_id,
                } => Some(
                    message_id
                        .clone()
                        .expect("every live assistant has a native ID"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2, "one final assistant Start and End");
        assert_eq!(ids[0], ids[1]);
        ids[0].clone()
    }

    async fn assert_unfinished(&self) -> String {
        assert!(records(&self.storage, "operation_finished")
            .await
            .is_empty());
        assert_eq!(self.ended.load(Ordering::SeqCst), 0);
        assert!(assistant_entries(&self.storage).await.is_empty());
        let open = self
            .storage
            .find_open_operations("main", None)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        let id = open[0].base.id.clone();
        assert!(self.harness.get_result(&id).await.unwrap().is_none());
        assert_valid(self.storage.as_ref()).await;
        id
    }
}

async fn assert_valid(storage: &dyn SessionStorage) {
    rpi_harness::session::validate_record_log(&rpi_harness::session::RecordLogSlice {
        lane: "main".into(),
        open_operations: storage.find_open_operations("main", None).await.unwrap(),
        records: storage.find_records(&RecordQuery::default()).await.unwrap(),
        entries: storage.find_entries(&EntryQuery::default()).await.unwrap(),
    })
    .unwrap();
}

async fn assert_terminal_identity(storage: &dyn SessionStorage, run_id: &str, id: &str) {
    let records = storage
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let LaneRecord::OperationFinished(record) = &records[0] else {
        unreachable!()
    };
    assert_eq!(record.run_id, run_id);
    assert_eq!(record.outcome, OperationOutcome::Completed);
    let identity = record.result.as_ref().unwrap();
    assert_eq!(identity.leaf_id, id);
    assert_eq!(identity.final_entry_id.as_deref(), Some(id));
    assert_eq!(identity.assistant_entry_id.as_deref(), Some(id));
    assert!(storage
        .find_open_operations("main", None)
        .await
        .unwrap()
        .is_empty());
    assert_valid(storage).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_final_entry_refusal_retries_the_same_native_id() {
    for volatile in [false, true] {
        let fixture = Fixture::new(volatile, 1, false).await;
        let result = fixture.harness.prompt_text("answer", vec![]).await.unwrap();
        let id = fixture.final_live_id();
        let HarnessRunOutcome::Completed {
            final_entry_id,
            final_message,
            ..
        } = &result.outcome
        else {
            panic!("one refused ACK must reach the same-ID fallback: {result:?}")
        };
        assert_eq!(final_entry_id, &id);
        assert_eq!(final_message.stop_reason, StopReason::Stop);
        assert_eq!(
            *fixture.storage.assistant_attempts.lock().unwrap(),
            vec![id.clone(), id.clone()],
            "settle and fallback must try the same reservation"
        );
        assert_eq!(*fixture.storage.fail_assistants.lock().unwrap(), 0);
        let entries = assistant_entries(&fixture.storage).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].base.id, id);
        assert_eq!(
            entries[0].message,
            AgentMessage::Assistant(Box::new(final_message.clone()))
        );
        assert_eq!(fixture.ended.load(Ordering::SeqCst), 1);
        let queried = fixture
            .harness
            .get_result(&result.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queried.run_id, result.run_id);
        let HarnessRunOutcome::Completed {
            leaf_id: query_leaf,
            final_entry_id: query_id,
            final_message: query_message,
        } = queried.outcome
        else {
            panic!("the acknowledged result must remain completed")
        };
        assert_eq!(query_leaf, id);
        assert_eq!(query_id, id);
        assert_eq!(query_message, *final_message);
        assert_terminal_identity(fixture.storage.as_ref(), &result.run_id, &id).await;
        assert!(fixture
            .storage
            .read_progress(&result.run_id)
            .await
            .unwrap()
            .is_empty());

        let restored = JsonlSessionStorage::load(
            fixture.fs.clone(),
            &fixture.path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let Some(Entry::Message(entry)) = restored.get_entry(&id).await.unwrap() else {
            panic!("the final native identity must survive JSONL reload")
        };
        assert_eq!(entry.message, entries[0].message);
        assert_terminal_identity(&restored, &result.run_id, &id).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_final_entry_refusal_keeps_the_operation_open() {
    for volatile in [false, true] {
        let fixture = Fixture::new(volatile, 2, false).await;
        let error = fixture
            .harness
            .prompt_text("answer", vec![])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("controlled recovery entry failure"));
        let id = fixture.final_live_id();
        assert_eq!(
            *fixture.storage.assistant_attempts.lock().unwrap(),
            vec![id.clone(), id],
            "both refused writes must keep the bound identity"
        );
        assert_eq!(*fixture.storage.fail_assistants.lock().unwrap(), 0);
        let run_id = fixture.assert_unfinished().await;
        if !volatile {
            assert!(
                !fixture
                    .storage
                    .read_progress(&run_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "persistent refusal must not retire recoverable content"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_final_binding_still_fails_closed_before_live_identity() {
    // Volatile bindings use an infallible in-memory path, not a storage ACK.
    let fixture = Fixture::new(false, 0, true).await;
    let error = fixture
        .harness
        .prompt_text("answer", vec![])
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("controlled identity binding failure"));
    assert!(!fixture.storage.fail_binding.load(Ordering::SeqCst));
    assert!(fixture
        .storage
        .assistant_attempts
        .lock()
        .unwrap()
        .is_empty());
    assert!(
        !fixture.live.0.lock().unwrap().iter().any(|event| matches!(
            event,
            AgentEvent::MessageStart { .. }
                | AgentEvent::MessageUpdate { .. }
                | AgentEvent::MessageEnd { .. }
        )),
        "an unbound identity must not be published"
    );
    fixture.assert_unfinished().await;
}
