use super::*;
use rpi_extensions::{load_session_mixed_guarded, ExtensionSession};
use std::{path::PathBuf, process::Command, time::Duration};

pub(in crate::host) struct Fixture {
    _root: tempfile::TempDir,
    pub(in crate::host) path: PathBuf,
}
impl Fixture {
    pub(in crate::host) fn new(source: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("fixture.rs");
        let path = root.path().join(format!(
            "{}cleanup_fixture{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        ));
        std::fs::write(&input, source).unwrap();
        let output = Command::new("rustc")
            .args([
                "--crate-name",
                "cleanup_fixture",
                "--crate-type",
                "cdylib",
                "--edition=2021",
            ])
            .arg(&input)
            .arg("-o")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { _root: root, path }
    }
    fn load(&self, ledger: &Arc<PluginCleanupLedger>) -> ExtensionSession {
        load_session_mixed_guarded(
            &[],
            std::slice::from_ref(&self.path),
            ledger.clone(),
            None,
            None,
        )
    }
}
pub(in crate::host) const SUCCESS: &str = r#"
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const std::ffi::c_void) -> i32 { 0 }
"#;

#[tokio::test]
async fn observes_real_success_symbol_failure_and_rejected_registration_even_when_session_is_empty()
{
    for source in [
        SUCCESS,
        "#[no_mangle] pub extern \"C\" fn unrelated_symbol() {}",
        "#[no_mangle] pub extern \"C\" fn rpi_plugin_register(_: *const std::ffi::c_void) -> i32 { 73 }",
    ] {
        let fixture = Fixture::new(source);
        let ledger = PluginCleanupLedger::new();
        let session = fixture.load(&ledger);
        let receipts = ledger.state.lock().unwrap().receipts.clone();
        assert_eq!(receipts.len(), 1, "every opened load must be observable");
        if source != SUCCESS {
            assert!(session.loaded_paths().is_empty());
            assert!(session.cleanup_receipts().is_empty(), "successful-session evidence alone is incomplete");
        } else {
            assert_eq!(receipts[0].status(), PluginCleanupStatus::Pending);
        }
        drop(session);
        tokio::time::timeout(Duration::from_secs(10), ledger.wait_plugins_released())
            .await.unwrap().unwrap();
        assert_eq!(receipts[0].status(), PluginCleanupStatus::Released);
    }
    let ledger = PluginCleanupLedger::new();
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("absent-library");
    let session = load_session_mixed_guarded(&[], &[missing], ledger.clone(), None, None);
    assert!(session.loaded_paths().is_empty());
    assert!(
        ledger.state.lock().unwrap().receipts.is_empty(),
        "failed open must not fabricate a loaded handle"
    );
    ledger.wait_plugins_released().await.unwrap();
}

async fn sealed(ledger: &PluginCleanupLedger) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ledger.state.lock().unwrap().sealed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn abandoned_observer_does_not_drop_library_or_lose_pending_evidence() {
    let fixture = Fixture::new(SUCCESS);
    let ledger = PluginCleanupLedger::new();
    let first = fixture.load(&ledger);
    let second = fixture.load(&ledger);
    assert_eq!(ledger.state.lock().unwrap().receipts.len(), 2);
    let waiting = ledger.clone();
    let waiter = tokio::spawn(async move { waiting.wait_plugins_released().await });
    sealed(&ledger).await;
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(
        ledger.state.lock().unwrap().receipts[0].status(),
        PluginCleanupStatus::Pending
    );
    drop(first);
    assert_eq!(
        ledger.state.lock().unwrap().receipts[1].status(),
        PluginCleanupStatus::Pending
    );
    drop(second);
    tokio::time::timeout(Duration::from_secs(10), ledger.wait_plugins_released())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn overflow_and_late_observations_never_become_false_release_proof() {
    let fixture = Fixture::new(SUCCESS);
    for late in [false, true] {
        let ledger = PluginCleanupLedger::new();
        let session = fixture.load(&ledger);
        let receipt = ledger.state.lock().unwrap().receipts[0].clone();
        let waiter = if late {
            let waiting = ledger.clone();
            let task = tokio::spawn(async move { waiting.wait_plugins_released().await });
            sealed(&ledger).await;
            Some(task)
        } else {
            None
        };
        // Bounded metadata fault injection, not 1024 DLLs or an Agent benchmark.
        for _ in 0..if late { 1 } else { MAX_PENDING_RECEIPTS } {
            ledger.track_cleanup(receipt.clone());
        }
        assert!(ledger.state.lock().unwrap().unknown);
        assert!(ledger.state.lock().unwrap().receipts.len() <= MAX_PENDING_RECEIPTS);
        drop(session);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), receipt.wait())
                .await
                .unwrap(),
            PluginCleanupStatus::Released
        );
        if let Some(waiter) = waiter {
            assert!(waiter.await.unwrap().is_err());
        }
        assert!(ledger.wait_plugins_released().await.is_err());
    }
}
