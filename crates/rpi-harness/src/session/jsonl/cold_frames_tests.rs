use super::*;
use crate::session::jsonl::encode_mutation;
use crate::session::types::{QueueKind, RecordBase};
use async_trait::async_trait;
use rpi_tools::{env::ReadFile, FileError};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct MemoryReader {
    bytes: Mutex<Vec<u8>>,
    reads: AtomicUsize,
    read_bytes: AtomicUsize,
}
#[async_trait]
impl ReadFile for MemoryReader {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        Ok(self.bytes.lock().unwrap().clone())
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(length, Ordering::Relaxed);
        let bytes = self.bytes.lock().unwrap();
        Ok(bytes
            .get(offset as usize..offset as usize + length)
            .unwrap_or_default()
            .to_vec())
    }
}
fn frame(seq: u64, lane: &str, run: &str, op: AssistantFrameOp) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: RecordBase {
            id: format!("frame-{seq}"),
            seq,
            timestamp: 1,
            lane: lane.into(),
        },
        run_id: run.into(),
        stream_index: (seq % 2) as usize,
        op,
        frame: Some(json!({"type":"text_delta","delta":"🌊\r\n".repeat(32)})),
    }
}
fn fixture() -> (ResidentState, SessionState, Arc<MemoryReader>) {
    let mut lazy = ResidentState::default();
    let mut eager = SessionState::new();
    let reader = Arc::new(MemoryReader {
        bytes: Mutex::new(Vec::new()),
        reads: AtomicUsize::new(0),
        read_bytes: AtomicUsize::new(0),
    });
    let handle: Arc<dyn ReadFile> = reader.clone();
    for seq in 1..=100 {
        let mutation = if seq == 1 {
            SessionMutation::Lane {
                seq,
                lane: "side".into(),
                leaf_id: None,
            }
        } else if seq % 9 == 0 {
            SessionMutation::FactName {
                seq,
                name: Some(format!("name-{seq}")),
            }
        } else {
            let lane = if seq % 2 == 0 { "main" } else { "side" };
            let run = if seq % 3 == 0 { "run-b" } else { "run-a" };
            let op = if seq % 11 == 0 {
                AssistantFrameOp::CommitStream
            } else if seq % 7 == 0 {
                AssistantFrameOp::ClearRun
            } else {
                AssistantFrameOp::Append
            };
            let mut frame = frame(seq, lane, run, op);
            if seq % 13 == 0 {
                frame.frame = None;
            }
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            }
        };
        let line = encode_mutation(&mutation);
        let offset = reader.bytes.lock().unwrap().len();
        reader
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(line.as_bytes());
        match &mutation {
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            } if frame.op != AssistantFrameOp::CommitStream => {
                lazy.defer(frame, &handle, offset as u64, &line).unwrap();
            }
            _ => {
                lazy.apply_mutation(mutation.clone()).unwrap();
            }
        }
        eager.apply_mutation(mutation).unwrap();
    }
    (lazy, eager, reader)
}

#[tokio::test]
async fn cold_queries_preserve_eager_filters_order_bounds_and_defensive_copies() {
    let (lazy, eager, reader) = fixture();
    assert!(lazy.cold.locations.len() > 70);
    assert!(
        lazy.reducer
            .find_records(&RecordQuery::default())
            .unwrap()
            .len()
            < 10
    );
    assert_eq!(lazy.sequence(), eager.sequence());
    assert!(lazy.validate_unused_id("frame-2").is_err());
    for order in [
        None,
        Some(EntryOrder::OldestFirst),
        Some(EntryOrder::NewestFirst),
    ] {
        for limit in [None, Some(1), Some(8), Some(1000)] {
            for lane in [None, Some("main".into()), Some("side".into())] {
                for run_id in [None, Some("run-a".into()), Some("missing".into())] {
                    let query = RecordQuery {
                        order,
                        limit,
                        lane: lane.clone(),
                        run_id,
                        after_seq: Some(12),
                        ..Default::default()
                    };
                    assert_eq!(
                        lazy.record_plan(&query).unwrap().read().await.unwrap(),
                        eager.find_records(&query).unwrap()
                    );
                }
            }
        }
    }
    for query in [
        RecordQuery {
            record_type: Some("usage"),
            ..Default::default()
        },
        RecordQuery {
            operation_kind: Some("run"),
            ..Default::default()
        },
        RecordQuery {
            queue_kind: Some(QueueKind::Context),
            ..Default::default()
        },
        RecordQuery {
            tool_call_id: Some("tool".into()),
            ..Default::default()
        },
        RecordQuery {
            target_entry_id: Some("target".into()),
            ..Default::default()
        },
        RecordQuery {
            pending_queue_only: true,
            ..Default::default()
        },
    ] {
        let before = reader.reads.load(Ordering::Relaxed);
        assert_eq!(
            lazy.record_plan(&query).unwrap().read().await.unwrap(),
            eager.find_records(&query).unwrap()
        );
        assert_eq!(reader.reads.load(Ordering::Relaxed), before);
    }
    let query = RecordQuery {
        limit: Some(1),
        ..Default::default()
    };
    let mut first = lazy.record_plan(&query).unwrap().read().await.unwrap();
    if let LaneRecord::AssistantFrame(frame) = &mut first[0] {
        frame.frame = Some(json!("mutated"));
    }
    assert_eq!(
        lazy.record_plan(&query).unwrap().read().await.unwrap(),
        eager.find_records(&query).unwrap()
    );
    assert!(lazy
        .record_plan(&RecordQuery {
            limit: Some(0),
            ..Default::default()
        })
        .is_err());
}

#[tokio::test]
async fn cold_record_pages_and_interleaved_log_match_eager_state() {
    let (lazy, eager, _) = fixture();
    for lane in ["main", "side"] {
        for from_end in [false, true] {
            for limit in [1, 7, 1000] {
                let query = RecordPageQuery {
                    lane: lane.into(),
                    after_seq: Some(10),
                    before_seq: Some(85),
                    limit,
                    from_end,
                };
                let (plan, total) = lazy.page_plan(&query).unwrap();
                let actual = plan.page(total).await.unwrap();
                let expected = eager.record_page(&query).unwrap();
                assert_eq!(actual.total, expected.total);
                assert_eq!(actual.records, expected.records);
            }
        }
    }
    for after_seq in [None, Some(9), Some(99), Some(1000)] {
        for limit in [None, Some(1), Some(13), Some(1000)] {
            let options = LogOptions { after_seq, limit };
            assert_eq!(
                lazy.log_plan(&options).unwrap().read().await.unwrap(),
                eager.get_log(&options).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn cold_bounded_reads_coalesce_and_do_not_warm_or_copy_the_index() {
    let (mut lazy, _, reader) = fixture();
    let count = lazy.cold.locations.len();
    let query = RecordQuery {
        record_type: Some("assistant_frame"),
        limit: Some(5),
        ..Default::default()
    };
    let plan = lazy.record_plan(&query).unwrap();
    // Reconcile can replace the entire resident view while a query owns the
    // prior source generation. Dropping the view does not invalidate the plan.
    lazy = ResidentState::default();
    let records = plan.read().await.unwrap();
    assert_eq!(records.len(), 5);
    assert!(reader.reads.load(Ordering::Relaxed) <= 2);
    assert!(reader.read_bytes.load(Ordering::Relaxed) < reader.bytes.lock().unwrap().len() / 4);
    assert!(count > 70);
    assert!(lazy.cold.locations.is_empty());
}

#[tokio::test]
async fn cold_source_modification_or_short_range_fails_closed() {
    let (lazy, _, reader) = fixture();
    let query = RecordQuery {
        order: Some(EntryOrder::OldestFirst),
        limit: Some(1),
        ..Default::default()
    };
    let offset = lazy.cold.locations[0].offset as usize;
    reader.bytes.lock().unwrap()[offset] = b' ';
    assert!(lazy
        .record_plan(&query)
        .unwrap()
        .read()
        .await
        .unwrap_err()
        .message
        .contains("source changed"));
    reader.bytes.lock().unwrap().clear();
    assert!(lazy
        .record_plan(&query)
        .unwrap()
        .read()
        .await
        .unwrap_err()
        .message
        .contains("short cold"));
}
