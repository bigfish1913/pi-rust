use super::*;
use rpi_agent::AgentMessage;
use rpi_ai::{
    providers::faux::{FauxProvider, FauxScript},
    types::UserMessage,
};
use rpi_durable::execution::{ExecutionError, RunStatus};
use rpi_harness::agent_harness::AgentLane;
use rpi_harness::session::types::*;

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
async fn interrupted(h: &AgentHarness, id: &str) {
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
            source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let prompt = format!("{id}-prompt");
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: prompt.clone(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(UserMessage::new("synthetic prompt", 0)),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finished")),
            run_id: id.into(),
            outcome: OperationOutcome::Aborted,
            error: Some(OperationError {
                code: "interrupted".into(),
                message: "synthetic interruption".into(),
            }),
            result: Some(RunResultIdentity {
                leaf_id: prompt,
                final_entry_id: None,
                assistant_entry_id: None,
            }),
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_v2_execution_receipts_replay_historical_proof_and_reset_preserves_intent() {
    for disk in [false, true] {
        let (session, _dir) = fixture::fixture("writer-execution", disk).await;
        let h = open(session, Some(policy(EPOCH))).await;
        let s = owner(&h);
        apply(&s, "start", start()).await;
        interrupted(&h, "interrupted-run").await;
        let facts_before = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        assert_eq!(
            s.execution("interrupted-run")
                .await
                .unwrap()
                .run
                .unwrap()
                .status,
            RunStatus::Interrupted
        );
        let recover = request(
            &s,
            "recover",
            json!({"action":"recover"}),
            Some("interrupted-run"),
        )
        .await;
        let recovered = s.mutate(recover).await.unwrap();
        let intent = recovered.snapshot.execution_control.clone();
        assert!(intent.recovery_request.is_some());
        let reset = apply(&s, "reset", json!({"action":"reset"})).await;
        assert_eq!(reset.snapshot.execution_control, intent);
        let forget = request(
            &s,
            "forget",
            json!({"action":"forget_run"}),
            Some("interrupted-run"),
        )
        .await;
        let forgotten = s.mutate(forget).await.unwrap();
        assert!(forgotten
            .snapshot
            .execution_control
            .recovery_request
            .is_none());
        assert_eq!(
            forgotten.snapshot.execution_control.forgotten_run_ids,
            ["interrupted-run"]
        );
        assert!(s.execution("interrupted-run").await.unwrap().run.is_none());
        let again = request(
            &s,
            "recover-forgotten",
            json!({"action":"recover"}),
            Some("interrupted-run"),
        )
        .await;
        assert!(matches!(
            s.mutate(again).await,
            Err(DurableError::Execution(ExecutionError::NoRun))
        ));
        assert_eq!(
            h.session()
                .find_records(&RecordQuery::default())
                .await
                .unwrap(),
            facts_before
        );
        assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 4);
        let reopened = open(
            Session::new(h.session().storage(), None),
            Some(policy(EPOCH)),
        )
        .await;
        assert_eq!(
            owner(&reopened).snapshot().await.unwrap().execution_control,
            forgotten.snapshot.execution_control
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn experimental_admitted_faux_run_and_forget_keep_native_result_audit() {
    for disk in [false, true] {
        let (session, _dir) = fixture::fixture("writer-faux", disk).await;
        let provider = FauxProvider::new(FauxScript::new().with_text("synthetic answer"));
        let h = AgentHarness::create_experimental(
            AgentHarnessOptions {
                session,
                model: provider.default_model().clone(),
                models: vec![provider],
                require_native_durable: true,
                ..Default::default()
            },
            policy(EPOCH),
        )
        .await
        .unwrap();
        let s = owner(&h);
        apply(&s, "workflow", start()).await;
        let run = bounded(h.prompt_text("synthetic question", vec![]))
            .await
            .unwrap();
        let observed = s.execution(&run.run_id).await.unwrap().run.unwrap();
        assert_eq!(observed.status, RunStatus::Completed);
        let facts = h
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        let request = request(
            &s,
            "forget-faux",
            json!({"action":"forget_run"}),
            Some(&run.run_id),
        )
        .await;
        s.mutate(request).await.unwrap();
        assert_eq!(
            h.session()
                .find_records(&RecordQuery::default())
                .await
                .unwrap(),
            facts
        );
        assert!(s.execution(&run.run_id).await.unwrap().run.is_none());
        assert!(s.snapshot().await.unwrap().state.workflow.is_some());
    }
}

#[tokio::test]
async fn prospective_historical_record_budget_is_checked_before_execution_append() {
    let h = memory_v2("writer-historical-budget").await;
    let s = owner(&h);
    interrupted(&h, "budget-run").await;
    let limit = experimental_ledger::native_run::MAX_NATIVE_RECORDS;
    // Actual lane records, not an overridden page total. Unrelated synthetic
    // queue tombstones also count against the historical reader's full budget.
    for i in 0..limit - 2 {
        h.session()
            .append_record(LaneRecord::QueueCancelled(QueueCancelledRecord {
                base: base(&format!("unrelated-cancel-{i}")),
                run_id: None,
                entry_id: format!("unrelated-enqueue-{i}"),
            }))
            .await
            .unwrap();
    }
    let accepted = request(
        &s,
        "at-budget",
        json!({"action":"recover"}),
        Some("budget-run"),
    )
    .await;
    s.mutate(accepted).await.unwrap();
    h.session()
        .append_record(LaneRecord::QueueCancelled(QueueCancelledRecord {
            base: base("one-too-many"),
            run_id: None,
            entry_id: "unrelated-enqueue-extra".into(),
        }))
        .await
        .unwrap();
    let before = audit(&h, CUSTOM_TYPE).await;
    let snapshot = s.snapshot().await.unwrap();
    let rejected = request(
        &s,
        "over-budget",
        json!({"action":"recover"}),
        Some("budget-run"),
    )
    .await;
    assert!(matches!(
        s.mutate(rejected).await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(audit(&h, CUSTOM_TYPE).await, before);
    assert_eq!(s.snapshot().await.unwrap(), snapshot);
    assert!(s.request_outcome("over-budget").await.unwrap().is_none());
    assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 1);
}
