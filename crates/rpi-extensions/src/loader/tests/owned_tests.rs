//! Real compiled cdylib coverage of explicit load custody and cleanup evidence.
//! These are loader tests, NOT Host binary/management DLL acceptance.

use super::*;
use crate::PluginCleanupStatus::{Pending, Released, Retained};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

async fn settled(receipts: &[PluginCleanupReceipt], expected: crate::PluginCleanupStatus) {
    for receipt in receipts {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), receipt.wait())
                .await
                .expect("plugin cleanup did not settle"),
            expected
        );
        assert_eq!(receipt.status(), expected);
    }
}

async fn wait_for_marker(marker: &Path, value: u8) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fs::read(marker).unwrap_or_default().contains(&value) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("lifecycle marker did not appear");
}

fn must_not_register(name: &str, marker: &Path) -> CdylibFixture {
    build_cdylib_fixture(
        name,
        &format!(
            r#"
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const std::ffi::c_void) -> i32 {{
    std::fs::write({:?}, b"registered").unwrap();
    0
}}
"#,
            marker.to_string_lossy()
        ),
    )
}

#[tokio::test]
async fn failed_registration_retains_entire_batch_without_publishing_or_starting_later_dlls() {
    let (first, first_marker) = lifecycle_fixture("owned_first", 0, 0);
    let (bad, bad_marker) = lifecycle_fixture("owned_register_failure", 73, 0);
    let never_marker = first.dir.join("later-registered");
    let never = must_not_register("owned_never", &never_marker);
    let guard = Arc::new(());
    let weak = Arc::downgrade(&guard);
    let failure = load_explicit_session_owned(
        &[first.path.clone(), bad.path.clone(), never.path.clone()],
        Arc::new(NullDiagnostics),
        Some(guard.clone()),
    )
    .err()
    .expect("a failed DLL must fail the whole explicit selection");
    drop(guard);
    assert!(matches!(
        failure.error,
        PluginLoadError::RegisterReturned { code: 73, .. }
    ));
    assert_eq!(failure.custody.plugin_count(), 2);
    assert!(!never_marker.exists(), "loading continued past failure");
    assert!(!first_marker.exists());
    assert!(!bad_marker.exists(), "failed DLL was prematurely dropped");
    assert!(weak.upgrade().is_some());
    let receipts = failure.custody.cleanup_receipts();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|receipt| receipt.status() == Pending));
    failure.custody.begin_shutdown();
    failure.custody.begin_shutdown();
    assert!(!fs::read(&bad_marker).unwrap_or_default().contains(&b'F'));
    drop(failure);
    settled(&receipts, Released).await;
    assert!(weak.upgrade().is_none());
    for marker in [first_marker, bad_marker] {
        let events = fs::read(&marker).unwrap();
        assert_eq!(events.iter().filter(|&&event| event == b'B').count(), 1);
        assert!(events.ends_with(b"F"));
        fs::remove_file(marker).unwrap();
    }
}

#[tokio::test]
async fn later_open_failure_keeps_earlier_dll_in_owned_failure() {
    let (first, marker) = lifecycle_fixture("owned_open_failure_first", 0, 0);
    let bad = first.dir.join("invalid-library.dylib");
    fs::write(&bad, b"not a dynamic library").unwrap();
    let failure =
        load_explicit_session_owned(&[first.path.clone(), bad], Arc::new(NullDiagnostics), None)
            .err()
            .unwrap();
    assert!(matches!(failure.error, PluginLoadError::Open { .. }));
    assert_eq!(failure.custody.plugin_count(), 1);
    assert!(!marker.exists());
    let receipts = failure.custody.cleanup_receipts();
    assert_eq!(receipts[0].status(), Pending);
    drop(failure);
    settled(&receipts, Released).await;
    fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn symbol_lookup_failure_keeps_opened_library_and_earlier_workers_in_custody() {
    let (first, marker) = lifecycle_fixture("owned_symbol_first", 0, 0);
    let no_symbol = build_cdylib_fixture(
        "owned_no_symbol",
        "#[no_mangle] pub extern \"C\" fn not_the_plugin_entrypoint() -> i32 { 42 }",
    );
    let guard = Arc::new(());
    let weak = Arc::downgrade(&guard);
    let failure = load_explicit_session_owned(
        &[first.path.clone(), no_symbol.path.clone()],
        Arc::new(NullDiagnostics),
        Some(guard.clone()),
    )
    .err()
    .unwrap();
    drop(guard);
    assert!(matches!(failure.error, PluginLoadError::Symbol { .. }));
    assert_eq!(failure.custody.plugin_count(), 2);
    assert!(!marker.exists());
    assert!(weak.upgrade().is_some());
    let receipts = failure.custody.cleanup_receipts();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|receipt| receipt.status() == Pending));
    drop(failure);
    settled(&receipts, Released).await;
    assert!(weak.upgrade().is_none());
    fs::remove_file(marker).unwrap();
}

#[test]
fn entire_explicit_path_selection_is_validated_before_registration() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-register");
    let first = must_not_register("owned_preflight", &marker);
    let mut cases = vec![
        vec![first.path.clone(), PathBuf::from("relative.dylib")],
        vec![first.path.clone(), directory.path().join("missing.dylib")],
        vec![first.path.clone(), directory.path().to_path_buf()],
        vec![first.path.clone(), first.path.clone()],
        vec![first.path.clone(); 33],
    ];
    #[cfg(unix)]
    {
        let alias = directory.path().join("alias.dylib");
        std::os::unix::fs::symlink(&first.path, &alias).unwrap();
        cases.push(vec![first.path.clone(), alias]);
    }
    for files in cases {
        let failure = load_explicit_session_owned(&files, Arc::new(NullDiagnostics), None)
            .err()
            .expect("invalid explicit selection accepted");
        assert!(matches!(
            failure.error,
            PluginLoadError::ExplicitSelection { .. } | PluginLoadError::ExplicitPath { .. }
        ));
        assert_eq!(failure.custody.plugin_count(), 0);
        assert!(failure.custody.cleanup_receipts().is_empty());
        assert!(!marker.exists(), "preflight invoked a DLL entrypoint");
    }
    let session = load_explicit_session_owned(&[], Arc::new(NullDiagnostics), None).unwrap();
    assert!(session.is_empty());
    assert!(session.cleanup_receipts().is_empty());
}

struct ReleaseFile(PathBuf);
impl Drop for ReleaseFile {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"release");
    }
}

#[tokio::test]
async fn receipts_do_not_keep_dll_alive_and_wait_for_actual_cleanup_after_last_callback() {
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let gate = directory.path().join("release");
    let (fixture, marker) = lifecycle_fixture_gated("owned_physical_cleanup", 0, 0, Some(&gate));
    let _release_on_panic = ReleaseFile(gate.clone());
    // Even a test panic must not delete the worker's marker directory before
    // the asynchronous cleanup/guard actually returns.
    let guard = Arc::new(directory.clone());
    let weak = Arc::downgrade(&guard);
    let session = load_explicit_session_owned(
        std::slice::from_ref(&fixture.path),
        Arc::new(NullDiagnostics),
        Some(guard.clone()),
    )
    .unwrap();
    drop(guard);
    assert_eq!(
        session.loaded_paths(),
        std::slice::from_ref(&fs::canonicalize(&fixture.path).unwrap())
    );
    assert!(session.action_bridge().is_none());
    assert!(!session.is_empty());
    let receipts = session.cleanup_receipts();
    let callback = session.keepalive();
    session.begin_shutdown();
    drop(session);
    assert_eq!(receipts[0].status(), Pending);
    assert!(!fs::read(&marker).unwrap_or_default().contains(&b'F'));
    assert!(weak.upgrade().is_some());
    drop(callback);
    wait_for_marker(&marker, b'E').await;
    assert_eq!(receipts[0].status(), Pending, "queued Drop is not release");
    assert!(weak.upgrade().is_some());
    let waiter = tokio::spawn({
        let receipt = receipts[0].clone();
        async move { receipt.wait().await }
    });
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(receipts[0].status(), Pending);
    fs::write(&gate, b"release").unwrap();
    settled(&receipts, Released).await;
    assert!(weak.upgrade().is_none());
    assert!(fs::read(&marker).unwrap().ends_with(b"F"));
    #[cfg(windows)]
    assert!(!dll_is_mapped(&fixture.path));
}

#[tokio::test]
async fn release_is_reported_only_after_host_guard_destructor_returns() {
    struct Guard {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    struct ReleaseOnPanic(Arc<AtomicBool>);
    impl Drop for ReleaseOnPanic {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let (fixture, marker) = lifecycle_fixture("owned_guard_drop", 0, 0);
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let _release_on_panic = ReleaseOnPanic(release.clone());
    let session = load_explicit_session_owned(
        std::slice::from_ref(&fixture.path),
        Arc::new(NullDiagnostics),
        Some(Arc::new(Guard {
            entered: entered.clone(),
            release: release.clone(),
        })),
    )
    .unwrap();
    let receipts = session.cleanup_receipts();
    drop(session);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(fs::read(&marker).unwrap().ends_with(b"F"));
    assert_eq!(receipts[0].status(), Pending);
    release.store(true, Ordering::SeqCst);
    settled(&receipts, Released).await;
    fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn failed_sdk_shutdown_reports_retained_for_successful_and_failed_registration() {
    // Process isolation also lets Windows clean intentionally retained DLLs.
    const CHILD_DIR: &str = "RPI_TEST_GUARD_PROCESS_DIR";
    let Some(directory) = std::env::var_os(CHILD_DIR) else {
        for registration_failed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "loader::tests::owned_tests::failed_sdk_shutdown_reports_retained_for_successful_and_failed_registration",
                    "--nocapture",
                ])
                .env(CHILD_DIR, directory.path())
                .env("RPI_TEST_OWNED_REGISTRATION_FAIL", registration_failed.to_string())
                .output()
                .unwrap();
            directory.close().unwrap();
            assert!(
                output.status.success(),
                "owned lifecycle child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let registration_failed = std::env::var("RPI_TEST_OWNED_REGISTRATION_FAIL").unwrap() == "true";
    let gate = PathBuf::from(directory).join("release");
    let (fixture, marker) = lifecycle_fixture_gated(
        "owned_retained",
        if registration_failed { 73 } else { 0 },
        1,
        Some(&gate),
    );
    let _release_on_panic = ReleaseFile(gate.clone());
    let guard = Arc::new(());
    let weak = Arc::downgrade(&guard);
    let loaded = load_explicit_session_owned(
        std::slice::from_ref(&fixture.path),
        Arc::new(NullDiagnostics),
        Some(guard.clone()),
    );
    drop(guard);
    let receipts = match loaded {
        Ok(session) => {
            assert!(!registration_failed);
            let receipts = session.cleanup_receipts();
            drop(session);
            receipts
        }
        Err(failure) => {
            assert!(registration_failed);
            assert_eq!(failure.custody.plugin_count(), 1);
            let receipts = failure.custody.cleanup_receipts();
            drop(failure);
            receipts
        }
    };
    wait_for_marker(&marker, b'E').await;
    assert_eq!(receipts[0].status(), Pending);
    assert!(weak.upgrade().is_some());
    fs::write(&gate, b"release").unwrap();
    settled(&receipts, Retained).await;
    assert!(weak.upgrade().is_some(), "unsafe cleanup released custody");
    #[cfg(windows)]
    assert!(dll_is_mapped(&fixture.path));
}

#[tokio::test]
async fn checked_inventory_rejects_before_next_dll_and_retains_entire_loaded_batch() {
    let (first, first_marker) = lifecycle_fixture("owned_checked_first", 0, 0);
    let (second, second_marker) = lifecycle_fixture("owned_checked_second", 0, 0);
    let never_marker = first.dir.join("checked-never");
    let never = must_not_register("owned_checked_never", &never_marker);
    let mut indices = Vec::new();
    let failure = load_explicit_session_owned_checked(
        &[first.path.clone(), second.path.clone(), never.path.clone()],
        Arc::new(NullDiagnostics),
        None,
        |index, inventory| {
            assert_eq!(*inventory, RegistrationInventory::default());
            indices.push(index);
            index == 0
        },
    )
    .err()
    .expect("inventory rejection must not publish a session");
    assert_eq!(indices, [0, 1]);
    assert_eq!(failure.custody.plugin_count(), 2);
    assert!(matches!(
        failure.error,
        PluginLoadError::ExplicitPath {
            reason: "explicit registration inventory rejected",
            ..
        }
    ));
    assert!(!never_marker.exists());
    assert!(!first_marker.exists() && !second_marker.exists());
    let receipts = failure.custody.cleanup_receipts();
    assert!(receipts.iter().all(|receipt| receipt.status() == Pending));
    drop(failure);
    settled(&receipts, Released).await;
    for marker in [first_marker, second_marker] {
        fs::remove_file(marker).unwrap();
    }
}

#[tokio::test]
async fn checked_inventory_validator_panic_keeps_opened_plugin_and_receipt_in_custody() {
    let (fixture, marker) = lifecycle_fixture("owned_checked_panic", 0, 0);
    let failure = load_explicit_session_owned_checked(
        std::slice::from_ref(&fixture.path),
        Arc::new(NullDiagnostics),
        None,
        |_, _| panic!("injected data-only validator failure"),
    )
    .err()
    .expect("validator panic must not escape and lose plugin custody");
    assert_eq!(failure.custody.plugin_count(), 1);
    assert!(matches!(
        failure.error,
        PluginLoadError::ExplicitPath {
            reason: "explicit registration inventory validator panicked",
            ..
        }
    ));
    assert!(!marker.exists());
    let receipts = failure.custody.cleanup_receipts();
    drop(failure);
    settled(&receipts, Released).await;
    fs::remove_file(marker).unwrap();
}
