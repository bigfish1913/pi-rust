//! Mirrors `packages/ai/src/api/anthropic-messages.ts` — the SSE wire decoder
//! (`decodeSseLine` / `flushSseEvent` / `iterateSseMessages` /
//! `iterateAnthropicEvents`) ported to a pull-based state machine over a
//! `reqwest::Response::bytes_stream()`.
//!
//! The TS decoder is line-buffered: an empty line flushes the pending event;
//! `:` lines are comments; `event:`/`data:` set fields; multiple `data:` lines
//! are joined with `\n`. The Rust port reproduces that exactly, plus the
//! per-message validation: an `error` SSE event throws; a non-anthropic event
//! type is skipped; `message_start` without a `message_stop` throws. The
//! `RawMessageStreamEvent` union is modeled as a permissive `serde_json::Value`
//! (with a `type` discriminator) so the mapper can dispatch by event name
//! without binding to the Anthropic SDK's TS types.

use crate::error::AiError;
use crate::providers::anthropic::json_parse::parse_json_with_repair;
use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

/// The `ANTHROPIC_MESSAGE_EVENTS` set — event types the mapper consumes. Any
/// other event name is dropped (its `data` is never parsed). Mirrors the
/// `ANTHROPIC_MESSAGE_EVENTS` Set in the TS source.
pub const ANTHROPIC_MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// A decoded SSE frame. `event` is `None` when the server emitted no `event:`
/// line (Anthropic always sends one, but the spec allows nameless events).
#[derive(Debug, Clone)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    /// The raw `data:` line payloads (before `\n`-join) — preserved so error
    /// messages can mirror the TS `raw.join("\\n")` diagnostic.
    pub raw: Vec<String>,
}

/// Accumulator state for the line-buffered decoder. Mirrors `SseDecoderState`.
#[derive(Debug, Default)]
pub struct SseDecoderState {
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoderState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one complete line. Returns `Some(event)` when the line is empty
    /// (the flush trigger) and a pending event exists. Mirrors `decodeSseLine`.
    pub fn decode_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }

        self.raw.push(line.to_string());
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.find(':') {
            None => (line.to_string(), String::new()),
            Some(idx) => {
                let f = line[..idx].to_string();
                let mut v = line[idx + 1..].to_string();
                if let Some(stripped) = v.strip_prefix(' ') {
                    v = stripped.to_string();
                }
                (f, v)
            }
        };

        if field == "event" {
            self.event = Some(value);
        } else if field == "data" {
            self.data.push(value);
        }

        None
    }

    /// Flush the pending event, if any. Mirrors `flushSseEvent`.
    pub fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = ServerSentEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
            raw: std::mem::take(&mut self.raw),
        };
        self.data.clear();
        Some(event)
    }
}

/// Find the next `\r` or `\n` boundary, returning the byte index (or `None` if
/// neither is present). Mirrors `nextLineBreakIndex`.
fn next_line_break_index(text: &str) -> Option<usize> {
    let cr = text.find('\r');
    let nl = text.find('\n');
    match (cr, nl) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(a.min(b)),
    }
}

/// Split the first complete line off `text`, consuming a trailing `\r\n` or
/// single `\r`/`\n`. Returns `None` when no line boundary is present (so the
/// caller can buffer more bytes). Mirrors `consumeLine`.
fn consume_line(text: &str) -> Option<(&str, &str)> {
    let idx = next_line_break_index(text)?;
    let mut next = idx + 1;
    if text.as_bytes().get(idx) == Some(&b'\r') && text.as_bytes().get(next) == Some(&b'\n') {
        next += 1;
    }
    Some((&text[..idx], &text[next..]))
}

/// Pull-based SSE event stream over a `reqwest` response body. Mirrors the
/// async iterator `iterateSseMessages` — call `next_event` to pull one
/// `ServerSentEvent` at a time so the mapper can push wire events as they
/// arrive (true time-to-first-token) instead of buffering the whole body.
///
/// Honors `signal` between chunk reads: a cancellation surfaces as
/// `AiError::Abort`. Stream end surfaces the trailing flush (if any) then
/// `Ok(None)`.
pub struct SseEventStream {
    bytes_stream: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    state: SseDecoderState,
    buffer: String,
    signal: CancellationToken,
    done: bool,
}

impl SseEventStream {
    pub fn new(response: reqwest::Response, signal: CancellationToken) -> Self {
        Self {
            bytes_stream: response.bytes_stream().boxed(),
            state: SseDecoderState::new(),
            buffer: String::new(),
            signal,
            done: false,
        }
    }

    /// Pull the next SSE event, or `Ok(None)` when the body is exhausted (after
    /// the trailing flush). Mirrors one iteration of `iterateSseMessages`.
    pub async fn next_event(&mut self) -> Result<Option<ServerSentEvent>, AiError> {
        loop {
            // Try to flush a complete event from already-buffered lines first.
            // Take ownership of the buffer so we can mutate `self` while
            // dispatching on the borrowed slice (mirrors the TS string cursor).
            let buffer = std::mem::take(&mut self.buffer);
            let mut leftover = buffer;
            let mut produced = None;
            while let Some((line, rest)) = consume_line(&leftover) {
                let line_owned = line.to_string();
                leftover = rest.to_string();
                if let Some(event) = self.state.decode_line(&line_owned) {
                    produced = Some(event);
                    break;
                }
            }
            self.buffer = leftover;
            if let Some(event) = produced {
                return Ok(Some(event));
            }

            if self.done {
                // Tail flush: the last partial line (no terminator) then the
                // trailing event. Mirrors the post-loop decode + final flush.
                let mut buffer = std::mem::take(&mut self.buffer);
                if !buffer.is_empty() {
                    let pending = std::mem::take(&mut buffer);
                    if let Some(event) = self.state.decode_line(&pending) {
                        self.buffer = buffer;
                        return Ok(Some(event));
                    }
                    self.buffer = buffer;
                }
                return Ok(self.state.flush());
            }

            // Cancellation must race the body read itself. Checking the token
            // only before `.next().await` leaves the task stuck forever when a
            // server keeps the SSE connection open without sending another
            // chunk (and makes Ctrl+C/Esc appear to freeze the TUI).
            let next = tokio::select! {
                biased;
                _ = self.signal.cancelled() => {
                    return Err(AiError::Abort {
                        message: "Request was aborted".to_string(),
                    });
                }
                next = self.bytes_stream.next() => next,
            };

            match next {
                None => {
                    self.done = true;
                    continue;
                }
                Some(Err(e)) => {
                    return Err(AiError::Sse {
                        message: format!("error reading sse body: {e}"),
                    });
                }
                Some(Ok(chunk)) => {
                    self.buffer
                        .push_str(std::str::from_utf8(&chunk).unwrap_or(""));
                }
            }
        }
    }
}

/// Pull `ServerSentEvent`s from a streaming `reqwest` response body, honoring
/// `signal` between chunk reads. Mirrors `iterateSseMessages`. Returns
/// `AiError::Abort` when the token fires.
///
/// This is the collecting wrapper retained for tests; the streaming provider
/// path uses [`SseEventStream::next_event`] directly so events map as they
/// arrive.
pub async fn iterate_sse_messages(
    response: reqwest::Response,
    signal: &CancellationToken,
) -> Result<Vec<ServerSentEvent>, AiError> {
    let mut stream = SseEventStream::new(response, signal.clone());
    let mut out = Vec::new();
    while let Some(event) = stream.next_event().await? {
        out.push(event);
    }
    Ok(out)
}

/// Parse an SSE `data` payload into a typed-ish `AnthropicEvent`. Mirrors the
/// per-event `parseJsonWithRepair<RawMessageStreamEvent>(sse.data)` call in
/// `iterateAnthropicEvents`, plus its validation:
/// - `event: "error"` → `AiError::Provider { code: "sse_error", message: data }`.
/// - non-`ANTHROPIC_MESSAGE_EVENTS` → `AnthropicEvent::Skipped`.
/// - JSON parse failure → `AiError::Sse` with the TS-style diagnostic.
pub fn parse_anthropic_event(sse: &ServerSentEvent) -> Result<AnthropicEvent, AiError> {
    if sse.event.as_deref() == Some("error") {
        return Err(AiError::Provider {
            code: "sse_error".to_string(),
            message: sse.data.clone(),
        });
    }

    let event_name = match &sse.event {
        Some(name) if ANTHROPIC_MESSAGE_EVENTS.contains(&name.as_str()) => name.clone(),
        _ => return Ok(AnthropicEvent::Skipped),
    };

    let value: serde_json::Value = parse_json_with_repair(&sse.data).map_err(|e| AiError::Sse {
        message: format!(
            "Could not parse Anthropic SSE event {}: {}; data={}; raw={}",
            event_name,
            e,
            sse.data,
            sse.raw.join("\\n"),
        ),
    })?;

    Ok(AnthropicEvent::Message {
        event_type: event_name,
        payload: value,
    })
}

/// A decoded Anthropic SSE event. `Skipped` covers comments / ping / unknown
/// event types so the mapper can ignore them without a `continue` ripple.
#[derive(Debug, Clone)]
pub enum AnthropicEvent {
    /// A `message_start` / `message_delta` / `message_stop` / `content_block_*`
    /// event whose `data` parsed to a JSON object carrying a `type` field equal
    /// to the SSE `event:` name (Anthropic's convention).
    Message {
        event_type: String,
        payload: serde_json::Value,
    },
    /// A non-message SSE event (ping, comment, extension). Dropped by the mapper.
    Skipped,
}

impl AnthropicEvent {
    /// The `type` discriminator from the payload (equal to the SSE event name
    /// in practice). Convenience for the mapper's match.
    pub fn event_type(&self) -> Option<&str> {
        match self {
            AnthropicEvent::Message { event_type, .. } => Some(event_type),
            AnthropicEvent::Skipped => None,
        }
    }

    pub fn payload(&self) -> Option<&serde_json::Value> {
        match self {
            AnthropicEvent::Message { payload, .. } => Some(payload),
            AnthropicEvent::Skipped => None,
        }
    }
}

/// Decode a full SSE response into the message events the mapper consumes,
/// applying the `message_start`-without-`message_stop` invariant. Mirrors
/// `iterateAnthropicEvents` over `iterateSseMessages`.
pub async fn iterate_anthropic_events(
    response: reqwest::Response,
    signal: &CancellationToken,
) -> Result<Vec<AnthropicEvent>, AiError> {
    let frames = iterate_sse_messages(response, signal).await?;
    let mut saw_message_start = false;
    let mut saw_message_stop = false;
    let mut out = Vec::with_capacity(frames.len());

    for frame in frames {
        let event = parse_anthropic_event(&frame)?;
        match &event {
            AnthropicEvent::Message { event_type, .. } => {
                if event_type == "message_start" {
                    saw_message_start = true;
                } else if event_type == "message_stop" {
                    saw_message_stop = true;
                }
                out.push(event);
            }
            AnthropicEvent::Skipped => {}
        }
    }

    if saw_message_start && !saw_message_stop {
        return Err(AiError::Sse {
            message: "Anthropic stream ended before message_stop".to_string(),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(input: &str) -> Vec<ServerSentEvent> {
        let mut state = SseDecoderState::new();
        let mut out = Vec::new();
        for line in input.split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if let Some(event) = state.decode_line(trimmed) {
                out.push(event);
            }
        }
        // Tail flush (in case the input didn't end on an empty line).
        if let Some(event) = state.flush() {
            out.push(event);
        }
        out
    }

    #[test]
    fn decodes_simple_message_start() {
        let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
        let events = decode_all(sse);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"type\":\"message_start\"}");
        assert_eq!(events[0].raw.len(), 2);
    }

    #[test]
    fn joins_multiline_data_with_newline() {
        let sse = "event: content_block_delta\ndata: line1\ndata: line2\n\n";
        let events = decode_all(sse);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn comment_lines_are_dropped() {
        let sse = ": keepalive\nevent: ping\ndata: {}\n\nevent: message_start\ndata: {}\n\n";
        let events = decode_all(sse);
        // ping + message_start both flush.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[1].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn strips_single_leading_space_after_colon() {
        let sse = "event: message_delta\ndata: {\"x\":1}\n\n";
        let events = decode_all(sse);
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[test]
    fn nameless_event_has_none_event_field() {
        let sse = "data: only-data\n\n";
        let events = decode_all(sse);
        assert_eq!(events.len(), 1);
        assert!(events[0].event.is_none());
    }

    #[test]
    fn error_event_surfaces_provider_error() {
        let frame = ServerSentEvent {
            event: Some("error".to_string()),
            data: "rate limited".to_string(),
            raw: vec!["data: rate limited".to_string()],
        };
        let err = parse_anthropic_event(&frame).unwrap_err();
        assert!(matches!(err, AiError::Provider { code, .. } if code == "sse_error"));
    }

    #[test]
    fn non_message_event_is_skipped() {
        let frame = ServerSentEvent {
            event: Some("ping".to_string()),
            data: "{}".to_string(),
            raw: vec!["data: {}".to_string()],
        };
        let event = parse_anthropic_event(&frame).unwrap();
        assert!(matches!(event, AnthropicEvent::Skipped));
    }

    #[test]
    fn malformed_message_data_is_sse_error() {
        let frame = ServerSentEvent {
            event: Some("message_start".to_string()),
            data: "not json".to_string(),
            raw: vec!["data: not json".to_string()],
        };
        let err = parse_anthropic_event(&frame).unwrap_err();
        assert!(matches!(err, AiError::Sse { .. }));
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_body_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let mut stream = SseEventStream::new(response, signal);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next_event())
            .await
            .expect("cancellation must wake a pending response body read");
        assert!(matches!(result, Err(AiError::Abort { .. })));
        server.abort();
    }
}
