//! Fork issue #12 regression: a run with a later `clear_run` contributes no
//! cold payload locations. Its IDs stay reserved; unclosed runs stay live.

use super::*;
use crate::session::jsonl::{encode_header, encode_mutation, types::JsonlV4Header};
use crate::session::memory::{CounterIdGenerator, FakeClock};
use crate::session::types::{AssistantFrameRecord, RecordBase};
use rpi_tools::OsExecutionEnv;
use serde_json::json;

fn frame(seq: u64, run: &str, op: AssistantFrameOp) -> SessionMutation {
    SessionMutation::Record {
        record: LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: format!("f-{seq}"),
                seq,
                timestamp: 1,
                lane: "main".into(),
            },
            run_id: run.into(),
            stream_index: 0,
            op,
            frame: Some(json!({"type":"text_delta","delta":"payload"})),
        }),
    }
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

#[tokio::test]
async fn review_issue12_cleared_run_frames_never_enter_cold_payload_locations() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let header = JsonlV4Header::new("issue12".into(), 1, "/".into(), None, None, None);
    let mut raw = encode_header(&header);
    for seq in 1..=8 {
        raw.push_str(&encode_mutation(&frame(
            seq,
            "cleared",
            AssistantFrameOp::Append,
        )));
    }
    raw.push_str(&encode_mutation(&frame(
        9,
        "cleared",
        AssistantFrameOp::ClearRun,
    )));
    raw.push_str(&encode_mutation(&frame(
        10,
        "crashed",
        AssistantFrameOp::Append,
    )));
    fs.write_file("issue12.jsonl", FileContent::Text(raw.clone()), None)
        .await
        .unwrap();

    let storage = load(fs.clone(), "issue12.jsonl").await;
    let shared = storage.state();
    let state = shared.lock().await;
    let locations = state.cold_locations();
    assert_eq!(
        locations.len(),
        2,
        "only the crashed-run append and the terminal clear marker may be indexed"
    );
    assert_eq!(locations[0], ("cleared".into(), false));
    assert_eq!(locations[1], ("crashed".into(), true));
    assert!(
        !state.active_run("cleared"),
        "a cleared run must not retain its append lookup index"
    );
    assert!(
        state.validate_unused_id("f-1").is_err(),
        "retired frame IDs still occur in the append-only file"
    );
    drop(state);

    let records = storage
        .find_records(&RecordQuery {
            run_id: Some("cleared".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "the clear marker remains visible");
    assert!(matches!(
        &records[0],
        LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::ClearRun
    ));
    let crashed = storage
        .find_records(&RecordQuery {
            run_id: Some("crashed".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(crashed.len(), 1, "unclosed run frames remain salvageable");
    assert!(matches!(
        &crashed[0],
        LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::Append
    ));
    assert_eq!(fs.read_text_file("issue12.jsonl", None).await.unwrap(), raw);
}

fn history() -> String {
    let header = JsonlV4Header::new("issue12-axis".into(), 1, "/".into(), None, None, None);
    let mut raw = encode_header(&header);
    let rows = [
        (1, "done", 0, AssistantFrameOp::Append),
        (2, "done", 1, AssistantFrameOp::Append),
        (3, "open", 0, AssistantFrameOp::Append),
        (4, "mixed", 0, AssistantFrameOp::Append),
        (5, "mixed", 0, AssistantFrameOp::ClearStream),
        (6, "mixed", 1, AssistantFrameOp::Append),
        (7, "done", 0, AssistantFrameOp::ClearRun),
        (8, "mixed", 0, AssistantFrameOp::CommitStream),
    ];
    for (seq, run, stream, op) in rows {
        let mut mutation = frame(seq, run, op);
        if let SessionMutation::Record {
            record: LaneRecord::AssistantFrame(inner),
        } = &mut mutation
        {
            inner.stream_index = stream;
            if op == AssistantFrameOp::CommitStream {
                inner.frame = Some(json!({"entryId": "reserved"}));
            }
        }
        raw.push_str(&encode_mutation(&mutation));
    }
    raw
}

async fn assert_axis_contract(storage: &JsonlSessionStorage, expect_cold: bool) {
    if expect_cold {
        let shared = storage.state();
        let state = shared.lock().await;
        let locations = state.cold_locations();
        // Only the two `done` appends are unreachable. The open run, the live
        // stream of a run with a clear_stream, and both markers stay indexed;
        // CommitStream remains in the hot record index.
        assert_eq!(locations.len(), 5);
        assert!(state.active_run("open"));
        assert!(state.active_run("mixed"));
        assert!(!state.active_run("done"));
        assert!(state.validate_unused_id("f-1").is_err());
        assert!(state.validate_unused_id("f-2").is_err());
        drop(state);
    }

    let mut all = storage.find_records(&RecordQuery::default()).await.unwrap();
    all.sort_unstable_by_key(|record| record.seq());
    let seqs: Vec<_> = all.iter().map(|record| record.seq()).collect();
    // `clear_stream` retires its own stream's append under the existing
    // reducer; the terminal clear retires both `done` appends.
    assert_eq!(seqs, vec![3, 5, 6, 7, 8]);
    let open = storage
        .find_records(&RecordQuery {
            run_id: Some("open".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(open.len(), 1, "a crash-only run stays salvageable");
    assert_eq!(
        storage.get_log(&LogOptions::default()).await.unwrap().len(),
        5,
        "retired appends leave no log row; all live rows remain"
    );
}

#[tokio::test]
async fn review_issue12_axis_matrix_preserves_live_frames_and_eager_fallback() {
    let raw = history();
    for eager in [false, true] {
        if !eager {
            let dir = tempfile::tempdir().unwrap();
            let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
            fs.write_file("axis.jsonl", FileContent::Text(raw.clone()), None)
                .await
                .unwrap();
            let storage = load(fs.clone(), "axis.jsonl").await;
            assert_axis_contract(&storage, !eager).await;
            assert_eq!(
                fs.read_text_file("axis.jsonl", None).await.unwrap(),
                raw,
                "streaming reader must not rewrite an ordinary history"
            );
        } else {
            let fs: Arc<dyn FileSystem> =
                Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
            fs.write_file("/axis.jsonl", FileContent::Text(raw.clone()), None)
                .await
                .unwrap();
            let storage = load(fs.clone(), "/axis.jsonl").await;
            assert_axis_contract(&storage, !eager).await;
            assert_eq!(
                fs.read_text_file("/axis.jsonl", None).await.unwrap(),
                raw,
                "custom eager reader must not rewrite an ordinary history"
            );
        }
    }
}

#[tokio::test]
async fn review_issue12_recovery_boundaries_and_strict_reader_are_unchanged() {
    let raw = history();
    for damage in ["none", "duplicate", "torn", "strict"] {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let mut bytes = raw.clone();
        match damage {
            "duplicate" => bytes.push_str(&encode_mutation(&frame(
                1,
                "done",
                AssistantFrameOp::Append,
            ))),
            "torn" => bytes.push_str("{\"seq\":9,"),
            "strict" => {}
            _ => {}
        }
        fs.write_file("axis.jsonl", FileContent::Text(bytes), None)
            .await
            .unwrap();
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let storage = if damage == "strict" {
            let reader = fs.open_read_file("axis.jsonl").await.unwrap().unwrap();
            let length = reader.byte_len().await.unwrap().unwrap();
            JsonlSessionStorage::load_streaming(
                fs.clone(),
                "axis.jsonl",
                clock,
                ids,
                reader,
                length,
                ReadPolicy::ResourceTransitionStrict,
            )
            .await
            .unwrap()
        } else {
            JsonlSessionStorage::load(fs.clone(), "axis.jsonl", clock, ids)
                .await
                .unwrap()
        };
        let shared = storage.state();
        let state = shared.lock().await;
        let expected_locations = if damage == "strict" { 7 } else { 5 };
        // Strict transition readers never enable speculative retirement and
        // retain every ordinary cold payload location. A duplicate/torn tail
        // does not change the run's terminal clear, so it remains optimized.
        assert_eq!(state.cold_locations().len(), expected_locations);
        drop(state);
        let mut all = storage.find_records(&RecordQuery::default()).await.unwrap();
        all.sort_unstable_by_key(|record| record.seq());
        let seqs: Vec<_> = all.iter().map(|record| record.seq()).collect();
        // The existing reducer still retires cleared append records; strict
        // replay differs only in retaining their physical cold locations.
        assert_eq!(seqs, vec![3, 5, 6, 7, 8]);
    }
}

// Maintainer P1 on head 5a936e7: the pre-scan treated any run with a clear_run
// as fully retired, so an append written *after* its own clear marker was
// silently dropped. The existing loader keeps that later frame visible, so the
// optimization must retire only appends that physically precede their marker.
// Covered for both readers: the streaming path and the eager compatibility path.
fn append_after_clear_history() -> String {
    let header = JsonlV4Header::new("issue12-late".into(), 1, "/".into(), None, None, None);
    let mut raw = encode_header(&header);
    for (seq, op) in [
        (1, AssistantFrameOp::Append),
        (2, AssistantFrameOp::ClearRun),
        (3, AssistantFrameOp::Append),
    ] {
        raw.push_str(&encode_mutation(&frame(seq, "late", op)));
    }
    raw
}

#[tokio::test]
async fn review_issue12_append_after_its_own_clear_stays_visible() {
    let raw = append_after_clear_history();
    for eager in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (fs, path): (Arc<dyn FileSystem>, &str) = if eager {
            (
                Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
                "/late.jsonl",
            )
        } else {
            (
                Arc::new(OsExecutionEnv::with_cwd(dir.path().into())),
                "late.jsonl",
            )
        };
        fs.write_file(path, FileContent::Text(raw.clone()), None)
            .await
            .unwrap();
        let storage = load(fs.clone(), path).await;

        let shared = storage.state();
        let state = shared.lock().await;
        if !eager {
            // Only the streaming reader indexes payload locations; the eager
            // compatibility path keeps real payloads resident (pre-existing
            // split, see `expect_cold = !eager` in the axis probe).
            let locations = state.cold_locations();
            assert_eq!(
                locations.len(),
                2,
                "clear marker and the post-clear append keep payloads"
            );
            assert_eq!(locations[0], ("late".into(), false));
            assert_eq!(locations[1], ("late".into(), true));
        }
        assert!(
            state.validate_unused_id("f-1").is_err(),
            "eager={eager}: a retired frame ID stays reserved"
        );
        drop(state);

        let mut all = storage.find_records(&RecordQuery::default()).await.unwrap();
        all.sort_unstable_by_key(|record| record.seq());
        let seqs: Vec<_> = all.iter().map(|record| record.seq()).collect();
        assert_eq!(
            seqs,
            vec![2, 3],
            "eager={eager}: an append after clear_run must stay visible, as before this PR"
        );
        assert_eq!(
            fs.read_text_file(path, None).await.unwrap(),
            raw,
            "eager={eager}: neither reader may rewrite the history"
        );
    }
}

/// Several markers for one run: the retired set is every append before the
/// largest marker seq, so an append between two markers stays visible.
#[tokio::test]
async fn review_issue12_only_appends_before_the_last_clear_are_retired() {
    let header = JsonlV4Header::new("issue12-multi".into(), 1, "/".into(), None, None, None);
    let mut raw = encode_header(&header);
    for (seq, op) in [
        (1, AssistantFrameOp::Append),
        (2, AssistantFrameOp::ClearRun),
        (3, AssistantFrameOp::Append),
        (4, AssistantFrameOp::ClearRun),
        (5, AssistantFrameOp::Append),
    ] {
        raw.push_str(&encode_mutation(&frame(seq, "multi", op)));
    }
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    fs.write_file("multi.jsonl", FileContent::Text(raw.clone()), None)
        .await
        .unwrap();
    let storage = load(fs.clone(), "multi.jsonl").await;
    let mut all = storage.find_records(&RecordQuery::default()).await.unwrap();
    all.sort_unstable_by_key(|record| record.seq());
    let seqs: Vec<_> = all.iter().map(|record| record.seq()).collect();
    assert_eq!(
        seqs,
        vec![2, 4, 5],
        "appends before a later clear are retired; the trailing append survives"
    );
    let shared = storage.state();
    let state = shared.lock().await;
    assert_eq!(
        state.cold_locations().len(),
        3,
        "two markers plus the trailing live append"
    );
    drop(state);
    assert_eq!(fs.read_text_file("multi.jsonl", None).await.unwrap(), raw);
}
