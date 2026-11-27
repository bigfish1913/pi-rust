// Host Rust lifecycle + real DLL coverage. Not actual binary/DLL acceptance.
use super::*;
use crate::host::{
    service::{HostService, OwnerPhase},
    NativeExtensionCustody,
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

struct Dll {
    directory: Arc<tempfile::TempDir>,
    path: PathBuf,
    marker: PathBuf,
    callback_gate: PathBuf,
    finish_gate: PathBuf,
}
impl Drop for Dll {
    fn drop(&mut self) {
        // Plugin guard retains this directory until final physical cleanup,
        // including while a failing test unwinds and opens both gates.
        let _ = std::fs::write(&self.callback_gate, b"release");
        let _ = std::fs::write(&self.finish_gate, b"release");
    }
}
fn dll(register_status: i32, finish_status: i32) -> Dll {
    let directory = Arc::new(
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap(),
    );
    let root = directory.path().canonicalize().unwrap();
    let marker = root.join("events");
    let callback_gate = root.join("callback-release");
    let finish_gate = root.join("finish-release");
    let source = root.join("fixture.rs");
    let path = root.join(if cfg!(target_os = "macos") {
        "libhost_custody.dylib"
    } else {
        "libhost_custody.so"
    });
    let lifecycle_offset =
        std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_plugin_lifecycle);
    let result_offset = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_tool_result);
    std::fs::write(&source, format!(r#"
use std::ffi::c_void;
use std::sync::{{Arc, Mutex, atomic::{{AtomicBool, Ordering}}}};
static WORKER: Mutex<Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>> = Mutex::new(None);
#[repr(C)] #[derive(Clone, Copy)] struct Bytes {{ ptr: *const u8, len: usize }}
fn mark(value: &[u8]) {{
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open({marker:?}) {{ let _ = file.write_all(value); }}
}}
extern "C" fn begin(_: *mut c_void) {{
    mark(b"B");
    WORKER.lock().unwrap().as_ref().unwrap().0.store(true, Ordering::Release);
}}
extern "C" fn finish(_: *mut c_void) -> i32 {{
    let (_, worker) = WORKER.lock().unwrap().take().unwrap();
    worker.join().unwrap();
    mark(b"E");
    while !std::path::Path::new({finish_gate:?}).exists() {{ std::thread::sleep(std::time::Duration::from_millis(1)); }}
    mark(b"F");
    {finish_status}
}}
extern "C" fn result(_: Bytes, _: *mut Bytes, _: *mut c_void) -> i32 {{
    mark(b"C");
    while !std::path::Path::new({callback_gate:?}).exists() {{ std::thread::sleep(std::time::Duration::from_millis(1)); }}
    mark(b"D");
    0
}}
extern "C" fn free(_: Bytes) {{}}
#[no_mangle] pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    // Observe accidental loader module reuse without overwriting the first
    // instance's worker (which would make subsequent lifecycle callbacks UB).
    if WORKER.lock().unwrap().is_some() {{ mark(b"X"); return 79; }}
    type Lifecycle = extern "C" fn(extern "C" fn(*mut c_void), extern "C" fn(*mut c_void) -> i32, *mut c_void) -> i32;
    type ResultFn = extern "C" fn(Bytes, *mut Bytes, *mut c_void) -> i32;
    type ResultRegister = extern "C" fn(ResultFn, extern "C" fn(Bytes), *mut c_void) -> i32;
    let lifecycle: Option<Lifecycle> = unsafe {{ std::ptr::read_unaligned(api.add({lifecycle_offset}).cast()) }};
    assert_eq!(lifecycle.unwrap()(begin, finish, std::ptr::null_mut()), 0);
    let register_result: Option<ResultRegister> = unsafe {{ std::ptr::read_unaligned(api.add({result_offset}).cast()) }};
    assert_eq!(register_result.unwrap()(result, free, std::ptr::null_mut()), 0);
    let cancelled = Arc::new(AtomicBool::new(false));
    let work = cancelled.clone();
    let worker = std::thread::spawn(move || {{
        while !work.load(Ordering::Acquire) {{ std::thread::sleep(std::time::Duration::from_millis(1)); }}
        mark(b"W");
    }});
    *WORKER.lock().unwrap() = Some((cancelled, worker));
    mark(b"R");
    {register_status}
}}
"#, marker=marker.to_string_lossy(), finish_gate=finish_gate.to_string_lossy(), callback_gate=callback_gate.to_string_lossy())).unwrap();
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args([
            "--crate-name",
            "host_custody_fixture",
            "--crate-type",
            "cdylib",
            "--edition",
            "2021",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Dll {
        directory,
        path,
        marker,
        callback_gate,
        finish_gate,
    }
}
async fn marker(dll: &Dll, byte: u8) {
    tokio::time::timeout(DEADLINE, async {
        while !std::fs::read(&dll.marker)
            .unwrap_or_default()
            .contains(&byte)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
fn binding(
    record: &AgentRecord,
    provider: Arc<FauxProvider>,
    dll: &Dll,
    custody: Arc<NativeExtensionCustody>,
    tool_gate: Option<Arc<Gate>>,
) -> OrdinaryBinding {
    binding_with_inventory(
        record,
        provider,
        dll,
        custody,
        tool_gate,
        rpi_extensions::RegistrationInventory {
            tool_result: 1,
            ..Default::default()
        },
    )
}
fn binding_with_inventory(
    record: &AgentRecord,
    provider: Arc<FauxProvider>,
    dll: &Dll,
    custody: Arc<NativeExtensionCustody>,
    tool_gate: Option<Arc<Gate>>,
    expected: rpi_extensions::RegistrationInventory,
) -> OrdinaryBinding {
    let env = Arc::new(
        OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone()).with_environment(
            [(
                "HOME".to_string(),
                record.spec.launch.cwd.to_string_lossy().into_owned(),
            )]
            .into(),
        ),
    );
    let artifacts = Arc::new(
        crate::host::NativeExtensionArtifacts::freeze(std::slice::from_ref(&dll.path)).unwrap(),
    );
    let directory = dll.directory.clone();
    let agent = record.spec.id;
    let owner = custody.clone();
    let weak = Arc::downgrade(&custody);
    OrdinaryBinding::new_deferred(
        env,
        Box::new(move |spec, env, facts| {
            let after: rpi_agent::AfterToolCall = Arc::new(move |ctx, _signal| {
                let call = weak.upgrade().unwrap().call();
                let result = ctx.result.clone();
                let input = ctx.args.clone();
                let id = ctx.tool_call.id.clone();
                let name = ctx.tool_call.name.clone();
                let is_error = ctx.is_error;
                Box::pin(async move {
                    let call = call.unwrap();
                    tokio::task::spawn_blocking(move || {
                        let snapshot = call.session().snapshot().unwrap();
                        let changed = rpi_extensions::emit_tool_result_with_status(
                            &id, &name, &input, &result, is_error, snapshot,
                        )
                        .unwrap();
                        assert!(changed.is_none());
                        // call owns both the actual FFI's session keepalive and
                        // active scope until handler return and plugin frees.
                    })
                    .await
                    .unwrap();
                    None
                })
            });
            let options = super::binder(provider, tool_gate, Some(after))(spec, env, facts)?;
            Ok(Box::pin(async move {
                let staging_root = directory.path().to_owned();
                let staged =
                    tokio::task::spawn_blocking(move || artifacts.stage(&staging_root, agent))
                        .await
                        .map_err(|_| BackendError::TaskStopped)??;
                owner
                    .load_owned_checked(
                        staged.paths().to_vec(),
                        Arc::new(rpi_extensions::NullDiagnostics),
                        Some(Arc::new((staged, directory))),
                        move |index, actual| index == 0 && actual == &expected,
                    )
                    .await?;
                Ok(options)
            }))
        }),
    )
    .unwrap()
    .with_extensions(custody)
}
async fn writer_held(record: &AgentRecord) {
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(env
        .try_acquire_writer_lease(record.spec.session.as_ref().unwrap().path.to_str().unwrap())
        .await
        .is_err());
}
async fn no_owner(service: &HostService, id: AgentId) {
    tokio::time::timeout(DEADLINE, async {
        while service.phase(id).is_some() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
fn staging_directories(dll: &Dll) -> Vec<PathBuf> {
    std::fs::read_dir(dll.directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("rpi-host-")
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn readonly_preflight_does_not_register_dll_or_start_sdk_workers() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("dll-pure", &provider, false).await;
    let dll = dll(0, 0);
    let custody = NativeExtensionCustody::new();
    let bytes = std::fs::read(&record.spec.session.as_ref().unwrap().path).unwrap();
    binding(&record, provider, &dll, custody.clone(), None)
        .preflight_session(&record.spec)
        .await
        .unwrap();
    assert!(!dll.marker.exists());
    assert!(staging_directories(&dll).is_empty());
    assert_eq!(fixture.catalog.get(record.spec.id).await.unwrap(), record);
    assert_eq!(
        std::fs::read(&record.spec.session.as_ref().unwrap().path).unwrap(),
        bytes
    );
    assert!(custody.call().is_err());
    super::deferred_binding::writer_free(&record).await;
    HostService::new(fixture.catalog.clone())
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn failed_real_registration_keeps_starting_residency_until_actual_sdk_cleanup_after_waiter_abort(
) {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture
        .record("dll-register-failed", &provider, false)
        .await;
    let dll = dll(73, 0);
    let custody = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    let bound = binding(&record, provider, &dll, custody, None);
    let waiter = tokio::spawn({
        let service = service.clone();
        let guard = fixture.catalog.guard(&record);
        let id = record.spec.id;
        async move {
            service
                .restore_bound(id, guard, OrdinaryLimits::default(), bound)
                .await
        }
    });
    marker(&dll, b'E').await;
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Starting));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Starting
    );
    super::deferred_binding::writer_free(&record).await;
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    assert_eq!(service.owner_count(), 1);
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    no_owner(&service, record.spec.id).await;
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closed
    );
    super::deferred_binding::writer_free(&record).await;
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn real_after_tool_call_and_sdk_finalization_both_hold_host_owner_and_writer() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("done"),
    );
    let record = fixture.record("dll-callback", &provider, true).await;
    let dll = dll(0, 0);
    let custody = NativeExtensionCustody::new();
    let tool_gate = Gate::new();
    tool_gate.release.add_permits(1);
    let service = HostService::new(fixture.catalog.clone());
    let client = service
        .restore_bound(
            record.spec.id,
            fixture.catalog.guard(&record),
            OrdinaryLimits::default(),
            binding(&record, provider, &dll, custody, Some(tool_gate)),
        )
        .await
        .unwrap();
    let ticket = client.submit(prompt("run callback")).await.unwrap();
    marker(&dll, b'C').await;
    let close = tokio::spawn({
        let service = service.clone();
        let id = record.spec.id;
        async move { service.close_ordinary(id).await }
    });
    tokio::time::timeout(DEADLINE, async {
        while service.phase(record.spec.id) != Some(OwnerPhase::Closing) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    writer_held(&record).await;
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(!std::fs::read(&dll.marker).unwrap().contains(&b'E'));
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    std::fs::write(&dll.callback_gate, b"release").unwrap();
    marker(&dll, b'E').await;
    // Handler returned, but SDK finish/OS/context cleanup is still pending.
    assert_eq!(staging_directories(&dll).len(), 1);
    writer_held(&record).await;
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    no_owner(&service, record.spec.id).await;
    let _ = ticket.wait().await;
    assert!(staging_directories(&dll).is_empty());
    super::deferred_binding::writer_free(&record).await;
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closed
    );
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_binding_cannot_reuse_or_close_another_agents_dll_custody() {
    let fixture = Fixture::new(2).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let first = fixture.record("dll-first", &provider, false).await;
    let second = fixture.record("dll-second", &provider, false).await;
    let dll = dll(0, 0);
    let custody = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    service
        .restore_bound(
            first.spec.id,
            fixture.catalog.guard(&first),
            OrdinaryLimits::default(),
            binding(&first, provider.clone(), &dll, custody.clone(), None),
        )
        .await
        .unwrap();
    assert!(service
        .restore_bound(
            second.spec.id,
            fixture.catalog.guard(&second),
            OrdinaryLimits::default(),
            binding(&second, provider, &dll, custody.clone(), None)
        )
        .await
        .is_err());
    assert_eq!(service.phase(first.spec.id), Some(OwnerPhase::Live));
    assert_eq!(service.phase(second.spec.id), None);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    writer_held(&first).await;
    super::deferred_binding::writer_free(&second).await;
    drop(custody.call().unwrap());
    assert_eq!(std::fs::read(&dll.marker).unwrap(), b"R");
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    service.shutdown().await.unwrap();
}

fn refuse_idle(fixture: &Fixture) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_idle BEFORE UPDATE ON agents WHEN NEW.resident = 1 AND NEW.busy = 0 BEGIN SELECT RAISE(ABORT, 'synthetic Idle failure'); END;").unwrap();
    db
}

#[tokio::test(flavor = "current_thread")]
async fn post_commit_build_failure_holds_real_writer_until_dll_cleanup_finishes() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("dll-build-failed", &provider, false).await;
    let _db = refuse_idle(&fixture);
    let dll = dll(0, 0);
    let custody = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    let bound = binding(&record, provider, &dll, custody, None);
    let waiter = tokio::spawn({
        let service = service.clone();
        let guard = fixture.catalog.guard(&record);
        let id = record.spec.id;
        async move {
            service
                .restore_bound(id, guard, OrdinaryLimits::default(), bound)
                .await
        }
    });
    marker(&dll, b'E').await;
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Starting));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    writer_held(&record).await;
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    no_owner(&service, record.spec.id).await;
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closed
    );
    super::deferred_binding::writer_free(&record).await;
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn retained_sdk_cleanup_keeps_failed_start_or_close_and_any_acquired_writer() {
    const MODE: &str = "RPI_HOST_DLL_RETAINED_TEST_MODE";
    let Some(mode) = std::env::var_os(MODE) else {
        // These deliberately retained native handles/locks must die with an
        // isolated process, not contaminate this test executable's later runs.
        for mode in ["register", "build", "close"] {
            let directory = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::ordinary::tests::owner_registry::extension_custody::retained_sdk_cleanup_keeps_failed_start_or_close_and_any_acquired_writer", "--nocapture"])
                .env(MODE, mode).env("TMPDIR", directory.path()).output().unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let mode = mode.to_str().unwrap();
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let record = fixture.record("dll-retained", &provider, false).await;
    let _db = (mode == "build").then(|| refuse_idle(&fixture));
    let dll = dll(if mode == "register" { 73 } else { 0 }, 7);
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    let custody = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    let result = service
        .restore_bound(
            record.spec.id,
            fixture.catalog.guard(&record),
            OrdinaryLimits::default(),
            binding(&record, provider, &dll, custody.clone(), None),
        )
        .await;
    if mode == "close" {
        assert!(result.is_ok());
        assert!(service.close_ordinary(record.spec.id).await.is_err());
    } else {
        assert!(result.is_err());
    }
    let expected = if mode == "close" {
        OwnerPhase::CloseFailed
    } else {
        OwnerPhase::StartFailed
    };
    assert_eq!(service.phase(record.spec.id), Some(expected));
    assert_eq!(service.owner_count(), 1);
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(custody.call().is_err());
    assert_eq!(staging_directories(&dll).len(), 1);
    if mode == "register" {
        super::deferred_binding::writer_free(&record).await;
    } else {
        writer_held(&record).await;
    }
    // Neither an explicit retry nor shutdown can convert Retained to Released.
    assert!(service.close_ordinary(record.spec.id).await.is_err());
    assert_eq!(service.phase(record.spec.id), Some(expected));
    assert!(service.shutdown().await.is_err());
    assert_eq!(service.owner_count(), 1);
    assert!(!service.is_closed());
    assert!(matches!(
        HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await,
        Err(CatalogError::AlreadyRunning)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn same_artifact_staged_for_two_agents_has_independent_real_dll_static_runtime() {
    let fixture = Fixture::new(2).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let first = fixture.record("dll-isolated-first", &provider, false).await;
    let second = fixture
        .record("dll-isolated-second", &provider, false)
        .await;
    let dll = dll(0, 0);
    let first_owner = NativeExtensionCustody::new();
    let second_owner = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    service
        .restore_bound(
            first.spec.id,
            fixture.catalog.guard(&first),
            OrdinaryLimits::default(),
            binding(&first, provider.clone(), &dll, first_owner.clone(), None),
        )
        .await
        .unwrap();
    service
        .restore_bound(
            second.spec.id,
            fixture.catalog.guard(&second),
            OrdinaryLimits::default(),
            binding(&second, provider, &dll, second_owner.clone(), None),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dll.marker).unwrap(), b"RR");
    assert_eq!(staging_directories(&dll).len(), 2);
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    service.close_ordinary(first.spec.id).await.unwrap();
    assert_eq!(staging_directories(&dll).len(), 1);
    assert_eq!(service.phase(second.spec.id), Some(OwnerPhase::Live));
    drop(second_owner.call().unwrap());
    writer_held(&second).await;
    service.close_ordinary(second.spec.id).await.unwrap();
    assert!(staging_directories(&dll).is_empty());
    assert_eq!(std::fs::read(&dll.marker).unwrap(), b"RRBWEFBWEF");
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_actual_inventory_keeps_starting_until_real_registered_worker_cleanup() {
    let fixture = Fixture::new(1).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("must not run"));
    let record = fixture
        .record("dll-inventory-rejected", &provider, false)
        .await;
    let path = &record.spec.session.as_ref().unwrap().path;
    let original = std::fs::read(path).unwrap();
    let dll = dll(0, 0);
    let custody = NativeExtensionCustody::new();
    let service = HostService::new(fixture.catalog.clone());
    // Declares zero hooks; this fixture actually registered tool_result=1.
    let bound = binding_with_inventory(
        &record,
        provider,
        &dll,
        custody.clone(),
        None,
        rpi_extensions::RegistrationInventory::default(),
    );
    let waiter = tokio::spawn({
        let service = service.clone();
        let guard = fixture.catalog.guard(&record);
        let id = record.spec.id;
        async move {
            service
                .restore_bound(id, guard, OrdinaryLimits::default(), bound)
                .await
        }
    });
    marker(&dll, b'E').await;
    assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Starting));
    assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
    assert!(
        custody.call().is_err(),
        "rejected registry became executable"
    );
    assert_eq!(staging_directories(&dll).len(), 1);
    assert_eq!(std::fs::read(path).unwrap(), original);
    super::deferred_binding::writer_free(&record).await;
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    assert!(waiter.await.unwrap().is_err());
    assert_eq!(service.phase(record.spec.id), None);
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().state,
        AgentState::Closed
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(staging_directories(&dll).is_empty());
    service.shutdown().await.unwrap();
}
