use super::*;
use crate::host::cli_cleanup::tests::{Fixture as Dll, SUCCESS};
use crate::resource_management::{mutation_lease, shared_lease};
use std::{path::PathBuf, time::Duration};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    owner: Arc<CliPluginResources>,
    ledger: Arc<PluginCleanupLedger>,
    sessions: ExtensionSessionCell,
    bridge: Arc<rpi_extensions::ActionBridge>,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ledger = PluginCleanupLedger::new();
        let leases = Arc::new(vec![shared_lease(&root).unwrap()]);
        let sessions = Arc::new(Mutex::new(ExtensionSession::none()));
        let (host, _) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            root.clone(),
            tokio::runtime::Handle::current(),
            crate::args::Args::default().unknown_flags,
            rpi_extensions::ToolCallContext::new(root.to_string_lossy().into_owned()),
        );
        let bridge = rpi_extensions::ActionBridge::with_reload_and_ui(
            tokio::runtime::Handle::current(),
            Arc::new(host),
            rpi_extensions::reload_callback_from_mailbox(rpi_extensions::ReloadMailbox::new()),
            rpi_extensions::UiDialogMailbox::new(),
            rpi_extensions::ExtensionStatusMailbox::new(),
            rpi_extensions::EditorTextMailbox::new(),
        );
        let owner = CliPluginResources::new(
            ledger.clone(),
            leases,
            sessions.clone(),
            Arc::new(Mutex::new(Some(bridge.clone()))),
            Arc::new(RwLock::new(vec![])),
        );
        Self {
            _tmp: tmp,
            root,
            owner,
            ledger,
            sessions,
            bridge,
        }
    }
    async fn load(&self, dll: &Dll) -> ExtensionSession {
        let permit = self.owner.begin_reload().await.unwrap();
        let session = rpi_extensions::load_session_mixed_guarded(
            &[],
            std::slice::from_ref(&dll.path),
            self.ledger.clone(),
            Some(self.bridge.clone()),
            Some(permit.host_guard()),
        );
        *self.sessions.lock().unwrap() = session.clone();
        session
    }
}
async fn closing(owner: &CliPluginResources) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !owner.closing.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_permit_fences_retirement_and_late_reload_has_no_new_admission() {
    let fixture = Fixture::new();
    let permit = fixture.owner.begin_reload().await.unwrap();
    {
        let candidate = fixture.owner.begin_reload();
        tokio::pin!(candidate);
        assert!(
            futures::poll!(candidate.as_mut()).is_pending(),
            "reload candidates must not interleave cell publication"
        );
    }
    let owner = fixture.owner.clone();
    let close = tokio::spawn(async move { owner.close_plugins_and_leases().await });
    closing(&fixture.owner).await;
    assert!(!close.is_finished());
    assert!(
        fixture.bridge.is_active(),
        "do not invalidate a still-preparing candidate"
    );
    assert!(mutation_lease(&fixture.root).is_err());
    assert!(fixture.owner.begin_reload().await.is_none());
    drop(permit);
    tokio::time::timeout(Duration::from_secs(10), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!fixture.bridge.is_active());
    assert!(mutation_lease(&fixture.root).is_ok());
    fixture.owner.close_plugins_and_leases().await.unwrap();
}

#[tokio::test]
async fn cancelled_plugin_retirement_keeps_physical_lease_until_real_dll_release_and_retry() {
    let fixture = Fixture::new();
    let dll = Dll::new(SUCCESS);
    let external = fixture.load(&dll).await;
    let receipt = external.cleanup_receipts()[0].clone();
    let owner = fixture.owner.clone();
    let close = tokio::spawn(async move { owner.close_plugins_and_leases().await });
    closing(&fixture.owner).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.sessions.lock().unwrap().loaded_paths().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture.bridge.is_active());
    close.abort();
    let _ = close.await;
    assert_eq!(
        receipt.status(),
        rpi_extensions::PluginCleanupStatus::Pending
    );
    assert!(mutation_lease(&fixture.root).is_err());
    assert!(fixture.owner.begin_reload().await.is_none());
    drop(external);
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.owner.close_plugins_and_leases(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        receipt.status(),
        rpi_extensions::PluginCleanupStatus::Released
    );
    assert!(mutation_lease(&fixture.root).is_ok());
    fixture.owner.close_plugins_and_leases().await.unwrap();
}
