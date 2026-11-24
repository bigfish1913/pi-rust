mod auxiliary;
mod cli_resources;

use super::*;
use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::{
    providers::faux::{FauxProvider, FauxScript},
    types::Tool,
    Schema, UserMessage,
};
use rpi_harness::{
    agent_harness::HarnessRunOutcome,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::SessionStorage,
        DefaultIdGenerator, Session, SystemClock,
    },
    types::{AgentHarnessOptions, HarnessTool},
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio::sync::Semaphore;

const DEADLINE: Duration = Duration::from_secs(10);
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
struct ToolGate {
    schema: Tool,
    gate: Arc<Gate>,
    env: Arc<OsExecutionEnv>,
}
#[async_trait]
impl AgentTool for ToolGate {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "in-process fixture"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> std::result::Result<AgentToolResult, AgentError> {
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
struct Fixture {
    _root: tempfile::TempDir,
    env: Arc<OsExecutionEnv>,
    storage: Arc<JsonlSessionStorage>,
    harness: AgentHarness,
}
impl Fixture {
    async fn new(
        name: &str,
        gate: Option<Arc<Gate>>,
        after: Option<rpi_agent::AfterToolCall>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let env = Arc::new(OsExecutionEnv::with_cwd(cwd.clone()));
        let storage = JsonlSessionStorage::create(
            env.clone(),
            cwd.join("session.jsonl").to_str().unwrap(),
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
        let storage = Arc::new(storage);
        let script = if gate.is_some() {
            FauxScript::new()
                .with_tool_call("gate", serde_json::json!({}))
                .with_text("done")
        } else {
            FauxScript::new().with_text("done")
        };
        let provider = FauxProvider::new(script);
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            session: Session::new(storage.clone(), None),
            tools: gate
                .map(|gate| {
                    vec![HarnessTool::new(Arc::new(ToolGate {
                        schema: Tool {
                            name: "gate".into(),
                            description: "fixture".into(),
                            parameters: Schema::new(
                                serde_json::json!({"type":"object","properties":{}}),
                            ),
                            constrained_sampling: None,
                        },
                        gate,
                        env: env.clone(),
                    }))]
                })
                .unwrap_or_default(),
            after_tool_call: after,
            ..Default::default()
        })
        .await
        .unwrap();
        Self {
            _root: root,
            env,
            storage,
            harness,
        }
    }
    fn attach(&self, runtime: &Arc<HarnessRuntime>, id: AgentId) -> HarnessClient {
        runtime
            .attach(id, self.harness.clone())
            .unwrap_or_else(|(error, _)| panic!("{error}"))
    }
}
async fn entered(gate: &Gate, count: u32) {
    tokio::time::timeout(DEADLINE, gate.entered.acquire_many(count))
        .await
        .unwrap()
        .unwrap()
        .forget();
}
fn client(runtime: &Arc<HarnessRuntime>, id: AgentId) -> HarnessClient {
    let owner = runtime.entries().get(&id).unwrap().owner.clone().unwrap();
    HarnessClient {
        runtime: Arc::downgrade(runtime),
        owner: Arc::downgrade(&owner),
        id,
    }
}
async fn wait_phase(runtime: &HarnessRuntime, id: AgentId, phase: Option<OwnerPhase>) {
    tokio::time::timeout(DEADLINE, async {
        while runtime.phase(id) != phase {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn independent_harnesses_run_concurrently_and_cancel_does_not_poison_the_other() {
    let runtime = Arc::new(HarnessRuntime::new());
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let first = Fixture::new("first", Some(first_gate.clone()), None).await;
    let second = Fixture::new("second", Some(second_gate.clone()), None).await;
    let a = AgentId::new();
    let b = AgentId::new();
    let ca = first.attach(&runtime, a);
    let cb = second.attach(&runtime, b);
    let a_run = tokio::spawn(async move { ca.prompt_text("first".into()).await });
    let b_run = tokio::spawn(async move { cb.prompt_text("second".into()).await });
    entered(&first_gate, 1).await;
    entered(&second_gate, 1).await;
    assert!(matches!(
        client(&runtime, a).prompt_text("busy".into()).await,
        Err(InProcessError::Busy)
    ));
    assert!(client(&runtime, a).cancel().unwrap());
    second_gate.release.add_permits(1);
    let a_result = tokio::time::timeout(DEADLINE, a_run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let b_result = tokio::time::timeout(DEADLINE, b_run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        a_result.outcome,
        HarnessRunOutcome::Aborted { .. }
    ));
    assert!(matches!(
        b_result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_ne!(first.harness.session_id(), second.harness.session_id());
    assert_ne!(first.env.cwd(), second.env.cwd());
    assert!(!client(&runtime, b).cancel().unwrap());
    runtime.shutdown_harnesses().await.unwrap();
    assert!(runtime.is_closed());
    assert_eq!(runtime.owner_count(), 0);
    assert!(!first.storage.writer_lease_held());
    assert!(!second.storage.writer_lease_held());
}

#[tokio::test]
async fn duplicate_session_or_agent_is_rejected_without_closing_the_original() {
    let runtime = Arc::new(HarnessRuntime::new());
    let fixture = Fixture::new("original", None, None).await;
    let id = AgentId::new();
    let original = fixture.attach(&runtime, id);
    for candidate_id in [id, AgentId::new()] {
        let (error, rejected) = runtime
            .attach(candidate_id, fixture.harness.clone())
            .err()
            .unwrap();
        assert!(matches!(error, InProcessError::AlreadyOwned));
        assert_eq!(rejected.session_id(), "original");
        assert!(fixture.storage.writer_lease_held());
    }
    drop(original);
    assert_eq!(runtime.owner_count(), 1);
    assert!(client(&runtime, id)
        .prompt_text("still live".into())
        .await
        .is_ok());
    runtime.shutdown_harnesses().await.unwrap();
}

#[tokio::test]
async fn abandoned_waiters_keep_owner_and_writer_until_real_late_callback_finishes() {
    let runtime = Arc::new(HarnessRuntime::new());
    let tool = Gate::new();
    tool.release.add_permits(1);
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
    let fixture = Fixture::new("late", Some(tool), Some(hook)).await;
    let id = AgentId::new();
    let caller = fixture.attach(&runtime, id);
    let waiter = tokio::spawn(async move { caller.prompt_text("held".into()).await });
    entered(&late, 1).await;
    waiter.abort();
    let _ = waiter.await;
    let closer = runtime.clone();
    let close_waiter = tokio::spawn(async move { closer.close_harness(id).await });
    wait_phase(&runtime, id, Some(OwnerPhase::Closing)).await;
    close_waiter.abort();
    let _ = close_waiter.await;
    assert_eq!(runtime.owner_count(), 1);
    assert!(fixture.storage.writer_lease_held());
    let path = fixture.storage.jsonl_metadata().path;
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_err());
    late.release.add_permits(1);
    wait_phase(&runtime, id, None).await;
    assert!(!fixture.storage.writer_lease_held());
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_ok());
    runtime.shutdown_harnesses().await.unwrap();
}

#[tokio::test]
async fn unsaved_data_keeps_failed_close_owner_and_writer_and_does_not_claim_shutdown() {
    let runtime = Arc::new(HarnessRuntime::new());
    let fixture = Fixture::new("unsaved", None, None).await;
    fixture
        .harness
        .keep_unsaved_message_for_test("injected", UserMessage::new("unsaved", 1).into());
    let id = AgentId::new();
    fixture.attach(&runtime, id);
    assert!(matches!(
        runtime.close_harness(id).await,
        Err(InProcessError::Unsettled)
    ));
    assert_eq!(runtime.phase(id), Some(OwnerPhase::CloseFailed));
    assert!(fixture.storage.writer_lease_held());
    assert!(runtime.shutdown_harnesses().await.is_err());
    assert!(!runtime.is_closed());
    assert_eq!(runtime.owner_count(), 1);
    assert!(fixture.storage.writer_lease_held());
}

#[tokio::test]
async fn stopped_runtime_returns_rejected_harness_to_its_caller() {
    let runtime = Arc::new(HarnessRuntime::new());
    runtime.shutdown_harnesses().await.unwrap();
    let fixture = Fixture::new("not-adopted", None, None).await;
    let (error, rejected) = runtime
        .attach(AgentId::new(), fixture.harness.clone())
        .err()
        .unwrap();
    assert!(matches!(error, InProcessError::Stopping));
    assert_eq!(runtime.owner_count(), 0);
    assert!(fixture.storage.writer_lease_held());
    rejected.wait_for_idle().await.unwrap();
    rejected.drain_session().await;
    rejected.close().await.unwrap();
    fixture.storage.release_writer_lease();
}

#[tokio::test]
async fn old_client_never_retargets_a_replacement_with_the_same_agent_id() {
    let runtime = Arc::new(HarnessRuntime::new());
    let first = Fixture::new("old-instance", None, None).await;
    let id = AgentId::new();
    let old = first.attach(&runtime, id);
    runtime.close_harness(id).await.unwrap();
    let next = Fixture::new("new-instance", None, None).await;
    let current = next.attach(&runtime, id);
    assert!(matches!(old.cancel(), Err(InProcessError::NotLoaded)));
    assert!(matches!(
        old.write_failures(),
        Err(InProcessError::NotLoaded)
    ));
    assert!(matches!(
        old.prompt_text("must not run".into()).await,
        Err(InProcessError::NotLoaded)
    ));
    assert_eq!(runtime.phase(id), Some(OwnerPhase::Live));
    assert!(current.prompt_text("new owner".into()).await.is_ok());
    runtime.shutdown_harnesses().await.unwrap();
}

#[tokio::test]
async fn cleanup_barrier_keeps_resources_and_real_writer_after_shutdown_waiter_disappears() {
    let runtime = Arc::new(HarnessRuntime::new());
    let tool_binding = Gate::new();
    let retired_binding = Arc::downgrade(&tool_binding);
    let fixture = Fixture::new("cleanup-held", Some(tool_binding), None).await;
    let healthy = Fixture::new("cleanup-independent", None, None).await;
    let id = AgentId::new();
    let healthy_id = AgentId::new();
    healthy.attach(&runtime, healthy_id);
    let gate = Gate::new();
    let resource = Arc::new(());
    let weak = Arc::downgrade(&resource);
    let callback_gate = gate.clone();
    let cleanup: HarnessCleanup = Arc::new(move || {
        let resource = resource.clone();
        let gate = callback_gate.clone();
        let binding = retired_binding.clone();
        Box::pin(async move {
            assert!(binding.upgrade().is_none(), "Harness still pins the tool");
            let _retained = resource;
            gate.entered.add_permits(1);
            gate.release.acquire().await.unwrap().forget();
            Ok(())
        })
    });
    runtime
        .attach_with_cleanup(id, fixture.harness.clone(), &cleanup)
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    drop(cleanup);
    let closer = runtime.clone();
    let waiter = tokio::spawn(async move { closer.shutdown_harnesses().await });
    entered(&gate, 1).await;
    wait_phase(&runtime, healthy_id, None).await;
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(runtime.phase(id), Some(OwnerPhase::Closing));
    assert_eq!(runtime.owner_count(), 1);
    assert!(!runtime.is_closed());
    assert!(weak.upgrade().is_some());
    assert!(fixture.storage.writer_lease_held());
    let path = fixture.storage.jsonl_metadata().path;
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_err());
    gate.release.add_permits(1);
    tokio::time::timeout(DEADLINE, async {
        while !runtime.is_closed() || weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture.storage.writer_lease_held());
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_ok());
}

#[tokio::test]
async fn cleanup_error_or_panic_retains_failed_owner_and_retries_after_harness_closed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for panic_first in [false, true] {
        let runtime = Arc::new(HarnessRuntime::new());
        let fixture = Fixture::new("cleanup-retry", None, None).await;
        let id = AgentId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let cleanup: HarnessCleanup = Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert!(!panic_first, "injected cleanup panic");
                    return Err(InProcessError::Unsettled);
                }
                Ok(())
            })
        });
        runtime
            .attach_with_cleanup(id, fixture.harness.clone(), &cleanup)
            .unwrap_or_else(|(error, _)| panic!("{error}"));
        drop(cleanup);
        assert!(runtime.shutdown_harnesses().await.is_err());
        assert!(!runtime.is_closed());
        assert_eq!(runtime.phase(id), Some(OwnerPhase::CloseFailed));
        assert!(fixture.storage.writer_lease_held());
        let owner = runtime.entries().get(&id).unwrap().owner.clone().unwrap();
        assert!(owner.control().close_stage == CloseStage::BindingsRetired);
        assert!(owner.cleanup.is_some());
        drop(owner);
        runtime.shutdown_harnesses().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(runtime.is_closed());
        assert_eq!(runtime.owner_count(), 0);
        assert!(!fixture.storage.writer_lease_held());
    }
}

#[tokio::test]
async fn rejected_attach_does_not_consume_or_invoke_callers_cleanup() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let runtime = Arc::new(HarnessRuntime::new());
    runtime.shutdown_harnesses().await.unwrap();
    let fixture = Fixture::new("cleanup-rejected", None, None).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let cleanup: HarnessCleanup = Arc::new(move || {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let (error, rejected) = runtime
        .attach_with_cleanup(AgentId::new(), fixture.harness.clone(), &cleanup)
        .err()
        .unwrap();
    assert!(matches!(error, InProcessError::Stopping));
    assert_eq!(Arc::strong_count(&cleanup), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(fixture.storage.writer_lease_held());
    rejected.close().await.unwrap();
    cleanup().await.unwrap();
    fixture.storage.release_writer_lease();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn successful_cleanup_does_not_hide_late_unsaved_data_or_release_writer() {
    let runtime = Arc::new(HarnessRuntime::new());
    let fixture = Fixture::new("cleanup-unsaved", None, None).await;
    let harness = fixture.harness.clone();
    let cleanup: HarnessCleanup = Arc::new(move || {
        let harness = harness.clone();
        Box::pin(async move {
            // Fault injection, not an allowed raw-Harness operation. Real
            // cleanup can finish writes accepted before native close.
            harness.keep_unsaved_message_for_test(
                "cleanup injected",
                UserMessage::new("late unsaved", 1).into(),
            );
            Ok(())
        })
    });
    let id = AgentId::new();
    runtime
        .attach_with_cleanup(id, fixture.harness.clone(), &cleanup)
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    drop(cleanup);
    assert!(matches!(
        runtime.close_harness(id).await,
        Err(InProcessError::Unsettled)
    ));
    assert_eq!(runtime.phase(id), Some(OwnerPhase::CloseFailed));
    assert!(fixture.storage.writer_lease_held());
    assert!(runtime.shutdown_harnesses().await.is_err());
    assert!(!runtime.is_closed());
    assert_eq!(runtime.owner_count(), 1);
}
