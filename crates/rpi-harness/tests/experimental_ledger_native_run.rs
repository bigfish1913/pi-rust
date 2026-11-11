//! Synthetic fixtures only; no writer/host admission or complete journal proof.
#[path = "experimental_ledger_native_run/probe.rs"]
mod probe;
use probe::Probe;
use rpi_agent::AgentMessage;
use rpi_ai::{
    providers::faux::{faux_assistant_message, FauxProvider, FauxScript},
    types::{Content, StopReason, ToolResultMessage, ToolResultRole, UserMessage},
};
use rpi_durable::execution::{ReplayPolicy, RunStatus, ToolStatus};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    durable::{
        experimental_ledger::{
            native_run::{
                inspect_native_run, NativeRunError, NativeRunInspection, MAX_NATIVE_RECORDS,
                MAX_NATIVE_RECORD_BYTES,
            },
            source::SourceError,
            ReadPolicy,
        },
        DurableActor, DurableError, DurableService,
    },
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::json;
use std::{
    future::Future,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
async fn bounded<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), f)
        .await
        .expect("bounded fixture/inspection")
}
async fn fixture(storage: Arc<dyn SessionStorage>) -> (Arc<AgentHarness>, Arc<Probe>) {
    let p = Probe::new(storage);
    let h = AgentHarness::create(AgentHarnessOptions {
        session: Session::new(p.clone(), None),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    (Arc::new(h), p)
}
async fn memory() -> (Arc<AgentHarness>, Arc<Probe>) {
    fixture(AgentHarnessOptions::default().session.storage()).await
}
fn policy(h: &AgentHarness, lane: &str) -> ReadPolicy {
    ReadPolicy::new_epoch(
        h.session().storage().metadata().id,
        lane,
        "synthetic-native-run",
    )
    .unwrap()
}
async fn inspect(
    h: &AgentHarness,
    leaf: Option<&str>,
    r: u64,
    run: &str,
) -> Result<NativeRunInspection, NativeRunError> {
    bounded(inspect_native_run(
        h,
        "main",
        leaf,
        r,
        run,
        &policy(h, "main"),
    ))
    .await
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn started(id: &str) -> LaneRecord {
    LaneRecord::OperationStarted(OperationStartedRecord {
        base: base(id),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
    })
}
async fn append(h: &AgentHarness, id: &str, message: AgentMessage) -> Entry {
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Message {
                    message,
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap()
}
async fn start(h: &AgentHarness, id: &str) -> Entry {
    h.session().append_record(started(id)).await.unwrap();
    append(
        h,
        &format!("{id}-prompt"),
        AgentMessage::User(UserMessage::new("private prompt", 0)),
    )
    .await
}
async fn terminal(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finished")),
            run_id: id.into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
}
async fn audit(p: &Probe) -> (Vec<Entry>, Vec<LaneRecord>, Vec<LanePointer>, u64) {
    (
        p.inner.find_entries(&EntryQuery::default()).await.unwrap(),
        p.inner.find_records(&RecordQuery::default()).await.unwrap(),
        p.inner.get_lanes().await.unwrap(),
        p.inner.history_revision().await.unwrap(),
    )
}
fn is_source(e: NativeRunError, want: SourceError) {
    assert!(matches!(e,NativeRunError::Source(actual) if actual==want));
}

#[tokio::test]
async fn historical_open_excludes_later_terminal_and_current_observation_stays_real() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    let r = p.inner.history_revision().await.unwrap();
    terminal(&h, "run").await;
    p.reset();
    let before = audit(&p).await;
    let old = inspect(&h, Some(prompt.id()), r, "run").await.unwrap();
    assert_eq!(old.run().status, RunStatus::Open);
    assert_eq!(old.run().finished_seq, None);
    assert_eq!(old.as_of_revision(), r);
    assert_eq!(old.scope().history_revision, before.3);
    assert_ne!(old.scope().history_revision, r);
    let now = inspect(&h, Some(prompt.id()), before.3, "run")
        .await
        .unwrap();
    assert_eq!(now.run().status, RunStatus::Completed);
    assert_eq!(now.run().finished_seq, Some(before.3));
    assert_eq!(audit(&p).await, before);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn historical_tool_start_result_and_terminal_are_independently_bounded() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    let r0 = p.inner.history_revision().await.unwrap();
    let mut msg = faux_assistant_message("private assistant", StopReason::ToolUse);
    msg.content.push(Content::tool_call(
        "call",
        "write",
        json!({"secret":"private args"}),
    ));
    let assistant = append(&h, "assistant", AgentMessage::Assistant(Box::new(msg))).await;
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("invocation"),
            run_id: "run".into(),
            assistant_entry_id: assistant.id().into(),
            tool_index: 0,
            tool_call_id: "call".into(),
            tool_name: "write".into(),
            effective_args: json!({"secret":"private effective args"}),
            result_entry_id: "result".into(),
            replay: ToolReplay::Safe,
        }))
        .await
        .unwrap();
    let r1 = p.inner.history_revision().await.unwrap();
    let result = append(
        &h,
        "result",
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "write".into(),
            content: vec![Content::text("private result")],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        })),
    )
    .await;
    let r2 = p.inner.history_revision().await.unwrap();
    terminal(&h, "run").await;
    p.reset();
    let before = audit(&p).await;
    assert!(inspect(&h, Some(prompt.id()), r0, "run")
        .await
        .unwrap()
        .run()
        .tools
        .is_empty());
    let old = inspect(&h, Some(assistant.id()), r1, "run").await.unwrap();
    assert_eq!(old.run().status, RunStatus::Open);
    assert_eq!(old.run().tools.len(), 1);
    assert_eq!(old.run().tools[0].status, ToolStatus::Unknown);
    assert_eq!(old.run().tools[0].result_entry_id, None);
    assert_eq!(old.run().tools[0].replay, ReplayPolicy::Safe);
    let complete = inspect(&h, Some(result.id()), r2, "run").await.unwrap();
    assert_eq!(complete.run().status, RunStatus::Open);
    assert_eq!(complete.run().tools[0].status, ToolStatus::Completed);
    assert_eq!(
        complete.run().tools[0].result_entry_id.as_deref(),
        Some("result")
    );
    assert!(!serde_json::to_string(complete.run())
        .unwrap()
        .contains("private"));
    assert_eq!(audit(&p).await, before);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_none_sibling_invalid_scope_and_mixed_history_fail_without_fallback() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    h.session().create_lane("empty", None).await.unwrap();
    h.session().move_lane("main", None).await.unwrap();
    let sibling = append(
        &h,
        "sibling",
        AgentMessage::User(UserMessage::new("private sibling", 0)),
    )
    .await;
    let r = p.inner.history_revision().await.unwrap();
    p.reset();
    let before = audit(&p).await;
    assert!(matches!(
        inspect(&h, None, r, "run").await,
        Err(NativeRunError::Native(DurableError::ExecutionUnavailable(
            _
        )))
    ));
    assert!(matches!(
        inspect(&h, Some(sibling.id()), r, "run").await,
        Err(NativeRunError::Native(DurableError::ExecutionUnavailable(
            _
        )))
    ));
    assert_eq!(
        inspect(&h, Some(prompt.id()), r, "run")
            .await
            .unwrap()
            .run()
            .status,
        RunStatus::Open
    );
    assert!(matches!(
        inspect(&h, Some(prompt.id()), r + 1, "run").await,
        Err(NativeRunError::Watermark)
    ));
    assert!(matches!(
        inspect(&h, Some(prompt.id()), 0, "run").await,
        Err(NativeRunError::Watermark)
    ));
    assert!(matches!(
        inspect(&h, Some(prompt.id()), r, "\n").await,
        Err(NativeRunError::Native(DurableError::InvalidRequest(_)))
    ));
    let bad = ReadPolicy::new_epoch("other-session", "main", "synthetic").unwrap();
    is_source(
        inspect_native_run(&h, "main", Some(prompt.id()), r, "run", &bad)
            .await
            .unwrap_err(),
        SourceError::Policy,
    );
    assert!(
        inspect_native_run(&h, "missing", None, r, "run", &policy(&h, "missing"))
            .await
            .is_err()
    );
    assert_eq!(audit(&p).await, before);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: "legacy".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "durable-state".into(),
                    data: Some(json!({})),
                },
            },
            "main",
        )
        .await
        .unwrap();
    let r = p.inner.history_revision().await.unwrap();
    p.reset();
    let before = audit(&p).await;
    is_source(
        inspect(&h, Some("legacy"), r, "run").await.unwrap_err(),
        SourceError::Mixed,
    );
    assert_eq!(audit(&p).await, before);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_faux_jsonl_checkpoint_matches_service_and_bytes_survive_reopen() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "native.jsonl",
            JsonlV4Header::new(
                "synthetic-asof-jsonl".into(),
                1,
                "/".into(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = Probe::new(Arc::new(storage));
        let provider = FauxProvider::new(FauxScript::new().with_text("private faux answer"));
        let h = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(p.clone(), None),
            model: provider.default_model().clone(),
            models: vec![provider],
            ..Default::default()
        })
        .await
        .unwrap();
        let returned = h.prompt_text("private faux prompt", vec![]).await.unwrap();
        let s = DurableService::bind(&h, DurableActor::owner("owner").unwrap());
        let expected = s.execution(&returned.run_id).await.unwrap().run.unwrap();
        p.inner.drain().await;
        let bytes = std::fs::read(dir.path().join("native.jsonl")).unwrap();
        let before = audit(&p).await;
        let leaf = h.session().view("main").get_leaf_id().await.unwrap();
        p.reset();
        let result = inspect(&h, leaf.as_deref(), before.3, &returned.run_id)
            .await
            .unwrap();
        assert_eq!(result.run(), &expected);
        assert_eq!(audit(&p).await, before);
        assert_eq!(p.writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(dir.path().join("native.jsonl")).unwrap(),
            bytes
        );
        drop(s);
        drop(h);
        drop(p);
        let reopened = JsonlSessionStorage::load(
            fs,
            "native.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let (h, p) = fixture(Arc::new(reopened)).await;
        p.reset();
        let before = audit(&p).await;
        assert_eq!(
            inspect(&h, leaf.as_deref(), before.3, &returned.run_id)
                .await
                .unwrap()
                .run(),
            &expected
        );
        assert_eq!(audit(&p).await, before);
        assert_eq!(p.writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(dir.path().join("native.jsonl")).unwrap(),
            bytes
        );
    })
    .await
}

#[tokio::test]
async fn ascending_multi_page_scan_and_malformed_page_shapes_fail_closed() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    for i in 0..520 {
        h.session()
            .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                base: base(&format!("other-{i}")),
                run_id: "run".into(),
            }))
            .await
            .unwrap();
    }
    let r = p.inner.history_revision().await.unwrap();
    p.reset();
    let before = audit(&p).await;
    assert_eq!(
        inspect(&h, Some(prompt.id()), r, "run")
            .await
            .unwrap()
            .run()
            .status,
        RunStatus::Open
    );
    assert_eq!(p.page_calls.load(Ordering::SeqCst), 3);
    assert_eq!(audit(&p).await, before);
    let valid = p
        .inner
        .record_page(&RecordPageQuery {
            lane: "main".into(),
            after_seq: None,
            before_seq: r.checked_add(1),
            limit: 256,
            from_end: false,
        })
        .await
        .unwrap();
    for case in 0..7 {
        let mut page = valid.clone();
        match case {
            0 => page.records.swap(0, 1),
            1 => {
                page.records[1] = page.records[0].clone();
            }
            2 => {
                if let LaneRecord::OperationStarted(ref mut x) = page.records[0] {
                    x.base.lane = "wrong".into();
                }
            }
            3 => page.records.clear(),
            4 => page.total = 0,
            5 => {
                if let LaneRecord::OperationStarted(ref mut x) = page.records[0] {
                    x.base.seq = r + 1;
                }
            }
            6 => page.total = MAX_NATIVE_RECORDS + 1,
            _ => unreachable!(),
        }
        p.pages.lock().unwrap().push_back(page);
        let err = inspect(&h, Some(prompt.id()), r, "run").await.unwrap_err();
        if case == 6 {
            is_source(err, SourceError::Limit);
        } else {
            assert!(
                matches!(err, NativeRunError::Records),
                "case {case}: {err:?}"
            );
        }
        assert_eq!(audit(&p).await, before);
        assert_eq!(p.writes.load(Ordering::SeqCst), 0);
    }
    assert!(inspect(&h, Some(prompt.id()), r, "run").await.is_ok());
}

#[tokio::test]
async fn record_count_and_serialization_limits_are_exact_logical_bounds() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    let actual = p
        .inner
        .find_records(&RecordQuery::default())
        .await
        .unwrap()
        .pop()
        .unwrap();
    let mut records = Vec::new();
    for i in 0..MAX_NATIVE_RECORDS {
        let mut record = actual.clone();
        if let LaneRecord::OperationStarted(ref mut x) = record {
            x.base.id = if i == 0 {
                "run".into()
            } else {
                format!("other-{i}")
            };
            x.base.seq = i as u64 + 1;
        }
        records.push(record);
    }
    p.revision
        .store((MAX_NATIVE_RECORDS + 10) as u64, Ordering::SeqCst);
    for chunk in records.chunks(256) {
        p.pages.lock().unwrap().push_back(RecordPage {
            records: chunk.to_vec(),
            total: MAX_NATIVE_RECORDS - (chunk[0].seq() as usize - 1),
        });
    }
    p.reset();
    assert_eq!(
        inspect(
            &h,
            Some(prompt.id()),
            (MAX_NATIVE_RECORDS + 10) as u64,
            "run"
        )
        .await
        .unwrap()
        .run()
        .status,
        RunStatus::Open
    );
    assert_eq!(p.page_calls.load(Ordering::SeqCst), 16);
    p.pages.lock().unwrap().push_back(RecordPage {
        records: vec![],
        total: MAX_NATIVE_RECORDS + 1,
    });
    is_source(
        inspect(
            &h,
            Some(prompt.id()),
            (MAX_NATIVE_RECORDS + 10) as u64,
            "run",
        )
        .await
        .unwrap_err(),
        SourceError::Limit,
    );
    p.revision.store(0, Ordering::SeqCst);
    // A one-record typed serialized payload at exactly the byte cap succeeds;
    // one additional byte fails. This does not allocate a real huge session.
    let mut large = actual.clone();
    if let LaneRecord::OperationStarted(ref mut x) = large {
        x.intent = OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: Some(String::new()),
            resume_data: None,
        };
    }
    let overhead = serde_json::to_vec(&large).unwrap().len();
    if let LaneRecord::OperationStarted(ref mut x) = large {
        if let OperationIntent::Run {
            ref mut system_prompt_override,
            ..
        } = x.intent
        {
            *system_prompt_override = Some("x".repeat(MAX_NATIVE_RECORD_BYTES - overhead));
        }
    }
    assert_eq!(
        serde_json::to_vec(&large).unwrap().len(),
        MAX_NATIVE_RECORD_BYTES
    );
    let r = p.inner.history_revision().await.unwrap();
    p.pages.lock().unwrap().push_back(RecordPage {
        records: vec![large.clone()],
        total: 1,
    });
    assert!(inspect(&h, Some(prompt.id()), r, "run").await.is_ok());
    if let LaneRecord::OperationStarted(ref mut x) = large {
        if let OperationIntent::Run {
            ref mut system_prompt_override,
            ..
        } = x.intent
        {
            system_prompt_override.as_mut().unwrap().push('x');
        }
    }
    p.pages.lock().unwrap().push_back(RecordPage {
        records: vec![large],
        total: 1,
    });
    is_source(
        inspect(&h, Some(prompt.id()), r, "run").await.unwrap_err(),
        SourceError::Limit,
    );
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn captured_record_page_append_navigation_rebind_and_proof_errors_are_outer_fenced() {
    bounded(async {
        for change in ["append", "navigation", "rebind"] {
            for wrong_run in [false, true] {
                let (h, p) = memory().await;
                let prompt = start(&h, "run").await;
                let r = p.inner.history_revision().await.unwrap();
                p.reset();
                p.pause_page.store(true, Ordering::SeqCst);
                let child_h = h.clone();
                let id = prompt.id().to_owned();
                let task = tokio::spawn(async move {
                    inspect(
                        &child_h,
                        Some(&id),
                        r,
                        if wrong_run { "missing" } else { "run" },
                    )
                    .await
                });
                p.page_captured.notified().await;
                match change {
                    "append" => {
                        h.session()
                            .append_record(LaneRecord::AbortRequested(AbortRequestedRecord {
                                base: base("external"),
                                run_id: "run".into(),
                            }))
                            .await
                            .unwrap();
                    }
                    "navigation" => h.session().move_lane("main", None).await.unwrap(),
                    "rebind" => h.session().set_storage(p.clone()),
                    _ => unreachable!(),
                }
                let after = audit(&p).await;
                p.reset();
                p.release_page.notify_one();
                is_source(task.await.unwrap().unwrap_err(), SourceError::Conflict);
                assert_eq!(audit(&p).await, after);
                assert_eq!(p.writes.load(Ordering::SeqCst), 0);
            }
        }
    })
    .await
}

#[tokio::test]
async fn final_read_faults_override_collected_error_and_both_final_reads_complete() {
    for lane_fault in [false, true] {
        let (h, p) = memory().await;
        let prompt = start(&h, "run").await;
        let r = p.inner.history_revision().await.unwrap();
        p.reset();
        if lane_fault {
            p.fail_lane_at.store(4, Ordering::SeqCst);
        } else {
            p.fail_revision_at.store(4, Ordering::SeqCst);
        }
        is_source(
            inspect(&h, Some(prompt.id()), r, "missing")
                .await
                .unwrap_err(),
            SourceError::Read,
        );
        assert_eq!(p.revision_calls.load(Ordering::SeqCst), 4);
        assert_eq!(p.lane_calls.load(Ordering::SeqCst), 4);
        assert_eq!(p.writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn dropped_reader_has_no_worker_late_reads_or_mutations() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    let r = p.inner.history_revision().await.unwrap();
    p.reset();
    p.pause_page.store(true, Ordering::SeqCst);
    let h2 = h.clone();
    let id = prompt.id().to_owned();
    let task = tokio::spawn(async move { inspect(&h2, Some(&id), r, "run").await });
    bounded(p.page_captured.notified()).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let before = audit(&p).await;
    let reads = p.reads.load(Ordering::SeqCst);
    p.release_page.notify_one();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(p.reads.load(Ordering::SeqCst), reads);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
    assert_eq!(audit(&p).await, before);
    assert!(inspect(&h, Some(prompt.id()), r, "run").await.is_ok());
}

#[tokio::test]
async fn invalid_native_tool_ordinal_is_rejected_but_earlier_watermark_remains_readable() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    let earlier = p.inner.history_revision().await.unwrap();
    let mut msg = faux_assistant_message("private assistant", StopReason::ToolUse);
    msg.content
        .push(Content::tool_call("call", "write", json!({})));
    let assistant = append(&h, "assistant", AgentMessage::Assistant(Box::new(msg))).await;
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("invocation"),
            run_id: "run".into(),
            assistant_entry_id: assistant.id().into(),
            tool_index: 7,
            tool_call_id: "call".into(),
            tool_name: "write".into(),
            effective_args: json!({}),
            result_entry_id: "reserved-result".into(),
            replay: ToolReplay::Never,
        }))
        .await
        .unwrap();
    let now = p.inner.history_revision().await.unwrap();
    p.reset();
    let before = audit(&p).await;
    for _ in 0..2 {
        assert!(matches!(
            inspect(&h, Some(assistant.id()), now, "run").await,
            Err(NativeRunError::Native(DurableError::InvalidJournal(_)))
        ));
        assert!(inspect(&h, Some(prompt.id()), earlier, "run")
            .await
            .unwrap()
            .run()
            .tools
            .is_empty());
    }
    assert_eq!(audit(&p).await, before);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn maximal_revision_bound_does_not_overflow_and_page_faults_still_observe_outer_scope() {
    let (h, p) = memory().await;
    let prompt = start(&h, "run").await;
    p.revision.store(u64::MAX, Ordering::SeqCst);
    p.reset();
    let result = inspect(&h, Some(prompt.id()), u64::MAX, "run")
        .await
        .unwrap();
    assert_eq!(result.as_of_revision(), u64::MAX);
    assert_eq!(result.scope().history_revision, u64::MAX);
    assert_eq!(result.run().status, RunStatus::Open);
    p.fail_pages.store(true, Ordering::SeqCst);
    p.reset();
    is_source(
        inspect(&h, Some(prompt.id()), u64::MAX, "run")
            .await
            .unwrap_err(),
        SourceError::Read,
    );
    assert_eq!(p.lane_calls.load(Ordering::SeqCst), 4);
    assert_eq!(p.revision_calls.load(Ordering::SeqCst), 4);
    assert_eq!(p.writes.load(Ordering::SeqCst), 0);
}
