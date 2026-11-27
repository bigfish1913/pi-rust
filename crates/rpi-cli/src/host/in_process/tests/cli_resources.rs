use super::*;
use crate::host::{
    cli_cleanup::{
        tests::{Fixture as Dll, SUCCESS},
        PluginCleanupLedger,
    },
    cli_resources::CliPluginResources,
};
use crate::resource_management::{mutation_lease, shared_lease};
use rpi_extensions::{ExtensionSession, PluginCleanupStatus};

#[tokio::test]
async fn cli_plugin_owner_waits_for_real_dll_before_writer_and_resource_lease_release() {
    let fixture = Fixture::new("cli-resource-owner", None, None).await;
    let root = fixture._root.path().canonicalize().unwrap();
    let ledger = PluginCleanupLedger::new();
    let sessions = Arc::new(std::sync::Mutex::new(ExtensionSession::none()));
    let resources = CliPluginResources::new(
        ledger.clone(),
        Arc::new(vec![shared_lease(&root).unwrap()]),
        sessions.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(vec![])),
    );
    // Match the actual CLI's lifetime anchor. It must not pin an unretirable
    // lease after cleanup, and dropping a frontend context must not unpin it.
    let anchored = resources.clone();
    fixture
        .harness
        .events()
        .on_lifetime::<rpi_harness::events::RunEndEvent, _>(move |_| {
            let _ = &anchored;
        });
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
    let runtime = Arc::new(HarnessRuntime::new());
    let id = AgentId::new();
    let cleanup: HarnessCleanup = Arc::new(move || {
        let resources = resources.clone();
        Box::pin(async move { resources.close_plugins_and_leases().await })
    });
    runtime
        .attach_with_cleanup(id, fixture.harness.clone(), &cleanup)
        .unwrap_or_else(|(error, _)| panic!("{error}"));
    drop(cleanup);
    let closing = runtime.clone();
    let waiter = tokio::spawn(async move { closing.close_harness(id).await });
    tokio::time::timeout(DEADLINE, async {
        while !sessions.lock().unwrap().loaded_paths().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(receipt.status(), PluginCleanupStatus::Pending);
    assert_eq!(runtime.phase(id), Some(OwnerPhase::Closing));
    assert_eq!(runtime.owner_count(), 1);
    assert!(fixture.storage.writer_lease_held());
    let path = fixture.storage.jsonl_metadata().path;
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_err());
    assert!(mutation_lease(&root).is_err());
    drop(external);
    wait_phase(&runtime, id, None).await;
    assert_eq!(receipt.status(), PluginCleanupStatus::Released);
    assert!(!fixture.storage.writer_lease_held());
    // The outer native Harness still exists here, including the lifetime hook.
    assert!(mutation_lease(&root).is_ok());
    assert!(fixture.env.try_acquire_writer_lease(&path).await.is_ok());
    runtime.shutdown_harnesses().await.unwrap();
}
