use super::*;
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};

pub(crate) async fn fixture(id: &str, disk: bool) -> (Session, Option<tempfile::TempDir>) {
    if !disk {
        return (memory_session(id), None);
    }
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let storage = JsonlSessionStorage::create(
        fs,
        "session.jsonl",
        JsonlV4Header::new(id.into(), 1, "/synthetic".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    (Session::new(Arc::new(storage), None), Some(dir))
}
