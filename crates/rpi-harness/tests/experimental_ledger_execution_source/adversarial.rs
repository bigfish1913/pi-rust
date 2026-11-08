use super::*;
use codec::{
    chain::ChainError,
    control::{execution::ExecutionTransitionError, TransitionError},
    source::SourceError,
};

async fn raw_reset(p: &mut Pair, kind: &str) {
    let prev = p.records.last().unwrap();
    let state = prev.state.clone();
    let mut exec = prev.execution_control.clone();
    let mut extra = p.records[0].nodes[0].clone();
    if kind == "business" {
        exec = ExecutionControl::default();
    }
    let selected = p.right.session().view("main").get_leaf_id().await.unwrap();
    let scope = codec::source::read_branch(
        p.right.session(),
        "main",
        selected.as_deref(),
        &read_policy(&p.right),
    )
    .await
    .unwrap()
    .scope;
    let receipt = DurableReceipt {
        request_id: "raw-request".into(),
        actor: if kind == "permission" { "child" } else { OWNER }.into(),
        expected: rpi_harness::durable::DurableScope {
            session_id: scope.session_id,
            lane: scope.lane,
            binding_token: scope.binding_token,
            leaf_id: scope.selected_leaf_id,
            history_revision: scope.history_revision,
            history_generation: 0,
            entry_id: None,
            control_revision: state.revision,
        },
        action: ControlAction::Reset,
        run_id: None,
        changed: false,
        entry_id: "v2-2".into(),
        control_revision: state.revision,
    };
    let mut r = p.tree.push(receipt, state, exec);
    if kind == "canonical" {
        // Locally well-formed but unused old receipt copy. The full batch is not
        // the canonical Insert batch, even though its current-request lookup is.
        extra.address.entry_id = r.receipt.entry_id.clone();
        extra.address.ordinal = r.ordinal;
        extra.address.slot = r.nodes.len() as u16;
        r.nodes.push(extra);
    }
    candidate_one(&r);
    p.right
        .session()
        .append_entry(
            ProvisionedEntry {
                id: "v2-2".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: CUSTOM_TYPE.into(),
                    data: Some(serde_json::to_value(&r).unwrap()),
                },
            },
            "main",
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn physical_canonical_business_and_permission_corruption_do_not_yield_a_prefix() {
    for kind in ["canonical", "business", "permission"] {
        let mut p = Pair::new(false).await;
        p.run("run", RunStatus::Interrupted, false).await;
        p.act(owner(), ControlAction::Recover, Some("run")).await;
        raw_reset(&mut p, kind).await;
        let h = Arc::new(p.right);
        let probe = probe::Probe::new(h.session().storage());
        h.session().set_storage(probe.clone());
        let before = audit(&h).await;
        probe.reset();
        let result = inspect_execution_branch(&h, "main", Some("v2-2"), &policy(&h)).await;
        let expected = if kind == "canonical" {
            TransitionError::Chain(ChainError::Ledger)
        } else if kind == "permission" {
            TransitionError::Permission
        } else {
            TransitionError::Transition
        };
        assert!(
            matches!(result,Err(ExecutionBranchError::Transition(ExecutionTransitionError::Control(ref e))) if *e==expected),
            "{kind}: {result:?}"
        );
        assert_eq!(
            probe.page_calls.load(SeqCst),
            usize::from(kind != "canonical"),
            "canonical precheck must precede native reads"
        );
        assert_eq!(probe.writes.load(SeqCst), 0);
        assert_eq!(audit(&h).await, before);
    }
}
#[tokio::test]
async fn early_mixed_source_errors_are_still_outer_fenced_with_read_precedence() {
    bounded(async {for final_fault in [false,true] {
        let h=Arc::new(harness(memory_store("synthetic-v2")).await);
        h.session().append_entry(ProvisionedEntry {id:"mixed".into(),kind:ProvisionedKind::Custom {custom_type:"rpi.durable.control.v1".into(),data:None}},"main").await.unwrap();
        let p=probe::Probe::new(h.session().storage());h.session().set_storage(p.clone());p.reset();
        assert!(matches!(inspect_execution_branch(&h,"main",Some("mixed"),&policy(&h)).await,Err(ExecutionBranchError::Source(SourceError::Mixed))));
        assert_eq!(p.revision_calls.load(SeqCst),4);assert_eq!(p.lane_calls.load(SeqCst),4);assert_eq!(p.page_calls.load(SeqCst),0);
        p.reset();p.pause_revision_at.store(3,SeqCst);if final_fault {p.fail_lane_at.store(4,SeqCst);}
        let reader=h.clone();let task=tokio::spawn(async move {inspect_execution_branch(&reader,"main",Some("mixed"),&policy(&reader)).await});
        bounded(p.captured.notified()).await;external(&h,&p,"append").await;p.release.notify_one();
        let out=bounded(task).await.unwrap();
        assert!(matches!(out,Err(ExecutionBranchError::Source(e)) if e==if final_fault {SourceError::Read}else{SourceError::Conflict}));
        assert_eq!(p.revision_calls.load(SeqCst),4);assert_eq!(p.lane_calls.load(SeqCst),4);assert_eq!(p.writes.load(SeqCst),0);
    }}).await
}
#[tokio::test]
async fn physical_256_record_source_succeeds_and_257_is_not_truncated_success() {
    bounded(async {
        let h = harness(memory_store("synthetic-v2")).await;
        let scope = codec::source::read_branch(h.session(), "main", None, &read_policy(&h))
            .await
            .unwrap()
            .scope;
        assert_eq!(scope.history_revision, 0);
        let mut tree = Tree::default();
        let mut previous = None;
        for i in 0..257u64 {
            let id = format!("noop-{i}");
            let receipt = DurableReceipt {
                request_id: format!("request-{id}"),
                actor: OWNER.into(),
                expected: rpi_harness::durable::DurableScope {
                    session_id: scope.session_id.clone(),
                    lane: "main".into(),
                    binding_token: scope.binding_token.clone(),
                    leaf_id: previous,
                    history_revision: i,
                    history_generation: 0,
                    entry_id: None,
                    control_revision: 0,
                },
                action: ControlAction::Reset,
                run_id: None,
                changed: false,
                entry_id: id.clone(),
                control_revision: 0,
            };
            let r = tree.push(
                receipt,
                ControlState::default(),
                ExecutionControl::default(),
            );
            candidate_one(&r);
            h.session()
                .append_entry(
                    ProvisionedEntry {
                        id: id.clone(),
                        kind: ProvisionedKind::Custom {
                            custom_type: CUSTOM_TYPE.into(),
                            data: Some(serde_json::to_value(&r).unwrap()),
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            previous = Some(id);
            if i == 255 {
                let before = audit(&h).await;
                let out = inspect_execution_branch(&h, "main", previous.as_deref(), &policy(&h))
                    .await
                    .unwrap();
                assert_eq!(out.transitions().ledger().receipts(), 256);
                assert_eq!(out.transitions().state(), &ControlState::default());
                assert_eq!(audit(&h).await, before);
            }
        }
        let before = audit(&h).await;
        assert!(matches!(
            inspect_execution_branch(&h, "main", previous.as_deref(), &policy(&h)).await,
            Err(ExecutionBranchError::Source(SourceError::Limit))
        ));
        assert_eq!(audit(&h).await, before);
    })
    .await
}

#[tokio::test]
async fn a_sibling_native_run_is_not_authorized_by_its_shared_source_prefix() {
    bounded(async {
        let mut p = Pair::new(false).await;
        p.run("run", RunStatus::Interrupted, false).await;
        p.act(owner(), ControlAction::Recover, Some("run")).await;
        p.right
            .session()
            .append_entry(
                ProvisionedEntry {
                    id: "first-tail".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::User(UserMessage::new("first branch", 0)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        p.right
            .session()
            .storage()
            .move_lane("main", Some("v2-1"))
            .await
            .unwrap();
        p.run("sibling", RunStatus::Interrupted, false).await;
        p.right
            .session()
            .storage()
            .move_lane("main", Some("first-tail"))
            .await
            .unwrap();
        let scope = codec::source::read_branch(
            p.right.session(),
            "main",
            Some("first-tail"),
            &read_policy(&p.right),
        )
        .await
        .unwrap()
        .scope;
        let all = p
            .right
            .session()
            .storage()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        let started = all
            .iter()
            .find_map(|r| match r {
                LaneRecord::OperationStarted(s) if s.base.id == "sibling" => Some(s.base.seq),
                _ => None,
            })
            .unwrap();
        let mut state = p.records[0].state.clone();
        state.revision += 1;
        let exec = ExecutionControl {
            forgotten_run_ids: vec![],
            recovery_request: Some(rpi_durable::execution::RecoveryRequest {
                run_id: "sibling".into(),
                started_seq: started,
                observed_history_revision: scope.history_revision,
                unknown_invocation_ids: vec![],
            }),
        };
        let receipt = DurableReceipt {
            request_id: "sibling-request".into(),
            actor: OWNER.into(),
            expected: rpi_harness::durable::DurableScope {
                session_id: scope.session_id,
                lane: scope.lane,
                binding_token: scope.binding_token,
                leaf_id: scope.selected_leaf_id,
                history_revision: scope.history_revision,
                history_generation: 0,
                entry_id: None,
                control_revision: p.records[0].state.revision,
            },
            action: ControlAction::Recover,
            run_id: Some("sibling".into()),
            changed: true,
            entry_id: "v2-2".into(),
            control_revision: state.revision,
        };
        let record = p.tree.push(receipt, state, exec);
        candidate_one(&record);
        p.right
            .session()
            .append_entry(
                ProvisionedEntry {
                    id: "v2-2".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: CUSTOM_TYPE.into(),
                        data: Some(serde_json::to_value(&record).unwrap()),
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let before = audit(&p.right).await;
        assert!(matches!(
            inspect_execution_branch(&p.right, "main", Some("v2-2"), &policy(&p.right)).await,
            Err(ExecutionBranchError::Native(_))
        ));
        let valid =
            inspect_execution_branch(&p.right, "main", Some("first-tail"), &policy(&p.right))
                .await
                .unwrap();
        assert_eq!(valid.transitions().state(), &p.records[0].state);
        assert_eq!(
            valid.transitions().execution_control(),
            &p.records[0].execution_control
        );
        assert_eq!(
            valid.scope().selected_leaf_id.as_deref(),
            Some("first-tail")
        );
        assert_eq!(valid.scope().current_leaf_id.as_deref(), Some("v2-2"));
        assert_eq!(audit(&p.right).await, before);
    })
    .await
}
