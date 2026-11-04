//! Offline reproduction for fork issue #12: a 50 MB-class, 170k-line legacy
//! JSONL session whose streaming frames are all retired by `clear_run`.
//!
//! This is a measurement probe, not a timing assertion: run with
//! `cargo test -p rpi-harness --test issue12_load_probe -- --nocapture`.

use std::sync::Arc;
use std::time::Instant;

use rpi_harness::session::{
    jsonl::{encode_header, types::JsonlV4Header, JsonlSessionStorage},
    memory::{CounterIdGenerator, FakeClock},
    types::SessionStorage,
};
use rpi_tools::{FileContent, FileSystem, OsExecutionEnv};

const FRAMES: usize = 170_000;
const PAYLOAD: usize = 270;

fn frame_row(seq: usize, op: &str, body: &str) -> String {
    format!(
        "{{\"kind\":\"record\",\"type\":\"assistant_frame\",\"id\":\"f{seq}\",\"seq\":{seq},\"lane\":\"main\",\"timestamp\":{seq},\"runId\":\"run\",\"streamIndex\":0,\"op\":\"{op}\",\"frame\":{{\"text\":\"{body}\"}}}}\n"
    )
}

#[tokio::test]
async fn issue12_load_probe_reports_retired_frame_cost() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(temp.path().to_path_buf()));
    let path = "issue12.jsonl";
    let header = JsonlV4Header::new("issue12".into(), 1, "/cwd".into(), None, None, None);
    let mut bytes = encode_header(&header);
    let body = "x".repeat(PAYLOAD);
    for seq in 1..=FRAMES {
        bytes.push_str(&frame_row(seq, "append", &body));
    }
    let clear = FRAMES + 1;
    bytes.push_str(&frame_row(clear, "clear_run", ""));
    fs.write_file(path, FileContent::Text(bytes.clone()), None)
        .await
        .unwrap();

    let mut best_ms = u128::MAX;
    let mut storage = None;
    for _ in 0..3 {
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(CounterIdGenerator::new());
        let start = Instant::now();
        let loaded = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
            .await
            .unwrap();
        best_ms = best_ms.min(start.elapsed().as_millis());
        storage = Some(loaded);
    }
    let storage = storage.unwrap();
    let records = storage
        .find_records(&Default::default())
        .await
        .unwrap()
        .len();
    let log = storage.get_log(&Default::default()).await.unwrap().len();
    eprintln!(
        "issue12 load_probe bytes={} lines={} best_of_3_ms={} visible_records={} visible_log={}",
        bytes.len(),
        FRAMES + 2,
        best_ms,
        records,
        log
    );
    assert_eq!(records, 1, "only the ClearRun marker is visible");
    assert_eq!(log, 1, "retired frame rows must not appear in the log");
}
