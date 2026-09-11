//! OpenAI Responses API provider (`/v1/responses`).

use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::{Model, StreamingProtocolCompat};
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::anthropic::json_parse::parse_streaming_json;
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;
use crate::types::{
    Api, AssistantMessage, AssistantMessageEvent, Content, Context, DoneReason, ErrorReason,
    Message, StopReason, ThinkingLevel, ToolCall, ToolCallType, Usage, UserContent,
};
use crate::AiError;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct OpenAiResponsesProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}
pub fn openai_responses_models() -> Vec<Model> {
    let mut model = Model::new(
        "gpt-6-astra",
        "GPT-6 Astra",
        Api::OpenaiResponses,
        "openai",
        "https://api.openai.com/v1",
    );
    model.reasoning = true;
    model.context_window = 1_000_000;
    model.max_tokens = 128_000;
    model.compat = Some(StreamingProtocolCompat::OpenaiResponses(
        crate::model::OpenaiResponsesCompat {
            supports_developer_role: Some(true),
            supports_long_cache_retention: Some(true),
            supports_strict_mode: Some(true),
        },
    ));
    vec![model]
}
impl OpenAiResponsesProvider {
    pub fn with_models(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            http,
            models,
        }
    }
}
#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) = create_assistant_message_event_stream();
        let http = self.http.clone();
        let key = self.api_key.clone();
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        let opts = opts.clone();
        tokio::spawn(async move {
            run_stream(&mut producer, http, key, &model, &ctx, &opts).await;
        });
        stream
    }
}

async fn run_stream(
    p: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    provider_key: Option<String>,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) {
    let mut out = AssistantMessage::empty(
        Api::OpenaiResponses,
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    let mut started = false;
    let mut text_started = false;
    let mut text = String::new();
    let mut thinking_started = false;
    let mut thinking = String::new();
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(extra) = &opts.headers {
        for (k, v) in extra {
            headers.insert(k.clone(), v.clone());
        }
    }
    let key = opts.api_key.clone().or(provider_key).or_else(|| {
        std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|v| !v.is_empty())
    });
    if let Some(key) = key {
        headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
        headers.insert("authorization".into(), format!("Bearer {key}"));
    }
    if !headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-api-key"))
    {
        emit_error(p, &mut out, "No API key", false);
        return;
    }
    let body = build_request(model, ctx, opts);
    let url = responses_url(&model.base_url);
    let signal = opts.signal.clone();
    let response = retry_provider_request(move || { let http=http.clone(); let url=url.clone(); let body=body.clone(); let headers=headers.clone(); let signal=signal.clone(); async move {
        let mut req=http.post(&url).json(&body); if let Some(t)=opts.timeout { req=req.timeout(t); } for (k,v) in headers { req=req.header(k,v); }
        let response = tokio::select! { r=req.send()=>r.map_err(|e| AiError::Http{status:None,message:e.to_string()})?, _=signal.cancelled()=>return Err(AiError::Abort{message:"Request aborted".into()})};
        if response.status().is_success() { Ok(response) } else { let status=response.status().as_u16(); let msg=response.text().await.unwrap_or_default(); Err(AiError::Http{status:Some(status),message:msg}) }
    }}, opts.max_retries, opts.max_retry_delay, &opts.signal).await;
    let response = match response {
        Ok(r) => r,
        Err(e) => {
            emit_error(p, &mut out, e.to_string(), e.is_abort());
            return;
        }
    };
    let mut events = SseEventStream::new(response, opts.signal.clone());
    loop {
        match events.next_event().await {
            Ok(Some(ev)) => {
                let kind = ev.event.as_deref().unwrap_or("");
                let value: Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !started {
                    started = true;
                    p.push(AssistantMessageEvent::Start {
                        partial: Arc::new(out.clone()),
                    });
                }
                match kind {
                    "response.output_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if !text_started {
                                text_started = true;
                                out.content.push(Content::text(""));
                                p.push(AssistantMessageEvent::TextStart {
                                    content_index: 0,
                                    partial: Arc::new(out.clone()),
                                });
                            }
                            text.push_str(delta);
                            if let Some(Content::Text(t)) = out.content.get_mut(0) {
                                t.text.push_str(delta);
                            }
                            p.push(AssistantMessageEvent::TextDelta {
                                content_index: 0,
                                delta: delta.into(),
                                partial: Arc::new(out.clone()),
                            });
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            let index = if thinking_started {
                                out.content.len().saturating_sub(1)
                            } else {
                                thinking_started = true;
                                let index = out.content.len();
                                out.content.push(Content::thinking(""));
                                p.push(AssistantMessageEvent::ThinkingStart {
                                    content_index: index,
                                    partial: Arc::new(out.clone()),
                                });
                                index
                            };
                            thinking.push_str(delta);
                            if let Some(Content::Thinking(t)) = out.content.get_mut(index) {
                                t.thinking.push_str(delta);
                            }
                            p.push(AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta: delta.to_string(),
                                partial: Arc::new(out.clone()),
                            });
                        }
                    }
                    "response.completed" => {
                        if let Some(resp) = value.get("response") {
                            apply_response(&mut out, resp, p, &mut text_started, &mut text);
                        }
                        finish(p, out, model, text_started);
                        return;
                    }
                    "response.failed" | "error" => {
                        let msg = value
                            .get("error")
                            .and_then(|v| v.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("Responses API error");
                        emit_error(p, &mut out, msg, false);
                        return;
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                finish(p, out, model, text_started);
                return;
            }
            Err(e) => {
                emit_error(p, &mut out, e.to_string(), e.is_abort());
                return;
            }
        }
    }
}

fn build_request(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> Value {
    let mut input = Vec::new();
    for m in &ctx.messages {
        match m { Message::User(u)=>input.push(json!({"role":"user","content":user_content(&u.content)})), Message::Assistant(a)=>input.push(json!({"role":"assistant","content":Content::text_only(&a.content,"\n")})), Message::ToolResult(t)=>input.push(json!({"type":"function_call_output","call_id":t.tool_call_id,"output":Content::text_only(&t.content,"\n")})) }
    }
    let mut body = json!({"model":model.id,"input":input,"stream":true});
    if let Some(s) = &ctx.system_prompt {
        body["instructions"] = Value::String(s.clone());
    }
    let max = opts.max_tokens.unwrap_or(model.max_tokens);
    if max > 0 {
        body["max_output_tokens"] = Value::from(max);
    }
    if model.reasoning {
        if let Some(e) = reasoning_effort(opts.reasoning_level()) {
            body["reasoning"] = json!({"effort":e});
        }
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(ctx.tools.iter().map(|t| {
            let mut tool = json!({"type":"function","name":t.name,"description":t.description,"parameters":t.parameters.as_value()});
            if supports_strict_mode(model) { tool["strict"] = Value::Bool(true); }
            tool
        }).collect());
    }
    if let Some(params) = &model.sampling_params {
        if let Value::Object(map) = &mut body {
            for (k, v) in params {
                map.insert(k.clone(), v.clone());
            }
        }
    }
    body
}
fn user_content(c: &UserContent) -> Value {
    match c {
        UserContent::Text(s) => Value::String(s.clone()),
        UserContent::Blocks(b) => Value::Array(
            b.iter()
                .filter_map(|x| {
                    if let Content::Text(t) = x {
                        Some(json!({"type":"input_text","text":t.text}))
                    } else {
                        None
                    }
                })
                .collect(),
        ),
    }
}
fn responses_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/responses")
    } else {
        format!("{b}/v1/responses")
    }
}
fn reasoning_effort(l: ThinkingLevel) -> Option<&'static str> {
    match l {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high"),
    }
}
fn supports_strict_mode(model: &Model) -> bool {
    match model.compat.as_ref() {
        Some(StreamingProtocolCompat::OpenaiResponses(c)) => {
            c.supports_strict_mode.unwrap_or(false)
        }
        _ => false,
    }
}
fn apply_response(
    out: &mut AssistantMessage,
    resp: &Value,
    p: &mut AssistantMessageEventStreamProducer,
    text_started: &mut bool,
    text: &mut String,
) {
    if let Some(id) = resp.get("id").and_then(Value::as_str) {
        out.response_id = Some(id.into());
    }
    if let Some(m) = resp.get("model").and_then(Value::as_str) {
        out.response_model = Some(m.into());
    }
    if let Some(u) = resp.get("usage") {
        out.usage = parse_usage(u);
    }
    if let Some(items) = resp.get("output").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                let idx = out.content.len();
                let call = ToolCall {
                    kind: ToolCallType,
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    arguments: parse_streaming_json(item.get("arguments").and_then(Value::as_str)),
                    thought_signature: None,
                    namespace: None,
                };
                out.content.push(Content::ToolCall(call.clone()));
                p.push(AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    partial: Arc::new(out.clone()),
                });
                p.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call: call,
                    partial: Arc::new(out.clone()),
                });
            }
        }
    }
    if *text_started {
        p.push(AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text.clone(),
            partial: Arc::new(out.clone()),
        });
    }
}
fn parse_usage(v: &Value) -> Usage {
    let input = v.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let output = v.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
    let cached = v
        .get("input_tokens_details")
        .and_then(|x| x.get("cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Usage {
        input: (input - cached).max(0),
        output,
        cache_read: cached,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: Default::default(),
    }
}
fn finish(
    p: &mut AssistantMessageEventStreamProducer,
    mut out: AssistantMessage,
    model: &Model,
    _has_text: bool,
) {
    for (index, content) in out.content.iter().enumerate() {
        if let Content::Thinking(t) = content {
            p.push(AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: t.thinking.clone(),
                partial: Arc::new(out.clone()),
            });
        }
    }
    if out
        .content
        .iter()
        .any(|c| matches!(c, Content::ToolCall(_)))
    {
        out.stop_reason = StopReason::ToolUse;
    } else {
        out.stop_reason = StopReason::Stop;
    }
    out.usage.cost = calculate_cost(&model.cost, &out.usage);
    p.push(AssistantMessageEvent::Done {
        reason: if matches!(out.stop_reason, StopReason::ToolUse) {
            DoneReason::ToolUse
        } else {
            DoneReason::Stop
        },
        message: out,
    });
}
fn emit_error(
    p: &mut AssistantMessageEventStreamProducer,
    out: &mut AssistantMessage,
    msg: impl Into<String>,
    aborted: bool,
) {
    out.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    out.error_message = Some(msg.into());
    p.push(AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorReason::Aborted
        } else {
            ErrorReason::Error
        },
        error: out.clone(),
    });
}
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
