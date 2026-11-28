#![cfg(unix)]

use rpi_agent::message::AgentMessage;
use rpi_ai::types::UserMessage;
use rpi_harness::session::{
    jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions, JsonlSessionStorage,
    },
    memory::{CounterIdGenerator, FakeClock},
    types::{ProvisionedEntry, ProvisionedKind, SessionStorage},
};
use serde_json::{json, Value};
use std::{
    fs::{self, File, FileTimes, OpenOptions},
    io::Write,
    os::unix::fs::symlink,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};

const ID: &str = "bc348811-acf1-4317-9b82-12f73cadfe91";

struct Fixture {
    cwd: PathBuf,
    root: PathBuf,
    repo: JsonlSessionRepo,
}

impl Fixture {
    fn new(temp: &Path) -> Self {
        let cwd = temp.canonicalize().unwrap();
        let root = cwd.join("sessions");
        let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
            fs: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(cwd.clone())),
            sessions_root: root.to_str().unwrap().into(),
            clock: Arc::new(FakeClock::new()),
            ids: Arc::new(CounterIdGenerator::new()),
        });
        Self { cwd, root, repo }
    }

    async fn create(&self) -> JsonlSessionStorage {
        self.repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some(ID.into()),
                parent_session_id: None,
                cwd: self.cwd.to_str().unwrap().into(),
                metadata: None,
            })
            .await
            .unwrap()
    }

    async fn invoke(&self, id: &str, extra: &[&str]) -> Output {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
                .current_dir(&self.cwd)
                .env("RPI_CODING_AGENT_DIR", self.cwd.join("agent"))
                .args(["--session-preview", id, "--session-dir"])
                .arg(&self.root)
                .args(extra)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("preview must exit independently of startup")
        .unwrap()
    }

    async fn preview(&self) -> Value {
        let output = self.invoke(ID, &[]).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    async fn miss(&self) {
        let output = self.invoke(ID, &[]).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

fn message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage::new(text, 1))
}

async fn append(storage: &JsonlSessionStorage, id: &str, lane: &str) {
    storage
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Message {
                    message: message(id),
                    terminate: None,
                },
            },
            lane,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_projects_the_latest_main_window_and_rebuilds_on_restore() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(temp.path());
    let storage = fixture.create().await;
    let meta = storage.jsonl_metadata();
    let index = format!("{}.preview.json", meta.path);
    let stamp = format!("{}.preview.stamp", meta.path);
    assert!(!Path::new(&index).exists());
    fixture.miss().await;

    for number in 0..43 {
        append(&storage, &format!("message-{number}"), "main").await;
    }
    let compaction = storage
        .append_entry(
            ProvisionedEntry {
                id: "compact".into(),
                kind: ProvisionedKind::Compaction {
                    summary: "summary".into(),
                    retained_tail: vec![message("retained")],
                    tokens_before: 320,
                    details: None,
                    usage: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    storage
        .create_lane("side", Some("message-1"))
        .await
        .unwrap();
    append(&storage, "side-only", "side").await;
    assert!(Path::new(&stamp).is_file());
    let before = fixture.preview().await;
    let rows = before["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 40);
    assert_eq!(before["hasMore"], true);
    assert_eq!(rows[0]["entryId"], "message-4");
    assert_eq!(rows[0]["role"], "user");
    assert_eq!(rows[0]["content"], "message-4");
    assert_eq!(
        rows[39],
        json!({"role": "compaction", "entryId": "compact", "tokensBefore": 320,
        "retainedMessages": 1, "timestamp": compaction.base().timestamp})
    );
    drop(storage);

    fs::remove_file(&index).unwrap();
    fixture.miss().await;
    let prepared = fixture
        .repo
        .prepare_restore_by_jsonl_metadata(&meta)
        .await
        .unwrap();
    assert!(!Path::new(&index).exists());
    let storage = prepared.commit().await.unwrap();
    assert_eq!(fixture.preview().await, before);
    let metadata = fs::metadata(&index).unwrap();
    let storage = storage
        .enable_progress_under_exclusive_lock()
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(&index).unwrap().modified().unwrap(),
        metadata.modified().unwrap()
    );
    storage.move_lane("main", Some("message-1")).await.unwrap();
    let rewound = fixture.preview().await;
    assert_eq!(rewound["messages"].as_array().unwrap().len(), 2);
    assert_eq!(rewound["hasMore"], false);
    append(&storage, "alternate", "main").await;
    let alternate = fixture.preview().await;
    assert_eq!(alternate["messages"][2]["entryId"], "alternate");
    assert!(!fixture.cwd.join("agent").exists());

    drop(storage);
    let staging = format!("{}.preview.json.tmp", meta.path);
    fs::write(&staging, "interrupted publication").unwrap();
    fixture.repo.delete_by_jsonl_metadata(&meta).await.unwrap();
    for path in [
        meta.path.clone(),
        format!("{}.progress.jsonl", meta.path),
        index,
        stamp,
        staging,
    ] {
        assert!(!Path::new(&path).exists(), "{path}");
    }
    fixture.miss().await;
}

#[tokio::test]
async fn legacy_upgrade_rebases_preview_and_non_transcript_writes_keep_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(temp.path());
    let storage = fixture.create().await;
    append(&storage, "before-upgrade", "main").await;
    let meta = storage.jsonl_metadata();
    drop(storage);

    let path = Path::new(&meta.path);
    let index = format!("{}.preview.json", meta.path);
    let stamp = format!("{}.preview.stamp", meta.path);
    let journal = fs::read_to_string(path).unwrap();
    let (header, body) = journal.split_once('\n').unwrap();
    let mut header: Value = serde_json::from_str(header).unwrap();
    header.as_object_mut().unwrap().remove("progressFormat");
    fs::write(path, format!("{header}\n{body}")).unwrap();
    fs::remove_file(format!("{}.progress.jsonl", meta.path)).unwrap();

    let storage = fixture
        .repo
        .prepare_restore_by_jsonl_metadata(&meta)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap()
        .enable_progress_under_exclusive_lock()
        .await
        .unwrap();
    assert_eq!(
        fixture.preview().await["messages"][0]["entryId"],
        "before-upgrade"
    );
    let index_bytes = fs::read(&index).unwrap();
    let inode = fs::metadata(&index).unwrap().ino();
    for number in 0..3 {
        storage
            .append_entry(
                ProvisionedEntry {
                    id: format!("custom-{number}"),
                    kind: ProvisionedKind::Custom {
                        custom_type: "fixture".into(),
                        data: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        assert_eq!(
            fixture.preview().await["messages"][0]["entryId"],
            "before-upgrade"
        );
        assert_eq!(fs::read(&index).unwrap(), index_bytes);
        assert_eq!(fs::metadata(&index).unwrap().ino(), inode);
    }
    let proof = fs::read(&stamp).unwrap();
    fs::write(&index, [&index_bytes[..], b" "].concat()).unwrap();
    fixture.miss().await;
    fs::write(&index, index_bytes).unwrap();
    assert_eq!(fs::read(&stamp).unwrap(), proof);

    append(&storage, "after-upgrade", "main").await;
    let preview = fixture.preview().await;
    assert_eq!(preview["messages"][0]["entryId"], "before-upgrade");
    assert_eq!(preview["messages"][1]["entryId"], "after-upgrade");
}

#[tokio::test]
async fn preview_rejects_inode_and_prefix_changes_and_leaves_sources_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(temp.path());
    let storage = fixture.create().await;
    append(&storage, "first", "main").await;
    let meta = storage.jsonl_metadata();
    let path = Path::new(&meta.path);
    let index_path = format!("{}.preview.json", meta.path);
    let progress = format!("{}.progress.jsonl", meta.path);
    let lock = format!("{}.lock", meta.path);
    let baseline = fs::read(path).unwrap();
    let sidecar = fs::read(&index_path).unwrap();
    let progress_bytes = fs::read(&progress).unwrap();
    let fence = fs::read(&lock).unwrap();
    let wrong = fixture.invoke("unknown", &[]).await;
    assert_eq!(wrong.status.code(), Some(2));
    let mixed = fixture.invoke(ID, &["--session-id", ID]).await;
    assert_eq!(mixed.status.code(), Some(2));
    assert_eq!(fs::read(path).unwrap(), baseline);
    assert_eq!(fs::read(&index_path).unwrap(), sidecar);
    assert_eq!(fs::read(&progress).unwrap(), progress_bytes);
    assert_eq!(fs::read(&lock).unwrap(), fence);

    let mut index: Value = serde_json::from_slice(&sidecar).unwrap();
    index["rows"][0]["digest"][0] = json!(255 - index["rows"][0]["digest"][0].as_u64().unwrap());
    fs::write(&index_path, index.to_string()).unwrap();
    fixture.miss().await;
    fs::write(&index_path, &sidecar).unwrap();
    let saved_progress = path.with_extension("saved-progress");
    fs::rename(&progress, &saved_progress).unwrap();
    fixture.miss().await;
    symlink(&saved_progress, &progress).unwrap();
    fixture.miss().await;
    fs::remove_file(&progress).unwrap();
    fs::rename(&saved_progress, &progress).unwrap();

    let mtime = fs::metadata(path).unwrap().modified().unwrap();
    let replacement = path.with_extension("swap");
    fs::write(&replacement, &baseline).unwrap();
    File::open(&replacement)
        .unwrap()
        .set_times(FileTimes::new().set_modified(mtime))
        .unwrap();
    fs::rename(&replacement, path).unwrap();
    fixture.miss().await;
    append(&storage, "same-size-replacement", "main").await;
    fixture.miss().await;
    assert_eq!(fs::read(&index_path).unwrap(), sidecar);
    drop(storage);

    let storage = fixture
        .repo
        .prepare_restore_by_jsonl_metadata(&meta)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert_eq!(
        fixture.preview().await["messages"][1]["entryId"],
        "same-size-replacement"
    );
    let updated_index = fs::read(&index_path).unwrap();
    let baseline = fs::read_to_string(path).unwrap();
    let original_mtime = fs::metadata(path).unwrap().modified().unwrap();
    let changed = baseline.replacen("first", "ghost", 1);
    assert_ne!(changed, baseline);
    assert_eq!(changed.len(), baseline.len());
    let mut writer = OpenOptions::new().write(true).open(path).unwrap();
    writer.write_all(changed.as_bytes()).unwrap();
    writer
        .set_times(FileTimes::new().set_modified(original_mtime))
        .unwrap();
    drop(writer);
    fixture.miss().await;
    append(&storage, "prefix-changed", "main").await;
    fixture.miss().await;
    assert_eq!(fs::read(&index_path).unwrap(), updated_index);
    assert_eq!(fs::read(&progress).unwrap(), progress_bytes);
    assert!(!fixture.cwd.join("agent").exists());
}
