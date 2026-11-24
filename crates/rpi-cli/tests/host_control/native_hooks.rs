//! Actual production profile + binary + C DLL + HTTP. No test binder.
use super::*;
use execution_wire::{AdministrativeAction as A, Command as C};
use std::path::Path;

#[path = "native_close_disconnect_tests.rs"]
mod close_disconnect_tests;
#[path = "native_provider_hook_tests.rs"]
mod provider_tests;

struct HookDll {
    library: PathBuf,
    events: PathBuf,
    callback_gate: PathBuf,
    finish_gate: PathBuf,
    provider_gate: PathBuf,
    after_gate: PathBuf,
    provider_events: bool,
    staging: PathBuf,
}
impl Drop for HookDll {
    fn drop(&mut self) {
        // Unblock only this fixture's native work if a test assertion unwinds.
        let _ = std::fs::write(&self.callback_gate, b"release");
        let _ = std::fs::write(&self.finish_gate, b"release");
        let _ = std::fs::write(&self.provider_gate, b"release");
        let _ = std::fs::write(&self.after_gate, b"release");
    }
}
fn hook_dll(root: &Path, name: &str, status: i32, finish: i32, extra: bool) -> HookDll {
    hook_dll_impl(root, name, status, finish, extra, false)
}
fn provider_dll(root: &Path, name: &str) -> HookDll {
    hook_dll_impl(root, name, 0, 0, false, true)
}
fn hook_dll_impl(
    root: &Path,
    name: &str,
    status: i32,
    finish: i32,
    extra: bool,
    provider_events: bool,
) -> HookDll {
    let root = root.canonicalize().unwrap();
    let staging = root.join(format!("{name}-staging"));
    std::fs::create_dir(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
    let events = root.join(format!("{name}-events"));
    let callback_gate = root.join(format!("{name}-callback"));
    let finish_gate = root.join(format!("{name}-finish"));
    let provider_gate = root.join(format!("{name}-provider"));
    let after_gate = root.join(format!("{name}-after"));
    let source = root.join(format!("{name}.c"));
    let library = root.join(format!(
        "{name}.{}",
        if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        }
    ));
    let lifecycle = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_plugin_lifecycle);
    let result = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_tool_result);
    let declaration = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, declare);
    let register_event = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_event_handler);
    let payload_size = std::mem::size_of::<rpi_plugin_sdk::EventPayload>();
    let event_size = std::mem::size_of::<rpi_plugin_sdk::StablePluginEvent>();
    let event_offset = std::mem::offset_of!(rpi_plugin_sdk::StablePluginEvent, payload);
    let before_tag = rpi_plugin_sdk::EventTag::BeforeProviderRequest as u32;
    let headers_tag = rpi_plugin_sdk::EventTag::BeforeProviderHeaders as u32;
    let after_tag = rpi_plugin_sdk::EventTag::AfterProviderResponse as u32;
    std::fs::write(&source, format!(r#"
#define _DEFAULT_SOURCE
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <pthread.h>
#include <stdatomic.h>
typedef struct {{ const unsigned char *ptr; size_t len; }} Bytes;
typedef union {{ Bytes data; unsigned char storage[{payload_size}]; }} Payload;
typedef struct {{ uint32_t tag; Payload payload; }} Event;
_Static_assert(sizeof(Event) == {event_size}, "stable event size");
_Static_assert(offsetof(Event,payload) == {event_offset}, "stable payload offset");
static pthread_t worker;
static atomic_int cancelled;
static int started;
static void mark(char kind, const unsigned char *text, size_t size) {{
    char *line = malloc(size + 3); line[0] = kind; line[1] = '|';
    if(size) memcpy(line + 2, text, size); line[size + 2] = '\n';
    int fd = open({events}, O_WRONLY|O_APPEND|O_CREAT, 0600);
    if(fd >= 0) {{ if(write(fd, line, size + 3) != (ssize_t)(size + 3)) abort(); close(fd); }}
    free(line);
}}
static void *work(void *unused) {{
    (void)unused;
    while(!atomic_load(&cancelled)) usleep(1000);
    mark('W', NULL, 0); return NULL;
}}
static void begin(void *unused) {{ (void)unused; mark('B',NULL,0); atomic_store(&cancelled,1); }}
static int finish(void *unused) {{
    (void)unused;
    if(pthread_join(worker,NULL)) abort(); mark('E',NULL,0);
    while(access({finish_gate},F_OK)) usleep(1000);
    mark('F',NULL,0); return {finish};
}}
static int transform(Bytes event, Bytes *out, void *unused) {{
    (void)unused; mark('T',event.ptr,event.len);
    while(access({callback_gate},F_OK)) usleep(1000);
    const char *patch = "{{\"content\":[{{\"type\":\"text\",\"text\":\"DLL-TRANSFORMED\"}}],\"details\":{{\"hookApplied\":true}}}}";
    unsigned char *copy = malloc(strlen(patch)); memcpy(copy,patch,strlen(patch));
    out->ptr = copy; out->len = strlen(patch); mark('D',NULL,0); return 0;
}}
static void reclaim(Bytes value) {{ free((void *)value.ptr); mark('S',NULL,0); }}
static int observe(Event event, void *unused) {{
    (void)unused;
    char kind = event.tag == {before_tag} ? 'Q' : event.tag == {headers_tag} ? 'H' : event.tag == {after_tag} ? 'A' : 'X';
    mark(kind,event.payload.data.ptr,event.payload.data.len);
    if(kind == 'Q') {{ while(access({provider_gate},F_OK)) usleep(1000); mark('q',NULL,0); }}
    if(kind == 'A') {{ while(access({after_gate},F_OK)) usleep(1000); mark('a',NULL,0); }}
    return 0;
}}
__attribute__((destructor)) static void unloaded(void) {{ mark('U',NULL,0); }}
int32_t rpi_plugin_register(const unsigned char *api) {{
    if(started) {{ mark('X',NULL,0); return 79; }} started=1;
    typedef int32_t (*Lifecycle)(void(*)(void*),int(*)(void*),void*);
    typedef int32_t (*Result)(int(*)(Bytes,Bytes*,void*),void(*)(Bytes),void*);
    typedef int32_t (*Declare)(Bytes);
    typedef int32_t (*On)(uint32_t,int(*)(Event,void*),void*);
    Lifecycle lifecycle; Result result; Declare declare; On on;
    memcpy(&lifecycle,api+{lifecycle},sizeof lifecycle);
    memcpy(&result,api+{result},sizeof result);
    memcpy(&declare,api+{declaration},sizeof declare);
    memcpy(&on,api+{register_event},sizeof on);
    if(lifecycle(begin,finish,NULL) || result(transform,reclaim,NULL)) abort();
    if({provider_events}) {{ if(on({before_tag},observe,NULL) || on({headers_tag},observe,NULL) || on({after_tag},observe,NULL)) abort(); }}
    if({extra}) {{ const char *text="{{\"priority\":7}}"; Bytes data={{(const unsigned char*)text,strlen(text)}}; if(declare(data)) abort(); }}
    if(pthread_create(&worker,NULL,work,NULL)) abort(); mark('R',NULL,0); return {status};
}}
"#, events=json!(events), callback_gate=json!(callback_gate), finish_gate=json!(finish_gate), provider_gate=json!(provider_gate), after_gate=json!(after_gate), provider_events=i32::from(provider_events), extra=i32::from(extra))).unwrap();
    let mut compiler = std::process::Command::new(if cfg!(target_os = "macos") {
        "/usr/bin/clang"
    } else {
        "cc"
    });
    compiler.args([
        "-std=c11",
        "-pthread",
        "-fPIC",
        if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        },
    ]);
    let output = compiler
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    HookDll {
        library,
        events,
        callback_gate,
        finish_gate,
        provider_gate,
        after_gate,
        provider_events,
        staging,
    }
}
fn hook_profile(profile: &mut Value, dlls: &[&HookDll], url: &str) {
    profile["runtime"]["providers"]["fixture"]["baseUrl"] = json!(url);
    profile["runtime"]["providers"]["fixture"]["models"][0]["contextWindow"] = json!(131072);
    profile["approvedTools"] = json!(["read"]);
    profile["agent"]["tools"] = json!(["read"]);
    profile["providerValues"] = json!({"NO_PROXY":"*"});
    profile["resources"] = json!({"version":1,"tools":{"mode":"only","names":["read"]},
        "extensions":dlls.iter().map(|dll| &dll.library).collect::<Vec<_>>()});
    profile["nativeHooks"] = json!({"stagingRoot":dlls[0].staging,
        "registrations":dlls.iter().map(|dll| if dll.provider_events { json!({"toolResult":1,"providerEvents":[
            {"event":"afterResponse","count":1},{"event":"beforeRequest","count":1},{"event":"beforeHeaders","count":1}
        ]}) } else { json!({"toolResult":1}) }).collect::<Vec<_>>()});
}
fn hook_lines(dll: &HookDll) -> Vec<String> {
    std::fs::read_to_string(&dll.events)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
async fn hook_mark(dll: &HookDll, kind: char, count: usize) {
    tokio::time::timeout(DEADLINE, async {
        while hook_lines(dll)
            .iter()
            .filter(|line| line.starts_with(&format!("{kind}|")))
            .count()
            < count
        {
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .unwrap();
}
fn hook_unloaded(dll: &HookDll, count: usize) {
    for kind in ['R', 'B', 'W', 'E', 'F', 'U'] {
        assert_eq!(
            hook_lines(dll)
                .iter()
                .filter(|line| line.starts_with(&format!("{kind}|")))
                .count(),
            count,
            "{kind}: {:?}",
            hook_lines(dll)
        );
    }
    assert!(hook_lines(dll).iter().all(|line| !line.starts_with("X|")));
    assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
}
fn hook_tool_completion() -> String {
    [
        json!({"id":"hook-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"same-call","type":"function","function":{"name":"read","arguments":"{\"path\":\"input.txt\"}"}}]},"finish_reason":null}]}),
        json!({"id":"hook-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ].into_iter().map(|part|format!("data: {part}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}
async fn hook_http_reply(stream: &mut tokio::net::TcpStream, reply: &str) {
    use tokio::io::AsyncWriteExt;
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}
async fn hook_finished(
    client: &mut rpi_cli::host::client::HostClient,
    agent_id: execution_wire::AgentId,
    request_id: execution_wire::RequestId,
) -> Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let receipt = client
                .call(C::Receipt {
                    agent_id,
                    request_id,
                })
                .await
                .unwrap();
            if receipt["execution"]["finished"] == true {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn production_hook_profiles_freeze_without_loading_and_reject_ungranted_shapes() {
    let fixture = Fixture::new();
    let dll = hook_dll(fixture.root.path(), "pure", 0, 0, false);
    let (_, _, mut profile) = administrative_profile(&fixture);
    hook_profile(&mut profile, &[&dll], "http://127.0.0.1:9/v1");
    let startup = serde_json::from_value::<rpi_cli::host::AgentProfileFile>(profile.clone())
        .unwrap()
        .into_startup()
        .unwrap();
    assert!(startup
        .spec
        .launch
        .resource_profile
        .unwrap()
        .starts_with("native-hooks-v1:"));
    assert!(!dll.events.exists());
    assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
    for case in [
        "missing-approval",
        "missing-resources",
        "extra-approval",
        "provider-event",
        "provider-zero",
        "provider-budget",
        "provider-duplicate",
        "provider-unknown-field",
        "budget",
        "relative-root",
        "duplicate-dll",
    ] {
        let mut bad = profile.clone();
        match case {
            "missing-approval" => {
                bad.as_object_mut().unwrap().remove("nativeHooks");
            }
            "missing-resources" => {
                bad.as_object_mut().unwrap().remove("resources");
            }
            "extra-approval" => {
                bad["nativeHooks"]["registrations"] = json!([{"toolResult":1},{"toolResult":1}])
            }
            "provider-event" => {
                bad["nativeHooks"]["registrations"][0]["providerEvents"] =
                    json!([{"event":"beforeAgentStart","count":1}])
            }
            "provider-zero" => {
                bad["nativeHooks"]["registrations"][0]["providerEvents"] =
                    json!([{"event":"beforeRequest","count":0}])
            }
            "provider-budget" => {
                bad["nativeHooks"]["registrations"][0]["providerEvents"] =
                    json!([{"event":"afterResponse","count":17}])
            }
            "provider-duplicate" => {
                bad["nativeHooks"]["registrations"][0]["providerEvents"] =
                    json!([{"event":"beforeRequest","count":1},{"event":"beforeRequest","count":1}])
            }
            "provider-unknown-field" => {
                bad["nativeHooks"]["registrations"][0]["providerEvents"] =
                    json!([{"event":"beforeRequest","count":1,"other":true}])
            }
            "budget" => bad["nativeHooks"]["registrations"][0]["toolResult"] = json!(17),
            "relative-root" => bad["nativeHooks"]["stagingRoot"] = json!("relative"),
            "duplicate-dll" => {
                bad["resources"]["extensions"] = json!([dll.library, dll.library]);
                bad["nativeHooks"]["registrations"] = json!([{"toolResult":1},{"toolResult":1}]);
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<rpi_cli::host::AgentProfileFile>(bad)
                .ok()
                .and_then(|profile| profile.into_startup().ok())
                .is_none(),
            "{case}"
        );
        assert!(!dll.events.exists(), "{case} ran native registration");
        assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
    }
    assert!(!fixture.root.path().join("dynamic.jsonl").exists());
    assert!(!fixture.directory().exists());
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_native_hooks_transform_http_and_isolate_two_agents_then_reopen_without_replay(
) {
    let mut fixture = Fixture::new();
    let dll = provider_dll(fixture.root.path(), "shared");
    std::fs::write(&dll.callback_gate, b"release").unwrap();
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    std::fs::write(&dll.provider_gate, b"release").unwrap();
    std::fs::write(&dll.after_gate, b"release").unwrap();
    let original_dll = std::fs::read(&dll.library).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut profiles = Vec::new();
    let mut agents = Vec::new();
    let mut sessions = Vec::new();
    for index in 0..2 {
        let (agent, _, mut profile) = administrative_profile(&fixture);
        let cwd = fixture
            .root
            .path()
            .canonicalize()
            .unwrap()
            .join(format!("agent-{index}"));
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("input.txt"), format!("original-agent-{index}")).unwrap();
        let session = cwd.join("session.jsonl");
        let session_id = format!("session-{index}");
        profile["agent"]["cwd"] = json!(cwd);
        profile["agent"]["session"] = json!({"id":session_id,"path":session});
        profile["toolEnvironment"]["HOME"] = json!(cwd);
        hook_profile(&mut profile, &[&dll], &url);
        let path = fixture.root.path().join(format!("profile-{index}.json"));
        std::fs::write(&path, profile.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        profiles.push(path);
        agents.push(agent);
        sessions.push((session, session_id, cwd));
    }
    fixture.value["agents"] = json!(profiles);
    fixture.save();
    let (child, _) = fixture.start().await;
    hook_mark(&dll, 'R', 2).await;
    // Both owners already froze bytes, never load this replacement.
    std::fs::write(&dll.library, b"replacement-must-not-load").unwrap();
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    let provider = tokio::spawn(async move {
        let mut workers = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
                .await
                .unwrap()
                .unwrap();
            workers.push(tokio::spawn(async move {
                let request = execution_http_request(&mut stream).await;
                let body: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let transformed = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool");
                if transformed {
                    assert!(body.to_string().contains("DLL-TRANSFORMED"));
                    assert!(!body.to_string().contains("original-agent-"));
                    hook_http_reply(&mut stream, &sse_completion()).await;
                } else {
                    hook_http_reply(&mut stream, &hook_tool_completion()).await;
                }
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
        tokio::select! {
            _ = listener.accept() => panic!("duplicate/reopen replayed a Provider request"),
            result = finished => result.unwrap(),
        }
    });
    let mut client = administrative_client(&fixture).await;
    let mut requests = Vec::new();
    for agent in &agents {
        let agent_id = serde_json::from_value(json!(agent)).unwrap();
        let request_id = execution_wire::RequestId::new();
        let command = C::Submit {
            agent_id,
            request_id,
            prompt: "use the real native hook".into(),
        };
        let admitted = client.call(command.clone()).await.unwrap();
        assert_eq!(admitted["durable"], true);
        requests.push((agent_id, request_id, command, admitted));
    }
    for (agent, request, _, _) in &requests {
        hook_finished(&mut client, *agent, *request).await;
    }
    hook_mark(&dll, 'S', 2).await;
    let envelopes: Vec<Value> = hook_lines(&dll)
        .iter()
        .filter_map(|line| {
            line.strip_prefix("T|")
                .map(|value| serde_json::from_str(value).unwrap())
        })
        .collect();
    assert_eq!(envelopes.len(), 2);
    for (index, agent) in agents.iter().enumerate() {
        let context = json!({"agentId":agent.to_string(),"sessionId":sessions[index].1,"cwd":sessions[index].2});
        let envelope = envelopes
            .iter()
            .find(|envelope| envelope["__rpi"] == context)
            .unwrap();
        assert_eq!(envelope["toolCallId"], "same-call");
        assert_eq!(envelope["input"], json!({"path":"input.txt"}));
        let text = std::fs::read_to_string(&sessions[index].0).unwrap();
        assert!(text.contains("DLL-TRANSFORMED"));
        assert!(text.contains("hookRegistrations"));
        assert!(!text.contains("administration-test-private-key"));
    }
    // Provider event payloads are decorated by the existing SDK bridge with
    // each immutable binding, never process environment or another Agent.
    for kind in ['Q', 'H', 'A'] {
        let events: Vec<Value> = hook_lines(&dll)
            .iter()
            .filter_map(|line| {
                line.strip_prefix(&format!("{kind}|"))
                    .map(|value| serde_json::from_str(value).unwrap())
            })
            .collect();
        assert_eq!(events.len(), 4, "{kind}: {events:?}");
        for (index, agent) in agents.iter().enumerate() {
            let own: Vec<_> = events
                .iter()
                .filter(|event| event["agentId"] == json!(agent.to_string()))
                .collect();
            assert_eq!(own.len(), 2, "{kind}: missing Agent identity");
            assert!(own
                .iter()
                .all(|event| event["sessionId"] == json!(sessions[index].1)));
        }
        if kind == 'A' {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["stopReason"] == json!(rpi_ai::StopReason::ToolUse))
                    .count(),
                2
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["stopReason"] == "stop")
                    .count(),
                2
            );
        }
    }
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    hook_unloaded(&dll, 2);
    std::fs::write(&dll.library, original_dll).unwrap();
    let saved = std::fs::read(&sessions[0].0).unwrap();
    let mut rows: Vec<Value> = std::str::from_utf8(&saved)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let row = rows
        .iter_mut()
        .find(|row| row.pointer("/data/key") == Some(&json!("rpi.resource-selection.v1")))
        .unwrap();
    row["data"]["value"]["inventory"]["hookRegistrations"][0]["toolResult"] = json!(42);
    let corrupt = rows
        .into_iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    std::fs::write(&sessions[0].0, &corrupt).unwrap();
    let rejected = fixture.call("serve", &[]).await;
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("host_ready"));
    assert_eq!(std::fs::read(&sessions[0].0).unwrap(), corrupt.as_bytes());
    assert_eq!(
        hook_lines(&dll)
            .iter()
            .filter(|line| line.starts_with("R|"))
            .count(),
        2,
        "saved inventory was not rejected before native registration"
    );
    assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
    std::fs::write(&sessions[0].0, saved).unwrap();
    let before: Vec<_> = sessions
        .iter()
        .map(|(path, _, _)| std::fs::read(path).unwrap())
        .collect();
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    for (_, _, command, admitted) in requests {
        assert_eq!(client.call(command).await.unwrap(), admitted);
    }
    for (index, (path, _, _)) in sessions.iter().enumerate() {
        assert_eq!(std::fs::read(path).unwrap(), before[index]);
    }
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    hook_unloaded(&dll, 4);
    done.send(()).unwrap();
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    fixture.no_resources();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_native_hook_batch_rejection_settles_all_registered_workers_without_dispatch()
{
    for case in [
        "registration-error",
        "unsupported-metadata",
        "wrong-count",
        "unapproved-provider-event",
        "wrong-provider-count",
    ] {
        let mut fixture = Fixture::new();
        let good = hook_dll(fixture.root.path(), "good", 0, 0, false);
        let bad = hook_dll_impl(
            fixture.root.path(),
            "bad",
            if case == "registration-error" { 71 } else { 0 },
            0,
            case == "unsupported-metadata",
            case == "unapproved-provider-event" || case == "wrong-provider-count",
        );
        for dll in [&good, &bad] {
            std::fs::write(&dll.finish_gate, b"release").unwrap();
        }
        let (agent, path, mut profile) = administrative_profile(&fixture);
        hook_profile(&mut profile, &[&good, &bad], "http://127.0.0.1:9/v1");
        if case == "wrong-count" {
            profile["nativeHooks"]["registrations"][1]["toolResult"] = json!(0);
        }
        if case == "unapproved-provider-event" {
            profile["nativeHooks"]["registrations"][1]
                .as_object_mut()
                .unwrap()
                .remove("providerEvents");
        }
        if case == "wrong-provider-count" {
            profile["nativeHooks"]["registrations"][1]["providerEvents"][0]["count"] = json!(2);
        }
        std::fs::write(&path, profile.to_string()).unwrap();
        let scope = execution_wire::ControlScopeId::new();
        fixture.value["administration"] = json!({"scope":scope,"profiles":[path],"allowed_agents":[agent],"permissions":["create"]});
        fixture.save();
        let (child, _) = fixture.start().await;
        // Merely publishing a trusted profile never loads native code. Replacing
        // its source after freeze must not change the subsequent owned batch.
        assert!(!good.events.exists() && !bad.events.exists());
        std::fs::write(&good.library, b"replacement-after-profile-freeze").unwrap();
        let mut client = administrative_client(&fixture).await;
        let profiles = client
            .call(C::Profiles {
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        let reference = serde_json::from_value(profiles["profiles"][0].clone()).unwrap();
        let intent = execution_intent(&client, scope, A::CreateProfile { profile: reference });
        let receipt = client
            .call(execution_command(&intent))
            .await
            .unwrap_or_else(|error| panic!("{case}: {error:?}"));
        assert_eq!(
            receipt["receipt"]["status"]["state"], "uncertain",
            "{case}: {receipt}"
        );
        for dll in [&good, &bad] {
            hook_mark(dll, 'U', 1).await;
            assert!(!hook_lines(dll).iter().any(|line| ['T', 'Q', 'H', 'A']
                .iter()
                .any(|kind| line.starts_with(&format!("{kind}|")))));
        }
        hook_unloaded(&good, 1);
        // Both DLLs stage beneath the first approved root, not their own roots.
        hook_unloaded(&bad, 1);
        let status = client.call(C::Status {}).await.unwrap();
        assert_eq!(status["resident"], 0, "{case}: {status}");
        assert_eq!(status["owners"], 0, "{case}: {status}");
        assert_eq!(
            client.call(execution_command(&intent)).await.unwrap(),
            receipt
        );
        assert_eq!(
            hook_lines(&good)
                .iter()
                .filter(|line| line.starts_with("R|"))
                .count(),
            1
        );
        drop(client);
        fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_native_late_tool_result_and_finish_hold_owner_writer_and_close_receipt() {
    let mut fixture = Fixture::new();
    let dll = hook_dll(fixture.root.path(), "late", 0, 0, false);
    let (native_agent, path, mut profile) = administrative_profile(&fixture);
    let agent = serde_json::from_value(json!(native_agent)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    hook_profile(
        &mut profile,
        &[&dll],
        &format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    std::fs::write(fixture.root.path().join("input.txt"), b"late-input").unwrap();
    std::fs::write(&path, profile.to_string()).unwrap();
    let scope = execution_wire::ControlScopeId::new();
    fixture.value["agents"] = json!([path]);
    fixture.value["administration"] =
        json!({"scope":scope,"allowed_agents":[agent],"permissions":["close"]});
    fixture.save();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        execution_http_request(&mut stream).await;
        hook_http_reply(&mut stream, &hook_tool_completion()).await;
        listener
    });
    let (child, _) = fixture.start().await;
    let mut client = administrative_client(&fixture).await;
    client
        .call(C::Submit {
            agent_id: agent,
            request_id: execution_wire::RequestId::new(),
            prompt: "late-result".into(),
        })
        .await
        .unwrap();
    hook_mark(&dll, 'T', 1).await;
    let revision = execution_revision(&mut client, agent).await;
    let intent = execution_intent(
        &client,
        scope,
        A::Close {
            agent_id: agent,
            revision,
        },
    );
    let mut closer = administrative_client(&fixture).await;
    let command = execution_command(&intent);
    let closing = tokio::spawn(async move { closer.call(command).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!closing.is_finished());
    assert!(!hook_lines(&dll)
        .iter()
        .any(|line| line.starts_with("U|") || line.starts_with("B|")));
    let session = fixture.root.path().join("dynamic.jsonl");
    let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
    assert!(
        rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
            .await
            .is_err()
    );
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 1);
    std::fs::write(&dll.callback_gate, b"release").unwrap();
    hook_mark(&dll, 'E', 1).await;
    assert!(!closing.is_finished());
    assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("U|")));
    assert!(
        rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
            .await
            .is_err()
    );
    let status = client.call(C::Status {}).await.unwrap();
    assert_eq!(status["owners"], 1);
    assert_eq!(status["resident"], 1);
    std::fs::write(&dll.finish_gate, b"release").unwrap();
    let receipt = tokio::time::timeout(DEADLINE, closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    execution_succeeded(&receipt);
    hook_unloaded(&dll, 1);
    assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 0);
    let writer = rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    drop(writer);
    assert_eq!(
        client.call(execution_command(&intent)).await.unwrap(),
        receipt
    );
    let listener = provider.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "close replayed/continued the cancelled request"
    );
    drop(client);
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_native_retained_cleanup_never_reports_freed_owner_or_unmaps_code() {
    for register_status in [0, 71] {
        let mut fixture = Fixture::new();
        let dll = hook_dll(fixture.root.path(), "retained", register_status, 23, false);
        std::fs::write(&dll.finish_gate, b"release").unwrap();
        let (native_agent, path, mut profile) = administrative_profile(&fixture);
        let agent = serde_json::from_value(json!(native_agent)).unwrap();
        hook_profile(&mut profile, &[&dll], "http://127.0.0.1:9/v1");
        std::fs::write(&path, profile.to_string()).unwrap();
        let scope = execution_wire::ControlScopeId::new();
        fixture.value["administration"] = json!({"scope":scope,"profiles":[path],"allowed_agents":[agent],"permissions":["create","close"]});
        fixture.save();
        let (mut child, _) = fixture.start().await;
        let mut client = administrative_client(&fixture).await;
        let profiles = client
            .call(C::Profiles {
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        let reference = serde_json::from_value(profiles["profiles"][0].clone()).unwrap();
        let create = execution_intent(&client, scope, A::CreateProfile { profile: reference });
        let created = client.call(execution_command(&create)).await.unwrap();
        let (intent, receipt) = if register_status == 0 {
            execution_succeeded(&created);
            let revision = execution_revision(&mut client, agent).await;
            let close = execution_intent(
                &client,
                scope,
                A::Close {
                    agent_id: agent,
                    revision,
                },
            );
            let receipt = client.call(execution_command(&close)).await.unwrap();
            (close, receipt)
        } else {
            (create, created)
        };
        assert_eq!(receipt["receipt"]["status"]["state"], "uncertain");
        hook_mark(&dll, 'F', 1).await;
        assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("U|")));
        assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 1);
        let status = client.call(C::Status {}).await.unwrap();
        assert_eq!(status["resident"], 1);
        assert_eq!(status["owners"], 1);
        assert!(child.try_wait().unwrap().is_none());
        if register_status == 0 {
            let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
            let session = fixture.root.path().join("dynamic.jsonl");
            assert!(rpi_tools::FileSystem::try_acquire_writer_lease(
                &env,
                session.to_str().unwrap()
            )
            .await
            .is_err());
        }
        assert_eq!(
            client.call(execution_command(&intent)).await.unwrap(),
            receipt
        );
        assert_eq!(
            hook_lines(&dll)
                .iter()
                .filter(|line| line.starts_with("R|"))
                .count(),
            1
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        tokio::time::timeout(DEADLINE, async {
            while client.call(C::Status {}).await.unwrap()["stopping"] != true {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "retained DLL was reported clean"
        );
        assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 1);
        assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("U|")));
        drop(client);
        // This intentionally uncooperative fixture cannot produce a safe release
        // proof. Explicitly reap only our private child; this is not a successful
        // Host cleanup receipt or a production retry/recovery strategy.
        child.kill().await.unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }
}
