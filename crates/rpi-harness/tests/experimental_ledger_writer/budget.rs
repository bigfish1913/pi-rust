use super::*;

#[tokio::test]
async fn v2_allows_256_noop_receipts_and_replays_at_the_limit_without_append() {
    let h = memory_v2("writer-limit").await;
    let s = owner(&h);
    let mut last = None;
    for i in 0..256 {
        let mutation = request(&s, &format!("reset-{i}"), json!({"action":"reset"}), None).await;
        let outcome = apply_request(&s, mutation.clone()).await.unwrap();
        assert!(!outcome.replayed);
        assert!(!outcome.receipt.changed);
        assert_eq!(outcome.snapshot.state.revision, 0);
        last = Some(mutation);
    }
    let entries = custom_entries(&h, CUSTOM_TYPE).await;
    assert_eq!(entries.len(), 256);
    assert_canonical_v2(&entries);
    let before = audit(&h, CUSTOM_TYPE).await;
    let replay = apply_request(&s, last.unwrap()).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(audit(&h, CUSTOM_TYPE).await, before);

    let before = s.snapshot().await.unwrap();
    let next = request(&s, "reset-256", json!({"action":"reset"}), None).await;
    assert!(matches!(
        apply_request(&s, next).await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 256);
}

#[tokio::test]
async fn rejected_request_budget_does_not_poison_a_later_small_append() {
    let h = memory_v2("writer-request-budget").await;
    let s = owner(&h);
    let before = audit(&h, CUSTOM_TYPE).await;
    let mutation = request(
        &s,
        "oversized",
        json!({"action":"enqueue", "text":"x".repeat(70_000)}),
        None,
    )
    .await;
    assert!(matches!(
        apply_request(&s, mutation).await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(audit(&h, CUSTOM_TYPE).await, before);
    let small = apply(
        &s,
        "after-budget",
        json!({"action":"enqueue", "text":"small"}),
    )
    .await;
    assert_eq!(small.snapshot.state.queue.last().unwrap().text, "small");
    assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 1);
}

#[tokio::test]
async fn prospective_full_entry_budget_rejects_before_append_and_retains_readable_branch() {
    let h = memory_v2("writer-branch-budget").await;
    let s = owner(&h);
    let limit = experimental_ledger::source::MAX_BRANCH_BYTES;
    h.session()
        .append_custom_entry("unrelated-blob", Some(json!("x".repeat(limit - 500))))
        .await
        .unwrap();
    let before = s.snapshot().await.unwrap();
    let audit_before = audit(&h, CUSTOM_TYPE).await;
    let mutation = request(&s, "would-overflow", json!({"action":"reset"}), None).await;
    assert!(matches!(
        s.mutate(mutation).await,
        Err(DurableError::LimitExceeded)
    ));
    assert_eq!(s.snapshot().await.unwrap(), before);
    assert_eq!(audit(&h, CUSTOM_TYPE).await, audit_before);
}
