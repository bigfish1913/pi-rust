//! Native text-only sampling; no dependency on MCP or adapter crates.
use super::{protocol, writer::RpcWriter};
use rpi_agent::message::AgentMessage;
use rpi_ai::types::{AssistantMessage, Content, StopReason, UserMessage};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use serde_json::{json, Value};
use std::{collections::{HashMap, VecDeque}, time::Duration};
use tokio_util::sync::CancellationToken;

const RESPONSE_BYTES: usize = 256 * 1024;

// Conservative serialized result ceiling, before concatenation or JSON clones.
// Reserve 256 bytes for fixed keys/role/stop reason, count escaping exactly (or
// conservatively for short control escapes). Transport framing is separate.
fn bounded_text(content: &[Content], model: &str) -> Result<String, String> {
    let mut remaining = RESPONSE_BYTES - 256;
    for text in std::iter::once(model).chain(content.iter().map(|c| match c {
        Content::Text(t) => t.text.as_str(), _ => "",
    })) {
        for c in text.chars() {
            let n = match c { '"' | '\\' => 2, '\u{0}'..='\u{1f}' => 6, _ => c.len_utf8() };
            remaining = remaining.checked_sub(n).ok_or("sampling response byte budget exceeded")?;
        }
    }
    Ok(content.iter().filter_map(|c| match c { Content::Text(t) => Some(t.text.as_str()), _ => None }).collect())
}

struct Job { cancel: CancellationToken, task: tokio::task::JoinHandle<()> }
#[derive(Default)]
pub(crate) struct Samples { jobs: HashMap<String, Job>, retired: VecDeque<String> }

fn sample_id(msg: &Value) -> Result<String, String> {
    let id = msg["sampleId"].as_str().ok_or("sampleId must be a UUID")?;
    if id.len() > 128 || uuid::Uuid::parse_str(id).is_err() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err("sampleId must be a UUID".into());
    }
    Ok(id.to_ascii_lowercase())
}
struct Input { messages: Vec<AgentMessage>, system: String, tokens: u64, temperature: Option<f64>, timeout: Duration }
fn parse(msg: &Value) -> Result<Input, String> {
    let fail = || "invalid or unsupported sampling parameters".to_string();
    let request = msg.get("request").filter(|v| v.is_object()).ok_or_else(fail)?;
    if serde_json::to_vec(request).map_err(|_| fail())?.len() > 256 * 1024 { return Err(fail()); }
    for key in ["tools", "toolChoice"] { if request.get(key).is_some() { return Err(fail()); } }
    if request.get("includeContext").is_some_and(|v| v != "none") { return Err(fail()); }
    // Providers do not expose a uniform stop-sequence API. Never silently drop it.
    if request.get("stopSequences").is_some() { return Err("stopSequences unsupported".into()); }
    let tokens = request["maxTokens"].as_u64().filter(|n| (1..=16384).contains(n)).ok_or_else(fail)?;
    let system = match request.get("systemPrompt") { None => "", Some(v) => v.as_str().filter(|s| s.len() <= 64*1024).ok_or_else(fail)? }.to_owned();
    let temperature = match request.get("temperature") { None => None, Some(v) => Some(v.as_f64().filter(|n| n.is_finite() && (0.0..=2.0).contains(n)).ok_or_else(fail)?) };
    let timeout = match msg.get("timeoutMs") { None => 60000, Some(v) => v.as_u64().filter(|n| (1..=300000).contains(n)).ok_or_else(fail)? };
    let rows = request["messages"].as_array().filter(|v| !v.is_empty() && v.len() <= 128).ok_or_else(fail)?;
    let mut messages = Vec::with_capacity(rows.len());
    for row in rows {
        if row["content"]["type"] != "text" { return Err(fail()); }
        let text = row["content"]["text"].as_str().ok_or_else(fail)?;
        messages.push(match row["role"].as_str() {
            Some("user") => AgentMessage::User(UserMessage::new(text, 0)),
            Some("assistant") => AgentMessage::Assistant(Box::new(serde_json::from_value::<AssistantMessage>(json!({
                "role":"assistant", "content":[{"type":"text","text":text}],
                "api":"anthropic-messages", "provider":"sampling", "model":"sampling",
                "usage":rpi_ai::types::Usage::zero(), "stopReason":"stop", "timestamp":0
            })).map_err(|_| fail())?)),
            _ => return Err(fail()),
        });
    }
    Ok(Input { messages, system, tokens, temperature, timeout: Duration::from_millis(timeout) })
}

impl Samples {
    fn retire(&mut self, id: String) {
        self.retired.push_back(id);
        while self.retired.len() > 128 { self.retired.pop_front(); }
    }
    async fn reap(&mut self) {
        let ids: Vec<_> = self.jobs.iter().filter(|(_, j)| j.task.is_finished()).map(|(id, _)| id.clone()).collect();
        for id in ids { if let Some(job) = self.jobs.remove(&id) { let _ = job.task.await; self.retire(id); } }
    }
    pub(crate) async fn start(&mut self, parent: &AgentHarness, msg: &Value, out: RpcWriter) -> Result<(), String> {
        self.reap().await;
        let id = sample_id(msg)?;
        if self.jobs.len() >= 4 || self.jobs.contains_key(&id) || self.retired.contains(&id) { return Err("sample unavailable or duplicate sampleId".into()); }
        let input = parse(msg)?;
        let deadline = tokio::time::Instant::now() + input.timeout;
        let harness = parent.create_isolated_text(input.system, input.tokens, input.temperature, RESPONSE_BYTES).await.map_err(|_| "sampling unavailable: provider must support response budgets")?;
        let model = harness.get_model().await.map_err(|_| "sampling model unavailable")?;
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let rpc_id = msg.get("id").cloned();
        let task = tokio::spawn(async move {
            let lane = harness.lane("main");
            let run = lane.prompt_messages(input.messages);
            tokio::pin!(run);
            let result = tokio::select! {
                biased;
                _ = signal.cancelled() => { let _ = lane.abort().await; let _ = harness.close().await; let _ = run.await; Err("sampling cancelled".into()) },
                _ = tokio::time::sleep_until(deadline) => { let _ = lane.abort().await; let _ = harness.close().await; let _ = run.await; Err("sampling deadline exceeded".into()) },
                result = &mut run => match result {
                    Ok(result) => match result.outcome {
                        HarnessRunOutcome::Completed { final_message: m, .. } => {
                            let stop = match m.stop_reason { StopReason::Stop => Some("endTurn"), StopReason::Length => Some("maxTokens"), _ => None };
                            if let Some(stop) = stop {
                                if m.content.iter().all(|c| matches!(c, Content::Text(_))) {
                                    bounded_text(&m.content, &model.id).map(|text| Some(json!({"role":"assistant", "content":{"type":"text","text":text}, "model":model.id, "stopReason":stop})))
                                } else { Err("sampling returned non-text content".into()) }
                            } else { Err("sampling failed".into()) }
                        },
                        _ => Err("sampling failed".into()),
                    },
                    Err(_) => Err("sampling failed".into()),
                },
            };
            let _ = harness.close().await;
            // Provider has settled and isolation is closed before bounded terminal
            // enqueue. Failure signals the native connection owner, never a
            // detached delivery task or a silently dropped correlated success.
            let _ = out.write_bounded(&protocol::response(rpc_id, "sample", result)).await;
        });
        self.jobs.insert(id, Job { cancel, task });
        Ok(())
    }
    pub(crate) async fn cancel(&mut self, msg: &Value) -> Result<Value, String> {
        let id = sample_id(msg)?;
        if let Some(job) = self.jobs.remove(&id) {
            job.cancel.cancel();
            let _ = job.task.await;
            self.retire(id.clone());
        }
        Ok(json!({"sampleId": id, "cancelled":true}))
    }
    pub(crate) async fn shutdown(&mut self) {
        for job in self.jobs.values() { job.cancel.cancel(); }
        for (id, job) in self.jobs.drain().collect::<Vec<_>>() { let _ = job.task.await; self.retire(id); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_text_counts_json_escaping_before_result_construction() {
        for (unit, cost) in [("x", 1), ("\"", 2), ("\\", 2), ("\0", 6), ("\n", 6), ("界", 3)] {
            let count = (RESPONSE_BYTES - 256 - "fixture".len()) / cost;
            let text = unit.repeat(count);
            let accepted = bounded_text(&[Content::text(&text)], "fixture").unwrap();
            let result = json!({"role":"assistant", "content":{"type":"text","text":accepted}, "model":"fixture", "stopReason":"endTurn"});
            assert!(serde_json::to_vec(&result).unwrap().len() <= RESPONSE_BYTES);
            assert!(bounded_text(&[Content::text(unit.repeat(count + 1))], "fixture").is_err());
        }
        assert!(bounded_text(&[Content::text("x".repeat(RESPONSE_BYTES / 2)), Content::text("x".repeat(RESPONSE_BYTES / 2))], "fixture").is_err());
        assert!(bounded_text(&[], &"m".repeat(RESPONSE_BYTES)).is_err());
    }

    #[test]
    fn independent_validation() {
        let mut msg = json!({"request":{"maxTokens":7,"messages":[{"role":"user","content":{"type":"text","text":"hi"}}]}});
        assert!(parse(&msg).is_ok());
        for (k,v) in [("maxTokens",json!(16385)), ("tools",json!([])), ("stopSequences",json!(["stop"])), ("includeContext",json!("allServers"))] {
            let mut bad=msg.clone(); bad["request"][k]=v; assert!(parse(&bad).is_err());
        }
        msg["request"]["messages"][0]["role"]=json!("system"); assert!(parse(&msg).is_err());
    }
}
