//! Terminal UI for `rpi --connect`: renders a remote session's transcript and
//! forwards input as protocol commands.
//!
//! The client is intentionally thin — it holds no provider, tools, extensions,
//! or session files. It connects, starts a remote session, subscribes to its
//! event stream, and renders. All agent work happens on the server.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use rpi_tui::{
    bold as tui_bold, Component, Container, Editor, Focusable, Markdown, ProcessTerminal,
    ScrollView, StackChild, StackEntry, Text, TuiAltScreen, VStack, TUI,
};
use serde_json::json;

use crate::app::EXIT_RUNTIME;
use crate::remote::client::RemoteClient;
use crate::remote::protocol::RemoteCommand;
use crate::remote::session::{RemoteSession, Transcript, TranscriptItem};

/// Maximum transcript items rendered (older items are dropped from the view).
const MAX_RENDERED_ITEMS: usize = 400;

/// Connect to a server and run the interactive client until the user exits.
pub async fn run(addr: &str, token: Option<&str>) -> i32 {
    match connect_and_run(addr, token).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_RUNTIME
        }
    }
}

async fn connect_and_run(addr: &str, token: Option<&str>) -> Result<(), String> {
    let mut client = RemoteClient::connect(addr).await?;

    // Authenticate before any other request when a token is configured.
    if let Err(error) = client.authenticate(token.unwrap_or("")).await {
        return Err(auth_hint(error));
    }

    // Handshake: create a session and subscribe to its stream.
    let started = client
        .call("start_session", json!({}))
        .await
        .map_err(auth_hint)?;
    let session_id = started
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("server did not return a sessionId")?
        .to_string();

    let subscribed = client
        .call("subscribe", json!({"sessionId": session_id}))
        .await?;
    let subscription_id = subscribed
        .get("subscriptionId")
        .and_then(|v| v.as_u64())
        .ok_or("server did not return a subscriptionId")?;

    let mut events = client.start_event_pump(subscription_id);
    let mut next_id: u64 = 1;

    // Seed the state (model / thinking / tools) for the header.
    client
        .send_command(&session_id, RemoteCommand::GetState.with_id(next_id))
        .await?;
    next_id += 1;

    // ---- Build the UI ----
    let mut session = RemoteSession::new();
    let header = Arc::new(Text::new(
        format!("rpi remote · {addr} · connecting…"),
        1,
        0,
    ));
    let document = Arc::new(Container::new());
    document.add_child(header.clone());
    let transcript_container = Arc::new(Container::new());
    document.add_child(transcript_container.clone());
    let scroll = Arc::new(ScrollView::primary(document.clone()));

    let editor = Arc::new(Editor::simple());
    let footer = Arc::new(Text::new("", 1, 0));
    let status = Arc::new(Text::new("", 1, 0));

    let root = VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(scroll.clone())
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(status.clone()).shrink(0).min_size(1)),
        StackChild::Entry(StackEntry::new(editor.clone()).shrink(0).min_size(1)),
        StackChild::Entry(StackEntry::new(footer.clone()).shrink(0).min_size(1)),
    ]);
    let root: Arc<dyn Component> = Arc::new(root);

    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        false,
        None,
    ));
    tui.set_layout_root(Some(root));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);
    tui.start_render_scheduler();
    tui.start_readerless();

    // ---- Input plumbing ----
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    editor.on_submit(Arc::new(move |text: &str| {
        let _ = submit_tx.send(text.to_string());
    }));

    let input_tui = tui.clone();
    let _reader = tokio::task::spawn_blocking(move || loop {
        match crossterm::event::poll(Duration::from_millis(50)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break,
        }
        match crossterm::event::read() {
            Ok(event) => {
                let _ = input_tui.refresh_size();
                if input_tx.send(event).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    // Initial paint.
    rebuild_transcript(&transcript_container, &session.transcript);
    update_chrome(&header, &status, &footer, addr, &session);
    tui.request_render(true);

    let outcome = run_loop(
        &mut client,
        &session_id,
        &mut session,
        &mut events,
        &mut input_rx,
        &mut submit_rx,
        &tui,
        &editor,
        &scroll,
        &header,
        &status,
        &footer,
        &transcript_container,
        addr,
        &mut next_id,
    )
    .await;

    client.stop_session(&session_id).await;
    tui.stop(Default::default());
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    client: &mut RemoteClient,
    session_id: &str,
    session: &mut RemoteSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    submit_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    tui: &Arc<TuiAltScreen>,
    editor: &Arc<Editor>,
    _scroll: &Arc<ScrollView>,
    header: &Arc<Text>,
    status: &Arc<Text>,
    footer: &Arc<Text>,
    transcript_container: &Arc<Container>,
    addr: &str,
    next_id: &mut u64,
) -> Result<(), String> {
    loop {
        tokio::select! {
            maybe_event = events.recv() => {
                let Some(line) = maybe_event else { break };
                session.apply_line(&line);
                rebuild_transcript(transcript_container, &session.transcript);
                update_chrome(header, status, footer, addr, session);
                tui.request_render(false);
            }
            maybe_input = input_rx.recv() => {
                let Some(event) = maybe_input else { break };
                match event {
                    Event::Resize(_, _) => tui.refresh_size(),
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => tui.scroll_by(-3),
                            MouseEventKind::ScrollDown => tui.scroll_by(3),
                            _ => {}
                        }
                        tui.request_render(false);
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        crate::key_trace::key(&key, "remote-client");
                        if is_ctrl(&key, 'c') {
                            if session.streaming {
                                session.push_notice("aborting…");
                                send(client, session_id, RemoteCommand::Abort, next_id).await?;
                                rebuild_transcript(transcript_container, &session.transcript);
                            } else {
                                break;
                            }
                        } else {
                            editor.handle_key(key);
                        }
                        tui.request_render(false);
                    }
                    other => {
                        crate::key_trace::note(&format!("remote client event {other:?}"));
                    }
                }
            }
            maybe_text = submit_rx.recv() => {
                let Some(text) = maybe_text else { break };
                match handle_submit(client, session_id, session, &text, next_id).await {
                    Ok(SubmitAction::Continue) => {}
                    Ok(SubmitAction::Quit) => break,
                    Err(error) => {
                        session.push_notice(format!("error: {error}"));
                    }
                }
                editor.clear();
                rebuild_transcript(transcript_container, &session.transcript);
                update_chrome(header, status, footer, addr, session);
                tui.request_render(false);
            }
            else => break,
        }
    }
    Ok(())
}

enum SubmitAction {
    Continue,
    Quit,
}

async fn send(
    client: &mut RemoteClient,
    session_id: &str,
    command: RemoteCommand,
    next_id: &mut u64,
) -> Result<(), String> {
    let id = *next_id;
    *next_id += 1;
    client.send_command(session_id, command.with_id(id)).await
}

async fn handle_submit(
    client: &mut RemoteClient,
    session_id: &str,
    session: &mut RemoteSession,
    text: &str,
    next_id: &mut u64,
) -> Result<SubmitAction, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(SubmitAction::Continue);
    }

    if let Some(rest) = trimmed.strip_prefix('/') {
        let (name, arg) = match rest.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (rest, ""),
        };
        match name {
            "exit" | "quit" | "q" => return Ok(SubmitAction::Quit),
            "abort" => send(client, session_id, RemoteCommand::Abort, next_id).await?,
            "state" => send(client, session_id, RemoteCommand::GetState, next_id).await?,
            "model" if !arg.is_empty() => {
                send(
                    client,
                    session_id,
                    RemoteCommand::SetModel {
                        model: arg.to_string(),
                    },
                    next_id,
                )
                .await?
            }
            "thinking" if !arg.is_empty() => {
                send(
                    client,
                    session_id,
                    RemoteCommand::SetThinkingLevel {
                        level: arg.to_string(),
                    },
                    next_id,
                )
                .await?
            }
            "tools" => {
                let tools = if session.state.active_tools.is_empty() {
                    "(default)".to_string()
                } else {
                    session.state.active_tools.join(", ")
                };
                session.push_notice(format!("active tools: {tools}"));
            }
            "help" => session.push_notice(
                "commands: /abort /state /model <id> /thinking <level> /tools /exit",
            ),
            "" => session.push_notice("empty command (try /help)"),
            other => session.push_notice(format!(
                "unsupported command: /{other} (session-tree and reload commands are not available in remote mode; try /help)"
            )),
        }
        return Ok(SubmitAction::Continue);
    }

    session.on_user_prompt(trimmed);
    send(
        client,
        session_id,
        RemoteCommand::Prompt {
            content: trimmed.to_string(),
        },
        next_id,
    )
    .await?;
    Ok(SubmitAction::Continue)
}

fn is_ctrl(key: &KeyEvent, ch: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(ch)
}

/// Append a `--token` hint when the server rejected us for authentication.
fn auth_hint(error: String) -> String {
    let lower = error.to_lowercase();
    if lower.contains("authentication") || lower.contains("token") {
        format!("{error} (retrieve the token printed by `rpi --server` and pass `--token <token>`)")
    } else {
        error
    }
}

/// Rebuild the transcript view. Cheap enough for a first cut: the transcript is
/// bounded and re-rendering on each event keeps the model trivial.
fn rebuild_transcript(container: &Arc<Container>, transcript: &Transcript) {
    container.clear();
    let start = transcript.items.len().saturating_sub(MAX_RENDERED_ITEMS);
    for item in &transcript.items[start..] {
        container.add_child(render_item(item));
    }
}

fn render_item(item: &TranscriptItem) -> Arc<dyn Component> {
    match item {
        TranscriptItem::User(text) => {
            Arc::new(Text::new(format!("› {text}"), 1, 0)) as Arc<dyn Component>
        }
        TranscriptItem::Assistant(text) => {
            Arc::new(Markdown::new(text.clone(), 1, 0)) as Arc<dyn Component>
        }
        TranscriptItem::Thinking(text) => {
            Arc::new(Text::new(format!("[thinking] {text}"), 1, 0)) as Arc<dyn Component>
        }
        TranscriptItem::Tool(tool) => {
            let status = if tool.done {
                if tool.is_error {
                    "error"
                } else {
                    "ok"
                }
            } else {
                "running"
            };
            let body = tool
                .result
                .as_ref()
                .or(tool.partial.as_ref())
                .map(String::as_str)
                .unwrap_or("");
            let text = if body.is_empty() {
                format!("[tool {} · {}]", tool.name, status)
            } else {
                format!("[tool {} · {}]\n{body}", tool.name, status)
            };
            Arc::new(Text::new(text, 1, 0)) as Arc<dyn Component>
        }
        TranscriptItem::Notice(text) => {
            Arc::new(Text::new(format!("· {text}"), 1, 0)) as Arc<dyn Component>
        }
    }
}

fn update_chrome(
    header: &Arc<Text>,
    status: &Arc<Text>,
    footer: &Arc<Text>,
    addr: &str,
    session: &RemoteSession,
) {
    let model = session.state.model.as_deref().unwrap_or("(default)");
    let thinking = session
        .state
        .thinking_level
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("off");
    let connection = if session.ready { "ready" } else { "connecting" };
    header.set_text(format!(
        "{}",
        tui_bold(&format!(
            "rpi remote · {addr} · {connection} · model {model} · thinking {thinking}"
        ))
    ));

    let status_text = if let Some((attempt, max)) = session.retry_attempt {
        format!("retrying {attempt}/{max}…")
    } else if session.streaming {
        "working…".to_string()
    } else {
        String::new()
    };
    status.set_text(status_text);

    footer.set_text(
        "Enter: send · Ctrl+C: abort/exit · Esc: /exit · /help · (session tree & reload unavailable remotely)",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_detection_is_specific() {
        assert!(is_ctrl(
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            'c'
        ));
        assert!(!is_ctrl(
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            'c'
        ));
    }
}
