//! Offline compaction of stream frames a `clear_run` already retired.
//!
//! Fork issue DavidHinton-xcpc/pi-rust#14 §4 PR-D, write-side half. The
//! invariant these probes defend is **not** "the file got smaller" but "nothing
//! observable changed": the same visible entries, the same log rows, the same
//! recovery verdict, and the same behaviour for appends that follow the rewrite.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{UserContent, UserMessage};
use rpi_harness::session::{
    jsonl::{
        compact_retired_frames, encode_header, encode_mutation, types::JsonlV4Header,
        JsonlSessionStorage,
    },
    memory::{CounterIdGenerator, FakeClock},
    types::{
        AssistantFrameOp, AssistantFrameRecord, Entry, EntryBase, LaneRecord, MessageEntry,
        RecordBase, SessionMutation, SessionStorage,
    },
};
use rpi_tools::{FileContent, FileSystem, OsExecutionEnv};

fn frame_line(seq: u64, run: &str, op: AssistantFrameOp) -> String {
    encode_mutation(&SessionMutation::Record {
        record: LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: format!("f{seq}"),
                seq,
                lane: "main".into(),
                timestamp: seq as i64,
            },
            run_id: run.into(),
            stream_index: 0,
            op,
            frame: match op {
                AssistantFrameOp::Append => {
                    Some(serde_json::json!({"text": format!("frame-{seq}")}))
                }
                AssistantFrameOp::CommitStream => {
                    Some(serde_json::json!({"entryId": format!("e{seq}")}))
                }
                _ => None,
            },
        }),
    })
}

fn entry_line(seq: u64, id: &str, text: &str) -> String {
    encode_mutation(&SessionMutation::Entry {
        seq,
        timestamp: seq as i64,
        lane: Some("main".into()),
        entry: Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: id.into(),
                seq,
                parent_id: None,
                timestamp: seq as i64,
            },
            message: AgentMessage::User(UserMessage::new(
                UserContent::Text(text.to_string()),
                seq as i64,
            )),
            terminate: None,
        }),
    })
}

async fn write_session(
    dir: &std::path::Path,
    name: &str,
    body: &str,
) -> (Arc<dyn FileSystem>, String) {
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.to_path_buf()));
    let path = dir.join(name).to_string_lossy().into_owned();
    let header = JsonlV4Header::new("gc-session".into(), 1, "/cwd".into(), None, None, None);
    fs.write_file(
        &path,
        FileContent::Text(format!("{}{body}", encode_header(&header))),
        None,
    )
    .await
    .unwrap();
    (fs, path)
}

/// Every visible record's (type, seq), sorted — the "nothing observable
/// changed" fingerprint.
async fn record_seqs(storage: &JsonlSessionStorage) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = storage
        .find_records(&Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|record| (record.record_type().to_string(), record.base().seq))
        .collect();
    rows.sort();
    rows
}

async fn load(fs: Arc<dyn FileSystem>, path: &str) -> JsonlSessionStorage {
    JsonlSessionStorage::load(
        fs,
        path,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap()
}

/// The whole axis in one fixture:
/// - run `cleared` has appends and a terminal clear → its appends are retired;
/// - run `open` has appends and no clear → salvage needs them, keep all;
/// - an append *after* its own clear stays visible (the #252 P1 rule);
/// - entries are never touched.
#[tokio::test]
async fn review_issue14_gc_removes_only_appends_before_their_own_clear() {
    let temp = tempfile::tempdir().unwrap();
    let mut body = String::new();
    body.push_str(&entry_line(1, "e1", "first question"));
    body.push_str(&frame_line(2, "cleared", AssistantFrameOp::Append));
    body.push_str(&frame_line(3, "cleared", AssistantFrameOp::Append));
    body.push_str(&frame_line(4, "cleared", AssistantFrameOp::CommitStream));
    body.push_str(&frame_line(5, "cleared", AssistantFrameOp::ClearRun));
    body.push_str(&frame_line(6, "cleared", AssistantFrameOp::Append));
    body.push_str(&frame_line(7, "open", AssistantFrameOp::Append));
    body.push_str(&entry_line(8, "e2", "second question"));
    let (fs, path) = write_session(temp.path(), "s.jsonl", &body).await;

    let before = load(fs.clone(), &path).await;
    let before_seqs = record_seqs(&before).await;

    // dry-run must not touch the file.
    let original = fs.read_text_file(&path, None).await.unwrap();
    let dry = compact_retired_frames(fs.clone(), &path, true)
        .await
        .unwrap();
    assert_eq!(dry.retired_frames, 2, "{dry:?}");
    assert!(dry.would_rewrite());
    assert_eq!(fs.read_text_file(&path, None).await.unwrap(), original);

    let report = compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    assert_eq!(report.retired_frames, 2, "{report:?}");
    assert!(report.bytes_after < report.bytes_before);
    assert_eq!(report.lines - report.retired_frames, report.kept_lines);

    let after_text = fs.read_text_file(&path, None).await.unwrap();
    // Retired appends lose their payload but keep their identity: the ID
    // reservation contract must survive compaction (review #274 P2).
    assert!(!after_text.contains("frame-2"), "{after_text}");
    assert!(!after_text.contains("frame-3"), "{after_text}");
    assert!(after_text.contains("\"id\":\"f2\""), "{after_text}");
    assert!(after_text.contains("\"id\":\"f3\""), "{after_text}");
    // Everything the reducer still needs is byte-identical.
    assert!(after_text.contains("\"f6\""), "{after_text}");
    assert!(after_text.contains("\"f7\""), "{after_text}");
    assert!(after_text.contains("first question"), "{after_text}");
    assert!(after_text.contains("clear_run"), "{after_text}");

    let after = load(fs.clone(), &path).await;
    assert_eq!(
        record_seqs(&after).await,
        before_seqs,
        "visible record seqs must not change"
    );
    // Cleared run still salvages nothing; the open run still salvages.
    let facade = || {
        rpi_harness::session::Session::new(Arc::new(after.clone()) as Arc<dyn SessionStorage>, None)
    };
    assert!(
        rpi_harness::frame_progress::salvage_run_frames(&facade(), "cleared")
            .await
            .unwrap()
            .is_empty()
    );
    // The open run's frames must still be on disk for a later crash salvage.
    let open_rows = rpi_harness::session::jsonl::JsonlSessionStorage::load(
        fs.clone(),
        &path,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap();
    let kept_frame_ids: Vec<String> = open_rows
        .find_records(&Default::default())
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.record_type() == "assistant_frame")
        .map(|record| record.id().to_string())
        .collect();
    assert!(
        kept_frame_ids.contains(&"f7".to_string()),
        "uncleared frame must survive: {kept_frame_ids:?}"
    );
    let _ = &facade;

    // A second pass is a no-op: the rewrite is idempotent.
    let again = compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    assert_eq!(again.retired_frames, 0, "{again:?}");
    assert!(!again.would_rewrite());
    assert_eq!(fs.read_text_file(&path, None).await.unwrap(), after_text);
}

/// Appends after a compaction must land on their own line with a fresh seq, and
/// a resume must not see a damaged tail.
#[tokio::test]
async fn review_issue14_gc_keeps_the_file_appendable_and_resumable() {
    let temp = tempfile::tempdir().unwrap();
    let mut body = String::new();
    for seq in 1..=20 {
        body.push_str(&frame_line(seq as u64, "cleared", AssistantFrameOp::Append));
    }
    body.push_str(&frame_line(21, "cleared", AssistantFrameOp::ClearRun));
    body.push_str(&entry_line(22, "e1", "kept"));
    let (fs, path) = write_session(temp.path(), "append.jsonl", &body).await;

    let report = compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    assert_eq!(report.retired_frames, 20, "{report:?}");

    let storage = load(fs.clone(), &path).await;
    let new_seq = storage
        .append_record(rpi_harness::session::types::LaneRecord::AssistantFrame(
            AssistantFrameRecord {
                base: RecordBase {
                    id: "fresh".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                run_id: "later".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: None,
            },
        ))
        .await
        .unwrap();
    assert!(
        new_seq.base().seq > 22,
        "appends must continue past the kept seqs: {}",
        new_seq.base().seq
    );

    // Reopening after the append (and after the gc) still loads cleanly.
    drop(storage);
    let reloaded = load(fs.clone(), &path).await;
    let kinds: Vec<String> = reloaded
        .find_records(&Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|record| format!("{}@{}", record.record_type(), record.base().seq))
        .collect();
    // Only frame records remain (the retired appends are gone), and the new
    // append is the newest one.
    assert!(
        kinds
            .iter()
            .all(|kind| kind.starts_with("assistant_frame@")),
        "{kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|kind| kind.starts_with("assistant_frame@")),
        "{kinds:?}"
    );
    assert!(
        !kinds.iter().any(|kind| kind == "assistant_frame@1"),
        "{kinds:?}"
    );
}

/// A run without any `clear_run` must never lose a frame — that is the crash
/// salvage path.
#[tokio::test]
async fn review_issue14_gc_never_touches_an_uncleared_run() {
    let temp = tempfile::tempdir().unwrap();
    let mut body = String::new();
    for seq in 1..=5 {
        body.push_str(&frame_line(seq as u64, "open", AssistantFrameOp::Append));
    }
    let (fs, path) = write_session(temp.path(), "open.jsonl", &body).await;
    let original = fs.read_text_file(&path, None).await.unwrap();

    let report = compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    assert_eq!(report.retired_frames, 0, "{report:?}");
    assert!(!report.would_rewrite());
    assert_eq!(fs.read_text_file(&path, None).await.unwrap(), original);
}

/// An unparseable line is never deleted: the pass keeps what it cannot read.
#[tokio::test]
async fn review_issue14_gc_keeps_lines_it_cannot_parse() {
    let temp = tempfile::tempdir().unwrap();
    let mut body = String::new();
    body.push_str(&frame_line(1, "cleared", AssistantFrameOp::Append));
    body.push_str(&frame_line(2, "cleared", AssistantFrameOp::ClearRun));
    body.push_str("{not json at all}\n");
    let (fs, path) = write_session(temp.path(), "junk.jsonl", &body).await;

    let report = compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    assert_eq!(report.retired_frames, 1, "{report:?}");
    let text = fs.read_text_file(&path, None).await.unwrap();
    assert!(text.contains("{not json at all}"), "{text}");
    assert!(text.contains("clear_run"), "{text}");
    // The retired append is stripped but its identity row is kept verbatim.
    assert!(!text.contains("frame-1"), "{text}");
    assert!(text.contains("\"id\":\"f1\""), "{text}");
}

// Model a writer acknowledging an append immediately after GC reads its
// snapshot, before GC publishes its replacement. Every other operation is real.
struct ReviewAppendAfterSnapshot {
    inner: Arc<dyn FileSystem>,
    path: String,
    append: String,
    acknowledged: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl FileSystem for ReviewAppendAfterSnapshot {
    fn cwd(&self) -> &std::path::Path {
        self.inner.cwd()
    }
    async fn absolute_path(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<std::path::PathBuf, rpi_tools::FileError> {
        self.inner.absolute_path(p, c).await
    }
    async fn join_path(
        &self,
        p: &[&str],
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<std::path::PathBuf, rpi_tools::FileError> {
        self.inner.join_path(p, c).await
    }
    async fn read_text_file(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<String, rpi_tools::FileError> {
        let snapshot = self.inner.read_text_file(p, c).await?;
        if p == self.path && !self.acknowledged.load(std::sync::atomic::Ordering::SeqCst) {
            self.inner
                .append_file(p, FileContent::Text(self.append.clone()), c)
                .await?;
            self.acknowledged
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(snapshot)
    }
    async fn read_text_lines(
        &self,
        p: &str,
        n: Option<usize>,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<String>, rpi_tools::FileError> {
        self.inner.read_text_lines(p, n, c).await
    }
    async fn read_binary_file(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<u8>, rpi_tools::FileError> {
        self.inner.read_binary_file(p, c).await
    }
    async fn write_file(
        &self,
        p: &str,
        v: FileContent,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), rpi_tools::FileError> {
        self.inner.write_file(p, v, c).await
    }
    async fn append_file(
        &self,
        p: &str,
        v: FileContent,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), rpi_tools::FileError> {
        self.inner.append_file(p, v, c).await
    }
    async fn rename_file(
        &self,
        a: &str,
        b: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), rpi_tools::FileError> {
        self.inner.rename_file(a, b, c).await
    }
    async fn file_info(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<rpi_tools::FileInfo, rpi_tools::FileError> {
        self.inner.file_info(p, c).await
    }
    async fn list_dir(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Vec<rpi_tools::FileInfo>, rpi_tools::FileError> {
        self.inner.list_dir(p, c).await
    }
    async fn canonical_path(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<std::path::PathBuf, rpi_tools::FileError> {
        self.inner.canonical_path(p, c).await
    }
    async fn exists(
        &self,
        p: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<bool, rpi_tools::FileError> {
        self.inner.exists(p, c).await
    }
    async fn create_dir(
        &self,
        p: &str,
        r: bool,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), rpi_tools::FileError> {
        self.inner.create_dir(p, r, c).await
    }
    async fn remove(
        &self,
        p: &str,
        r: bool,
        f: bool,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<(), rpi_tools::FileError> {
        self.inner.remove(p, r, f, c).await
    }
    async fn create_temp_dir(
        &self,
        p: Option<&str>,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<std::path::PathBuf, rpi_tools::FileError> {
        self.inner.create_temp_dir(p, c).await
    }
    async fn create_temp_file(
        &self,
        p: &str,
        s: &str,
        c: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<std::path::PathBuf, rpi_tools::FileError> {
        self.inner.create_temp_file(p, s, c).await
    }
    async fn cleanup(&self) {
        self.inner.cleanup().await
    }
}

#[tokio::test]
async fn review274_gc_must_preserve_an_append_acknowledged_after_its_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}",
        frame_line(1, "done", AssistantFrameOp::Append),
        frame_line(2, "done", AssistantFrameOp::ClearRun),
        entry_line(3, "e3", "existing")
    );
    let (fs, path) = write_session(temp.path(), "race.jsonl", &body).await;
    let interleaved = Arc::new(ReviewAppendAfterSnapshot {
        inner: fs.clone(),
        path: path.clone(),
        append: entry_line(4, "acknowledged-entry", "ACKNOWLEDGED_NEW_MESSAGE"),
        acknowledged: std::sync::atomic::AtomicBool::new(false),
    });
    let result = compact_retired_frames(interleaved.clone(), &path, false).await;
    assert!(interleaved
        .acknowledged
        .load(std::sync::atomic::Ordering::SeqCst));
    let after = fs.read_text_file(&path, None).await.unwrap();
    assert!(after.contains("ACKNOWLEDGED_NEW_MESSAGE"),
        "GC must reject a stale snapshot or preserve the acknowledged append; result={result:?}, after={after}");
}

#[tokio::test]
async fn review274_gc_must_keep_retired_frame_ids_reserved() {
    use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind};
    let temp = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}",
        frame_line(1, "done", AssistantFrameOp::Append),
        frame_line(2, "done", AssistantFrameOp::ClearRun),
        entry_line(3, "e3", "existing")
    );
    let (fs, path) = write_session(temp.path(), "identity.jsonl", &body).await;
    let replacement = ProvisionedEntry {
        id: "f1".into(),
        kind: ProvisionedKind::Custom {
            custom_type: "review-reused-id".into(),
            data: None,
        },
    };
    let before = load(fs.clone(), &path).await;
    assert!(
        before
            .append_entry(replacement.clone(), "main")
            .await
            .is_err(),
        "the pre-GC storage reserves the retired frame ID"
    );
    drop(before);
    compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    let after = load(fs, &path).await;
    let reused = after.append_entry(replacement, "main").await;
    assert!(
        reused.is_err(),
        "compaction must preserve the storage's ID uniqueness contract; got {reused:?}"
    );
}

/// Review #274 P1, generation half: an append handle that was opened before
/// the atomic rewrite must not keep writing into the retired inode. The OS
/// append capability detects the path's new identity and reopens; the CLI also
/// holds the session lock, so a live storage in another process is rejected
/// before this path can occur. This probe pins the in-process contract for a
/// lock-less caller: the acknowledged append lands in the published file.
#[cfg(unix)]
#[tokio::test]
async fn review274_gc_publishes_a_generation_existing_append_handles_follow() {
    use rpi_harness::session::types::{ProvisionedEntry, ProvisionedKind};

    let temp = tempfile::tempdir().unwrap();
    let body = format!(
        "{}{}{}",
        frame_line(1, "done", AssistantFrameOp::Append),
        frame_line(2, "done", AssistantFrameOp::ClearRun),
        entry_line(3, "e3", "existing")
    );
    let (fs, path) = write_session(temp.path(), "generation.jsonl", &body).await;
    let storage = load(fs.clone(), &path).await;
    // Force the reusable append handle to exist before the rewrite. This is
    // the handle that would otherwise be bound to the old generation.
    storage
        .append_entry(
            ProvisionedEntry {
                id: "before-gc".into(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(UserMessage::new(
                        UserContent::Text("before gc".into()),
                        4,
                    )),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();

    compact_retired_frames(fs.clone(), &path, false)
        .await
        .unwrap();
    storage
        .append_entry(
            ProvisionedEntry {
                id: "after-gc".into(),
                kind: ProvisionedKind::Message {
                    message: AgentMessage::User(UserMessage::new(
                        UserContent::Text("after gc".into()),
                        5,
                    )),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    drop(storage);

    let published = fs.read_text_file(&path, None).await.unwrap();
    assert!(
        published.contains("after gc"),
        "an append handle opened before GC must follow the published generation: {published}"
    );
    let reloaded = load(fs, &path).await;
    assert!(
        reloaded.get_entry("after-gc").await.unwrap().is_some(),
        "the acknowledged append must be recoverable from the published file"
    );
}
