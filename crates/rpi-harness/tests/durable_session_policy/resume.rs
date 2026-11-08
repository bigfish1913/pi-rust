//! Resume admission only: real recovery/poll paths, synthetic stores and Faux.
//! Startup repair is fixture setup, never part of the rejected-resume baseline.
use super::*;
use rpi_ai::{
    provider::DeferredProvider,
    providers::faux::{faux_assistant_message, DeferredPoll, FauxProvider, FauxScript},
    AssistantMessageEventStream, Context, DeferredHandle, Model, Provider, SimpleStreamOptions,
    StopReason,
};
use rpi_harness::{
    agent_harness::{HarnessRunOutcome, RunResult, SendMessageResult},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    },
    types::{CompactionSettings, MessageDelivery, SendMessageOptions},
};
use rpi_tools::{FileSystem, OsExecutionEnv};

const HANDLE: &str = "synthetic-resume-handle";

// Faux's ordinary call_count deliberately does not count deferred polls.
struct CountingFaux {
    inner: Arc<FauxProvider>,
    polls: AtomicUsize,
}
impl CountingFaux {
    fn new() -> Arc<Self> {
        let inner = FauxProvider::new(
            FauxScript::new()
                .with_text("audited native answer")
                .with_text("continued native answer"),
        );
        inner.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
            "polled native answer",
            StopReason::Stop,
        )));
        Arc::new(Self {
            inner,
            polls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.inner.state().call_count.load(Ordering::SeqCst)
    }
}
#[async_trait::async_trait]
impl Provider for CountingFaux {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn models(&self) -> &[Model] {
        self.inner.models()
    }
    fn deferred(&self) -> Option<&dyn DeferredProvider> {
        Some(self)
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_simple(model, context, options).await
    }
}
#[async_trait::async_trait]
impl DeferredProvider for CountingFaux {
    async fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.inner.stream_deferred(model, handle, options).await
    }
}

fn configured(
    session: Session,
    provider: Arc<CountingFaux>,
    tool: Arc<NoCalls>,
    required: bool,
) -> AgentHarnessOptions {
    AgentHarnessOptions {
        model: provider.inner.default_model().clone(),
        models: vec![provider],
        tools: vec![HarnessTool::new(tool)],
        active_tool_names: vec!["policy-effect".into()],
        require_native_durable: required,
        compaction: CompactionSettings {
            enabled: false,
            ..Default::default()
        },
        ..options(session)
    }
}
async fn seed(h: &AgentHarness) {
    apply(
        &service(h),
        "workflow",
        json!({"action":"start","name":"resume-policy","steps":[{"id":"one","title":"Keep prior work"}]}),
    )
    .await;
    apply(
        &service(h),
        "manual",
        json!({"action":"enqueue","text":"Keep exact manual receipt"}),
    )
    .await;
}
async fn park(session: &Session, provider: &CountingFaux, open: bool) {
    if open {
        open_operation(session).await;
    }
    let mut message = faux_assistant_message("", StopReason::Deferred);
    let model = provider.inner.default_model();
    message.deferred = Some(DeferredHandle {
        provider: provider.id().into(),
        model_id: model.id.clone(),
        api: model.api.as_str().into(),
        id: HANDLE.into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    });
    session
        .append_message(AgentMessage::Assistant(Box::new(message)))
        .await
        .unwrap();
}
async fn recover_pending(session: &Session) {
    open_operation(session).await;
    session
        .append_message(AgentMessage::User(UserMessage::new(
            "Continue this actual interrupted request",
            0,
        )))
        .await
        .unwrap();
}
struct Fixture {
    h: AgentHarness,
    provider: Arc<CountingFaux>,
    tool: Arc<NoCalls>,
}
async fn fixture(session: Session, pending: bool, open: bool, required: bool) -> Fixture {
    let provider = CountingFaux::new();
    let tool = NoCalls::new();
    let mut h = AgentHarness::create(configured(
        session.clone(),
        provider.clone(),
        tool.clone(),
        required,
    ))
    .await
    .unwrap();
    seed(&h).await;
    if pending {
        // Recovery must see a genuine open native operation and user tip.
        drop(h);
        recover_pending(&session).await;
        h = AgentHarness::create(configured(
            session,
            provider.clone(),
            tool.clone(),
            required,
        ))
        .await
        .unwrap();
        let marker = h.pending_resume().expect("real startup recovery marker");
        assert_eq!(marker.interrupted_run_id, "open-run");
        assert_eq!(marker.chain_origin_run_id, "open-run");
        assert_eq!(marker.attempt, 0);
    } else {
        park(&session, &provider, open).await;
    }
    Fixture { h, provider, tool }
}
async fn resume(h: &AgentHarness, pending: bool) -> Result<RunResult, String> {
    if pending {
        h.resume_pending()
            .await
            .map_err(|e| e.to_string())
            .map(|r| r.expect("the pending marker must actually continue"))
    } else {
        h.resume(HANDLE).await.map_err(|e| e.to_string())
    }
}
async fn controls(session: &Session) -> Value {
    json!(session
        .storage()
        .find_entries(&EntryQuery {
            custom_type: Some(CONTROL.into()),
            ..Default::default()
        })
        .await
        .unwrap())
}
async fn notices(h: &AgentHarness) -> usize {
    h.session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .iter()
        .filter(|entry| match entry {
            Entry::Message(m) => rpi_harness::messages::custom_data(&m.message)
                .is_some_and(|data| data.custom_type == "runResume"),
            _ => false,
        })
        .count()
}
fn untouched_provider(f: &Fixture) {
    assert_eq!(f.provider.calls(), 0);
    assert_eq!(f.provider.polls.load(Ordering::SeqCst), 0);
    assert_eq!(f.tool.calls.load(Ordering::SeqCst), 0);
}

// Never detach a blocked admission task, including on assertion/timeout unwind.
struct AbortTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_poststartup_damage_preserves_recovered_marker_and_every_fact() {
    bounded(async {
        for kind in ["durable-state", "rpi-durable-runtime", CONTROL] {
            let f = fixture(memory(), true, true, true).await;
            f.h.follow_up(AgentMessage::User(UserMessage::new("keep queued", 0)))
                .await
                .unwrap();
            let child =
                f.h.create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
                    .await
                    .unwrap();
            let marker = f.h.pending_resume();
            let receipt = service(&f.h).request_outcome("manual").await.unwrap();
            assert!(receipt.is_some());
            custom(
                f.h.session(),
                "main",
                "bad-resume",
                kind,
                Some(json!({"private":"NEVER-REFLECT"})),
            )
            .await;
            let facts = controls(f.h.session()).await;
            let before = image(f.h.session()).await;
            let queues = f.h.queued_messages().await.unwrap();
            let error = resume(&f.h, true).await.unwrap_err();
            assert!(
                error.contains("Unsupported durable journal format")
                    || error.contains("Invalid native durable journal")
            );
            assert!(!error.contains("NEVER-REFLECT"));
            assert_eq!(image(f.h.session()).await, before);
            assert_eq!(controls(f.h.session()).await, facts);
            assert_eq!(f.h.pending_resume(), marker);
            assert_eq!(f.h.queued_messages().await.unwrap(), queues);
            assert_eq!(notices(&f.h).await, 0);
            assert!(!child.scope.is_cancelled());
            assert!(f.h.snapshot().await.unwrap().active.is_none());
            untouched_provider(&f);
            // Both the message gate and the active-run lease are released.
            f.h.set_session(memory()).await.unwrap();
            assert!(child.scope.is_cancelled());
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_poststartup_damage_preserves_parked_and_imported_handles_without_intent() {
    bounded(async {
        for open in [false, true] {
            for kind in ["durable-state", "rpi-durable-runtime", CONTROL] {
                let f = fixture(memory(), false, open, true).await;
                f.h.follow_up(AgentMessage::User(UserMessage::new("keep queue", 0)))
                    .await
                    .unwrap();
                let child =
                    f.h.create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
                        .await
                        .unwrap();
                let receipt = service(&f.h).request_outcome("manual").await.unwrap();
                assert!(receipt.is_some());
                custom(
                    f.h.session(),
                    "main",
                    "bad-deferred",
                    kind,
                    Some(json!({"private":"NEVER-REFLECT"})),
                )
                .await;
                let before = image(f.h.session()).await;
                let queues = f.h.queued_messages().await.unwrap();
                // Missing handles still win over native policy rejection.
                assert!(f
                    .h
                    .resume("missing-synthetic-handle")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("no suspended operation"));
                for _ in 0..2 {
                    let error = resume(&f.h, false).await.unwrap_err();
                    assert!(
                        error.contains("Unsupported durable journal format")
                            || error.contains("Invalid native durable journal")
                    );
                    assert!(!error.contains("NEVER-REFLECT"));
                    assert_eq!(image(f.h.session()).await, before);
                    assert_eq!(f.h.queued_messages().await.unwrap(), queues);
                    assert!(f.h.snapshot().await.unwrap().active.is_none());
                    assert!(!child.scope.is_cancelled());
                    assert_eq!(notices(&f.h).await, 0);
                    untouched_provider(&f);
                }
                f.h.set_session(memory()).await.unwrap();
                assert!(child.scope.is_cancelled());
            }
        }
    })
    .await;
}

async fn barrier_cases(pending: bool) {
    // Exercise imported as well as parked deferred operations at the exact same
    // CONTROL-read boundary; fail_reads would stop at find_handle too early.
    for open in if pending {
        vec![true]
    } else {
        vec![false, true]
    } {
        let base = memory();
        let probe = probe::Probe::new(base.storage());
        let f = fixture(Session::new(probe.clone(), None), pending, open, true).await;
        // A resume is not an explicit next turn. Keep this queue across both
        // failed admissions and the eventual successful retry; no cancellation
        // batch support is needed from the existing read/control Probe.
        let queued =
            f.h.send_message(
                AgentMessage::User(UserMessage::new("keep next turn", 0)),
                SendMessageOptions {
                    trigger_turn: Some(false),
                    deliver_as: MessageDelivery::NextTurn,
                },
            )
            .await
            .unwrap();
        let SendMessageResult::Queued {
            entry_id: queued_id,
            queue: QueueKind::NextRun,
        } = queued
        else {
            panic!("fixture must queue an explicit next turn");
        };
        let child =
            f.h.create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
                .await
                .unwrap();
        let marker = f.h.pending_resume();
        let queues = f.h.queued_messages().await.unwrap();
        assert!(f.h.get_entry(&queued_id).await.unwrap().is_none());
        let receipt = service(&f.h).request_outcome("manual").await.unwrap();
        let state = service(&f.h).snapshot().await.unwrap().state;
        let facts = controls(&base).await;
        let attempts = probe.attempts.load(Ordering::SeqCst);
        let commits = probe.commits.load(Ordering::SeqCst);
        for mode in ["read", "race", "cancel"] {
            let mut expected = image(&base).await;
            probe.pause_branch_reads.store(true, Ordering::SeqCst);
            let owner = f.h.clone();
            let mut worker = AbortTask(tokio::spawn(async move { resume(&owner, pending).await }));
            bounded(probe.branch_read_entered.notified()).await;
            assert!(f.h.snapshot().await.unwrap().active.is_some());
            assert_eq!(f.h.pending_resume(), marker);
            untouched_provider(&f);
            if mode == "cancel" {
                worker.0.abort();
                assert!(bounded(&mut worker.0).await.unwrap_err().is_cancelled());
            } else {
                if mode == "read" {
                    probe.fail_revision.store(true, Ordering::SeqCst);
                } else {
                    base.append_custom_entry("external-resume-race", None)
                        .await
                        .unwrap();
                    expected = image(&base).await;
                }
                probe.release_branch_read.notify_one();
                let error = bounded(&mut worker.0).await.unwrap().unwrap_err();
                assert!(error.contains(if mode == "read" {
                    "Native durable session validation failed"
                } else {
                    "changed during validation"
                }));
                assert!(!error.contains("injected"));
                probe.fail_revision.store(false, Ordering::SeqCst);
            }
            assert_eq!(image(&base).await, expected);
            assert_eq!(f.h.pending_resume(), marker);
            assert_eq!(f.h.queued_messages().await.unwrap(), queues);
            assert_eq!(notices(&f.h).await, 0);
            assert!(!child.scope.is_cancelled());
            assert!(f.h.snapshot().await.unwrap().active.is_none());
            assert_eq!(
                bounded(service(&f.h).request_outcome("manual"))
                    .await
                    .unwrap(),
                receipt
            );
            assert_eq!(
                bounded(service(&f.h).snapshot()).await.unwrap().state,
                state
            );
            assert_eq!(controls(&base).await, facts);
            assert_eq!(probe.attempts.load(Ordering::SeqCst), attempts);
            assert_eq!(probe.commits.load(Ordering::SeqCst), commits);
            untouched_provider(&f);
        }
        // Retry the very same marker/handle after all three rejections. This
        // proves both admission lease release and no deferred-handle consumption.
        let result = resume(&f.h, pending).await.unwrap();
        assert!(matches!(
            result.outcome,
            HarnessRunOutcome::Completed { .. }
        ));
        assert_eq!(f.provider.calls(), usize::from(pending));
        assert_eq!(
            f.provider.polls.load(Ordering::SeqCst),
            usize::from(!pending)
        );
        assert_eq!(f.tool.calls.load(Ordering::SeqCst), 0);
        if !pending && open {
            assert_eq!(result.run_id, "open-run");
        }
        assert!(f.h.get_entry(&queued_id).await.unwrap().is_none());
        assert!(base
            .find_records(&RecordQuery {
                record_type: Some("queue_consumed"),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert!(f.h.snapshot().await.unwrap().active.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_control_read_failure_revision_race_and_abort_release_gate_and_lease() {
    bounded(barrier_cases(true)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_control_read_failure_revision_race_and_abort_release_gate_and_lease() {
    bounded(barrier_cases(false)).await;
}

async fn positive(disk: bool) {
    for pending in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
        let session = if disk {
            Session::new(
                Arc::new(
                    JsonlSessionStorage::create(
                        fs.clone(),
                        "resume.jsonl",
                        JsonlV4Header::new(
                            "synthetic-resume".into(),
                            1,
                            "/synthetic".into(),
                            None,
                            None,
                            None,
                        ),
                        Arc::new(SystemClock),
                        Arc::new(DefaultIdGenerator::new()),
                    )
                    .await
                    .unwrap(),
                ),
                None,
            )
        } else {
            memory()
        };
        let provider = CountingFaux::new();
        let tool = NoCalls::new();
        let mut h = AgentHarness::create(configured(
            session.clone(),
            provider.clone(),
            tool.clone(),
            true,
        ))
        .await
        .unwrap();
        seed(&h).await;
        let prior = h.prompt_text("real native audit", vec![]).await.unwrap();
        assert!(matches!(prior.outcome, HarnessRunOutcome::Completed { .. }));
        let prior_result = format!("{:?}", h.get_result(&prior.run_id).await.unwrap().unwrap());
        let prior_execution = service(&h).execution(&prior.run_id).await.unwrap().run;
        assert!(prior_execution.is_some());
        if pending {
            drop(h);
            recover_pending(&session).await;
            h = AgentHarness::create(configured(
                session.clone(),
                provider.clone(),
                tool.clone(),
                true,
            ))
            .await
            .unwrap();
        } else {
            park(&session, &provider, true).await;
        }
        let marker = h.pending_resume();
        assert_eq!(marker.is_some(), pending);
        let s = service(&h); // retain this exact binding throughout the resume
        let before = s.snapshot().await.unwrap();
        let workflow_receipt = s.request_outcome("workflow").await.unwrap();
        let receipt = s.request_outcome("manual").await.unwrap();
        assert!(workflow_receipt.is_some() && receipt.is_some());
        let facts = controls(h.session()).await;
        assert_eq!(notices(&h).await, 0);
        let result = resume(&h, pending).await.unwrap();
        assert!(matches!(
            result.outcome,
            HarnessRunOutcome::Completed { .. }
        ));
        assert_eq!(provider.calls(), 1 + usize::from(pending));
        assert_eq!(provider.polls.load(Ordering::SeqCst), usize::from(!pending));
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        assert!(h.pending_resume().is_none());
        assert_eq!(notices(&h).await, usize::from(pending));
        let records = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        if pending {
            let marker = marker.unwrap();
            assert_ne!(result.run_id, marker.interrupted_run_id);
            let chain = records
                .iter()
                .find_map(|r| match r {
                    LaneRecord::OperationStarted(start) if start.base.id == result.run_id => {
                        match &start.intent {
                            OperationIntent::Run { resume_data, .. } => resume_data.as_ref(),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .expect("real resumed native intent");
            assert_eq!(
                chain.get("resumedFrom"),
                Some(&json!(marker.chain_origin_run_id))
            );
            assert_eq!(chain.get("resumeAttempt"), Some(&json!(marker.attempt + 1)));
            assert!(h.resume_pending().await.unwrap().is_none());
        } else {
            assert_eq!(result.run_id, "open-run");
            assert!(
                h.resume(HANDLE).await.is_err(),
                "completed handle cannot be polled twice"
            );
        }
        assert_eq!(records.iter().filter(|r| matches!(r, LaneRecord::OperationFinished(f) if f.run_id == result.run_id && f.outcome == OperationOutcome::Completed)).count(), 1);
        let after = s.snapshot().await.unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.execution_control, before.execution_control);
        assert_eq!(after.scope.entry_id, before.scope.entry_id);
        assert_eq!(controls(h.session()).await, facts);
        assert_eq!(
            s.request_outcome("workflow").await.unwrap(),
            workflow_receipt
        );
        assert_eq!(s.request_outcome("manual").await.unwrap(), receipt);
        assert_eq!(
            s.execution(&prior.run_id).await.unwrap().run,
            prior_execution
        );
        assert_eq!(
            format!("{:?}", h.get_result(&prior.run_id).await.unwrap().unwrap()),
            prior_result
        );
        let resumed_result = format!("{:?}", h.get_result(&result.run_id).await.unwrap().unwrap());
        assert_eq!(resumed_result, format!("{:?}", result));
        let resumed_execution = s.execution(&result.run_id).await.unwrap().run;
        assert!(resumed_execution.is_some());
        h.drain_session().await;
        h.close().await.unwrap();
        drop(s);
        drop(h);
        let bytes = if disk {
            Some(std::fs::read(tmp.path().join("resume.jsonl")).unwrap())
        } else {
            None
        };
        let reopened_session = if disk {
            // The external `session` binding owns the JSONL lease independently
            // of the harness. Release it before constructing a fresh binding.
            drop(session);
            Session::new(
                Arc::new(
                    JsonlSessionStorage::load(
                        fs,
                        "resume.jsonl",
                        Arc::new(SystemClock),
                        Arc::new(DefaultIdGenerator::new()),
                    )
                    .await
                    .unwrap(),
                ),
                None,
            )
        } else {
            session
        };
        // No models/providers registered on reopen: native terminal records,
        // not provider replay or cached in-memory results, are authoritative.
        let reopened = strict(reopened_session).await;
        let rs = service(&reopened);
        let snapshot = rs.snapshot().await.unwrap();
        assert_eq!(snapshot.state, after.state);
        assert_eq!(snapshot.execution_control, after.execution_control);
        assert_eq!(snapshot.scope.entry_id, after.scope.entry_id);
        assert_eq!(
            snapshot.scope.history_revision,
            after.scope.history_revision
        );
        assert_eq!(snapshot.scope.leaf_id, after.scope.leaf_id);
        assert_eq!(controls(reopened.session()).await, facts);
        assert_eq!(
            rs.request_outcome("workflow").await.unwrap(),
            workflow_receipt
        );
        assert_eq!(rs.request_outcome("manual").await.unwrap(), receipt);
        assert_eq!(
            rs.execution(&prior.run_id).await.unwrap().run,
            prior_execution
        );
        assert_eq!(
            rs.execution(&result.run_id).await.unwrap().run,
            resumed_execution
        );
        assert_eq!(
            format!(
                "{:?}",
                reopened.get_result(&prior.run_id).await.unwrap().unwrap()
            ),
            prior_result
        );
        assert_eq!(
            format!(
                "{:?}",
                reopened.get_result(&result.run_id).await.unwrap().unwrap()
            ),
            resumed_result
        );
        assert!(reopened.pending_resume().is_none());
        assert_eq!(provider.calls(), 1 + usize::from(pending));
        assert_eq!(provider.polls.load(Ordering::SeqCst), usize::from(!pending));
        reopened.drain_session().await;
        reopened.close().await.unwrap();
        if let Some(bytes) = bytes {
            assert_eq!(
                std::fs::read(tmp.path().join("resume.jsonl")).unwrap(),
                bytes
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_pending_and_deferred_resume_preserve_native_receipts_audit_and_reopen() {
    bounded(positive(false)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jsonl_pending_and_deferred_resume_preserve_native_receipts_audit_and_reopen() {
    bounded(positive(true)).await;
}
