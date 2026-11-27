//! Real raw Agent + closed native profile + HTTP. These prove the Rust owner
//! boundary, NOT daemon/wire Mini qualification or a management DLL E2E.
use super::*;
use crate::host::{
    mini::{MiniClient, MiniLimits, MiniOutcome, MiniReceipt, MiniResult},
    runtime::OwnerPhase,
    AgentRecord, AgentState, HostEpoch, RequestId, RunId,
};
use std::os::unix::fs::PermissionsExt;
#[path = "mini_wire_tests.rs"]
mod mini_wire_tests;

async fn registered(fixture: &Fixture, name: &str) -> AgentRecord {
    let cwd = fixture.root.path().join(name);
    tokio::fs::create_dir(&cwd).await.unwrap();
    let mut spec = mini_spec(&cwd.canonicalize().unwrap());
    spec.name = name.into();
    fixture.catalog.register(spec).await.unwrap()
}
async fn start(
    service: &HostService,
    record: &AgentRecord,
    url: &str,
    limits: MiniLimits,
) -> MiniClient {
    service
        .start_mini(
            record.spec.id,
            service.catalog().guard(record),
            profile(&record.spec.launch.cwd, url, "private-mini-key"),
            limits,
        )
        .await
        .unwrap()
}
async fn finished(client: &MiniClient, receipt: MiniReceipt) -> MiniResult {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let result = client.result(receipt).await.unwrap().unwrap();
            if result.outcome != MiniOutcome::Running {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap()
}
async fn reply(stream: &mut tokio::net::TcpStream) {
    let body = completion(None);
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}
fn no_session_files(record: &AgentRecord) {
    assert!(record.spec.session.is_none());
    assert_eq!(
        std::fs::read_dir(&record.spec.launch.cwd).unwrap().count(),
        0
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_volatile_identity_exact_input_no_replay_and_restart_lost() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-identity").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = request(&mut stream).await;
            assert!(headers
                .to_lowercase()
                .contains("authorization: bearer private-mini-key"));
            assert_eq!(
                body["messages"].as_array().unwrap().last().unwrap()["content"],
                "literal {{do_not_expand}}"
            );
            arrived.send(()).unwrap();
            released.await.unwrap();
            reply(&mut stream).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "duplicate replayed HTTP"
            );
        });
        let service = HostService::new(fixture.catalog.clone());
        let client = start(&service, &record, &url, MiniLimits::default()).await;
        let epoch = fixture.catalog.epoch();
        let id = RequestId::new();
        assert!(client
            .submit(HostEpoch::new(), id, "wrong epoch".into())
            .await
            .is_err());
        assert!(service.ordinary_client(record.spec.id).is_err());
        assert!(service
            .close_ordinary_guarded(
                record.spec.id,
                fixture
                    .catalog
                    .guard(&fixture.catalog.get(record.spec.id).await.unwrap())
            )
            .await
            .is_err());
        assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::Live));
        let receipt = client
            .submit(epoch, id, "literal {{do_not_expand}}".into())
            .await
            .unwrap();
        arrival.await.unwrap();
        assert_eq!(
            client
                .submit(epoch, id, "literal {{do_not_expand}}".into())
                .await
                .unwrap(),
            receipt
        );
        assert!(client.submit(epoch, id, "different".into()).await.is_err());
        assert!(matches!(
            client.submit(epoch, RequestId::new(), "busy".into()).await,
            Err(BackendError::Busy)
        ));
        let mut wrong = receipt;
        wrong.agent_id = AgentId::new();
        assert!(client.result(wrong).await.is_err());
        wrong = receipt;
        wrong.run_id = RunId::new();
        assert!(client.result(wrong).await.unwrap().is_none());
        assert!(!client.cancel(wrong).await.unwrap());
        drop(client); // Observer drop is not abort; reattach to this owner only.
        let client = service.mini_client(record.spec.id).unwrap();
        assert_eq!(
            client.result(receipt).await.unwrap().unwrap().outcome,
            MiniOutcome::Running
        );
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        release.send(()).unwrap();
        let result = finished(&client, receipt).await;
        assert_eq!(result.outcome, MiniOutcome::Succeeded);
        assert_eq!(result.answer, "native-binding-finished");
        assert!(!client.cancel(receipt).await.unwrap());
        assert_eq!(
            client
                .submit(epoch, id, "literal {{do_not_expand}}".into())
                .await
                .unwrap(),
            receipt
        );
        server.await.unwrap();
        no_session_files(&record);
        service.shutdown().await.unwrap();
        assert_eq!(service.owner_count(), 0);
        assert!(client.result(receipt).await.is_err());
        no_session_files(&record);
        let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default())
            .await
            .unwrap();
        assert_ne!(reopened.epoch(), epoch);
        let lost = reopened.get(record.spec.id).await.unwrap();
        assert_eq!(lost.state, AgentState::Lost);
        let restarted = HostService::new(reopened.clone());
        assert!(restarted.mini_client(record.spec.id).is_err());
        // Lost Mini does not restore or silently create a replacement owner.
        assert!(restarted
            .start_mini(
                record.spec.id,
                reopened.guard(&lost),
                profile(&record.spec.launch.cwd, &url, "unused"),
                MiniLimits::default()
            )
            .await
            .is_err());
        assert_eq!(restarted.owner_count(), 0);
        restarted.shutdown().await.unwrap();
        no_session_files(&record);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_bounded_outputs_and_request_slots_do_not_evict_or_replay() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-budget").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                request(&mut stream).await;
                reply(&mut stream).await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let service = HostService::new(fixture.catalog.clone());
        let client = start(
            &service,
            &record,
            &url,
            MiniLimits {
                max_input_bytes: 32,
                max_results: 2,
                max_output_bytes: 30,
            },
        )
        .await;
        let epoch = fixture.catalog.epoch();
        assert!(matches!(
            client.submit(epoch, RequestId::new(), "x".repeat(33)).await,
            Err(BackendError::InputTooLarge)
        ));
        let first = client
            .submit(epoch, RequestId::new(), "first".into())
            .await
            .unwrap();
        assert_eq!(
            finished(&client, first).await.outcome,
            MiniOutcome::Succeeded
        );
        let second = client
            .submit(epoch, RequestId::new(), "second".into())
            .await
            .unwrap();
        let over = finished(&client, second).await;
        assert_eq!(over.outcome, MiniOutcome::OutputLimit);
        assert!(over.answer.is_empty());
        assert_eq!(
            client.result(first).await.unwrap().unwrap().answer,
            "native-binding-finished"
        );
        assert!(client
            .submit(epoch, RequestId::new(), "third".into())
            .await
            .is_err());
        assert_eq!(
            client
                .submit(epoch, first.request_id, "first".into())
                .await
                .unwrap(),
            first
        );
        server.await.unwrap();
        service.close_agent(record.spec.id).await.unwrap();
        assert_eq!(
            fixture.catalog.get(record.spec.id).await.unwrap().state,
            AgentState::Lost
        );
        no_session_files(&record);
        service.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_provider_failure_does_not_wedge_following_run() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-error").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap(); request(&mut stream).await;
            let body = r#"{"error":{"message":"private-provider-failure","type":"invalid_request_error"}}"#;
            stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap(); request(&mut stream).await;
            reply(&mut stream).await;
        });
        let service = HostService::new(fixture.catalog.clone());
        let client = start(&service, &record, &url, MiniLimits::default()).await;
        let first = client.submit(fixture.catalog.epoch(), RequestId::new(), "fail".into()).await.unwrap();
        let failure = finished(&client, first).await;
        assert_eq!(failure.outcome, MiniOutcome::Failed);
        assert!(!failure.answer.contains("private-provider-failure"));
        let second = client.submit(fixture.catalog.epoch(), RequestId::new(), "recover".into()).await.unwrap();
        assert_eq!(finished(&client, second).await.outcome, MiniOutcome::Succeeded);
        server.await.unwrap();
        service.shutdown().await.unwrap(); no_session_files(&record);
    }).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_and_persistent_share_exact_owner_table_capacity_and_close_dispatch() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let mini = registered(&fixture, "mini-mixed").await;
        let ordinary = fixture
            .record("persistent-mixed", "persistent-workspace", &[], None)
            .await;
        let third = registered(&fixture, "mini-excess").await;
        let service = HostService::new(fixture.catalog.clone());
        let _observer = start(
            &service,
            &mini,
            "http://127.0.0.1:9/v1",
            MiniLimits::default(),
        )
        .await;
        let binding = profile(&ordinary.spec.launch.cwd, "http://127.0.0.1:9/v1", "unused")
            .bind(ordinary.spec.clone())
            .await
            .unwrap();
        service
            .restore_bound(
                ordinary.spec.id,
                fixture.catalog.guard(&ordinary),
                OrdinaryLimits::default(),
                binding,
            )
            .await
            .unwrap();
        assert_eq!(service.owner_count(), 2);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 2);
        assert!(service.mini_client(ordinary.spec.id).is_err());
        assert!(service
            .start_mini(
                third.spec.id,
                fixture.catalog.guard(&third),
                profile(&third.spec.launch.cwd, "http://127.0.0.1:9/v1", "unused"),
                MiniLimits::default()
            )
            .await
            .is_err());
        assert_eq!(service.owner_count(), 2);
        service.close_agent(mini.spec.id).await.unwrap();
        service.close_agent(ordinary.spec.id).await.unwrap();
        assert_eq!(service.owner_count(), 0);
        assert_eq!(
            fixture.catalog.get(mini.spec.id).await.unwrap().state,
            AgentState::Lost
        );
        assert_eq!(
            fixture.catalog.get(ordinary.spec.id).await.unwrap().state,
            AgentState::Closed
        );
        no_session_files(&mini);
        no_session_files(&third);
        service.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_failed_start_keeps_same_residue_until_explicit_close() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-start-failure").await;
        let service = HostService::new(fixture.catalog.clone());
        let mut input = profile(&record.spec.launch.cwd, "http://127.0.0.1:9/v1", "unused");
        input.tool_environment.remove("HOME");
        assert!(service
            .start_mini(
                record.spec.id,
                fixture.catalog.guard(&record),
                input,
                MiniLimits::default()
            )
            .await
            .is_err());
        assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::StartFailed));
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert!(service
            .start_mini(
                record.spec.id,
                fixture.catalog.guard(&record),
                profile(&record.spec.launch.cwd, "http://127.0.0.1:9/v1", "unused"),
                MiniLimits::default()
            )
            .await
            .is_err());
        service.close_agent(record.spec.id).await.unwrap();
        assert_eq!(service.owner_count(), 0);
        assert_eq!(
            fixture.catalog.get(record.spec.id).await.unwrap().state,
            AgentState::Lost
        );
        no_session_files(&record);
        service.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

// Deliberately tiny native fixture: only the approved after-response observer
// and lifecycle slots. Both actual C callback and actual finish are gated.
struct LateDll {
    root: PathBuf,
    library: PathBuf,
    staging: PathBuf,
}
impl LateDll {
    fn new(root: &Path) -> Self {
        let root = root.canonicalize().unwrap();
        let staging = root.join("mini-staging");
        std::fs::create_dir(&staging).unwrap();
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = root.join(if cfg!(target_os = "macos") {
            "mini-late.dylib"
        } else {
            "mini-late.so"
        });
        let source = root.join("mini-late.c");
        std::fs::write(&source, format!(r#"
#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
typedef struct {{ const unsigned char *ptr; size_t len; }} Bytes;
typedef union {{ Bytes data; unsigned char storage[{payload_size}]; }} Payload;
typedef struct {{ uint32_t tag; Payload payload; }} Event;
_Static_assert(sizeof(Event)=={event_size},"event ABI");
_Static_assert(offsetof(Event,payload)=={event_offset},"event payload ABI");
static void mark(char ch) {{ int fd=open({events},O_CREAT|O_APPEND|O_WRONLY,0600); if(fd>=0) {{ (void)write(fd,&ch,1); close(fd); }} }}
static int wait_gate(const char *path) {{ for(int n=0;n<10000;n++) {{ if(access(path,F_OK)==0) return 1; usleep(1000); }} return 0; }}
static int observe(Event event,void *ctx) {{
    (void)ctx; int fd=open({context},O_CREAT|O_WRONLY|O_TRUNC,0600);
    if(fd>=0) {{ (void)write(fd,event.payload.data.ptr,event.payload.data.len); close(fd); }}
    mark('A'); return wait_gate({callback}) ? 0 : 1;
}}
static void begin(void *ctx) {{ (void)ctx; mark('B'); }}
static int finish(void *ctx) {{ (void)ctx; mark('E'); if(!wait_gate({finish})) return 1; mark('F'); return 0; }}
__attribute__((destructor)) static void unloaded(void) {{ mark('U'); }}
int32_t rpi_plugin_register(const unsigned char *api) {{
    typedef int32_t (*Lifecycle)(void(*)(void*),int(*)(void*),void*);
    typedef int32_t (*On)(uint32_t,int(*)(Event,void*),void*);
    Lifecycle lifecycle; On on;
    memcpy(&lifecycle,api+{lifecycle},sizeof lifecycle);
    memcpy(&on,api+{register_event},sizeof on);
    if(lifecycle(begin,finish,NULL) || on({after},observe,NULL)) return 1;
    mark('R'); return 0;
}}
"#, payload_size=std::mem::size_of::<rpi_plugin_sdk::EventPayload>(),
            event_size=std::mem::size_of::<rpi_plugin_sdk::StablePluginEvent>(),
            event_offset=std::mem::offset_of!(rpi_plugin_sdk::StablePluginEvent,payload),
            lifecycle=std::mem::offset_of!(rpi_plugin_sdk::PluginApi,register_plugin_lifecycle),
            register_event=std::mem::offset_of!(rpi_plugin_sdk::PluginApi,register_event_handler),
            after=rpi_plugin_sdk::EventTag::AfterProviderResponse as u32,
            events=json!(root.join("mini-events")), context=json!(root.join("mini-context.json")),
            callback=json!(root.join("mini-callback-gate")), finish=json!(root.join("mini-finish-gate")))).unwrap();
        let compiler = if cfg!(target_os = "macos") {
            "/usr/bin/clang"
        } else {
            "cc"
        };
        let output = std::process::Command::new(compiler)
            .args([
                "-std=c11",
                "-fPIC",
                if cfg!(target_os = "macos") {
                    "-dynamiclib"
                } else {
                    "-shared"
                },
            ])
            .arg(source)
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self {
            root,
            library,
            staging,
        }
    }
    fn events(&self) -> String {
        std::fs::read_to_string(self.root.join("mini-events")).unwrap_or_default()
    }
    async fn marked(&self, ch: char) {
        tokio::time::timeout(DEADLINE, async {
            while !self.events().contains(ch) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
    fn release(&self, gate: &str) {
        std::fs::write(self.root.join(gate), b"release").unwrap();
    }
}
impl Drop for LateDll {
    fn drop(&mut self) {
        // Also bounded in C, so fixture assertion failure cannot strand a worker.
        let _ = std::fs::write(self.root.join("mini-callback-gate"), b"release");
        let _ = std::fs::write(self.root.join("mini-finish-gate"), b"release");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_real_dll_late_cancel_disconnected_close_and_host_drain_retain_custody() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let dll = LateDll::new(fixture.root.path());
        let cwd = fixture.root.path().join("mini-late-workspace");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await; reply(&mut stream).await;
        });
        let mut input = profile(&cwd, &url, "private-mini-key");
        let selection = serde_json::from_value(json!({"version":1,"tools":{"mode":"none"},"extensions":[dll.library]})).unwrap();
        let config = serde_json::from_value(json!({"stagingRoot":dll.staging,"registrations":[{"providerEvents":[{"event":"afterResponse","count":1}]}]})).unwrap();
        let resources = Arc::new(crate::host::NativeBindingResources::freeze(selection, Some(config)).unwrap());
        let mut spec = mini_spec(&cwd);
        spec.launch.resource_profile = Some(resources.identity().into());
        let bound = fixture.catalog.register(spec).await.unwrap();
        input.instruction_resources = Some(resources);
        let service = HostService::new(fixture.catalog.clone());
        let client = service.start_mini(bound.spec.id, fixture.catalog.guard(&bound), input, MiniLimits::default()).await.unwrap();
        let receipt = client.submit(fixture.catalog.epoch(), RequestId::new(), "late {{literal}}".into()).await.unwrap();
        dll.marked('A').await; server.await.unwrap();
        let context: Value = serde_json::from_slice(&std::fs::read(fixture.root.path().join("mini-context.json")).unwrap()).unwrap();
        assert_eq!(context["agentId"], bound.spec.id.to_string());
        assert!(context["sessionId"].is_null());
        assert!(client.cancel(receipt).await.unwrap());
        assert_eq!(client.result(receipt).await.unwrap().unwrap().outcome, MiniOutcome::Running);
        let closing_service = service.clone();
        let closing = tokio::spawn(async move { closing_service.close_agent(bound.spec.id).await });
        while service.phase(bound.spec.id) != Some(OwnerPhase::Closing) { tokio::task::yield_now().await; }
        assert!(!closing.is_finished());
        closing.abort(); assert!(closing.await.unwrap_err().is_cancelled());
        let stopping_service = service.clone();
        let stopping = tokio::spawn(async move { stopping_service.shutdown().await });
        while !service.is_stopping() { tokio::task::yield_now().await; }
        assert!(matches!(client.submit(fixture.catalog.epoch(), RequestId::new(), "stopped".into()).await, Err(BackendError::Closed)));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(service.owner_count(), 1);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert!(!stopping.is_finished());
        assert!(!dll.events().contains('B')); assert!(!dll.events().contains('U'));
        no_session_files(&bound);
        drop(client);
        dll.release("mini-callback-gate"); dll.marked('E').await;
        assert_eq!(service.owner_count(), 1);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert!(!stopping.is_finished()); assert!(!dll.events().contains('U'));
        dll.release("mini-finish-gate");
        stopping.await.unwrap().unwrap();
        assert_eq!(service.owner_count(), 0);
        assert_eq!(dll.events(), "RABEFU");
        assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
        no_session_files(&bound);
        let reopened = HostCatalog::open(fixture.root.path().join("host"), HostConfig::default()).await.unwrap();
        assert_eq!(reopened.get(bound.spec.id).await.unwrap().state, AgentState::Lost);
        reopened.shutdown().await.unwrap();
    }).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_active_cancel_is_aborted_only_after_native_prompt_settlement() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-active-cancel").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await;
            arrived.send(()).unwrap();
            released.await.unwrap();
            drop(stream);
        });
        let service = HostService::new(fixture.catalog.clone());
        let client = start(&service, &record, &url, MiniLimits::default()).await;
        let receipt = client
            .submit(fixture.catalog.epoch(), RequestId::new(), "cancel".into())
            .await
            .unwrap();
        arrival.await.unwrap();
        assert!(client.cancel(receipt).await.unwrap());
        assert_eq!(
            finished(&client, receipt).await.outcome,
            MiniOutcome::Aborted
        );
        assert_eq!(
            fixture.catalog.get(record.spec.id).await.unwrap().state,
            AgentState::Idle
        );
        assert!(!client.cancel(receipt).await.unwrap());
        release.send(()).unwrap();
        server.await.unwrap();
        service.shutdown().await.unwrap();
        no_session_files(&record);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_owner_failed_final_lost_keeps_capacity_and_same_owner_for_close_retry() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = registered(&fixture, "mini-close-retry").await;
        let service = HostService::new(fixture.catalog.clone());
        let client = start(&service, &record, "http://127.0.0.1:9/v1", MiniLimits::default()).await;
        let db = rusqlite::Connection::open(fixture.root.path().join("host/agents.sqlite3")).unwrap();
        db.execute_batch("CREATE TRIGGER refuse_mini_lost BEFORE UPDATE ON agents WHEN OLD.resident=1 AND NEW.resident=0 BEGIN SELECT RAISE(ABORT,'injected final Mini Lost failure'); END;").unwrap();
        assert!(matches!(service.shutdown().await, Err(ServiceError::ShutdownIncomplete { owners: 1 })));
        assert!(!service.is_closed());
        assert_eq!(service.phase(record.spec.id), Some(OwnerPhase::CloseFailed));
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        assert_eq!(fixture.catalog.get(record.spec.id).await.unwrap().state, AgentState::Closing);
        assert!(matches!(client.submit(fixture.catalog.epoch(), RequestId::new(), "not settled".into()).await, Err(BackendError::Closed)));
        db.execute_batch("DROP TRIGGER refuse_mini_lost").unwrap(); drop(db);
        // Retry through the same HostRuntime entry; no reconstruction/replay.
        service.close_agent(record.spec.id).await.unwrap();
        assert_eq!(service.owner_count(), 0);
        assert_eq!(fixture.catalog.get(record.spec.id).await.unwrap().state, AgentState::Lost);
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
        service.shutdown().await.unwrap(); no_session_files(&record);
    }).await.unwrap();
}
