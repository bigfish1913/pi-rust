//! Authenticated daemon + actual raw Agent + actual gated native DLL + HTTP.
//! Management-DLL single-chain qualification is a separate native Python gate.
use super::*;
use crate::host::{
    client::{ClientError, ConnectionConfig, HostClient},
    daemon::HostDaemon,
    mini_control::MiniControlConfig,
    protocol as w,
    server::ControlConfig,
    HostStartupAgent,
};

fn startup(root: &Path, dll: &LateDll, url: &str, id: AgentId) -> HostStartupAgent {
    let cwd = root.join("mini-wire-workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let resources = Arc::new(crate::host::NativeBindingResources::freeze(
        serde_json::from_value(json!({"version":1,"tools":{"mode":"none"},"extensions":[dll.library]})).unwrap(),
        Some(serde_json::from_value(json!({"stagingRoot":dll.staging,"registrations":[{"providerEvents":[{"event":"afterResponse","count":1}]}]})).unwrap()),
    ).unwrap());
    let mut spec = mini_spec(&cwd.canonicalize().unwrap());
    spec.id = id;
    spec.launch.resource_profile = Some(resources.identity().into());
    let mut input = profile(&cwd, url, "private-mini-key");
    input.instruction_resources = Some(resources);
    HostStartupAgent {
        spec,
        profile: input,
        limits: OrdinaryLimits::default(),
    }
}
async fn settled(client: &mut HostClient, request: &w::MiniRequest) -> Value {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let data = client
                .call(w::Command::MiniReceipt {
                    request: request.clone(),
                })
                .await
                .unwrap();
            if data["operation"]["state"] != "running" {
                break data;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test(flavor = "current_thread")]
async fn mini_wire_private_grants_volatile_intents_disconnect_native_custody_and_restart_lost() {
    tokio::time::timeout(DEADLINE, async {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let dll = LateDll::new(root.path());
        let id = AgentId::new();
        let agent = id.to_string().try_into().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (_, body) = request(&mut stream).await;
            assert_eq!(
                body["messages"].as_array().unwrap().last().unwrap()["content"],
                "literal {{mini-wire}}"
            );
            reply(&mut stream).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let control = ControlConfig {
            socket_path: root.path().join("s"),
            host_id: w::HostId::new(),
            token: w::AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
            max_connections: 8,
            handshake_timeout: DEADLINE,
            request_timeout: DEADLINE,
        };
        let connection = ConnectionConfig {
            socket_path: control.socket_path.clone(),
            host_id: control.host_id,
            token: control.token.clone(),
            timeout: DEADLINE,
        };
        let scope = w::ControlScopeId::new();
        let policy = || {
            serde_json::from_value::<MiniControlConfig>(
                json!({"scope":scope,"allowed_agents":[agent],
            "permissions":["submit","cancel","close"],"max_operations":3}),
            )
            .unwrap()
        };
        let mut daemon = HostDaemon::open_with_mini(
            root.path().join("daemon"),
            HostConfig::default(),
            control.clone(),
            vec![startup(root.path(), &dll, &url, id)],
            None,
            Some(policy()),
        )
        .await
        .unwrap();
        assert!(daemon.welcome().supports(w::Capability::Mini));
        let epoch = daemon.welcome().epoch;
        let mut client = HostClient::connect(connection.clone()).await.unwrap();
        let submit = w::MiniRequest {
            scope,
            epoch,
            request_id: w::RequestId::new(),
            action: w::MiniAction::Submit {
                agent_id: agent,
                prompt: "literal {{mini-wire}}".into(),
            },
        };
        let mut denied = submit.clone();
        denied.scope = w::ControlScopeId::new();
        assert!(matches!(
            client
                .call(w::Command::MiniExecute { request: denied })
                .await,
            Err(ClientError::Rejected(w::ErrorCode::Unauthorized))
        ));
        let mut stale = submit.clone();
        stale.epoch = w::Epoch::new();
        assert!(matches!(
            client
                .call(w::Command::MiniExecute { request: stale })
                .await,
            Err(ClientError::Rejected(w::ErrorCode::StaleEpoch))
        ));
        let first = client
            .call(w::Command::MiniExecute {
                request: submit.clone(),
            })
            .await
            .unwrap();
        assert_eq!(first["durable"], false);
        assert_eq!(first["volatile"], true);
        let admitted = settled(&mut client, &submit).await;
        assert_eq!(admitted["operation"]["state"], "succeeded");
        let run =
            serde_json::from_value(admitted["operation"]["effect"]["run_id"].clone()).unwrap();
        dll.marked('A').await;
        server.await.unwrap();
        let mut conflict = submit.clone();
        conflict.action = w::MiniAction::Submit {
            agent_id: agent,
            prompt: "different".into(),
        };
        assert!(matches!(
            client
                .call(w::Command::MiniExecute { request: conflict })
                .await,
            Err(ClientError::Rejected(w::ErrorCode::Conflict))
        ));
        let data = client
            .call(w::Command::MiniResult {
                scope,
                epoch,
                agent_id: agent,
                request_id: submit.request_id,
                run_id: Some(run),
                offset: 0,
                max_bytes: 4096,
            })
            .await
            .unwrap();
        assert_eq!(data["result"]["state"], "running");
        let cancel = w::MiniRequest {
            scope,
            epoch,
            request_id: w::RequestId::new(),
            action: w::MiniAction::Cancel {
                agent_id: agent,
                submitted_request_id: submit.request_id,
                run_id: run,
            },
        };
        client
            .call(w::Command::MiniExecute {
                request: cancel.clone(),
            })
            .await
            .unwrap();
        let signalled = settled(&mut client, &cancel).await;
        assert_eq!(signalled["operation"]["effect"]["cancel_signalled"], true);
        assert_eq!(signalled["operation"]["effect"]["settlement_proven"], false);
        let close = w::MiniRequest {
            scope,
            epoch,
            request_id: w::RequestId::new(),
            action: w::MiniAction::Close { agent_id: agent },
        };
        client
            .call(w::Command::MiniExecute {
                request: close.clone(),
            })
            .await
            .unwrap();
        client.detach(); // Lose the close observer, not the owned operation.
        let mut client = HostClient::connect(connection.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let pending = client
            .call(w::Command::MiniReceipt {
                request: close.clone(),
            })
            .await
            .unwrap();
        assert_eq!(pending["operation"]["state"], "running");
        assert_eq!(daemon.service().owner_count(), 1);
        assert!(!dll.events().contains('B'));
        assert!(!dll.events().contains('U'));
        let extra = w::MiniRequest {
            request_id: w::RequestId::new(),
            ..close.clone()
        };
        assert!(matches!(
            client
                .call(w::Command::MiniExecute { request: extra })
                .await,
            Err(ClientError::Rejected(w::ErrorCode::Capacity))
        ));
        dll.release("mini-callback-gate");
        dll.marked('E').await;
        assert_eq!(daemon.service().owner_count(), 1);
        assert_eq!(
            client
                .call(w::Command::MiniReceipt {
                    request: close.clone()
                })
                .await
                .unwrap()["operation"]["state"],
            "running"
        );
        dll.release("mini-finish-gate");
        let closed = settled(&mut client, &close).await;
        assert_eq!(closed["operation"]["effect"]["native_settled"], true);
        assert_eq!(daemon.service().owner_count(), 0);
        assert_eq!(dll.events(), "RABEFU");
        assert_eq!(
            client
                .call(w::Command::MiniExecute {
                    request: submit.clone()
                })
                .await
                .unwrap(),
            admitted
        );
        daemon.shutdown().await.unwrap();
        client.detach();
        let mut restarted = HostDaemon::open_with_mini(
            root.path().join("daemon"),
            HostConfig::default(),
            control,
            vec![startup(root.path(), &dll, &url, id)],
            None,
            Some(policy()),
        )
        .await
        .unwrap();
        assert_ne!(restarted.welcome().epoch, epoch);
        assert_eq!(restarted.service().owner_count(), 0);
        assert_eq!(
            restarted.service().catalog().get(id).await.unwrap().state,
            AgentState::Lost
        );
        let mut client = HostClient::connect(connection).await.unwrap();
        let unknown = client
            .call(w::Command::MiniReceipt {
                request: close.clone(),
            })
            .await
            .unwrap();
        assert_eq!(unknown["found"], false);
        assert_eq!(unknown["operation"]["state"], "unknown");
        assert!(matches!(
            client
                .call(w::Command::MiniExecute { request: submit })
                .await,
            Err(ClientError::Rejected(w::ErrorCode::StaleEpoch))
        ));
        assert_eq!(dll.events(), "RABEFU", "restart reloaded a Mini DLL");
        assert_eq!(
            std::fs::read_dir(root.path().join("mini-wire-workspace"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(std::fs::read_dir(&dll.staging).unwrap().count(), 0);
        restarted.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn mini_wire_invalid_private_policy_precedes_any_catalog_or_session_effect() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let dll = LateDll::new(root.path());
    let id = AgentId::new();
    let agent = startup(root.path(), &dll, "http://127.0.0.1:9/v1", id);
    let control = ControlConfig {
        socket_path: root.path().join("s"),
        host_id: w::HostId::new(),
        token: w::AuthToken::new(uuid::Uuid::new_v4().simple().to_string()).unwrap(),
        max_connections: 4,
        handshake_timeout: DEADLINE,
        request_timeout: DEADLINE,
    };
    let config = serde_json::from_value(
        json!({"scope":w::ControlScopeId::new(),"allowed_agents":[id,id],"permissions":["submit"]}),
    )
    .unwrap();
    assert!(HostDaemon::open_with_mini(
        root.path().join("absent"),
        HostConfig::default(),
        control,
        vec![agent],
        None,
        Some(config)
    )
    .await
    .is_err());
    assert!(!root.path().join("absent").exists());
    assert!(!root.path().join("s").exists());
    assert!(dll.events().is_empty());
}
