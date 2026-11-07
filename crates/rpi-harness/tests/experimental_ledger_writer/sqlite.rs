use super::*;

#[tokio::test]
async fn sqlite_archive_lookup_is_historical_and_not_current_write_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SqliteSessionRepo::with_clock_ids(
        dir.path().join("archive.sqlite"),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .unwrap();
    let storage = repo
        .create(&rpi_harness::session::types::SessionCreateOptions {
            id: Some("archive-sqlite".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    let (checkpoint, receipt) = archived_checkpoint("archive-sqlite");
    storage
        .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
        .await
        .unwrap();

    let harness = open(Session::new(Arc::new(storage), None), Some(policy(EPOCH))).await;
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
