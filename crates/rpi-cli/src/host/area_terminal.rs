//! Terminal and stdio adapters for the same authenticated area session.

use super::{
    area::Connection,
    area_rpc::{io_error, SessionHello},
    client::{encode_frame, read_frame, write_frame},
    protocol::{Hello, Version, MAX_FRAME_BYTES, PROTOCOL},
};
use crate::{
    remote::{protocol::RemoteEvent, session::message_text},
    session_driver::{map_remote_event, DriverCommand, DriverEvent, DriverStatus, SessionDriver},
    transcript_view::UiEvent,
};
use serde_json::{json, Value};
use std::{io, sync::Mutex, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::{unix::OwnedWriteHalf, UnixStream},
    sync::mpsc,
};

pub(super) async fn run(connection: Connection, rpc: bool) -> Result<(), String> {
    use std::io::IsTerminal;
    let Connection {
        name,
        control,
        agent_id,
        session_id,
        session_path,
        client,
        ..
    } = connection;
    drop(client);
    let stream = tokio::time::timeout(control.timeout, async {
        let mut stream = UnixStream::connect(&control.socket_path).await?;
        if stream.peer_cred()?.uid() != nix::unistd::Uid::effective().as_raw() {
            return Err(io::Error::other("unsafe area Host peer"));
        }
        let hello = SessionHello {
            session_rpc: Hello {
                protocol: PROTOCOL.into(),
                version: Version::default(),
                host_id: control.host_id,
                token: control.token,
            },
            agent_id: agent_id.to_string().try_into().map_err(io::Error::other)?,
        };
        write_value(&mut stream, &serde_json::to_value(hello)?).await?;
        Ok::<_, io::Error>(stream)
    })
    .await
    .map_err(|_| "area Session connection timed out")?
    .map_err(|e| e.to_string())?;
    let (mut read, mut write) = stream.into_split();
    let mut ready: Value =
        tokio::time::timeout(control.timeout, read_frame(&mut read, MAX_FRAME_BYTES))
            .await
            .map_err(|_| "area Session readiness timed out")?
            .map_err(|e| io_error(e).to_string())?;
    if ready["type"] != "ready"
        || ready["hostId"] != control.host_id.to_string()
        || ready["agentId"] != agent_id.to_string()
        || ready["sessionId"] != session_id
    {
        return Err("area Session identity did not match its configuration".into());
    }
    if rpc {
        ready["sharedHostArea"] = json!(name);
        ready["sessionPath"] = json!(session_path);
        return bridge(read, write, ready).await.map_err(|e| e.to_string());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("Joined area {name} · Session {session_id}");
        return Ok(());
    }
    write_value(
        &mut write,
        &json!({"type":"get_messages","id":"history","limit":100}),
    )
    .await
    .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::unbounded_channel();
    let mut status = status(&ready["state"]);
    let _ = tx.send(DriverEvent::Status(status.clone()));
    let reader = tokio::spawn(async move {
        while let Ok(frame) = read_frame::<_, Value>(&mut read, MAX_FRAME_BYTES).await {
            let event = match frame["type"].as_str() {
                Some("response") if frame["success"] != true => frame["error"]
                    .as_str()
                    .map(|error| DriverEvent::Notice(error.to_owned())),
                Some("response") if frame["command"] == "get_state" => {
                    status = self::status(&frame["data"]);
                    Some(DriverEvent::Status(status.clone()))
                }
                Some("response") if frame["id"] == "history" => {
                    if let Some(messages) = frame["data"]["messages"].as_array() {
                        for message in messages {
                            if let Some(event) = history_event(message) {
                                let _ = tx.send(event);
                            }
                        }
                    }
                    None
                }
                Some("message_end") if frame["message"]["role"] == "user" => {
                    history_event(&frame["message"])
                }
                Some("agent_start" | "agent_settled") => {
                    status.streaming = frame["type"] == "agent_start";
                    if let Some(error) = frame["error"].as_str() {
                        let _ = tx.send(DriverEvent::Notice(error.to_owned()));
                    }
                    Some(DriverEvent::Status(status.clone()))
                }
                _ => serde_json::from_value::<RemoteEvent>(frame)
                    .ok()
                    .and_then(|event| map_remote_event(&event))
                    .map(DriverEvent::Ui),
            };
            if event.is_some_and(|event| tx.send(event).is_err()) {
                break;
            }
        }
        let _ = tx.send(DriverEvent::Ended);
    });
    let driver = Driver {
        write: tokio::sync::Mutex::new(write),
        events: Mutex::new(Some(rx)),
    };
    let result = crate::tui_shell::run_driver_tui_with_echo(
        &driver,
        &format!("{name} · {session_id}"),
        false,
    )
    .await;
    reader.abort();
    let _ = reader.await;
    result
}

fn history_event(message: &Value) -> Option<DriverEvent> {
    use crate::remote::session::message_role;
    match message_role(message) {
        "user" => Some(DriverEvent::Notice(format!("› {}", message_text(message)))),
        "assistant" => Some(DriverEvent::Ui(UiEvent::AssistantEnd {
            blocks: crate::transcript_view::assistant_blocks_from_value(message),
        })),
        _ => None,
    }
}

fn status(state: &Value) -> DriverStatus {
    DriverStatus {
        ready: true,
        streaming: state["activeOperation"].is_object(),
        model: state["modelId"].as_str().map(str::to_owned),
        thinking: state["thinkingLevel"].as_str().map(str::to_owned),
        active_tools: serde_json::from_value(state["activeTools"].clone()).unwrap_or_default(),
        retry: None,
    }
}

struct Driver {
    write: tokio::sync::Mutex<OwnedWriteHalf>,
    events: Mutex<Option<mpsc::UnboundedReceiver<DriverEvent>>>,
}

#[async_trait::async_trait]
impl SessionDriver for Driver {
    fn events(&self) -> Option<mpsc::UnboundedReceiver<DriverEvent>> {
        self.events.lock().unwrap().take()
    }

    async fn send(&self, command: DriverCommand) -> Result<(), String> {
        let mut frame = match command {
            DriverCommand::Prompt(message) => json!({"type":"prompt","message":message}),
            DriverCommand::Abort => json!({"type":"abort"}),
            DriverCommand::RefreshState => json!({"type":"get_state"}),
            _ => return Err("command is unavailable for this Host area".into()),
        };
        frame["id"] = json!(uuid::Uuid::new_v4().to_string());
        write_value(&mut *self.write.lock().await, &frame)
            .await
            .map_err(|e| e.to_string())
    }
}

async fn write_value(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
    let frame = encode_frame(value, MAX_FRAME_BYTES).map_err(io_error)?;
    tokio::time::timeout(Duration::from_secs(15), write_frame(writer, &frame))
        .await
        .map_err(io::Error::other)?
        .map_err(io_error)
}

async fn bridge(
    mut read: tokio::net::unix::OwnedReadHalf,
    mut write: OwnedWriteHalf,
    ready: Value,
) -> io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut line = serde_json::to_vec(&ready)?;
    line.push(b'\n');
    stdout.write_all(&line).await?;
    stdout.flush().await?;
    // Tokio stdin uses a blocking runtime task. A detached reader thread lets
    // the bridge exit on Host EOF even while the caller's input pipe stays open.
    let (input, mut frames) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("rpi-host-stdin".into())
        .spawn(move || {
            use std::io::{BufRead, Read};
            let mut stdin = std::io::stdin().lock();
            loop {
                let mut frame = Vec::new();
                let result = match (&mut stdin)
                    .take((MAX_FRAME_BYTES + 2) as u64)
                    .read_until(b'\n', &mut frame)
                {
                    Ok(0) => return,
                    Ok(_) if frame.pop() == Some(b'\n') && frame.len() <= MAX_FRAME_BYTES => {
                        Ok(frame)
                    }
                    Ok(_) => Err(io::Error::other("invalid or oversized Host RPC frame")),
                    Err(error) => Err(error),
                };
                let failed = result.is_err();
                if input.blocking_send(result).is_err() || failed {
                    return;
                }
            }
        })?;
    let outbound = async {
        while let Some(frame) = frames.recv().await {
            write_frame(&mut write, &frame?).await.map_err(io_error)?;
        }
        Ok(())
    };
    let inbound = async {
        loop {
            let value: Value = read_frame(&mut read, MAX_FRAME_BYTES)
                .await
                .map_err(io_error)?;
            let mut frame = serde_json::to_vec(&value)?;
            frame.push(b'\n');
            stdout.write_all(&frame).await?;
            stdout.flush().await?;
        }
    };
    tokio::select! { result = outbound => result, result = inbound => result }
}
