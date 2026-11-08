//! Explicit compaction only. Automatic threshold/mid-loop compaction is separate.
use super::*;
use rpi_ai::{
    providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep},
    types::StopReason,
};
use rpi_harness::{
    agent_harness::CompactionOutcome,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    },
    types::CompactionSettings,
};
use rpi_tools::{FileSystem, OsExecutionEnv};

fn calls(provider: &FauxProvider) -> usize {
    provider.state().call_count.load(Ordering::SeqCst)
}
fn configured(
    session: Session,
    provider: Arc<FauxProvider>,
    required: bool,
) -> AgentHarnessOptions {
    AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        // Disable automatic compaction so each explicit request is unambiguous.
        compaction: CompactionSettings {
            enabled: false,
            reserve_tokens: 100,
            keep_recent_tokens: 20,
        },
        require_native_durable: required,
        ..options(session)
    }
}

#[tokio::test]
async fn invalid_journal_rejects_before_recovery_intent_or_provider_and_releases_lease() {
    for (kind, payload) in [
        ("durable-state", None),
        ("rpi-durable-runtime", None),
        (CONTROL, Some(json!({"private":"MUST-NOT-REFLECT"}))),
    ] {
        let provider = FauxProvider::new(FauxScript::new().with_text("must not execute"));
        let tool = NoCalls::new();
        let mut opts = configured(memory(), provider.clone(), true);
        opts.tools = vec![HarnessTool::new(tool.clone())];
        opts.active_tool_names = vec!["policy-effect".into()];
        let h = AgentHarness::create(opts).await.unwrap();
        h.follow_up(AgentMessage::User(UserMessage::new("keep queued", 0)))
            .await
            .unwrap();
        let child = h
            .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
            .await
            .unwrap();
        // These are direct synthetic fixture writes after valid startup.
        prune_sentinel(h.session()).await;
        open_operation(h.session()).await;
        custom(h.session(), "main", "bad-compaction-tip", kind, payload).await;
        let before = image(h.session()).await;
        let queues = h.queued_messages().await.unwrap();
        let error = bounded(h.compact(None)).await.unwrap_err().to_string();
        assert!(!error.contains("MUST-NOT-REFLECT"));
        assert!(
            error.contains("explicit native import")
                || error.contains("Invalid native durable journal")
        );
        assert_eq!(image(h.session()).await, before);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert!(!child.scope.is_cancelled());
        assert_eq!(calls(&provider), 0);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        // The rejected read left no admitted operation behind. Returning to an
        // independent empty session and declining compaction can acquire again.
        h.set_session(memory()).await.unwrap();
        assert!(matches!(
            bounded(h.compact(None)).await.unwrap().outcome,
            CompactionOutcome::Declined { .. }
        ));
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert_eq!(calls(&provider), 0);
    }
}

#[tokio::test]
async fn read_failure_revision_race_and_cancel_before_execution_leave_no_compaction_facts() {
    let base = memory();
    let probe = probe::Probe::new(base.storage());
    let provider = FauxProvider::new(FauxScript::new().with_text("must not execute"));
    let h = AgentHarness::create(configured(
        Session::new(probe.clone(), None),
        provider.clone(),
        true,
    ))
    .await
    .unwrap();
    apply(
        &service(&h),
        "native-queue",
        json!({"action":"enqueue", "text":"keep"}),
    )
    .await;
    h.follow_up(AgentMessage::User(UserMessage::new("stay queued", 0)))
        .await
        .unwrap();
    let child = h
        .create_child(memory(), Arc::new(CollectorEmitter::new().0), 1, None)
        .await
        .unwrap();
    let queues = h.queued_messages().await.unwrap();
    for mode in ["failure", "race", "cancel"] {
        let before = service(&h).snapshot().await.unwrap();
        let mut expected = image(&base).await;
        if mode == "failure" {
            probe.fail_reads.store(true, Ordering::SeqCst);
            let error = bounded(h.compact(None)).await.unwrap_err().to_string();
            assert!(error.contains("Native durable session validation failed"));
            assert!(!error.contains("injected"));
            probe.fail_reads.store(false, Ordering::SeqCst);
        } else {
            probe.pause_branch_reads.store(true, Ordering::SeqCst);
            let owner = h.clone();
            let task = tokio::spawn(async move { owner.compact(None).await });
            bounded(probe.branch_read_entered.notified()).await;
            // The in-memory run lease exists while validation is pending, but
            // there is still no durable operation intent or provider request.
            assert!(h.snapshot().await.unwrap().active.is_some());
            if mode == "cancel" {
                task.abort();
                assert!(bounded(task).await.unwrap_err().is_cancelled());
            } else {
                base.append_custom_entry("synthetic-compaction-race", None)
                    .await
                    .unwrap();
                expected = image(&base).await;
                probe.release_branch_read.notify_one();
                let error = bounded(task).await.unwrap().unwrap_err().to_string();
                assert!(error.contains("changed during validation"));
            }
        }
        let after = bounded(service(&h).snapshot()).await.unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.execution_control, before.execution_control);
        assert_eq!(image(&base).await, expected);
        assert_eq!(h.queued_messages().await.unwrap(), queues);
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert!(!child.scope.is_cancelled());
        assert_eq!(calls(&provider), 0);
        assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    }
    probe.release_branch_read.notify_one();
    // A custom-only branch is not an empty transcript for prepare_compaction;
    // it can legitimately request a summary. Use an actually empty replacement
    // to test released admission without authorizing any model request.
    h.set_session(memory()).await.unwrap();
    assert!(matches!(
        bounded(h.compact(None)).await.unwrap().outcome,
        CompactionOutcome::Declined { .. }
    ));
    assert_eq!(calls(&provider), 0);
}

#[tokio::test]
async fn default_legacy_compaction_and_native_decline_retain_existing_behavior() {
    for required in [false, true] {
        let session = memory();
        if !required {
            custom(&session, "main", "opaque", "durable-state", None).await;
        }
        let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
        let mut opts = configured(session, provider.clone(), required);
        opts.models.clear();
        let h = AgentHarness::create(opts).await.unwrap();
        let result = bounded(h.compact(None)).await.unwrap();
        let terminal = if required {
            assert!(matches!(result.outcome, CompactionOutcome::Declined { .. }));
            OperationOutcome::Declined
        } else {
            // Default policy permits the opaque legacy branch to reach ordinary
            // compaction preparation/provider resolution, not native validation.
            assert!(
                matches!(result.outcome, CompactionOutcome::Failed { ref error, .. } if error.code == "no_provider")
            );
            OperationOutcome::Failed
        };
        let records = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        assert_eq!(
            records
                .iter()
                .filter(
                    |r| matches!(r, LaneRecord::OperationStarted(s) if s.base.id == result.run_id)
                )
                .count(),
            1
        );
        assert_eq!(records.iter().filter(|r| matches!(r, LaneRecord::OperationFinished(f) if f.run_id == result.run_id && f.outcome == terminal)).count(), 1);
        assert!(h.snapshot().await.unwrap().active.is_none());
        assert_eq!(calls(&provider), 0);
        h.close().await.unwrap();
        let before = image(h.session()).await;
        assert!(bounded(h.compact(None))
            .await
            .unwrap_err()
            .to_string()
            .contains("closed"));
        assert_eq!(image(h.session()).await, before);
    }
}

async fn control_entries(h: &AgentHarness) -> Value {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_explicit_compaction_preserves_native_state_receipts_and_audit_in_memory_and_jsonl() {
    for disk in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
        let session = if disk {
            Session::new(
                Arc::new(
                    JsonlSessionStorage::create(
                        fs.clone(),
                        "compact.jsonl",
                        JsonlV4Header::new(
                            "synthetic-compact".into(),
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
                    .unwrap(),
                ),
                None,
            )
        } else {
            memory()
        };
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::text("audited answer"),
            FauxStep::text("## Goal\nSynthetic compact summary"),
        ]);
        let provider = FauxProvider::new(script);
        let h = AgentHarness::create(configured(session, provider.clone(), true))
            .await
            .unwrap();
        apply(&service(&h), "workflow", json!({"action":"start", "name":"Keep workflow", "steps":[{"id":"one", "title":"pending"}]})).await;
        apply(
            &service(&h),
            "queue",
            json!({"action":"enqueue", "text":"Keep manual queue"}),
        )
        .await;
        let native = bounded(h.prompt_text("audit prompt", vec![]))
            .await
            .unwrap();
        let audit = format!("{:?}", h.get_result(&native.run_id).await.unwrap().unwrap());
        let run = service(&h).execution(&native.run_id).await.unwrap().run;
        assert!(run.is_some());
        // Long synthetic transcript, no tool execution and no extra model calls.
        for n in 0..12 {
            h.session()
                .append_message(AgentMessage::User(UserMessage::new(
                    format!("turn {n}: {}", "synthetic words ".repeat(40)),
                    n,
                )))
                .await
                .unwrap();
            h.session()
                .append_message(AgentMessage::Assistant(Box::new(faux_assistant_message(
                    "short synthetic reply",
                    StopReason::Stop,
                ))))
                .await
                .unwrap();
        }
        let before = service(&h).snapshot().await.unwrap();
        let receipt = service(&h).request_outcome("queue").await.unwrap();
        let controls = control_entries(&h).await;
        let old_request = request(
            &service(&h),
            "stale-after-compact",
            json!({"action":"pause"}),
        )
        .await;
        let result = bounded(h.compact(Some("synthetic instructions")))
            .await
            .unwrap();
        let CompactionOutcome::Completed {
            entry: Entry::Compaction(entry),
            ..
        } = result.outcome
        else {
            panic!("explicit compaction must complete")
        };
        assert!(entry.summary.contains("Synthetic compact summary"));
        assert_eq!(calls(&provider), 2);
        let after = service(&h).snapshot().await.unwrap();
        assert_eq!(after.state, before.state);
        assert_eq!(after.execution_control, before.execution_control);
        assert_eq!(after.scope.entry_id, before.scope.entry_id);
        assert_ne!(after.scope.leaf_id, before.scope.leaf_id);
        assert_ne!(after.scope.history_revision, before.scope.history_revision);
        assert_eq!(control_entries(&h).await, controls);
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
            service(&h).mutate(old_request).await,
            Err(DurableError::Conflict)
        ));
        let records = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        assert_eq!(records.iter().filter(|r| matches!(r, LaneRecord::OperationFinished(f) if f.run_id == result.run_id && f.outcome == OperationOutcome::Completed)).count(), 1);
        h.drain_session().await;
        h.close().await.unwrap();
        drop(h);
        if disk {
            let bytes = std::fs::read(tmp.path().join("compact.jsonl")).unwrap();
            let reopened = strict(Session::new(
                Arc::new(
                    JsonlSessionStorage::load(
                        fs,
                        "compact.jsonl",
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
            assert_eq!(control_entries(&reopened).await, controls);
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
            assert_eq!(
                std::fs::read(tmp.path().join("compact.jsonl")).unwrap(),
                bytes
            );
            assert_eq!(calls(&provider), 2);
        }
    }
}
