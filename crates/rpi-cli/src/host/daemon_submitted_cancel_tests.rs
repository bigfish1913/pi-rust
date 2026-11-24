//! Shared real HTTP exercise for native and authenticated cancellation paths.
use super::*;
use crate::host::{
    client::{ClientError, ConnectionConfig, HostClient},
    lifecycle::LifecycleResult,
    ordinary::OrdinaryOutput,
    protocol as wire,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const DEADLINE: Duration = Duration::from_secs(15);
async fn http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(DEADLINE, stream.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 1024 * 1024);
        if let Some(end) = bytes
            .windows(4)
            .position(|value| value == b"\r\n\r\n")
            .map(|n| n + 4)
        {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let size = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            assert!(size <= 1024 * 1024 - end);
            if bytes.len() >= end + size {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}
async fn ipc_client(config: &ControlConfig) -> HostClient {
    HostClient::connect(ConnectionConfig {
        socket_path: config.socket_path.clone(),
        host_id: config.host_id,
        token: config.token.clone(),
        timeout: DEADLINE,
    })
    .await
    .unwrap()
}
async fn ipc_cancel(
    config: &ControlConfig,
    intent: &LifecycleRequest,
) -> Result<LifecycleReceipt, ClientError> {
    let mut client = ipc_client(config).await;
    assert!(client
        .welcome()
        .capabilities
        .contains(&wire::Capability::AdministrativeCancel));
    assert!(!client
        .welcome()
        .capabilities
        .contains(&wire::Capability::Cancel));
    let request = serde_json::from_value(json!(intent)).unwrap();
    let data = client
        .call(wire::Command::AdministrativeCancel { request })
        .await?;
    let receipt: LifecycleReceipt = serde_json::from_value(data["receipt"].clone()).unwrap();
    receipt.validate().unwrap();
    Ok(receipt)
}
async fn execute_cancel(
    daemon: &HostDaemon,
    config: &ControlConfig,
    intent: &LifecycleRequest,
    ipc: bool,
) -> LifecycleReceipt {
    if ipc {
        ipc_cancel(config, intent).await.unwrap()
    } else {
        daemon
            .administration()
            .unwrap()
            .execute(intent.clone())
            .await
            .unwrap()
    }
}
#[tokio::test(flavor = "current_thread")]
async fn submitted_cancel_native_daemon_aborts_gated_http_and_reconciles_restart_without_replay() {
    http_cancel_scenario(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn ipc_submitted_cancel_aborts_gated_http_and_reconciles_restart_without_retargeting() {
    http_cancel_scenario(true).await;
}
async fn http_cancel_scenario(ipc: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut config = control(root.path());
    if ipc {
        config.request_timeout = Duration::from_millis(500);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let agent = AgentId::new();
    let scope = ControlScopeId::new();
    let profile = json!({
        "agent":{"id":agent,"name":"native-http-cancel","cwd":root.path(),"session":{"id":"native-http-cancel","path":root.path().join("native-http-cancel.jsonl")},"model":{"provider":"fixture","model":"host-fixture"},"tools":[]},
        "runtime":{"providers":{"fixture":{"api":"openai-completions","baseUrl":format!("http://{}/v1",listener.local_addr().unwrap()),"apiKey":"daemon-native-test-key","models":[{"id":"host-fixture","contextWindow":131072,"maxTokens":128}]}}},
        "toolEnvironment":{"HOME":root.path(),"PATH":"/usr/bin:/bin"},"shell":"/bin/sh","thinking":"off"
    });
    let startup = || {
        serde_json::from_value::<AgentProfileFile>(profile.clone())
            .unwrap()
            .into_startup()
            .unwrap()
    };
    let (first_arrived, first_arrival) = tokio::sync::oneshot::channel();
    let (drop_first, dropped_first) = tokio::sync::oneshot::channel();
    let (second_arrived, second_arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut first, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(http_request(&mut first).await.contains("cancel-http-first"));
        first_arrived.send(()).unwrap();
        // Deliberately never return even response headers. The native run must
        // settle by cancellation, independently of this fixture releasing TCP.
        tokio::time::timeout(DEADLINE, dropped_first)
            .await
            .unwrap()
            .unwrap();
        drop(first);
        let (mut second, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(http_request(&mut second)
            .await
            .contains("cancel-http-second"));
        second_arrived.send(()).unwrap();
        tokio::time::timeout(DEADLINE, released)
            .await
            .unwrap()
            .unwrap();
        let body=concat!("data: {\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"host-fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"native-http-finished\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"host-fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
        second.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        second.shutdown().await.unwrap();
    });
    let mut authority = policy(scope, [agent].into(), vec![]);
    if ipc {
        authority.max_jobs = NonZeroUsize::new(1).unwrap();
    }
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        config.clone(),
        vec![startup()],
        Some(authority),
    )
    .await
    .unwrap();
    let admin = daemon.administration().unwrap().clone();
    let client = daemon.service().ordinary_client(agent).unwrap();
    let original = RequestId::new();
    let first = client
        .submit_admitted(
            config.host_id.to_string(),
            original.to_string(),
            vec![rpi_ai::UserMessage::new("cancel-http-first", 0).into()],
        )
        .await
        .unwrap();
    first.wait_admitted().await.unwrap();
    tokio::time::timeout(DEADLINE, first_arrival)
        .await
        .unwrap()
        .unwrap();
    let running = client.snapshot().await.unwrap();
    let intent = request(
        &daemon,
        &admin,
        LifecycleAction::CancelSubmitted {
            agent_id: agent,
            revision: running.record.revision,
            submitted_request_id: original,
        },
    );
    if ipc {
        for case in 0..5 {
            let mut invalid = intent.clone();
            invalid.request_id = RequestId::new();
            let expected = match case {
                0 => {
                    invalid.scope = ControlScopeId::new();
                    ClientError::Rejected(wire::ErrorCode::Unauthorized)
                }
                1 => {
                    invalid.action = LifecycleAction::CancelSubmitted {
                        agent_id: AgentId::new(),
                        revision: running.record.revision,
                        submitted_request_id: original,
                    };
                    ClientError::Rejected(wire::ErrorCode::Unauthorized)
                }
                2 => {
                    invalid.action = LifecycleAction::CancelSubmitted {
                        agent_id: agent,
                        revision: running.record.revision + 1,
                        submitted_request_id: original,
                    };
                    ClientError::Rejected(wire::ErrorCode::StaleRevision)
                }
                3 => {
                    invalid.epoch = crate::host::HostEpoch::new();
                    ClientError::Rejected(wire::ErrorCode::StaleEpoch)
                }
                _ => {
                    invalid.action = LifecycleAction::Cancel {
                        agent_id: agent,
                        revision: running.record.revision,
                        ticket_id: crate::host::RunId::new(),
                    };
                    ClientError::NotSent(crate::host::client::Failure::InvalidCommand)
                }
            };
            assert_eq!(ipc_cancel(&config, &invalid).await.unwrap_err(), expected);
            assert!(!client.snapshot().await.unwrap().cancel_requested);
        }
    }
    let cancelled = if ipc {
        let service = daemon.service().clone();
        let (entered, held) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let fence = tokio::spawn(async move {
            service
                .with_stop_reservation(false, move |reservation| {
                    Box::pin(async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                        drop(reservation);
                        Ok(())
                    })
                })
                .await
                .unwrap();
        });
        held.await.unwrap();
        let failure = ipc_cancel(&config, &intent).await.unwrap_err();
        assert!(
            matches!(failure,ClientError::OutcomeUnknown {request_id,..} if request_id.to_string()==intent.request_id.to_string())
        );
        assert_eq!(admin.active_jobs(), 1);
        assert!(!client.snapshot().await.unwrap().cancel_requested);
        let mut observer = ipc_client(&config).await;
        assert_eq!(
            observer
                .call(wire::Command::AdministrativeReceipt {
                    request: serde_json::from_value(json!(intent)).unwrap()
                })
                .await
                .unwrap()["found"],
            false
        );
        let mut other = intent.clone();
        other.request_id = RequestId::new();
        assert_eq!(
            ipc_cancel(&config, &other).await.unwrap_err(),
            ClientError::Rejected(wire::ErrorCode::Capacity)
        );
        release.send(()).unwrap();
        fence.await.unwrap();
        let receipt = tokio::time::timeout(DEADLINE, async {
            loop {
                let value = observer
                    .call(wire::Command::AdministrativeReceipt {
                        request: serde_json::from_value(json!(intent)).unwrap(),
                    })
                    .await
                    .unwrap();
                if value["found"] == true {
                    let receipt: LifecycleReceipt =
                        serde_json::from_value(value["receipt"].clone()).unwrap();
                    if matches!(receipt.status, LifecycleState::Succeeded { .. }) {
                        break receipt;
                    }
                    assert!(matches!(receipt.status, LifecycleState::Prepared {}));
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(admin.active_jobs(), 0);
        receipt
    } else {
        execute_cancel(&daemon, &config, &intent, false).await
    };
    assert_eq!(
        cancelled.status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: true }
        }
    );
    let report = tokio::time::timeout(DEADLINE, first.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,rpi_harness::agent_harness::HarnessRunOutcome::Aborted {..})),
        "{report:?}"
    );
    assert!(!report.persistence_uncertain);
    drop_first.send(()).unwrap();
    let second_id = RequestId::new();
    let second = client
        .submit_admitted(
            config.host_id.to_string(),
            second_id.to_string(),
            vec![rpi_ai::UserMessage::new("cancel-http-second", 0).into()],
        )
        .await
        .unwrap();
    second.wait_admitted().await.unwrap();
    tokio::time::timeout(DEADLINE, second_arrival)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        execute_cancel(&daemon, &config, &intent, ipc).await,
        cancelled
    );
    let running = client.snapshot().await.unwrap();
    if ipc {
        let mut changed = intent.clone();
        changed.action = LifecycleAction::CancelSubmitted {
            agent_id: agent,
            revision: running.record.revision,
            submitted_request_id: second_id,
        };
        assert_eq!(
            ipc_cancel(&config, &changed).await.unwrap_err(),
            ClientError::Rejected(wire::ErrorCode::Conflict)
        );
    }
    let no_target = request(
        &daemon,
        &admin,
        LifecycleAction::CancelSubmitted {
            agent_id: agent,
            revision: running.record.revision,
            submitted_request_id: original,
        },
    );
    assert_eq!(
        execute_cancel(&daemon, &config, &no_target, ipc)
            .await
            .status,
        LifecycleState::Succeeded {
            result: LifecycleResult::CancelSignalled { signalled: false }
        }
    );
    let snapshot = client.snapshot().await.unwrap();
    assert!(!snapshot.cancel_requested);
    assert_eq!(snapshot.ticket, Some(second.id));
    release.send(()).unwrap();
    tokio::time::timeout(DEADLINE, provider)
        .await
        .unwrap()
        .unwrap();
    let report = tokio::time::timeout(DEADLINE, second.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(report.outcome,Ok(OrdinaryOutput::Run(ref result)) if matches!(result.outcome,rpi_harness::agent_harness::HarnessRunOutcome::Completed {..})),
        "{report:?}"
    );
    daemon.shutdown().await.unwrap();
    let path = root.path().join("native-http-cancel.jsonl");
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&before)
            .matches("rpi.host.admission.v1")
            .count(),
        2
    );
    let mut restarted = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        config.clone(),
        vec![startup()],
        Some(policy(scope, [agent].into(), vec![])),
    )
    .await
    .unwrap();
    let admin = restarted.administration().unwrap();
    assert_eq!(
        admin.receipt(&intent).await.unwrap(),
        Some(cancelled.clone())
    );
    if ipc {
        let mut client = ipc_client(&config).await;
        let value = client
            .call(wire::Command::AdministrativeReceipt {
                request: serde_json::from_value(json!(intent)).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(value["found"], true);
        assert_eq!(
            serde_json::from_value::<LifecycleReceipt>(value["receipt"].clone()).unwrap(),
            cancelled
        );
    }
    assert_eq!(
        execute_cancel(&restarted, &config, &intent, ipc).await,
        cancelled
    );
    let snapshot = restarted
        .service()
        .ordinary_client(agent)
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert!(snapshot.ticket.is_none() && snapshot.submitted_request_id.is_none());
    restarted.shutdown().await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), before);
}
