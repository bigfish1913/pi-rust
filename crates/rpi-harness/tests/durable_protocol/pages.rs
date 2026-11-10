//! Versioned explicit paging uses the existing core proof, not a wire-side index.
use super::*;
use rpi_ai::types::UserMessage;
use rpi_harness::session::types::*;

fn frame(cursor: Value) -> Value {
    json!({"type":"get_durable_runs","version":1,"cursor":cursor})
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        timestamp: 0,
        lane: "main".into(),
    }
}
async fn closed(h: &AgentHarness, id: &str, run: bool) {
    let source = h.session().view("main").get_leaf_id().await.unwrap();
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id),
            source_leaf_id: source,
            intent: if run {
                OperationIntent::Run {
                    original_prompt: vec![],
                    initial_messages: vec![],
                    system_prompt_override: None,
                    resume_data: None,
                }
            } else {
                OperationIntent::Compaction {
                    custom_instructions: Some("private filtered".into()),
                    result_entry_id: format!("{id}-unused"),
                }
            },
        }))
        .await
        .unwrap();
    let result = if run {
        let anchor = format!("{id}-prompt");
        h.session()
            .append_entry(
                ProvisionedEntry {
                    id: anchor.clone(),
                    kind: ProvisionedKind::Message {
                        message: UserMessage::new("private wire prompt", 0).into(),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        Some(RunResultIdentity {
            leaf_id: anchor,
            final_entry_id: None,
            assistant_entry_id: None,
        })
    } else {
        None
    };
    h.session()
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(&format!("{id}-finish")),
            run_id: id.into(),
            outcome: OperationOutcome::Aborted,
            error: Some(OperationError {
                code: "interrupted".into(),
                message: "private terminal".into(),
            }),
            result,
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn page_cursor_validation_is_strict_complete_and_before_any_service_read() {
    bounded(async {
        let (h,p) = probed().await;
        let s = owner(&h);
        let scope = s.snapshot().await.unwrap().scope;
        let cursor = json!({"scope":scope,"beforeSeq":1});
        let mut invalid = vec![json!({}),json!({"scope":scope}),json!({"beforeSeq":1})];
        for field in protocol::SCOPE_FIELDS {
            let mut bad=cursor.clone(); bad["scope"].as_object_mut().unwrap().remove(*field); invalid.push(bad);
        }
        for position in [Value::Null,json!(1.0),json!(-1),json!("1"),json!(true),json!([])] {
            let mut bad=cursor.clone();bad["beforeSeq"]=position;invalid.push(bad);
        }
        for key in ["actor","lane","unknown"] {
            let mut bad=cursor.clone();bad[key]=json!("spoof");invalid.push(bad);
            let mut bad=cursor.clone();bad["scope"][key]=json!({});invalid.push(bad);
        }
        p.fail_reads.store(true,Ordering::SeqCst);
        p.fail_revision.store(true,Ordering::SeqCst);
        for cursor in invalid {
            assert_eq!(call(&s,frame(cursor)).await.unwrap_err().code,"invalid_request");
        }
        p.fail_reads.store(false,Ordering::SeqCst);
        p.fail_revision.store(false,Ordering::SeqCst);
        for key in ["requestId","expected","runId","actor","lane","__rpi","limit"] {
            let mut bad=frame(Value::Null);bad[key]=json!("spoof");
            assert_eq!(protocol::decode_rpc(&bad,true).unwrap_err().code,"invalid_request");
        }
        let scope=serde_json::to_string(&scope).unwrap();
        for raw in [
            r#"{"type":"get_durable_runs","version":1,"cursor":null,"cursor":null}"#.into(),
            format!(r#"{{"type":"get_durable_runs","version":1,"cursor":{{"scope":{scope},"beforeSeq":1,"\u0062eforeSeq":2}}}}"#),
            format!(r#"{{"type":"get_durable_runs","version":1,"cursor":{{"scope":{{"lane":"main",{}}},"beforeSeq":1}}}}"#,&scope[1..scope.len()-1]),
        ] {
            assert_eq!(protocol::parse_rpc(&raw,true).unwrap_err().code,"invalid_request");
        }
        assert_eq!(protocol::decode_rpc(&frame(Value::Null),false).unwrap_err().code,"disabled");
        assert_eq!(call(&s,json!({"type":"get_durable_runs","version":1})).await.unwrap(),
                   call(&s,frame(Value::Null)).await.unwrap());
        assert_eq!(p.attempts.load(Ordering::SeqCst),0);
        assert_eq!(control_count(&h).await,0);
        assert_eq!(s.snapshot().await.unwrap().scope.history_revision,0);
        // This is RPC only: legacy flat tool/slash action grammar is not expanded.
        assert_eq!(protocol::flat_request(&s,json!({"action":"get_durable_runs"}),None).await.unwrap_err().code,"invalid_request");
    }).await;
}

async fn filtered_roundtrip(h: &AgentHarness) {
    closed(h, "wire-old-run", true).await;
    for i in 0..65 {
        closed(h, &format!("wire-filter-{i}"), false).await;
    }
    let s = owner(h);
    let before = s.snapshot().await.unwrap();
    let records = h
        .session()
        .storage()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    let mut cursor = Value::Null;
    let mut pages = Vec::new();
    for _ in 0..4 {
        let dto = if cursor.is_null() {
            None
        } else {
            Some(serde_json::from_value(cursor.clone()).unwrap())
        };
        let explicit = s.execution_page(dto.as_ref()).await.unwrap();
        let result = call(&s, frame(cursor.clone())).await.unwrap();
        assert_eq!(result, json!({"version":1,"page":explicit}));
        assert!(!result.to_string().contains("private"));
        cursor = result["page"]["next"].clone();
        pages.push(result["page"].clone());
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(pages.len(), 3);
    assert!(pages[0]["runs"].as_array().unwrap().is_empty() && !pages[0]["next"].is_null());
    assert!(pages[1]["runs"].as_array().unwrap().is_empty() && !pages[1]["next"].is_null());
    assert_eq!(pages[2]["runs"][0]["id"], "wire-old-run");
    assert_eq!(
        pages
            .iter()
            .map(|p| p["scannedRecords"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [64, 64, 4]
    );
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(
        h.session()
            .storage()
            .find_records(&RecordQuery::default())
            .await
            .unwrap(),
        records
    );
    assert_eq!(control_count(h).await, 0);
    let cursor = pages[0]["next"].clone();
    call(&s, mutation(&s, "wire-page-start", start()).await)
        .await
        .unwrap();
    assert_eq!(call(&s, frame(cursor)).await.unwrap_err().code, "conflict");
    assert_eq!(control_count(h).await, 1);
}

#[tokio::test]
async fn memory_and_jsonl_pages_match_core_and_explicitly_continue_empty_windows() {
    bounded(async {
        filtered_roundtrip(&memory().await).await;
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let store = JsonlSessionStorage::create(
            fs,
            "wire-pages.jsonl",
            JsonlV4Header::new("wire-page-session".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let h = harness(Arc::new(store)).await;
        filtered_roundtrip(&h).await;
        owner(&h).drain().await.unwrap();
        let before = std::fs::read(dir.path().join("wire-pages.jsonl")).unwrap();
        call(&owner(&h), frame(Value::Null)).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("wire-pages.jsonl")).unwrap(),
            before
        );
    })
    .await;
}

#[tokio::test]
async fn cursor_scope_conflicts_and_position_failures_are_sanitized_without_receipts() {
    bounded(async {
        let h = memory().await;
        closed(&h, "wire-position", true).await;
        let s = owner(&h);
        let before = s.snapshot().await.unwrap();
        let cursor = json!({"scope":before.scope,"beforeSeq":1});
        for key in protocol::SCOPE_FIELDS {
            let mut bad = cursor.clone();
            bad["scope"][*key] = match *key {
                "historyRevision" | "historyGeneration" | "controlRevision" => json!(999),
                _ => json!("private-forged"),
            };
            let err = call(&s, frame(bad)).await.unwrap_err();
            assert_eq!(err.code, "conflict");
            assert!(err.request_id.is_none());
            assert!(!err.message.contains("private"));
        }
        for position in [0, before.scope.history_revision + 1] {
            let mut bad = cursor.clone();
            bad["beforeSeq"] = json!(position);
            assert_eq!(
                call(&s, frame(bad)).await.unwrap_err().code,
                "invalid_request"
            );
        }
        assert_eq!(
            call(&s, frame(cursor)).await.unwrap()["page"]["runs"],
            json!([])
        );
        assert_eq!(s.snapshot().await.unwrap(), before);
        assert_eq!(control_count(&h).await, 0);
    })
    .await;
}

#[tokio::test]
async fn page_read_failure_and_cancelled_captured_rpc_do_not_leave_a_worker_or_write() {
    bounded(async {
        let (h, p) = probed().await;
        closed(&h, "wire-cancel", true).await;
        let s = owner(&h);
        let before = s.snapshot().await.unwrap();
        p.fail_pages.store(true, Ordering::SeqCst);
        let err = call(&s, frame(Value::Null)).await.unwrap_err();
        assert_eq!(err.code, "read_failure");
        assert_eq!(err.message, "Durable observation failed");
        assert!(err.request_id.is_none());
        p.fail_pages.store(false, Ordering::SeqCst);
        let normal = call(&s, frame(Value::Null)).await.unwrap();
        p.pause_execution_pages.store(true, Ordering::SeqCst);
        let reader = {
            let s = s.clone();
            tokio::spawn(async move { call(&s, frame(Value::Null)).await })
        };
        p.execution_page_captured.notified().await;
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        p.pause_execution_pages.store(false, Ordering::SeqCst);
        assert_eq!(call(&s, frame(Value::Null)).await.unwrap(), normal);
        assert_eq!(s.snapshot().await.unwrap(), before);
        assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(control_count(&h).await, 0);
        s.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn oversized_actual_page_fails_atomically_as_read_limit_not_commit_uncertain() {
    bounded(async {
        // The memory constructor accepts explicit synthetic metadata. Oversize
        // the *scope*, without relaxing the execution domain's 512-scalar IDs.
        // This tests a real service result, not normal-session scale behavior.
        let mut metadata = AgentHarnessOptions::default().session.storage().metadata();
        metadata.id = "x".repeat(protocol::MAX_RESPONSE_BYTES);
        let storage = rpi_harness::session::InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        let h = harness(Arc::new(storage)).await;
        let s = owner(&h);
        let before = s.snapshot().await.unwrap();
        let raw = s.execution_page(None).await.unwrap();
        assert!(raw.runs.is_empty());
        assert!(raw.next.is_none());
        assert!(!protocol::fits(
            &json!({"version":1,"page":raw}),
            protocol::MAX_RESPONSE_BYTES
        ));
        let err = call(&s, frame(Value::Null)).await.unwrap_err();
        assert_eq!(err.code, "limit_exceeded");
        assert!(err.request_id.is_none());
        assert_eq!(s.snapshot().await.unwrap(), before);
        assert_eq!(control_count(&h).await, 0);
    })
    .await;
}
