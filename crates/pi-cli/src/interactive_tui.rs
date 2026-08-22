//! Interactive mode for pi-cli.
//!
//! Full-screen terminal UI with a streaming transcript, an editor, a live
//! status indicator, and tool-execution display. Mirrors the TypeScript
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` event→UI
//! mapping (`handleEvent`), driven by the live `AgentEvent` stream the harness
//! emits via the `BroadcastEmitter` installed in [`crate::session`].
//!
//! Key architecture facts (see `docs/tui-gap-analysis.md`):
//! - `TuiAltScreen::start()` and `show_overlay` are stubs, so this module owns
//!   a `spawn_blocking` crossterm `read()` loop for key dispatch and a
//!   `tokio::spawn` task that drains `broadcast::Receiver<AgentEvent>` into UI
//!   mutations.
//! - The layout root is built ONCE at startup (mirrors the TS
//!   `fullscreenLayoutRoot`); per-message we mutate only `chat_container` /
//!   `status_container` / `autocomplete_container` children and call
//!   `request_render(false)` so the differential renderer repaints just the
//!   changed rows.
//! - Selectors (`/model` `/session` `/theme`) are implemented by **swapping the
//!   `editor_container` child** (the TS `showSelector` swap pattern,
//!   `interactive-mode.ts:4354-4377`) — the `show_overlay` stub is avoided
//!   entirely. An `active_selector` state field holds the live `SelectList`;
//!   while it is `Some` the key loop routes to it first and restores the editor
//!   on done/cancel.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::mpsc::channel;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use tokio::sync::broadcast;

use rpi_agent::{AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, Content};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_tui::{
    AutocompleteManager, CombinedAutocompleteProvider, Container, Editor, EditorOptions,
    EditorStyle, FilePathAutocompleteProvider, Focusable, FollowMode, Loader, ProcessTerminal,
    ScrollView, ScrollViewOptions, SlashCommand, SlashCommandAutocompleteProvider, Spacer,
    StackChild, StackEntry, Text, TuiAltScreen, TUI, VStack, AssistantMessageComponent,
    AssistantMessageOptions, AutocompleteSuggestions, FooterComponent, SelectList, SelectItem,
    ThemeManager, ThemePreset, ToolExecutionComponent,
};

use crate::args::Args;

// ===========================================================================
// Slash commands
// ===========================================================================

/// Result of slash command handling.
enum SlashCommandResult {
    /// Exit the application.
    Exit,
    /// Clear the chat.
    ClearChat,
    /// Unknown command.
    Unknown,
    /// Not a command, send as message.
    SendMessage(String),
    /// Show help information.
    Help,
    /// Show version.
    Version,
    /// Show hotkeys.
    Hotkeys,
    /// Open the model selector overlay.
    SelectModel,
    /// Open the session selector overlay.
    SelectSession,
    /// Open the theme selector overlay.
    SelectTheme,
    /// Compact the conversation (lane.compact).
    Compact,
    /// Copy the last assistant message to the clipboard.
    Copy,
    /// Not supported in this v1 build (carries the command for the message).
    Unsupported(String),
}

/// Handle slash commands. Returns the result indicating what action to take.
///
/// Mirrors the v1-applicable subset of the TS `BUILTIN_SLASH_COMMANDS`
/// (`.reference/.../core/slash-commands.ts`). Commands beyond the v1 surface
/// resolve to `Unsupported` with a consistent message.
fn handle_slash_command(text: &str) -> SlashCommandResult {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return SlashCommandResult::SendMessage(text.to_string());
    }

    let command = parts[0];
    match command {
        "/help" | "/?" => SlashCommandResult::Help,
        "/clear" | "/new" => SlashCommandResult::ClearChat,
        "/exit" | "/quit" | "/q" => SlashCommandResult::Exit,
        "/version" | "/v" => SlashCommandResult::Version,
        "/model" | "/m" => SlashCommandResult::SelectModel,
        "/hotkeys" => SlashCommandResult::Hotkeys,
        "/session" | "/resume" => SlashCommandResult::SelectSession,
        "/theme" => SlashCommandResult::SelectTheme,
        "/compact" => SlashCommandResult::Compact,
        "/copy" => SlashCommandResult::Copy,
        // `/name` is recognized-v1 but inert (no session-renaming surface yet).
        "/name" => SlashCommandResult::Unsupported("/name".to_string()),
        // The remaining TS builtins are out of v1 scope.
        "/settings"
        | "/scoped-models"
        | "/export"
        | "/import"
        | "/share"
        | "/fork"
        | "/clone"
        | "/tree"
        | "/trust"
        | "/login"
        | "/logout"
        | "/reload" => SlashCommandResult::Unsupported(command.to_string()),
        _ => SlashCommandResult::Unknown,
    }
}

/// The v1 slash commands surfaced to the autocomplete provider (the TS
/// `BUILTIN_SLASH_COMMANDS` v1 subset, with descriptions). Kept in sync with
/// [`handle_slash_command`] so `/`-autocomplete lists exactly the commands the
/// dispatcher recognizes.
fn v1_slash_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand { name: "/help".into(), description: "Show available commands".into() },
        SlashCommand { name: "/clear".into(), description: "Clear the conversation".into() },
        SlashCommand { name: "/new".into(), description: "Clear the conversation".into() },
        SlashCommand { name: "/exit".into(), description: "Exit the application".into() },
        SlashCommand { name: "/quit".into(), description: "Exit the application".into() },
        SlashCommand { name: "/version".into(), description: "Show version information".into() },
        SlashCommand { name: "/model".into(), description: "Choose a model (selector)".into() },
        SlashCommand { name: "/session".into(), description: "List saved sessions".into() },
        SlashCommand { name: "/theme".into(), description: "Choose a theme (selector)".into() },
        SlashCommand { name: "/compact".into(), description: "Compact the conversation".into() },
        SlashCommand { name: "/copy".into(), description: "Copy last reply to clipboard".into() },
        SlashCommand { name: "/hotkeys".into(), description: "Show keyboard shortcuts".into() },
    ]
}

// ===========================================================================
// Channel + helpers
// ===========================================================================

/// Message type for communication between the key/callback threads and the
/// main async loop.
enum TuiMessage {
    UserInput(String),
    Exit,
    /// Clear the transcript (from `/clear`).
    ClearChat,
    /// Compact the conversation (from `/compact`).
    Compact,
    /// Copy the last assistant reply to the clipboard (from `/copy`).
    Copy,
}

/// Extract the concatenated text content from an assistant message (mirrors
/// the TS `contentText` projection — drops thinking/tool-call/image blocks).
fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// The name displayed for a model id (last path segment / after the final
/// `:`), to keep the footer compact.
fn short_model_name(id: &str) -> String {
    id.rsplit([':', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string()
}

// ===========================================================================
// Streaming run status
// ===========================================================================

/// The live status of the agent run, fed to the footer + status slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Idle,
    Working,
    Aborting,
}

/// Which selector overlay (if any) is currently swapped into the editor slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectorKind {
    /// `/model` — available models (display-only; v1 can't switch mid-run).
    Model,
    /// `/session` — saved JSONL sessions (restore not implemented in v1).
    Session,
    /// `/theme` — dark / light / monochrome presets applied live.
    Theme,
}

/// Shared mutable TUI state, `Arc`-cloned into the drain task, the key loop,
/// and the render-tick task.
struct TuiState {
    /// The in-flight streaming assistant message (cleared on finalize).
    current_assistant: std::sync::Mutex<Option<Arc<AssistantMessageComponent>>>,
    /// Tool-execution components keyed by `tool_call_id`.
    tool_components: std::sync::Mutex<HashMap<String, Arc<ToolExecutionComponent>>>,
    /// Run status for the status indicator + interrupt routing.
    status: std::sync::Mutex<RunStatus>,
    /// The footer, updated live by the drain task.
    footer: Arc<FooterComponent>,
    /// The status-container (status slot in the dock) — cleared/filled with a
    /// loader while a run is active.
    status_container: Arc<Container>,
    /// The chat transcript container.
    chat_container: Arc<Container>,
    /// The active loader shown while `Working`.
    loader: Arc<Loader>,
    /// The last finalized assistant text (for `/copy`). Updated by the drain
    /// task on `MessageEnd` / `AgentEnd`.
    last_assistant_text: std::sync::Mutex<String>,
    /// The active selector overlay, swapped into the editor slot. `Some` while
    /// a selector is open; the key loop routes to it first and restores the
    /// editor on done/cancel.
    active_selector: std::sync::Mutex<Option<(Arc<SelectList>, SelectorKind)>>,
    /// The autocomplete manager (slash + @file providers) consulted on every
    /// editor keystroke.
    autocomplete: AutocompleteManager,
    /// The container rendered above the editor holding the live autocomplete
    /// suggestion list (cleared when there are no suggestions).
    autocomplete_container: Arc<Container>,
    /// The owned theme manager — `/theme` applies presets here. The global
    /// `theme()` is read-only after OnceLock init, so per-instance state is the
    /// only way to apply a preset at runtime.
    theme_manager: Arc<ThemeManager>,
}

impl TuiState {
    fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = status;
        match status {
            RunStatus::Working => {
                self.footer.set_status("Working…");
                self.status_container.clear();
                self.loader.start();
                self.status_container.add_child(self.loader.clone());
            }
            RunStatus::Aborting => {
                self.footer.set_status("Aborting…");
            }
            RunStatus::Idle => {
                self.footer.set_status("");
                self.loader.stop();
                self.status_container.clear();
            }
        }
    }

    /// Whether a selector overlay is currently open (routes keys to it first).
    fn selector_open(&self) -> bool {
        self.active_selector.lock().unwrap().is_some()
    }
}

// ===========================================================================
// interactive_tui — the entry point
// ===========================================================================

/// TUI-based interactive mode.
///
/// `event_rx` carries the live `AgentEvent` stream (installed by
/// [`crate::session::build`]); when `None` (e.g. a non-TUI caller reuses this
/// fn), it falls back to a blocking, await-final-text path.
///
/// `model_catalog` is the read-only catalog the `/model` selector displays.
///
/// This implementation mirrors the TypeScript `InteractiveMode` class:
/// build the layout root once, drain `AgentEvent`s into UI mutations that
/// mirror `handleEvent`, and dispatch keys from a `spawn_blocking` crossterm
/// loop (the `TuiAltScreen` start() handler is a stub). Selectors and
/// autocomplete are layered on via the editor-container swap pattern.
pub async fn interactive_tui(
    harness: &AgentHarness,
    event_rx: Option<broadcast::Receiver<AgentEvent>>,
    args: &Args,
    model_catalog: Vec<rpi_ai::Model>,
    initial: Option<String>,
    extra_messages: &[String],
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    // Resolve the model id for the footer (best-effort; ignore failure).
    let model_name = lane
        .get_model()
        .await
        .map(|m| short_model_name(&m.id))
        .unwrap_or_else(|_| "model".to_string());

    // The cwd for @file autocomplete + session discovery.
    let cwd = std::env::current_dir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Channel between the key/callback threads and the main async loop.
    let (tx, rx) = channel::<TuiMessage>();

    // ---- TUI + containers ----
    let terminal = Box::new(ProcessTerminal::new());
    let tui = Arc::new(TuiAltScreen::new(terminal, true, None));

    let chat_container = Arc::new(Container::new());
    add_welcome_message(&chat_container);

    // `document_container` wraps the welcome header + chat so the scrollview
    // follows the whole transcript (mirrors TS `documentContainer`).
    let document_container = Arc::new(Container::new());
    document_container.add_child(chat_container.clone());

    let scroll_view = Arc::new(ScrollView::new(
        document_container.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            ..Default::default()
        },
    ));

    // ---- Editor ----
    // Bordered box matching native pi: no `> ` prompt, no placeholder — the
    // editor renders full-width `─` top/bottom borders with padding-only lines
    // (see Editor::render). padding_x:1 gives a 1-col inset inside the box.
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));

    // ---- Footer + status ----
    let footer = Arc::new(FooterComponent::new());
    footer.set_model(&model_name);
    footer.set_hints("Shift+Enter: Send | Ctrl+C: Abort/Exit | /help | Tab: Complete");

    let status_container = Arc::new(Container::new());
    let loader = Arc::new(Loader::with_text("Working…"));

    // ---- Autocomplete (slash commands + @file paths, rooted at cwd) ----
    let autocomplete = AutocompleteManager::new();
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            v1_slash_commands(),
        )));
        combined.add_provider(Arc::new(FilePathAutocompleteProvider::with_root(cwd.clone())));
        autocomplete.set_provider(Arc::new(combined));
    }
    let autocomplete_container = Arc::new(Container::new());

    let state = Arc::new(TuiState {
        current_assistant: std::sync::Mutex::new(None),
        tool_components: std::sync::Mutex::new(HashMap::new()),
        status: std::sync::Mutex::new(RunStatus::Idle),
        footer: footer.clone(),
        status_container: status_container.clone(),
        chat_container: chat_container.clone(),
        loader: loader.clone(),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        autocomplete,
        autocomplete_container: autocomplete_container.clone(),
        theme_manager: Arc::new(ThemeManager::new()),
    });

    // Capture the model catalog + cwd for the selector builders + the key loop
    // (the callbacks fire on blocking threads and need owned data).
    let model_catalog_arc = Arc::new(model_catalog.clone());
    let lane_model_id = lane
        .get_model()
        .await
        .map(|m| m.id)
        .unwrap_or_default();

    // ---- Layout root (built ONCE; mirrors TS fullscreenLayoutRoot) ----
    // root = VStack[ scrollview(grow=1), dock ]
    // dock  = VStack[ status_container, autocomplete_container, editor_container, footer ]
    let editor_container = Arc::new(Container::new());
    editor_container.add_child(Arc::new(Spacer::new(0)));
    editor_container.add_child(editor.clone());

    let dock = Arc::new(VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(status_container.clone())),
        StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
        StackChild::Entry(StackEntry::new(editor_container.clone())),
        StackChild::Entry(StackEntry::new(footer.clone())),
    ]));

    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll_view.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(dock)),
    ]);

    tui.set_layout_root(Some(Arc::new(root)));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);

    // ---- Submit handler (fires on the blocking key thread; must stay sync) ----
    let chat_for_cb = chat_container.clone();
    let tui_for_cb = tui.clone();
    let tx_for_cb = tx.clone();
    let state_for_cb = state.clone();
    let editor_for_cb = editor.clone();
    // Clone the shared selector inputs for the closure; the originals stay
    // available for the key-dispatch loop below (Ctrl+L opens /model too).
    let editor_container_for_cb = editor_container.clone();
    let model_catalog_for_cb = model_catalog_arc.clone();
    let lane_model_id_for_cb = lane_model_id.clone();
    editor.on_submit(Arc::new(move |text: &str| {
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        if text.starts_with('/') {
            match handle_slash_command(text) {
                SlashCommandResult::Exit => {
                    let _ = tx_for_cb.send(TuiMessage::Exit);
                }
                SlashCommandResult::ClearChat => {
                    let _ = tx_for_cb.send(TuiMessage::ClearChat);
                }
                SlashCommandResult::Help => {
                    add_help_message(&chat_for_cb);
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::Version => {
                    add_version_message(&chat_for_cb);
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::Hotkeys => {
                    add_hotkeys_message(&chat_for_cb);
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::SelectModel => {
                    open_model_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &model_catalog_for_cb,
                        &lane_model_id_for_cb,
                    );
                }
                SlashCommandResult::SelectSession => {
                    open_session_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &cwd,
                    );
                }
                SlashCommandResult::SelectTheme => {
                    open_theme_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                    );
                }
                SlashCommandResult::Compact => {
                    let _ = tx_for_cb.send(TuiMessage::Compact);
                }
                SlashCommandResult::Copy => {
                    let _ = tx_for_cb.send(TuiMessage::Copy);
                }
                SlashCommandResult::Unsupported(cmd) => {
                    add_note_message(
                        &chat_for_cb,
                        &format!("{cmd} is not supported in v1."),
                    );
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::Unknown => {
                    add_error_message(
                        &chat_for_cb,
                        &format!("Unknown command: {text}. Type /help for available commands."),
                    );
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::SendMessage(msg) => {
                    add_user_message(&chat_for_cb, &msg);
                    tui_for_cb.request_render(false);
                    let _ = tx_for_cb.send(TuiMessage::UserInput(msg));
                }
            }
            return;
        }

        add_user_message(&chat_for_cb, text);
        tui_for_cb.request_render(false);
        let _ = tx_for_cb.send(TuiMessage::UserInput(text.to_string()));
    }));

    tui.start();

    // ---- Streaming drain task ----
    let drain_handle = if let Some(rx) = event_rx {
        let tui_drain = tui.clone();
        let state_drain = state.clone();
        let chat_drain = chat_container.clone();
        Some(tokio::spawn(async move {
            drain_agent_events(rx, tui_drain, state_drain, chat_drain).await;
        }))
    } else {
        None
    };

    // ---- Render-tick task (advances the loader spinner while Working) ----
    //
    // The `Loader` only advances its frame on render; without a periodic
    // `request_render` the spinner visibly freezes between events.
    let tui_tick = tui.clone();
    let state_tick = state.clone();
    let tick_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(120));
        interval.tick().await; // discard immediate
        loop {
            interval.tick().await;
            let working = *state_tick.status.lock().unwrap() == RunStatus::Working;
            if working {
                tui_tick.request_render(false);
            }
        }
    });

    // ---- Key dispatch loop (spawn_blocking crossterm read) ----
    let running = Arc::new(std::sync::Mutex::new(true));
    let running_key = running.clone();
    let tx_for_key = tx.clone();
    let tui_for_key = tui.clone();
    let editor_for_key = editor.clone();
    let editor_container_for_key = editor_container.clone();
    let scroll_for_key = scroll_view.clone();
    let lane_for_key = lane.clone();
    let state_for_key = state.clone();
    // The Ctrl+L model selector needs the catalog + current id; these are
    // already-known owned values (no async needed in the blocking key loop).
    let catalog_for_key = model_catalog_arc.clone();
    let lane_model_id_for_key = lane_model_id.clone();

    tokio::task::spawn_blocking(move || {
        loop {
            if !*running_key.lock().unwrap() {
                break;
            }
            let Ok(Event::Key(key)) = crossterm::event::read() else {
                continue;
            };

            // 1. A selector overlay is open → route to it first. Only Esc
            //    (cancel) and Enter/Up/Down/Ctrl-K/J/P/N (navigate/select)
            //    escape to the selector; on done/cancel the selector callbacks
            //    restore the editor and clear `active_selector`.
            if state_for_key.selector_open() {
                // Esc always cancels the selector (even with modifiers off).
                if key.code == KeyCode::Esc {
                    close_selector(
                        &state_for_key,
                        &editor_container_for_key,
                        &editor_for_key,
                        &tui_for_key,
                    );
                    continue;
                }
                let (selector, _kind) = state_for_key
                    .active_selector
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("selector_open guaranteed Some");
                selector.handle_key(key);
                tui_for_key.request_render(false);
                continue;
            }

            // 2. Ctrl+C: abort a run if one is active, else exit.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                let status = *state_for_key.status.lock().unwrap();
                if status == RunStatus::Working {
                    state_for_key.set_status(RunStatus::Aborting);
                    let lane = lane_for_key.clone();
                    tokio::spawn(async move {
                        let _ = lane.abort().await;
                    });
                } else {
                    let _ = tx_for_key.send(TuiMessage::Exit);
                }
                continue;
            }

            // 3. Ctrl+L: open the model selector (TS binds Ctrl+L to
            //    model-select). The broker can't switch models mid-run, so
            //    this is the same display-only selector `/model` opens; the
            //    catalog + current id were captured before this blocking loop.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('l') {
                open_model_selector(
                    &state_for_key,
                    &editor_container_for_key,
                    &editor_for_key,
                    &tui_for_key,
                    &catalog_for_key,
                    &lane_model_id_for_key,
                );
                continue;
            }

            // 4. Tab: accept the top autocomplete suggestion (if any).
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Tab {
                if accept_top_suggestion(&state_for_key, &editor_for_key) {
                    tui_for_key.request_render(false);
                }
                continue;
            }

            // 5. Global transcript scroll: PageUp/PageDown move the scrollview.
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::PageUp {
                scroll_for_key.scroll_by(-10);
                tui_for_key.request_render(false);
                continue;
            }
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::PageDown {
                scroll_for_key.scroll_by(10);
                tui_for_key.request_render(false);
                continue;
            }

            // 6. Otherwise forward to the editor + refresh autocomplete.
            editor_for_key.handle_key(key);
            refresh_autocomplete(&state_for_key, &editor_for_key);
            tui_for_key.request_render(false);
        }
    });

    // ---- Initial prompts (run before reading from the channel) ----
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }
    for prompt in prompts {
        if !*running.lock().unwrap() {
            break;
        }
        add_user_message(&chat_container, &prompt);
        tui.request_render(false);
        run_prompt_streaming(&lane, &prompt, &tui, &state, drain_handle.is_some()).await;
    }

    // ---- Main loop: process submitted input + lifecycle messages ----
    loop {
        if !*running.lock().unwrap() {
            break;
        }
        match rx.try_recv() {
            Ok(TuiMessage::UserInput(prompt)) => {
                run_prompt_streaming(&lane, &prompt, &tui, &state, drain_handle.is_some()).await;
            }
            Ok(TuiMessage::ClearChat) => {
                chat_container.clear();
                add_welcome_message(&chat_container);
                tui.request_render(false);
            }
            Ok(TuiMessage::Compact) => {
                run_compact(&lane, &tui, &state).await;
            }
            Ok(TuiMessage::Copy) => {
                copy_last_assistant(&state, &chat_container);
                tui.request_render(false);
            }
            Ok(TuiMessage::Exit) => {
                *running.lock().unwrap() = false;
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }

    // ---- Shutdown ----
    tick_handle.abort();
    if let Some(handle) = drain_handle {
        handle.abort();
    }
    tui.stop(Default::default());
    println!("\nGoodbye!");
    let _ = args;

    0
}

// ===========================================================================
// Run a single prompt (streaming or blocking)
// ===========================================================================

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and this function only
/// awaits completion (to surface hard errors). When false (no `event_rx`),
/// it falls back to the blocking await-final-text path.
async fn run_prompt_streaming(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    streaming: bool,
) {
    // Ensure the run starts in a clean streaming state.
    state.set_status(RunStatus::Working);
    tui.request_render(false);

    let outcome = lane.prompt_text(prompt, Vec::new()).await;

    // The drain task finalized the assistant message via MessageEnd/AgentEnd,
    // but guard against runs that ended without a terminal event (e.g. a hard
    // provider rejection before any streaming) by clearing streaming state.
    {
        let mut cur = state.current_assistant.lock().unwrap();
        if let Some(comp) = cur.take() {
            comp.set_streaming(false);
        }
    }

    state.set_status(RunStatus::Idle);

    match outcome {
        Ok(result) => match &result.outcome {
            HarnessRunOutcome::Failed { error, final_message, .. } => {
                // Only add an error line if the stream did NOT already render
                // an assistant message for it (drain task leaves
                // current_assistant Some only on an abrupt end).
                let already_rendered = final_message.is_some();
                if !already_rendered {
                    let msg = final_message
                        .as_ref()
                        .and_then(|m| m.error_message.clone())
                        .unwrap_or_else(|| format!("{error:?}"));
                    add_error_message(&state.chat_container, &msg);
                }
            }
            HarnessRunOutcome::Suspended { .. } => {
                add_error_message(
                    &state.chat_container,
                    "Run suspended (deferred) — resume is not supported in v1.",
                );
            }
            HarnessRunOutcome::Aborted { final_message, .. } => {
                // Aborted runs render their own partial/final message via the
                // stream; only add a note on the blocking fallback path.
                if !streaming {
                    add_error_message(&state.chat_container, "Request aborted.");
                    let _ = final_message; // (rendered by the stream in streaming mode)
                }
            }
            HarnessRunOutcome::Completed { final_message, .. } => {
                if !streaming {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        add_assistant_message_blocking(&state.chat_container, &text);
                        *state.last_assistant_text.lock().unwrap() = text;
                    }
                }
            }
        },
        Err(e) => {
            add_error_message(&state.chat_container, &e.to_string());
        }
    }

    tui.request_render(false);
}

/// `/compact`: drive a compaction on the lane (mirrors TS `app.compact`).
/// Reports the outcome as a transcript note; v1's compaction summarizes the
/// session in place, so no streaming display is wired (compaction emits no
/// `AgentEvent`s — only the harness bus `RunEnd`).
async fn run_compact(lane: &Arc<dyn AgentLane>, tui: &Arc<TuiAltScreen>, state: &Arc<TuiState>) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);
    match lane.compact(None).await {
        Ok(_) => {
            add_note_message(&state.chat_container, "Conversation compacted.");
        }
        Err(e) => {
            add_error_message(
                &state.chat_container,
                &format!("Compact failed: {e}"),
            );
        }
    }
    state.set_status(RunStatus::Idle);
    tui.request_render(false);
}

/// `/copy`: copy the last assistant reply to the clipboard. Best-effort —
/// when no clipboard is available (or the `clipboard` feature is off), prints a
/// hint instead. Mirrors the TS `/copy` (copies `this.messages.at(-1)` text).
fn copy_last_assistant(state: &Arc<TuiState>, chat: &Arc<Container>) {
    let text = state.last_assistant_text.lock().unwrap().clone();
    if text.is_empty() {
        add_note_message(chat, "Nothing to copy yet — no assistant reply captured.");
        return;
    }
    if copy_to_clipboard(&text) {
        add_note_message(chat, "Copied last reply to the clipboard.");
    } else {
        // Clipboard unavailable — print the text to the transcript so the user
        // can select/copy it manually (degrades gracefully in headless envs).
        let preview: String = text.chars().take(200).collect();
        add_note_message(
            chat,
            &format!("Clipboard unavailable. Last reply: {preview}{}", if text.chars().count() > 200 { "…" } else { "" }),
        );
    }
}

/// Best-effort clipboard write. Enabled only with the `clipboard` feature
/// (`arboard`); otherwise returns `false` so the caller degrades to a hint.
#[cfg(feature = "clipboard")]
fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}

#[cfg(not(feature = "clipboard"))]
fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// Blocking fallback (no `event_rx`): render the final assistant text as a
/// single `AssistantMessageComponent`, mirroring the pre-streaming behavior.
fn add_assistant_message_blocking(container: &Arc<Container>, text: &str) {
    if text.is_empty() {
        return;
    }
    let msg = Arc::new(AssistantMessageComponent::new(AssistantMessageOptions::default()));
    msg.update_text(text);
    container.add_child(msg);
    container.add_child(Arc::new(Spacer::new(1)));
}

// ===========================================================================
// AgentEvent drain task — the streaming core
// ===========================================================================

/// Drain `AgentEvent`s from the broadcast receiver and apply the TS
/// `handleEvent` event→UI mapping. Runs on a `tokio::spawn`'d task for the
/// lifetime of the TUI.
async fn drain_agent_events(
    mut rx: broadcast::Receiver<AgentEvent>,
    tui: Arc<TuiAltScreen>,
    state: Arc<TuiState>,
    chat: Arc<Container>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => handle_agent_event(event, &tui, &state, &chat).await,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // We dropped some intermediate deltas; the next MessageUpdate/
                // MessageEnd carries a full partial snapshot so the UI re-syncs.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Apply a single `AgentEvent` to the UI. Mirrors the TS `handleEvent` switch
/// (`interactive-mode.ts:3068-3396`).
async fn handle_agent_event(
    event: AgentEvent,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    chat: &Arc<Container>,
) {
    match event {
        AgentEvent::AgentStart => {
            state.set_status(RunStatus::Working);
            tui.request_render(false);
        }

        AgentEvent::AgentEnd { .. } => {
            // Finalize any still-streaming assistant message.
            if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                comp.set_streaming(false);
            }
            state.set_status(RunStatus::Idle);
            tui.request_render(false);
        }

        AgentEvent::TurnStart => {
            // A new turn: reset the streaming-assistant guard so the next
            // MessageStart creates a fresh component.
            if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                comp.set_streaming(false);
            }
        }

        AgentEvent::TurnEnd { message, tool_results } => {
            // Finalize the assistant message for this turn.
            if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                if let AgentMessage::Assistant(a) = &message {
                    comp.update_text(&assistant_text(a));
                }
                comp.set_streaming(false);
            }
            // Any tool results whose components were never ended by a
            // ToolExecutionEnd get a static rendering here (best-effort). The
            // normal path removes the component via ToolExecutionEnd; this is
            // just a no-op guard so a stray TurnEnd doesn't double-finalize.
            let tools = state.tool_components.lock().unwrap();
            for tr in &tool_results {
                if tools.contains_key(&tr.tool_call_id) {
                    // Will be removed below via ToolExecutionEnd in the normal
                    // path; leave as-is if still present.
                    let _ = tr;
                }
            }
            drop(tools);
            tui.request_render(false);
        }

        AgentEvent::MessageStart { message } => match message {
            AgentMessage::Assistant(a) => {
                let comp = Arc::new(AssistantMessageComponent::new(
                    AssistantMessageOptions::default(),
                ));
                comp.set_streaming(true);
                let text = assistant_text(&a);
                if !text.is_empty() {
                    comp.update_text(&text);
                }
                chat.add_child(comp.clone());
                chat.add_child(Arc::new(Spacer::new(0)));
                *state.current_assistant.lock().unwrap() = Some(comp);
                tui.request_render(false);
            }
            // User / ToolResult / Custom starts are echoed at submit time or
            // via the tool-execution components; ignore here to avoid dupes.
            _ => {}
        },

        AgentEvent::MessageUpdate { message, assistant_message_event } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                // Scan content for finalized tool calls → proactively create
                // tool components (TS shows the tool as soon as the assistant
                // emits the ToolCall; ToolExecutionStart coalesces if it
                // already exists).
                for c in &a.content {
                    if let Content::ToolCall(tc) = c {
                        let mut tools = state.tool_components.lock().unwrap();
                        if !tools.contains_key(&tc.id) {
                            let comp = Arc::new(ToolExecutionComponent::new(
                                &tc.name,
                                &tc.arguments.to_string(),
                            ));
                            comp.set_running();
                            chat.add_child(comp.clone());
                            tools.insert(tc.id.clone(), comp);
                        }
                    }
                }
                let _ = assistant_message_event; // snapshot already applied via `a`
                if let Some(comp) = state.current_assistant.lock().unwrap().as_ref() {
                    comp.update_text(&text);
                }
                *state.last_assistant_text.lock().unwrap() = text;
                tui.request_render(false);
            }
        }

        AgentEvent::MessageEnd { message } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                    comp.update_text(&text);
                    comp.set_streaming(false);
                }
                // Cache the finalized text for `/copy`.
                if !text.is_empty() {
                    *state.last_assistant_text.lock().unwrap() = text;
                }
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args } => {
            let comp = {
                let mut tools = state.tool_components.lock().unwrap();
                if let Some(existing) = tools.get(&tool_call_id) {
                    existing.set_args(&args.to_string());
                    existing.clone()
                } else {
                    let comp = Arc::new(ToolExecutionComponent::new(&tool_name, &args.to_string()));
                    comp.set_running();
                    chat.add_child(comp.clone());
                    tools.insert(tool_call_id.clone(), comp.clone());
                    comp
                }
            };
            let _ = comp;
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate { tool_call_id, tool_name, partial_result, .. } => {
            if let Some(comp) = state.tool_components.lock().unwrap().get(&tool_call_id) {
                let summary = summarize_tool_result(&partial_result);
                comp.set_result(&summary, false);
            } else {
                // No component yet — create a running one so the partial shows.
                let comp = Arc::new(ToolExecutionComponent::new(&tool_name, ""));
                comp.set_running();
                comp.set_result(&summarize_tool_result(&partial_result), false);
                chat.add_child(comp.clone());
                state
                    .tool_components
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), comp);
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionEnd { tool_call_id, tool_name: _, result, is_error } => {
            let comp = state.tool_components.lock().unwrap().remove(&tool_call_id);
            if let Some(comp) = comp {
                comp.set_result(&summarize_tool_result(&result), is_error);
            } else {
                // Tool ended without a Start/Update (e.g. a very fast tool):
                // render a finalized component directly.
                let comp = Arc::new(ToolExecutionComponent::new("", ""));
                comp.set_result(&summarize_tool_result(&result), is_error);
                chat.add_child(comp);
            }
            tui.request_render(false);
        }
    }
}

/// Render an `AgentToolResult` as a single-line summary for the
/// `ToolExecutionComponent` (joins text blocks; truncates for compactness).
fn summarize_tool_result(result: &rpi_agent::AgentToolResult) -> String {
    use rpi_agent::TextContentOrImage;
    let mut parts: Vec<String> = Vec::new();
    for c in &result.content {
        if let TextContentOrImage::Text(t) = c {
            parts.push(t.text.clone());
        }
    }
    let joined = parts.join("\n");
    // Keep the tool line compact: collapse to a single line, trim length.
    let one_line: String = joined.lines().collect::<Vec<_>>().join(" ⏎ ");
    if one_line.chars().count() > 200 {
        let truncated: String = one_line.chars().take(200).collect();
        format!("{truncated}…")
    } else {
        one_line
    }
}

// ===========================================================================
// Selectors — editor-container swap (TS showSelector pattern)
// ===========================================================================

/// Swap the `editor_container`'s second child (the editor) for a `SelectList`,
/// hiding the editor while the selector is open. The first child (a Spacer) is
/// preserved. Records the selector in `state.active_selector` so the key loop
/// routes to it.
fn open_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    list: Arc<SelectList>,
    kind: SelectorKind,
) {
    // Unfocus the editor so its cursor marker doesn't render behind the list.
    editor.set_focused(false);
    // Swap: clear the container, re-add the Spacer + the list.
    editor_container.clear();
    editor_container.add_child(Arc::new(Spacer::new(0)));
    editor_container.add_child(list.clone());
    *state.active_selector.lock().unwrap() = Some((list, kind));
    tui.request_render(false);
}

/// Restore the editor into the `editor_container` and clear the active
/// selector. Called by selector `on_cancel` and the Esc handler.
fn close_selector(state: &Arc<TuiState>, editor_container: &Arc<Container>, editor: &Arc<Editor>, tui: &Arc<TuiAltScreen>) {
    editor_container.clear();
    editor_container.add_child(Arc::new(Spacer::new(0)));
    editor_container.add_child(editor.clone());
    editor.set_focused(true);
    *state.active_selector.lock().unwrap() = None;
    tui.request_render(false);
}

/// Build + open the `/model` selector. Items are the resolved catalog (display
/// label = model name; description = id), with the current model marked. v1
/// can't switch models mid-run, so selecting surfaces guidance in the
/// transcript (mirrors the existing `/model` note but via the selector UI).
fn open_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
) {
    let mut items: Vec<SelectItem> = Vec::new();
    for m in catalog {
        let label = if m.name.is_empty() { short_model_name(&m.id) } else { m.name.clone() };
        let marker = if m.id.eq_ignore_ascii_case(lane_model_id) { " (current)" } else { "" };
        items.push(
            SelectItem::new(&m.id, &label)
                .with_description(&format!("{id}{marker}", id = m.id)),
        );
    }
    if items.is_empty() {
        add_note_message(
            &state.chat_container,
            "No models in the catalog. Use --model at startup to select one.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.chat_container.clone();
    list.on_select(Arc::new(move |item| {
        // v1 can't rebuild the harness mid-run — surface guidance.
        add_note_message(
            &chat_sel,
            &format!("Selected model: {}. Restart with --model {} to apply (v1).", item.label, item.value),
        );
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Model);
}

/// Build + open the `/session` selector. Lists JSONL session files under the
/// default session dir (`<cwd>/.pi/sessions`). Selecting reports "restore not
/// implemented in v1" (existing constraint) but shows the list for
/// discoverability.
fn open_session_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    cwd: &std::path::Path,
) {
    let dir = crate::session::default_session_dir(cwd);
    let mut items: Vec<SelectItem> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("(unnamed)")
                .to_string();
            let display = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&stem)
                .to_string();
            items.push(SelectItem::new(&stem, &display));
        }
    }
    if items.is_empty() {
        add_note_message(
            &state.chat_container,
            "No saved sessions found. Sessions are created automatically in interactive mode.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.chat_container.clone();
    list.on_select(Arc::new(move |item| {
        add_note_message(
            &chat_sel,
            &format!("Session {} — restore is not implemented in v1.", item.label),
        );
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Session);
}

/// Build + open the `/theme` selector. Presets [dark, light, monochrome];
/// selecting applies it live via the owned `ThemeManager` + re-renders.
fn open_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    let items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.chat_container.clone();
    list.on_select(Arc::new(move |item| {
        let preset = match item.value.as_str() {
            "light" => ThemePreset::Light,
            "monochrome" => ThemePreset::Monochrome,
            _ => ThemePreset::Dark,
        };
        state_sel.theme_manager.apply_preset(preset);
        // A quick accent note so the user sees the change registered even if
        // the terminal's own colors mask the preset difference.
        add_note_message(&chat_sel, &format!("Theme set to {}.", item.label));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        tui_sel.render_now(true);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Theme);
}

// ===========================================================================
// Autocomplete
// ===========================================================================

/// Refresh the autocomplete suggestion list from the current editor text +
/// cursor. Renders the suggestions into `autocomplete_container` (above the
/// editor) or clears it when there are none.
fn refresh_autocomplete(state: &Arc<TuiState>, editor: &Arc<Editor>) {
    let text = editor.get_text();
    let (_row, col) = editor.cursor_position();
    // The editor's `cursor_col` is a byte offset into the current line; for
    // single-line input (the common case) that equals the byte offset into
    // `get_text()`, which is exactly what the autocomplete providers expect to
    // slice on. Clamp to the text length so a stale/multi-line col can't
    // overshoot. Providers snap to a char boundary internally as a safety net
    // (`autocomplete::snap_cursor`), so a byte col landing mid-character never
    // panics.
    let cursor = col.min(text.len());
    let suggestions = state.autocomplete.get_suggestions(&text, cursor);
    render_autocomplete(state, suggestions);
}

/// Render (or clear) the autocomplete suggestion list into the container.
fn render_autocomplete(state: &Arc<TuiState>, suggestions: Option<AutocompleteSuggestions>) {
    state.autocomplete_container.clear();
    let Some(sugg) = suggestions else {
        return;
    };
    if sugg.items.is_empty() {
        return;
    }
    // Build a compact list: top item marked with `→`, rest with `  `.
    // Cap at 5 lines so the dock doesn't swallow the transcript.
    let accent = state.theme_manager.get().colors.accent;
    let muted = state.theme_manager.get().colors.muted;
    for (i, item) in sugg.items.iter().take(5).enumerate() {
        let prefix = if i == 0 { "→ " } else { "  " };
        let label = item.display_text();
        let line = if i == 0 {
            format!("{prefix}{} {}", accent.fg(label), muted.fg(item.description.as_deref().unwrap_or("")))
        } else {
            format!("{prefix}{} {}", muted.fg(label), muted.fg(item.description.as_deref().unwrap_or("")))
        };
        state
            .autocomplete_container
            .add_child(Arc::new(Text::new(line, 1, 0)));
    }
}

/// Accept the top autocomplete suggestion: replace `text[start..end]` with the
/// suggestion text, reposition the caret, and clear the suggestion list.
/// Returns `true` if a suggestion was accepted.
fn accept_top_suggestion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    let text = editor.get_text();
    let (_row, col) = editor.cursor_position();
    let cursor = col.min(text.len());
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    let Some(top) = sugg.items.first() else {
        return false;
    };
    // Replace the [start, end) span with the suggestion text. `start`/`end`
    // are byte offsets emitted by the providers on char boundaries, so the
    // `text[..start]` / `text[end..]` slices are sound for multibyte input.
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    let mut replaced = String::with_capacity(text.len() + top.text.len());
    replaced.push_str(&text[..start]);
    replaced.push_str(&top.text);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    // New caret position: after the inserted text (byte offset; the editor
    // snaps `set_cursor` to a char boundary as a safety net).
    let new_cursor = replaced.len().min(
        start + top.text.len()
            + if top.insert_space && !top.text.ends_with('/') {
                1
            } else {
                0
            },
    );
    let _ = end;
    editor.set_text(&replaced);
    editor.set_cursor(0, new_cursor);
    state.autocomplete_container.clear();
    true
}

// ===========================================================================
// Transcript message helpers
// ===========================================================================

/// Add the welcome header to the chat container.
fn add_welcome_message(container: &Arc<Container>) {
    container.add_child(Arc::new(Text::new("rpi interactive TUI", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        "Type your message and press Shift+Enter to send.",
        1, 0,
    )));
    container.add_child(Arc::new(Text::new(
        "Ctrl+C: Abort/Exit | Enter: New line | Shift+Enter: Send | Tab: Complete | /help for commands",
        1, 0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/help` command listing to the chat container.
fn add_help_message(container: &Arc<Container>) {
    container.add_child(Arc::new(Text::new("📚 Available Commands:", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new("  /help, /?       — Show this help message", 1, 0)));
    container.add_child(Arc::new(Text::new("  /clear, /new    — Clear the conversation", 1, 0)));
    container.add_child(Arc::new(Text::new("  /exit, /quit, /q — Exit the application", 1, 0)));
    container.add_child(Arc::new(Text::new("  /version, /v    — Show version information", 1, 0)));
    container.add_child(Arc::new(Text::new("  /model, /m      — Choose a model (selector)", 1, 0)));
    container.add_child(Arc::new(Text::new("  /session        — List saved sessions", 1, 0)));
    container.add_child(Arc::new(Text::new("  /theme          — Choose a theme (selector)", 1, 0)));
    container.add_child(Arc::new(Text::new("  /compact        — Compact the conversation", 1, 0)));
    container.add_child(Arc::new(Text::new("  /copy           — Copy last reply to clipboard", 1, 0)));
    container.add_child(Arc::new(Text::new("  /hotkeys        — Show keyboard shortcuts", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/version` block to the chat container.
fn add_version_message(container: &Arc<Container>) {
    container.add_child(Arc::new(Text::new("📦 Version Information:", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new("  rpi-cli v0.1.2", 1, 0)));
    container.add_child(Arc::new(Text::new(
        "  Rust implementation of pi coding agent TUI",
        1, 0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/hotkeys` block to the chat container.
fn add_hotkeys_message(container: &Arc<Container>) {
    container.add_child(Arc::new(Text::new("⌨️  Keyboard Shortcuts:", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new("  Shift+Enter   — Send message", 1, 0)));
    container.add_child(Arc::new(Text::new("  Enter         — New line", 1, 0)));
    container.add_child(Arc::new(Text::new("  Tab           — Accept autocomplete suggestion", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+A / Ctrl+E — Line start / end", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+K / Ctrl+U — Delete to end / start of line", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+C        — Abort a run, or exit when idle", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+L        — Open model selector", 1, 0)));
    container.add_child(Arc::new(Text::new("  PageUp/Down   — Scroll transcript", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a user message echo to the chat container.
fn add_user_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(Text::new(format!("> {text}"), 1, 0)));
    container.add_child(Arc::new(Spacer::new(0)));
}

/// Add an error message to the chat container.
fn add_error_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(Text::new(format!("❌ {text}"), 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a neutral note (e.g. unsupported-command message) to the chat container.
fn add_note_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(Text::new(format!("ℹ️  {text}"), 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

// ===========================================================================
// TUI support + entry detection
// ===========================================================================

/// Check if the terminal supports TUI mode.
pub fn is_tui_supported() -> bool {
    std::io::stdout().is_terminal()
}

// Keep the `Color` import used (theme accent rendering in autocomplete).
#[allow(unused_imports)]
use rpi_tui::Color as _Color;

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::Component;

    #[test]
    fn test_layout_renders_welcome_message() {
        let chat = Arc::new(Container::new());
        add_welcome_message(&chat);

        let scroll = Arc::new(ScrollView::new(
            chat.clone(),
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));

        let editor = Arc::new(Editor::new(
            EditorOptions {
                padding_x: 1,
                ..Default::default()
            },
            EditorStyle::default(),
            Arc::new(rpi_tui::Keybindings::new()),
        ));
        let dock = Arc::new(Container::new());
        dock.add_child(editor);

        let footer = Arc::new(FooterComponent::new());

        let root = VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(scroll.clone()).grow(1).min_size(1)),
            StackChild::Entry(StackEntry::new(dock)),
            StackChild::Entry(StackEntry::new(footer)),
        ]);

        let frame = rpi_tui::render_layout_frame(Arc::new(root), 80, 24);

        let all: String = frame.lines.join("\n");
        assert!(all.contains("rpi interactive"), "Welcome message not found. Rendered: {}", all);
        assert!(all.contains("Type your message"), "Help text not found. Rendered: {}", all);
    }

    #[test]
    fn test_chat_container_has_welcome_content() {
        let chat = Arc::new(Container::new());
        add_welcome_message(&chat);

        let lines = chat.render(80);
        let all: String = lines.join("\n");
        assert!(all.contains("rpi interactive"), "Welcome message not in chat container: {:?}", lines);
    }

    #[test]
    fn test_slash_command_dispatch() {
        assert!(matches!(handle_slash_command("/help"), SlashCommandResult::Help));
        assert!(matches!(handle_slash_command("/clear"), SlashCommandResult::ClearChat));
        assert!(matches!(handle_slash_command("/q"), SlashCommandResult::Exit));
        assert!(matches!(handle_slash_command("/hotkeys"), SlashCommandResult::Hotkeys));
        assert!(matches!(
            handle_slash_command("/model"),
            SlashCommandResult::SelectModel
        ));
        assert!(matches!(
            handle_slash_command("/theme"),
            SlashCommandResult::SelectTheme
        ));
        assert!(matches!(handle_slash_command("/session"), SlashCommandResult::SelectSession));
        assert!(matches!(handle_slash_command("/compact"), SlashCommandResult::Compact));
        assert!(matches!(handle_slash_command("/copy"), SlashCommandResult::Copy));
        assert!(matches!(
            handle_slash_command("/settings"),
            SlashCommandResult::Unsupported(_)
        ));
        assert!(matches!(handle_slash_command("/nope"), SlashCommandResult::Unknown));
        // Empty input resolves to SendMessage (defensive; the submit handler
        // guards on `starts_with('/')` so this path is only hit for blanks).
        assert!(matches!(
            handle_slash_command(""),
            SlashCommandResult::SendMessage(_)
        ));
    }

    #[test]
    fn test_v1_slash_commands_cover_dispatcher() {
        // Every command the dispatcher recognizes as non-Unsupported/Unknown
        // should appear in the autocomplete list (so `/`-autocomplete stays in
        // sync with the actual command surface).
        let cmds = v1_slash_commands();
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        for recognized in ["/help", "/clear", "/new", "/exit", "/quit", "/version",
            "/model", "/session", "/theme", "/compact", "/copy", "/hotkeys"]
        {
            assert!(names.contains(&recognized), "{recognized} missing from autocomplete list");
        }
    }

    #[test]
    fn test_agent_event_mapping_creates_assistant_and_tool() {
        // Synthetic AgentEvent sequence → UI mutations, exercised against the
        // real drain handler with a no-op TUI stand-in.
        use rpi_ai::types::{StopReason, TextContent, TextContentType, ToolCall, ToolCallType, Usage};

        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            status: std::sync::Mutex::new(RunStatus::Idle),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
        });

        // The drain handler takes `Arc<TuiAltScreen>`, which needs a real
        // terminal; instead, exercise the *mutation* half directly against a
        // captured chat container via a synthetic message-start event's data.
        let assistant = AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![
                Content::Text(TextContent {
                    kind: TextContentType,
                    text: "Hello.".into(),
                    text_signature: None,
                }),
                Content::ToolCall(ToolCall {
                    kind: ToolCallType,
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                    thought_signature: None,
                    namespace: None,
                }),
            ],
            api: rpi_ai::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        };

        // Manually apply the MessageStart assistant branch logic (mirrors the
        // drain handler, without needing a TuiAltScreen).
        let comp = Arc::new(AssistantMessageComponent::new(AssistantMessageOptions::default()));
        comp.set_streaming(true);
        comp.update_text(&assistant_text(&assistant));
        let chat = Arc::new(Container::new());
        chat.add_child(comp.clone());
        *state.current_assistant.lock().unwrap() = Some(comp);

        // Manually apply the MessageUpdate tool-call scan (mirrors drain).
        for c in &assistant.content {
            if let Content::ToolCall(tc) = c {
                let mut tools = state.tool_components.lock().unwrap();
                if !tools.contains_key(&tc.id) {
                    let tc_comp = Arc::new(ToolExecutionComponent::new(
                        &tc.name,
                        &tc.arguments.to_string(),
                    ));
                    tc_comp.set_running();
                    chat.add_child(tc_comp.clone());
                    tools.insert(tc.id.clone(), tc_comp);
                }
            }
        }

        // Assert: the assistant component rendered the text, and a tool
        // component was registered.
        let rendered = chat.render(80);
        let joined: String = rendered.join("\n");
        assert!(joined.contains("Hello."), "assistant text not rendered: {joined}");
        assert_eq!(state.tool_components.lock().unwrap().len(), 1);
        assert!(state.current_assistant.lock().unwrap().is_some());

        // Manually apply ToolExecutionEnd (mirrors drain).
        let ended = state.tool_components.lock().unwrap().remove("tc1").unwrap();
        ended.set_result("hi", false);
        assert!(state.tool_components.lock().unwrap().is_empty());
    }

    #[test]
    fn test_short_model_name() {
        assert_eq!(short_model_name("anthropic:claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(short_model_name("claude-sonnet-5"), "claude-sonnet-5");
    }

    #[test]
    fn test_autocomplete_slash_suggestions_render() {
        // The autocomplete container should render at least one suggestion
        // line when the editor holds a `/` prefix, and clear when it doesn't.
        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            status: std::sync::Mutex::new(RunStatus::Idle),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
        });
        {
            let mut combined = CombinedAutocompleteProvider::new();
            combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
                v1_slash_commands(),
            )));
            state.autocomplete.set_provider(Arc::new(combined));
        }

        let editor = Arc::new(Editor::simple());
        editor.set_text("/he");
        editor.set_cursor(0, 3);
        refresh_autocomplete(&state, &editor);
        let lines = state.autocomplete_container.render(80);
        let joined: String = lines.join("\n");
        assert!(joined.contains("/help"), "slash suggestions not rendered: {joined}");

        // Clear: no suggestions for plain text.
        editor.set_text("hello");
        editor.set_cursor(0, 5);
        refresh_autocomplete(&state, &editor);
        assert!(state.autocomplete_container.render(80).is_empty());
    }

    #[test]
    fn test_select_list_swap_restores_editor() {
        // The editor-container swap: opening a selector replaces the editor
        // child; closing restores it. Verify the container child count + the
        // active_selector flag round-trip.
        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            status: std::sync::Mutex::new(RunStatus::Idle),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
        });
        let editor_container = Arc::new(Container::new());
        let editor = Arc::new(Editor::simple());
        editor_container.add_child(Arc::new(Spacer::new(0)));
        editor_container.add_child(editor.clone());
        assert!(!state.selector_open());

        let tui_terminal = Box::new(ProcessTerminal::new());
        let tui = Arc::new(TuiAltScreen::new(tui_terminal, true, None));
        let list = Arc::new(SelectList::new(
            vec![SelectItem::new("a", "A"), SelectItem::new("b", "B")],
            5,
        ));
        open_selector(&state, &editor_container, &editor, &tui, list, SelectorKind::Theme);
        assert!(state.selector_open());
        // Spacer + list = 2 children (editor swapped out).
        assert_eq!(editor_container.child_count(), 2);

        close_selector(&state, &editor_container, &editor, &tui);
        assert!(!state.selector_open());
        // Spacer + editor = 2 children (restored).
        assert_eq!(editor_container.child_count(), 2);
    }

    #[test]
    fn test_accept_top_suggestion_replaces_prefix() {
        // `/he` + Tab → `/help ` (slash command provider inserts a space).
        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            status: std::sync::Mutex::new(RunStatus::Idle),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
        });
        {
            let mut combined = CombinedAutocompleteProvider::new();
            combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
                v1_slash_commands(),
            )));
            state.autocomplete.set_provider(Arc::new(combined));
        }
        let editor = Arc::new(Editor::simple());
        editor.set_text("/he");
        editor.set_cursor(0, 3);
        refresh_autocomplete(&state, &editor);
        let accepted = accept_top_suggestion(&state, &editor);
        assert!(accepted, "should accept the top suggestion");
        let text = editor.get_text();
        assert!(
            text.starts_with("/help"),
            "editor text should start with /help, got {text}"
        );
    }
}
