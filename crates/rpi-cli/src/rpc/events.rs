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
