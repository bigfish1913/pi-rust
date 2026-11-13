//! Explicit, isolated native RPC execution pages. No provider or tool is run.
use super::*;
use rpi_agent::AgentMessage;
use rpi_ai::types::UserMessage;
use rpi_harness::{
    agent_harness::AgentHarness,
    session::{jsonl::JsonlSessionStorage, types::*, DefaultIdGenerator, Session, SystemClock},
    types::AgentHarnessOptions,
};
use rpi_tools::OsExecutionEnv;
use std::collections::HashSet;

fn page(host: &mut Host, cursor: Value) -> Value {
    let data = host.data("get_durable_runs", json!({"version":1,"cursor":cursor}));
    assert_eq!(data["version"], 1, "{data}");
    let page = data["page"].clone();
    assert_eq!(page.as_object().unwrap().len(), 6, "{page}");
    assert!(
        !data.to_string().contains("private"),
        "payload leaked: {data}"
    );
    page
}

#[test]
fn enabled_no_tools_pages_are_empty_strict_and_read_only() {
    let fixture = Fixture::new();
    let mut command = fixture.command(
        true,
        &[
            "--package-only",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--session-id",
            "rpc-empty-pages",
            "--no-tools",
        ],
    );
    let mut host = Host::spawn(&mut command);
    let ready = host.until(|value| value["type"] == "ready");
    assert_eq!(ready["durable"]["enabled"], true);
    assert!(ready["durable"]["commands"]
        .as_array()
        .unwrap()
        .contains(&json!("get_durable_runs")));
    assert_eq!(
        ready["durable"]["executionPages"],
        json!({"version":1,"command":"get_durable_runs",
            "cursor":"scope_bound_exclusive","maxRecords":64,"automatic":false})
    );
    let before = host.state();
    let state = host.data("get_state", json!({}));
    assert!(state["activeOperation"].is_null());
    let tools = host.data("get_available_tools", json!({}));
    assert!(!has_name(&tools, "tools", "durable"));
    assert!(has_name(
        &host.data("get_commands", json!({})),
        "commands",
        "durable"
    ));
    let omitted = host.data("get_durable_runs", json!({"version":1}));
    let empty = page(&mut host, Value::Null);
    assert_eq!(omitted, json!({"version":1,"page":empty}));
    assert_eq!(empty["scope"], before["scope"]);
    assert_eq!(empty["control"], before["executionControl"]);
    assert_eq!(empty["runs"], json!([]));
    assert_eq!(empty["unavailableRuns"], 0);
    assert_eq!(empty["scannedRecords"], 0);
    assert!(empty["next"].is_null());
    let file = fixture.session_file();
    let bytes = std::fs::read(&file).unwrap();

    let cursor = json!({"scope":before["scope"],"beforeSeq":1});
    let mut invalid = Vec::new();
    for field in ["actor", "lane", "runId", "requestId", "expected", "unknown"] {
        let mut request = json!({"version":1});
        request[field] = json!("untrusted");
        invalid.push(request);
    }
    invalid.push(json!({"cursor":null}));
    for version in [Value::Null, json!(1.0), json!(2), json!("1")] {
        invalid.push(json!({"version":version}));
    }
    for partial in [
        json!({}),
        json!({"scope":before["scope"]}),
        json!({"beforeSeq":1}),
    ] {
        invalid.push(json!({"version":1,"cursor":partial}));
    }
    for field in [
        "sessionId",
        "lane",
        "bindingToken",
        "leafId",
        "historyRevision",
        "entryId",
        "controlRevision",
    ] {
        let mut missing = cursor.clone();
        missing["scope"].as_object_mut().unwrap().remove(field);
        invalid.push(json!({"version":1,"cursor":missing}));
    }
    for position in [
        Value::Null,
        json!(1.0),
        json!(-1),
        json!("1"),
        json!(true),
        json!([]),
    ] {
        let mut bad = cursor.clone();
        bad["beforeSeq"] = position;
        invalid.push(json!({"version":1,"cursor":bad}));
    }
    for nested in ["cursor", "scope"] {
        let mut bad = cursor.clone();
        if nested == "cursor" {
            bad["unknown"] = json!(true);
        } else {
            bad["scope"]["unknown"] = json!(true);
        }
        invalid.push(json!({"version":1,"cursor":bad}));
    }
    for request in invalid {
        assert_error(
            &host.request("get_durable_runs", request),
            "invalid_request",
        );
    }
    let scope = before["scope"].to_string();
    for (id, raw) in [
        (
            "dup-top",
            r#"{"type":"get_durable_runs","id":"dup-top","version":1,"\u0076ersion":1}"#.to_owned(),
        ),
        (
            "dup-cursor",
            format!(
                r#"{{"type":"get_durable_runs","id":"dup-cursor","version":1,"cursor":{{"scope":{scope},"beforeSeq":1,"\u0062eforeSeq":2}}}}"#
            ),
        ),
        (
            "dup-scope",
            format!(
                r#"{{"type":"get_durable_runs","id":"dup-scope","version":1,"cursor":{{"scope":{{"\u006cane":"main",{}}},"beforeSeq":1}}}}"#,
                &scope[1..scope.len() - 1]
            ),
        ),
    ] {
        assert_error(&host.raw(&raw, &json!(id)), "invalid_request");
    }
    // Raw whitespace counts, including Host::raw's trailing JSONL newline:
    // the CLI passes that newline to the shared parser as part of raw_line.
    let frame = json!({"type":"get_durable_runs","id":"boundary","version":1}).to_string();
    let exact = format!("{}{}", frame, " ".repeat(64 * 1024 - frame.len() - 1));
    assert_eq!(exact.len() + 1, 64 * 1024);
    let response = host.raw(&exact, &json!("boundary"));
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"], omitted);
    for raw in [
        format!("{exact} "),
        format!(" {}{}", " ".repeat(64 * 1024 - frame.len()), frame),
    ] {
        assert_error(&host.raw(&raw, &json!("boundary")), "limit_exceeded");
    }
    let oversized_id = "x".repeat(4096);
    let raw = json!({"type":"get_durable_runs","id":oversized_id,"version":1}).to_string();
    let response = host.raw(&raw, &Value::Null);
    assert_error(&response, "limit_exceeded");
    assert!(
        response["id"].is_null(),
        "oversized correlation ID echoed: {response}"
    );
    assert!(!response.to_string().contains(&oversized_id));
    assert_eq!(
        host.state(),
        before,
        "read/invalid frames changed scope or control"
    );
    assert_eq!(page(&mut host, Value::Null), empty);
    host.assert_idle();
    eof_exit(&mut host);
    assert_eq!(
        std::fs::read(file).unwrap(),
        bytes,
        "paging wrote an audit entry"
    );
    drop(host);
    fixture.assert_no_provider_calls();
}

#[test]
fn disabled_pages_are_recognized_and_validate_before_disabled() {
    let fixture = Fixture::new();
    let mut host = fixture.start(false, &["--no-session", "--no-tools"]);
    for fields in [
        json!({"version":1}),
        json!({"version":1,"cursor":null}),
        json!({"version":1,"cursor":{"scope":{"sessionId":"synthetic","lane":"main",
            "bindingToken":"synthetic","leafId":null,"historyRevision":1,
            "entryId":null,"controlRevision":0},"beforeSeq":1}}),
    ] {
        assert_error(&host.request("get_durable_runs", fields), "disabled");
    }
    assert_error(
        &host.request("get_durable_runs", json!({"version":1,"actor":"spoofed"})),
        "invalid_request",
    );
    assert!(!has_name(
        &host.data("get_available_tools", json!({})),
        "tools",
        "durable"
    ));
    assert!(!has_name(
        &host.data("get_commands", json!({})),
        "commands",
        "durable"
    ));
    host.assert_idle();
    eof_exit(&mut host);
    drop(host);
    fixture.assert_no_provider_calls();
}

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}

fn install_seventy_runs(fixture: &Fixture) -> PathBuf {
    // Let the real CLI repo create/index a test-owned session first. A hand-made
    // filename is not necessarily discoverable by the session-selection layer.
    let mut seed = fixture.start(true, &["--session-id", "rpc-page-fixture", "--no-tools"]);
    seed.state();
    eof_exit(&mut seed);
    drop(seed);
    let path = fixture.session_file();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let storage = JsonlSessionStorage::load(
                Arc::new(OsExecutionEnv::with_cwd(
                    path.parent().unwrap().to_path_buf(),
                )),
                path.file_name().unwrap().to_str().unwrap(),
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap();
            let h = AgentHarness::create(AgentHarnessOptions {
                session: Session::new(Arc::new(storage), None),
                require_native_durable: true,
                ..Default::default()
            })
            .await
            .unwrap();
            for index in 0..70 {
                let run = format!("native-paged-{index:02}");
                let prompt = format!("{run}-prompt");
                h.session()
                    .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                        base: base(&run),
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
                h.session()
                    .append_entry(
                        ProvisionedEntry {
                            id: prompt.clone(),
                            kind: ProvisionedKind::Message {
                                message: AgentMessage::User(UserMessage::new(
                                    "private page prompt payload",
                                    0,
                                )),
                                terminate: None,
                            },
                        },
                        "main",
                    )
                    .await
                    .unwrap();
                h.session()
                    .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                        base: base(&format!("{run}-finished")),
                        run_id: run,
                        outcome: OperationOutcome::Aborted,
                        error: Some(OperationError {
                            code: "interrupted".into(),
                            message: "private page error".into(),
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
            assert_eq!(
                h.session()
                    .storage()
                    .find_records(&RecordQuery::default())
                    .await
                    .unwrap()
                    .len(),
                140
            );
            // These are interrupted proof fixtures, not completed assistant
            // results: get_result legitimately requires a final entry.
            let service = rpi_harness::durable::DurableService::bind(
                &h,
                rpi_harness::durable::DurableActor::owner("fixture-owner").unwrap(),
            );
            assert_eq!(
                service
                    .execution("native-paged-69")
                    .await
                    .unwrap()
                    .run
                    .unwrap()
                    .id,
                "native-paged-69"
            );
            service.drain().await.unwrap();
            drop(service);
            h.drain_session().await;
            h.close().await.unwrap();
            drop(h);
        });
    path
}

fn traverse(host: &mut Host) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut cursor = Value::Null;
    let mut ids = HashSet::new();
    for _ in 0..4 {
        let current = page(host, cursor.clone());
        if let Some(first) = pages.first() {
            let first: &Value = first;
            assert_eq!(current["scope"], first["scope"]);
            assert_eq!(current["control"], first["control"]);
        }
        assert_eq!(current["unavailableRuns"], 0);
        for row in current["runs"].as_array().unwrap() {
            assert!(
                ids.insert(row["id"].as_str().unwrap().to_owned()),
                "duplicate row: {row}"
            );
        }
        let next = current["next"].clone();
        if !next.is_null() {
            assert_eq!(next["scope"], current["scope"]);
            assert_eq!(current["scannedRecords"], 64);
            if !cursor.is_null() {
                assert!(
                    next["beforeSeq"].as_u64().unwrap() < cursor["beforeSeq"].as_u64().unwrap()
                );
            }
        }
        pages.push(current);
        if next.is_null() {
            assert_eq!(
                pages
                    .iter()
                    .map(|p| p["scannedRecords"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                vec![64, 64, 12]
            );
            let actual: Vec<_> = pages
                .iter()
                .flat_map(|p| p["runs"].as_array().unwrap())
                .map(|r| r["id"].as_str().unwrap().to_owned())
                .collect();
            let expected: Vec<_> = (0..70)
                .rev()
                .map(|i| format!("native-paged-{i:02}"))
                .collect();
            assert_eq!(actual, expected);
            return pages;
        }
        // The exact returned object, never an inferred or reconstructed cursor.
        cursor = next;
    }
    panic!("explicit page traversal failed to terminate");
}

#[test]
fn jsonl_pages_preserve_all_runs_and_audit_and_conflict_after_mutation_and_rebind() {
    let fixture = Fixture::new();
    let file = install_seventy_runs(&fixture);
    let args = ["--session", file.to_str().unwrap(), "--no-tools"];
    let mut host = fixture.start(true, &args);
    let before = host.state();
    assert!(before["state"]["workflow"].is_null());
    assert!(before["scope"]["entryId"].is_null());
    assert_eq!(before["scope"]["controlRevision"], 0);
    // Baseline after loading: header normalization is not mistaken for paging IO.
    let bytes = std::fs::read(&file).unwrap();
    let pages = traverse(&mut host);
    let row = &pages[0]["runs"][0];
    let explicit = host.data("get_durable_state", json!({"version":1,"runId":row["id"]}));
    let run = &explicit["state"]["run"];
    for field in [
        "id",
        "startedSeq",
        "finishedRecordId",
        "finishedSeq",
        "status",
    ] {
        assert_eq!(
            row[field], run[field],
            "explicit run metadata differs: {explicit}"
        );
    }
    assert_eq!(host.state(), before);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        bytes,
        "read-only pages changed native facts"
    );
    let old = pages[0]["next"].clone();
    let outcome = host.mutate("pages-control-start", start_action());
    assert_eq!(outcome["replayed"], false);
    assert_error(
        &host.request("get_durable_runs", json!({"version":1,"cursor":old})),
        "conflict",
    );
    let fresh = traverse(&mut host);
    let rebound_cursor = fresh[0]["next"].clone();
    let saved = host.state();
    assert_eq!(saved["scope"]["controlRevision"], 1);
    let receipt = host.data(
        "get_durable_request",
        json!({"version":1,"requestId":"pages-control-start"}),
    )["receipt"]
        .clone();
    assert_eq!(receipt, outcome["receipt"]);
    let state = host.data("get_state", json!({}));
    assert!(state["activeOperation"].is_null());
    eof_exit(&mut host);
    let committed = std::fs::read(&file).unwrap();
    // Every original JSONL fact, prompt anchor and terminal result identity must
    // survive the one control receipt. No append_entry forgery or audit pruning.
    let committed_text = std::str::from_utf8(&committed).unwrap();
    for line in std::str::from_utf8(&bytes).unwrap().lines() {
        assert!(
            committed_text.lines().any(|saved| saved == line),
            "original audit line removed: {line}"
        );
    }
    assert_eq!(
        committed_text
            .lines()
            .filter(|line| line.contains("rpi.durable.control.v1"))
            .count(),
        1
    );
    drop(host);
    fixture.assert_no_provider_calls();

    let mut reopened = fixture.start(true, &args);
    let restored = reopened.state();
    assert_ne!(
        restored["scope"]["bindingToken"],
        saved["scope"]["bindingToken"]
    );
    assert_eq!(restored["state"], saved["state"]);
    assert_eq!(restored["executionControl"], saved["executionControl"]);
    assert_eq!(
        restored["scope"]["controlRevision"],
        saved["scope"]["controlRevision"]
    );
    assert_error(
        &reopened.request(
            "get_durable_runs",
            json!({"version":1,"cursor":rebound_cursor}),
        ),
        "conflict",
    );
    traverse(&mut reopened);
    assert_eq!(
        reopened.data(
            "get_durable_request",
            json!({"version":1,"requestId":"pages-control-start"})
        )["receipt"],
        receipt
    );
    assert!(reopened.data("get_state", json!({}))["activeOperation"].is_null());
    eof_exit(&mut reopened);
    assert_eq!(
        std::fs::read(file).unwrap(),
        committed,
        "read-only reopen changed native audit/result bytes"
    );
    drop(reopened);
    fixture.assert_no_provider_calls();
}

fn eof_exit(host: &mut Host) {
    host.input.take();
    let deadline = Instant::now() + WAIT;
    loop {
        match host
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(line) => {
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert!(
                    !matches!(
                        frame["type"].as_str(),
                        Some("agent_start" | "tool_execution_start")
                    ),
                    "manual paging started execution: {frame}"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!(
                "paging EOF timeout: {error}; stderr: {}",
                String::from_utf8_lossy(&host.stderr.lock().unwrap())
            ),
        }
        assert!(Instant::now() < deadline, "paging output failed to settle");
    }
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(0),
                "paging child exit: {status}; stderr: {}",
                String::from_utf8_lossy(&host.stderr.lock().unwrap())
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "paging child did not exit after EOF"
        );
        thread::yield_now();
    }
}
