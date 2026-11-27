//! Recovery access-boundary regressions using the real streaming JSONL loader.
use super::*;
use crate::session::{
    jsonl::{encode_header, encode_mutation, JsonlSessionStorage, JsonlV4Header},
    memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage},
    state::SessionState,
    types::*,
};
use async_trait::async_trait;
use rpi_ai::types::{ToolResultMessage, ToolResultRole};
use rpi_tools::{
    env::{FileInfo, ReadFile},
    FileContent, FileError, FileErrorCode, FileSystem,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// Counts actual source ranges, not query calls. Faults are enabled only after
/// initial replay, so an unselected damaged payload cannot hide an eager scan.
struct ObservedReader {
    bytes: Vec<u8>,
    ranges: Mutex<Vec<(u64, usize)>>,
    faults: Mutex<Vec<(u64, usize, ReadFault)>>,
}
#[derive(Clone, Copy)]
enum ReadFault {
    Io,
    Corrupt,
    Short,
}
#[async_trait]
impl ReadFile for ObservedReader {
    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        Ok(Some(self.bytes.len() as u64))
    }
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        panic!("cold recovery must use ranges")
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.ranges.lock().unwrap().push((offset, length));
        let mut bytes = self.bytes[offset as usize..offset as usize + length].to_vec();
        for &(start, len, fault) in self.faults.lock().unwrap().iter() {
            let from = start.max(offset);
            let to = (start + len as u64).min(offset + bytes.len() as u64);
            if from >= to {
                continue;
            }
            match fault {
                ReadFault::Io => {
                    return Err(FileError::new(
                        FileErrorCode::NotSupported,
                        "injected selected payload failure",
                    ))
                }
                ReadFault::Corrupt => {
                    bytes[(from - offset) as usize..(to - offset) as usize].fill(b'!')
                }
                ReadFault::Short => bytes.truncate((from - offset) as usize),
            }
        }
        Ok(bytes)
    }
}
struct ObservedFs {
    inner: Arc<dyn FileSystem>,
    reader: Arc<ObservedReader>,
}
#[async_trait]
impl FileSystem for ObservedFs {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    async fn absolute_path(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(p, c).await
    }
    async fn join_path(
        &self,
        p: &[&str],
        c: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.join_path(p, c).await
    }
    async fn read_text_file(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        self.inner.read_text_file(p, c).await
    }
    async fn read_text_lines(
        &self,
        p: &str,
        n: Option<usize>,
        c: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        self.inner.read_text_lines(p, n, c).await
    }
    async fn read_binary_file(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(p, c).await
    }
    async fn write_file(
        &self,
        p: &str,
        v: FileContent,
        c: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.write_file(p, v, c).await
    }
    async fn append_file(
        &self,
        p: &str,
        v: FileContent,
        c: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.append_file(p, v, c).await
    }
    async fn open_read_file(&self, p: &str) -> Result<Option<Arc<dyn ReadFile>>, FileError> {
        if p == "recovery.jsonl" {
            Ok(Some(self.reader.clone()))
        } else {
            self.inner.open_read_file(p).await
        }
    }
    async fn rename_file(
        &self,
        a: &str,
        b: &str,
        c: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.rename_file(a, b, c).await
    }
    async fn file_info(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        self.inner.file_info(p, c).await
    }
    async fn list_dir(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(p, c).await
    }
    async fn canonical_path(
        &self,
        p: &str,
        c: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.canonical_path(p, c).await
    }
    async fn exists(&self, p: &str, c: Option<&CancellationToken>) -> Result<bool, FileError> {
        self.inner.exists(p, c).await
    }
    async fn create_dir(
        &self,
        p: &str,
        r: bool,
        c: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.create_dir(p, r, c).await
    }
    async fn remove(
        &self,
        p: &str,
        r: bool,
        f: bool,
        c: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.remove(p, r, f, c).await
    }
    async fn create_temp_dir(
        &self,
        p: Option<&str>,
        c: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(p, c).await
    }
    async fn create_temp_file(
        &self,
        p: &str,
        s: &str,
        c: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_file(p, s, c).await
    }
    async fn cleanup(&self) {
        self.inner.cleanup().await
    }
}

#[derive(Default)]
struct Log {
    mutations: Vec<SessionMutation>,
    leaves: HashMap<String, String>,
}
impl Log {
    fn seq(&self) -> u64 {
        self.mutations.len() as u64 + 1
    }
    fn lane(&mut self, lane: &str, leaf: Option<&str>) {
        self.mutations.push(SessionMutation::Lane {
            seq: self.seq(),
            lane: lane.into(),
            leaf_id: leaf.map(str::to_owned),
        });
        match leaf {
            Some(id) => {
                self.leaves.insert(lane.into(), id.into());
            }
            None => {
                self.leaves.remove(lane);
            }
        }
    }
    fn entry(&mut self, id: &str, lane: &str, kind: ProvisionedKind) {
        let seq = self.seq();
        let entry = provisioned_into_entry(
            ProvisionedEntry {
                id: id.into(),
                kind,
            },
            seq,
            self.leaves.get(lane).cloned(),
            seq as i64,
        );
        self.mutations.push(SessionMutation::Entry {
            seq,
            timestamp: seq as i64,
            lane: Some(lane.into()),
            entry,
        });
        self.leaves.insert(lane.into(), id.into());
    }
    fn record(&mut self, mut record: LaneRecord) {
        // Keep all fixture stamping explicit via the wire representation.
        let mut value = serde_json::to_value(&record).unwrap();
        value["seq"] = self.seq().into();
        record = serde_json::from_value(value).unwrap();
        self.mutations.push(SessionMutation::Record { record });
    }
    fn open(&mut self, id: &str, lane: &str) {
        self.record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id, lane),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }));
    }
    fn write(&mut self, id: &str, run: &str, target: serde_json::Value) {
        self.record(LaneRecord::WriteDeferred(WriteDeferredRecord {
            base: base(id, "main"),
            run_id: run.into(),
            target,
        }));
    }
    fn tool(&mut self, id: &str, reserved: &str) {
        self.record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base(id, "main"),
            run_id: "closed".into(),
            assistant_entry_id: "assistant".into(),
            tool_index: 0,
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            result_entry_id: reserved.into(),
            effective_args: serde_json::json!({"path":"a.txt"}),
            replay: ToolReplay::Never,
        }));
    }
}
fn base(id: &str, lane: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: lane.into(),
        timestamp: 0,
    }
}
fn user(text: &str) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: rpi_ai::UserMessage::new(text, 0).into(),
        terminate: None,
    }
}
fn result(call: &str, name: &str) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: call.into(),
            tool_name: name.into(),
            content: vec![],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        })),
        terminate: None,
    }
}
struct Fixture {
    cold: SessionRuntime,
    memory: SessionRuntime,
    reader: Arc<ObservedReader>,
    spans: HashMap<String, (u64, usize)>,
    _temp: tempfile::TempDir,
}
async fn fixture(log: Log) -> Fixture {
    let mut state = SessionState::new();
    let header = JsonlV4Header::new("bounded-recovery".into(), 1, "/".into(), None, None, None);
    let mut bytes = encode_header(&header);
    let mut spans = HashMap::new();
    for mutation in log.mutations {
        let line = encode_mutation(&mutation);
        if let SessionMutation::Entry { entry, .. } = &mutation {
            // Cold payload ranges exclude the physical newline.
            spans.insert(entry.id().to_owned(), (bytes.len() as u64, line.len() - 1));
        }
        bytes.push_str(&line);
        state.apply_mutation(mutation).unwrap();
    }
    let reader = Arc::new(ObservedReader {
        bytes: bytes.as_bytes().to_vec(),
        ranges: Mutex::new(vec![]),
        faults: Mutex::new(vec![]),
    });
    let temp = tempfile::tempdir().unwrap();
    let inner: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    inner
        .write_file("recovery.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(ObservedFs {
        inner,
        reader: reader.clone(),
    });
    let cold = JsonlSessionStorage::load(
        fs,
        "recovery.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap();
    assert!(
        !reader.ranges.lock().unwrap().is_empty(),
        "streaming replay must exercise our reader"
    );
    reader.ranges.lock().unwrap().clear();
    let memory = InMemorySessionStorage::from_state(
        SessionMetadata {
            id: header.id,
            created_at: 1,
            parent_session_id: None,
        },
        state,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    );
    Fixture {
        cold: SessionRuntime::new(Session::new(Arc::new(cold), None)),
        memory: SessionRuntime::new(Session::new(Arc::new(memory), None)),
        reader,
        spans,
        _temp: temp,
    }
}
fn basic_log() -> Log {
    let mut log = Log::default();
    log.entry(
        "unrelated-huge",
        "main",
        user(&"🌊 unrelated\r\n".repeat(200_000)),
    );
    log.lane("orphan", None);
    log.open("orphan-run", "orphan");
    log.open("run-a", "main");
    log.open("run-b", "main");
    log
}
#[tokio::test]
async fn no_recovery_records_read_zero_payloads_but_report_orphans_and_multiple_open() {
    let f = fixture(basic_log()).await;
    let (start, len) = f.spans["unrelated-huge"];
    f.reader
        .faults
        .lock()
        .unwrap()
        .push((start, len, ReadFault::Corrupt));
    for lane in ["main", "orphan"] {
        let report = f.cold.recover_lane(lane).await.unwrap();
        assert_eq!(report, f.memory.recover_lane(lane).await.unwrap());
        if lane == "main" {
            assert!(report.is_corrupt());
        } else {
            assert!(matches!(
                report.findings.as_slice(),
                [RecoveryFinding::OrphanedOperation { .. }]
            ));
        }
    }
    assert!(f.reader.ranges.lock().unwrap().is_empty());
}
fn selected_log() -> Log {
    let mut log = basic_log();
    // The assistant is deliberately unrelated to settlement lookup.
    log.entry("assistant", "main", user("placeholder"));
    log.lane("side", None);
    log.entry("cross-write", "side", user("landed write"));
    log.entry("in-window-unselected", "side", result("call", "read"));
    log.entry("cross-tool", "side", result("call", "read"));
    log.entry("abandoned-tool", "main", result("call", "read"));
    log.entry(
        "abandoned-write",
        "main",
        user("landed on abandoned branch"),
    );
    log.lane("main", Some("assistant"));
    log.entry("wrong-call", "side", result("other", "read"));
    log.entry("wrong-name", "side", result("call", "other"));
    log.entry("not-tool", "side", user("not a result"));
    log.entry("unreserved", "side", result("call", "read"));
    for (id, target) in [
        ("write-cross", serde_json::json!({"id":"cross-write"})),
        ("write-branch", serde_json::json!({"id":"abandoned-write"})),
        ("write-missing", serde_json::json!({"id":"missing-write"})),
        ("write-no-id", serde_json::json!({"type":"custom"})),
        ("write-number-id", serde_json::json!({"id":42})),
    ] {
        log.write(id, "closed", target);
    }
    for (id, reserved) in [
        ("tool-cross", "cross-tool"),
        ("tool-branch", "abandoned-tool"),
        ("tool-call", "wrong-call"),
        ("tool-name", "wrong-name"),
        ("tool-type", "not-tool"),
        ("tool-missing", "missing-tool"),
    ] {
        log.tool(id, reserved);
    }
    log
}
#[tokio::test]
async fn cold_full_report_matches_memory_and_only_validates_selected_payloads() {
    let f = fixture(selected_log()).await;
    for id in ["unrelated-huge", "in-window-unselected"] {
        let (start, len) = f.spans[id];
        f.reader
            .faults
            .lock()
            .unwrap()
            .push((start, len, ReadFault::Corrupt));
    }
    let expected = f.memory.recover_lane("main").await.unwrap();
    assert_eq!(expected.pending_writes.len(), 3);
    assert_eq!(
        expected
            .pending_writes
            .iter()
            .map(|w| w.provisioned_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("missing-write"), None, None]
    );
    assert_eq!(
        expected
            .tool_frames
            .iter()
            .map(|t| t.result_entry_id.as_str())
            .collect::<Vec<_>>(),
        vec!["wrong-call", "wrong-name", "not-tool", "missing-tool"]
    );
    assert_eq!(expected.findings.len(), 8); // Seven dangling frames + multiple-open.
    let (start, _) = f.spans["cross-write"];
    let (last, len) = f.spans["not-tool"];
    let window = (start, (last - start) as usize + len);
    // All selected rows fit in one window, even with missing lookups and
    // record order differing from physical row order. Every sweep rereads it.
    for _ in 0..2 {
        f.reader.ranges.lock().unwrap().clear();
        assert_eq!(f.cold.recover_lane("main").await.unwrap(), expected);
        assert_eq!(*f.reader.ranges.lock().unwrap(), [window]);
    }

    // Changing a selected row after successful sweeps must be observable.
    let (start, len) = f.spans["cross-tool"];
    f.reader
        .faults
        .lock()
        .unwrap()
        .push((start, len, ReadFault::Corrupt));
    let error = f.cold.recover_lane("main").await.unwrap_err();
    assert_eq!(error.code, crate::error::SessionErrorCode::InvalidEntry);
    assert!(error.message.contains("cold history source changed"));
}
#[tokio::test]
async fn selected_write_and_tool_payload_errors_propagate() {
    use crate::error::SessionErrorCode;

    for selected in ["cross-write", "cross-tool"] {
        for (fault, code, message, retries) in [
            (
                ReadFault::Io,
                SessionErrorCode::Storage,
                "injected selected payload failure",
                true,
            ),
            (
                ReadFault::Short,
                SessionErrorCode::Storage,
                "short cold history read",
                true,
            ),
            (
                ReadFault::Corrupt,
                SessionErrorCode::InvalidEntry,
                "cold history source changed",
                false,
            ),
        ] {
            let f = fixture(selected_log()).await;
            let (start, len) = f.spans[selected];
            f.reader.faults.lock().unwrap().push((start, len, fault));
            let error = f.cold.recover_lane("main").await.unwrap_err();
            assert_eq!(error.code, code);
            assert!(error.message.contains(message), "{error}");
            let ranges = f.reader.ranges.lock().unwrap();
            assert_eq!(ranges.contains(&(start, len)), retries);
        }
    }
}

#[tokio::test]
async fn coalesced_io_or_short_read_in_gap_falls_back_to_selected_rows() {
    let f = fixture(selected_log()).await;
    let expected = f.memory.recover_lane("main").await.unwrap();
    let (start, _) = f.spans["cross-write"];
    let (last, len) = f.spans["not-tool"];
    let (gap, gap_len) = f.spans["in-window-unselected"];
    for fault in [ReadFault::Io, ReadFault::Short] {
        *f.reader.faults.lock().unwrap() = vec![(gap, gap_len, fault)];
        f.reader.ranges.lock().unwrap().clear();
        assert_eq!(f.cold.recover_lane("main").await.unwrap(), expected);
        assert_eq!(
            *f.reader.ranges.lock().unwrap(),
            [
                (start, (last - start) as usize + len),
                f.spans["cross-write"],
                f.spans["abandoned-write"],
                f.spans["cross-tool"],
                f.spans["abandoned-tool"],
                f.spans["wrong-call"],
                f.spans["wrong-name"],
                f.spans["not-tool"],
            ],
        );
    }
}

#[tokio::test]
async fn coalesced_failure_preserves_first_error_in_record_order() {
    let f = fixture(selected_log()).await;
    let (later_offset, later_len) = f.spans["cross-tool"];
    let (earlier_offset, earlier_len) = f.spans["abandoned-write"];
    assert!(later_offset < earlier_offset);
    *f.reader.faults.lock().unwrap() = vec![
        (later_offset, later_len, ReadFault::Io),
        (earlier_offset, earlier_len, ReadFault::Corrupt),
    ];
    let error = f.cold.recover_lane("main").await.unwrap_err();
    assert_eq!(error.code, crate::error::SessionErrorCode::InvalidEntry);
    assert!(error.message.contains("cold history source changed"));
    let ranges = f.reader.ranges.lock().unwrap();
    assert_eq!(
        &ranges[1..],
        &[f.spans["cross-write"], f.spans["abandoned-write"]]
    );
}

#[tokio::test]
async fn recovery_reads_respect_window_span_gap_and_large_row_chunk_limits() {
    const CHUNK: usize = 1024 * 1024;
    let mut log = Log::default();
    for (id, size) in [
        ("first", 100 * 1024),
        ("second", 100 * 1024),
        ("third", 100 * 1024),
        ("unselected", 12 * 1024),
        ("fourth", 100 * 1024),
        ("fifth", 100 * 1024),
        ("large", CHUNK + 1024),
    ] {
        log.entry(id, "main", user(&"x".repeat(size)));
    }
    for id in ["first", "second", "third", "fourth", "fifth", "large"] {
        log.write(
            &format!("write-{id}"),
            "closed",
            serde_json::json!({"id": id}),
        );
    }
    let f = fixture(log).await;
    let (start, len) = f.spans["unselected"];
    f.reader
        .faults
        .lock()
        .unwrap()
        .push((start, len, ReadFault::Io));
    assert_eq!(
        f.cold.recover_lane("main").await.unwrap(),
        f.memory.recover_lane("main").await.unwrap()
    );
    let window = |start: &str, end: &str| {
        let (start, _) = f.spans[start];
        let (end, len) = f.spans[end];
        (start, (end - start) as usize + len)
    };
    let (large_start, large_len) = f.spans["large"];
    assert_eq!(
        *f.reader.ranges.lock().unwrap(),
        [
            window("first", "second"),
            f.spans["third"],
            window("fourth", "fifth"),
            (large_start, CHUNK),
            (large_start + CHUNK as u64, large_len - CHUNK),
        ],
    );
}
