//! Event projection shared by the P0 native host and its clients.
use rpi_agent::events::AgentEvent;

/// Reuse the existing wire-safe event projection while the P0 host is moved
/// onto the dedicated `rpc` module. The projection contains no local handles
/// or provider credentials.
pub(crate) fn project(event: &AgentEvent) -> serde_json::Value {
    serde_json::to_value(crate::remote::protocol::RemoteEvent::from_agent_event(
        event,
    ))
    .unwrap_or(serde_json::Value::Null)
}

/// Encode frequent delta updates without building two full snapshot trees plus
/// a third projection tree. The JSON fields remain identical to `project`.
#[cfg(test)]
pub(crate) fn encode(event: &AgentEvent) -> serde_json::Result<Vec<u8>> {
    encode_with_message_id(event, None)
}

/// Keep the single-snapshot delta encoder used by the native streaming path.
/// The optional ID is assigned by the stateful RPC writer at MessageStart and
/// is copied onto every cumulative snapshot for that assistant message.
pub(crate) fn encode_with_message_id(
    event: &AgentEvent,
    message_id: Option<&str>,
) -> serde_json::Result<Vec<u8>> {
    use rpi_ai::AssistantMessageEvent;
    use std::sync::Arc;
    let AgentEvent::MessageUpdate {
        message,
        assistant_message_event,
        ..
    } = event
    else {
        let mut value = project(event);
        if let Some(id) = message_id {
            value["message"]["messageId"] = serde_json::Value::String(id.to_owned());
        }
        return serde_json::to_vec(&value);
    };
    let (index, delta, partial) = match assistant_message_event {
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            partial,
        }
        | AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            partial,
        }
        | AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            partial,
        } => (content_index, delta, partial),
        _ => {
            let mut value = project(event);
            if let Some(id) = message_id {
                value["message"]["messageId"] = serde_json::Value::String(id.to_owned());
            }
            return serde_json::to_vec(&value);
        }
    };
    let mut snapshot_value = serde_json::to_value(&**message)?;
    if let Some(id) = message_id {
        snapshot_value["messageId"] = serde_json::Value::String(id.to_owned());
    }
    let snapshot = serde_json::to_vec(&snapshot_value)?;
    let mut partial_value = serde_json::to_value(&**partial)?;
    if let Some(id) = message_id {
        partial_value["messageId"] = serde_json::Value::String(id.to_owned());
    }
    let other = if Arc::ptr_eq(message, partial) {
        None
    } else {
        Some(serde_json::to_vec(&partial_value)?)
    };
    let partial = other.as_deref().unwrap_or(&snapshot);
    let delta = serde_json::to_vec(delta)?;
    let tag = serde_json::to_vec(assistant_message_event.type_tag())?;
    let index = serde_json::to_vec(index)?;
    let mut out = Vec::with_capacity(snapshot.len() + partial.len() + delta.len() * 2 + 256);
    out.extend_from_slice(br#"{"type":"message_update","message":{"kind":"assistant","#);
    out.extend_from_slice(&snapshot[1..]);
    out.extend_from_slice(br#", "assistantMessageEvent":{"type":"#);
    out.extend_from_slice(&tag);
    out.extend_from_slice(br#", "contentIndex": "#);
    out.extend_from_slice(&index);
    out.extend_from_slice(br#", "delta": "#);
    out.extend_from_slice(&delta);
    out.extend_from_slice(br#", "partial":"#);
    out.extend_from_slice(partial);
    out.extend_from_slice(br#"},"eventType":"#);
    out.extend_from_slice(&tag);
    out.extend_from_slice(br#", "contentIndex": "#);
    out.extend_from_slice(&index);
    out.extend_from_slice(br#", "delta": "#);
    out.extend_from_slice(&delta);
    out.push(b'}');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::{Api, AssistantMessage, AssistantMessageEvent, Content};
    use std::sync::Arc;

    #[test]
    fn encoded_deltas_match_legacy_fields_with_shared_and_distinct_snapshots() {
        let delta = "quote\" slash\\ line\n nul\0 🐈";
        for shared in [false, true] {
            let mut message = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
            message.content = vec![Content::text(delta), Content::thinking("reasoning")];
            let message = Arc::new(message);
            let partial = if shared {
                Arc::clone(&message)
            } else {
                let mut other = (*message).clone();
                other.content = vec![Content::text("different snapshot")];
                Arc::new(other)
            };
            for event in [
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
                AssistantMessageEvent::ThinkingDelta {
                    content_index: 1,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
                AssistantMessageEvent::ToolCallDelta {
                    content_index: 2,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
            ] {
                let event = AgentEvent::MessageUpdate {
                    message: Arc::clone(&message),
                    assistant_message_event: event,
                    message_id: None,
                };
                let bytes = encode(&event).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    project(&event)
                );
            }
        }
    }

    #[test]
    fn structural_events_keep_the_existing_projection() {
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::AgentEnd { messages: vec![] },
            AgentEvent::MessageEnd {
                message: rpi_ai::UserMessage::new("hello", 0).into(),
                message_id: None,
            },
        ] {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&encode(&event).unwrap()).unwrap(),
                project(&event)
            );
        }
    }
}
