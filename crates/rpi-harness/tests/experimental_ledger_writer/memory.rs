use super::*;

#[tokio::test]
async fn explicit_history_checkpoint_publication_is_opt_in() {
    let harness = memory_v2("checkpoint-api").await;
    let service = owner(&harness);
    let checkpoint = SessionState::new()
        .history_checkpoint(HistoryIdentity {
            revision: 0,
            generation: 0,
        })
        .unwrap()
        .with_receipt_archive(ReceiptArchive::empty())
        .unwrap();
    assert_eq!(
        service
            .publish_history_checkpoint(checkpoint)
            .await
            .unwrap(),
        HistoryIdentity {
            revision: 0,
            generation: 1,
        }
    );
    assert!(harness
        .session()
        .storage()
        .history_checkpoint()
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn memory_archive_lookup_is_historical_and_not_current_write_authorization() {
    let session = memory_session("archive-memory");
    let storage = session.storage();
    let (checkpoint, receipt) = archived_checkpoint("archive-memory");
    storage
        .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
        .await
        .unwrap();

    let harness = open(session, Some(policy(EPOCH))).await;
    let service = owner(&harness);
    assert_eq!(
        service.request_outcome(&receipt.request_id).await.unwrap(),
        Some(receipt.clone())
    );
    let request = DurableMutation {
        request_id: receipt.request_id,
        expected: receipt.expected,
        action: receipt.action,
        run_id: None,
    };
    assert!(matches!(
        service.mutate(request).await,
        Err(DurableError::Conflict)
    ));
}
