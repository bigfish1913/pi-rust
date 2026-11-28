//! Terminal and stdio adapters for the same authenticated area session.

use super::{
    area::Connection,
    area_rpc::{io_error, SessionHello, RESPONSE_TOO_LARGE},
    client::{encode_frame, read_frame, write_frame},
    protocol::{Hello, Version, MAX_FRAME_BYTES, PROTOCOL},
};
use crate::{
    remote::{protocol::RemoteEvent, session::message_text},
    session_driver::{map_remote_event, DriverCommand, DriverEvent, DriverStatus, SessionDriver},
    transcript_view::UiEvent,
};
use serde_json::{json, Value};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixStream},
    sync::mpsc,
};

const HISTORY_LIMIT: usize = 100;

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
    let mut history_limit = HISTORY_LIMIT;
    let mut history_before = None::<String>;
    request_history(&mut write, history_limit, None)
        .await
        .map_err(|e| e.to_string())?;
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let history_writer = Arc::clone(&write);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut status = status(&ready["state"]);
    let _ = tx.send(DriverEvent::Status(status.clone()));
    let reader = tokio::spawn(async move {
        while let Ok(frame) = read_frame::<_, Value>(&mut read, MAX_FRAME_BYTES).await {
            let event = match frame["type"].as_str() {
                Some("response")
                    if frame["id"] == "history"
                        && frame["success"] == false
                        && frame["error"] == RESPONSE_TOO_LARGE =>
                {
                    if let Some(before) = frame["beforeId"]
                        .as_str()
                        .filter(|before| Some(*before) != history_before.as_deref())
                    {
                        if history_before.is_none() {
                            let _ = tx.send(DriverEvent::Notice(
                                "Skipped oversized saved messages while loading Host history."
                                    .into(),
                            ));
                        }
                        history_before = Some(before.to_owned());
                        history_limit = HISTORY_LIMIT;
                    } else if history_limit > 1 {
                        history_limit = history_limit.div_ceil(2);
                    } else {
                        let _ = tx.send(DriverEvent::Notice(RESPONSE_TOO_LARGE.into()));
                        continue;
                    }
                    let mut writer = history_writer.lock().await;
                    if let Err(error) =
                        request_history(&mut *writer, history_limit, history_before.as_deref())
                            .await
                    {
                        let _ = tx.send(DriverEvent::Notice(error.to_string()));
                        break;
                    }
                    None
                }
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
                Some("thinking_level_changed") => {
                    status.thinking = frame["thinkingLevel"].as_str().map(str::to_owned);
                    Some(DriverEvent::Status(status.clone()))
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
        write,
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

async fn request_history(
    writer: &mut OwnedWriteHalf,
    limit: usize,
    before: Option<&str>,
) -> io::Result<()> {
    write_value(
        writer,
        &json!({"type":"get_messages","id":"history","limit":limit,"beforeId":before}),
    )
    .await
}

fn history_event(message: &Value) -> Option<DriverEvent> {
    use crate::remote::session::message_role;
    match message_role(message) {
        "user" => Some(DriverEvent::Notice(format!("› {}", message_text(message)))),
        "assistant" => Some(DriverEvent::Ui(UiEvent::SavedAssistant {
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
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
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
            DriverCommand::SetThinkingLevel(level) => {
                json!({"type":"set_thinking_level", "level":level})
            }
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
    read: tokio::net::unix::OwnedReadHalf,
    write: OwnedWriteHalf,
    ready: Value,
) -> io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut line = serde_json::to_vec(&ready)?;
    line.push(b'\n');
    stdout.write_all(&line).await?;
    stdout.flush().await?;
    // Tokio stdin uses a blocking runtime task. A detached reader thread lets
    // the bridge exit on Host EOF even while the caller's input pipe stays open.
    let (input, frames) = mpsc::channel(1);
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
    relay(read, write, frames, &mut stdout).await
}

async fn relay(
    read: tokio::net::unix::OwnedReadHalf,
    mut write: OwnedWriteHalf,
    mut frames: mpsc::Receiver<io::Result<Vec<u8>>>,
    stdout: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    let pending = AtomicUsize::new(0);
    let outbound = async {
        while let Some(frame) = frames.recv().await {
            // The peer can reply before our frame write future returns.
            pending.fetch_add(1, Ordering::Relaxed);
            write_frame(&mut write, &frame?).await.map_err(io_error)?;
        }
        // The Host finishes its serial command queue before observing EOF.
        write.shutdown().await
    };
    let mut read = BufReader::new(read);
    let inbound = async {
        while !read.fill_buf().await?.is_empty() {
            let value: Value = read_frame(&mut read, MAX_FRAME_BYTES)
                .await
                .map_err(io_error)?;
            if value["type"] == "response" && pending.load(Ordering::Relaxed) > 0 {
                pending.fetch_sub(1, Ordering::Relaxed);
            }
            let mut frame = serde_json::to_vec(&value)?;
            frame.push(b'\n');
            stdout.write_all(&frame).await?;
            stdout.flush().await?;
        }
        if pending.load(Ordering::Relaxed) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Host session closed before all RPC replies arrived",
            ));
        }
        Ok(())
    };
    tokio::pin!(inbound);
    tokio::select! {
        biased;
        result = outbound => {
            result?;
            inbound.await
        }
        result = &mut inbound => {
            result?;
            Err(io_error(super::client::Failure::Connection))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn stdin_eof_drains_host_responses() {
        let (client, server) = UnixStream::pair().unwrap();
        let (read, write) = client.into_split();
        let (send, frames) = mpsc::channel(2);
        let requests = [
            json!({"type": "get_commands", "id": "first"}),
            json!({"type": "get_state", "id": "second"}),
        ];
        for request in &requests {
            send.send(Ok(serde_json::to_vec(request).unwrap()))
                .await
                .unwrap();
        }
        drop(send);
        let (mut output, mut stdout) = tokio::io::duplex(512);
        let serve = async {
            let mut server = BufReader::new(server);
            for request in &requests {
                let frame: Value = read_frame(&mut server, MAX_FRAME_BYTES).await.unwrap();
                assert_eq!(&frame, request);
            }
            assert!(server.fill_buf().await.unwrap().is_empty());
            for request in &requests {
                write_value(
                    server.get_mut(),
                    &json!({"type": "response", "id": request["id"], "success": true}),
                )
                .await
                .unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let (result, ()) = tokio::join!(relay(read, write, frames, &mut stdout), serve);
            result.unwrap();
        })
        .await
        .unwrap();
        drop(stdout);
        let mut text = String::new();
        output.read_to_string(&mut text).await.unwrap();
        let replies: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            replies.iter().map(|reply| &reply["id"]).collect::<Vec<_>>(),
            requests
                .iter()
                .map(|request| &request["id"])
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn host_eof_with_a_pending_reply_is_an_error_after_stdin_eof() {
        let (client, server) = UnixStream::pair().unwrap();
        let (read, write) = client.into_split();
        let (send, frames) = mpsc::channel(1);
        let request = json!({"type": "get_commands", "id": "pending"});
        send.send(Ok(serde_json::to_vec(&request).unwrap()))
            .await
            .unwrap();
        drop(send);
        let serve = async {
            let mut server = BufReader::new(server);
            let frame: Value = read_frame(&mut server, MAX_FRAME_BYTES).await.unwrap();
            assert_eq!(frame, request);
            assert!(server.fill_buf().await.unwrap().is_empty());
            write_value(server.get_mut(), &json!({"type": "agent_start"}))
                .await
                .unwrap();
        };
        let mut output = tokio::io::sink();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(relay(read, write, frames, &mut output), serve)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}
