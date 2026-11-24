use super::*;
use crate::host::{
    AgentId, AgentMode, AgentSpec, AgentState, ExecutionMode, HostConfig, LaunchSpec, ModelRef,
    SessionRef, TimeoutOverrides,
};
use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::{
    provider::Provider,
    providers::faux::{FauxProvider, FauxScript, FauxStep},
    types::Tool,
    Schema, UserMessage,
};
use rpi_harness::{
    agent_harness::HarnessRunOutcome,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    },
    types::HarnessTool,
};
use rpi_tools::FileSystem;
use std::{num::NonZeroUsize, path::Path, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(15);

#[cfg(unix)]
#[tokio::test]
async fn immutable_native_profile_prepares_distinct_owned_environments_without_sessions() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let startup = serde_json::from_value::<crate::host::AgentProfileFile>(serde_json::json!({
        "agent": {
            "id": AgentId::new().to_string(),
            "name": "reusable-profile",
            "cwd": cwd,
            "session": {"id": "first", "path": cwd.join("first.jsonl")},
            "model": {"provider": "fixture", "model": "host-fixture"},
            "tools": []
        },
        "runtime": {"providers": {"fixture": {
            "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
            "apiKey": "unused-preflight-key",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]
        }}},
        "toolEnvironment": {"HOME": cwd, "PATH": "/usr/bin:/bin"},
        "shell": "/bin/sh"
    }))
    .unwrap()
    .into_startup()
    .unwrap();
    let mut second_spec = startup.spec.clone();
    second_spec.id = AgentId::new();
    second_spec.session = Some(SessionRef {
        id: "second".into(),
        path: cwd.join("second.jsonl"),
    });
    let (first, second) = tokio::join!(
        startup.profile.bind(startup.spec.clone()),
        startup.profile.bind(second_spec)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    let first_env = first.environment.as_ref().unwrap();
    let second_env = second.environment.as_ref().unwrap();
    assert!(!Arc::ptr_eq(first_env, second_env));
    // The profile retained neither environment; each binding owns its fresh
    // cleanup scope, rather than two wrappers around one cached environment.
    assert_eq!(Arc::strong_count(first_env), 1);
    assert_eq!(Arc::strong_count(second_env), 1);
    drop(first);
    assert_eq!(Arc::strong_count(second_env), 1);
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);
}

struct Fixture {
    root: tempfile::TempDir,
    catalog: HostCatalog,
}
impl Fixture {
    async fn new(max_agents: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = HostCatalog::open(
            root.path().join("host"),
            HostConfig {
                max_agents: NonZeroUsize::new(max_agents),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        Self { root, catalog }
    }
    async fn spec(&self, name: &str, provider: &FauxProvider, with_tool: bool) -> AgentSpec {
        let cwd = self.root.path().join(name);
        tokio::fs::create_dir(&cwd).await.unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let path = cwd.join("exact.jsonl");
        let storage = JsonlSessionStorage::create(
            Arc::new(OsExecutionEnv::with_cwd(cwd.clone())),
            path.to_str().unwrap(),
            JsonlV4Header::new(
                name.into(),
                1,
                cwd.to_string_lossy().into_owned(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        storage.drain().await;
        storage.release_writer_lease();
        AgentSpec {
            id: AgentId::new(),
            name: name.into(),
            parent_id: None,
            mode: AgentMode::Persistent,
            execution: ExecutionMode::InProcess,
            session: Some(SessionRef {
                id: name.into(),
                path,
            }),
            launch: LaunchSpec {
                cwd,
                model: Some(ModelRef {
                    provider: provider.id().into(),
                    model: provider.default_model().id.clone(),
                }),
                tools: if with_tool {
                    vec!["gate".into()]
                } else {
                    vec![]
                },
                resource_profile: None,
            },
            timeouts: TimeoutOverrides::default(),
        }
    }
    async fn record(&self, name: &str, provider: &FauxProvider, with_tool: bool) -> AgentRecord {
        self.catalog
            .register(self.spec(name, provider, with_tool).await)
            .await
            .unwrap()
    }
    async fn restore(&self, record: &AgentRecord, binder: OrdinaryBinder) -> OrdinaryAgent {
        OrdinaryAgent::restore(
            self.catalog.clone(),
            record.spec.id,
            self.catalog.guard(record),
            binder,
        )
        .await
        .unwrap()
    }
}

struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
}
struct GateTool {
    schema: Tool,
    gate: Arc<Gate>,
    env: Arc<OsExecutionEnv>,
}
#[async_trait]
impl AgentTool for GateTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "host test gate"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.gate.entered.add_permits(1);
        tokio::select! {
            _ = signal.cancelled() => Err(AgentError::Tool("cancelled".into())),
            permit = self.gate.release.acquire() => {
                permit.unwrap().forget();
                Ok(AgentToolResult::text(self.env.cwd().to_string_lossy()))
            }
        }
    }
}
fn binder(
    provider: Arc<FauxProvider>,
    gate: Option<Arc<Gate>>,
    after: Option<rpi_agent::AfterToolCall>,
) -> OrdinaryBinder {
    Box::new(move |spec, env, facts| {
        assert_eq!(facts.metadata.id, spec.session.as_ref().unwrap().id);
        assert_eq!(env.cwd(), spec.launch.cwd);
        Ok(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            tools: gate
                .map(|gate| {
                    vec![HarnessTool::new(Arc::new(GateTool {
                        schema: Tool {
                            name: "gate".into(),
                            description: "test only".into(),
                            parameters: Schema::new(
                                serde_json::json!({"type":"object","properties":{}}),
                            ),
                            constrained_sampling: None,
                        },
                        gate,
                        env,
                    }))]
                })
                .unwrap_or_default(),
            after_tool_call: after,
            ..Default::default()
        })
    })
}
fn prompt(text: &str) -> OrdinaryOperation {
    OrdinaryOperation::Prompt(vec![UserMessage::new(text, 1).into()])
}
async fn permits(semaphore: &Semaphore, count: u32) {
    tokio::time::timeout(DEADLINE, semaphore.acquire_many(count))
        .await
        .unwrap()
        .unwrap()
        .forget();
}
async fn complete(ticket: &RunTicket) -> Arc<BackendReport> {
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(report.catalog_error.is_none(), "{report:?}");
    assert!(report.write_failures.is_empty(), "{report:?}");
    assert_eq!(report.unsaved_messages, 0);
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result))
        if matches!(result.outcome, HarnessRunOutcome::Completed { .. })),
        "{report:?}"
    );
    report
}
async fn wait_state(catalog: &HostCatalog, id: AgentId, expected: AgentState) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if catalog.get(id).await.unwrap().state == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn bytes(record: &AgentRecord) -> Vec<u8> {
    std::fs::read(&record.spec.session.as_ref().unwrap().path).unwrap()
}
fn tool_results(record: &AgentRecord) -> Vec<String> {
    use rpi_harness::session::{
        jsonl::parse_mutation,
        types::{Entry, SessionMutation},
    };
    String::from_utf8(bytes(record))
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|line| match parse_mutation(line).unwrap() {
            SessionMutation::Entry {
                entry: Entry::Message(entry),
                ..
            } => match entry.message {
                AgentMessage::ToolResult(message) => Some(
                    message
                        .content
                        .into_iter()
                        .filter_map(|content| match content {
                            rpi_ai::types::Content::Text(text) => Some(text.text),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn real_tools_overlap_on_one_worker_and_detached_clients_do_not_cancel() {
    let fixture = Fixture::new(2).await;
    let before_cwd = std::env::current_dir().unwrap();
    let provider_a = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("done-a"),
    );
    let provider_b = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("done-b"),
    );
    let a = fixture.record("alpha", &provider_a, true).await;
    let b = fixture.record("beta", &provider_b, true).await;
    let gate = Gate::new();
    let owner_a = fixture
        .restore(&a, binder(provider_a, Some(gate.clone()), None))
        .await;
    let owner_b = fixture
        .restore(&b, binder(provider_b, Some(gate.clone()), None))
        .await;
    let client_a = owner_a.client();
    let client_b = owner_b.client();
    let (run_a, run_b) = tokio::join!(
        client_a.submit(prompt("only-alpha")),
        client_b.submit(prompt("only-beta"))
    );
    let run_a = run_a.unwrap();
    let run_b = run_b.unwrap();
    permits(&gate.entered, 2).await; // Serial scheduling cannot pass this barrier.
    assert_eq!(fixture.catalog.status().await.unwrap().executing, 2);
    assert!(matches!(
        client_a.submit(prompt("busy")).await,
        Err(BackendError::Busy)
    ));
    let observer_a = run_a.clone();
    drop(run_a);
    drop(client_a);
    drop(client_b);
    gate.release.add_permits(2);
    complete(&observer_a).await;
    complete(&run_b).await;
    let text_a = String::from_utf8(bytes(&a)).unwrap();
    let text_b = String::from_utf8(bytes(&b)).unwrap();
    assert!(text_a.contains("only-alpha") && !text_a.contains("only-beta"));
    assert!(text_b.contains("only-beta") && !text_b.contains("only-alpha"));
    assert_eq!(
        tool_results(&a),
        vec![a.spec.launch.cwd.to_string_lossy().into_owned()]
    );
    assert_eq!(
        tool_results(&b),
        vec![b.spec.launch.cwd.to_string_lossy().into_owned()]
    );
    assert_eq!(std::env::current_dir().unwrap(), before_cwd);
    owner_a.close().await.unwrap();
    owner_b.close().await.unwrap();
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn steering_followups_and_late_cancel_preserve_multi_turn_execution() {
    let fixture = Fixture::new(1).await;
    let script = FauxScript::new()
        .with_tool_call("gate", serde_json::json!({}))
        .with_text("first-answer")
        .with_text("follow-answer");
    let provider = FauxProvider::new(script.clone());
    let record = fixture.record("multi", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider, Some(gate.clone()), None))
        .await;
    let client = owner.client();
    let first = client.submit(prompt("first-input")).await.unwrap();
    permits(&gate.entered, 1).await;
    client
        .deliver(
            UserMessage::new("steering-input", 1).into(),
            Delivery::Steer,
        )
        .await
        .unwrap();
    client
        .deliver(
            UserMessage::new("follow-input", 1).into(),
            Delivery::FollowUp,
        )
        .await
        .unwrap();
    gate.release.add_permits(1);
    complete(&first).await;
    script.set_responses(vec![
        FauxStep::tool_call("gate", serde_json::json!({})),
        FauxStep::text("second-answer"),
    ]);
    let second = client.submit(prompt("second-input")).await.unwrap();
    permits(&gate.entered, 1).await;
    assert!(!client.cancel(first.id).await.unwrap());
    gate.release.add_permits(1);
    complete(&second).await;
    let text = String::from_utf8(bytes(&record)).unwrap();
    for needle in [
        "first-input",
        "steering-input",
        "follow-input",
        "second-input",
        "second-answer",
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_close_waiter_cannot_release_slot_or_writer_before_late_callback() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("unused"),
    );
    let record = fixture.record("late", &provider, true).await;
    let gate = Gate::new();
    gate.release.add_permits(1);
    let late = Gate::new();
    let hook_gate = late.clone();
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let gate = hook_gate.clone();
        Box::pin(async move {
            gate.entered.add_permits(1);
            gate.release.acquire().await.unwrap().forget();
            None
        })
    });
    let owner = fixture
        .restore(&record, binder(provider.clone(), Some(gate), Some(hook)))
        .await;
    let client = owner.client();
    let ticket = client.submit(prompt("held-callback")).await.unwrap();
    permits(&late.entered, 1).await;
    assert!(client.cancel(ticket.id).await.unwrap());
    let close_waiter = tokio::spawn(async move { owner.close().await });
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closing).await;
    close_waiter.abort();
    let _ = close_waiter.await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    let path = record.spec.session.as_ref().unwrap().path.to_str().unwrap();
    assert!(env.try_acquire_writer_lease(path).await.is_err());
    let other = fixture.record("no-slot", &provider, false).await;
    assert!(matches!(
        OrdinaryAgent::restore(
            fixture.catalog.clone(),
            other.spec.id,
            fixture.catalog.guard(&other),
            binder(provider, None, None)
        )
        .await,
        Err(BackendError::Catalog(_))
    ));
    assert!(!ticket.completion.borrow().is_some());
    late.release.add_permits(1);
    wait_state(&fixture.catalog, record.spec.id, AgentState::Closed).await;
    let report = ticket.wait().await.unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    assert!(env.try_acquire_writer_lease(path).await.unwrap().is_some());
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_reopens_exact_reference_not_newer_session() {
    let fixture = Fixture::new(1).await;
    let first_provider = FauxProvider::new(FauxScript::new().with_text("first-output"));
    let record = fixture.record("restore", &first_provider, false).await;
    let first = fixture
        .restore(&record, binder(first_provider.clone(), None, None))
        .await;
    complete(&first.client().submit(prompt("saved-input")).await.unwrap()).await;
    first.close().await.unwrap();
    let extra = record.spec.launch.cwd.join("newer.jsonl");
    let storage = JsonlSessionStorage::create(
        Arc::new(OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone())),
        extra.to_str().unwrap(),
        JsonlV4Header::new(
            "newer".into(),
            2,
            record.spec.launch.cwd.to_string_lossy().into_owned(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    storage.drain().await;
    storage.release_writer_lease();
    let newer_before = std::fs::read(&extra).unwrap();
    let epoch = fixture.catalog.epoch();
    fixture.catalog.shutdown().await.unwrap();
    let next = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_ne!(epoch, next.epoch());
    let current = next.get(record.spec.id).await.unwrap();
    let second_provider = FauxProvider::new(FauxScript::new().with_text("resumed-output"));
    let owner = OrdinaryAgent::restore(
        next.clone(),
        current.spec.id,
        next.guard(&current),
        binder(second_provider, None, None),
    )
    .await
    .unwrap();
    let report = owner
        .client()
        .submit(OrdinaryOperation::ResumePending)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(matches!(
        report.outcome,
        Ok(OrdinaryOutput::NoPendingResume)
    ));
    complete(
        &owner
            .client()
            .submit(prompt("resumed-input"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes(&record)).unwrap();
    assert!(text.contains("saved-input") && text.contains("resumed-input"));
    assert_eq!(std::fs::read(&extra).unwrap(), newer_before);
    owner.close().await.unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_writer_rejected_without_repair_and_reopens_after_clean_close() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("duplicate", &provider, false).await;
    let first = fixture
        .restore(&record, binder(provider.clone(), None, None))
        .await;
    let before = bytes(&record);
    let other = HostCatalog::open(
        fixture.root.path().join("other-host"),
        HostConfig::default(),
    )
    .await
    .unwrap();
    let duplicate = other.register(record.spec.clone()).await.unwrap();
    assert!(matches!(
        OrdinaryAgent::restore(
            other.clone(),
            duplicate.spec.id,
            other.guard(&duplicate),
            binder(provider.clone(), None, None)
        )
        .await,
        Err(BackendError::Session(_))
    ));
    assert_eq!(bytes(&record), before);
    assert_eq!(other.status().await.unwrap().resident, 0);
    first.close().await.unwrap();
    let current = other.get(duplicate.spec.id).await.unwrap();
    let reopened = OrdinaryAgent::restore(
        other.clone(),
        current.spec.id,
        other.guard(&current),
        binder(provider, None, None),
    )
    .await
    .unwrap();
    reopened.close().await.unwrap();
    other.shutdown().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_identity_or_bindings_do_not_repair_torn_source_or_take_writer() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    for (name, wrong_id) in [("wrong-id", true), ("rejected-binding", false)] {
        let mut spec = fixture.spec(name, &provider, false).await;
        if wrong_id {
            spec.session.as_mut().unwrap().id = "not-the-header".into();
        }
        let path = spec.session.as_ref().unwrap().path.clone();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"kind\":")
            .unwrap();
        let record = fixture.catalog.register(spec).await.unwrap();
        let before = bytes(&record);
        let result = OrdinaryAgent::restore(
            fixture.catalog.clone(),
            record.spec.id,
            fixture.catalog.guard(&record),
            Box::new(|_, _, _| Err(BackendError::Binding("resource preflight refused"))),
        )
        .await;
        assert!(matches!(result, Err(BackendError::Binding(_))));
        assert_eq!(bytes(&record), before);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
        let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
        assert!(env
            .try_acquire_writer_lease(path.to_str().unwrap())
            .await
            .unwrap()
            .is_some());
    }
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn input_budget_rejects_before_catalog_or_session_write() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("ok"));
    let record = fixture.record("budget", &provider, false).await;
    let owner = OrdinaryAgent::restore_with_limits(
        fixture.catalog.clone(),
        record.spec.id,
        fixture.catalog.guard(&record),
        OrdinaryLimits {
            max_input_bytes: 256,
        },
        binder(provider, None, None),
    )
    .await
    .unwrap();
    let before = bytes(&record);
    let prior = fixture.catalog.get(record.spec.id).await.unwrap();
    assert!(matches!(
        owner.client().submit(prompt(&"x".repeat(257))).await,
        Err(BackendError::InputTooLarge)
    ));
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().revision,
        prior.revision
    );
    assert_eq!(bytes(&record), before);
    complete(&owner.client().submit(prompt("fits")).await.unwrap()).await;
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

fn refuse_closed(root: &Path) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(root.join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_closed BEFORE UPDATE ON agents WHEN NEW.resident = 0 BEGIN SELECT RAISE(ABORT, 'synthetic close failure'); END;").unwrap();
    db
}
#[tokio::test]
async fn failed_closed_commit_retains_reservation_and_retry_does_not_reclose_harness() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture.record("close-fault", &provider, false).await;
    let owner = fixture.restore(&record, binder(provider, None, None)).await;
    let db = refuse_closed(fixture.root.path());
    assert!(matches!(owner.close().await, Err(BackendError::Catalog(_))));
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closing
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(matches!(
        owner.client().submit(prompt("must-not-run")).await,
        Err(BackendError::Closed)
    ));
    // Actual writer cleanup precedes even the failed Closed publication.
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
    db.execute_batch("DROP TRIGGER refuse_closed").unwrap();
    owner.close().await.unwrap();
    owner.close().await.unwrap();
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    fixture.catalog.shutdown().await.unwrap();
}

fn deferred_script(
    provider: &FauxProvider,
    id: &str,
) -> (FauxScript, rpi_ai::types::DeferredHandle) {
    use rpi_ai::{
        providers::faux::faux_assistant_message,
        types::{DeferredHandle, StopReason},
    };
    let handle = DeferredHandle {
        provider: provider.id().into(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().into(),
        id: id.into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    let mut message = faux_assistant_message("parked", StopReason::Deferred);
    message.deferred = Some(handle.clone());
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::Message(message)]);
    (script, handle)
}

#[tokio::test]
async fn deferred_model_wait_is_busy_and_explicit_polls_keep_native_run_identity() {
    use rpi_ai::{
        providers::faux::{faux_assistant_message, DeferredPoll},
        types::StopReason,
    };
    let fixture = Fixture::new(1).await;
    let model_source = FauxProvider::new(FauxScript::new());
    let (script, handle) = deferred_script(&model_source, "park-one");
    let provider = FauxProvider::new(script);
    let record = fixture.record("deferred", &provider, false).await;
    let owner = fixture
        .restore(&record, binder(provider.clone(), None, None))
        .await;
    let client = owner.client();
    let initial = tokio::time::timeout(
        DEADLINE,
        client
            .submit(prompt("wait-on-provider"))
            .await
            .unwrap()
            .wait(),
    )
    .await
    .unwrap()
    .unwrap();
    let native_id = match &initial.outcome {
        Ok(OrdinaryOutput::Run(result))
            if matches!(result.outcome, HarnessRunOutcome::Suspended { .. }) =>
        {
            result.run_id.clone()
        }
        _ => panic!("{initial:?}"),
    };
    assert_eq!(
        client.snapshot().await.unwrap().deferred.as_deref(),
        Some(handle.id.as_str())
    );
    assert_eq!(fixture.catalog.status().await.unwrap().busy, 1);
    assert!(matches!(
        client
            .submit(prompt("cannot-overwrite-pending-model"))
            .await,
        Err(BackendError::Busy)
    ));
    let wrong = client
        .submit(OrdinaryOperation::ResumeDeferred("obsolete".into()))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(wrong.outcome.is_err());
    assert_eq!(fixture.catalog.status().await.unwrap().busy, 1);
    let next = rpi_ai::types::DeferredHandle {
        id: "park-two".into(),
        ..handle.clone()
    };
    provider.push_deferred_poll(DeferredPoll::StillDeferred(next.clone()));
    let parked = client
        .submit(OrdinaryOperation::ResumeDeferred(handle.id))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(
        matches!(parked.outcome, Ok(OrdinaryOutput::Run(ref result)) if result.run_id == native_id && matches!(result.outcome, HarnessRunOutcome::Suspended { .. }))
    );
    assert_eq!(fixture.catalog.status().await.unwrap().busy, 1);
    provider.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "deferred-finished",
        StopReason::Stop,
    )));
    let terminal = complete(
        &client
            .submit(OrdinaryOperation::ResumeDeferred(next.id))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        matches!(terminal.outcome, Ok(OrdinaryOutput::Run(ref result)) if result.run_id == native_id)
    );
    assert_eq!(fixture.catalog.status().await.unwrap().busy, 0);
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn restoring_parked_provider_publishes_busy_without_implicitly_polling() {
    use rpi_ai::{
        providers::faux::{faux_assistant_message, DeferredPoll},
        types::StopReason,
    };
    let fixture = Fixture::new(1).await;
    let model_source = FauxProvider::new(FauxScript::new());
    let (script, handle) = deferred_script(&model_source, "saved-park");
    let provider = FauxProvider::new(script);
    let record = fixture.record("saved-deferred", &provider, false).await;
    let first = fixture
        .restore(&record, binder(provider.clone(), None, None))
        .await;
    let parked = first
        .client()
        .submit(prompt("save-provider-handle"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(
        matches!(parked.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Suspended { .. }))
    );
    first.close().await.unwrap();
    provider.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "only-after-explicit-poll",
        StopReason::Stop,
    )));
    let current = fixture.catalog.get(record.spec.id).await.unwrap();
    let restored = fixture
        .restore(&current, binder(provider, None, None))
        .await;
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Running
    );
    assert_eq!(
        restored
            .client()
            .snapshot()
            .await
            .unwrap()
            .deferred
            .as_deref(),
        Some(handle.id.as_str())
    );
    assert!(!String::from_utf8(bytes(&record))
        .unwrap()
        .contains("only-after-explicit-poll"));
    complete(
        &restored
            .client()
            .submit(OrdinaryOperation::ResumeDeferred(handle.id))
            .await
            .unwrap(),
    )
    .await;
    restored.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_commit_failure_still_cancels_and_settles_but_retains_writer_and_slot() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("unused"),
    );
    let record = fixture.record("closing-fault", &provider, true).await;
    let gate = Gate::new();
    let owner = fixture
        .restore(&record, binder(provider, Some(gate.clone()), None))
        .await;
    let ticket = owner
        .client()
        .submit(prompt("cancel-despite-db-failure"))
        .await
        .unwrap();
    permits(&gate.entered, 1).await;
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_closing BEFORE UPDATE ON agents WHEN NEW.resident = 1 AND NEW.busy = 1 AND NEW.executing = 0 BEGIN SELECT RAISE(ABORT, 'synthetic closing failure'); END;").unwrap();
    assert!(matches!(
        tokio::time::timeout(DEADLINE, owner.close()).await.unwrap(),
        Err(BackendError::Catalog(_))
    ));
    let report = ticket.wait().await.unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome, HarnessRunOutcome::Aborted { .. })),
        "{report:?}"
    );
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(owner.client().snapshot().await.unwrap().active.is_none());
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
    db.execute_batch("DROP TRIGGER refuse_closing").unwrap();
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_writer_fence_reports_uncertain_persistence_and_refuses_to_publish_closed() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("must-not-be-saved"),
    );
    let record = fixture.record("write-fault", &provider, true).await;
    let gate = Gate::new();
    gate.release.add_permits(1);
    let lock = record
        .spec
        .session
        .as_ref()
        .unwrap()
        .path
        .with_extension("jsonl.lock");
    let hook: rpi_agent::AfterToolCall = Arc::new(move |_, _| {
        let lock = lock.clone();
        Box::pin(async move {
            tokio::fs::write(lock, b"0\n").await.unwrap();
            None
        })
    });
    let owner = fixture
        .restore(&record, binder(provider, Some(gate), Some(hook)))
        .await;
    let report = tokio::time::timeout(
        DEADLINE,
        owner
            .client()
            .submit(prompt("preserve-recovery-copy"))
            .await
            .unwrap()
            .wait(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(report.persistence_uncertain, "{report:?}");
    assert!(report.outcome.is_err(), "{report:?}");
    assert!(matches!(
        owner
            .client()
            .submit(prompt("cannot-continue-unsaved"))
            .await,
        Err(BackendError::Faulted)
    ));
    assert!(matches!(owner.close().await, Err(BackendError::Unsaved)));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closing
    );
    let exported = owner.client().unsaved_messages().unwrap();
    // Fallible native writes can fail before Harness makes a recovery copy.
    // No copy/warning is NOT a saved-data acknowledgement; the native journal
    // still describes the interrupted/unknown tool outcome.
    assert!(exported
        .iter()
        .all(|message| message.session_id == record.spec.session.as_ref().unwrap().id));
    // Deliberately discard this faulted test owner only after observing its
    // uncertain outcome. Production must retain it for explicit reconciliation.
    // Drop is not a successful unload: the persisted row remains interrupted.
    let client = owner.client();
    drop(owner);
    tokio::time::timeout(DEADLINE, async {
        while client.inner.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.catalog.shutdown().await.unwrap();
    let recovered = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    assert_eq!(
        recovered.get(record.spec.id).await.unwrap().state,
        AgentState::Interrupted
    );
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_model_tools_provider_manual_mode_and_panicking_binders_fail_before_commit() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    for case in 0..5 {
        let record = fixture
            .record(&format!("bad-binding-{case}"), &provider, false)
            .await;
        let before = bytes(&record);
        let provider = provider.clone();
        let rejected = OrdinaryAgent::restore(
            fixture.catalog.clone(),
            record.spec.id,
            fixture.catalog.guard(&record),
            Box::new(move |_, _, _| {
                if case == 4 {
                    panic!("synthetic preflight panic");
                }
                let mut options = AgentHarnessOptions {
                    model: provider.default_model().clone(),
                    models: vec![provider],
                    ..Default::default()
                };
                match case {
                    0 => options.model.id = "unregistered-model".into(),
                    1 => options.active_tool_names = vec!["unregistered-tool".into()],
                    2 => options.models.clear(),
                    3 => options.drive = rpi_harness::types::DrivingMode::Manual,
                    _ => unreachable!(),
                }
                Ok(options)
            }),
        )
        .await;
        assert!(matches!(
            rejected,
            Err(BackendError::Binding(_)) | Err(BackendError::TaskStopped)
        ));
        assert_eq!(bytes(&record), before);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    }
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandoned_startup_waiter_still_cleans_up_owner_writer_and_reservation() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture.record("abandoned-start", &provider, false).await;
    let catalog = fixture.catalog.clone();
    let guard = catalog.guard(&record);
    let id = record.spec.id;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let request = tokio::spawn(OrdinaryAgent::restore(
        catalog,
        id,
        guard,
        Box::new(move |_, _, _| {
            let _ = entered.send(());
            Ok(AgentHarnessOptions {
                model: provider.default_model().clone(),
                models: vec![provider],
                ..Default::default()
            })
        }),
    ));
    ready.await.unwrap();
    request.abort();
    drop(request.await);
    wait_state(&fixture.catalog, id, AgentState::Closed).await;
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .unwrap()
        .is_some());
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_compaction_uses_the_same_supervisor_and_settles_before_idle() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new());
    let record = fixture.record("compact", &provider, false).await;
    let owner = fixture.restore(&record, binder(provider, None, None)).await;
    // An empty history may legitimately skip compaction; either way this is the
    // real Harness compaction path, not a synthetic Host state transition.
    let ticket = owner
        .client()
        .submit(OrdinaryOperation::Compact(None))
        .await
        .unwrap();
    let report = tokio::time::timeout(DEADLINE, ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome, Ok(OrdinaryOutput::Compaction(_))),
        "{report:?}"
    );
    assert!(!report.persistence_uncertain);
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Idle
    );
    assert!(owner.client().snapshot().await.unwrap().active.is_none());
    owner.close().await.unwrap();
    fixture.catalog.shutdown().await.unwrap();
}

mod administrative_guards {
    use super::*;
    include!("guarded_tests.rs");
    include!("submitted_cancel_tests.rs");
}

mod owner_registry {
    use super::*;
    include!("service_tests.rs");

    mod deferred_binding {
        include!("binding_plan_tests.rs");
    }

    mod extension_custody {
        include!("extension_custody_tests.rs");
    }

    mod guarded_lifecycle {
        include!("service_guarded_tests.rs");
        include!("service_reservation_tests.rs");
        include!("administration_owner_tests.rs");
        include!("submitted_administration_tests.rs");
    }

    mod admission_faults {
        include!("admission_fault_tests.rs");
        include!("instruction_resource_tests.rs");
    }
}
