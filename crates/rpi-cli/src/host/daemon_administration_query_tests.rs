//! Actual authenticated Unix IPC + native daemon. No wire mutation is enabled.
use super::*;
use crate::host::{
    client::{ClientError, ConnectionConfig, HostClient},
    protocol as wire,
};

async fn connect(config: &ControlConfig) -> HostClient {
    HostClient::connect(ConnectionConfig {
        socket_path: config.socket_path.clone(),
        host_id: config.host_id,
        token: config.token.clone(),
        timeout: Duration::from_secs(15),
    })
    .await
    .unwrap()
}
fn receipt_command(request: &LifecycleRequest) -> wire::Command {
    wire::Command::AdministrativeReceipt {
        request: serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap(),
    }
}

#[tokio::test]
async fn profiles_are_allowed_opaque_sorted_pages_without_loading_or_disclosing_configuration() {
    let root = tempfile::tempdir().unwrap();
    let profiles: Vec<_> = (0..5)
        .map(|n| input(root.path(), &format!("query-{n}")))
        .collect();
    let specs: Vec<_> = profiles.iter().map(|p| p.spec.clone()).collect();
    let allowed = specs[..4].iter().map(|s| s.id).collect();
    let scope = ControlScopeId::new();
    let control = control(root.path());
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control.clone(),
        vec![],
        Some(policy(scope, allowed, profiles)),
    )
    .await
    .unwrap();
    let refs: Vec<_> = daemon
        .administration()
        .unwrap()
        .profile_references()
        .collect();
    let mut client = connect(&control).await;
    let mut received = vec![];
    let mut after = None;
    for page in 0..2 {
        let response = client
            .call(wire::Command::Profiles { after, limit: 2 })
            .await
            .unwrap();
        assert_eq!(response["scope"], json!(scope));
        assert_eq!(response.as_object().unwrap().len(), 3);
        assert_eq!(response["profiles"], json!(refs[page * 2..page * 2 + 2]));
        for item in response["profiles"].as_array().unwrap() {
            assert_eq!(item.as_object().unwrap().len(), 2);
            received.push(item.clone());
        }
        let encoded = response.to_string();
        assert!(!encoded.contains("daemon-native-test-key"));
        assert!(!encoded.contains(root.path().to_str().unwrap()));
        after = serde_json::from_value(response["next_cursor"].clone()).unwrap();
        assert_eq!(after.is_some(), page == 0);
    }
    assert_eq!(
        received,
        serde_json::to_value(&refs)
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec()
    );
    assert!(!received.iter().any(|p| p["agent_id"] == json!(specs[4].id)));
    let tail = client
        .call(wire::Command::Profiles {
            after: Some(serde_json::from_value(json!(refs.last().unwrap().agent_id)).unwrap()),
            limit: 1,
        })
        .await
        .unwrap();
    assert_eq!(tail["profiles"], json!([]));
    assert!(tail["next_cursor"].is_null());
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["registered"],
        0
    );
    assert_eq!(daemon.administration().unwrap().active_jobs(), 0);
    for spec in specs {
        assert!(!spec.session.unwrap().path.exists());
    }
    drop(client);
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn administrative_receipt_reconciles_full_old_intent_after_refreeze_without_reexecution() {
    let root = tempfile::tempdir().unwrap();
    let profile = input(root.path(), "historical-query");
    let spec = profile.spec.clone();
    let scope = ControlScopeId::new();
    let mut config = policy(scope, BTreeSet::from([spec.id]), vec![profile]);
    config.permissions = AdministrativePermissions {
        create: true,
        ..Default::default()
    };
    let mut first = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control(root.path()),
        vec![],
        Some(config),
    )
    .await
    .unwrap();
    let admin = first.administration().unwrap();
    let old_ref = admin.profile_references().next().unwrap();
    let original = request(
        &first,
        admin,
        LifecycleAction::CreateProfile { profile: old_ref },
    );
    let receipt = admin.execute(original.clone()).await.unwrap();
    succeeded(&receipt);
    first.shutdown().await.unwrap();
    let before = std::fs::read(spec.session.as_ref().unwrap().path.clone()).unwrap();
    let mut new_profile = input(root.path(), "historical-query");
    new_profile.spec = spec.clone();
    let mut config = policy(scope, BTreeSet::from([spec.id]), vec![new_profile]);
    config.permissions = AdministrativePermissions {
        create: true,
        ..Default::default()
    };
    let control = control(root.path());
    let mut daemon = HostDaemon::open_configured(
        root.path().join("host"),
        HostConfig::default(),
        control.clone(),
        vec![],
        Some(config),
    )
    .await
    .unwrap();
    let new_ref = daemon
        .administration()
        .unwrap()
        .profile_references()
        .next()
        .unwrap();
    assert_ne!(new_ref, old_ref);
    let mut client = connect(&control).await;
    let data = client.call(receipt_command(&original)).await.unwrap();
    assert_eq!(data["found"], true);
    assert_eq!(data["receipt"], json!(receipt));
    assert_ne!(
        original.epoch.to_string(),
        client.welcome().epoch.to_string()
    );
    let mut changed = original.clone();
    changed.action = LifecycleAction::CreateProfile { profile: new_ref };
    assert_eq!(
        client.call(receipt_command(&changed)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Conflict)
    );
    changed = original.clone();
    changed.epoch = daemon.service().catalog().epoch();
    assert_eq!(
        client.call(receipt_command(&changed)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Conflict)
    );
    changed = original.clone();
    changed.scope = ControlScopeId::new();
    assert_eq!(
        client.call(receipt_command(&changed)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Unauthorized)
    );
    changed = original.clone();
    changed.action = LifecycleAction::Stop {};
    assert_eq!(
        client.call(receipt_command(&changed)).await.unwrap_err(),
        ClientError::Rejected(wire::ErrorCode::Unauthorized)
    );
    changed = original.clone();
    changed.request_id = RequestId::new();
    let missing = client.call(receipt_command(&changed)).await.unwrap();
    assert_eq!(missing["found"], false);
    assert!(missing["receipt"].is_null());
    assert_eq!(daemon.administration().unwrap().active_jobs(), 0);
    assert_eq!(
        client.call(wire::Command::Status {}).await.unwrap()["resident"],
        0
    );
    assert_eq!(
        std::fs::read(spec.session.as_ref().unwrap().path.clone()).unwrap(),
        before
    );
    drop(client);
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_policy_has_no_query_capabilities_and_default_policy_never_grants_mutation_or_receipt_access(
) {
    for enabled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let scope = ControlScopeId::new();
        let mut config = policy(scope, BTreeSet::new(), vec![]);
        config.permissions = AdministrativePermissions::default();
        let control = control(root.path());
        let mut daemon = HostDaemon::open_configured(
            root.path().join("host"),
            HostConfig::default(),
            control.clone(),
            vec![],
            enabled.then_some(config),
        )
        .await
        .unwrap();
        let mut client = connect(&control).await;
        assert_eq!(
            client
                .welcome()
                .supports(wire::Capability::AdministrativeReceipts),
            enabled
        );
        let query = receipt_command(&LifecycleRequest {
            scope,
            request_id: RequestId::new(),
            epoch: daemon.service().catalog().epoch(),
            action: LifecycleAction::Stop {},
        });
        let result = client.call(query).await.unwrap_err();
        if enabled {
            assert_eq!(result, ClientError::Rejected(wire::ErrorCode::Unauthorized));
            assert_eq!(
                client
                    .call(wire::Command::Profiles {
                        after: None,
                        limit: 1
                    })
                    .await
                    .unwrap()["profiles"],
                json!([])
            );
        } else {
            assert_eq!(
                result,
                ClientError::NotSent(crate::host::client::Failure::UnsupportedCapability)
            );
        }
        for capability in [
            wire::Capability::Create,
            wire::Capability::Restore,
            wire::Capability::Cancel,
            wire::Capability::Close,
            wire::Capability::Stop,
        ] {
            assert!(!client.welcome().supports(capability));
        }
        assert_eq!(
            client.call(wire::Command::Status {}).await.unwrap()["registered"],
            0
        );
        drop(client);
        daemon.shutdown().await.unwrap();
    }
}

#[path = "daemon_administration_execution_tests.rs"]
mod execution;
