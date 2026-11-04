//! Invocation-owned finalized results. Request disposal is deliberately independent.
use super::*;
use tokio::sync::OwnedSemaphorePermit;

pub(super) const TREE_BYTES: usize = 64 * 1024 * 1024;
#[derive(Default)]
pub(super) struct Store(HashMap<String, Retained>);
struct Retained {
    native: crate::AgentToolResult,
    is_error: bool,
    encoded: Box<str>,
    _bytes: OwnedSemaphorePermit,
}
impl Store {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}
// JSON's encoded punctuation can be much smaller than its native Value/map
// nodes. Include a conservative per-node allocation estimate, not just strings.
fn value_overhead(value: &Value) -> Option<usize> {
    let add = |sum: usize, child: &Value| sum.checked_add(value_overhead(child)?);
    match value {
        Value::Array(values) => values.iter().try_fold(128, add),
        Value::Object(values) => values.values().try_fold(128, add),
        _ => Some(128),
    }
}
impl ToolProgram {
    pub(super) fn retain_result(
        &self,
        id: &str,
        native: crate::AgentToolResult,
        is_error: bool,
        encoded: String,
    ) -> Result<Value, String> {
        // Charge encoded bytes plus a native payload estimate and structural overhead.
        // The shared permit follows ownership and is returned on release/finish/drop.
        let charge = encoded
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(4096))
            .and_then(|n| n.checked_add(value_overhead(&native.details)?))
            .and_then(|n| n.checked_add(native.content.len().checked_mul(256)?))
            .filter(|n| *n <= TREE_BYTES)
            .ok_or("ToolProgram result reference byte quota exceeded")?;
        let mut state = self.state.lock().unwrap();
        if state.results.0.len() >= 16 {
            return Err("ToolProgram retained result reference limit exceeded".into());
        }
        let bytes = self
            .result_bytes
            .clone()
            .try_acquire_many_owned(charge as u32)
            .map_err(|_| "ToolProgram invocation-tree result reference byte quota exceeded")?;
        let manifest = json!({"requestId":id,"status":"done","isError":is_error,
            "resultReference":{"version":1,"id":id,"byteLength":encoded.len(),
                "contentBlocks":native.content.len(),"isError":is_error,"terminate":native.terminate}});
        state.results.0.insert(
            id.to_owned(),
            Retained {
                native,
                is_error,
                encoded: encoded.into_boxed_str(),
                _bytes: bytes,
            },
        );
        Ok(manifest)
    }
    pub(super) fn read_result(&self, args: &Value) -> Result<Value, String> {
        let id = args["resultId"].as_str().ok_or("resultId is required")?;
        let offset = integer(args, "offset", 0, 0, u64::MAX)?;
        let limit = integer(args, "limit", 65536, 4, 65536)? as usize;
        let state = self.state.lock().unwrap();
        if self.signal.is_cancelled() {
            return Err("parent invocation is no longer active".into());
        }
        let retained = state
            .results
            .0
            .get(id)
            .ok_or("unknown result reference for this invocation")?;
        let text = &retained.encoded;
        let offset = usize::try_from(offset).map_err(|_| "offset is past EOF")?;
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err("offset must be a UTF-8 boundary at or before EOF".into());
        }
        let mut end = offset.saturating_add(limit).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let reply = json!({"resultId":id,"offset":offset,"data":&text[offset..end],
            "byteLength":text.len(),"nextOffset":if end < text.len() {Some(end)} else {None}});
        // Even worst-case JSON escaping of 64 KiB stays below the frame limit.
        bounded(&reply)?;
        Ok(reply)
    }
    pub(super) fn release_result(&self, args: &Value) -> Result<Value, String> {
        let id = args["resultId"].as_str().ok_or("resultId is required")?;
        self.state
            .lock()
            .unwrap()
            .results
            .0
            .remove(id)
            .ok_or("unknown result reference for this invocation")?;
        Ok(json!({"resultId":id,"status":"released"}))
    }
    /// Resolve native authority only within this owning program's active lifetime.
    /// Usage/names remain in the receipt for inspection but are merged by host
    /// accounting, never by the attachment. No transcript calls are synthesized.
    pub fn resolve_result_reference(
        &self,
        id: &str,
    ) -> Result<(crate::AgentToolResult, bool), String> {
        let state = self.state.lock().unwrap();
        if self.signal.is_cancelled() {
            return Err("parent invocation is no longer active".into());
        }
        let retained = state
            .results
            .0
            .get(id)
            .ok_or("unknown result reference for this invocation")?;
        let mut result = retained.native.clone();
        result.usage = None;
        result.added_tool_names.clear();
        Ok((result, retained.is_error))
    }
}
