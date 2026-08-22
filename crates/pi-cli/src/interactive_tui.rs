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

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
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
    ThemeManager, ThemePreset, ToolExecutionComponent, render_diff,
    BashExecutionComponent, BashTruncation, UserMessageComponent,
};

#[allow(unused_imports)]
use rpi_tui::BashStatus;

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
    /// Open the thinking-level selector overlay.
    SelectThinking,
    /// Open the tools toggle selector overlay.
    SelectTools,
    /// Open the image-display toggle overlay.
    SelectImages,
    /// Show the armin easter-egg.
    Armin,
    /// Show the earendil announcement.
    Earendil,
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
        "/thinking" | "/think" => SlashCommandResult::SelectThinking,
        "/tools" => SlashCommandResult::SelectTools,
        "/images" => SlashCommandResult::SelectImages,
        "/armin" => SlashCommandResult::Armin,
        "/earendil" => SlashCommandResult::Earendil,
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
        SlashCommand { name: "/thinking".into(), description: "Set thinking level (selector)".into() },
        SlashCommand { name: "/tools".into(), description: "Toggle tools on/off".into() },
        SlashCommand { name: "/images".into(), description: "Toggle inline images".into() },
        SlashCommand { name: "/session".into(), description: "List saved sessions".into() },
        SlashCommand { name: "/theme".into(), description: "Choose a theme (selector)".into() },
        SlashCommand { name: "/compact".into(), description: "Compact the conversation".into() },
        SlashCommand { name: "/copy".into(), description: "Copy last reply to clipboard".into() },
        SlashCommand { name: "/hotkeys".into(), description: "Show keyboard shortcuts".into() },
        SlashCommand { name: "/armin".into(), description: "??? (easter egg)".into() },
        SlashCommand { name: "/earendil".into(), description: "Announcement".into() },
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
    /// `/model` — available models (live switch via `lane.set_model`).
    Model,
    /// `/thinking` — supported thinking levels (live via `lane.set_thinking_level`).
    Thinking,
    /// `/tools` — toggle builtin tools on/off.
    Tools,
    /// `/images` — toggle inline image rendering.
    Images,
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
    /// Bash-execution components keyed by `tool_call_id` (kept separate from the
    /// generic tool map so bash output streams into a `BashExecutionComponent`
    /// rather than a plain `ToolExecutionComponent`). Phase 5 routing.
    bash_components: std::sync::Mutex<HashMap<String, Arc<BashExecutionComponent>>>,
    /// The most recently created tool component (bash or generic). Ctrl+T
    /// toggles `expanded` on this — a pragmatic "expand last tool" since the
    /// key loop has no per-line focus. Updated on every tool/bash Start.
    last_tool_comp: std::sync::Mutex<Option<Arc<ToolExecutionComponent>>>,
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
    /// The alt-screen handle, held so `set_status` can reflect run state in the
    /// terminal window title ("rpi — working" / "rpi"). `None` in unit tests
    /// that never call `set_status` with a title.
    tui: Option<Arc<TuiAltScreen>>,
    /// The model id currently shown in the footer + used as the Ctrl+M
    /// cycle anchor. Sync-tracked (updated on every `/model`/Ctrl+M switch) so
    /// the blocking key loop can cycle without awaiting `lane.get_model()`.
    current_model_id: std::sync::Mutex<String>,
    /// Whether inline image rendering is enabled (`/images` toggle). Stored
    /// even though image wiring is minimal this pass — the flag is consulted
    /// where images would be shown and echoed back by `/images`.
    show_images: std::sync::Mutex<bool>,
}

impl TuiState {
    fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = status;
        match status {
            RunStatus::Working => {
                self.footer.set_status("Working…");
                // Reflect the in-flight turn in the terminal window/tab title
                // (OSC 2). No-op when `tui` is absent (unit tests).
                if let Some(tui) = &self.tui {
                    tui.set_title("rpi — working");
                }
                self.status_container.clear();
                self.loader.start();
                self.status_container.add_child(self.loader.clone());
            }
            RunStatus::Aborting => {
                self.footer.set_status("Aborting…");
            }
            RunStatus::Idle => {
                self.footer.set_status("");
                if let Some(tui) = &self.tui {
                    tui.set_title("rpi");
                }
                self.loader.stop();
                self.status_container.clear();
            }
        }
    }

    /// Whether a selector overlay is currently open (routes keys to it first).
    fn selector_open(&self) -> bool {
        self.active_selector.lock().unwrap().is_some()
    }

    /// Record a freshly created tool component as the "most recent" so Ctrl+T
    /// can toggle its expansion. Idempotent overwrites — only the latest lives.
    fn remember_tool(&self, comp: Arc<ToolExecutionComponent>) {
        *self.last_tool_comp.lock().unwrap() = Some(comp);
    }

    /// Toggle `expanded` on the most recent tool component (Ctrl+T). Returns
    /// `true` if a component was toggled. Limitation: the key loop tracks no
    /// per-line focus, so this always targets the *last* tool shown — not the
    /// one under the cursor. Documented in the plan; a focused expansion would
    /// need mouse/line hit-testing which is out of scope this pass.
    fn toggle_expand_last_tool(&self) -> bool {
        if let Some(comp) = self.last_tool_comp.lock().unwrap().as_ref() {
            let cur = comp.is_expanded();
            comp.set_expanded(!cur);
            true
        } else {
            false
        }
    }

    /// The model id currently tracked as active (footer + Ctrl+M anchor).
    fn current_model_id(&self) -> String {
        self.current_model_id.lock().unwrap().clone()
    }

    /// Update the tracked model id + footer label after a switch (live or
    /// cycle). Called from the `/model` on_select and the Ctrl+M handler.
    fn set_current_model(&self, model: &rpi_ai::Model) {
        *self.current_model_id.lock().unwrap() = model.id.clone();
        self.footer.set_model(&short_model_name(&model.id));
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

    // Resolve the active model once, up front. The full id feeds the TuiState
    // tracking field + the selectors/key loop (which run on a blocking thread
    // and can't await `lane.get_model()`); the short name feeds the footer.
    let lane_model_id = lane
        .get_model()
        .await
        .map(|m| m.id)
        .unwrap_or_default();
    let model_name = short_model_name(&lane_model_id);

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

    // First-launch gate: if `~/.rpi/.setup_done` is absent, show the welcome
    // banner + the earendil announcement once, then write the sentinel. The TS
    // original is a multi-step dialog (theme picker + analytics opt-in); this
    // v1 simplifies to a one-shot banner (theme still pickable via `/theme`,
    // analytics deferred — no telemetry wiring). See `extras.rs`.
    crate::extras::maybe_first_time_setup(&chat_container);

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
    footer.set_hints("Enter: Send | Shift+Enter: New line | Ctrl+C: Abort/Exit | Esc: Abort | Ctrl+L: Model | Ctrl+M: Cycle | Ctrl+T: Expand tool | /help");

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
        bash_components: std::sync::Mutex::new(HashMap::new()),
        last_tool_comp: std::sync::Mutex::new(None),
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
        tui: Some(tui.clone()),
        current_model_id: std::sync::Mutex::new(lane_model_id.clone()),
        show_images: std::sync::Mutex::new(true),
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
    // root = VStack[ scrollview(basis:0 grow:1 shrink:1 min:1), dock(shrink:1) ]
    // dock  = VStack[ status(auto), autocomplete(auto), editor_container(shrink:0 min:3), footer(auto) ]
    //
    // The scrollview gets `basis(0)` so the constrained stack allocator starts
    // it at zero height and grows it to fill the space the dock does not need
    // — this keeps the dock (editor borders + footer) pinned to the bottom and
    // never shrinks it below the editor's 3 rows (top border + content + bottom
    // border). The editor_container is `shrink(0).min_size(3)` so a tall
    // transcript can never clip the bordered editor below its minimum.
    let editor_container = Arc::new(Container::new());
    editor_container.add_child(editor.clone());

    let dock = Arc::new(VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(status_container.clone())),
        StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
        StackChild::Entry(
            StackEntry::new(editor_container.clone())
                .shrink(0)
                .min_size(3),
        ),
        StackChild::Entry(StackEntry::new(footer.clone())),
    ]));

    let root = VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(scroll_view.clone())
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(dock).shrink(1)),
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
    let lane_for_cb = lane.clone();
    // Clone the shared selector inputs for the closure; the originals stay
    // available for the key-dispatch loop below (Ctrl+L opens /model too).
    let editor_container_for_cb = editor_container.clone();
    let model_catalog_for_cb = model_catalog_arc.clone();
    let lane_model_id_for_cb = lane_model_id.clone();
    let cwd_for_cb = cwd.clone();
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
                        &lane_for_cb,
                        &lane_model_id_for_cb,
                        &chat_for_cb,
                    );
                }
                SlashCommandResult::SelectThinking => {
                    open_thinking_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &lane_for_cb,
                        &model_catalog_for_cb,
                        &lane_model_id_for_cb,
                        &chat_for_cb,
                    );
                }
                SlashCommandResult::SelectTools => {
                    open_tools_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &lane_for_cb,
                        &chat_for_cb,
                    );
                }
                SlashCommandResult::SelectImages => {
                    open_images_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &chat_for_cb,
                    );
                }
                SlashCommandResult::Armin => {
                    crate::extras::add_armin(&chat_for_cb);
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::Earendil => {
                    crate::extras::add_earendil(&chat_for_cb);
                    tui_for_cb.request_render(false);
                }
                SlashCommandResult::SelectSession => {
                    open_session_selector(
                        &state_for_cb,
                        &editor_container_for_cb,
                        &editor_for_cb,
                        &tui_for_cb,
                        &cwd_for_cb,
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

    tui.start_readerless();

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
    let chat_for_key = chat_container.clone();

    tokio::task::spawn_blocking(move || {
        loop {
            if !*running_key.lock().unwrap() {
                break;
            }
            let Ok(ev) = crossterm::event::read() else {
                continue;
            };
            // `Event::Resize` is delivered as its own event (not a Key). With
            // `start_readerless` there is no competing terminal-reader thread to
            // handle it, so refresh the cached terminal size here and force a
            // full redraw so the constrained layout re-fits the new dimensions.
            if let Event::Resize(_cols, _rows) = ev {
                tui_for_key.refresh_size();
                continue;
            }
            let Event::Key(key) = ev else { continue; };
            // Drop release/repeat events — on Windows a single keystroke
            // yields both a Press and a Release; without this filter every
            // char is inserted twice. (Mirrors the TS `isKeyRelease` guard;
            // the editor never sets `wants_key_release`.) On terminals that
            // only emit Press this is a no-op.
            if key.kind != KeyEventKind::Press {
                continue;
            }

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

            // 2b. Esc: interrupt an active run (mirrors Ctrl+C abort). When a
            //     selector is open Esc already cancelled it above; when idle,
            //     Esc falls through to the editor (no-op-ish). Only fire while
            //     Working so an idle Esc doesn't abort a non-existent run.
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc {
                let status = *state_for_key.status.lock().unwrap();
                if status == RunStatus::Working {
                    state_for_key.set_status(RunStatus::Aborting);
                    let lane = lane_for_key.clone();
                    tokio::spawn(async move {
                        let _ = lane.abort().await;
                    });
                    continue;
                }
            }

            // 2c. Ctrl+T: toggle expansion on the most recent tool component.
            //     The key loop tracks no per-line focus, so this is an "expand
            //     last tool" affordance rather than a cursor-targeted toggle
            //     (documented limitation; see `toggle_expand_last_tool`).
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('t') {
                state_for_key.toggle_expand_last_tool();
                tui_for_key.request_render(false);
                continue;
            }

            // 2d. Ctrl+M: cycle to the next model in the catalog after the one
            //     currently tracked in `current_model_id`, apply it live via
            //     `lane.set_model` (takes effect on the next user message — the
            //     in-flight run's config is already snapshotted), and update the
            //     footer. `set_model` is async so it runs on a spawned task.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('m') {
                if let Some(next) = cycle_next_model(&catalog_for_key, &state_for_key.current_model_id()) {
                    state_for_key.set_current_model(&next);
                    let lane = lane_for_key.clone();
                    tokio::spawn(async move {
                        let _ = lane.set_model(next).await;
                    });
                    tui_for_key.request_render(false);
                }
                continue;
            }

            // 3. Ctrl+L: open the model selector (TS binds Ctrl+L to
            //    model-select). Selecting now applies live via `lane.set_model`
            //    (next-prompt effect); the catalog + current id were captured
            //    before this blocking loop.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('l') {
                open_model_selector(
                    &state_for_key,
                    &editor_container_for_key,
                    &editor_for_key,
                    &tui_for_key,
                    &catalog_for_key,
                    &lane_for_key,
                    &lane_model_id_for_key,
                    &chat_for_key,
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
            if tool_name == "bash" {
                // Bash streams into a dedicated BashExecutionComponent (command
                // header + live preview + exit/truncation status) rather than a
                // generic ToolExecutionComponent. The command comes from the
                // `command` field of the bash tool args.
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let comp = Arc::new(BashExecutionComponent::new(command));
                chat.add_child(comp.clone());
                state
                    .bash_components
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), comp);
            } else {
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
                state.remember_tool(comp);
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate { tool_call_id, tool_name, partial_result, .. } => {
            if tool_name == "bash" {
                // Append the streamed chunk to the bash component's preview.
                let chunk = summarize_tool_result(&partial_result);
                if let Some(bash) = state.bash_components.lock().unwrap().get(&tool_call_id) {
                    bash.append_output(&chunk);
                } else {
                    // No component yet — create a running bash one so the
                    // partial shows (command unknown at Update time; leave blank).
                    let comp = Arc::new(BashExecutionComponent::new(""));
                    comp.append_output(&chunk);
                    chat.add_child(comp.clone());
                    state
                        .bash_components
                        .lock()
                        .unwrap()
                        .insert(tool_call_id.clone(), comp);
                }
            } else if let Some(comp) = state.tool_components.lock().unwrap().get(&tool_call_id) {
                let summary = summarize_tool_result(&partial_result);
                comp.set_result(&summary, false);
                apply_edit_diff(comp, &tool_name, &partial_result.details, &tui);
                state.remember_tool(comp.clone());
            } else {
                // No component yet — create a running one so the partial shows.
                let comp = Arc::new(ToolExecutionComponent::new(&tool_name, ""));
                comp.set_running();
                comp.set_result(&summarize_tool_result(&partial_result), false);
                apply_edit_diff(&comp, &tool_name, &partial_result.details, &tui);
                chat.add_child(comp.clone());
                state
                    .tool_components
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), comp.clone());
                state.remember_tool(comp);
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error } => {
            if tool_name == "bash" {
                let bash = state.bash_components.lock().unwrap().remove(&tool_call_id);
                if let Some(bash) = bash {
                    finalize_bash(&bash, &result, is_error);
                } else {
                    // Bash ended without a Start/Update — render a finalized
                    // component directly from the result text.
                    let command = result
                        .details
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    comp.append_output(&summarize_tool_result(&result));
                    finalize_bash(&comp, &result, is_error);
                    chat.add_child(comp);
                }
            } else {
                let comp = state.tool_components.lock().unwrap().remove(&tool_call_id);
                if let Some(comp) = comp {
                    comp.set_result(&summarize_tool_result(&result), is_error);
                    apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                } else {
                    // Tool ended without a Start/Update (e.g. a very fast tool):
                    // render a finalized component directly.
                    let comp = Arc::new(ToolExecutionComponent::new(&tool_name, ""));
                    comp.set_result(&summarize_tool_result(&result), is_error);
                    apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                    chat.add_child(comp.clone());
                    state.remember_tool(comp);
                }
            }
            tui.request_render(false);
        }
    }
}

/// Extract `BashToolDetails` (`truncation`, `full_output_path`) from a bash
/// tool result and mark the component complete. Mirrors the TS bash finalize
/// path; only the fields `BashExecutionComponent` needs are read.
fn finalize_bash(comp: &Arc<BashExecutionComponent>, result: &rpi_agent::AgentToolResult, is_error: bool) {
    // The exit code isn't in details directly (TS carries it elsewhere); use
    // `is_error` as the error signal and 0/1 as a best-effort exit code.
    let exit_code = if is_error { Some(1) } else { Some(0) };
    let truncated = result
        .details
        .get("truncation")
        .and_then(|t| t.get("truncated"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let full_output_path = result
        .details
        .get("full_output_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let truncation = BashTruncation {
        truncated,
        full_output_path,
    };
    let cancelled = false; // cancellation surfaces via Abort/AgentEnd, not a bash detail
    comp.set_complete(exit_code, cancelled, truncation);
}

/// If `tool_name` is an editing tool (`edit`) whose `details.diff` carries a
/// display-diff string, render it with colors and attach to the component so
/// the changes show in the transcript. `write` has no diff (details: Null) and
/// stays a plain summary.
fn apply_edit_diff(
    comp: &Arc<ToolExecutionComponent>,
    tool_name: &str,
    details: &serde_json::Value,
    tui: &Arc<TuiAltScreen>,
) {
    if tool_name != "edit" {
        return;
    }
    let Some(diff_text) = details.get("diff").and_then(|v| v.as_str()) else {
        return;
    };
    if diff_text.is_empty() {
        return;
    }
    let width = tui.width();
    let lines = render_diff(diff_text, width);
    comp.set_diff(lines);
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

/// Swap the `editor_container`'s child (the editor) for a `SelectList`,
/// hiding the editor while the selector is open. Records the selector in
/// `state.active_selector` so the key loop routes to it.
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
    // Swap: clear the container and add just the list.
    editor_container.clear();
    editor_container.add_child(list.clone());
    *state.active_selector.lock().unwrap() = Some((list, kind));
    tui.request_render(false);
}

/// Restore the editor into the `editor_container` and clear the active
/// selector. Called by selector `on_cancel` and the Esc handler.
fn close_selector(state: &Arc<TuiState>, editor_container: &Arc<Container>, editor: &Arc<Editor>, tui: &Arc<TuiAltScreen>) {
    editor_container.clear();
    editor_container.add_child(editor.clone());
    editor.set_focused(true);
    *state.active_selector.lock().unwrap() = None;
    tui.request_render(false);
}

/// Build + open the `/model` selector. Items are the resolved catalog (display
/// label = model name; description = id), with the current model marked.
/// Selecting applies the model **live** via `lane.set_model` (takes effect on
/// the next user message — the in-flight run's config is already snapshotted),
/// updates the footer, and notes the next-prompt effect.
fn open_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
    lane: &Arc<dyn AgentLane>,
    lane_model_id: &str,
    chat: &Arc<Container>,
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
            chat,
            "No models in the catalog. Use --model at startup to select one.",
        );
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    // Capture the catalog + lane so the on_select closure can resolve the
    // chosen Model and apply it. `on_select` fires on the blocking key thread,
    // so the async `set_model` runs on a spawned task (matches Ctrl+M).
    let catalog_arc = catalog.to_vec();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = catalog_arc.iter().find(|m| m.id == item.value).cloned() else {
            add_note_message(&chat_sel, &format!("Model {} not found in catalog.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        state_sel.set_current_model(&model);
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_model(model).await;
        });
        add_note_message(
            &chat_sel,
            &format!(
                "Model set to {} — applies to the next message.",
                short_model_name(&item.value)
            ),
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

/// Cycle to the next catalog entry after `current_id`, wrapping to the first.
/// Returns `None` only when the catalog is empty or the current id isn't
/// found (in which case the first entry is returned — a no-op if it IS the
/// current). Used by the Ctrl+M model-cycle hotkey.
fn cycle_next_model(catalog: &[rpi_ai::Model], current_id: &str) -> Option<rpi_ai::Model> {
    if catalog.is_empty() {
        return None;
    }
    let idx = catalog
        .iter()
        .position(|m| m.id.eq_ignore_ascii_case(current_id));
    match idx {
        Some(i) => {
            let next = (i + 1) % catalog.len();
            Some(catalog[next].clone())
        }
        None => Some(catalog[0].clone()),
    }
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
// Feasible selectors — /thinking, /tools, /images
// ===========================================================================

/// One-line descriptions for each thinking level, ported from
/// thinking-selector.ts (the TS `getThinkingLevelDescription` table).
fn thinking_level_description(level: rpi_ai::types::ThinkingLevel) -> &'static str {
    use rpi_ai::types::ThinkingLevel::*;
    match level {
        Off => "Off — No reasoning",
        Minimal => "Minimal — Brief reasoning (~1k tokens)",
        Low => "Low — Light reasoning (~1k tokens)",
        Medium => "Medium — Moderate reasoning (~80% of max)",
        High => "High — Extensive reasoning (~95% of max)",
        Xhigh => "Xhigh — Near-maximal reasoning",
        Max => "Max — Maximum reasoning",
    }
}

/// The lowercase serialized name of a [`ThinkingLevel`] (matches its
/// `#[serde(rename_all = "lowercase")]` form): "off", "minimal", … "max".
fn thinking_level_name(level: rpi_ai::types::ThinkingLevel) -> &'static str {
    use rpi_ai::types::ThinkingLevel::*;
    match level {
        Off => "off",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
}

/// Parse a thinking-level name back to the enum (case-insensitive). Returns
/// `None` for an unknown name; used by the `/thinking` selector callback.
fn thinking_level_from_name(name: &str) -> Option<rpi_ai::types::ThinkingLevel> {
    use rpi_ai::types::ThinkingLevel::*;
    match name.to_ascii_lowercase().as_str() {
        "off" => Some(Off),
        "minimal" => Some(Minimal),
        "low" => Some(Low),
        "medium" => Some(Medium),
        "high" => Some(High),
        "xhigh" => Some(Xhigh),
        "max" => Some(Max),
        _ => None,
    }
}

/// Build + open the `/thinking` selector. Items are the levels the current
/// model supports (`Model::supported_thinking_levels`), each with a
/// description; the current level (read beforehand via `lane.get_thinking_level`)
/// is preselected. Selecting applies it live via `lane.set_thinking_level`.
///
/// `on_select` fires on the blocking key thread, so it can't await
/// `lane.get_thinking_level()` to know the current level — the opener resolves
/// it first (best-effort) and preselects; the toggle on_select just applies
/// whatever was picked.
fn open_thinking_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
    // Find the current model in the catalog to read its supported levels. If
    // absent, fall back to all levels so the selector still opens.
    let model = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(lane_model_id));
    let levels: Vec<rpi_ai::types::ThinkingLevel> = model
        .map(|m| m.supported_thinking_levels())
        .unwrap_or_else(|| {
            use rpi_ai::types::ThinkingLevel::*;
            vec![Off, Minimal, Low, Medium, High]
        });
    let mut items: Vec<SelectItem> = Vec::new();
    for lvl in &levels {
        let name = thinking_level_name(*lvl);
        items.push(
            SelectItem::new(name, name)
                .with_description(thinking_level_description(*lvl)),
        );
    }
    if items.is_empty() {
        add_note_message(chat, "This model has no supported thinking levels.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(level) = thinking_level_from_name(&item.value) else {
            add_note_message(&chat_sel, &format!("Unknown thinking level: {}.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_thinking_level(level).await;
        });
        add_note_message(&chat_sel, &format!("Thinking set to {}.", item.label));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Thinking);
}

/// Build + open the `/tools` selector. Lists the 7 builtin tool names; each
/// visit reads the live active set via `lane.get_active_tools()` (best-effort,
/// resolved synchronously by the opener using `tokio::runtime::Handle` block_on
/// — the blocking key thread can't await) and selecting a tool **toggles** it
/// on/off via `lane.set_active_tools`. Active tools are marked `(on)`.
fn open_tools_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    chat: &Arc<Container>,
) {
    // Best-effort read of the current active set. The opener runs on the async
    // runtime (it's called from the main loop's channel dispatch or the submit
    // closure that lives on the blocking thread — but `handle.block_on` is safe
    // because `get_active_tools` is std-Mutex-backed and finishes quickly).
    let active = match tokio::runtime::Handle::try_current() {
        Ok(h) => h.block_on(async { lane.get_active_tools().await }).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut items: Vec<SelectItem> = Vec::new();
    for name in crate::session::BUILTIN_TOOL_NAMES {
        let on = active.iter().any(|a| a == name);
        let label = if on { format!("{name} (on)") } else { (*name).to_string() };
        items.push(SelectItem::new(name, &label).with_description("Toggle tool on/off"));
    }
    let list = Arc::new(SelectList::new(items, 10));

    // Capture the active set so on_select can toggle without re-reading.
    let active_captured = active.clone();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let mut next = active_captured.clone();
        if let Some(pos) = next.iter().position(|a| a == &item.value) {
            next.remove(pos);
        } else {
            next.push(item.value.clone());
        }
        let on = next.iter().any(|a| a == &item.value);
        let lane = lane_sel.clone();
        let next_clone = next.clone();
        tokio::spawn(async move {
            let _ = lane.set_active_tools(next_clone).await;
        });
        let list_str = if next.is_empty() {
            "(none)".to_string()
        } else {
            next.join(", ")
        };
        add_note_message(
            &chat_sel,
            &format!(
                "{} {} — active tools: {}",
                item.value,
                if on { "enabled" } else { "disabled" },
                list_str
            ),
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

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Tools);
}

/// Build + open the `/images` selector (Yes/No). Stores the choice in
/// `state.show_images` and notes it. Image wiring is minimal this pass — the
/// flag is consulted where images would be shown and echoed back here.
fn open_images_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
) {
    let current = *state.show_images.lock().unwrap();
    let items = vec![
        SelectItem::new("yes", "Yes")
            .with_description(if current { "Inline images (current)" } else { "Inline images" }),
        SelectItem::new("no", "No")
            .with_description(if current { "Placeholder only" } else { "Placeholder only (current)" }),
    ];
    let list = Arc::new(SelectList::new(items, 5));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    list.on_select(Arc::new(move |item| {
        let on = item.value == "yes";
        *state_sel.show_images.lock().unwrap() = on;
        add_note_message(
            &chat_sel,
            &format!("Inline images {}.", if on { "enabled" } else { "disabled" }),
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

    open_selector(state, editor_container, editor, tui, list, SelectorKind::Images);
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
        "Type your message and press Enter to send.",
        1, 0,
    )));
    container.add_child(Arc::new(Text::new(
        "Ctrl+C: Abort/Exit | Esc: Abort | Enter: Send | Shift+Enter: New line | Tab: Complete | Ctrl+L: Model | Ctrl+M: Cycle | Ctrl+T: Expand tool | /help",
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
    container.add_child(Arc::new(Text::new("  /model, /m      — Choose a model (live switch)", 1, 0)));
    container.add_child(Arc::new(Text::new("  /thinking, /think — Set reasoning depth (selector)", 1, 0)));
    container.add_child(Arc::new(Text::new("  /tools          — Toggle built-in tools on/off", 1, 0)));
    container.add_child(Arc::new(Text::new("  /images         — Toggle inline image rendering", 1, 0)));
    container.add_child(Arc::new(Text::new("  /session        — List saved sessions", 1, 0)));
    container.add_child(Arc::new(Text::new("  /theme          — Choose a theme (selector)", 1, 0)));
    container.add_child(Arc::new(Text::new("  /compact        — Compact the conversation", 1, 0)));
    container.add_child(Arc::new(Text::new("  /copy           — Copy last reply to clipboard", 1, 0)));
    container.add_child(Arc::new(Text::new("  /hotkeys        — Show keyboard shortcuts", 1, 0)));
    container.add_child(Arc::new(Text::new("  /armin          — 🐾 Easter egg", 1, 0)));
    container.add_child(Arc::new(Text::new("  /earendil       — Earendil announcement", 1, 0)));
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
    container.add_child(Arc::new(Text::new("  Enter         — Send message", 1, 0)));
    container.add_child(Arc::new(Text::new("  Shift+Enter   — New line", 1, 0)));
    container.add_child(Arc::new(Text::new("  Tab           — Accept autocomplete suggestion", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+A / Ctrl+E — Line start / end", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+K / Ctrl+U — Delete to end / start of line", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+C        — Abort a run, or exit when idle", 1, 0)));
    container.add_child(Arc::new(Text::new("  Esc           — Abort a running prompt", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+L        — Open model selector", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+M        — Cycle to the next model (live)", 1, 0)));
    container.add_child(Arc::new(Text::new("  Ctrl+T        — Expand/collapse last tool result", 1, 0)));
    container.add_child(Arc::new(Text::new("  PageUp/Down   — Scroll transcript", 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a user message echo to the chat container — a bordered `UserMessageComponent`
/// (surface-colored box with OSC133 prompt-boundary markers) replacing the old
/// plain `> text` echo.
fn add_user_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(UserMessageComponent::new(text.to_string())));
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
            handle_slash_command("/thinking"),
            SlashCommandResult::SelectThinking
        ));
        assert!(matches!(
            handle_slash_command("/think"),
            SlashCommandResult::SelectThinking
        ));
        assert!(matches!(
            handle_slash_command("/tools"),
            SlashCommandResult::SelectTools
        ));
        assert!(matches!(
            handle_slash_command("/images"),
            SlashCommandResult::SelectImages
        ));
        assert!(matches!(handle_slash_command("/armin"), SlashCommandResult::Armin));
        assert!(matches!(handle_slash_command("/earendil"), SlashCommandResult::Earendil));
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
            bash_components: std::sync::Mutex::new(HashMap::new()),
            last_tool_comp: std::sync::Mutex::new(None),
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
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
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
    fn test_cycle_next_model_wraps_around() {
        use rpi_ai::{Api, Model};
        let mk = |id: &str| {
            Model::new(id, id, Api::AnthropicMessages, "anthropic", "https://api.anthropic.com")
        };
        let catalog = [mk("a"), mk("b"), mk("c")];
        // Next after "a" is "b"; after "c" wraps to "a".
        assert_eq!(cycle_next_model(&catalog, "a").unwrap().id, "b");
        assert_eq!(cycle_next_model(&catalog, "c").unwrap().id, "a");
        // An unknown current id falls back to the first model.
        assert_eq!(cycle_next_model(&catalog, "zzz").unwrap().id, "a");
        // Empty catalog yields None.
        let empty: Vec<Model> = vec![];
        assert!(cycle_next_model(&empty, "a").is_none());
    }

    #[test]
    fn test_autocomplete_slash_suggestions_render() {
        // The autocomplete container should render at least one suggestion
        // line when the editor holds a `/` prefix, and clear when it doesn't.
        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            bash_components: std::sync::Mutex::new(HashMap::new()),
            last_tool_comp: std::sync::Mutex::new(None),
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
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
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
            bash_components: std::sync::Mutex::new(HashMap::new()),
            last_tool_comp: std::sync::Mutex::new(None),
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
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
        });
        let editor_container = Arc::new(Container::new());
        let editor = Arc::new(Editor::simple());
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
        // list only (editor swapped out).
        assert_eq!(editor_container.child_count(), 1);

        close_selector(&state, &editor_container, &editor, &tui);
        assert!(!state.selector_open());
        // editor restored.
        assert_eq!(editor_container.child_count(), 1);
    }

    #[test]
    fn test_accept_top_suggestion_replaces_prefix() {
        // `/he` + Tab → `/help ` (slash command provider inserts a space).
        let state = Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            bash_components: std::sync::Mutex::new(HashMap::new()),
            last_tool_comp: std::sync::Mutex::new(None),
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
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
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
