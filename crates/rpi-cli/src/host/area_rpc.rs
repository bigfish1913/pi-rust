//! Native session RPC over an authenticated Host area connection.

use super::{
    client::{encode_frame, read_frame, write_frame, Failure},
    ordinary::{AreaPeer, BackendReport, OrdinaryClient, OrdinaryOutput},
    protocol::{Hello, HostId, RequestId, MAX_FRAME_BYTES, MAX_SUBMIT_PROMPT_BYTES},
    service::{attach_area_peer, HostService, ServiceError},
    AgentId, AgentState,
};
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_harness::agent_harness::HarnessRunOutcome;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::AsyncWrite,
    net::UnixStream,
    sync::{broadcast, Mutex},
};
use tokio_util::sync::CancellationToken;

const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMANDS: &[&str] = &[
    "get_state",
    "get_messages",
    "get_message_summaries",
    "get_message_window",
    "get_history",
    "get_available_models",
    "get_available_thinking_levels",
    "set_thinking_level",
    "get_commands",
    "prompt",
    "abort",
];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionHello {
    pub session_rpc: Hello,
    pub agent_id: AgentId,
}

/// Ephemeral delivery from the owning Harness. Slow readers reconnect and
/// fetch a fresh bounded transcript. No observer can slow an Agent down.
#[derive(Clone)]
pub(in crate::host) struct Events(broadcast::Sender<Arc<[u8]>>);

impl Events {
    pub(super) fn new() -> Self {
        Self(broadcast::channel(128).0)
    }

    fn send(&self, value: Value) {
        if let Ok(bytes) = encode_frame(&value, MAX_FRAME_BYTES) {
            let _ = self.0.send(bytes.into());
        } else {
            let _ = self.0.send(Arc::from(&b"{\"type\":\"host_error\",\"error\":\"Host event exceeded the frame budget\"}"[..]));
        }
    }

    pub(in crate::host) fn admitted(&self, messages: &[rpi_agent::AgentMessage], entry_id: &str) {
        if self.0.receiver_count() == 0 {
            return;
        }
        for (index, message) in messages.iter().enumerate() {
            let id = if index == 0 {
                entry_id.to_owned()
            } else {
                format!("{entry_id}-{index}")
            };
            self.send(json!({"type":"message_end", "messageId":id, "message":message}));
        }
    }

    pub(in crate::host) fn thinking_changed(&self, level: &Value) {
        self.send(json!({"type":"thinking_level_changed", "thinkingLevel":level}));
    }

    pub(in crate::host) fn settled(&self, report: &BackendReport, pending_resume: bool) {
        if self.0.receiver_count() == 0 {
            return;
        }
        let (outcome, error) = if report.persistence_uncertain || report.catalog_error.is_some() {
            ("failed", Some("Host could not confirm run settlement"))
        } else {
            match &report.outcome {
                Ok(OrdinaryOutput::Run(result)) => match &result.outcome {
                    HarnessRunOutcome::Completed { .. } => ("completed", None),
                    HarnessRunOutcome::Aborted { .. } => ("aborted", None),
                    HarnessRunOutcome::Failed { .. } => ("failed", Some("Host run failed")),
                    HarnessRunOutcome::Suspended { .. } => {
                        ("failed", Some("Host run is suspended"))
                    }
                },
                Ok(_) => ("completed", None),
                Err(_) => ("failed", Some("Host run failed")),
            }
        };
        self.send(json!({"type":"agent_settled", "outcome":outcome,
            "aborted":outcome == "aborted", "error":error,"pendingResume":pending_resume}));
    }
}

impl AgentEmitter for Events {
    fn emit(&self, event: AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.try_emit(event);
        Box::pin(async {})
    }

    fn try_emit(&self, event: AgentEvent) {
        if self.0.receiver_count() != 0 {
            let projected = crate::remote::protocol::RemoteEvent::from_agent_event(&event);
            if let Ok(value) = serde_json::to_value(projected) {
                self.send(value);
            }
        }
    }
}

#[derive(Default)]
pub(super) struct SessionRoutes {
    pub agents: BTreeSet<AgentId>,
    pub restore_on_attach: bool,
}

pub(super) struct Session {
    agent_id: AgentId,
    host_id: HostId,
    client: OrdinaryClient,
    events: Events,
    peer: AreaPeer,
}

impl Session {
    pub(super) async fn open(
        service: &HostService,
        agent_id: AgentId,
        host_id: HostId,
        restore: Option<(Arc<Mutex<()>>, super::daemon::DaemonAdministration)>,
    ) -> Result<Self, ServiceError> {
        let (client, (events, peer)) = if let Some((gate, administration)) = restore {
            let gate = gate.lock_owned().await;
            let service = service.clone();
            service
                .clone()
                .with_administration(move |reserved| {
                    Box::pin(async move {
                        // The owned transaction keeps serialization after a lost waiter.
                        let _gate = gate;
                        match service.ordinary_client(agent_id) {
                            Ok(client) => {
                                let attachment = attach_area_peer(&client).await?;
                                Ok((client, attachment))
                            }
                            Err(ServiceError::NotLoaded) => {
                                let record = reserved
                                    .catalog()
                                    .get(agent_id)
                                    .await
                                    .map_err(super::ordinary::BackendError::from)?;
                                if record.state != AgentState::Unloaded {
                                    return Err(ServiceError::NotLoaded);
                                }
                                let prepared = administration.prepare_attach(agent_id).await?;
                                if record.spec != prepared.spec {
                                    return Err(ServiceError::NotLoaded);
                                }
                                reserved
                                    .restore_for_attach(prepared, reserved.catalog().guard(&record))
                                    .await
                            }
                            Err(error) => Err(error),
                        }
                    })
                })
                .await?
        } else {
            let client = service.ordinary_client(agent_id)?;
            let attachment = attach_area_peer(&client).await?;
            (client, attachment)
        };
        Ok(Self {
            agent_id,
            host_id,
            client,
            events,
            peer,
        })
    }

    pub(super) async fn serve(&self, stream: UnixStream, stop: CancellationToken) {
        let _ = tokio::select! {
            biased;
            _ = stop.cancelled() => Ok(()),
            _ = self.peer.closed.cancelled() => Ok(()),
            result = self.exchange(stream) => result,
        };
    }

    async fn exchange(&self, stream: UnixStream) -> Result<(), Failure> {
        let mut events = self.events.0.subscribe();
        let (mut read, mut write) = stream.into_split();
        let state = tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.client.area_command("get_state", &Value::Null),
        )
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Closed)?;
        let ready = json!({"type":"ready","protocol":"rpi","version":1,
            "hostId":self.host_id,"agentId":self.agent_id,"sessionId":state["sessionId"],
            "state":state,"supportedCommands":COMMANDS});
        send(&mut write, &ready).await?;
        if state["activeOperation"].is_object() {
            send(&mut write, &json!({"type":"agent_start"})).await?;
        }
        loop {
            let request = read_frame::<_, Value>(&mut read, MAX_FRAME_BYTES);
            tokio::pin!(request);
            let command = loop {
                tokio::select! {
                    request = &mut request => break request?,
                    event = events.recv() => {
                        let event = event.map_err(|_| Failure::Closed)?;
                        timed_write(&mut write, &event).await?;
                    }
                }
            };
            let kind = command["type"].as_str().unwrap_or_default();
            let result = tokio::time::timeout(COMMAND_TIMEOUT, self.command(kind, &command))
                .await
                .unwrap_or_else(|_| {
                    Err("Host command timed out; inspect the Session before retrying".into())
                });
            let id = &command["id"];
            let response = match result {
                Ok(data) => {
                    json!({"type":"response","id":id,"command":kind,"success":true,"data":data})
                }
                Err(error) => {
                    json!({"type":"response","id":id,"command":kind,"success":false,"error":error})
                }
            };
            match send(&mut write, &response).await {
                Err(Failure::FrameTooLarge) => send(&mut write, &json!({
                    "type":"response","id":id,"command":kind,"success":false,"error":"Host response exceeds frame budget; request a smaller page"
                })).await?,
                result => result?,
            }
        }
    }

    async fn command(&self, kind: &str, msg: &Value) -> Result<Value, String> {
        match kind {
            "prompt" => {
                let prompt = msg["message"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty() && text.len() <= MAX_SUBMIT_PROMPT_BYTES)
                    .ok_or("Host area prompt requires bounded text")?;
                if msg.get("contentBlocks").is_some()
                    || msg.get("images").is_some_and(|v| v != &json!([]))
                {
                    return Err("Host area prompt supports text only".into());
                }
                let ticket = self
                    .client
                    .submit_admitted(
                        self.host_id.to_string(),
                        RequestId::new().to_string(),
                        vec![rpi_ai::UserMessage::new(
                            prompt.to_owned(),
                            crate::modes::rpc_now_ms(),
                        )
                        .into()],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let receipt = ticket.wait_admitted().await.map_err(|e| e.to_string())?;
                Ok(json!({"accepted": true, "requestId":receipt.request_id}))
            }
            "abort" => {
                let snapshot = self.client.snapshot().await.map_err(|e| e.to_string())?;
                let aborted = match snapshot.ticket {
                    Some(ticket) => self
                        .client
                        .cancel(ticket)
                        .await
                        .map_err(|e| e.to_string())?,
                    None => false,
                };
                Ok(json!({"aborted":aborted}))
            }
            "get_commands" => Ok(json!({"commands":[]})),
            _ => self.client.area_command(kind, msg).await,
        }
    }
}

async fn timed_write(writer: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> Result<(), Failure> {
    tokio::time::timeout(WRITE_TIMEOUT, write_frame(writer, frame))
        .await
        .map_err(|_| Failure::Timeout)?
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<(), Failure> {
    timed_write(writer, &encode_frame(value, MAX_FRAME_BYTES)?).await
}

#[cfg(target_os = "macos")]
pub(super) fn io_error(error: Failure) -> std::io::Error {
    std::io::Error::other(format!("Host session connection failed: {error:?}"))
}
