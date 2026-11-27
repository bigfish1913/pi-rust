//! The projection is only used after byte-for-byte source verification.
use super::*;
use async_trait::async_trait;
use rpi_tools::FileError;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct Reader {
    bytes: Mutex<Vec<u8>>,
    reads: AtomicUsize,
}
#[async_trait]
impl ReadFile for Reader {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        Ok(self.bytes.lock().unwrap().clone())
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let bytes = self.bytes.lock().unwrap();
        Ok(bytes
            .get(offset as usize..(offset as usize + length).min(bytes.len()))
            .unwrap_or_default()
            .to_vec())
    }
}

fn row() -> String {
    json!({
        "kind":"entry", "type":"message", "id":"entry", "seq":1,
        "parentId":null, "timestamp":1, "lane":"main",
        "message":{
            "kind":"tool_result", "role":"toolResult", "toolCallId":"call-a",
            "toolName":"read", "content":[{"type":"text","text":"🌊 output\r\n"}],
            "details":{"nested":[1, true, null, {"kind":"custom"}]},
            "isError":false, "timestamp":1
        }
    })
    .to_string()
}
fn source(line: &str) -> (Location, Arc<Reader>) {
    // Exactly the production precondition: the complete codec accepts the row
    // before its digest is captured; this is not a schema-validation fast path.
    let mutation = parse_mutation(line).unwrap();
    let reader = Arc::new(Reader {
        bytes: Mutex::new(line.as_bytes().to_vec()),
        reads: AtomicUsize::new(0),
    });
    let location = Location::new(reader.clone(), 0, line.as_bytes(), &mutation);
    (location, reader)
}

#[tokio::test]
async fn targeted_identity_matches_full_codec_and_rereads_without_a_cache() {
    let line = row();
    assert!(
        parse_tool_result_probe(&line).is_some(),
        "exercise the fast path"
    );
    let (location, reader) = source(&line);
    for count in 1..=2 {
        assert_eq!(
            location.read_tool_result_identity().await.unwrap(),
            Some(("call-a".into(), "read".into()))
        );
        assert_eq!(reader.reads.load(Ordering::Relaxed), count);
    }
    let mut value: serde_json::Value = serde_json::from_str(&line).unwrap();
    value["message"]["toolCallId"] = json!("escaped-\"-🌊");
    value["message"]["toolName"] = json!("read\nfile");
    let (location, _) = source(&value.to_string());
    let SessionMutation::Entry { entry, .. } = location.read().await.unwrap() else {
        panic!()
    };
    assert_eq!(
        location.read_tool_result_identity().await.unwrap(),
        tool_result_identity(&entry)
    );
    let expected = tool_result_identity(&entry);
    assert_eq!(
        JsonlPayload::Resident(Arc::new(entry))
            .tool_result_identity()
            .await
            .unwrap(),
        expected,
    );
}

#[tokio::test]
async fn targeted_identity_falls_back_for_duplicates_unknown_fields_and_non_tools() {
    let line = row();
    let mut unknown: serde_json::Value = serde_json::from_str(&line).unwrap();
    unknown["futureField"] = json!({"anything":[1,2,3]});
    let mut unknown_message: serde_json::Value = serde_json::from_str(&line).unwrap();
    unknown_message["message"]["futureField"] = json!(true);
    let mut non_tool: serde_json::Value = serde_json::from_str(&line).unwrap();
    non_tool["message"] = json!({"kind":"user", "role":"user", "content":"hello", "timestamp":1});
    for line in [
        line.replace(
            "\"toolCallId\":\"call-a\"",
            "\"toolCallId\":\"other\",\"toolCallId\":\"call-a\"",
        ),
        line.replacen("\"seq\":1", "\"seq\":9,\"seq\":1", 1),
        // Value's private raw-value key must not be interpreted by this probe.
        // Put it last so the complete codec treats it as an ordinary unknown key.
        format!(
            "{},\"$serde_json::private::RawValue\":\"null\"}}",
            &line[..line.len() - 1]
        ),
        unknown.to_string(),
        unknown_message.to_string(),
        non_tool.to_string(),
    ] {
        assert!(parse_tool_result_probe(&line).is_none());
        let (location, reader) = source(&line);
        let SessionMutation::Entry { entry, .. } = location.read().await.unwrap() else {
            panic!()
        };
        assert_eq!(
            location.read_tool_result_identity().await.unwrap(),
            tool_result_identity(&entry)
        );
        assert_eq!(
            reader.reads.load(Ordering::Relaxed),
            2,
            "fallback must not reread"
        );
    }
}

#[tokio::test]
async fn targeted_identity_rejects_changed_valid_json_short_reads_and_identity_mismatch() {
    let line = row();
    let (location, reader) = source(&line);
    *reader.bytes.lock().unwrap() = line.replace("call-a", "call-b").into_bytes();
    let error = location.read_tool_result_identity().await.unwrap_err();
    assert!(error.message.contains("source changed"));
    reader.bytes.lock().unwrap().pop();
    assert!(location
        .read_tool_result_identity()
        .await
        .unwrap_err()
        .message
        .contains("short"));
    for change_seq in [true, false] {
        let (mut location, _) = source(&line);
        if change_seq {
            location.seq += 1;
        } else {
            location.id = "other".into();
        }
        assert!(location
            .read_tool_result_identity()
            .await
            .unwrap_err()
            .message
            .contains("identity mismatch"));
    }
}

#[tokio::test]
async fn targeted_identity_keeps_oversized_payload_reads_bounded() {
    let mut value: serde_json::Value = serde_json::from_str(&row()).unwrap();
    value["message"]["details"] = json!({"large":"x".repeat(2 * 1024 * 1024)});
    let line = value.to_string();
    let (location, reader) = source(&line);
    assert_eq!(
        location.read_tool_result_identity().await.unwrap(),
        Some(("call-a".into(), "read".into()))
    );
    assert_eq!(reader.reads.load(Ordering::Relaxed), 3);
}
