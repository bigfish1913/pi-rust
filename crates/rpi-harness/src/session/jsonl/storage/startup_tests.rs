use super::*;
use crate::session::memory::{CounterIdGenerator, FakeClock};
use std::io::Write;

struct Fixture {
    directory: tempfile::TempDir,
    path: String,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join("history.jsonl")
            .to_string_lossy()
            .into_owned();
        let mut history = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(
            history,
            "{}",
            serde_json::json!({
                "kind": "header", "version": 4, "id": "test", "createdAt": 0,
                "cwd": directory.path(),
            })
        )
        .unwrap();
        let content = "x".repeat(256 * 1024);
        for seq in 1..=17 {
            writeln!(
                history,
                "{}",
                serde_json::json!({
                    "kind": "entry", "type": "message", "id": format!("m{seq}"),
                    "seq": seq, "timestamp": 0, "lane": "main",
                    "parentId": (seq > 1).then(|| format!("m{}", seq - 1)),
                    "message": {"kind": "user", "role": "user", "timestamp": 0, "content": content},
                })
            )
            .unwrap();
        }
        history.flush().unwrap();
        Self { directory, path }
    }

    fn image(&self) -> String {
        format!("{}{}", self.path, startup_checkpoint::SUFFIX)
    }

    async fn prepare(&self) -> SessionResult<PreparedJsonlRestore> {
        JsonlSessionStorage::prepare_restore(
            Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
                self.directory.path().into(),
            )),
            &self.path,
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
    }

    fn append(&self, mutation: serde_json::Value) {
        let mut history = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(history, "{mutation}").unwrap();
    }
}

#[tokio::test]
async fn startup_image_skips_the_prefix_replays_suffix_and_is_read_only_until_commit() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare().await.unwrap();
    assert!(!std::path::Path::new(&fixture.image()).exists());
    assert!(prepared
        .storage
        .startup_checkpoint
        .unwrap()
        .reused
        .is_none());
    let storage = prepared.commit().await.unwrap();
    let prefix = std::fs::read(&fixture.path).unwrap();
    let index_bytes = std::fs::read(fixture.image()).unwrap();
    assert!(index_bytes.len() < prefix.len() / 10);
    assert_eq!(
        storage
            .lane_branch("main")
            .await
            .unwrap()
            .leaf_id
            .as_deref(),
        Some("m17")
    );
    drop(storage);

    fixture.append(serde_json::json!({
        "kind": "entry", "type": "message", "id": "m18", "seq": 18,
        "timestamp": 0, "lane": "main", "parentId": "m17",
        "message": {"kind": "user", "role": "user", "timestamp": 0, "content": "appended"},
    }));
    let prepared = fixture.prepare().await.unwrap();
    let reused = prepared.storage.startup_checkpoint.unwrap().reused.unwrap();
    assert_eq!(reused.bytes, prefix.len() as u64);
    let storage = prepared.commit().await.unwrap();
    assert_eq!(
        storage
            .lane_branch("main")
            .await
            .unwrap()
            .leaf_id
            .as_deref(),
        Some("m18")
    );
    let page = storage
        .transcript_page(
            "main",
            &TranscriptPageQuery {
                limit: 2,
                before_id: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.total, 18);
    assert_eq!(
        page.entries.iter().map(Entry::id).collect::<Vec<_>>(),
        ["m17", "m18"]
    );
    assert_eq!(std::fs::read(fixture.image()).unwrap(), index_bytes);
}

#[tokio::test]
async fn startup_image_falls_back_for_corrupt_image_changed_prefix_and_duplicate_tail() {
    let fixture = Fixture::new();
    drop(fixture.prepare().await.unwrap().commit().await.unwrap());
    let image = std::fs::read(fixture.image()).unwrap();
    let mut damaged = image.clone();
    *damaged.last_mut().unwrap() ^= 1;
    std::fs::write(fixture.image(), damaged).unwrap();
    let prepared = fixture.prepare().await.unwrap();
    assert!(prepared
        .storage
        .startup_checkpoint
        .unwrap()
        .reused
        .is_none());
    drop(prepared);
    std::fs::write(fixture.image(), &image).unwrap();

    let text = std::fs::read_to_string(&fixture.path).unwrap();
    let duplicate: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    fixture.append(duplicate);
    let prepared = fixture.prepare().await.unwrap();
    assert!(prepared.storage.startup_checkpoint.is_none());
    assert!(prepared.storage.pending_repair.lock().unwrap().is_some());
    drop(prepared);
    std::fs::write(&fixture.path, text.replacen("xxxxx", "yyyyy", 1)).unwrap();
    let prepared = fixture.prepare().await.unwrap();
    assert!(prepared
        .storage
        .startup_checkpoint
        .unwrap()
        .reused
        .is_none());
    let storage = prepared.commit().await.unwrap();
    let entry = storage.get_entry("m1").await.unwrap().unwrap();
    let Some(rpi_agent::message::AgentMessage::User(message)) = entry.as_message() else {
        panic!("expected user message")
    };
    assert!(serde_json::to_string(&message.content)
        .unwrap()
        .starts_with("\"yyyyy"));
}
