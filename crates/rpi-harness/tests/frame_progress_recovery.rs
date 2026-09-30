//! Frame-progress recovery across a real disk round trip.
//!
//! The in-memory recovery tests (`harness_run_e2e`) prove the logic; this file
//! proves the **storage** half: frames written by a crashed run must still be
//! found after the session is reloaded from JSONL. That is the path a real crash
//! takes, and it is where a missing codec whitelist entry would surface as
//! "recovery silently finds nothing".
//!
//! See `docs/llm-repetition-forensics.md` §11.7.1.

use std::sync::Arc;

use rpi_ai::frames::AssistantMessageFrame;
use rpi_ai::types::{
    Api, AssistantMessage, AssistantRole, Content, StopReason, TextContent, TextContentType, Usage,
};
use rpi_harness::frame_progress::{
    salvage_run_frames, ASSISTANT_FRAME_RECORD_TYPE, INTERRUPTED_NOTICE,
};
use rpi_harness::session::jsonl::storage::JsonlSessionStorage;
use rpi_harness::session::jsonl::types::JsonlV4Header;
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::SessionStorage;
use rpi_harness::session::types::{
    AssistantFrameOp, AssistantFrameRecord, EntryOrder, EntryQuery, LaneRecord, OperationIntent,
    OperationStartedRecord, RecordBase, RecordQuery,
};
use rpi_harness::session::Session;
use rpi_tools::FileSystem;

const LANE: &str = "main";
const RUN_ID: &str = "run-crashed";
const PATH: &str = "/crashed.jsonl";

fn header() -> JsonlV4Header {
    JsonlV4Header::new(
        "crashed".into(),
        1_700_000_000_000,
        "/proj".into(),
        None,
        None,
        None,
    )
}

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock::new())
}

fn ids() -> Arc<CounterIdGenerator> {
    Arc::new(CounterIdGenerator::new())
}

fn record_base(id: &str) -> RecordBase {
    RecordBase {
        id: id.to_string(),
        seq: 0,
        lane: LANE.to_string(),
        timestamp: 0,
    }
}

/// The stream a real run would have written before dying: a `start`, a text
/// block, the text committed so far, then nothing.
///
/// Built from the typed frame enum rather than hand-written JSON so the fixture
/// cannot drift from the wire contract `reduce_frames` expects.
fn crashed_stream() -> Vec<LaneRecord> {
    const TEXT: &str = "recovered from disk";
    let partial = AssistantMessage {
        role: AssistantRole,
        // The identity part only: `start` frames carry empty content.
        content: Vec::new(),
        api: Api::Faux,
        provider: "faux".into(),
        model: "faux-model".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 1_700_000_000_000,
    };
    let frames = vec![
        AssistantMessageFrame::Start { partial },
        AssistantMessageFrame::TextStart {
            content_index: 0,
            content: TextContent {
                kind: TextContentType,
                text: String::new(),
                text_signature: None,
            },
        },
        AssistantMessageFrame::TextDelta {
            content_index: 0,
            delta: TEXT.into(),
        },
    ];
    let mut records = vec![
        // The run opened and never closed: the crash left it dangling.
        LaneRecord::OperationStarted(OperationStartedRecord {
            base: record_base("op-1"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        }),
    ];
    for (index, frame) in frames.into_iter().enumerate() {
        records.push(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: record_base(&format!("frame-{index}")),
            run_id: RUN_ID.into(),
            stream_index: 0,
            op: AssistantFrameOp::Append,
            frame: Some(serde_json::to_value(&frame).expect("frame serializes")),
        }));
    }
    records
}

/// Write the crashed run's records, then reload the session from disk exactly as
/// a restart would.
async fn crashed_session_on_disk() -> Session {
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    let storage = JsonlSessionStorage::create(fs.clone(), PATH, header(), clock(), ids())
        .await
        .expect("create jsonl session");
    for record in crashed_stream() {
        storage.append_record(record).await.expect("append record");
    }

    // Reload from the same file: nothing in memory is carried over.
    let reloaded = JsonlSessionStorage::load(fs, PATH, clock(), ids())
        .await
        .expect("reload jsonl session");
    Session::new(Arc::new(reloaded), None)
}

/// The frames a crashed run committed must survive the JSONL round trip, or
/// recovery has nothing to work with.
#[tokio::test]
async fn frames_survive_a_reload_from_disk() {
    let session = crashed_session_on_disk().await;

    let records = session
        .find_records(&RecordQuery {
            record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
            run_id: Some(RUN_ID.to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("frame records readable after reload");
    assert_eq!(
        records.len(),
        3,
        "every committed frame must be reloaded, got {records:?}"
    );
}

/// End to end on disk: the reloaded session yields the interrupted message, and
/// the frames never became branch history.
#[tokio::test]
async fn reloaded_frames_reduce_to_an_interrupted_message() {
    let session = crashed_session_on_disk().await;

    let salvaged = salvage_run_frames(&session, RUN_ID)
        .await
        .expect("salvage from disk");
    assert_eq!(salvaged.len(), 1, "one stream, one salvaged message");

    let message = &salvaged[0];
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some(INTERRUPTED_NOTICE));
    assert_eq!(
        message.usage,
        Usage::zero(),
        "a salvaged partial must not bill usage"
    );
    assert!(
        matches!(
            message.content.first(),
            Some(Content::Text(text)) if text.text == "recovered from disk"
        ),
        "the committed text must survive the disk round trip, got {:?}",
        message.content
    );

    // Frames are progress, not history: the reloaded branch holds no assistant
    // entry, so a salvaged message can never double up with a persisted one.
    let entries = session
        .view(LANE)
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("branch readable");
    assert!(
        entries
            .iter()
            .all(|entry| !matches!(entry, rpi_harness::session::types::Entry::Message(_))),
        "frame records must not appear in the branch, got {entries:?}"
    );
}

/// A run that committed (wrote `ClearRun`) must salvage nothing, even though its
/// frames are still on disk.
#[tokio::test]
async fn a_cleared_run_salvages_nothing_from_disk() {
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    let storage = JsonlSessionStorage::create(fs.clone(), PATH, header(), clock(), ids())
        .await
        .expect("create jsonl session");
    for record in crashed_stream() {
        storage.append_record(record).await.expect("append record");
    }
    // The run then finished and its messages were persisted, retiring the frames.
    storage
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: record_base("frame-clear"),
            run_id: RUN_ID.into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        }))
        .await
        .expect("append clear");

    let reloaded = JsonlSessionStorage::load(fs, PATH, clock(), ids())
        .await
        .expect("reload jsonl session");
    let session = Session::new(Arc::new(reloaded), None);

    // The frames are still readable...
    let records = session
        .find_records(&RecordQuery {
            record_type: Some(ASSISTANT_FRAME_RECORD_TYPE),
            run_id: Some(RUN_ID.to_string()),
            ..Default::default()
        })
        .await
        .expect("records readable");
    assert_eq!(
        records.len(),
        4,
        "frames + the clear marker are all on disk"
    );

    // ...but nothing is salvaged, so a committed run is never duplicated.
    let salvaged = salvage_run_frames(&session, RUN_ID)
        .await
        .expect("salvage runs");
    assert!(
        salvaged.is_empty(),
        "a cleared run must salvage nothing, got {salvaged:?}"
    );
}

/// An unreadable frame must be skipped, not turned into a failed recovery: a
/// session must never become unrecoverable because of one bad record.
#[tokio::test]
async fn an_undecodable_frame_is_skipped_not_fatal() {
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    let storage = JsonlSessionStorage::create(fs.clone(), PATH, header(), clock(), ids())
        .await
        .expect("create jsonl session");
    // Stream 0 is a valid, reducible stream.
    for record in crashed_stream()
        .into_iter()
        .filter(|record| !matches!(record, LaneRecord::OperationStarted(_)))
    {
        storage
            .append_record(record)
            .await
            .expect("append good frame");
    }
    // Stream 1 is garbage.
    storage
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: record_base("frame-bad"),
            run_id: RUN_ID.into(),
            stream_index: 1,
            op: AssistantFrameOp::Append,
            frame: Some(serde_json::json!({"type": "not_a_real_frame_variant"})),
        }))
        .await
        .expect("append bad frame");

    let reloaded = JsonlSessionStorage::load(fs, PATH, clock(), ids())
        .await
        .expect("reload jsonl session");
    let session = Session::new(Arc::new(reloaded), None);

    let salvaged = salvage_run_frames(&session, RUN_ID)
        .await
        .expect("an undecodable frame must not fail recovery");
    assert_eq!(
        salvaged.len(),
        1,
        "the readable stream is still salvaged, got {salvaged:?}"
    );
    assert!(
        matches!(
            salvaged[0].content.first(),
            Some(Content::Text(text)) if text.text == "recovered from disk"
        ),
        "the good stream must survive intact, got {:?}",
        salvaged[0].content
    );
}
