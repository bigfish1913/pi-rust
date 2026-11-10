use super::*;
use rpi_harness::durable::DurableActor;

#[tokio::test]
async fn actor_role_membership_and_child_control_permissions_are_fixed_by_policy() {
    let h = memory_v2("writer-permissions").await;
    let root = owner(&h);
    let child_service = child(&h);
    let scope = root.snapshot().await.unwrap().scope;

    for (id, action, run_id) in [
        (
            "child-start",
            json!({"action":"start", "name":"x", "steps":[]}),
            None,
        ),
        ("child-reset", json!({"action":"reset"}), None),
        ("child-recover", json!({"action":"recover"}), Some("run")),
        ("child-forget", json!({"action":"forget_run"}), Some("run")),
    ] {
        let mutation = DurableMutation {
            request_id: id.into(),
            expected: scope.clone(),
            action: serde_json::from_value(action).unwrap(),
            run_id: run_id.map(str::to_owned),
        };
        assert!(matches!(
            child_service.mutate(mutation).await,
            Err(DurableError::PermissionDenied)
        ));
    }

    apply(
        &root,
        "enqueue",
        json!({"action":"enqueue", "text":"owned"}),
    )
    .await;
    apply(
        &child_service,
        "claim",
        json!({"action":"claim", "item":"item-1"}),
    )
    .await;
    let owner_scope = root.snapshot().await.unwrap().scope;
    let finish = DurableMutation {
        request_id: "owner-finish".into(),
        expected: owner_scope.clone(),
        action: serde_json::from_value(
            json!({"action":"finish", "item":"item-1", "status":"completed"}),
        )
        .unwrap(),
        run_id: None,
    };
    assert!(matches!(
        root.mutate(finish).await,
        Err(DurableError::PermissionDenied)
    ));
    let cancelled = apply(
        &root,
        "owner-cancel",
        json!({"action":"cancel", "item":"item-1"}),
    )
    .await;
    assert_eq!(
        cancelled.snapshot.state.queue[0].status,
        rpi_durable::control::QueueStatus::Cancelled
    );

    let configured_owner_as_child = DurableService::bind(&h, DurableActor::child(OWNER).unwrap());
    let configured_child_as_owner = DurableService::bind(&h, DurableActor::owner(CHILD).unwrap());
    for service in [configured_owner_as_child, configured_child_as_owner] {
        let mutation = DurableMutation {
            request_id: "role-mismatch".into(),
            expected: root.snapshot().await.unwrap().scope,
            action: serde_json::from_value(json!({"action":"reset"})).unwrap(),
            run_id: None,
        };
        assert!(matches!(
            service.mutate(mutation).await,
            Err(DurableError::PermissionDenied)
        ));
    }
}

#[tokio::test]
async fn mismatched_actor_action_and_scope_are_conflicts_not_new_receipts() {
    let h = memory_v2("writer-conflicts").await;
    let root = owner(&h);
    let other = DurableService::bind(&h, DurableActor::child("other").unwrap());
    let original = request(&root, "same", start(), None).await;
    apply_request(&root, original.clone()).await.unwrap();
    let count = custom_entries(&h, CUSTOM_TYPE).await.len();

    let mut actor_mismatch = original.clone();
    actor_mismatch.expected = root.snapshot().await.unwrap().scope;
    assert!(matches!(
        other.mutate(actor_mismatch).await,
        Err(DurableError::PermissionDenied)
    ));
    let mut action_mismatch = request(&root, "same", json!({"action":"reset"}), None).await;
    action_mismatch.expected = root.snapshot().await.unwrap().scope;
    assert!(matches!(
        root.mutate(action_mismatch).await,
        Err(DurableError::Conflict)
    ));

    let stale = original;
    let newer = apply(&root, "newer", json!({"action":"reset"})).await;
    assert!(matches!(
        root.mutate(stale).await,
        Err(DurableError::Conflict)
    ));
    assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), count + 1);
    assert_eq!(
        root.request_outcome("same")
            .await
            .unwrap()
            .unwrap()
            .request_id,
        "same"
    );
    assert_eq!(newer.receipt.request_id, "newer");
}

#[tokio::test]
async fn same_revision_replacement_rejects_writer_with_stale_generation() {
    let h = memory_v2("writer-generation-conflict").await;
    let root = owner(&h);
    let stale = request(&root, "stale-generation", json!({"action":"reset"}), None).await;

    h.session()
        .storage()
        .replace_history_if_revision_and_generation(
            0,
            0,
            HistoryReplacement {
                state: SessionState::new(),
                canonical_mutations: None,
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        apply_request(&root, stale).await,
        Err(DurableError::Conflict)
    ));
    assert!(custom_entries(&h, CUSTOM_TYPE).await.is_empty());
    assert_eq!(root.snapshot().await.unwrap().scope.history_revision, 0);
    assert_eq!(root.snapshot().await.unwrap().scope.history_generation, 1);
}

#[tokio::test]
async fn v1_and_v2_journals_are_not_interpreted_as_empty_or_cross_mode_history() {
    let v2 = memory_v2("mode-v2").await;
    apply(&owner(&v2), "v2-start", start()).await;
    let v2_storage = v2.session().clone();
    assert!(
        try_open(v2_storage.clone(), None).await.is_err(),
        "v1 must refuse a v2 journal rather than see empty state"
    );
    assert!(try_open(v2_storage, Some(policy("different-epoch")))
        .await
        .is_err());

    let v1 = memory_v1("mode-v1").await;
    apply(&owner(&v1), "v1-start", start()).await;
    assert!(try_open(v1.session().clone(), Some(policy(EPOCH)))
        .await
        .is_err());

    let legacy = memory_session("mode-legacy");
    legacy
        .append_custom_entry("durable-state", Some(json!({"revision":1})))
        .await
        .unwrap();
    assert!(try_open(legacy, Some(policy(EPOCH))).await.is_err());

    let mixed = memory_v2("mode-mixed").await;
    apply(&owner(&mixed), "mixed-start", start()).await;
    mixed
        .session()
        .append_custom_entry("durable-state", Some(json!({"revision":1})))
        .await
        .unwrap();
    assert!(try_open(mixed.session().clone(), Some(policy(EPOCH)))
        .await
        .is_err());
}

#[tokio::test]
async fn explicit_mode_requires_native_admission_and_trusted_owner_policy_survives_reopen() {
    assert!(ExperimentalDurablePolicy::new("epoch", vec![]).is_err());
    assert!(ExperimentalDurablePolicy::new("epoch", vec!["same".into(), "same".into()]).is_err());
    assert!(ExperimentalDurablePolicy::new("bad epoch", vec![OWNER.into()]).is_err());
    assert!(AgentHarness::create_experimental(
        AgentHarnessOptions {
            session: memory_session("writer-admission-disabled"),
            ..Default::default()
        },
        policy(EPOCH)
    )
    .await
    .is_err());
    let h = memory_v2("writer-reopen-policy").await;
    apply(&owner(&h), "privileged-start", start()).await;
    let wrong = ExperimentalDurablePolicy::new(EPOCH, vec!["unrelated-owner".into()]).unwrap();
    assert!(try_open(h.session().clone(), Some(wrong)).await.is_err());
    let reopened = open(
        Session::new(h.session().storage(), None),
        Some(policy(EPOCH)),
    )
    .await;
    assert!(owner(&reopened)
        .snapshot()
        .await
        .unwrap()
        .state
        .workflow
        .is_some());
}

#[tokio::test]
async fn mode_is_shared_by_lane_services_and_switch_navigation_admission() {
    use rpi_harness::agent_harness::AgentLane;
    let h = memory_v2("writer-navigation").await;
    let initial = apply(&owner(&h), "root", start()).await;
    h.session()
        .storage()
        .create_lane("side", Some(&initial.receipt.entry_id))
        .await
        .unwrap();
    let side = DurableService::bind_lane(&h, DurableActor::owner(OWNER).unwrap(), "side");
    // Receipts from main cannot be adopted by a different lane.
    assert!(side.snapshot().await.is_err());
    let invalid = memory_session("writer-invalid-target");
    invalid
        .append_custom_entry("durable-state", Some(json!({})))
        .await
        .unwrap();
    let before = owner(&h).snapshot().await.unwrap();
    assert!(h.set_session(invalid).await.is_err());
    assert_eq!(owner(&h).snapshot().await.unwrap(), before);
    h.session()
        .append_custom_entry("durable-state", Some(json!({})))
        .await
        .unwrap();
    let bad_leaf = h
        .session()
        .view("main")
        .get_leaf_id()
        .await
        .unwrap()
        .unwrap();
    h.session()
        .move_lane("main", Some(&initial.receipt.entry_id))
        .await
        .unwrap();
    let nav = h
        .navigate_tree(Some(&bad_leaf), false, None, None)
        .await
        .unwrap();
    assert!(matches!(
        nav.outcome,
        rpi_harness::agent_harness::NavigationOutcome::Failed { .. }
    ));
    assert_eq!(
        h.session()
            .view("main")
            .get_leaf_id()
            .await
            .unwrap()
            .as_deref(),
        Some(initial.receipt.entry_id.as_str())
    );
    let clear = h.navigate_tree(None, false, None, None).await.unwrap();
    assert!(matches!(
        clear.outcome,
        rpi_harness::agent_harness::NavigationOutcome::Completed { .. }
    ));
    assert_eq!(
        owner(&h).snapshot().await.unwrap().state,
        Default::default()
    );
}

#[tokio::test]
async fn canonical_but_forged_business_state_is_not_admitted_or_extended() {
    use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind};
    let original = memory_v2("writer-forged").await;
    let result = apply(&owner(&original), "real-start", start()).await;
    let rows = custom_entries(&original, CUSTOM_TYPE).await;
    let Entry::Custom(entry) = &rows[0] else {
        panic!("custom")
    };
    let mut data = entry.data.clone().unwrap();
    // Keep the genuine receipt/map/root and revision. Local schema and canonical
    // map are valid; only reducer replay can reject this invented post-state.
    data["state"]["workflow"] = Value::Null;
    let bad = memory_v2("writer-forged").await;
    let s = owner(&bad);
    let pending = request(&s, "must-not-append", json!({"action":"reset"}), None).await;
    bad.session()
        .storage()
        .append_entry(
            ProvisionedEntry {
                id: result.receipt.entry_id,
                kind: ProvisionedKind::Custom {
                    custom_type: CUSTOM_TYPE.into(),
                    data: Some(data),
                },
            },
            "main",
        )
        .await
        .unwrap();
    let before = audit(&bad, CUSTOM_TYPE).await;
    assert!(s.snapshot().await.is_err());
    assert!(s.mutate(pending).await.is_err());
    assert_eq!(audit(&bad, CUSTOM_TYPE).await, before);
}

#[tokio::test]
async fn experimental_policy_follows_independent_child_harness_creation() {
    let h = memory_v2("writer-child-parent").await;
    let invalid = memory_session("writer-child-legacy");
    invalid
        .append_custom_entry("durable-state", Some(json!({})))
        .await
        .unwrap();
    let emitter = Arc::new(rpi_agent::CollectorEmitter::new().0);
    assert!(h
        .create_child(invalid, emitter.clone(), 1, None)
        .await
        .is_err());
    let child = h
        .create_child(memory_session("writer-child-fresh"), emitter, 1, None)
        .await
        .unwrap();
    // Trusted embedding directly binds an owner to the independent child store;
    // this is not a child tool invocation or a permission escalation claim.
    apply(&owner(&child.harness), "child-store", start()).await;
    assert_eq!(custom_entries(&child.harness, CUSTOM_TYPE).await.len(), 1);
    assert!(owner(&h).snapshot().await.unwrap().state.workflow.is_none());
}
