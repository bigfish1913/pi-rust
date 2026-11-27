//! A disconnected close waiter is not the owner of real native settlement.
use super::*;
use rpi_cli::host::lifecycle::{LifecycleReceipt, LifecycleResult, LifecycleState};
use sha2::{Digest, Sha256};

async fn close_receipt(
    client: &mut rpi_cli::host::client::HostClient,
    intent: &execution_wire::AdministrativeRequest,
    settled: bool,
) -> Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let value = client
                .call(C::AdministrativeReceipt {
                    request: intent.clone(),
                })
                .await
                .unwrap();
            if value["found"] == true {
                assert_eq!(value["receipt"]["request"], json!(intent));
                let receipt: LifecycleReceipt =
                    serde_json::from_value(value["receipt"].clone()).unwrap();
                match receipt.status {
                    LifecycleState::Prepared {} if !settled => return value,
                    LifecycleState::Prepared {} => {}
                    LifecycleState::Succeeded {
                        result: LifecycleResult::Closed { .. },
                    } => {
                        assert!(settled, "close settled before the actual native gate");
                        return value;
                    }
                    other => panic!("unexpected close receipt: {other:?}"),
                }
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_native_close_disconnect_keeps_custody_and_reconciles_original_after_restart()
{
    for provider in [false, true] {
        let case = if provider {
            "provider-response"
        } else {
            "tool-result"
        };
        let mut fixture = Fixture::new();
        let dll = if provider {
            provider_dll(fixture.root.path(), "disconnect")
        } else {
            hook_dll(fixture.root.path(), "disconnect", 0, 0, false)
        };
        let dll_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&dll.library).unwrap()));
        std::fs::write(&dll.provider_gate, b"release").unwrap();
        let (native_agent, path, mut profile) = administrative_profile(&fixture);
        let agent = serde_json::from_value(json!(native_agent)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        hook_profile(
            &mut profile,
            &[&dll],
            &format!("http://{}/v1", listener.local_addr().unwrap()),
        );
        std::fs::write(fixture.root.path().join("input.txt"), b"disconnect-input").unwrap();
        std::fs::write(&path, profile.to_string()).unwrap();
        let scope = execution_wire::ControlScopeId::new();
        fixture.value["administration"] = json!({"scope":scope,"profiles":[path],
            "allowed_agents":[agent],"permissions":["create","restore","cancel","close"]});
        fixture.save();
        let (child, _) = fixture.start().await;
        let mut client = administrative_client(&fixture).await;
        assert!(!dll.events.exists(), "profile discovery loaded native code");
        assert_eq!(client.call(C::Status {}).await.unwrap()["owners"], 0);
        let profiles = client
            .call(C::Profiles {
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        let create = execution_intent(
            &client,
            scope,
            A::CreateProfile {
                profile: serde_json::from_value(profiles["profiles"][0].clone()).unwrap(),
            },
        );
        let created = client.call(execution_command(&create)).await.unwrap();
        execution_succeeded(&created);
        assert_eq!(created["receipt"]["request"], json!(create));
        hook_mark(&dll, 'R', 1).await;
        let submitted_request_id = execution_wire::RequestId::new();
        let submit = C::Submit {
            agent_id: agent,
            request_id: submitted_request_id,
            prompt: "disconnect-native-close".into(),
        };
        let admitted = client.call(submit.clone()).await.unwrap();
        let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        execution_http_request(&mut stream).await;
        hook_http_reply(
            &mut stream,
            &if provider {
                sse_completion()
            } else {
                hook_tool_completion()
            },
        )
        .await;
        let kind = if provider { 'A' } else { 'T' };
        hook_mark(&dll, kind, 1).await;
        let event: Value = hook_lines(&dll)
            .iter()
            .find_map(|line| {
                line.strip_prefix(&format!("{kind}|"))
                    .map(|text| serde_json::from_str(text).unwrap())
            })
            .unwrap();
        let context = if provider { &event } else { &event["__rpi"] };
        assert_eq!(context["agentId"], json!(native_agent.to_string()));
        assert_eq!(context["sessionId"], profile["agent"]["session"]["id"]);
        let revision = execution_revision(&mut client, agent).await;
        let cancel = execution_intent(
            &client,
            scope,
            A::CancelSubmitted {
                agent_id: agent,
                revision,
                submitted_request_id,
            },
        );
        let cancelled = client.call(execution_command(&cancel)).await.unwrap();
        cancellation_signalled(&cancelled, true);
        assert_eq!(cancelled["receipt"]["request"], json!(cancel));
        assert!(
            !hook_lines(&dll)
                .iter()
                .any(|line| line.starts_with(if provider { "a|" } else { "D|" })
                    || line.starts_with("B|")),
            "CancelSignalled must not mean the actual native callback settled"
        );
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
        // Observe durable admission, not a sleep or a guess that the command
        // reached the server. The actual native callback is still gated.
        close_receipt(&mut client, &intent, false).await;
        assert!(!closing.is_finished());
        closing.abort();
        assert!(closing.await.unwrap_err().is_cancelled());
        // HostClient::call owns its UnixStream inside the cancelled future;
        // join above proves it has been dropped, not merely stop being polled.
        drop(client);
        let mut observer = administrative_client(&fixture).await;
        let session = fixture.root.path().join("dynamic.jsonl");
        let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
        let mut checkpoints = Vec::new();
        for stage in ["callback", "finish"] {
            let pending = close_receipt(&mut observer, &intent, false).await;
            let status = tokio::time::timeout(Duration::from_secs(2), observer.call(C::Status {}))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(status["owners"], 1, "{case}/{stage}");
            assert_eq!(status["resident"], 1, "{case}/{stage}");
            assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("U|")));
            assert!(
                rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                    .await
                    .is_err(),
                "{case}/{stage}: writer released early"
            );
            checkpoints.push(json!({"stage":stage,"receipt":pending["receipt"],
                "owners":status["owners"],"resident":status["resident"]}));
            if stage == "callback" {
                assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("B|")));
                std::fs::write(
                    if provider {
                        &dll.after_gate
                    } else {
                        &dll.callback_gate
                    },
                    b"release",
                )
                .unwrap();
                hook_mark(&dll, 'E', 1).await;
            } else {
                std::fs::write(&dll.finish_gate, b"release").unwrap();
            }
        }
        // No close response can now reach the original peer. A fresh query of
        // the unchanged full intent is the only completion observation.
        let completed = close_receipt(&mut observer, &intent, true).await;
        hook_unloaded(&dll, 1);
        let marks = hook_lines(&dll);
        let kinds = if provider {
            "A a B W E F U"
        } else {
            "T D S B W E F U"
        };
        let mut previous = None;
        for kind in kinds.split_whitespace() {
            let position = marks
                .iter()
                .position(|line| line.starts_with(&format!("{kind}|")))
                .unwrap();
            if let Some(previous) = previous {
                assert!(previous < position, "{case}: {kinds}");
            }
            previous = Some(position);
        }
        let status = observer.call(C::Status {}).await.unwrap();
        assert_eq!(status["owners"], 0);
        assert_eq!(status["resident"], 0);
        let writer =
            rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                .await
                .unwrap()
                .unwrap();
        drop(writer);
        assert_eq!(
            observer.call(execution_command(&intent)).await.unwrap()["receipt"],
            completed["receipt"]
        );
        let saved = std::fs::read(&session).unwrap();
        drop(observer);
        fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
        let (child, _) = fixture.start().await;
        let mut observer = administrative_client(&fixture).await;
        assert_ne!(observer.welcome().epoch, intent.epoch);
        let restart_epoch = observer.welcome().epoch;
        assert_eq!(observer.call(C::Status {}).await.unwrap()["owners"], 0);
        assert_eq!(hook_lines(&dll), marks, "restart loaded a dynamic profile");
        let profiles = observer
            .call(C::Profiles {
                after: None,
                limit: 1,
            })
            .await
            .unwrap();
        let revision = execution_revision(&mut observer, agent).await;
        let restore = execution_intent(
            &observer,
            scope,
            A::RestoreProfile {
                profile: serde_json::from_value(profiles["profiles"][0].clone()).unwrap(),
                revision,
            },
        );
        let restored = observer.call(execution_command(&restore)).await.unwrap();
        execution_succeeded(&restored);
        assert_eq!(restored["receipt"]["request"], json!(restore));
        hook_mark(&dll, 'R', 2).await;
        let before = hook_lines(&dll);
        assert_eq!(
            observer.call(execution_command(&cancel)).await.unwrap()["receipt"],
            cancelled["receipt"]
        );
        assert_eq!(
            observer.call(execution_command(&create)).await.unwrap()["receipt"],
            created["receipt"]
        );
        assert_eq!(
            observer.call(execution_command(&restore)).await.unwrap()["receipt"],
            restored["receipt"]
        );
        assert_eq!(
            close_receipt(&mut observer, &intent, true).await["receipt"],
            completed["receipt"]
        );
        assert_eq!(
            observer.call(execution_command(&intent)).await.unwrap()["receipt"],
            completed["receipt"]
        );
        assert_eq!(observer.call(submit).await.unwrap(), admitted);
        let status = observer.call(C::Status {}).await.unwrap();
        assert_eq!(
            status["owners"], 1,
            "historical close retired the new owner"
        );
        assert_eq!(status["resident"], 1);
        assert!(
            rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "{case}: historical close/submit repeated HTTP"
        );
        assert_eq!(std::fs::read(&session).unwrap(), saved);
        assert_eq!(
            hook_lines(&dll),
            before,
            "{case}: historical intent dispatched native work"
        );
        drop(observer);
        fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
        hook_unloaded(&dll, 2);
        fixture.no_resources();
        eprintln!(
            "native_close_disconnect_evidence={}",
            json!({"case":case,
            "dllSha256":dll_sha256,"intent":intent,"checkpoints":checkpoints,
            "createReceipt":created["receipt"],"cancelReceipt":cancelled["receipt"],
            "restoreReceipt":restored["receipt"],
            "completedReceipt":completed["receipt"],"restartEpoch":restart_epoch,
            "historicalCloseAndSubmitDidNotReplay":true,"nativeUnloadCount":2})
        );
    }
}
