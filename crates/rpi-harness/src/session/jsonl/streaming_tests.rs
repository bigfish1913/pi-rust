use super::*;
use crate::session::{
    jsonl::cold_frames::JsonlPayload,
    memory::{CounterIdGenerator, FakeClock},
    state::SessionState,
    types::{EntryOrder, ForkPosition, MessageSummaryQuery, MessageWindowQuery, ProvisionedKind},
};
use std::sync::atomic::AtomicUsize;

struct StrictReader {
    inner: Arc<dyn ReadFile>,
    calls: AtomicUsize,
    ranges: std::sync::Mutex<Vec<(u64, usize)>>,
}
#[async_trait]
impl ReadFile for StrictReader {
    async fn read_all(&self) -> Result<Vec<u8>, rpi_tools::FileError> {
        panic!("stable JSONL history reader must never use read_all")
    }
    async fn byte_len(&self) -> Result<Option<u64>, rpi_tools::FileError> {
        self.inner.byte_len().await
    }
    async fn read_range(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, rpi_tools::FileError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.ranges.lock().unwrap().push((offset, length));
        self.inner.read_range(offset, length).await
    }
}
fn header() -> JsonlV4Header {
    JsonlV4Header::new("stream-test".into(), 1, "/cwd".into(), None, None, None)
}
fn message(id: &str, seq: u64, parent: Option<&str>, text: &str) -> SessionMutation {
    let entry = provisioned_into_entry(
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(text, seq as i64).into(),
                terminate: None,
            },
        },
        seq,
        parent.map(str::to_owned),
        seq as i64,
    );
    SessionMutation::Entry {
        seq,
        timestamp: seq as i64,
        lane: Some("main".into()),
        entry,
    }
}
fn is_cold(storage: &tokio::sync::MutexGuard<'_, ResidentState>, id: &str) -> bool {
    matches!(storage.entry_ref(id), Some(JsonlPayload::Cold { .. }))
}
async fn strict_load(
    fs: Arc<dyn FileSystem>,
    path: &str,
) -> (JsonlSessionStorage, Arc<StrictReader>) {
    let inner = fs.open_read_file(path).await.unwrap().unwrap();
    let reader = Arc::new(StrictReader {
        inner,
        calls: AtomicUsize::new(0),
        ranges: std::sync::Mutex::new(Vec::new()),
    });
    let length = reader.byte_len().await.unwrap().unwrap();
    let storage = JsonlSessionStorage::load_streaming(
        fs,
        path,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
        reader.clone(),
        length,
        true,
    )
    .await
    .unwrap();
    (storage, reader)
}

#[tokio::test]
async fn os_ranged_load_large_unicode_rechain_duplicate_and_repair_keep_source_generation() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let first = message(
        "e1",
        1,
        None,
        &format!(
            "{}🌊\r\n{}",
            "x".repeat(CHUNK - 220),
            "🌊".repeat(CHUNK / 4)
        ),
    );
    // Forward gaps and a divergent parent are accepted; the cold reference
    // must apply the repaired parent without changing the original source row.
    let second = message("e2", 3, None, "second");
    let mut bytes = encode_header(&header()).replace('\n', "\r\n");
    bytes.push_str(&encode_mutation(&first).replace('\n', "\r\n"));
    bytes.push_str(&encode_mutation(&second));
    // Same mutation, distinct raw encoding: duplicate comparison is semantic.
    bytes.push_str("  ");
    bytes.push_str(encode_mutation(&first).trim_end());
    bytes.push_str(" \n");
    bytes.push_str("{\"kind\":");
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, reader) = strict_load(fs.clone(), "history.jsonl").await;
    assert!(reader.calls.load(Ordering::Relaxed) >= 3);
    assert!(is_cold(&storage.state.lock().await, "e1"));
    assert!(is_cold(&storage.state.lock().await, "e2"));
    assert_eq!(
        storage.get_entry("e2").await.unwrap().unwrap().parent_id(),
        Some("e1")
    );
    let original = storage.get_entry("e1").await.unwrap().unwrap();
    // Replace the published path again. The loaded offsets still belong to
    // the retired, stable source reader, not this new path's shorter bytes.
    let replacement = format!(
        "{}{}",
        encode_header(&header()),
        encode_mutation(&message("other", 1, None, "replacement"))
    );
    fs.write_file("replacement.tmp", FileContent::Text(replacement), None)
        .await
        .unwrap();
    fs.rename_file("replacement.tmp", "history.jsonl", None)
        .await
        .unwrap();
    assert_eq!(storage.get_entry("e1").await.unwrap().unwrap(), original);
    let appended = storage
        .append_entry(
            ProvisionedEntry {
                id: "e3".into(),
                kind: ProvisionedKind::Message {
                    message: rpi_ai::UserMessage::new("fresh", 4).into(),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    assert_eq!(appended.parent_id(), Some("e2"));
    assert!(is_cold(&storage.state.lock().await, "e3"));
    assert_eq!(storage.get_entry("e3").await.unwrap().unwrap(), appended);
    assert_eq!(storage.get_entry("e1").await.unwrap().unwrap(), original);
}

#[tokio::test]
async fn cold_entry_queries_windows_context_log_and_fork_match_eager() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mut eager = SessionState::new();
    let mut bytes = encode_header(&header());
    for seq in 1..=12 {
        let id = format!("e{seq}");
        let parent = (seq > 1).then(|| format!("e{}", seq - 1));
        let mutation = message(&id, seq, parent.as_deref(), &format!("user {seq} 🌊"));
        bytes.push_str(&encode_mutation(&mutation));
        eager.apply_mutation(mutation).unwrap();
    }
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, _) = strict_load(fs.clone(), "history.jsonl").await;
    for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
        for limit in [None, Some(1), Some(5)] {
            let query = EntryQuery {
                order: Some(order),
                limit,
                ..Default::default()
            };
            assert_eq!(
                storage.find_entries(&query).await.unwrap(),
                eager.find_entries(&query).unwrap()
            );
            let bounds = BranchBounds::default();
            assert_eq!(
                storage
                    .find_entries_on_branch(&query, &bounds, "e8")
                    .await
                    .unwrap(),
                eager.find_entries_on_branch(&query, &bounds, "e8").unwrap()
            );
        }
    }
    let summary = MessageSummaryQuery {
        limit: 3,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(storage.message_summaries("main", &summary).await.unwrap()).unwrap(),
        serde_json::to_value(eager.message_summaries("main", &summary).unwrap()).unwrap()
    );
    for query in [
        MessageWindowQuery {
            anchor_id: Some("e6".into()),
            limit: 4,
            ..Default::default()
        },
        MessageWindowQuery {
            before_id: Some("e6".into()),
            limit: 2,
            ..Default::default()
        },
        MessageWindowQuery {
            after_id: Some("e6".into()),
            limit: 2,
            ..Default::default()
        },
    ] {
        let actual = storage.message_window("main", &query).await.unwrap();
        let expected = eager.message_window("main", &query).unwrap();
        assert_eq!(actual.entries, expected.entries);
        assert_eq!(actual.has_more_before, expected.has_more_before);
        assert_eq!(actual.has_more_after, expected.has_more_after);
        assert_eq!(actual.leaf_id, expected.leaf_id);
    }
    assert_eq!(
        storage
            .context_on_branch(&BranchBounds::default(), "e12")
            .await
            .unwrap()
            .messages,
        eager
            .context_on_branch(&BranchBounds::default(), "e12")
            .unwrap()
            .messages
    );
    assert_eq!(
        storage.get_log(&LogOptions::default()).await.unwrap(),
        eager.get_log(&LogOptions::default()).unwrap()
    );
    storage.set_name(Some("fork name")).await.unwrap();
    storage.set_label("e6", Some("label")).await.unwrap();
    let options = ForkOptions::Branch {
        entry_id: Some("e8".into()),
        position: Some(ForkPosition::At),
    };
    let fork = storage
        .fork(
            "fork.jsonl",
            header(),
            &options,
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        fork.find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        8
    );
    assert_eq!(fork.get_name().await.unwrap().as_deref(), Some("fork name"));
    assert_eq!(
        fork.get_label("e6").await.unwrap().as_deref(),
        Some("label")
    );
    assert!(is_cold(&fork.state.lock().await, "e1"));
}

#[tokio::test]
async fn scanner_delimiters_preserve_raw_lines_offsets_flags_and_digest() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mut cases = vec![Vec::new(), b"\n\n".to_vec(), b"one\r\ntwo".to_vec()];
    for newline_offset in [CHUNK - 1, CHUNK, CHUNK + 1] {
        let mut bytes = vec![b'a'; newline_offset - 1];
        bytes.extend_from_slice(b"\r\n\n");
        // Keep raw scanning distinct from UTF-8/schema validation. Include
        // split multibyte text, an invalid byte and an unterminated tail.
        bytes.extend_from_slice("🌊".repeat(CHUNK / 4 + 1).as_bytes());
        bytes.extend_from_slice(b"\xff\nlast\r");
        cases.push(bytes);
    }
    for (index, bytes) in cases.into_iter().enumerate() {
        let path = format!("scanner-{index}");
        fs.write_file(&path, FileContent::Bytes(bytes.clone()), None)
            .await
            .unwrap();
        let reader = fs.open_read_file(&path).await.unwrap().unwrap();
        let mut scanner = Scanner::new(reader, bytes.len() as u64);
        let mut offset = 0;
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            let newline = part.last() == Some(&b'\n');
            let body = if newline {
                &part[..part.len() - 1]
            } else {
                part
            };
            let end = offset + part.len() as u64;
            assert_eq!(
                scanner.next().await.unwrap().unwrap(),
                (offset, body.to_vec(), end == bytes.len() as u64, newline),
                "case={index}, offset={offset}"
            );
            offset = end;
        }
        assert!(scanner.next().await.unwrap().is_none());
        assert_eq!(scanner.offset, bytes.len() as u64);
        assert_eq!(
            <[u8; 32]>::from(scanner.hash.finalize()),
            <[u8; 32]>::from(Sha256::digest(&bytes))
        );
    }
}

#[tokio::test]
async fn scanner_chunk_boundaries_utf8_crlf_and_final_newline() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let mutation = message("large", 1, None, &"🌊".repeat(CHUNK));
    let mut bytes = encode_header(&header());
    bytes.push_str(encode_mutation(&mutation).trim_end());
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, _) = strict_load(fs.clone(), "history.jsonl").await;
    let SessionMutation::Entry { entry, .. } = mutation else {
        unreachable!()
    };
    assert_eq!(storage.get_entry("large").await.unwrap().unwrap(), entry);
    assert!(fs
        .read_binary_file("history.jsonl", None)
        .await
        .unwrap()
        .ends_with(b"\n"));
    assert!(is_cold(&storage.state.lock().await, "large"));
}

#[tokio::test]
async fn bounded_header_lookup_does_not_bypass_full_history_utf8_validation() {
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let encoded_header = encode_header(&header());
    let mut bytes = encoded_header.as_bytes().to_vec();
    bytes.extend_from_slice(encode_mutation(&message("valid", 1, None, "synthetic")).as_bytes());
    // A complete malformed line, not a recoverable torn final write.
    bytes.extend_from_slice(b"\xff\n");
    fs.write_file("history.jsonl", FileContent::Bytes(bytes), None)
        .await
        .unwrap();
    assert_eq!(
        fs.read_text_lines("history.jsonl", Some(1), None)
            .await
            .unwrap(),
        [encoded_header.trim_end_matches('\n')]
    );
    let result = JsonlSessionStorage::load(
        fs,
        "history.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await;
    let error = result
        .err()
        .expect("full replay must reject invalid history");
    assert!(error.message.contains("invalid session UTF-8"), "{error}");
}

#[tokio::test]
async fn context_uses_metadata_before_compaction_and_payload_filter_precedes_limit() {
    use crate::session::types::{
        ActiveToolsEntry, CompactionEntry, CustomEntry, EntryBase, MessageEntry, ModelChangeEntry,
        ThinkingLevelEntry,
    };
    let temp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        temp.path().to_path_buf(),
    ));
    let base = |seq: u64, kind: &str| EntryBase {
        entry_type: kind.into(),
        id: format!("e{seq}"),
        seq,
        parent_id: (seq > 1).then(|| format!("e{}", seq - 1)),
        timestamp: seq as i64,
    };
    let mut entries = vec![
        Entry::ModelChange(ModelChangeEntry {
            base: base(1, "model_change"),
            provider: "provider".into(),
            model_id: "model".into(),
        }),
        Entry::ThinkingLevel(ThinkingLevelEntry {
            base: base(2, "thinking_level_change"),
            thinking_level: "high".into(),
        }),
        Entry::ActiveTools(ActiveToolsEntry {
            base: base(3, "active_tools_change"),
            active_tool_names: vec!["read".into()],
        }),
    ];
    let SessionMutation::Entry { entry: old, .. } =
        message("e4", 4, Some("e3"), &"old".repeat(CHUNK))
    else {
        unreachable!()
    };
    entries.push(old);
    entries.push(Entry::Compaction(CompactionEntry {
        base: base(5, "compaction"),
        summary: "summary".into(),
        retained_tail: Vec::new(),
        tokens_before: 100,
        details: None,
        usage: None,
    }));
    let SessionMutation::Entry { entry: tail, .. } = message("e6", 6, Some("e5"), "tail") else {
        unreachable!()
    };
    entries.push(tail);
    entries.push(Entry::Custom(CustomEntry {
        base: base(7, "custom"),
        custom_type: "extension".into(),
        data: Some(serde_json::json!({"large": "payload".repeat(CHUNK/7)})),
    }));
    for seq in 8..=10 {
        entries.push(Entry::Message(MessageEntry { base: base(seq,"message"), terminate: None,
            message: rpi_agent::message::AgentMessage::Custom(rpi_agent::message::CustomMessage::new("custom", Vec::new(), serde_json::json!({"__rpiOutbox":{"key": if seq == 8 { "wanted" } else { "other" }}}),seq as i64)),
        }));
    }
    let mut eager = SessionState::new();
    let mut bytes = encode_header(&header());
    let mut context_start = 0;
    for entry in entries {
        if entry.seq() == 5 {
            context_start = bytes.len() as u64;
        }
        let mutation = SessionMutation::Entry {
            seq: entry.seq(),
            timestamp: entry.base().timestamp,
            lane: Some("main".into()),
            entry,
        };
        bytes.push_str(&encode_mutation(&mutation));
        eager.apply_mutation(mutation).unwrap();
    }
    fs.write_file("history.jsonl", FileContent::Text(bytes), None)
        .await
        .unwrap();
    let (storage, reader) = strict_load(fs.clone(), "history.jsonl").await;
    let before = reader.calls.load(Ordering::Relaxed);
    let context = storage
        .context_on_branch(&BranchBounds::default(), "e7")
        .await
        .unwrap();
    let expected = eager
        .context_on_branch(&BranchBounds::default(), "e7")
        .unwrap();
    assert_eq!(context.messages, expected.messages);
    assert_eq!(context.model, expected.model);
    assert_eq!(context.thinking_level, expected.thinking_level);
    assert_eq!(context.active_tool_names, expected.active_tool_names);
    // Only compaction, its tail and the custom entry are hydrated. Model,
    // thinking and active-tools state come from small full-path facts.
    // Compaction and its tail coalesce; the >256 KiB custom entry stays alone.
    assert_eq!(reader.calls.load(Ordering::Relaxed) - before, 2);
    let ranges = reader.ranges.lock().unwrap();
    let selected_ranges = &ranges[ranges.len() - 2..];
    assert_eq!(selected_ranges[0].0, context_start);
    assert!(selected_ranges
        .iter()
        .all(|(offset, _)| *offset >= context_start));
    drop(ranges);
    let before = reader.calls.load(Ordering::Relaxed);
    let overview = storage.branch_overview("main").await.unwrap();
    assert_eq!(overview.model, expected.model);
    assert_eq!(overview.thinking_level, expected.thinking_level);
    assert_eq!(overview.active_tool_names, expected.active_tool_names);
    assert_eq!(
        reader.calls.load(Ordering::Relaxed),
        before,
        "overview must not hydrate historical payloads"
    );
    assert!(storage.branch_overview("missing-lane").await.is_err());
    let query = EntryQuery {
        outbox_key: Some("wanted".into()),
        limit: Some(1),
        ..Default::default()
    };
    let actual = storage.find_entries(&query).await.unwrap();
    assert_eq!(actual, eager.find_entries(&query).unwrap());
    assert_eq!(actual[0].id(), "e8");
    assert!(is_cold(&storage.state.lock().await, "e7"));
    assert_eq!(
        storage.get_entry("e7").await.unwrap().as_ref(),
        eager.get_entry("e7")
    );
    // A source change must be an error, never a missing entry or fake payload.
    let inner = fs.open_read_file("history.jsonl").await.unwrap().unwrap();
    drop(inner);
    fs.truncate_file("history.jsonl", 1, None).await.unwrap();
    assert!(storage.get_entry("e7").await.is_err());
}
