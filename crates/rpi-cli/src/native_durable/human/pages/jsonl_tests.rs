//! Actual Faux/TempDir JSONL continuation, hiding and provider-free cold replay.
use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::{
    agent_harness::{AgentLane, HarnessRunOutcome},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    },
};
use rpi_tools::OsExecutionEnv;

async fn result(h: &AgentHarness, id: &str) -> Value {
    let run = bounded(h.get_result(id)).await.unwrap().unwrap();
    assert_eq!(run.run_id, id);
    match run.outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => json!({"leaf":leaf_id,"entry":final_entry_id,"message":final_message}),
        other => panic!("expected actual completed result: {other:?}"),
    }
}
async fn chain(h: &AgentHarness, hidden: bool, id: &str) -> String {
    let s = native::service(h).unwrap();
    let original = audit(h).await;
    let mut args = "runs-page".to_owned();
    let mut cursor = None;
    let mut first_next = None;
    let mut counts = Vec::new();
    let mut seen = Vec::new();
    for i in 0..4 {
        let page = bounded(s.execution_page(cursor.as_ref())).await.unwrap();
        let text = bounded(native::interactive_command(h, &args))
            .await
            .unwrap();
        assert_eq!(text, render(&page).unwrap());
        mandatory(&text, &page);
        assert!(!text.contains("private faux"));
        if i < 2 {
            assert!(page.runs.is_empty() && page.next.is_some());
            assert!(text.contains("not a complete empty-history claim"));
        }
        if i == 0 {
            first_next = next_args(&text).map(str::to_owned);
        }
        assert_eq!(page.unavailable_runs, 0);
        assert_eq!(page.control.forgotten_run_ids.len(), usize::from(hidden));
        counts.push(page.scanned_records);
        seen.extend(page.runs.iter().map(|r| r.id.clone()));
        assert_eq!(audit(h).await, original);
        match next_args(&text) {
            Some(next) => {
                args = next.into();
                cursor = next_cursor(&text);
            }
            None => break,
        }
    }
    // The real Faux run has four records (including its request/terminal facts).
    assert_eq!(counts, [64, 64, 6]);
    assert_eq!(seen, if hidden { vec![] } else { vec![id.to_owned()] });
    first_next.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_empty_windows_explicit_continuation_forget_and_provider_free_reopen_keep_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let provider = FauxProvider::new(FauxScript::new().with_text("private faux page answer"));
    let h = bounded(AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        session: Session::new(
            Arc::new(
                JsonlSessionStorage::create(
                    fs.clone(),
                    "tui-pages.jsonl",
                    JsonlV4Header::new(
                        "synthetic-tui-pages".into(),
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
        ),
        require_native_durable: true,
        ..Default::default()
    }))
    .await
    .unwrap();
    let returned = bounded(h.prompt_text("private faux page prompt", vec![]))
        .await
        .unwrap();
    let id = returned.run_id.clone();
    let completed = result(&h, &id).await;
    for i in 0..65 {
        let operation = format!("tui-page-compaction-{i}");
        h.session()
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: base(&operation),
                source_leaf_id: h.session().view("main").get_leaf_id().await.unwrap(),
                intent: OperationIntent::Compaction {
                    custom_instructions: None,
                    result_entry_id: format!("{operation}-result"),
                },
            }))
            .await
            .unwrap();
        h.session()
            .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                base: base(&format!("{operation}-finish")),
                run_id: operation,
                outcome: OperationOutcome::Completed,
                error: None,
                result: None,
            }))
            .await
            .unwrap();
    }
    let s = native::service(&h).unwrap();
    bounded(s.drain()).await.unwrap();
    let path = tmp.path().join("tui-pages.jsonl");
    let original_bytes = std::fs::read(&path).unwrap();
    let original = audit(&h).await;
    let old_next = chain(&h, false, &id).await;
    assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    assert_eq!(result(&h, &id).await, completed);
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    let ack = native::command(
        &h,
        &json!({"action":"forget_run","runId":id,"requestId":"tui-pages-forget-001"}).to_string(),
    )
    .await
    .unwrap();
    assert!(ack.get("outcome").is_some(), "{ack}");
    assert!(native::interactive_command(&h, &old_next)
        .await
        .unwrap_err()
        .contains("conflict"));
    bounded(s.drain()).await.unwrap();
    let forgotten_bytes = std::fs::read(&path).unwrap();
    let forgotten = audit(&h).await;
    assert_eq!(original.0, forgotten.0);
    assert_eq!(forgotten.1.as_array().unwrap().len(), 1);
    let reopen_next = chain(&h, true, &id).await;
    assert_eq!(result(&h, &id).await, completed);
    assert_eq!(std::fs::read(&path).unwrap(), forgotten_bytes);
    bounded(s.drain()).await.unwrap();
    h.close().await.unwrap();
    drop(s);
    drop(h);
    drop(returned);
    let h = bounded(AgentHarness::create(AgentHarnessOptions {
        session: Session::new(
            Arc::new(
                JsonlSessionStorage::load(
                    fs,
                    "tui-pages.jsonl",
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            ),
            None,
        ),
        allow_existing_session: true,
        require_native_durable: true,
        ..Default::default()
    }))
    .await
    .unwrap();
    assert!(native::interactive_command(&h, &reopen_next)
        .await
        .unwrap_err()
        .contains("conflict"));
    chain(&h, true, &id).await;
    assert_eq!(result(&h, &id).await, completed);
    assert_eq!(audit(&h).await, forgotten);
    assert_eq!(std::fs::read(&path).unwrap(), forgotten_bytes);
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    let receipt = native::command(
        &h,
        r#"{"action":"status","requestId":"tui-pages-forget-001"}"#,
    )
    .await
    .unwrap();
    assert!(receipt.to_string().contains("tui-pages-forget-001"));
    bounded(native::service(&h).unwrap().drain()).await.unwrap();
    h.close().await.unwrap();
}
