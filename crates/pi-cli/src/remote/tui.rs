//! Terminal UI for `rpi --connect`: renders a remote session's transcript and
//! forwards input as protocol commands.
//!
//! The client is intentionally thin — it holds no provider, tools, extensions,
//! or session files. It connects, starts a remote session, subscribes to its
//! event stream, and renders. All agent work happens on the server.
//!
//! It drives its [`SessionDriver`] (here a [`RemoteDriver`]) rather than talking
//! to the socket directly, so the runner is transport-agnostic: the same loop
//! applies to any driver that yields [`DriverEvent`]s and accepts
//! [`DriverCommand`]s. The transcript itself renders through the shared
//! [`TranscriptView`], the same component layer the local TUI uses.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use rpi_tui::{
    bold as tui_bold, Component, Container, Editor, Focusable, FooterComponent, ProcessTerminal,
    ScrollView, StackChild, StackEntry, StatusIndicator, Text, TuiAltScreen, VStack, WorkingState,
    TUI,
};

use crate::app::EXIT_RUNTIME;
use crate::session_driver::{
    DriverCommand, DriverEvent, DriverStatus, RemoteDriver, SessionDriver,
};
use crate::transcript_view::TranscriptView;

/// Connect to a server and run the interactive client until the user exits.
pub async fn run(addr: &str, token: Option<&str>) -> i32 {
    let driver = match RemoteDriver::connect(addr, token).await {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("error: {error}");
            return EXIT_RUNTIME;
        }
    };
    let code = match run_driver_tui(&driver, addr).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_RUNTIME
        }
    };
    driver.shutdown().await;
    code
}

/// Driver-generic TUI runner: any [`SessionDriver`] can be driven by this loop,
/// so the same shell applies to the remote client and (in time) the in-process
/// host.
async fn run_driver_tui<D: SessionDriver>(driver: &D, addr: &str) -> Result<(), String> {
    let mut events = driver
        .events()
        .ok_or("driver did not yield an event stream")?;

    // ---- Build the UI ----
    let chrome = RemoteChrome::new(addr);
    let document = Arc::new(Container::new());
    document.add_child(chrome.header.clone());
    let transcript_container = Arc::new(Container::new());
    document.add_child(transcript_container.clone());
    let scroll = Arc::new(ScrollView::primary(document.clone()));

    // Retained transcript view: the same pi-tui components the local TUI uses,
    // updated in place from the driver's normalized event stream.
    let view = TranscriptView::new(transcript_container.clone());

    let editor = Arc::new(Editor::simple());

    // Layout mirrors the local TUI's bottom dock: transcript (grow) over
    // status slot + editor + footer.
    let root = VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(scroll.clone())
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(chrome.status.clone())),
        StackChild::Entry(StackEntry::new(editor.clone()).shrink(0).min_size(3)),
        StackChild::Entry(StackEntry::new(chrome.footer.clone())),
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

    // Initial paint (the transcript starts empty; chrome shows connection state).
    let mut status = DriverStatus::default();
    chrome.refresh(addr, &status);
    tui.request_render(true);

    let outcome = run_loop(
        driver,
        &mut status,
        &mut events,
        &mut input_rx,
        &mut submit_rx,
        &tui,
        &editor,
        &chrome,
        &view,
        addr,
    )
    .await;

    tui.stop(Default::default());
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<D: SessionDriver>(
    driver: &D,
    status: &mut DriverStatus,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<DriverEvent>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    submit_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    tui: &Arc<TuiAltScreen>,
    editor: &Arc<Editor>,
    chrome: &RemoteChrome,
    view: &TranscriptView,
    addr: &str,
) -> Result<(), String> {
    // Repaint cadence while a run is in flight so the in-editor spinner and the
    // retry countdown animate between streamed tokens (mirrors the local render
    // tick). Idle screens never request a frame.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            maybe_event = events.recv() => {
                let Some(event) = maybe_event else { break };
                match event {
                    DriverEvent::Ui(ui) => view.apply(&ui, tui.width()),
                    DriverEvent::Status(next) => {
                        *status = next;
                        sync_working(editor, status.streaming);
                        chrome.refresh(addr, status);
                    }
                    DriverEvent::Notice(message) => view.add_notice(&message),
                    DriverEvent::Ended => break,
                }
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
                            if status.streaming {
                                view.add_notice("aborting…");
                                let _ = driver.send(DriverCommand::Abort).await;
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
                match handle_submit(driver, status, view, &text).await {
                    Ok(SubmitAction::Continue) => {}
                    Ok(SubmitAction::Quit) => break,
                    Err(error) => view.add_notice(&format!("error: {error}")),
                }
                editor.clear();
                tui.request_render(false);
            }
            _ = ticker.tick() => {
                let retrying = chrome.tick_retry();
                if status.streaming || retrying {
                    tui.request_render(false);
                }
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

async fn handle_submit<D: SessionDriver>(
    driver: &D,
    status: &DriverStatus,
    view: &TranscriptView,
    text: &str,
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
            "abort" => driver.send(DriverCommand::Abort).await?,
            "state" => driver.send(DriverCommand::RefreshState).await?,
            "model" if !arg.is_empty() => {
                driver
                    .send(DriverCommand::SetModel(arg.to_string()))
                    .await?
            }
            "thinking" if !arg.is_empty() => {
                driver
                    .send(DriverCommand::SetThinkingLevel(arg.to_string()))
                    .await?
            }
            "tools" => {
                let tools = if status.active_tools.is_empty() {
                    "(default)".to_string()
                } else {
                    status.active_tools.join(", ")
                };
                view.add_notice(&format!("active tools: {tools}"));
            }
            "help" => view
                .add_notice("commands: /abort /state /model <id> /thinking <level> /tools /exit"),
            "" => view.add_notice("empty command (try /help)"),
            other => view.add_notice(&format!(
                "unsupported command: /{other} (session-tree and reload commands are not available in remote mode; try /help)"
            )),
        }
        return Ok(SubmitAction::Continue);
    }

    view.add_user(trimmed);
    driver
        .send(DriverCommand::Prompt(trimmed.to_string()))
        .await?;
    Ok(SubmitAction::Continue)
}

fn is_ctrl(key: &KeyEvent, ch: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(ch)
}

/// The non-transcript chrome (top banner + status slot + footer), built from
/// the same `pi-tui` components the local TUI uses ([`StatusIndicator`],
/// [`FooterComponent`]) so the two screens read the same.
struct RemoteChrome {
    header: Arc<Text>,
    status: Arc<Container>,
    footer: Arc<FooterComponent>,
    /// The live retry indicator, kept across refreshes so its countdown ticks.
    retry: Mutex<Option<(u32, u32, Arc<StatusIndicator>)>>,
}

impl RemoteChrome {
    fn new(addr: &str) -> Self {
        let footer = Arc::new(FooterComponent::new());
        footer.set_hints(
            "Enter: Send | Shift+Enter: New line | Ctrl+C: Abort/Exit | Esc: Abort | /help",
        );
        Self {
            header: Arc::new(Text::new(
                format!("rpi remote · {addr} · connecting…"),
                1,
                0,
            )),
            status: Arc::new(Container::new()),
            footer,
            retry: Mutex::new(None),
        }
    }

    /// Refresh header + footer + retry indicator from the driver status.
    fn refresh(&self, addr: &str, status: &DriverStatus) {
        let model = status.model.as_deref().unwrap_or("");
        let thinking = status.thinking.as_deref().unwrap_or("off");
        let connection = if status.ready { "ready" } else { "connecting" };
        self.header.set_text(tui_bold(&format!(
            "rpi remote · {addr} · {connection} · model {} · thinking {thinking}",
            if model.is_empty() { "(default)" } else { model }
        )));
        self.footer.set_model(&short_model_name(model));
        self.footer.set_thinking_level(Some(thinking));
        self.footer.set_status("");

        let mut slot = self.retry.lock().unwrap();
        match status.retry {
            Some((attempt, max, delay_ms)) => {
                let unchanged =
                    matches!(slot.as_ref(), Some((a, m, _)) if *a == attempt && *m == max);
                if !unchanged {
                    let indicator = Arc::new(StatusIndicator::retry(
                        attempt,
                        max,
                        Duration::from_millis(delay_ms),
                    ));
                    self.status.clear();
                    self.status.add_child(indicator.clone());
                    *slot = Some((attempt, max, indicator));
                }
            }
            None => {
                if slot.take().is_some() {
                    self.status.clear();
                }
            }
        }
    }

    /// Tick the retry countdown; returns whether a retry is currently shown.
    fn tick_retry(&self) -> bool {
        match self.retry.lock().unwrap().as_ref() {
            Some((_, _, indicator)) => {
                indicator.tick_countdown();
                true
            }
            None => false,
        }
    }
}

/// Keep the in-editor working indicator in step with the run state — the local
/// TUI shows the spinner in the editor's top border, not the status slot.
fn sync_working(editor: &Arc<Editor>, streaming: bool) {
    if streaming {
        if editor.working().is_none() {
            editor.set_working(Some(WorkingState {
                frame: 0,
                started_at: Instant::now(),
                message: "Working…".to_string(),
            }));
        }
    } else if editor.working().is_some() {
        editor.set_working(None);
    }
}

/// The trailing path segment of a model id (`provider/model` → `model`).
fn short_model_name(id: &str) -> String {
    id.rsplit([':', '/'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(id)
        .to_string()
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

    #[test]
    fn short_model_name_keeps_the_trailing_segment() {
        assert_eq!(short_model_name("openai/gpt-5.2"), "gpt-5.2");
        assert_eq!(short_model_name("anthropic:claude-4"), "claude-4");
        assert_eq!(short_model_name("bare"), "bare");
        assert_eq!(short_model_name(""), "");
    }
}
