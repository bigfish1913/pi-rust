//! Provider callback ordering/custody through the actual production binary.
use super::*;

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_provider_late_cancel_success_error_keep_ipc_owner_and_writer_until_real_finish(
) {
    for case in ["before-cancel", "after-success", "after-error"] {
        let mut fixture = Fixture::new();
        let dll = provider_dll(fixture.root.path(), "provider-late");
        if case != "before-cancel" {
            std::fs::write(&dll.provider_gate, b"release").unwrap();
        }
        let (native_agent, path, mut profile) = administrative_profile(&fixture);
        let agent = serde_json::from_value(json!(native_agent)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        hook_profile(
            &mut profile,
            &[&dll],
            &format!("http://{}/v1", listener.local_addr().unwrap()),
        );
        std::fs::write(&path, profile.to_string()).unwrap();
        let scope = execution_wire::ControlScopeId::new();
        fixture.value["agents"] = json!([path]);
        fixture.value["administration"] =
            json!({"scope":scope,"allowed_agents":[agent],"permissions":["close"]});
        fixture.save();
        let (child, _) = fixture.start().await;
        let mut client = administrative_client(&fixture).await;
        client
            .call(C::Submit {
                agent_id: agent,
                request_id: execution_wire::RequestId::new(),
                prompt: "observe without replacing input".into(),
            })
            .await
            .unwrap();
        hook_mark(&dll, 'Q', 1).await;
        if case == "before-cancel" {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
            assert!(!hook_lines(&dll)
                .iter()
                .any(|line| line.starts_with("A|") || line.starts_with("H|")));
        } else {
            let (mut stream, _) = tokio::time::timeout(DEADLINE, listener.accept())
                .await
                .unwrap()
                .unwrap();
            execution_http_request(&mut stream).await;
            if case == "after-error" {
                use tokio::io::AsyncWriteExt;
                let body = r#"{"error":{"message":"provider-fixture-rejected","type":"invalid_request_error"}}"#;
                stream.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            } else {
                hook_http_reply(&mut stream, &sse_completion()).await;
            }
            hook_mark(&dll, 'A', 1).await;
        }
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
        assert!(
            !closing.is_finished(),
            "{case}: close ran ahead of callback"
        );
        assert!(!hook_lines(&dll)
            .iter()
            .any(|line| line.starts_with("B|") || line.starts_with("U|")));
        let env = rpi_tools::OsExecutionEnv::with_cwd(fixture.root.path().to_path_buf());
        let session = fixture.root.path().join("dynamic.jsonl");
        assert!(
            rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                .await
                .is_err()
        );
        // An actual native callback is blocked. The production IPC executor
        // must still answer, while all physical ownership remains retained.
        let status = tokio::time::timeout(Duration::from_secs(2), client.call(C::Status {}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status["owners"], 1);
        assert_eq!(status["resident"], 1);
        if case == "before-cancel" {
            assert!(
                !hook_lines(&dll).iter().any(|line| line.starts_with("A|")),
                "Aborted response overtook cancelled before-request worker"
            );
            std::fs::write(&dll.provider_gate, b"release").unwrap();
            hook_mark(&dll, 'A', 1).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "cancelled open reached HTTP"
            );
        }
        let response: Value = hook_lines(&dll)
            .iter()
            .find_map(|line| {
                line.strip_prefix("A|")
                    .map(|value| serde_json::from_str(value).unwrap())
            })
            .unwrap();
        assert_eq!(response["agentId"], json!(native_agent.to_string()));
        assert_eq!(response["sessionId"], profile["agent"]["session"]["id"]);
        assert_eq!(
            response["stopReason"],
            match case {
                "before-cancel" => "aborted",
                "after-error" => "error",
                _ => "stop",
            }
        );
        assert!(!closing.is_finished());
        std::fs::write(&dll.after_gate, b"release").unwrap();
        hook_mark(&dll, 'E', 1).await;
        assert!(
            !closing.is_finished(),
            "finish waiter cannot stand in for actual finish"
        );
        assert!(!hook_lines(&dll).iter().any(|line| line.starts_with("U|")));
        assert!(
            rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                .await
                .is_err()
        );
        assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 1);
        std::fs::write(&dll.finish_gate, b"release").unwrap();
        let receipt = tokio::time::timeout(DEADLINE, closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        execution_succeeded(&receipt);
        hook_unloaded(&dll, 1);
        let marks = hook_lines(&dll);
        let position = |kind| {
            marks
                .iter()
                .position(|line| line.starts_with(&format!("{kind}|")))
                .unwrap()
        };
        for (a, b) in [
            ('Q', 'q'),
            ('q', 'H'),
            ('H', 'A'),
            ('A', 'a'),
            ('a', 'B'),
            ('B', 'W'),
            ('W', 'E'),
            ('E', 'F'),
            ('F', 'U'),
        ] {
            assert!(
                position(a) < position(b),
                "{case}: {a} must precede {b}: {marks:?}"
            );
        }
        assert_eq!(client.call(C::Status {}).await.unwrap()["resident"], 0);
        let writer =
            rpi_tools::FileSystem::try_acquire_writer_lease(&env, session.to_str().unwrap())
                .await
                .unwrap()
                .unwrap();
        drop(writer);
        assert_eq!(
            client.call(execution_command(&intent)).await.unwrap(),
            receipt
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "{case}: duplicate or retry reached HTTP"
        );
        drop(client);
        fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
    }
}
