use super::*;
use rpi_harness::durable::DurableService;

fn same_business_state(left: &DurableOutcome, right: &DurableOutcome) {
    assert_eq!(left.snapshot.state, right.snapshot.state);
    assert_eq!(
        left.snapshot.execution_control,
        right.snapshot.execution_control
    );
    assert_eq!(
        left.snapshot.scope.control_revision,
        right.snapshot.scope.control_revision
    );
    assert_eq!(left.receipt.actor, right.receipt.actor);
    assert_eq!(left.receipt.action, right.receipt.action);
    assert_eq!(left.receipt.run_id, right.receipt.run_id);
    assert_eq!(
        left.receipt.control_revision,
        right.receipt.control_revision
    );
    assert_eq!(left.receipt.changed, right.receipt.changed);
}

async fn step(
    v1: &DurableService,
    v2: &DurableService,
    actor: usize,
    request_id: &str,
    action: Value,
    child_v1: &DurableService,
    child_v2: &DurableService,
) {
    let (a, b) = if actor == 0 {
        (v1, v2)
    } else {
        (child_v1, child_v2)
    };
    let left = apply(a, request_id, action.clone()).await;
    let right = apply(b, request_id, action).await;
    same_business_state(&left, &right);
}

#[tokio::test]
async fn actual_memory_v1_and_v2_writers_have_the_same_business_state() {
    differential(false).await;
}
#[tokio::test]
async fn actual_jsonl_v1_and_v2_writers_have_the_same_business_state() {
    differential(true).await;
}
async fn differential(disk: bool) {
    let (session1, _dir1) = fixture::fixture("writer-v1", disk).await;
    let (session2, _dir2) = fixture::fixture("writer-v2", disk).await;
    let v1_harness = open(session1, None).await;
    let v2_harness = open(session2, Some(policy(EPOCH))).await;
    let v1 = owner(&v1_harness);
    let v2 = owner(&v2_harness);
    let child_v1 = child(&v1_harness);
    let child_v2 = child(&v2_harness);

    step(&v1, &v2, 0, "start", start(), &child_v1, &child_v2).await;
    step(
        &v1,
        &v2,
        0,
        "checkpoint",
        checkpoint("in_progress"),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        0,
        "checkpoint-done",
        checkpoint("completed"),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        0,
        "complete",
        json!({"action":"complete"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        0,
        "reset",
        json!({"action":"reset"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        1,
        "enqueue-one",
        json!({"action":"enqueue", "text":"first"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        1,
        "claim-one",
        json!({"action":"claim", "item":"item-1", "claimedBy":"spoofed"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        1,
        "finish-one",
        json!({"action":"finish", "item":"item-1", "status":"completed", "note":"done"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        0,
        "enqueue-two",
        json!({"action":"enqueue", "text":"second"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        1,
        "claim-two",
        json!({"action":"claim", "item":"item-2"}),
        &child_v1,
        &child_v2,
    )
    .await;
    step(
        &v1,
        &v2,
        0,
        "cancel-two",
        json!({"action":"cancel", "item":"item-2"}),
        &child_v1,
        &child_v2,
    )
    .await;

    let v1_entries = custom_entries(&v1_harness, "rpi.durable.control.v1").await;
    let v2_entries = custom_entries(&v2_harness, CUSTOM_TYPE).await;
    assert_eq!(v1_entries.len(), 11);
    assert_eq!(v2_entries.len(), 11);
    assert_canonical_v2(&v2_entries);
    assert_eq!(
        v2.snapshot().await.unwrap().state,
        v1.snapshot().await.unwrap().state
    );
}

#[tokio::test]
async fn no_op_receipt_replays_only_at_its_current_anchor() {
    let h = memory_v2("writer-replay").await;
    let s = owner(&h);
    let mutation = request(&s, "noop", json!({"action":"reset"}), None).await;
    let first = apply_request(&s, mutation.clone()).await.unwrap();
    assert!(!first.receipt.changed);
    assert_eq!(first.receipt.control_revision, 0);
    let before = audit(&h, CUSTOM_TYPE).await;
    let replay = apply_request(&s, mutation.clone()).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first.receipt);
    assert_eq!(audit(&h, CUSTOM_TYPE).await, before);

    apply(&s, "newer", start()).await;
    assert_eq!(
        s.request_outcome("noop").await.unwrap(),
        Some(first.receipt.clone())
    );
    assert!(matches!(
        apply_request(&s, mutation).await,
        Err(DurableError::Conflict)
    ));
    assert_eq!(
        s.request_outcome("noop").await.unwrap(),
        Some(first.receipt)
    );
}
