use super::*;
use crate::host::{
    ordinary::{OrdinaryLimits, OrdinaryOperation},
    service::{HostService, ServiceError},
    AgentId, HostCatalog, HostConfig, LaunchSpec, ModelRef, NativeExtensionCustody, SessionRef,
    TimeoutOverrides,
};
use rpi_harness::{
    agent_harness::HarnessRunOutcome,
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, Session, SessionValueWriter, SystemClock,
    },
};
use serde_json::{json, Value};
use std::{num::NonZeroUsize, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEADLINE: Duration = Duration::from_secs(15);
#[path = "mini_owner_tests.rs"]
mod mini_owner_tests;
struct Fixture {
    root: tempfile::TempDir,
    catalog: HostCatalog,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = HostCatalog::open(
            root.path().join("host"),
            HostConfig {
                max_agents: NonZeroUsize::new(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        Self { root, catalog }
    }
    async fn record(
        &self,
        name: &str,
        workspace: &str,
        tools: &[&str],
        value: Option<(&str, Value)>,
    ) -> crate::host::AgentRecord {
        let cwd = self.root.path().join(workspace);
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let path = cwd.join(format!("{name}.jsonl"));
        let storage = JsonlSessionStorage::create(
            Arc::new(OsExecutionEnv::with_cwd(cwd.clone())),
            path.to_str().unwrap(),
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
        if let Some((key, value)) = value {
            SessionValueWriter::new(Session::new(Arc::new(storage.clone()), None))
                .set(key, value)
                .await
                .unwrap();
        }
        storage.drain().await;
        storage.release_writer_lease();
        self.catalog
            .register(AgentSpec {
                id: AgentId::new(),
                name: name.into(),
                parent_id: None,
                mode: AgentMode::Persistent,
                execution: ExecutionMode::InProcess,
                session: Some(SessionRef {
                    id: name.into(),
                    path,
                }),
                launch: LaunchSpec {
                    cwd,
                    model: Some(ModelRef {
                        provider: "fixture".into(),
                        model: "host-fixture".into(),
                    }),
                    tools: tools.iter().map(|s| s.to_string()).collect(),
                    resource_profile: None,
                },
                timeouts: TimeoutOverrides::default(),
            })
            .await
            .unwrap()
    }
}
fn profile(cwd: &Path, base_url: &str, key: &str) -> NativeBindingProfile {
    NativeBindingProfile {
        runtime: RuntimeConfig {
            models: serde_json::from_value(json!({"providers":{"fixture":{
                "api":"openai-completions", "baseUrl":base_url, "apiKey":"$BINDING_KEY",
                "models":[{"id":"host-fixture","contextWindow":32768,"maxTokens":1024}]
            }}}))
            .unwrap(),
            ..Default::default()
        },
        provider_values: BTreeMap::from([
            ("BINDING_KEY".into(), key.into()),
            ("NO_PROXY".into(), "*".into()),
        ]),
        tool_environment: BTreeMap::from([
            ("HOME".into(), cwd.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("SCOPE".into(), key.replace("private-key-", "agent-")),
        ]),
        shell_path: "/bin/sh".into(),
        tools: NativeToolPolicy::Preapproved(
            ["bash", "read", "write", "edit", "docs"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        thinking: rpi_ai::ThinkingLevel::Off,
        stream_options: AgentHarnessStreamOptions {
            timeout: Some(DEADLINE),
            ..Default::default()
        },
        instruction_resources: None,
    }
}
enum RejectionStage {
    Preflight,
    Facts,
}
async fn rejected(
    fixture: &Fixture,
    record: &crate::host::AgentRecord,
    profile: NativeBindingProfile,
    stage: RejectionStage,
) {
    let path = &record.spec.session.as_ref().unwrap().path;
    // Valid restore preparation can observe the torn suffix, but must not
    // acquire its writer or repair it before this profile accepts its facts.
    let mut bytes = tokio::fs::read(path).await.unwrap();
    bytes.extend_from_slice(b"{torn");
    tokio::fs::write(path, &bytes).await.unwrap();
    let service = HostService::new(fixture.catalog.clone());
    let binding = profile.bind(record.spec.clone()).await;
    match stage {
        RejectionStage::Preflight => {
            assert!(matches!(binding, Err(BackendError::Binding(_))));
            assert_eq!(
                fixture.catalog.get(record.spec.id).await.unwrap().revision,
                record.revision
            );
        }
        RejectionStage::Facts => {
            assert!(matches!(
                service
                    .restore_bound(
                        record.spec.id,
                        fixture.catalog.guard(record),
                        OrdinaryLimits::default(),
                        binding.unwrap()
                    )
                    .await,
                Err(ServiceError::Backend(BackendError::Binding(_)))
            ));
        }
    }
    assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
    let env = OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone());
    assert!(
        rpi_tools::FileSystem::try_acquire_writer_lease(&env, path.to_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(service.owner_count(), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_policy_does_not_grant_tools_or_ignore_unsupported_resource_profiles() {
    let fixture = Fixture::new().await;
    let record = fixture.record("policy", "workspace", &["bash"], None).await;
    let mut input = profile(
        &record.spec.launch.cwd,
        "http://127.0.0.1:9/v1",
        "private-key-policy",
    );
    input.tools = NativeToolPolicy::DenyAll;
    assert!(matches!(
        input.bind(record.spec.clone()).await,
        Err(BackendError::Binding(_))
    ));
    let mut changed = record.spec.clone();
    changed.launch.resource_profile = Some("requires-real-resource-binding".into());
    assert!(matches!(
        profile(
            &changed.launch.cwd,
            "http://127.0.0.1:9/v1",
            "private-key-policy"
        )
        .bind(changed)
        .await,
        Err(BackendError::Binding(_))
    ));
    let mut unknown = record.spec.clone();
    unknown.launch.tools = vec!["unimplemented".into()];
    let mut input = profile(
        &unknown.launch.cwd,
        "http://127.0.0.1:9/v1",
        "private-key-policy",
    );
    input.tools = NativeToolPolicy::Preapproved(["unimplemented".into()].into_iter().collect());
    assert!(matches!(
        input.bind(unknown).await,
        Err(BackendError::Binding(_))
    ));
    assert_eq!(
        fixture.catalog.get(record.spec.id).await.unwrap().revision,
        record.revision
    );
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_binding_refuses_saved_resource_authority_before_writer_and_repair() {
    for key in [
        "rpi.resource-selection.v1",
        "rpi.resource-selection.future",
        "rpi.resource-inventory.future",
    ] {
        let fixture = Fixture::new().await;
        let record = fixture
            .record(
                "saved-resources",
                "workspace",
                &[],
                Some((key, json!({"version":"invalid-saved-declaration"}))),
            )
            .await;
        let input = profile(
            &record.spec.launch.cwd,
            "http://127.0.0.1:9/v1",
            "private-key-resource",
        );
        rejected(&fixture, &record, input, RejectionStage::Facts).await;
    }
}

#[tokio::test]
async fn native_binding_refuses_bad_proxy_without_logging_secret_config_or_repairing() {
    let fixture = Fixture::new().await;
    let record = fixture.record("bad-proxy", "workspace", &[], None).await;
    let mut input = profile(
        &record.spec.launch.cwd,
        "http://127.0.0.1:9/v1",
        "private-key-proxy",
    );
    input.provider_values.insert("NO_PROXY".into(), "".into());
    input.provider_values.insert(
        "HTTP_PROXY".into(),
        "socks5://private-proxy-secret.invalid".into(),
    );
    rejected(&fixture, &record, input, RejectionStage::Preflight).await;
}

#[tokio::test]
async fn prepared_binding_cannot_be_transferred_to_another_agent_in_the_same_workspace() {
    let fixture = Fixture::new().await;
    let first = fixture.record("first", "same-workspace", &[], None).await;
    let second = fixture.record("second", "same-workspace", &[], None).await;
    let path = &second.spec.session.as_ref().unwrap().path;
    let before = tokio::fs::read(path).await.unwrap();
    let binding = profile(
        &first.spec.launch.cwd,
        "http://127.0.0.1:9/v1",
        "private-key-scope",
    )
    .bind(first.spec)
    .await
    .unwrap();
    let service = HostService::new(fixture.catalog.clone());
    assert!(matches!(
        service
            .restore_bound(
                second.spec.id,
                fixture.catalog.guard(&second),
                OrdinaryLimits::default(),
                binding
            )
            .await,
        Err(ServiceError::Backend(BackendError::Binding(_)))
    ));
    assert_eq!(tokio::fs::read(path).await.unwrap(), before);
    service.shutdown().await.unwrap();
}

fn completion(tool: Option<Value>) -> String {
    let (delta, finish) = match tool {
        Some(tool) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"local-tool","type":"function","function":{"name":tool["name"],"arguments":tool["arguments"].to_string()}}]}),
            "tool_calls",
        ),
        None => (
            json!({"role":"assistant","content":"native-binding-finished"}),
            "stop",
        ),
    };
    [json!({"id":"fixture-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"fixture-reply","object":"chat.completion.chunk","created":1,"model":"host-fixture","choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})]
        .into_iter().map(|v| format!("data: {v}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}
async fn request(stream: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 1024 * 1024);
        if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let size: usize = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim().parse().unwrap())
        })
        .unwrap();
    assert!(size <= 1024 * 1024);
    while bytes.len() < header_end + size {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        headers,
        serde_json::from_slice(&bytes[header_end..header_end + size]).unwrap(),
    )
}

fn mini_spec(cwd: &Path) -> AgentSpec {
    AgentSpec {
        id: AgentId::new(),
        name: "raw-mini".into(),
        parent_id: None,
        mode: AgentMode::Mini,
        execution: ExecutionMode::InProcess,
        session: None,
        launch: LaunchSpec {
            cwd: cwd.to_path_buf(),
            model: Some(ModelRef {
                provider: "fixture".into(),
                model: "host-fixture".into(),
            }),
            tools: vec![],
            resource_profile: None,
        },
        timeouts: TimeoutOverrides::default(),
    }
}

#[tokio::test]
async fn native_mini_binder_rejects_wrong_mode_session_authority_and_missing_inputs() {
    let fixture = Fixture::new().await;
    let cwd = fixture.root.path().join("rejected-mini");
    tokio::fs::create_dir(&cwd).await.unwrap();
    for case in [
        "persistent",
        "session",
        "child",
        "resource",
        "tool",
        "unknown-tool",
        "key",
        "home",
    ] {
        let mut spec = mini_spec(&cwd);
        let mut input = profile(&cwd, "http://127.0.0.1:9/v1", "private-key-mini");
        match case {
            "persistent" => {
                spec.mode = AgentMode::Persistent;
                spec.session = Some(SessionRef {
                    id: "forbidden".into(),
                    path: cwd.join("forbidden.jsonl"),
                });
            }
            "session" => {
                spec.session = Some(SessionRef {
                    id: "forbidden".into(),
                    path: cwd.join("forbidden.jsonl"),
                })
            }
            "child" => spec.execution = ExecutionMode::ChildProcess,
            "resource" => spec.launch.resource_profile = Some("not-frozen".into()),
            "tool" => {
                spec.launch.tools = vec!["bash".into()];
                input.tools = NativeToolPolicy::DenyAll;
            }
            "unknown-tool" => {
                spec.launch.tools = vec!["not-implemented".into()];
                input.tools = NativeToolPolicy::Preapproved(["not-implemented".into()].into());
            }
            "key" => {
                input.provider_values.remove("BINDING_KEY");
            }
            "home" => {
                input.tool_environment.remove("HOME");
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                input
                    .bind_mini_agent(spec, fixture.catalog.clone(), NativeExtensionCustody::new())
                    .await,
                Err(BackendError::Binding(_))
            ),
            "{case}"
        );
    }
    assert_eq!(fixture.catalog.status().await.unwrap().registered, 0);
    assert!(tokio::fs::read_dir(&cwd)
        .await
        .unwrap()
        .next_entry()
        .await
        .unwrap()
        .is_none());
    fixture.catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn native_mini_binder_runs_raw_agent_without_session_or_workspace_files() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let cwd = fixture.root.path().join("mini-workspace");
        tokio::fs::create_dir(&cwd).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = request(&mut stream).await;
            assert!(headers.contains("Bearer private-key-mini"));
            assert!(body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user" && message["content"].as_str() == Some("hello {{literal}}")
            }));
            let reply = completion(None);
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            ).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let spec = mini_spec(&cwd);
        let record = fixture.catalog.register(spec.clone()).await.unwrap();
        assert!(record.spec.session.is_none());
        let owner = profile(&cwd, &format!("{url}/v1"), "private-key-mini")
            .bind_mini_agent(spec, fixture.catalog.clone(), NativeExtensionCustody::new())
            .await
            .unwrap();
        owner.agent().prompt("hello {{literal}}").await.unwrap();
        server.await.unwrap();
        let state = owner.agent().state();
        assert_eq!(state.messages.len(), 2);
        assert!(serde_json::to_value(&state.messages[0]).unwrap()["content"] == "hello {{literal}}");
        assert_eq!(fixture.catalog.status().await.unwrap().mini, 1);
        let mut entries = tokio::fs::read_dir(&cwd).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
        owner.close().await.unwrap();
        fixture.catalog.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn native_mini_binder_roundtrips_granted_tool_and_frozen_prompt_over_http() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let cwd = fixture.root.path().join("mini-tool-workspace");
        tokio::fs::create_dir(&cwd).await.unwrap();
        let instruction = fixture.root.path().join("mini-instructions.md");
        tokio::fs::write(&instruction, "frozen mini {{instruction}}").await.unwrap();
        let resources = Arc::new(crate::host::NativeBindingResources::freeze(
            serde_json::from_value(json!({"version": 1, "tools": {"mode": "only", "names": ["bash"]}, "instructions": [instruction]})).unwrap(),
            None,
        ).unwrap());
        tokio::fs::write(&instruction, "changed source must not leak").await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for turn in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = request(&mut stream).await;
                assert!(headers.contains("Bearer private-key-mini"));
                assert_eq!(body["tools"].as_array().unwrap().len(), 1);
                let messages = body["messages"].as_array().unwrap();
                assert!(messages.iter().any(|message| message["role"] == "system"
                    && message["content"].as_str().unwrap().contains("frozen mini {{instruction}}")));
                assert!(!body.to_string().contains("changed source must not leak"));
                let reply = if turn == 0 {
                    completion(Some(json!({"name": "bash", "arguments": {
                        "command": "printf 'mini {{literal}} scope=%s key=%s' \"$SCOPE\" \"$BINDING_KEY\""
                    }})))
                } else {
                    let result = messages.iter().find(|message| message["role"] == "tool").unwrap();
                    let text = result["content"].to_string();
                    assert!(text.contains("mini {{literal}} scope=agent-mini key="));
                    assert!(!text.contains("private-key"));
                    completion(None)
                };
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                ).as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let mut spec = mini_spec(&cwd);
        spec.launch.tools = vec!["bash".into()];
        spec.launch.resource_profile = Some(resources.identity().into());
        let mut input = profile(&cwd, &format!("{url}/v1"), "private-key-mini");
        input.instruction_resources = Some(resources);
        let owner = input.bind_mini_agent(spec, fixture.catalog.clone(), NativeExtensionCustody::new()).await.unwrap();
        owner.agent().prompt("run {{literal}}").await.unwrap();
        server.await.unwrap();
        assert_eq!(owner.agent().state().messages.len(), 4);
        assert!(tokio::fs::read_dir(&cwd).await.unwrap().next_entry().await.unwrap().is_none());
        owner.close().await.unwrap();
        fixture.catalog.shutdown().await.unwrap();
    }).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn real_http_providers_isolate_keys_and_proxy_while_builtin_shells_overlap_in_two_workspaces()
{
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let service = HostService::new(fixture.catalog.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let records = [fixture.record("one", "one", &["bash"], None).await, fixture.record("two", "two", &["bash"], None).await];
        for (i, record) in records.iter().enumerate() {
            tokio::fs::write(record.spec.launch.cwd.join("identity.txt"), format!("home-agent-{}",i+1)).await.unwrap();
        }
        let server = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                workers.spawn(async move {
                    let (headers, body) = request(&mut stream).await;
                    let first = headers.contains("Bearer private-key-one");
                    assert!(first || headers.contains("Bearer private-key-two"));
                    let own = if first { "one" } else { "two" };
                    let peer = if first { "two" } else { "one" };
                    let target = headers.lines().next().unwrap();
                    assert_eq!(target.contains("http://upstream.invalid/"), first, "per-Agent proxy must use its own route");
                    let tool_result = body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool");
                    let reply = if let Some(message) = tool_result {
                        let text = message["content"].to_string();
                        assert!(text.contains(&format!("home-agent-{}", if first {1} else {2})), "tilde must use explicit HOME");
                        assert!(text.contains(&format!("agent-{own}")), "shell must see only its own tool environment");
                        assert!(!text.contains("private-key-"), "Provider key must not leak into a tool");
                        completion(None)
                    } else {
                        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
                        completion(Some(json!({"name":"bash","arguments":{"command":format!(
                            ": > entered; while [ ! -f ../{peer}/entered ]; do /bin/sleep 0.01; done; /bin/pwd; /bin/cat ~/identity.txt; printf '\\nscope=%s key=%s' \"$SCOPE\" \"$BINDING_KEY\""
                        )}})))
                    };
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                });
            }
            while let Some(result) = workers.join_next().await { result.unwrap(); }
        });
        let mut clients = Vec::new();
        for (index, record) in records.iter().enumerate() {
            let key = if index == 0 {"private-key-one"} else {"private-key-two"};
            let endpoint = if index == 0 {"http://upstream.invalid/v1".into()} else {format!("{url}/v1")};
            let mut input = profile(&record.spec.launch.cwd, &endpoint, key);
            input.provider_values.insert("HTTP_PROXY".into(), if index == 0 {url.clone()} else {"http://127.0.0.1:9".into()});
            input.provider_values.insert("NO_PROXY".into(), if index == 0 {""} else {"127.0.0.1"}.into());
            let binding = input.bind(record.spec.clone()).await.unwrap();
            clients.push(service.restore_bound(record.spec.id, fixture.catalog.guard(record), OrdinaryLimits::default(), binding).await.unwrap());
        }
        let mut tickets = Vec::new();
        for client in clients { tickets.push(client.submit(OrdinaryOperation::Prompt(vec![rpi_ai::UserMessage::new("use the granted shell",1).into()])).await.unwrap()); }
        for ticket in tickets {
            let report = ticket.wait().await.unwrap();
            assert!(!report.persistence_uncertain, "{report:?}");
            assert!(matches!(report.outcome, Ok(crate::host::ordinary::OrdinaryOutput::Run(ref run)) if matches!(run.outcome, HarnessRunOutcome::Completed { .. })), "{report:?}");
        }
        server.await.unwrap();
        for record in &records {
            let text = tokio::fs::read_to_string(&record.spec.session.as_ref().unwrap().path).await.unwrap();
            assert!(text.contains("native-binding-finished"));
            assert!(!text.contains("private-key-"));
            assert!(record.spec.launch.cwd.join("entered").is_file());
        }
        service.shutdown().await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn explicit_environment_mismatch_is_rejected_before_the_binder_or_writer() {
    let fixture = Fixture::new().await;
    let record = fixture
        .record("wrong-environment", "workspace", &[], None)
        .await;
    let wrong = tempfile::tempdir().unwrap();
    assert!(matches!(
        OrdinaryBinding::new(
            Arc::new(OsExecutionEnv::with_cwd(record.spec.launch.cwd.clone())),
            Box::new(|_, _, _| panic!("must not run"))
        ),
        Err(BackendError::Binding(_))
    ));
    let env = Arc::new(
        OsExecutionEnv::with_cwd(wrong.path().canonicalize().unwrap())
            .with_environment(Default::default()),
    );
    let binding = OrdinaryBinding::new(
        env,
        Box::new(|_, _, _| panic!("wrong workspace must not reach the binder")),
    )
    .unwrap();
    let path = &record.spec.session.as_ref().unwrap().path;
    let bytes = tokio::fs::read(path).await.unwrap();
    let service = HostService::new(fixture.catalog.clone());
    assert!(matches!(
        service
            .restore_bound(
                record.spec.id,
                fixture.catalog.guard(&record),
                OrdinaryLimits::default(),
                binding
            )
            .await,
        Err(ServiceError::Backend(BackendError::Binding(_)))
    ));
    assert_eq!(tokio::fs::read(path).await.unwrap(), bytes);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_model_reference_is_not_a_fuzzy_selection_or_missing_credential_fallback() {
    for credential in [true, false] {
        let fixture = Fixture::new().await;
        let mut spec = fixture
            .record("exact-model", "workspace", &[], None)
            .await
            .spec;
        if credential {
            spec.launch.model.as_mut().unwrap().model = "host".into();
        }
        // A new record uses the changed reference, with its own exact Session.
        // Fixture registration cannot be overwritten, so use a second fixture.
        let other = Fixture::new().await;
        let record = other.catalog.register(spec).await.unwrap();
        let mut input = profile(
            &record.spec.launch.cwd,
            "http://127.0.0.1:9/v1",
            "private-key-model",
        );
        if !credential {
            input.provider_values.remove("BINDING_KEY");
        }
        rejected(&other, &record, input, RejectionStage::Preflight).await;
        fixture.catalog.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_write_edit_and_read_execute_real_tools_with_the_explicit_home() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = fixture.record("native-files", "workspace", &["write", "edit", "read"], None).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let actions = [json!({"name":"write","arguments":{"path":"~/proof.txt","content":"before"}}),
            json!({"name":"edit","arguments":{"path":"~/proof.txt","oldText":"before","newText":"after"}}),
            json!({"name":"read","arguments":{"path":"~/proof.txt"}})];
        let server = tokio::spawn(async move {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = request(&mut stream).await;
                assert!(headers.contains("Bearer private-key-files"));
                assert_eq!(body["tools"].as_array().unwrap().len(), 3);
                if index == 3 {
                    let result = body["messages"].as_array().unwrap().iter().rev().find(|message| message["role"] == "tool").unwrap();
                    assert!(result["content"].to_string().contains("after"));
                }
                let reply = completion(actions.get(index).cloned());
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let binding = profile(&record.spec.launch.cwd, &url, "private-key-files").bind(record.spec.clone()).await.unwrap();
        let service = HostService::new(fixture.catalog.clone());
        let client = service.restore_bound(record.spec.id, fixture.catalog.guard(&record), OrdinaryLimits::default(), binding).await.unwrap();
        let report = client.submit(OrdinaryOperation::Prompt(vec![rpi_ai::UserMessage::new("exercise native file tools",1).into()])).await.unwrap().wait().await.unwrap();
        assert!(!report.persistence_uncertain);
        assert!(matches!(report.outcome, Ok(crate::host::ordinary::OrdinaryOutput::Run(ref run)) if matches!(run.outcome, HarnessRunOutcome::Completed { .. })), "{report:?}");
        server.await.unwrap();
        assert_eq!(tokio::fs::read_to_string(record.spec.launch.cwd.join("proof.txt")).await.unwrap(), "after");
        service.shutdown().await.unwrap();
    }).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn production_bound_shell_is_cancelled_and_settled_before_service_close_releases_capacity() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new().await;
        let record = fixture.record("native-cancel", "workspace", &["bash"], None).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _) = request(&mut stream).await;
            assert!(headers.contains("Bearer private-key-cancel"));
            let reply = completion(Some(json!({"name":"bash","arguments":{"command":": > entered; /bin/sleep 30"}})));
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let binding = profile(&record.spec.launch.cwd, &url, "private-key-cancel").bind(record.spec.clone()).await.unwrap();
        let service = HostService::new(fixture.catalog.clone());
        let client = service.restore_bound(record.spec.id, fixture.catalog.guard(&record), OrdinaryLimits::default(), binding).await.unwrap();
        let ticket = client.submit(OrdinaryOperation::Prompt(vec![rpi_ai::UserMessage::new("cancel native child",1).into()])).await.unwrap();
        while !record.spec.launch.cwd.join("entered").is_file() { tokio::time::sleep(Duration::from_millis(2)).await; }
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 1);
        service.close_ordinary(record.spec.id).await.unwrap();
        let report = ticket.wait().await.unwrap();
        assert!(matches!(report.outcome, Ok(crate::host::ordinary::OrdinaryOutput::Run(ref run)) if matches!(run.outcome, HarnessRunOutcome::Aborted { .. })), "{report:?}");
        assert_eq!(fixture.catalog.status().await.unwrap().resident, 0);
        assert_eq!(service.owner_count(), 0);
        server.await.unwrap();
        service.shutdown().await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn native_binding_never_executes_secret_commands_or_silently_drops_required_headers() {
    for raw in [
        "!do-not-execute-this-command",
        "$UNSET_REQUIRED_HOST_HEADER",
    ] {
        let fixture = Fixture::new().await;
        let record = fixture
            .record("strict-headers", "workspace", &[], None)
            .await;
        let mut input = profile(
            &record.spec.launch.cwd,
            "http://127.0.0.1:9/v1",
            "private-key-header",
        );
        input
            .runtime
            .models
            .providers
            .get_mut("fixture")
            .unwrap()
            .headers = Some(BTreeMap::from([("x-required".into(), raw.into())]));
        rejected(&fixture, &record, input, RejectionStage::Preflight).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_startup_profile_loads_a_real_agent_and_serves_ipc_submit_and_receipt() {
    use crate::host::{
        client::{ConnectionConfig, HostClient},
        daemon::HostDaemon,
        protocol::{AgentId as ProtocolAgentId, AuthToken, Command, HostId, RequestId},
        server::ControlConfig,
        AgentProfileFile, HostConfig,
    };
    tokio::time::timeout(DEADLINE, async {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        tokio::fs::create_dir(&workspace).await.unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let agent: AgentId = AgentId::new();
        let session_path = workspace.join("profile.jsonl");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _body) = request(&mut stream).await;
            assert!(headers.contains("Bearer daemon-private-key"));
            let reply = completion(None);
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len())
                        .as_bytes(),
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let profile = || {
            serde_json::from_value::<AgentProfileFile>(json!({
                "agent": {
                    "id": agent.to_string(),
                    "name": "daemon-profile",
                    "cwd": workspace.to_string_lossy(),
                    "session": {"id": "daemon-profile-session", "path": session_path.to_string_lossy()},
                    "model": {"provider": "fixture", "model": "host-fixture"},
                    "tools": []
                },
                "runtime": {"providers": {"fixture": {
                    "api": "openai-completions",
                    "baseUrl": format!("{url}/v1"),
                    "apiKey": "$DAEMON_KEY",
                    "models": [{"id": "host-fixture", "contextWindow": 32768, "maxTokens": 1024}]
                }}},
                "providerValues": {"DAEMON_KEY": "daemon-private-key", "NO_PROXY": "*"},
                "toolEnvironment": {"HOME": workspace.to_string_lossy(), "PATH": "/usr/bin:/bin"},
                "shell": "/bin/sh",
                "approvedTools": [],
                "thinking": "off",
                "timeoutMs": 5000
            }))
            .unwrap()
            .into_startup()
            .unwrap()
        };
        let control = ControlConfig {
            socket_path: root.path().join("host/control.sock"),
            host_id: HostId::new(),
            token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            max_connections: 4,
            handshake_timeout: DEADLINE,
            request_timeout: DEADLINE,
        };
        let connection = ConnectionConfig {
            socket_path: control.socket_path.clone(),
            host_id: control.host_id,
            token: control.token.clone(),
            timeout: DEADLINE,
        };
        let mut daemon = HostDaemon::open_with_agents(
            root.path().join("host"),
            HostConfig::default(),
            control.clone(),
            vec![profile()],
        )
        .await
        .unwrap();

        let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
        let agent_id: ProtocolAgentId = agent.to_string().try_into().unwrap();
        let request_id = RequestId::new();
        let submitted = ipc
            .call(Command::Submit {
                agent_id,
                request_id,
                prompt: "startup profile prompt".into(),
            })
            .await
            .unwrap();
        assert_eq!(submitted["durable"], true);
        assert_eq!(submitted["receipt"]["entrySeq"], 1);
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let status = ipc
                .call(Command::Receipt {
                    agent_id,
                    request_id,
                })
                .await
                .unwrap();
            if status["execution"]["finished"] == true {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{status}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server.await.unwrap();
        let text = tokio::fs::read_to_string(&session_path).await.unwrap();
        assert!(text.contains("native-binding-finished"));
        assert!(!text.contains("daemon-private-key"));
        assert_eq!(daemon.service().catalog().status().await.unwrap().registered, 1);
        daemon.shutdown().await.unwrap();
        assert!(!connection.socket_path.exists());

        // Restart: the same explicit profile reuses the registered Agent and
        // Session; the durable receipt is still readable and never re-executes.
        let mut restarted = HostDaemon::open_with_agents(
            root.path().join("host"),
            HostConfig::default(),
            control,
            vec![profile()],
        )
        .await
        .unwrap();
        assert_eq!(
            restarted
                .service()
                .catalog()
                .status()
                .await
                .unwrap()
                .registered,
            1
        );
        let mut ipc = HostClient::connect(connection.clone()).await.unwrap();
        let status = ipc
            .call(Command::Receipt {
                agent_id,
                request_id,
            })
            .await
            .unwrap();
        assert_eq!(status["found"], true);
        assert_eq!(status["receipt"]["entrySeq"], 1);
        let replay = ipc
            .call(Command::Submit {
                agent_id,
                request_id,
                prompt: "startup profile prompt".into(),
            })
            .await
            .unwrap();
        assert_eq!(replay["receipt"]["entrySeq"], 1);
        assert_eq!(
            restarted
                .service()
                .admissions(agent)
                .await
                .unwrap()
                .len(),
            1
        );
        restarted.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_startup_profile_failure_settles_and_releases_catalog_ownership() {
    use crate::host::{
        daemon::HostDaemon, protocol::AuthToken, server::ControlConfig, AgentProfileFile,
        HostConfig,
    };
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let control = ControlConfig {
        socket_path: root.path().join("host/control.sock"),
        host_id: crate::host::protocol::HostId::new(),
        token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
        max_connections: 4,
        handshake_timeout: DEADLINE,
        request_timeout: DEADLINE,
    };
    // Bind rejects this closed environment: the explicit HOME baseline is
    // missing. Startup must fail rather than fall back to process HOME.
    let profile_json = json!({
        "agent": {
            "id": AgentId::new().to_string(),
            "name": "startup-failure",
            "cwd": workspace.to_string_lossy(),
            "session": {"id": "startup-failure", "path": workspace.join("failed.jsonl").to_string_lossy()},
            "model": {"provider": "fixture", "model": "host-fixture"},
            "tools": []
        },
        "runtime": {"providers": {"fixture": {
            "api": "openai-completions",
            "baseUrl": "http://127.0.0.1:9/v1",
            "apiKey": "unused",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]
        }}},
        "toolEnvironment": {"PATH": "/usr/bin:/bin"},
        "shell": "/bin/sh",
        "approvedTools": [],
        "thinking": "off"
    });
    let mut valid_json = profile_json.clone();
    valid_json["agent"]["id"] = json!(AgentId::new().to_string());
    valid_json["agent"]["session"] = json!({
        "id": "startup-first", "path": workspace.join("first.jsonl")
    });
    valid_json["toolEnvironment"]["HOME"] = json!(workspace);
    let valid = serde_json::from_value::<AgentProfileFile>(valid_json)
        .unwrap()
        .into_startup()
        .unwrap();
    let invalid = serde_json::from_value::<AgentProfileFile>(profile_json)
        .unwrap()
        .into_startup()
        .unwrap();
    let error = HostDaemon::open_with_agents(
        root.path().join("host"),
        HostConfig::default(),
        control.clone(),
        vec![valid, invalid],
    )
    .await
    .err()
    .expect("startup must fail without an explicit HOME");
    assert!(matches!(
        error,
        crate::host::daemon::DaemonError::StartupAgent
    ));
    assert!(!control.socket_path.exists());
    assert!(!workspace.join("failed.jsonl").exists());
    assert!(workspace.join("first.jsonl").exists());
    // The failed daemon settled the first owner and released its Session writer
    // and the catalog singleton. The invalid profile created no Session/record.
    let first = JsonlSessionStorage::load(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(workspace.clone())),
        workspace.join("first.jsonl").to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    first.release_writer_lease();
    let reopened = HostCatalog::open(root.path().join("host"), HostConfig::default())
        .await
        .unwrap();
    let status = reopened.status().await.unwrap();
    assert_eq!(status.registered, 1);
    assert_eq!(status.resident, 0);
    reopened.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_rejects_orphan_companion_on_create_and_restart_without_overwriting() {
    use crate::host::{
        daemon::{DaemonError, HostDaemon},
        protocol::{AuthToken, HostId},
        server::ControlConfig,
        HostStartupAgent,
    };
    use rpi_tools::FileSystem;
    use std::os::unix::fs::PermissionsExt;
    tokio::time::timeout(DEADLINE, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let path = workspace.join("new.jsonl");
        let companion = workspace.join("new.jsonl.progress.jsonl");
        std::fs::write(&companion, "foreign progress must be kept").unwrap();
        let agent = AgentId::new();
        let startup = || HostStartupAgent {
            spec: AgentSpec {
                id: agent,
                name: "orphan-refusal".into(),
                parent_id: None,
                mode: AgentMode::Persistent,
                execution: ExecutionMode::InProcess,
                session: Some(SessionRef {
                    id: "orphan-refusal".into(),
                    path: path.clone(),
                }),
                launch: LaunchSpec {
                    cwd: workspace.clone(),
                    model: Some(ModelRef {
                        provider: "fixture".into(),
                        model: "host-fixture".into(),
                    }),
                    tools: vec![],
                    resource_profile: None,
                },
                timeouts: TimeoutOverrides::default(),
            },
            profile: profile(&workspace, "http://127.0.0.1:9/v1", "unused"),
            limits: OrdinaryLimits::default(),
        };
        let control = ControlConfig {
            socket_path: workspace.join("host/control.sock"),
            host_id: HostId::new(),
            token: AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            max_connections: 4,
            handshake_timeout: DEADLINE,
            request_timeout: DEADLINE,
        };
        let mut preserved_header = None;
        for _ in 0..2 {
            let error = HostDaemon::open_with_agents(
                workspace.join("host"),
                HostConfig::default(),
                control.clone(),
                vec![startup()],
            )
            .await
            .err()
            .expect("invalid companion must not publish a ready Host");
            assert!(matches!(error, DaemonError::StartupAgent));
            assert!(!control.socket_path.exists());
            assert_eq!(
                std::fs::read(&companion).unwrap(),
                b"foreign progress must be kept"
            );
            let bytes = std::fs::read(&path).unwrap();
            if let Some(expected) = &preserved_header {
                assert_eq!(&bytes, expected);
            } else {
                preserved_header = Some(bytes);
            }
            let env = OsExecutionEnv::with_cwd(workspace.clone());
            let lease = env
                .try_acquire_writer_lease(path.to_str().unwrap())
                .await
                .unwrap();
            assert!(lease.is_some());
            drop(lease);
            let reopened = HostCatalog::open(workspace.join("host"), HostConfig::default())
                .await
                .unwrap();
            let status = reopened.status().await.unwrap();
            assert_eq!(
                status.registered, 0,
                "readonly saved-facts preflight rejects both attempts before registration"
            );
            assert_eq!(status.resident, 0);
            reopened.shutdown().await.unwrap();
        }
    })
    .await
    .unwrap();
}
