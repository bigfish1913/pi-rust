use super::*;
use std::sync::mpsc;

struct AdmissionClock {
    started: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Clock for AdmissionClock {
    fn now_ms(&self) -> i64 {
        if let Some(sender) = self.started.lock().unwrap().take() {
            sender.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        42
    }
}
#[tokio::test]
async fn lifecycle_owner_retains_gate_after_create_caller_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let repo = ExperimentalNativeRepo::with_clock(
        dir.path().to_owned(),
        Arc::new(AdmissionClock {
            started: Mutex::new(Some(start_tx)),
            release: Mutex::new(release_rx),
        }),
    )
    .await
    .unwrap();
    let writer = repo.clone();
    let create = tokio::spawn(async move {
        writer
            .create(&SessionCreateOptions {
                id: Some("cancelled-create".into()),
                ..Default::default()
            })
            .await
    });
    tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
        .await
        .unwrap();
    create.abort();
    let _ = create.await;
    let listing = repo.list();
    tokio::pin!(listing);
    assert!(
        futures::poll!(&mut listing).is_pending(),
        "list must wait for the admitted create owner"
    );
    release_tx.send(()).unwrap();
    let catalog = listing.await.unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "cancelled-create");
    let storage = repo.open(&catalog[0]).await.unwrap();
    assert_eq!(storage.history_revision().await.unwrap(), 0);
    assert!(storage.read_progress("run").await.unwrap().is_empty());
}
#[tokio::test]
async fn failed_initialization_does_not_publish_a_partial_schema_or_overwrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rejected.rpinative");
    let identity = Identity {
        session: "rejected".into(),
        generation: "generation".into(),
    };
    let result = Store::create_initialized(&path, identity.clone(), |db, identity| {
        publish_aux(
            db,
            identity,
            "metadata",
            &SessionMetadata {
                id: "rejected".into(),
                created_at: 42,
                parent_session_id: None,
            },
        )?;
        Err("injected initialization failure".into())
    });
    assert!(result.is_err());
    let bytes = std::fs::read(&path).unwrap();
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
    drop(db);
    assert!(Store::open(&path, identity.clone()).is_err());
    assert!(Store::create(&path, identity).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
#[tokio::test]
async fn catalog_rejects_a_misnamed_or_foreign_experimental_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("fixture".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    drop(storage);
    std::fs::rename(
        dir.path().join("fixture.rpinative"),
        dir.path().join("mismatch.rpinative"),
    )
    .unwrap();
    assert!(repo.list().await.is_err());
    std::fs::rename(
        dir.path().join("mismatch.rpinative"),
        dir.path().join("fixture.rpinative"),
    )
    .unwrap();
    assert_eq!(repo.list().await.unwrap().len(), 1);
    let path = dir.path().join("foreign.rpinative");
    std::fs::write(&path, b"not sqlite").unwrap();
    assert!(repo.list().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
}
