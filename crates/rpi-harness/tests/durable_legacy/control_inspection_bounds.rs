//! Typed-analysis limits are not authorization/source authenticity checks.
use super::*;
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{DurableActor, DurableService},
};

#[tokio::test]
async fn inspector_rechecks_typed_metadata_bounds_and_does_not_adopt_a_candidate() {
    let s = memory();
    let src = trace(&s, 1).await;
    let before = audit(&s).await;
    for (path, value) in [
        ("/seq", json!(0)),
        ("/seq", json!(u64::MAX)),
        ("/id", json!("")),
        ("/type", json!("message")),
        ("/customType", json!("durable-state.future")),
        ("/timestamp", json!("not-a-timestamp")),
        ("/parentId", json!("")),
    ] {
        reject(&patch(&src, 0, path, value), Error::InvalidSource);
    }
    let mut duplicate = src.clone();
    duplicate.canonical_entries[1]["id"] = duplicate.canonical_entries[0]["id"].clone();
    reject(&duplicate, Error::InvalidSource);
    let mut reversed = src.clone();
    reversed.canonical_entries.reverse();
    reject(&reversed, Error::UnanchoredHistory);
    for field in 0..5 {
        let mut bad = src.clone();
        match field {
            0 => bad.scope.session_id.clear(),
            1 => bad.scope.lane.clear(),
            2 => bad.scope.binding_token.clear(),
            3 => bad.scope.selected_leaf_id = None,
            4 => bad.scope.history_revision = 0,
            _ => unreachable!(),
        };
        reject(&bad, Error::InvalidSource);
    }
    let mut too_many = src.clone();
    too_many.canonical_entries = vec![src.canonical_entries[0].clone(); 257];
    too_many.inspected_custom_entries = 257;
    reject(&too_many, Error::LimitExceeded);
    assert_eq!(audit(&s).await, before);

    // Positive inclusive count boundary from a real Session, not duplicated IDs.
    let mut anchor = "oracle-1".to_owned();
    for revision in 2..=255 {
        let id = format!("limit-{revision}");
        append(
            &s,
            &id,
            "durable-state",
            Some(complete_snapshot(
                revision,
                revision - 1,
                Some(&anchor),
                json!({"revision":revision,"nextRunId":0,"nextQueueId":0,"queue":[]}),
            )),
        )
        .await;
        anchor = id;
    }
    let exact = source(&s, &anchor).await;
    assert_eq!(exact.canonical_entries.len(), 256);
    let before = audit(&s).await;
    let inspected = inspect_legacy_control(&exact).unwrap();
    assert_eq!(inspected.projection.revision, 255);
    assert_eq!(inspected.canonical_entry_ids.len(), 256);
    assert_eq!(audit(&s).await, before);

    let h = AgentHarness::create(Default::default()).await.unwrap();
    append_frame(h.session(), &frames()[0]).await;
    let before = audit(h.session()).await;
    let input = source(h.session(), "oracle-0").await;
    let candidate = inspect_legacy_control(&input).unwrap();
    assert_eq!(candidate.projection, ControlState::default());
    let native = DurableService::bind(&h, DurableActor::owner("synthetic-inspector").unwrap());
    assert!(matches!(
        native.snapshot().await,
        Err(DurableError::LegacyJournalPresent)
    ));
    assert_eq!(audit(h.session()).await, before);
    h.close().await.unwrap();
}

#[tokio::test]
async fn exact_sum_byte_limit_accepts_a_valid_typed_projection_and_plus_one_rejects_atomically() {
    bounded(async {
        let s=memory();append_frame(&s,&frames()[0]).await;
        let mut anchor="oracle-0".to_owned();
        for index in 1..=17 {
            let id=format!("snapshot-{index}");
            append(&s,&id,"durable-state",Some(complete_snapshot(index,index-1,Some(&anchor),
                json!({"revision":index,"nextRunId":0,"nextQueueId":0,"queue":[]})))).await;
            anchor=id;
        }
        let original=source(&s,&anchor).await;
        let audit_before=audit(&s).await;
        // Intentionally edit a typed COPY to exercise pure inspector budgeting.
        // This is not proof the edited values came from Session, nor an import permit.
        let mut sized=original.clone();
        for (index,entry) in sized.canonical_entries.iter_mut().enumerate().skip(1) {
            entry["data"]["state"]=json!({"revision":index,"nextRunId":0,"nextQueueId":32,
                "queue":(1..=32).map(|i|json!({"id":format!("item-{i}"),"text":"x","status":"queued",
                    "createdRevision":1,"updatedRevision":1})).collect::<Vec<_>>()});
        }
        fn bytes(src:&LegacyBranchSource)->usize {src.canonical_entries.iter().map(|entry|serde_json::to_vec(entry).unwrap().len()).sum()}
        let mut remaining=MAX_LEGACY_BRANCH_BYTES-bytes(&sized);
        assert!(remaining<=17*32*3999);
        for entry in sized.canonical_entries.iter_mut().skip(1) {
            for item in entry["data"]["state"]["queue"].as_array_mut().unwrap() {
                let add=remaining.min(3999);item["text"]=json!("x".repeat(1+add));remaining-=add;
            }
        }
        assert_eq!(remaining,0);assert_eq!(bytes(&sized),MAX_LEGACY_BRANCH_BYTES);
        let before=serde_json::to_value(&sized).unwrap();
        let candidate=inspect_legacy_control(&sized).unwrap();
        assert_eq!(candidate.projection.revision,17);assert_eq!(candidate.projection.next_queue_id,32);
        assert_eq!(serde_json::to_value(&sized).unwrap(),before);
        let last=sized.canonical_entries.last_mut().unwrap();
        let item=last["data"]["state"]["queue"].as_array_mut().unwrap().last_mut().unwrap();
        let text=item["text"].as_str().unwrap().to_owned()+"y";assert!(text.len()<=4000);item["text"]=json!(text);
        assert_eq!(bytes(&sized),MAX_LEGACY_BRANCH_BYTES+1);reject(&sized,Error::LimitExceeded);
        assert_eq!(source(&s,&anchor).await,original);assert_eq!(audit(&s).await,audit_before);
    }).await;
}
