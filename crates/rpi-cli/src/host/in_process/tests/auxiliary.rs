use super::*;
use crate::host::{
    cli_cleanup::{
        tests::{Fixture as Dll, SUCCESS},
        PluginCleanupLedger,
    },
    cli_resources::CliPluginResources,
};
use crate::resource_management::{mutation_lease, shared_lease};
use rpi_extensions::{ExtensionSession, PluginCleanupStatus, RuntimeActionHost, ToolCallContext};
use std::sync::atomic::{AtomicBool, Ordering};

struct DeferredDriver {
    schema: Tool,
    entered: Arc<Gate>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    exited: Arc<AtomicBool>,
}
#[async_trait]
impl AgentTool for DeferredDriver {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "deferred native driver"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> std::result::Result<AgentToolResult, AgentError> {
        let guard = rpi_agent::tool_execution_scope::track_background_tool().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        let exited = self.exited.clone();
        std::thread::spawn(move || {
            let _guard = guard;
            let _ = release.recv_timeout(DEADLINE);
            exited.store(true, Ordering::Release);
        });
        self.entered.entered.add_permits(1);
        Ok(AgentToolResult::text(
            "driver still owns its execution scope",
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_auxiliary_exit_then_real_dll_release_precede_physical_writer_and_resource_unlock() {
    let gate = Gate::new();
    let fixture = Fixture::new("auxiliary-retirement", Some(gate.clone()), None).await;
    let root = fixture._root.path().canonicalize().unwrap();
    let (release, receiver) = std::sync::mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    fixture
        .harness
        .set_tools(
            vec![HarnessTool::new(Arc::new(DeferredDriver {
                schema: Tool {
                    name: "gate".into(),
                    description: "fixture".into(),
                    parameters: Schema::new(serde_json::json!({"type":"object"})),
                    constrained_sampling: None,
                },
                entered: gate.clone(),
                release: std::sync::Mutex::new(Some(receiver)),
                exited: exited.clone(),
            }))],
            None,
        )
        .await
        .unwrap();
    let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
        vec![],
        root.clone(),
        tokio::runtime::Handle::current(),
        Default::default(),
        ToolCallContext::default(),
    );
    crate::extensions_actions::HarnessActionHost::set_harness(
        &cell,
        Arc::new(fixture.harness.clone()),
    );
    let host = Arc::new(host);
    host.independent_run(
        7,
        serde_json::json!({"op":"start","input":"background","maxTurns":8,"timeoutSeconds":300}),
    )
    .await
    .unwrap();
    entered(&gate, 1).await;

    let ledger = PluginCleanupLedger::new();
    let sessions = Arc::new(std::sync::Mutex::new(ExtensionSession::none()));
    let resources = CliPluginResources::new(
        ledger.clone(),
        Arc::new(vec![shared_lease(&root).unwrap()]),
        sessions.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(vec![])),
    );
    let dll = Dll::new(SUCCESS);
    let permit = resources.begin_reload().await.unwrap();
    let external = rpi_extensions::load_session_mixed_guarded(
        &[],
        std::slice::from_ref(&dll.path),
        ledger,
        None,
        Some(permit.host_guard()),
    );
    assert_eq!(external.loaded_paths().len(), 1);
    let receipt = external.cleanup_receipts()[0].clone();
    *sessions.lock().unwrap() = external.clone();
    drop(permit);
    let cleanup_host = host.clone();
    let auxiliary_entered = Arc::new(Semaphore::new(0));
    let cleanup_entered = auxiliary_entered.clone();
    let cleanup: HarnessCleanup = Arc::new(move || {
        let host = cleanup_host.clone();
        let resources = resources.clone();
        let entered = cleanup_entered.clone();
        Box::pin(async move {
            host.begin_auxiliary_shutdown();
            entered.add_permits(1);
            host.wait_auxiliary_shutdown()
                .await
                .map_err(|_| InProcessError::Unsettled)?;
            resources.close_plugins_and_leases().await
        })
    });
    let runtime = Arc::new(HarnessRuntime::new());
    let id = AgentId::new();
    runtime
        .attach_with_cleanup(id, fixture.harness.clone(), &cleanup)
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    drop(cleanup);
    let closing = runtime.clone();
    let waiter = tokio::spawn(async move { closing.close_harness(id).await });
    wait_phase(&runtime, id, Some(OwnerPhase::Closing)).await;
    tokio::time::timeout(DEADLINE, auxiliary_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    waiter.abort();
    let _ = waiter.await;
    let path = fixture.storage.jsonl_metadata().path;
    assert!(!exited.load(Ordering::Acquire));
    assert_eq!(runtime.owner_count(), 1);
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_err());
    assert!(mutation_lease(&root).is_err());
    release.send(()).unwrap();
    // Reaching plugin teardown requires the native auxiliary scope to really
    // exit, not merely publish its terminal status or disappear from a roster.
    tokio::time::timeout(DEADLINE, async {
        while !sessions.lock().unwrap().loaded_paths().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(exited.load(Ordering::Acquire));
    assert_eq!(receipt.status(), PluginCleanupStatus::Pending);
    assert_eq!(runtime.owner_count(), 1);
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_err());
    assert!(mutation_lease(&root).is_err());
    drop(external);
    wait_phase(&runtime, id, None).await;
    assert_eq!(receipt.status(), PluginCleanupStatus::Released);
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_ok());
    assert!(mutation_lease(&root).is_ok());
    runtime.shutdown_harnesses().await.unwrap();
}
