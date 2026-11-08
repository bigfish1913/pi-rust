//! Admitted-run and threshold compaction boundaries; not transactional rollback.
use super::*;
use rpi_ai::{
    providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep},
    types::StopReason,
};
use rpi_harness::{
    agent_harness::HarnessRunOutcome,
    events::HarnessEvent,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    },
    types::{CompactionSettings, MessageDelivery, SendMessageOptions},
};
use rpi_tools::{FileSystem, OsExecutionEnv};

fn calls(p: &FauxProvider) -> usize {
    p.state().call_count.load(Ordering::SeqCst)
}
fn configured(session: Session, p: Arc<FauxProvider>, required: bool) -> AgentHarnessOptions {
    let mut model = p.default_model().clone();
    model.context_window = 512;
    AgentHarnessOptions {
        model,
        models: vec![p],
        require_native_durable: required,
        compaction: CompactionSettings {
            enabled: true,
            reserve_tokens: 20,
            keep_recent_tokens: 1,
        },
        ..options(session)
    }
}
async fn entries(h: &AgentHarness) -> Vec<Entry> {
    h.session()
        .storage()
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
}
async fn controls(h: &AgentHarness) -> Value {
    json!(h
        .session()
        .storage()
        .find_entries(&EntryQuery {
            custom_type: Some(CONTROL.into()),
            ..Default::default()
        })
        .await
        .unwrap())
}
async fn long_history(session: &Session) {
    for n in 0..12 {
        session
            .append_message(AgentMessage::User(UserMessage::new(
                format!("synthetic {n}: {}", "long synthetic history ".repeat(40)),
                n,
            )))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::Assistant(Box::new(faux_assistant_message(
                "synthetic answer",
                StopReason::Stop,
            ))))
            .await
            .unwrap();
    }
}
async fn next_turn(h: &AgentHarness) {
    h.send_message(
        AgentMessage::User(UserMessage::new("reserved next turn", 0)),
        SendMessageOptions {
            trigger_turn: Some(false),
            deliver_as: MessageDelivery::NextTurn,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn initial_run_guard_precedes_recovery_reservation_prompt_and_provider() {
    for automatic in [false, true] {
        for kind in ["durable-state", "rpi-durable-runtime", CONTROL] {
            let p = FauxProvider::new(FauxScript::new().with_text("must not execute"));
            let tool = NoCalls::new();
            let mut opts = configured(memory(), p.clone(), true);
            opts.compaction.enabled = automatic;
            opts.tools = vec![HarnessTool::new(tool.clone())];
            let h = AgentHarness::create(opts).await.unwrap();
            next_turn(&h).await;
            h.follow_up(AgentMessage::User(UserMessage::new("keep follow up", 0)))
                .await
                .unwrap();
            let child = h
                .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
                .await
                .unwrap();
            prune_sentinel(h.session()).await;
            open_operation(h.session()).await;
            custom(
                h.session(),
                "main",
                "bad-run-tip",
                kind,
                Some(json!({"private":"NEVER-REFLECT"})),
            )
            .await;
            let before = image(h.session()).await;
            let queues = h.queued_messages().await.unwrap();
            let error = bounded(h.prompt_text("not persisted", vec![]))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("Unsupported durable journal format")
                    || error.contains("Invalid native durable journal")
            );
            assert!(!error.contains("NEVER-REFLECT"));
            assert_eq!(image(h.session()).await, before);
            assert_eq!(h.queued_messages().await.unwrap(), queues);
            assert!(!child.scope.is_cancelled());
            assert!(h.snapshot().await.unwrap().active.is_none());
            assert_eq!(calls(&p), 0);
            assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn initial_run_read_failure_revision_race_and_task_cancel_release_gate_and_lease() {
    let base = memory();
    let probe = probe::Probe::new(base.storage());
    let p = FauxProvider::new(FauxScript::new().with_text("unused"));
    let h = AgentHarness::create(configured(
        Session::new(probe.clone(), None),
        p.clone(),
        true,
    ))
    .await
    .unwrap();
    apply(
        &service(&h),
        "keep",
        json!({"action":"enqueue","text":"manual"}),
    )
    .await;
    next_turn(&h).await;
    let queues = h.queued_messages().await.unwrap();
    for mode in ["failure", "race", "cancel"] {
        let mut expected = image(&base).await;
        if mode == "failure" {
            probe.fail_reads.store(true, Ordering::SeqCst);
            let error = bounded(h.prompt_text("not committed", vec![]))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("Native durable session validation failed"));
            assert!(!error.contains("injected"));
            probe.fail_reads.store(false, Ordering::SeqCst);
        } else {
            probe.pause_branch_reads.store(true, Ordering::SeqCst);
            let h = h.clone();
            let worker = tokio::spawn(async move { h.prompt_text("not committed", vec![]).await });
            bounded(probe.branch_read_entered.notified()).await;
            if mode == "cancel" {
                worker.abort();
                assert!(bounded(worker).await.unwrap_err().is_cancelled());
            } else {
                base.append_custom_entry("external-synthetic-race", None)
                    .await
                    .unwrap();
                expected = image(&base).await;
                probe.release_branch_read.notify_one();
                assert!(bounded(worker)
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("changed during validation"));
            }
        }
        assert_eq!(image(&base).await, expected);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert!(h.snapshot().await.unwrap().active.is_none());
        bounded(service(&h).snapshot()).await.unwrap();
        assert_eq!(calls(&p), 0);
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn pre_run_threshold_rechecks_after_run_start_and_releases_on_race_or_task_cancel() {
    for cancel in [false, true] {
        let base = memory();
        let probe = probe::Probe::new(base.storage());
        let p = FauxProvider::new(FauxScript::new().with_text("summary must not execute"));
        let h = AgentHarness::create(configured(
            Session::new(probe.clone(), None),
            p.clone(),
            true,
        ))
        .await
        .unwrap();
        apply(
            &service(&h),
            "keep",
            json!({"action":"enqueue","text":"manual"}),
        )
        .await;
        long_history(&base).await;
        let receipt = service(&h).request_outcome("keep").await.unwrap();
        let pause = probe.clone();
        let mut watch = h.events().watch(|| ());
        watch.start(Arc::new(move |event: &HarnessEvent| {
            if matches!(event, HarnessEvent::RunStart(_)) {
                pause.pause_branch_reads.store(true, Ordering::SeqCst);
            }
        }));
        let worker = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("threshold", vec![]).await })
        };
        bounded(probe.branch_read_entered.notified()).await;
        assert_eq!(calls(&p), 0);
        assert!(h.snapshot().await.unwrap().active.is_some());
        // Admission/prompt are already committed at this *later* boundary.
        let mut expected = image(&base).await;
        if cancel {
            worker.abort();
            assert!(bounded(worker).await.unwrap_err().is_cancelled());
        } else {
            base.append_custom_entry("late-synthetic-race", None)
                .await
                .unwrap();
            expected = image(&base).await;
            probe.release_branch_read.notify_one();
            assert!(bounded(worker)
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("changed during validation"));
        }
        assert_eq!(image(&base).await, expected);
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert_eq!(service(&h).request_outcome("keep").await.unwrap(), receipt);
        assert_eq!(calls(&p), 0);
        assert!(!entries(&h)
            .await
            .iter()
            .any(|e| matches!(e, Entry::Compaction(_))));
        let records = base.find_records(&RecordQuery::default()).await.unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, LaneRecord::OperationStarted(_)))
                .count(),
            1
        );
        assert!(!records
            .iter()
            .any(|r| matches!(r, LaneRecord::OperationFinished(_))));
    }
}

struct Inflate {
    session: Session,
    damage: Option<&'static str>,
    pause: Option<Arc<probe::Probe>>,
    count: AtomicUsize,
    schema: Tool,
}
impl Inflate {
    fn new(session: Session, damage: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            session,
            damage,
            pause: None,
            count: AtomicUsize::new(0),
            schema: Tool {
                name: "inflate".into(),
                description: "Synthetic tool result".into(),
                parameters: Schema::new(json!({"type":"object"})),
                constrained_sampling: None,
            },
        })
    }
}
#[async_trait::async_trait]
impl AgentTool for Inflate {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "inflate"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if let Some(kind) = self.damage {
            // Direct synthetic storage injection, not an alternate production API.
            custom(
                &self.session,
                "main",
                "during-tool-tip",
                kind,
                Some(json!({"private":"NEVER-REFLECT"})),
            )
            .await;
        }
        if let Some(probe) = &self.pause {
            probe.pause_branch_reads.store(true, Ordering::SeqCst);
        }
        Ok(AgentToolResult::text(
            "large returned synthetic words ".repeat(1000),
        ))
    }
}
fn with_tool(mut opts: AgentHarnessOptions, tool: Arc<Inflate>) -> AgentHarnessOptions {
    opts.tools = vec![HarnessTool::new(tool)];
    opts.active_tool_names = vec!["inflate".into()];
    opts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_mid_loop_journal_refuses_summary_and_next_provider_without_fake_terminal() {
    for kind in ["durable-state", "rpi-durable-runtime", CONTROL] {
        let session = memory();
        let tool = Inflate::new(session.clone(), Some(kind));
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::tool_call("inflate", json!({})),
            FauxStep::text("must not summarize"),
            FauxStep::text("must not continue"),
        ]);
        let p = FauxProvider::new(script);
        let h = AgentHarness::create(with_tool(
            configured(session, p.clone(), true),
            tool.clone(),
        ))
        .await
        .unwrap();
        apply(
            &service(&h),
            "keep",
            json!({"action":"enqueue","text":"manual"}),
        )
        .await;
        let original = controls(&h).await;
        let error = bounded(h.prompt_text("inflate", vec![]))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Unsupported durable journal format")
                || error.contains("Invalid native durable journal")
        );
        assert!(!error.contains("NEVER-REFLECT"));
        assert_eq!(calls(&p), 1);
        assert_eq!(tool.count.load(Ordering::SeqCst), 1);
        assert!(h.snapshot().await.unwrap().active.is_none());
        let history = entries(&h).await;
        assert!(!history.iter().any(|e| matches!(e, Entry::Compaction(_))));
        assert!(history.iter().any(
            |e| matches!(e,Entry::Message(m) if matches!(m.message,AgentMessage::ToolResult(_)))
        ));
        let records = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, LaneRecord::OperationStarted(_)))
                .count(),
            1
        );
        assert!(records
            .iter()
            .any(|r| matches!(r, LaneRecord::ToolStarted(_))));
        assert!(!records
            .iter()
            .any(|r| matches!(r, LaneRecord::OperationFinished(_))));
        // Original committed control facts survive even when observation fails.
        let remaining = controls(&h).await;
        for fact in original.as_array().unwrap() {
            assert!(remaining.as_array().unwrap().contains(fact));
        }
        assert!(service(&h).snapshot().await.is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_loop_read_failure_race_and_caller_cancel_keep_admitted_facts_without_summary() {
    for mode in ["read", "race", "cancel"] {
        let base = memory();
        let probe = probe::Probe::new(base.storage());
        let mut tool = Inflate::new(base.clone(), None);
        Arc::get_mut(&mut tool).unwrap().pause = Some(probe.clone());
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::tool_call("inflate", json!({})),
            FauxStep::text("must not summarize"),
        ]);
        let provider = FauxProvider::new(script);
        let h = AgentHarness::create(with_tool(
            configured(Session::new(probe.clone(), None), provider.clone(), true),
            tool.clone(),
        ))
        .await
        .unwrap();
        apply(
            &service(&h),
            "keep",
            json!({"action":"enqueue","text":"manual"}),
        )
        .await;
        let receipt = service(&h).request_outcome("keep").await.unwrap();
        let worker = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("inflate", vec![]).await })
        };
        bounded(probe.branch_read_entered.notified()).await;
        assert_eq!(calls(&provider), 1);
        assert_eq!(tool.count.load(Ordering::SeqCst), 1);
        assert!(h.snapshot().await.unwrap().active.is_some());
        let mut expected = image(&base).await;
        if mode == "cancel" {
            worker.abort();
            assert!(bounded(worker).await.unwrap_err().is_cancelled());
        } else {
            if mode == "race" {
                base.append_custom_entry("mid-loop-synthetic-race", None)
                    .await
                    .unwrap();
                expected = image(&base).await;
            } else {
                // The post-read revision query fails, after the actual guarded
                // CONTROL read has entered. Do not confuse a context build error
                // or a first-run admission failure with this later boundary.
                probe.fail_revision.store(true, Ordering::SeqCst);
            }
            probe.release_branch_read.notify_one();
            let error = bounded(worker).await.unwrap().unwrap_err().to_string();
            assert!(error.contains(if mode == "race" {
                "changed during validation"
            } else {
                "Native durable session validation failed"
            }));
            assert!(!error.contains("injected"));
            probe.fail_revision.store(false, Ordering::SeqCst);
        }
        // A fail-closed signal can still settle an aborted assistant/frame via
        // the existing emitter. It is not a summary, provider call or native
        // operation terminal; don't demand rollback of that normal bookkeeping.
        let after = image(&base).await;
        for key in ["entries", "records"] {
            for fact in expected[key].as_array().unwrap() {
                assert!(after[key].as_array().unwrap().contains(fact));
            }
        }
        let added: Vec<_> = after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !expected["entries"].as_array().unwrap().contains(e))
            .collect();
        assert!(added.len() <= 1);
        for entry in &added {
            assert_eq!(entry["type"], "message");
            assert_eq!(entry["message"]["kind"], "assistant");
            assert_eq!(entry["message"]["stopReason"], "aborted");
        }
        for record in after["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| !expected["records"].as_array().unwrap().contains(r))
        {
            assert!(matches!(
                record["type"].as_str(),
                Some("assistant_frame" | "step_attempt" | "usage")
            ));
            if record["type"] == "step_attempt" {
                assert_eq!(record["step"], "assistant");
                assert!(added.iter().any(|e| e["id"] == record["resultEntryId"]));
            }
        }
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert_eq!(
            bounded(service(&h).request_outcome("keep")).await.unwrap(),
            receipt
        );
        assert_eq!(calls(&provider), 1);
        assert!(!entries(&h)
            .await
            .iter()
            .any(|e| matches!(e, Entry::Compaction(_))));
        let records = base.find_records(&RecordQuery::default()).await.unwrap();
        assert!(records
            .iter()
            .any(|r| matches!(r, LaneRecord::ToolStarted(_))));
        assert!(!records
            .iter()
            .any(|r| matches!(r, LaneRecord::OperationFinished(_))));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_pre_run_and_mid_loop_compaction_preserve_control_receipts_and_prior_audit() {
    for mid_loop in [false, true] {
        for disk in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
            let session = if disk {
                Session::new(
                    Arc::new(
                        JsonlSessionStorage::create(
                            fs.clone(),
                            "auto.jsonl",
                            JsonlV4Header::new(
                                "synthetic-auto".into(),
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
            let tool = Inflate::new(session.clone(), None);
            let script = FauxScript::new();
            let mut responses = vec![FauxStep::text("audited answer")];
            if mid_loop {
                responses.push(FauxStep::tool_call("inflate", json!({})));
            }
            responses.extend([
                FauxStep::text("## Goal\nAutomatic guard summary"),
                FauxStep::text("done"),
            ]);
            script.set_responses(responses);
            let p = FauxProvider::new(script);
            let h = AgentHarness::create(with_tool(
                configured(session, p.clone(), true),
                tool.clone(),
            ))
            .await
            .unwrap();
            apply(
                &service(&h),
                "workflow",
                json!({"action":"start","name":"keep","steps":[{"id":"one","title":"pending"}]}),
            )
            .await;
            apply(
                &service(&h),
                "queue",
                json!({"action":"enqueue","text":"keep manual"}),
            )
            .await;
            let native = bounded(h.prompt_text("audit", vec![])).await.unwrap();
            assert_eq!(calls(&p), 1);
            let audit = format!("{:?}", h.get_result(&native.run_id).await.unwrap().unwrap());
            let run = service(&h).execution(&native.run_id).await.unwrap().run;
            assert!(run.is_some());
            if !mid_loop {
                long_history(h.session()).await;
            }
            let before = service(&h).snapshot().await.unwrap();
            let receipt = service(&h).request_outcome("queue").await.unwrap();
            let facts = controls(&h).await;
            let old = request(&service(&h), "stale", json!({"action":"pause"})).await;
            let result = bounded(h.prompt_text("go", vec![])).await.unwrap();
            assert!(matches!(
                result.outcome,
                HarnessRunOutcome::Completed { .. }
            ));
            assert_eq!(calls(&p), if mid_loop { 4 } else { 3 });
            assert_eq!(tool.count.load(Ordering::SeqCst), usize::from(mid_loop));
            let compacted: Vec<_> = entries(&h)
                .await
                .into_iter()
                .filter_map(|e| {
                    if let Entry::Compaction(c) = e {
                        Some(c)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(compacted.len(), 1);
            assert!(compacted[0].summary.contains("Automatic guard summary"));
            assert!(compacted[0].tokens_before > 492);
            let records = h
                .session()
                .find_records(&RecordQuery::default())
                .await
                .unwrap();
            assert_eq!(records.iter().filter(|r| matches!(r,LaneRecord::StepAttempt(s) if s.run_id==result.run_id && s.compaction_reason==Some(CompactionReason::Threshold))).count(),1);
            let after = service(&h).snapshot().await.unwrap();
            assert_eq!(after.state, before.state);
            assert_eq!(after.execution_control, before.execution_control);
            assert_eq!(after.scope.entry_id, before.scope.entry_id);
            assert_ne!(after.scope.leaf_id, before.scope.leaf_id);
            assert_ne!(after.scope.history_revision, before.scope.history_revision);
            assert_eq!(controls(&h).await, facts);
            assert_eq!(service(&h).request_outcome("queue").await.unwrap(), receipt);
            assert_eq!(
                service(&h).execution(&native.run_id).await.unwrap().run,
                run
            );
            assert_eq!(
                format!("{:?}", h.get_result(&native.run_id).await.unwrap().unwrap()),
                audit
            );
            assert!(matches!(
                service(&h).mutate(old).await,
                Err(DurableError::Conflict)
            ));
            h.drain_session().await;
            h.close().await.unwrap();
            drop(h);
            if disk {
                // `Inflate` keeps a clone of the external storage binding.  The
                // harness close is not the lease-release boundary; release all
                // test-owned bindings before reopening the same JSONL path.
                drop(tool);
                let bytes = std::fs::read(tmp.path().join("auto.jsonl")).unwrap();
                let reopened = strict(Session::new(
                    Arc::new(
                        JsonlSessionStorage::load(
                            fs,
                            "auto.jsonl",
                            Arc::new(SystemClock),
                            Arc::new(DefaultIdGenerator::new()),
                        )
                        .await
                        .unwrap(),
                    ),
                    None,
                ))
                .await;
                assert_eq!(
                    service(&reopened).snapshot().await.unwrap().state,
                    before.state
                );
                assert_eq!(controls(&reopened).await, facts);
                assert_eq!(
                    service(&reopened).request_outcome("queue").await.unwrap(),
                    receipt
                );
                assert_eq!(
                    service(&reopened)
                        .execution(&native.run_id)
                        .await
                        .unwrap()
                        .run,
                    run
                );
                assert_eq!(
                    format!(
                        "{:?}",
                        reopened.get_result(&native.run_id).await.unwrap().unwrap()
                    ),
                    audit
                );
                reopened.drain_session().await;
                assert_eq!(std::fs::read(tmp.path().join("auto.jsonl")).unwrap(), bytes);
            }
        }
    }
}
