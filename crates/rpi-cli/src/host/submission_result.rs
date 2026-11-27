//! Request-linked result projection. Native admission + get_result remain the
//! authorities; no transcript scan, current-tip answer, replay or new journal.
use super::*;
use rpi_ai::types::{AssistantMessage, Content};
use rpi_harness::agent_harness::{HarnessRunOutcome, HostAdmissionExecution};
use serde_json::{json, Value};

pub(super) async fn query(
    service: &HostService,
    host_id: HostId,
    agent_id: AgentId,
    request_id: RequestId,
    expected_run: Option<String>,
    offset: usize,
    max_bytes: usize,
) -> Reply {
    let command = Command::Result {
        agent_id,
        request_id,
        run_id: expected_run.clone(),
        offset,
        max_bytes,
    };
    if !command.valid(Limits::default()) {
        return Reply::Error {
            code: ErrorCode::InvalidRequest,
        };
    }
    let Ok(id) = String::from(agent_id).try_into() else {
        return Reply::Error {
            code: ErrorCode::InvalidRequest,
        };
    };
    let client = match service.ordinary_client(id) {
        Ok(client) => client,
        Err(error) => return service_error(error),
    };
    let status = match client.admission(&String::from(request_id)).await {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Reply::Data {
                data: json!({"found":false,"agent_id":agent_id,"request_id":request_id}),
            }
        }
        Err(error) => {
            return Reply::Error {
                code: backend_code(&error),
            }
        }
    };
    // A request id alone never grants another Host/Agent's result. Validate
    // the admitted owner before reading any assistant reference.
    if status.receipt.host_id != String::from(host_id)
        || status.receipt.agent_id != String::from(agent_id)
        || status.receipt.request_id != String::from(request_id)
    {
        return Reply::Error {
            code: ErrorCode::PersistenceUncertain,
        };
    }
    let mut data = json!({"found":true,"agent_id":agent_id,"request_id":request_id,
        "receipt":status.receipt,"execution":status.execution});
    let (run_id, finished) = match &status.execution {
        HostAdmissionExecution::Admitted {} => {
            if expected_run.is_some() {
                return Reply::Error {
                    code: ErrorCode::InvalidRequest,
                };
            }
            data["result"] = json!({"state":"admitted"});
            return Reply::Data { data };
        }
        HostAdmissionExecution::Started { run_id, finished } => (run_id, *finished),
    };
    if expected_run
        .as_ref()
        .is_some_and(|expected| expected != run_id)
    {
        return Reply::Error {
            code: ErrorCode::InvalidRequest,
        };
    }
    if !finished {
        let active = client.snapshot().await.ok().is_some_and(|snapshot| {
            !snapshot.retiring
                && !snapshot.closed
                && snapshot.fault.is_none()
                && snapshot.submitted_request_id.as_deref()
                    == Some(status.receipt.request_id.as_str())
                && snapshot
                    .active
                    .is_some_and(|active| active.run_id == *run_id)
        });
        data["result"] = json!({"state":if active { "running" } else { "unknown" },
            "run_id":run_id,"reason":if active { "active_native_run" } else { "no_terminal_or_active_run_proof" }});
        return Reply::Data { data };
    }
    let (result, covered) = match client.get_result(run_id, status.receipt.history).await {
        Ok(Some(result)) => result,
        // Legacy/missing references or read/sync failure are not success. Do
        // not guess an outcome from finished=true or leak raw storage errors.
        Ok(None) | Err(_) => {
            data["result"] =
                json!({"state":"unknown","run_id":run_id,"reason":"native_result_unavailable"});
            return Reply::Data { data };
        }
    };
    let (state, leaf, final_id, message, error) = match result.outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => (
            "completed",
            leaf_id,
            Some(final_entry_id),
            Some(final_message),
            None,
        ),
        HarnessRunOutcome::Aborted {
            leaf_id,
            final_entry_id,
            final_message,
        } => (
            "aborted",
            leaf_id,
            Some(final_entry_id),
            Some(final_message),
            None,
        ),
        HarnessRunOutcome::Failed {
            leaf_id,
            final_entry_id,
            final_message,
            error,
        } => {
            let (code, code_truncated) = bounded(&error.code, 128);
            let (message, message_truncated) = bounded(&error.message, 4096);
            (
                "failed",
                leaf_id,
                final_entry_id,
                final_message,
                Some(
                    json!({"code":code,"message":message,"truncated":code_truncated || message_truncated}),
                ),
            )
        }
        HarnessRunOutcome::Suspended { .. } => {
            data["result"] =
                json!({"state":"unknown","run_id":run_id,"reason":"no_terminal_native_result"});
            return Reply::Data { data };
        }
    };
    let output = match message
        .as_ref()
        .map(|message| text_page(message, offset, max_bytes))
        .transpose()
    {
        Ok(output) if output.is_some() || offset == 0 => output,
        _ => {
            return Reply::Error {
                code: ErrorCode::InvalidRequest,
            }
        }
    };
    data["result"] = json!({"state":state,"run_id":run_id,"leaf_id":leaf,
        "final_entry_id":final_id,"history":covered,"output":output,"error":error});
    Reply::Data { data }
}

fn bounded(text: &str, limit: usize) -> (&str, bool) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

// Concatenate only terminal assistant TEXT blocks; never export thinking,
// tool arguments, images, raw Provider payloads or another run's current tip.
// Offsets count UTF-8 bytes; reject non-boundaries rather than lose characters.
fn text_page(message: &AssistantMessage, offset: usize, max_bytes: usize) -> Result<Value, ()> {
    if !(4..=MAX_RESULT_TEXT_BYTES).contains(&max_bytes) {
        return Err(());
    }
    let texts = message.content.iter().filter_map(|block| match block {
        Content::Text(text) => Some(text.text.as_str()),
        _ => None,
    });
    let total = texts
        .clone()
        .try_fold(0usize, |sum, text| sum.checked_add(text.len()))
        .ok_or(())?;
    if offset > total {
        return Err(());
    }
    let mut output = String::with_capacity(max_bytes);
    let mut position = 0;
    for text in texts {
        let end = position + text.len();
        if offset >= end {
            position = end;
            continue;
        }
        let start = offset.saturating_sub(position);
        if !text.is_char_boundary(start) {
            return Err(());
        }
        let remaining = max_bytes - output.len();
        let (part, more) = bounded(&text[start..], remaining);
        output.push_str(part);
        if more || output.len() == max_bytes {
            break;
        }
        position = end;
    }
    let next = offset.checked_add(output.len()).ok_or(())?;
    Ok(
        json!({"text":output,"offset":offset,"bytes":next-offset,"total_bytes":total,
        "next_offset":if next < total { Some(next) } else { None }}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(texts: &[&str]) -> AssistantMessage {
        let mut message =
            AssistantMessage::empty(rpi_ai::Api::OpenaiCompletions, "fixture", "fixture", 1);
        message.content = texts.iter().map(|text| Content::text(*text)).collect();
        message
    }
    #[test]
    fn utf8_pages_are_exact_bounded_and_make_progress_across_blocks() {
        let message = message(&["a中", "😀b", "\n尾"]);
        let mut offset = 0;
        let mut collected = String::new();
        loop {
            let page = text_page(&message, offset, 4).unwrap();
            let text = page["text"].as_str().unwrap();
            assert!(text.len() <= 4);
            collected.push_str(text);
            let Some(next) = page["next_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(collected, "a中😀b\n尾");
        assert!(text_page(&message, 2, 4).is_err());
        assert!(text_page(&message, usize::MAX, 4).is_err());
        assert!(text_page(&message, 0, 3).is_err());
        assert!(text_page(&message, 0, MAX_RESULT_TEXT_BYTES + 1).is_err());
        assert_eq!(text_page(&message, collected.len(), 4).unwrap()["text"], "");
    }
    #[test]
    fn terminal_projection_omits_thinking_and_tool_arguments() {
        let mut message = message(&["answer"]);
        message.content.push(Content::thinking("private-thinking"));
        message.content.push(Content::tool_call(
            "call",
            "tool",
            json!({"private_argument":"secret"}),
        ));
        let page = text_page(&message, 0, MAX_RESULT_TEXT_BYTES).unwrap();
        assert_eq!(page["text"], "answer");
        assert!(!page.to_string().contains("private"));
        assert_eq!(page["next_offset"], Value::Null);
    }
    #[test]
    fn error_diagnostics_are_bounded_without_invalid_utf8() {
        let (value, truncated) = bounded("中😀tail", 4);
        assert_eq!(value, "中");
        assert!(truncated);
    }
}
