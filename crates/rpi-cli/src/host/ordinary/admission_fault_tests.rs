// Included inside ordinary::tests::owner_registry::admission_faults.
use super::*;
use rpi_tools::env::{FileContent, ReadFile, WriterLease};
use rpi_tools::{FileError, FileErrorCode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct SyncFault {
    fail: AtomicBool,
    block_once: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
    calls: AtomicUsize,
}
impl SyncFault {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            fail: AtomicBool::new(fail),
            block_once: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            calls: AtomicUsize::new(0),
        })
    }
}
struct FaultFs {
    inner: Arc<dyn FileSystem>,
    fault: Arc<SyncFault>,
}
macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        #[async_trait]
        impl FileSystem for FaultFs {
            fn cwd(&self) -> &Path { self.inner.cwd() }
            async fn cleanup(&self) { self.inner.cleanup().await; }
            async fn sync_file_and_parent(&self, path: &str, cancel: Option<&CancellationToken>) -> Result<(), FileError> {
                self.fault.calls.fetch_add(1, Ordering::SeqCst);
                if self.fault.block_once.swap(false, Ordering::SeqCst) {
                    self.fault.entered.add_permits(1);
                    self.fault.release.acquire().await.unwrap().forget();
                }
                if self.fault.fail.load(Ordering::SeqCst) {
                    self.inner.sync_file(path, false, cancel).await?;
                    return Err(FileError::new(FileErrorCode::Unknown, "injected private sync failure"));
                }
                self.inner.sync_file_and_parent(path, cancel).await
            }
            $(async fn $name(&self, $($arg: $ty),*) -> Result<$ret, FileError> {
                self.inner.$name($($arg),*).await
            })*
        }
    };
}
delegate! {
    absolute_path(path: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
    join_path(parts: &[&str], cancel: Option<&CancellationToken>) -> std::path::PathBuf;
    read_text_file(path: &str, cancel: Option<&CancellationToken>) -> String;
    read_text_lines(path: &str, max_lines: Option<usize>, cancel: Option<&CancellationToken>) -> Vec<String>;
    read_binary_file(path: &str, cancel: Option<&CancellationToken>) -> Vec<u8>;
    write_file(path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> ();
    append_file(path: &str, content: FileContent, cancel: Option<&CancellationToken>) -> ();
    open_append_file(path: &str) -> Option<Box<dyn rpi_tools::env::AppendFile>>;
    open_read_file(path: &str) -> Option<Arc<dyn ReadFile>>;
    try_acquire_writer_lease(path: &str) -> Option<Box<dyn WriterLease>>;
    sync_file(path: &str, data_only: bool, cancel: Option<&CancellationToken>) -> ();
    sync_directory(path: &str, cancel: Option<&CancellationToken>) -> ();
    truncate_file(path: &str, len: u64, cancel: Option<&CancellationToken>) -> ();
    rename_file(source: &str, dest: &str, cancel: Option<&CancellationToken>) -> ();
    file_info(path: &str, cancel: Option<&CancellationToken>) -> rpi_tools::env::FileInfo;
    list_dir(path: &str, cancel: Option<&CancellationToken>) -> Vec<rpi_tools::env::FileInfo>;
    canonical_path(path: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
    exists(path: &str, cancel: Option<&CancellationToken>) -> bool;
    create_dir(path: &str, recursive: bool, cancel: Option<&CancellationToken>) -> ();
    remove(path: &str, recursive: bool, force: bool, cancel: Option<&CancellationToken>) -> ();
    create_temp_dir(prefix: Option<&str>, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
    create_temp_file(prefix: &str, suffix: &str, cancel: Option<&CancellationToken>) -> std::path::PathBuf;
}

async fn fault_restore(
    service: &HostService,
    fixture: &Fixture,
    record: &AgentRecord,
    provider: Arc<FauxProvider>,
    tool_gate: Option<Arc<Gate>>,
    fault: Arc<SyncFault>,
) -> OrdinaryClient {
    service
        .restore_bound(
            record.spec.id,
            fixture.catalog.guard(record),
            OrdinaryLimits::default(),
            OrdinaryBinding {
                environment: None,
                binder: BindingResolver::Immediate(binder(provider, tool_gate, None)),
                instruction_resources: None,
                extensions: None,
                filesystem: Some(Arc::new(FaultFs {
                    inner: Arc::new(OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone())),
                    fault,
                })),
            },
        )
        .await
        .unwrap()
}
async fn settled(client: &OrdinaryClient) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match client.snapshot().await {
                Ok(snapshot)
                    if snapshot.active.is_none() && snapshot.record.state == AgentState::Idle =>
                {
                    break
                }
                Ok(_) | Err(BackendError::Busy) => {}
                Err(error) => panic!("unexpected snapshot error: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn backend_sync_failure_retry_and_receipt_fail_closed_until_reproved() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let provider = FauxProvider::new(FauxScript::new().with_text("must-not-execute"));
    let record = fixture.record("sync-fail", &provider, false).await;
    let fault = SyncFault::new(true);
    let client = fault_restore(&service, &fixture, &record, provider, None, fault.clone()).await;
    let id = uuid::Uuid::now_v7().to_string();
    let messages = vec![UserMessage::new("uncertain custody", 0).into()];
    let ticket = client
        .submit_admitted("host".into(), id.clone(), messages.clone())
        .await
        .unwrap();
    assert!(matches!(
        ticket.wait_admitted().await,
        Err(BackendError::AdmissionUncertain)
    ));
    let report = ticket.wait().await.unwrap();
    assert!(matches!(
        report.outcome,
        Err(BackendError::AdmissionUncertain)
    ));
    let visible = bytes(&record);
    assert!(String::from_utf8_lossy(&visible).contains(&id));
    assert!(!String::from_utf8_lossy(&visible).contains("operation_started"));
    for _ in 0..2 {
        assert!(matches!(
            client
                .submit_admitted("host".into(), id.clone(), messages.clone())
                .await,
            Err(BackendError::AdmissionUncertain)
        ));
        assert!(matches!(
            client.admission(&id).await,
            Err(BackendError::AdmissionUncertain)
        ));
        assert!(matches!(
            client.admissions().await,
            Err(BackendError::AdmissionUncertain)
        ));
    }
    assert_eq!(bytes(&record), visible);
    // Identity conflicts must not be "repaired" into another request's receipt.
    assert!(matches!(
        client
            .submit_admitted("other-host".into(), id.clone(), messages.clone())
            .await,
        Err(BackendError::Admission(_))
    ));
    fault.fail.store(false, Ordering::SeqCst);
    let replay = client
        .submit_admitted("host".into(), id.clone(), messages)
        .await
        .unwrap();
    assert_eq!(replay.wait_admitted().await.unwrap().entry_seq, 1);
    assert!(matches!(
        replay.wait().await.unwrap().outcome,
        Ok(OrdinaryOutput::AlreadyAdmitted)
    ));
    assert_eq!(
        bytes(&record),
        visible,
        "re-proving custody must not execute or append"
    );
    assert!(fault.calls.load(Ordering::SeqCst) >= 8);
    assert!(matches!(
        client.admission(&id).await.unwrap().unwrap().execution,
        rpi_harness::agent_harness::HostAdmissionExecution::Admitted {}
    ));
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn active_run_retry_requires_a_new_barrier_without_reexecuting_or_cancelling() {
    let fixture = Fixture::new(1).await;
    let service = HostService::new(fixture.catalog.clone());
    let gate = Gate::new();
    let fault = SyncFault::new(false);
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("once"),
    );
    let record = fixture.record("sync-active", &provider, true).await;
    let client = fault_restore(
        &service,
        &fixture,
        &record,
        provider,
        Some(gate.clone()),
        fault.clone(),
    )
    .await;
    let id = uuid::Uuid::now_v7().to_string();
    let messages = vec![UserMessage::new("execute once", 0).into()];
    let ticket = client
        .submit_admitted("host".into(), id.clone(), messages.clone())
        .await
        .unwrap();
    let receipt = ticket.wait_admitted().await.unwrap();
    permits(&gate.entered, 1).await;
    let active_run = client.snapshot().await.unwrap().active.unwrap().run_id;
    fault.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        client
            .submit_admitted("host".into(), id.clone(), messages.clone())
            .await,
        Err(BackendError::AdmissionUncertain)
    ));
    assert!(matches!(
        client.admission(&id).await,
        Err(BackendError::AdmissionUncertain)
    ));
    assert_eq!(
        client.snapshot().await.unwrap().active.unwrap().run_id,
        active_run
    );
    fault.fail.store(false, Ordering::SeqCst);
    let replay = client
        .submit_admitted("host".into(), id, messages)
        .await
        .unwrap();
    assert_eq!(*replay.wait_admitted().await.unwrap(), *receipt);
    assert!(matches!(
        replay.wait().await.unwrap().outcome,
        Ok(OrdinaryOutput::AlreadyAdmitted)
    ));
    assert_eq!(gate.entered.available_permits(), 0);
    gate.release.add_permits(1);
    assert!(matches!(
        ticket.wait().await.unwrap().outcome,
        Ok(OrdinaryOutput::Run(_))
    ));
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ipc_sync_failure_never_returns_a_durable_receipt_from_visible_bytes() {
    use crate::host::{
        client::{ClientError, HostClient},
        protocol::{Command, ErrorCode, RequestId},
    };
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(FauxScript::new().with_text("never"));
    let record = fixture.record("sync-ipc", &provider, false).await;
    let fault = SyncFault::new(true);
    let client = fault_restore(
        daemon.service(),
        &fixture,
        &record,
        provider,
        None,
        fault.clone(),
    )
    .await;
    let mut ipc = HostClient::connect(connection).await.unwrap();
    let agent_id = record.spec.id.to_string().try_into().unwrap();
    let request_id = RequestId::new();
    let submit = Command::Submit {
        agent_id,
        request_id,
        prompt: "IPC uncertain input".into(),
    };
    let receipt = Command::Receipt {
        agent_id,
        request_id,
    };
    assert_eq!(
        ipc.call(submit.clone()).await,
        Err(ClientError::Rejected(ErrorCode::PersistenceUncertain))
    );
    settled(&client).await;
    let visible = bytes(&record);
    assert_eq!(
        ipc.call(submit.clone()).await,
        Err(ClientError::Rejected(ErrorCode::PersistenceUncertain))
    );
    assert_eq!(
        ipc.call(receipt.clone()).await,
        Err(ClientError::Rejected(ErrorCode::PersistenceUncertain))
    );
    assert_eq!(bytes(&record), visible);
    let absent = ipc
        .call(Command::Receipt {
            agent_id,
            request_id: RequestId::new(),
        })
        .await
        .unwrap();
    assert_eq!(absent["found"], false);
    fault.fail.store(false, Ordering::SeqCst);
    let observed = ipc.call(receipt).await.unwrap();
    assert_eq!(observed["found"], true);
    assert_eq!(observed["execution"]["state"], "admitted");
    assert_eq!(
        ipc.call(submit).await.unwrap()["receipt"],
        observed["receipt"]
    );
    assert_eq!(bytes(&record), visible);
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn ipc_admission_timeout_keeps_the_native_sync_and_run_owned() {
    use crate::host::{
        client::{ClientError, HostClient},
        protocol::{Command, RequestId},
    };
    let (fixture, mut daemon, connection) = daemon_fixture().await;
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("gate", serde_json::json!({}))
            .with_text("after-disconnect"),
    );
    let record = fixture.record("sync-disconnect", &provider, true).await;
    let fault = SyncFault::new(false);
    fault.block_once.store(true, Ordering::SeqCst);
    let tool = Gate::new();
    let client = fault_restore(
        daemon.service(),
        &fixture,
        &record,
        provider,
        Some(tool.clone()),
        fault.clone(),
    )
    .await;
    let mut short = connection.clone();
    short.timeout = Duration::from_millis(100);
    let mut ipc = HostClient::connect(short).await.unwrap();
    let agent_id = record.spec.id.to_string().try_into().unwrap();
    let request_id = RequestId::new();
    let submit = Command::Submit {
        agent_id,
        request_id,
        prompt: "disconnect while syncing".into(),
    };
    let call = tokio::spawn(async move { ipc.call(submit).await });
    permits(&fault.entered, 1).await;
    assert!(
        matches!(call.await.unwrap(), Err(ClientError::OutcomeUnknown { request_id: id, .. }) if id == request_id)
    );
    assert_eq!(tool.entered.available_permits(), 0);
    let mut ipc = HostClient::connect(connection).await.unwrap();
    assert_eq!(ipc.call(Command::Status {}).await.unwrap()["owners"], 1);
    fault.release.add_permits(1);
    permits(&tool.entered, 1).await;
    let receipt = Command::Receipt {
        agent_id,
        request_id,
    };
    assert_eq!(ipc.call(receipt.clone()).await.unwrap()["found"], true);
    tool.release.add_permits(1);
    settled(&client).await;
    let observed = ipc.call(receipt).await.unwrap();
    assert_eq!(observed["execution"]["finished"], true);
    assert!(String::from_utf8_lossy(&bytes(&record)).contains("after-disconnect"));
    assert_eq!(client.admissions().await.unwrap().len(), 1);
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_receipt_wait_retains_the_barrier_without_executing_input() {
    use crate::host::{
        client::{ClientError, HostClient},
        protocol::{Command, RequestId},
    };
    let (fixture, mut daemon, connection) =
        daemon_fixture_with_timeout(Duration::from_millis(100)).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("must-not-run"));
    let record = fixture.record("sync-query-cancel", &provider, false).await;
    let fault = SyncFault::new(true);
    let client = fault_restore(
        daemon.service(),
        &fixture,
        &record,
        provider,
        None,
        fault.clone(),
    )
    .await;
    let request_id = RequestId::new();
    let ticket = client
        .submit_admitted(
            String::from(connection.host_id),
            String::from(request_id),
            vec![UserMessage::new("unexecuted input", 0).into()],
        )
        .await
        .unwrap();
    assert!(ticket.wait_admitted().await.is_err());
    assert!(matches!(
        ticket.wait().await.unwrap().outcome,
        Err(BackendError::AdmissionUncertain)
    ));
    let visible = bytes(&record);
    fault.fail.store(false, Ordering::SeqCst);
    fault.block_once.store(true, Ordering::SeqCst);
    // Let the server's dispatch deadline drop the native observation future,
    // not just the client-side waiter. The storage-owned barrier must survive.
    let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
    let agent_id = record.spec.id.to_string().try_into().unwrap();
    let query = Command::Receipt {
        agent_id,
        request_id,
    };
    let waiting_query = query.clone();
    let wait = tokio::spawn(async move { ipc.call(waiting_query).await });
    permits(&fault.entered, 1).await;
    assert!(matches!(
        wait.await.unwrap(),
        Err(ClientError::QueryFailed(_))
    ));
    let mut ipc = HostClient::connect(connection).await.unwrap();
    assert_eq!(ipc.call(Command::Status {}).await.unwrap()["owners"], 1);
    fault.release.add_permits(1);
    let observed = ipc.call(query).await.unwrap();
    assert_eq!(observed["found"], true);
    assert_eq!(observed["execution"]["state"], "admitted");
    assert_eq!(bytes(&record), visible);
    daemon.shutdown().await.unwrap();
}
