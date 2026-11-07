use super::*;

#[tokio::test]
async fn jsonl_reads_rejections_and_reopen_preserve_bytes_and_recover_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = rpi_harness::session::jsonl::JsonlSessionStorage::create(
        fs.clone(),
        "writer.jsonl",
        rpi_harness::session::jsonl::JsonlV4Header::new(
            "writer-jsonl".into(),
            1,
            "/synthetic".into(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = open(Session::new(Arc::new(store), None), Some(policy(EPOCH))).await;
    let s = owner(&h);
    let accepted = apply(&s, "persisted", start()).await;
    s.drain().await.unwrap();
    let path = dir.path().join("writer.jsonl");
    let bytes = std::fs::read(&path).unwrap();
    let before = audit(&h, CUSTOM_TYPE).await;
    assert_canonical_v2(&before.0);

    assert_eq!(s.snapshot().await.unwrap().state, accepted.snapshot.state);
    assert_eq!(
        s.request_outcome("persisted").await.unwrap(),
        Some(accepted.receipt.clone())
    );
    let other = DurableService::bind(&h, DurableActor::child("other").unwrap());
    let mismatch = request(&other, "persisted", start(), None).await;
    assert!(matches!(
        other.mutate(mismatch).await,
        Err(DurableError::PermissionDenied)
    ));
    assert_eq!(audit(&h, CUSTOM_TYPE).await, before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let original_binding = s.snapshot().await.unwrap().scope.binding_token;
    // `other` retains the harness/session Arc even after the owner service is
    // dropped; release every old binding before reopening the OS journal.
    drop(other);
    drop(s);
    drop(h);
    let loaded = rpi_harness::session::jsonl::JsonlSessionStorage::load(
        fs,
        "writer.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let reopened = open(Session::new(Arc::new(loaded), None), Some(policy(EPOCH))).await;
    let reopened_service = owner(&reopened);
    let reopened_snapshot = reopened_service.snapshot().await.unwrap();
    assert_eq!(reopened_snapshot.state, accepted.snapshot.state);
    assert_ne!(reopened_snapshot.scope.binding_token, original_binding);
    assert_eq!(
        reopened_service.request_outcome("persisted").await.unwrap(),
        Some(accepted.receipt)
    );
    assert_eq!(audit(&reopened, CUSTOM_TYPE).await, before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[tokio::test]
async fn checkpoint_archive_is_read_as_history_but_never_as_current_write_proof() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = rpi_harness::session::jsonl::JsonlSessionStorage::create(
        fs.clone(),
        "archive.jsonl",
        rpi_harness::session::jsonl::JsonlV4Header::new(
            "archive-jsonl".into(),
            1,
            "/synthetic".into(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let history = HistoryIdentity {
        revision: 0,
        generation: 0,
    };
    let expected = DurableScope {
        session_id: "archive-jsonl".into(),
        lane: "main".into(),
        binding_token: "old-binding".into(),
        leaf_id: None,
        history_revision: history.revision,
        history_generation: history.generation,
        entry_id: None,
        control_revision: 0,
    };
    let receipt = DurableReceipt {
        request_id: "archived-request".into(),
        actor: OWNER.into(),
        expected: expected.clone(),
        action: serde_json::from_value(start()).unwrap(),
        run_id: None,
        entry_id: "archived-entry".into(),
        control_revision: 1,
        changed: true,
    };
    let archived = rpi_harness::durable::experimental_ledger::archive::ArchivedReceipt::new(
        receipt.clone(),
        history,
        1,
    )
    .unwrap();
    let archive =
        rpi_harness::durable::experimental_ledger::archive::ReceiptArchive::from_entries(vec![
            archived,
        ])
        .unwrap();
    let checkpoint = SessionState::new()
        .history_checkpoint(history)
        .unwrap()
        .with_receipt_archive(archive)
        .unwrap();
    store
        .replace_history_with_checkpoint_if_revision_and_generation(0, 0, checkpoint)
        .await
        .unwrap();

    let h = open(Session::new(Arc::new(store), None), Some(policy(EPOCH))).await;
    let s = owner(&h);
    assert_eq!(
        s.request_outcome("archived-request").await.unwrap(),
        Some(receipt.clone())
    );

    let request = DurableMutation {
        request_id: receipt.request_id.clone(),
        expected,
        action: receipt.action.clone(),
        run_id: None,
    };
    assert!(matches!(
        s.mutate(request).await,
        Err(DurableError::Conflict)
    ));
}
