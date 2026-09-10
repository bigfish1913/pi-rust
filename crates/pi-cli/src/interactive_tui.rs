//! Interactive mode for pi-cli.
//!
//! Full-screen terminal UI with a streaming transcript, an editor, a live
//! status indicator, and tool-execution display. Mirrors the TypeScript
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` event→UI
//! mapping (`handleEvent`), driven by the live `AgentEvent` stream the harness
//! emits via the `BroadcastEmitter` installed in [`crate::session`].
//!
//! Key architecture facts (see `docs/tui-gap-analysis.md`):
//! - `TuiAltScreen::start()` still has a readerless companion, so this module
//!   owns a `spawn_blocking` crossterm `read()` loop for key dispatch and a
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

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::{broadcast, mpsc};

use rpi_agent::{AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, Content, UserMessage};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::types::{Entry, EntryOrder, EntryQuery};
use rpi_tui::scroll_view::{OverscrollMode, ScrollbarMode};
#[cfg(test)]
use rpi_tui::strip_ansi;
use rpi_tui::{
    apply_theme_preset, render_diff, AssistantBlock, AssistantMessageComponent,
    AssistantMessageOptions, AutocompleteManager, AutocompleteSuggestions, BashExecutionComponent,
    BashTruncation, CombinedAutocompleteProvider, Container, DynamicBorder, Editor, EditorOptions,
    EditorStyle, FilePathAutocompleteProvider, Focusable, FollowMode, FooterComponent, Loader,
    ProcessTerminal, ScrollView, ScrollViewOptions, SelectItem, SelectList,
    SlashCommand as SlashCommandEntry, SlashCommandAutocompleteProvider, Spacer, StackChild,
    StackEntry, Text, ThemeManager, ThemePreset, ToolExecutionComponent, TuiAltScreen,
    UserMessageComponent, VStack, TUI,
};
use rpi_tui::{bold as tui_bold, theme as current_theme};

#[allow(unused_imports)]
use rpi_tui::BashStatus;

use crate::args::Args;

/// B5e: the markdown-transformer trait object the assistant-message render path
/// applies to raw text BEFORE the [`Markdown`] renderer styles it. A plain
/// `Fn(&str) -> String` (NO `rpi-extensions` types) so `rpi-tui` stays free of
/// an `rpi-extensions` dep — `rpi-cli` (which already depends on
/// `rpi-extensions`) builds the closure from the live `RegistrySnapshot` and
/// hands the trait object to `AssistantMessageComponent::set_markdown_transformer`.
type MarkdownTransformer = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// B5e: build the `AssistantMessageComponent` markdown-transformer closure the
/// render path applies to raw assistant text before styling. Wraps any plugin
/// `register_markdown_transformer` handlers registered in `snapshot` (chained
/// in registration order: each handler's output feeds the next). `None` when
/// no markdown transformers are registered (the component defaults to the
/// identity transform + this avoids a closure allocation on the hot render
/// path).
///
/// The closure captures an `Arc<RegistrySnapshot>` clone so it outlives the
/// borrow that built it (the snapshot's `active` flag guards dispatch in
/// `emit_resources_discover`/event translation; a reloaded session's old
/// snapshot flips false, so a stale closure no-ops rather than driving a
/// half-swapped registry — the transformer falls back to the input unchanged
/// on an inactive snapshot, matching the plugin's per-handler skip-on-error).
///
/// This is the cycle-free seam: `rpi-tui` takes a `Fn(&str) -> String` trait
/// object (no `rpi-extensions` dep); `rpi-cli` (which already depends on
/// `rpi-extensions`) builds the closure from the live `RegistrySnapshot`. The
/// calling pattern mirrors `plugin_stub_smoke.rs`'s direct `RenderFn` round-
/// trip (input `{"markdown":…}` → `render_fn` → reclaim `out` via the plugin's
/// `free_string` → parse `{"markdown":…}`).
fn build_markdown_transformer(
    snapshot: Option<std::sync::Arc<rpi_extensions::RegistrySnapshot>>,
) -> Option<MarkdownTransformer> {
    let snapshot = snapshot?;
    // Pre-check: if no markdown renderers are registered, return None so the
    // component uses the identity path (no per-delta closure call). The
    // renderers list is a per-call `renderers_of` clone; snapshotting it once
    // here keeps the closure cheap on the hot path.
    let renderers = snapshot.renderers_of(rpi_extensions::RegisteredRendererKind::Markdown);
    if renderers.is_empty() {
        return None;
    }
    Some(Arc::new(move |raw: &str| -> String {
        transform_markdown_chain(&snapshot, &renderers, raw)
    }))
}

/// Drive the markdown-transformer chain for one input string. Each registered
/// handler receives the previous handler's output (or the raw input for the
/// first), as a `{"markdown": <text>}` JSON envelope; its `RenderFn` returns
/// `{"markdown": <transformed>}` (rc=0) or an error (rc!=0). On any failure —
/// nonzero rc, a panic across the FFI (caught), a missing `markdown` field, or
/// an inactive snapshot — the chain short-circuits to the current text
/// unchanged (per-handler skip-on-error, mirroring pi's `runner.ts` fan-out).
fn transform_markdown_chain(
    snapshot: &rpi_extensions::RegistrySnapshot,
    renderers: &[rpi_extensions::RegisteredRenderer],
    raw: &str,
) -> String {
    // A stale snapshot (post-/reload) must not drive a swapped-out registry.
    // The renderers were captured from this snapshot; if it has gone inactive,
    // fall back to the raw input so the UI never renders stale-transformed text
    // from a dead plugin.
    if !snapshot.is_active() {
        return raw.to_string();
    }

    let mut current = raw.to_string();
    for renderer in renderers {
        let input = match serde_json::to_string(&serde_json::json!({ "markdown": current })) {
            Ok(s) => s,
            Err(_) => return current, // serialize failure — keep current, stop chain
        };
        // SAFETY: `render_fn` is a plugin-provided `extern "C" fn` over a
        // borrowed `StbStringRef` + an out-param. The plugin warrants
        // `poll`/`render` are non-blocking + thread-safe (the same contract
        // the tool adapter relies on). `user_data` is the plugin's opaque
        // pointer, stable for the registry lifetime (the keepalive keeps the
        // cdylib mapped). We reclaim `out` via the plugin's `free_string`
        // exactly once. The whole call is `catch_unwind`-wrapped — a plugin
        // panic must not unwind across the FFI boundary (same policy as the
        // tool partial cb + the runtime_action trampoline).
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = rpi_plugin_sdk::StbString::empty();
            let rc = (renderer.render_fn)(
                rpi_plugin_sdk::StbStringRef::from_str(&input),
                &mut out as *mut rpi_plugin_sdk::StbString,
                renderer.user_data,
            );
            let text = if rc == 0 {
                let s = out.to_string_lossy();
                Some(s)
            } else {
                None
            };
            // Reclaim the plugin-owned `out` regardless of rc (rc!=0 may still
            // have written an error JSON the plugin allocated). `free_with` is
            // idempotent on an empty `StbString`.
            out.free_with(Some(renderer.plugin_free_string));
            text
        }));
        let out_text = match outcome {
            Ok(Some(s)) => s,
            Ok(None) => return current, // rc != 0 — skip this handler, keep current
            Err(_) => return current,   // panic — skip, keep current (do not abort: the
                                         // render path is not the action trampoline; a panicking transformer
                                         // degrades to identity rather than killing the process. Logged via
                                         // the `tracing` crate's panic hook.)
        };
        // Parse `{"markdown": <text>}`; lenient — a missing/non-string field
        // keeps the current text (skip this handler).
        let next = serde_json::from_str::<serde_json::Value>(&out_text)
            .ok()
            .and_then(|v| {
                v.get("markdown")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or(current);
        current = next;
    }
    current
}

// ===========================================================================
// Slash commands — trait + registry
// ===========================================================================
//
// Each built-in slash command is one `impl SlashCommand`. The commands are
// registered at startup into a [`CommandRegistry`] (one source of truth) that
// serves both dispatch ("given this token, run the command") and autocomplete
// ("list the visible commands"). This replaces the old two-list + sync-test
// arrangement, where `handle_slash_command` and `v1_slash_commands()` had to be
// kept in lock-step by hand.
//
// `execute` runs on the blocking key/compose thread (the editor `on_submit`
// callback and the Ctrl+L hotkey both land there), so it MUST stay synchronous:
//   - commands needing async (`set_model`/`set_thinking_level`/`set_active_tools`)
//     `tokio::spawn` the work and return immediately;
//   - commands needing the main async loop (`compact`/`copy`/`exit`/`clear`/
//     `user-input`) signal it via `ctx.tx.send(TuiMessage::…)`;
//   - everything else mutates the chat container + requests a render directly.

/// The borrowed world a slash command runs against. All fields are `Arc` (or a
/// cheap `String` snapshot), so one `CommandContext` clones freely into each
/// command without per-capture ceremony — this struct is exactly the set of
/// `*_for_cb` clones the old submit closure used to make individually.
#[derive(Clone)]
struct CommandContext {
    chat: Arc<Container>,
    tui: Arc<TuiAltScreen>,
    tx: mpsc::UnboundedSender<TuiMessage>,
    state: Arc<TuiState>,
    editor: Arc<Editor>,
    editor_container: Arc<Container>,
    lane: Arc<dyn AgentLane>,
    model_catalog: Arc<Vec<rpi_ai::Model>>,
    /// Lane model id snapshot, read once via `lane.get_model().await` BEFORE the
    /// blocking key loop starts. Selectors/key loop can't await, so they read
    /// this owned string instead. Semantically unchanged from pre-refactor.
    lane_model_id: String,
    cwd: std::path::PathBuf,
    /// Harness resources snapshot (skills + prompt templates) for `/context`.
    /// Captured once at TUI startup because the blocking submit thread can't
    /// `.await get_resources()`.
    resources: Arc<rpi_harness::types::AgentHarnessResources>,
    /// B5d: the reload context `/reload` drives. `Arc<ReloadContext>` so the
    /// blocking submit thread can cheaply clone it into the `ReloadCommand`
    /// without an `.await` (the command can't drive reload directly — it signals
    /// the main loop via `TuiMessage::ReloadExtensions`, which awaits the shared
    /// `reload_extension_resources` routine on the async runtime).
    reload_context: Arc<crate::session::ReloadContext>,
}

/// One slash command.
trait SlashCommand: Send + Sync {
    /// Canonical name, with the leading `/` (e.g. "/model").
    fn name(&self) -> &str;
    /// Aliases, also `/`-prefixed. Matched alongside `name()` during dispatch.
    /// Use [`SlashCommand::alias_visible`] to also surface an alias in the
    /// `/`-autocomplete list (most aliases stay hidden).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    /// Whether the canonical name appears in the `/` autocomplete list. Hidden
    /// commands (`/context`, `/name`, …) return `false`.
    fn visible(&self) -> bool {
        true
    }
    /// Aliases that should also appear in the `/` autocomplete list. Defaults to
    /// none — most aliases (`/q`, `/m`, `/think`, `/resume`, `/v`) are kept off
    /// the list to keep it short. `/new` and `/quit` override this to surface.
    fn alias_visible(&self) -> &'static [&'static str] {
        &[]
    }
    /// Description shown in autocomplete and `/help`. A non-empty description is
    /// required to surface in autocomplete even when `visible()` is true.
    fn description(&self) -> &'static str {
        ""
    }
    fn description_owned(&self) -> String {
        self.description().to_string()
    }
    /// Execute the command. Only invoked for inputs starting with `/` whose
    /// first token matches `name()` or an alias. `args` is the whitespace-
    /// trimmed remainder after the command token ("" when none). Must stay
    /// synchronous (see the module-level note) — async work goes through
    /// `ctx.tx.send(TuiMessage::…)` or `tokio::spawn`.
    fn execute(&self, ctx: &CommandContext, args: &str);
}

/// Holds all registered slash commands; the single source of truth for both
/// dispatch and the autocomplete list.
struct CommandRegistry {
    commands: Vec<Arc<dyn SlashCommand>>,
}

impl CommandRegistry {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    fn register(&mut self, cmd: Arc<dyn SlashCommand>) {
        self.commands.push(cmd);
    }

    /// Find the command whose `name()` or an alias matches `token` (e.g. "/q").
    /// `token` is the first whitespace-delimited word of the input, `/`-prefixed.
    fn find(&self, token: &str) -> Option<&Arc<dyn SlashCommand>> {
        self.commands
            .iter()
            .find(|c| c.name() == token || c.aliases().contains(&token))
    }

    /// The autocomplete entries, derived from the registry so it can never drift
    /// from what dispatch recognizes. Surfaces the canonical name when
    /// `visible()` + non-empty description, plus any `alias_visible()` entries.
    /// Order = registration order; built-ins are registered before templates,
    /// so they win on a fuzzy tie (unchanged).
    fn visible_entries(&self) -> Vec<SlashCommandEntry> {
        let mut out: Vec<SlashCommandEntry> = Vec::new();
        for c in &self.commands {
            let description = c.description_owned();
            if c.visible() && !description.is_empty() {
                out.push(SlashCommandEntry {
                    name: c.name().into(),
                    description: description.clone(),
                });
            }
            // Surfaced aliases share the command's description.
            for alias in c.alias_visible() {
                out.push(SlashCommandEntry {
                    name: (*alias).into(),
                    description: description.clone(),
                });
            }
        }
        out
    }
}

/// Resolve the command for a `/`-prefixed input and run it, or emit the
/// unknown-command error if nothing matches. Non-slash text never reaches here
/// — callers route only `/`-prefixed inputs and send plain text directly.
fn dispatch_slash(text: &str, ctx: &CommandContext, registry: &CommandRegistry) {
    let mut parts = text.split_whitespace();
    let token = parts.next().unwrap_or("");
    let args = parts.collect::<Vec<_>>().join(" ");
    match registry.find(token) {
        Some(cmd) => cmd.execute(ctx, &args),
        None => {
            add_error_message(
                &ctx.chat,
                &format!("Unknown command: {text}. Type /help for available commands."),
            );
            ctx.tui.request_render(false);
        }
    }
}

/// A slash command registered by a native extension. The command metadata is
/// captured for autocomplete, while the handler is looked up from the live
/// session on every invocation so `/reload` takes effect without rebuilding
/// the editor callback.
struct ExtensionCommand {
    name: String,
    description: String,
    session: crate::session::ExtensionSessionCell,
}

impl SlashCommand for ExtensionCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &'static str {
        "extension command"
    }

    fn description_owned(&self) -> String {
        self.description.clone()
    }

    fn execute(&self, ctx: &CommandContext, args: &str) {
        let result = invoke_extension_command(&self.session, &self.name, args);
        handle_extension_ui_result(result, ctx, self.session.clone(), self.name.clone());
    }
}

fn invoke_extension_command(
    session: &crate::session::ExtensionSessionCell,
    name: &str,
    args: &str,
) -> Option<serde_json::Value> {
    let command = session
        .lock()
        .ok()
        .and_then(|s| s.snapshot_arc())
        .and_then(|snap| {
            snap.commands()
                .iter()
                .find(|c| c.name.trim_start_matches('/') == name.trim_start_matches('/'))
                .cloned()
        })?;
    let input = serde_json::json!({ "args": args, "command": name });
    let input = serde_json::to_string(&input).ok()?;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = rpi_plugin_sdk::StbString::empty();
        let rc = (command.handler)(
            rpi_plugin_sdk::StbStringRef::from_str(&input),
            &mut out as *mut rpi_plugin_sdk::StbString,
            command.user_data,
        );
        let text = if rc == 0 {
            Some(out.to_string_lossy())
        } else {
            None
        };
        rpi_extensions::host_free_string(out);
        text
    }))
    .ok()
    .flatten()?;
    serde_json::from_str(&outcome).ok()
}

fn handle_extension_ui_result(
    result: Option<serde_json::Value>,
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
) {
    let Some(value) = result else {
        add_error_message(&ctx.chat, "Extension command failed.");
        ctx.tui.request_render(false);
        return;
    };
    match value.get("kind").and_then(|v| v.as_str()) {
        Some("message") | None => {
            let fallback = value.to_string();
            let text = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or(&fallback)
                .to_string();
            if !text.is_empty() {
                add_note_message(&ctx.chat, &text);
            }
            ctx.tui.request_render(false);
        }
        Some("selector") => open_extension_selector(ctx, session, command_name, value),
        Some("editor") => open_extension_editor(ctx, session, command_name, value),
        Some(other) => {
            add_error_message(&ctx.chat, &format!("Unsupported extension UI: {other}"));
            ctx.tui.request_render(false);
        }
    }
}

fn open_extension_selector(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let items = value
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let value = item.get("value")?.as_str()?;
                    let label = item.get("label").and_then(|v| v.as_str()).unwrap_or(value);
                    let mut out = SelectItem::new(value, label);
                    if let Some(desc) = item.get("description").and_then(|v| v.as_str()) {
                        out = out.with_description(desc);
                    }
                    Some(out)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if items.is_empty() {
        add_error_message(&ctx.chat, "Extension selector has no items.");
        ctx.tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));
    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let session_select = session.clone();
    let command_select = command_name.clone();
    let ctx_select = ctx.clone();
    list.on_select(Arc::new(move |item| {
        let args = serde_json::json!({ "action": "select", "value": item.value });
        let result = invoke_extension_command(
            &session_select,
            &command_select,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_selector(&state, &ec, &editor, &tui);
        handle_extension_ui_result(
            result,
            &ctx_select,
            session_select.clone(),
            command_select.clone(),
        );
    }));
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    open_selector(
        &ctx.state,
        &ctx.editor_container,
        &ctx.editor,
        &ctx.tui,
        list,
        SelectorKind::Extension,
    );
}

fn open_extension_editor(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let initial = value
        .get("initialText")
        .or_else(|| value.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 0,
            placeholder: value
                .get("placeholder")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            initial_text: Some(initial),
        },
        EditorStyle {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        },
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);
    *ctx.state.active_extension_editor.lock().unwrap() = Some(editor.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(editor.clone());

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let session_submit = session.clone();
    let command_submit = command_name.clone();
    let ctx_submit = ctx.clone();
    editor.on_submit(Arc::new(move |text| {
        let args = serde_json::json!({ "action": "edit", "text": text });
        let result = invoke_extension_command(
            &session_submit,
            &command_submit,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state, &ec, &original);
        handle_extension_ui_result(
            result,
            &ctx_submit,
            session_submit.clone(),
            command_submit.clone(),
        );
    }));
    ctx.tui.set_focus(Some(editor));
    ctx.tui.request_render(false);
}

fn close_extension_editor(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
) {
    editor_container.clear();
    editor_container.add_child(editor.clone());
    *state.active_extension_editor.lock().unwrap() = None;
    editor.set_focused(true);
}

// ---- Built-in command implementations ----

struct HelpCommand;
impl SlashCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "/help"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/?"]
    }
    fn description(&self) -> &'static str {
        "Show available commands"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_help_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

struct ClearChatCommand;
impl SlashCommand for ClearChatCommand {
    fn name(&self) -> &'static str {
        "/clear"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/new"]
    }
    // `/new` carries its own weight as a discoverable entry, so surface it.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/new"]
    }
    fn description(&self) -> &'static str {
        "Clear the conversation"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ClearChat);
    }
}

struct ExitCommand;
impl SlashCommand for ExitCommand {
    fn name(&self) -> &'static str {
        "/exit"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/quit", "/q"]
    }
    // `/quit` is surfaced (matches pi's BUILTIN list); `/q` stays a hidden alias.
    fn alias_visible(&self) -> &'static [&'static str] {
        &["/quit"]
    }
    fn description(&self) -> &'static str {
        "Exit the application"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Exit);
    }
}

struct VersionCommand;
impl SlashCommand for VersionCommand {
    fn name(&self) -> &'static str {
        "/version"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/v"]
    }
    fn description(&self) -> &'static str {
        "Show version information"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_version_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

struct HotkeysCommand;
impl SlashCommand for HotkeysCommand {
    fn name(&self) -> &'static str {
        "/hotkeys"
    }
    fn description(&self) -> &'static str {
        "Show keyboard shortcuts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_hotkeys_message(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

struct ModelCommand;
impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "/model"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/m"]
    }
    fn description(&self) -> &'static str {
        "Choose a model (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let term = args.trim();
        if !term.is_empty() {
            // /model <name> — direct switch by id (pi handleModelCommand).
            let Some(model) = ctx
                .model_catalog
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(term))
                .cloned()
            else {
                add_error_message(
                    &ctx.chat,
                    &format!("No model matches \"{term}\". Try /model for the list."),
                );
                ctx.tui.request_render(false);
                return;
            };
            let model_id = model.id.clone();
            ctx.state.set_current_model(&model);
            let lane = ctx.lane.clone();
            tokio::spawn(async move {
                let _ = lane.set_model(model).await;
            });
            add_note_message(
                &ctx.chat,
                &format!(
                    "Model set to {} — applies to the next message.",
                    short_model_name(&model_id)
                ),
            );
            ctx.tui.request_render(false);
            return;
        }
        open_model_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.model_catalog,
            &ctx.lane,
            &ctx.lane_model_id,
            &ctx.chat,
        );
    }
}

struct ThinkingCommand;
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &'static str {
        "/thinking"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/think"]
    }
    fn description(&self) -> &'static str {
        "Set thinking level (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let level_name = args.trim();
        if !level_name.is_empty() {
            // /thinking <level> — direct set (pi supports the param form).
            let Some(level) = thinking_level_from_name(level_name) else {
                add_error_message(
                    &ctx.chat,
                    &format!(
                        "Unknown thinking level \"{level_name}\". Valid: {}",
                        crate::args::VALID_THINKING_LEVELS.join(", ")
                    ),
                );
                ctx.tui.request_render(false);
                return;
            };
            let lane = ctx.lane.clone();
            let footer = ctx.state.footer.clone();
            tokio::spawn(async move {
                let _ = lane.set_thinking_level(level).await;
            });
            footer.set_thinking_level(Some(thinking_level_name(level)));
            add_note_message(&ctx.chat, &format!("Thinking set to {level_name}."));
            ctx.tui.request_render(false);
            return;
        }
        open_thinking_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog,
            &ctx.lane_model_id,
            &ctx.chat,
        );
    }
}

struct ToolsCommand;
impl SlashCommand for ToolsCommand {
    fn name(&self) -> &'static str {
        "/tools"
    }
    fn description(&self) -> &'static str {
        "Toggle tools on/off"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_tools_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.chat,
        );
    }
}

struct ImagesCommand;
impl SlashCommand for ImagesCommand {
    fn name(&self) -> &'static str {
        "/images"
    }
    fn description(&self) -> &'static str {
        "Toggle inline images"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_images_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.chat,
        );
    }
}

struct SessionCommand;
impl SlashCommand for SessionCommand {
    fn name(&self) -> &'static str {
        "/session"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["/resume"]
    }
    fn description(&self) -> &'static str {
        "List saved sessions"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_session_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.cwd,
            &ctx.tx,
        );
    }
}

struct ThemeCommand;
impl SlashCommand for ThemeCommand {
    fn name(&self) -> &'static str {
        "/theme"
    }
    fn description(&self) -> &'static str {
        "Choose a theme (selector)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let name = args.trim().to_ascii_lowercase();
        if !name.is_empty() {
            // /theme <name> — direct apply + persist (matches /settings Theme).
            let preset = match name.as_str() {
                "light" => ThemePreset::Light,
                "monochrome" => ThemePreset::Monochrome,
                "dark" => ThemePreset::Dark,
                _ => {
                    add_error_message(
                        &ctx.chat,
                        &format!("Unknown theme \"{name}\". Valid: dark, light, monochrome."),
                    );
                    ctx.tui.request_render(false);
                    return;
                }
            };
            apply_theme_preset(preset);
            let mut settings = crate::settings::load_settings().unwrap_or_default();
            settings.theme = Some(name.clone());
            let _ = crate::settings::save_settings(&settings);
            add_note_message(&ctx.chat, &format!("Theme set to {name} (saved)."));
            ctx.tui.request_render(false);
            ctx.tui.render_now(true);
            return;
        }
        open_theme_selector(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
    }
}

struct CompactCommand;
impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "/compact"
    }
    fn description(&self) -> &'static str {
        "Compact the conversation"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Compact);
    }
}

struct CopyCommand;
impl SlashCommand for CopyCommand {
    fn name(&self) -> &'static str {
        "/copy"
    }
    fn description(&self) -> &'static str {
        "Copy last reply to clipboard"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::Copy);
    }
}

struct ExportCommand;
impl SlashCommand for ExportCommand {
    fn name(&self) -> &'static str {
        "/export"
    }
    fn description(&self) -> &'static str {
        "Export session to a markdown file"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ExportSession);
    }
}

struct ForkCommand;
impl SlashCommand for ForkCommand {
    fn name(&self) -> &'static str {
        "/fork"
    }
    fn description(&self) -> &'static str {
        "Fork the session into a new one"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

/// `/clone` is the native Pi spelling for duplicating the current session.
/// Reuse the same durable fork path as `/fork`; both create a child session
/// and rebind the live harness to it.
struct CloneCommand;
impl SlashCommand for CloneCommand {
    fn name(&self) -> &'static str {
        "/clone"
    }
    fn description(&self) -> &'static str {
        "Duplicate the current session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ForkSession);
    }
}

struct TreeCommand;
impl SlashCommand for TreeCommand {
    fn name(&self) -> &'static str {
        "/tree"
    }
    fn description(&self) -> &'static str {
        "Navigate the current session tree"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::OpenTree);
    }
}

struct LoginCommand;
impl SlashCommand for LoginCommand {
    fn name(&self) -> &'static str {
        "/login"
    }
    fn description(&self) -> &'static str {
        "Save an Anthropic API key"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let key = args.trim();
        if key.is_empty() {
            add_note_message(&ctx.chat, "Usage: /login <api-key>");
        } else {
            let result = crate::config::upsert_credential(
                "anthropic",
                crate::config::Credential::ApiKey {
                    key: Some(key.to_string()),
                    env: None,
                },
            );
            match result {
                Ok(()) => add_note_message(&ctx.chat, "Saved Anthropic credentials."),
                Err(error) => {
                    add_error_message(&ctx.chat, &format!("Could not save credentials: {error}"))
                }
            }
        }
        ctx.tui.request_render(false);
    }
}

struct LogoutCommand;
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &'static str {
        "/logout"
    }
    fn description(&self) -> &'static str {
        "Remove saved Anthropic credentials"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        match crate::config::delete_credential("anthropic") {
            Ok(true) => add_note_message(&ctx.chat, "Removed saved Anthropic credentials."),
            Ok(false) => add_note_message(&ctx.chat, "No saved Anthropic credentials found."),
            Err(error) => {
                add_error_message(&ctx.chat, &format!("Could not remove credentials: {error}"))
            }
        }
        ctx.tui.request_render(false);
    }
}

struct TrustCommand;
impl SlashCommand for TrustCommand {
    fn name(&self) -> &'static str {
        "/trust"
    }
    fn description(&self) -> &'static str {
        "Trust the current project"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let value = match args.trim().to_ascii_lowercase().as_str() {
            "" | "yes" | "y" | "true" => Some(true),
            "no" | "n" | "false" => Some(false),
            "clear" | "reset" | "none" => None,
            _ => {
                add_note_message(&ctx.chat, "Usage: /trust [yes|no|clear]");
                ctx.tui.request_render(false);
                return;
            }
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match crate::config::set_project_trust(&cwd, value) {
            Ok(()) => {
                let label = match value {
                    Some(true) => "trusted",
                    Some(false) => "untrusted",
                    None => "trust decision cleared",
                };
                add_note_message(&ctx.chat, &format!("Current project marked {label}."));
            }
            Err(error) => add_error_message(
                &ctx.chat,
                &format!("Could not save trust decision: {error}"),
            ),
        }
        ctx.tui.request_render(false);
    }
}

struct NameCommand;
impl SlashCommand for NameCommand {
    fn name(&self) -> &'static str {
        "/name"
    }
    fn description(&self) -> &'static str {
        "Set session display name"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            add_note_message(
                &ctx.chat,
                "Usage: /name <display name> — sets the current session's name.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::SetSessionName(name.to_string()));
    }
}

struct ImportCommand;
impl SlashCommand for ImportCommand {
    fn name(&self) -> &'static str {
        "/import"
    }
    fn description(&self) -> &'static str {
        "Import a session file (path)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let path = args.trim();
        if path.is_empty() {
            add_note_message(
                &ctx.chat,
                "Usage: /import <path-to-session.jsonl> — copies the file into the session dir and switches to it.",
            );
            ctx.tui.request_render(false);
            return;
        }
        let _ = ctx.tx.send(TuiMessage::ImportSession(path.to_string()));
    }
}

struct SettingsCommand;
impl SlashCommand for SettingsCommand {
    fn name(&self) -> &'static str {
        "/settings"
    }
    fn description(&self) -> &'static str {
        "Open settings menu"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_settings_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.lane,
            &ctx.model_catalog,
            &ctx.lane_model_id,
            &ctx.chat,
        );
    }
}

struct ScopedModelsCommand;
impl SlashCommand for ScopedModelsCommand {
    fn name(&self) -> &'static str {
        "/scoped-models"
    }
    fn description(&self) -> &'static str {
        "Choose models for Ctrl+M cycling"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        open_scoped_models_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.model_catalog,
            &ctx.chat,
        );
    }
}

struct ShareCommand;
impl SlashCommand for ShareCommand {
    fn name(&self) -> &'static str {
        "/share"
    }
    fn description(&self) -> &'static str {
        "Share session (gist via gh, or clipboard)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShareSession);
    }
}

struct ArminCommand;
impl SlashCommand for ArminCommand {
    fn name(&self) -> &'static str {
        "/armin"
    }
    fn description(&self) -> &'static str {
        "??? (easter egg)"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        crate::extras::add_armin(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

struct EarendilCommand;
impl SlashCommand for EarendilCommand {
    fn name(&self) -> &'static str {
        "/earendil"
    }
    fn description(&self) -> &'static str {
        "Announcement"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        crate::extras::add_earendil(&ctx.chat);
        ctx.tui.request_render(false);
    }
}

/// `/context` — lists discovered context files, skills, and prompt templates.
/// Hidden from autocomplete (needs the resources snapshot to be meaningful as a
/// discovery surface; like `/name`, it's recognized-v1 but kept off the list).
struct ContextCommand;
impl SlashCommand for ContextCommand {
    fn name(&self) -> &'static str {
        "/context"
    }
    fn visible(&self) -> bool {
        false
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        show_context_panel(&ctx.chat, &ctx.resources);
        ctx.tui.request_render(false);
    }
}

/// `/reload` — re-run extension + resource discovery into the LIVE harness
/// (B5d): reload the cdylib plugins, invalidate the old `ActionBridge` +
/// registry snapshot, rebuild skills/prompts/context/SYSTEM.md/APPEND_SYSTEM.md
/// + the `TeeEmitter`, and push the rebuilt state via the B5d harness setters.
/// The command itself runs on the blocking submit thread, so it can't drive
/// the async `reload_extension_resources` routine directly — it signals the main
/// loop via `TuiMessage::ReloadExtensions`, which awaits it on the async runtime.
/// (A plugin's `runtime_action(Reload)` signals the same loop via the
/// `ReloadMailbox` the TUI installs — the B5d async-reload design avoids the
/// self-unmapping race a synchronous plugin-initiated reload would have.)
struct ReloadCommand;
impl SlashCommand for ReloadCommand {
    fn name(&self) -> &'static str {
        "/reload"
    }
    fn description(&self) -> &'static str {
        "Reload extensions, skills, prompts"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        // Signal the main loop. It owns the `&AgentHarness` borrow the
        // `reload_extension_resources` routine needs (the blocking submit thread
        // only has the context's `Arc<ReloadContext>` + the `Arc<dyn AgentLane>`).
        add_note_message(&ctx.chat, "Reloading extensions + resources…");
        ctx.tui.request_render(false);
        let _ = ctx.tx.send(TuiMessage::ReloadExtensions);
    }
}

/// Build the full command registry: active built-ins first (so they win on a
/// fuzzy autocomplete tie), then the v1-out-of-scope stubs. Prompt-template
/// commands are merged in separately by the autocomplete builder (they dispatch
/// via template expansion, not this registry).
fn build_builtin_registry() -> CommandRegistry {
    let mut r = CommandRegistry::new();
    r.register(Arc::new(HelpCommand));
    r.register(Arc::new(ClearChatCommand));
    r.register(Arc::new(ExitCommand));
    r.register(Arc::new(VersionCommand));
    r.register(Arc::new(ModelCommand));
    r.register(Arc::new(ThinkingCommand));
    r.register(Arc::new(ToolsCommand));
    r.register(Arc::new(ImagesCommand));
    r.register(Arc::new(SessionCommand));
    r.register(Arc::new(ThemeCommand));
    r.register(Arc::new(CompactCommand));
    r.register(Arc::new(CopyCommand));
    r.register(Arc::new(HotkeysCommand));
    r.register(Arc::new(ArminCommand));
    r.register(Arc::new(EarendilCommand));
    r.register(Arc::new(ContextCommand));
    // Recognized but inert in v1 (one struct backs them all). The TS builtins
    // out of v1 scope; each carries a description so autocomplete surfaces its
    // existence even though running it reports "not supported".
    r.register(Arc::new(NameCommand));
    r.register(Arc::new(SettingsCommand));
    r.register(Arc::new(ScopedModelsCommand));
    r.register(Arc::new(ExportCommand));
    r.register(Arc::new(ImportCommand));
    r.register(Arc::new(ShareCommand));
    r.register(Arc::new(ForkCommand));
    r.register(Arc::new(CloneCommand));
    r.register(Arc::new(TreeCommand));
    r.register(Arc::new(TrustCommand));
    r.register(Arc::new(LoginCommand));
    r.register(Arc::new(LogoutCommand));
    r.register(Arc::new(ReloadCommand));
    r
}

fn register_extension_commands(
    registry: &mut CommandRegistry,
    session: crate::session::ExtensionSessionCell,
) {
    let commands = session
        .lock()
        .ok()
        .and_then(|s| s.snapshot_arc())
        .map(|snap| snap.commands().to_vec())
        .unwrap_or_default();
    for command in commands {
        let name = if command.name.starts_with('/') {
            command.name.clone()
        } else {
            format!("/{}", command.name)
        };
        if registry.find(&name).is_some() {
            continue;
        }
        registry.register(Arc::new(ExtensionCommand {
            name,
            description: command.description,
            session: session.clone(),
        }));
    }
}

// ===========================================================================
// Channel + helpers
// ===========================================================================

/// Message type for communication between the key/callback threads and the
/// main async loop.
enum TuiMessage {
    UserInput(String),
    OpenTree,
    NavigateTree(String),
    Exit,
    /// Clear the transcript (from `/clear`).
    ClearChat,
    /// Compact the conversation (from `/compact`).
    Compact,
    /// Copy the last assistant reply to the clipboard (from `/copy`).
    Copy,
    /// Hot-switch to another saved session (from the `/session` selector):
    /// the payload is the session id the selector's item value carried.
    SwitchSession(String),
    /// Export the current session to a markdown file (from `/export`).
    ExportSession,
    /// Fork the current session into a new one and switch to it (from `/fork`).
    ForkSession,
    /// Rename the current session (from `/name <name>`).
    SetSessionName(String),
    /// Import a JSONL session file into the session dir and switch to it
    /// (from `/import <path>`).
    ImportSession(String),
    /// Share the current session (`/share`): `gh gist create` when the gh CLI
    /// is available, otherwise copy the transcript to the clipboard.
    ShareSession,
    /// `/reload` — re-run extension + resource discovery into the live harness
    /// (B5d). The command (and a plugin's `runtime_action(Reload)` via the
    /// mailbox) signal the main loop, which awaits
    /// `reload_extension_resources` on the async runtime.
    ReloadExtensions,
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

/// The user message's text (Text content or the text blocks of a Blocks
/// payload — images are skipped, consistent with the v1 text-only prompt path).
fn user_message_text(msg: &rpi_ai::types::UserMessage) -> String {
    match &msg.content {
        rpi_ai::types::UserContent::Text(s) => s.clone(),
        rpi_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                Content::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
    }
}

/// Render the `/settings` panel: the saved settings.json values the session
/// honors, plus pointers to the commands that edit them (theme via `/theme`,
/// defaults via flags, cycle scope via `/scoped-models`). Kept for the
/// read-only summary; the interactive menu is [`open_settings_selector`].
fn show_settings_panel(chat: &Arc<Container>) {
    let s = crate::settings::load_settings().unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    lines.push("⚙️  Saved settings:".into());
    lines.push(format!(
        "  Theme: {} (edit with /theme)",
        s.theme.as_deref().unwrap_or("(default)")
    ));
    lines.push(format!(
        "  Default model: {} (set at launch with --model)",
        s.default_model.as_deref().unwrap_or("(none)")
    ));
    lines.push(format!(
        "  Default thinking: {} (set at launch with --thinking)",
        s.default_thinking_level.as_deref().unwrap_or("(default)")
    ));
    match &s.scoped_models {
        Some(list) if !list.is_empty() => lines.push(format!(
            "  Ctrl+M cycle scope: {} (edit with /scoped-models)",
            list.join(", ")
        )),
        _ => lines.push("  Ctrl+M cycle scope: all models (edit with /scoped-models)".into()),
    }
    let body = lines.join("\n");
    container_note_block(chat, &body);
}

/// The catalog allowed in the Ctrl+M cycle: the `/scoped-models` set from
/// settings.json when present, otherwise every model. The current model is
/// always included (fallback) so cycling can never strand the user off-scope.
fn scoped_catalog(catalog: &[rpi_ai::Model], current_id: &str) -> Vec<rpi_ai::Model> {
    let scoped = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    if scoped.is_empty() {
        return catalog.to_vec();
    }
    let mut out: Vec<rpi_ai::Model> = catalog
        .iter()
        .filter(|m| scoped.iter().any(|s| s.eq_ignore_ascii_case(&m.id)))
        .cloned()
        .collect();
    // Never strand the user: if the current model isn't in scope, keep it.
    if !out.iter().any(|m| m.id.eq_ignore_ascii_case(current_id)) {
        if let Some(cur) = catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(current_id))
        {
            out.push(cur.clone());
        }
    }
    out
}

/// Interactive `/settings` menu: a top-level selector over the editable
/// settings, each opening a sub-selector that applies the choice AND persists
/// it to settings.json (theme / default model / default thinking / cycle
/// scope). Selecting a menu item swaps the current selector for the
/// sub-selector (the `active_selector` slot is single, so each open replaces
/// the previous list); the sub-selector's cancel restores the editor.
fn open_settings_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
    let settings = crate::settings::load_settings().unwrap_or_default();
    let mut items: Vec<SelectItem> = Vec::new();
    items.push(
        SelectItem::new("theme", "Theme")
            .with_description(&settings.theme.clone().unwrap_or_else(|| "(default)".into())),
    );
    items.push(
        SelectItem::new("model", "Default model").with_description(
            &settings
                .default_model
                .clone()
                .unwrap_or_else(|| "(none)".into()),
        ),
    );
    items.push(
        SelectItem::new("thinking", "Default thinking").with_description(
            &settings
                .default_thinking_level
                .clone()
                .unwrap_or_else(|| "(default)".into()),
        ),
    );
    let scope_desc = match &settings.scoped_models {
        Some(list) if !list.is_empty() => format!("{}", list.join(", ")),
        _ => "all models".to_string(),
    };
    items
        .push(SelectItem::new("scoped-models", "Ctrl+M cycle scope").with_description(&scope_desc));
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let lane_sel = lane.clone();
    let chat_sel = chat.clone();
    let catalog_sel = catalog.to_vec();
    let lane_model_sel = lane_model_id.to_string();
    list.on_select(Arc::new(move |item| {
        // Swap this menu for the sub-selector; each sub-selector saves its
        // choice to settings.json on select.
        match item.value.as_str() {
            "theme" => {
                open_settings_theme_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel, &chat_sel)
            }
            "model" => open_settings_model_selector(
                &state_sel,
                &ec_sel,
                &editor_sel,
                &tui_sel,
                &lane_sel,
                &catalog_sel,
                &lane_model_sel,
                &chat_sel,
            ),
            "thinking" => open_settings_thinking_selector(
                &state_sel,
                &ec_sel,
                &editor_sel,
                &tui_sel,
                &lane_sel,
                &catalog_sel,
                &lane_model_sel,
                &chat_sel,
            ),
            "scoped-models" => open_scoped_models_selector(
                &state_sel,
                &ec_sel,
                &editor_sel,
                &tui_sel,
                &catalog_sel,
                &chat_sel,
            ),
            _ => close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel),
        }
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// Apply a theme choice AND persist it to settings.json (`/settings` → Theme).
fn open_settings_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
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
    let chat_sel = chat.clone();
    list.on_select(Arc::new(move |item| {
        let preset = match item.value.as_str() {
            "light" => ThemePreset::Light,
            "monochrome" => ThemePreset::Monochrome,
            _ => ThemePreset::Dark,
        };
        apply_theme_preset(preset);
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.theme = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Theme set to {} (saved{})",
                item.label,
                if saved.is_ok() { "" } else { ", not saved" },
            ),
        );
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// Choose the default model AND persist it (`/settings` → Default model):
/// applies live via `lane.set_model` and saves `defaultModel` to settings.json
/// (which `provider::resolve` honors as pi's `findInitialModel` step 3).
fn open_settings_model_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
    let mut items: Vec<SelectItem> = Vec::new();
    for m in catalog {
        let label = if m.name.is_empty() {
            short_model_name(&m.id)
        } else {
            m.name.clone()
        };
        let marker = if m.id.eq_ignore_ascii_case(lane_model_id) {
            " (current)"
        } else {
            ""
        };
        items.push(
            SelectItem::new(&m.id, &label).with_description(&format!("{id}{marker}", id = m.id)),
        );
    }
    if items.is_empty() {
        add_note_message(chat, "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 10));

    let catalog_arc = catalog.to_vec();
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let lane_sel = lane.clone();
    list.on_select(Arc::new(move |item| {
        let Some(model) = catalog_arc.iter().find(|m| m.id == item.value).cloned() else {
            add_note_message(&chat_sel, &format!("Model {} not found.", item.label));
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        state_sel.set_current_model(&model);
        let lane = lane_sel.clone();
        tokio::spawn(async move {
            let _ = lane.set_model(model).await;
        });
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.default_model = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Default model set to {} (saved{}",
                short_model_name(&item.value),
                if saved.is_ok() { ")" } else { ", not saved)" },
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// Choose the default thinking level AND persist it (`/settings` → Default
/// thinking): applies live via `lane.set_thinking_level` and saves
/// `defaultThinkingLevel` to settings.json.
fn open_settings_thinking_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
) {
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
        items.push(SelectItem::new(name, name).with_description(thinking_level_description(*lvl)));
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
            add_note_message(
                &chat_sel,
                &format!("Unknown thinking level: {}.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        let footer_sel = state_sel.footer.clone();
        tokio::spawn(async move {
            let _ = lane.set_thinking_level(level).await;
        });
        footer_sel.set_thinking_level(Some(thinking_level_name(level)));
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.default_thinking_level = Some(item.value.clone());
        let saved = crate::settings::save_settings(&settings);
        add_note_message(
            &chat_sel,
            &format!(
                "Default thinking set to {} (saved{}",
                item.label,
                if saved.is_ok() { ")" } else { ", not saved)" },
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Settings,
    );
}

/// `/scoped-models`: a multi-toggle selector over the catalog. Selecting an
/// item toggles it in the in-progress set (the selector stays open); Esc saves
/// the set to settings.json and closes. The active scoped set is echoed after
/// each toggle so the user sees the current selection.
fn open_scoped_models_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    catalog: &[rpi_ai::Model],
    chat: &Arc<Container>,
) {
    if catalog.is_empty() {
        add_note_message(chat, "No models in the catalog.");
        tui.request_render(false);
        return;
    }
    // Seed the edit set from the saved scoped models.
    let seed: Vec<String> = crate::settings::load_settings()
        .ok()
        .and_then(|s| s.scoped_models)
        .unwrap_or_default();
    *state.scoped_edit.lock().unwrap() = Some(seed);

    let mut items: Vec<SelectItem> = Vec::new();
    for m in catalog {
        items.push(SelectItem::new(&m.id, &m.id));
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let chat_sel = chat.clone();
    let tui_sel = tui.clone();
    list.on_select(Arc::new(move |item| {
        // Toggle the model in the in-progress set; the selector stays open.
        let mut set = state_sel.scoped_edit.lock().unwrap();
        let set = set.get_or_insert_with(Vec::new);
        if let Some(pos) = set.iter().position(|m| m.eq_ignore_ascii_case(&item.value)) {
            set.remove(pos);
            add_note_message(&chat_sel, &format!("{} removed — Esc to save", item.label));
        } else {
            set.push(item.value.clone());
            add_note_message(&chat_sel, &format!("{} added — Esc to save", item.label));
        }
        tui_sel.request_render(false);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    let chat_cancel = chat.clone();
    list.on_cancel(Arc::new(move || {
        // Save the edited set to settings.json and close.
        let set = state_cancel
            .scoped_edit
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        settings.scoped_models = if set.is_empty() {
            None
        } else {
            Some(set.clone())
        };
        match crate::settings::save_settings(&settings) {
            Ok(()) => {
                if set.is_empty() {
                    add_note_message(&chat_cancel, "Ctrl+M cycles all models (scope cleared).");
                } else {
                    add_note_message(
                        &chat_cancel,
                        &format!("Ctrl+M cycle scope: {}", set.join(", ")),
                    );
                }
            }
            Err(e) => add_error_message(&chat_cancel, &format!("Could not save settings: {e}")),
        }
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::ScopedModels,
    );
}

/// `/share`: mirror the TS intent (share the session). With the `gh` CLI on
/// PATH, create a gist of the exported markdown; otherwise fall back to the
/// clipboard (best-effort) and note the local path.
async fn share_session(harness: &AgentHarness, chat: &Arc<Container>) {
    use std::process::Stdio;

    // Reuse the export builder for the transcript text.
    let tree = harness.session().view("main");
    let entries = match tree
        .find_entries(&EntryQuery {
            entry_type: None,
            custom_type: None,
            // Exports append entries top-to-bottom, so use chronological order
            // instead of the session query default (newest-first).
            order: Some(EntryOrder::OldestFirst),
            limit: None,
            cursor: None,
        })
        .await
    {
        Ok(e) => e,
        Err(e) => {
            add_error_message(chat, &format!("Could not read session: {e}"));
            return;
        }
    };
    let mut md = String::from("# Session\n\n");
    for e in entries {
        let Entry::Message(me) = e else { continue };
        match &me.message {
            AgentMessage::User(u) => {
                md.push_str(&format!("## User\n\n{}\n\n", user_message_text(u)));
            }
            AgentMessage::Assistant(a) => {
                let text = assistant_text(a);
                if !text.is_empty() {
                    md.push_str(&format!("## Assistant\n\n{}\n\n", text));
                }
            }
            _ => {}
        }
    }

    // `gh gist create` — stdin-piped, best-effort; only when gh exists.
    let gh = std::process::Command::new("gh")
        .arg("gist")
        .arg("create")
        .arg("--filename")
        .arg("session.md")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = gh {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(md.as_bytes());
            let _ = stdin.flush();
        }
        let out = child.wait_with_output().ok();
        if let Some(out) = out {
            if out.status.success() {
                let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                add_note_message(chat, &format!("Shared session: {url}"));
                return;
            }
        }
        add_note_message(chat, "gh gist failed — falling back to the clipboard.");
    } else {
        add_note_message(chat, "gh CLI not found — falling back to the clipboard.");
    }
    // Clipboard fallback (or transcript echo when the clipboard feature is off).
    if copy_to_clipboard(&md) {
        add_note_message(chat, "Session transcript copied to the clipboard.");
    } else {
        add_note_message(
            chat,
            "Clipboard unavailable — use /export to write the transcript to a file.",
        );
    }
}

/// Export the current session to a markdown transcript file. Writes
/// `<cwd>/<session-name-or-id>.md` with the user/assistant/tool-call history
/// (mirrors the TS `/export` intent locally — no remote sharing in v1).
/// Best-effort: failures surface as a chat note.
/// Export the current session to a markdown transcript file. Writes
/// `<cwd>/<session-name-or-id>.md` with the user/assistant/tool-call history
/// (mirrors the TS `/export` intent locally — no remote sharing in v1).
/// Best-effort: failures surface as a chat note.
async fn export_session(harness: &AgentHarness, chat: &Arc<Container>) {
    let tree = harness.session().view("main");
    let entries = match tree
        .find_entries(&EntryQuery {
            entry_type: None,
            custom_type: None,
            // Keep exported entries in the same chronological order shown in
            // the transcript; the storage default is newest-first.
            order: Some(EntryOrder::OldestFirst),
            limit: None,
            cursor: None,
        })
        .await
    {
        Ok(e) => e,
        Err(e) => {
            add_error_message(chat, &format!("Could not read session: {e}"));
            return;
        }
    };
    let name = tree.get_name().await.ok().flatten().unwrap_or_default();
    let id = tree
        .get_leaf_id()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "session".to_string());
    let mut md = String::from("# Session\n\n");
    for e in entries {
        let Entry::Message(me) = e else { continue };
        match &me.message {
            AgentMessage::User(u) => {
                md.push_str(&format!("## User\n\n{}\n\n", user_message_text(u)));
            }
            AgentMessage::Assistant(a) => {
                let text = assistant_text(a);
                if !text.is_empty() {
                    md.push_str(&format!("## Assistant\n\n{}\n\n", text));
                }
            }
            _ => {}
        }
    }
    let file_name = if name.is_empty() {
        format!("{id}.md")
    } else {
        format!("{name}.md")
    };
    let path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(&file_name);
    match std::fs::write(&path, md) {
        Ok(_) => add_note_message(chat, &format!("Exported session to {}", path.display())),
        Err(e) => add_error_message(chat, &format!("Could not write export: {e}")),
    }
}

/// Fork the current session into a new JSONL session and switch to it (TS
/// `/fork` — a copy of the transcript in a fresh file; the fork is a new
/// session the user continues in). Uses the repo's `fork_typed`, then swaps
/// the harness backing and renders the (empty-ish) fork transcript.
/// Hot-switch the harness to another saved session: abort any in-flight run,
/// open the target session file, swap the durable backing, and re-render the
/// transcript from the new history (mirrors pi's `/session` resume-in-place).
/// Shared by the `/session` selector, `/import`, and `/fork`. The current
/// model/footer stay put (v1 doesn't replay the session's ModelChange entries).
async fn switch_to_session(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    id: &str,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
) -> bool {
    if *state.status.lock().unwrap() == RunStatus::Working {
        state.set_status(RunStatus::Aborting);
        let _ = lane.abort().await;
    }
    let cwd_str = cwd.to_string_lossy().to_string();
    match crate::session::open_session_by_id(id, &cwd_str).await {
        Ok(new_session) => {
            let _ = harness.set_session(new_session).await;
            chat.clear();
            add_welcome_message(chat);
            render_session_history(
                harness,
                chat,
                state.markdown_transformer(),
                Some(state.extension_session.clone()),
            )
            .await;
            state.set_status(RunStatus::Idle);
            add_note_message(chat, &format!("Switched to session {id}."));
            true
        }
        Err(e) => {
            state.set_status(RunStatus::Idle);
            add_error_message(chat, &format!("Could not open session {id}: {e}"));
            false
        }
    }
}

/// `/import <path>`: copy a JSONL session file into the default session dir,
/// then hot-switch to it (the file name becomes its id — matching the
/// selector/`open_session_by_id` containment rules).
async fn import_session(
    harness: &AgentHarness,
    lane: &Arc<dyn AgentLane>,
    path: &str,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
) {
    use std::path::Path as FsPath;

    let src = FsPath::new(path);
    if !src.is_file() {
        add_error_message(chat, &format!("Import source not found: {path}"));
        return;
    }
    let Some(fname) = src.file_name().and_then(|f| f.to_str()) else {
        add_error_message(chat, "Import source has no file name.");
        return;
    };
    if !fname.ends_with(".jsonl") {
        add_error_message(chat, "Import source must be a .jsonl session file.");
        return;
    }
    let dir = crate::session::default_session_dir(cwd);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        add_error_message(chat, &format!("Could not create session dir: {e}"));
        return;
    }
    let dest = dir.join(fname);
    match std::fs::copy(src, &dest) {
        Ok(_) => {
            let id = fname.strip_suffix(".jsonl").unwrap_or(fname).to_string();
            if switch_to_session(harness, lane, &id, cwd, chat, state).await {
                add_note_message(chat, &format!("Imported session from {path}"));
            }
        }
        Err(e) => add_error_message(chat, &format!("Could not copy import: {e}")),
    }
}

async fn fork_session(
    harness: &AgentHarness,
    cwd: &std::path::Path,
    chat: &Arc<Container>,
    state: &Arc<TuiState>,
) {
    use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    use rpi_tools::FileSystem;

    let cwd_str = cwd.to_string_lossy().to_string();
    let dir = crate::session::default_session_dir(cwd);
    let env = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(cwd.to_path_buf()));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(rpi_harness::session::memory::SystemClock),
        ids: Arc::new(rpi_harness::session::session::DefaultIdGenerator::new()),
    });
    // The fork needs the rich JSONL metadata (with the on-disk path); resolve
    // it from the session list by the current session's id.
    let id = harness.session().storage().metadata().id.clone();
    let metas = match crate::session::list_session_metadata(&cwd_str).await {
        Ok(m) => m,
        Err(e) => {
            add_error_message(chat, &format!("Could not list sessions: {e}"));
            return;
        }
    };
    let Some(source) = metas.iter().find(|m| m.id == id) else {
        add_error_message(chat, &format!("Current session {id} not found on disk."));
        return;
    };
    let fork_storage = match repo
        .fork_typed(
            source,
            &rpi_harness::session::jsonl::JsonlSessionCreateOptions {
                id: None,
                parent_session_id: Some(source.id.clone()),
                cwd: cwd_str.clone(),
                metadata: None,
            },
            &rpi_harness::session::types::ForkOptions::default(),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            add_error_message(chat, &format!("Could not fork session: {e}"));
            return;
        }
    };
    let new_session = rpi_harness::session::session::Session::new(Arc::new(fork_storage), None);
    let _ = harness.set_session(new_session).await;
    chat.clear();
    add_welcome_message(chat);
    render_session_history(
        harness,
        chat,
        state.markdown_transformer(),
        Some(state.extension_session.clone()),
    )
    .await;
    state.set_status(RunStatus::Idle);
    add_note_message(chat, "Forked into a new session.");
}

/// Render the restored session's prior transcript (user + assistant messages)
/// into the chat container. Called at TUI startup for `--continue`/`--resume`/
/// `--session` launches; a no-op for fresh sessions (no entries). Best-effort:
/// any session read failure just starts with an empty transcript.
///
/// `transformer` is the live assistant-markdown transformer (B5e); `None` is
/// the identity path. Each restored assistant component installs it so replayed
/// history renders through the same `register_markdown_transformer` handlers
/// the live stream does.
async fn render_session_history(
    harness: &AgentHarness,
    chat: &Arc<Container>,
    transformer: Option<MarkdownTransformer>,
    extension_session: Option<crate::session::ExtensionSessionCell>,
) {
    let tree = harness.session().view("main");
    let entries = match tree
        .find_entries(&EntryQuery {
            entry_type: None,
            custom_type: None,
            // Session queries default to newest-first for selectors and
            // pagination. The transcript appends children top-to-bottom, so
            // restored history must explicitly be chronological.
            order: Some(EntryOrder::OldestFirst),
            limit: None,
            cursor: None,
        })
        .await
    {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut rendered_any = false;
    for e in entries {
        match e {
            Entry::Message(me) => match &me.message {
                AgentMessage::User(u) => {
                    add_user_message(chat, &user_message_text(u));
                    rendered_any = true;
                }
                AgentMessage::Assistant(a) => {
                    let comp = Arc::new(AssistantMessageComponent::new(
                        AssistantMessageOptions::default(),
                    ));
                    if let Some(t) = &transformer {
                        comp.set_markdown_transformer(Some(t.clone()));
                    }
                    comp.update_blocks(&assistant_blocks(a));
                    chat.add_child(comp);
                    // Single trailing spacer: the next transcript entry (user or
                    // assistant) follows one blank line below.
                    chat.add_child(Arc::new(Spacer::new(1)));
                    if let Some(text) = extension_usage_text(extension_session.as_ref(), &a.usage) {
                        add_note_message(chat, &text);
                    }
                    rendered_any = true;
                }
                AgentMessage::Custom(custom) => {
                    if let Some(session) = &extension_session {
                        if let Some(component) = extension_message_component(
                            session,
                            &custom.role,
                            &serde_json::json!({
                                "customType": custom.role,
                                "content": custom.content,
                                "details": custom.data,
                            }),
                            transformer.clone(),
                        ) {
                            chat.add_child(component);
                            chat.add_child(Arc::new(Spacer::new(1)));
                            rendered_any = true;
                            continue;
                        }
                    }
                    add_note_message(chat, &custom_message_fallback(&custom));
                    rendered_any = true;
                }
                _ => {}
            },
            Entry::Compaction(compaction) => {
                add_note_message(
                    chat,
                    &format!(
                        "Compacted {} tokens: {}",
                        compaction.tokens_before, compaction.summary
                    ),
                );
                rendered_any = true;
            }
            Entry::BranchSummary(summary) => {
                add_note_message(chat, &format!("Branch summary: {}", summary.summary));
                rendered_any = true;
            }
            Entry::Custom(custom) => {
                let rendered = extension_session.as_ref().and_then(|session| {
                    extension_entry_component(session, &custom.custom_type, custom.data.clone())
                });
                if let Some(component) = rendered {
                    chat.add_child(component);
                    chat.add_child(Arc::new(Spacer::new(1)));
                    rendered_any = true;
                } else if let Some(text) =
                    custom_entry_display_text(&custom.custom_type, custom.data.as_ref())
                {
                    add_note_message(chat, &text);
                    rendered_any = true;
                }
            }
            Entry::ModelChange(change) => {
                add_note_message(
                    chat,
                    &format!("Model changed to {}:{}", change.provider, change.model_id),
                );
                rendered_any = true;
            }
            Entry::ThinkingLevel(change) => {
                add_note_message(
                    chat,
                    &format!("Thinking level: {:?}", change.thinking_level),
                );
                rendered_any = true;
            }
            Entry::ActiveTools(change) => {
                add_note_message(
                    chat,
                    &format!("Active tools: {}", change.active_tool_names.join(", ")),
                );
                rendered_any = true;
            }
        }
    }
    if rendered_any {
        // No trailing spacer here — each entry already adds its own trailing
        // Spacer(1), so an extra would double the bottom gap.
    }
}

fn invoke_extension_renderer(
    session: &crate::session::ExtensionSessionCell,
    kind: rpi_extensions::RegisteredRendererKind,
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let snapshot = session.lock().ok()?.snapshot_arc()?;
    let input = serde_json::to_string(payload).ok()?;
    for renderer in snapshot.renderers_of(kind) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = rpi_plugin_sdk::StbString::empty();
            let rc = (renderer.render_fn)(
                rpi_plugin_sdk::StbStringRef::from_str(&input),
                &mut out as *mut rpi_plugin_sdk::StbString,
                renderer.user_data,
            );
            let text = if rc == 0 {
                Some(out.to_string_lossy())
            } else {
                None
            };
            out.free_with(Some(renderer.plugin_free_string));
            text
        }))
        .ok()
        .flatten();
        let Some(text) = outcome else { continue };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            return Some(value);
        }
    }
    None
}

fn extension_text_component(value: &serde_json::Value) -> Option<Arc<dyn rpi_tui::Component>> {
    if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
        let text = lines
            .iter()
            .filter_map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return Some(Arc::new(Text::new(text, 0, 0)));
    }
    let text = value.get("text").and_then(|v| v.as_str())?;
    if value.get("markdown").and_then(|v| v.as_bool()) == Some(true) {
        let component = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        component.update_blocks(&[AssistantBlock::Text(text.to_string())]);
        Some(component)
    } else {
        Some(Arc::new(Text::new(text, 0, 0)))
    }
}

fn extension_message_component(
    session: &crate::session::ExtensionSessionCell,
    custom_type: &str,
    payload: &serde_json::Value,
    transformer: Option<MarkdownTransformer>,
) -> Option<Arc<dyn rpi_tui::Component>> {
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Message,
        payload,
    )?;
    if value.get("markdown").and_then(|v| v.as_bool()) == Some(true) {
        let text = value.get("text").and_then(|v| v.as_str())?;
        let component = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        if let Some(transformer) = transformer {
            component.set_markdown_transformer(Some(transformer));
        }
        component.update_blocks(&[AssistantBlock::Text(text.to_string())]);
        return Some(component);
    }
    extension_text_component(&value)
        .or_else(|| Some(Arc::new(Text::new(format!("[{custom_type}]"), 0, 0))))
}

/// Render usage from a completed assistant message through the registered
/// message renderers. Hosts without a token-usage renderer return `None`.
fn extension_usage_text(
    session: Option<&crate::session::ExtensionSessionCell>,
    usage: &rpi_ai::types::Usage,
) -> Option<String> {
    let session = session?;
    let payload = serde_json::json!({
        "customType": "token-usage",
        "usage": usage,
    });
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Message,
        &payload,
    )?;
    value
        .get("text")
        .and_then(|value| value.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn extension_entry_component(
    session: &crate::session::ExtensionSessionCell,
    custom_type: &str,
    data: Option<serde_json::Value>,
) -> Option<Arc<dyn rpi_tui::Component>> {
    let payload = serde_json::json!({
        "customType": custom_type,
        "data": data,
    });
    let value = invoke_extension_renderer(
        session,
        rpi_extensions::RegisteredRendererKind::Entry,
        &payload,
    )?;
    extension_text_component(&value)
}

/// Project an assistant message's content into the provider-free
/// [`AssistantBlock`] list (text, thinking, and decoded image blocks, in
/// document order) the `AssistantMessageComponent` renders. Tool-call blocks
/// are rendered by their own components in the transcript.
/// Whether startup intentionally opened a session that already has history.
fn launch_restores_history(args: &Args) -> bool {
    args.continue_session
        || args.resume
        || args.session.is_some()
        || args.session_id.is_some()
        || args.fork.is_some()
}

fn assistant_blocks(msg: &AssistantMessage) -> Vec<AssistantBlock> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(AssistantBlock::Text(t.text.clone())),
            Content::Thinking(t) => Some(AssistantBlock::Thinking(t.thinking.clone())),
            Content::Image(image) => base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .ok()
                .filter(|data| !data.is_empty())
                .map(AssistantBlock::Image),
            _ => None,
        })
        .collect()
}

fn custom_message_fallback(custom: &rpi_agent::CustomMessage) -> String {
    let content = custom
        .content
        .iter()
        .filter_map(|item| match item {
            Content::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if content.is_empty() {
        format!("{}: {}", custom.role, custom.data)
    } else {
        format!("{}: {}", custom.role, content)
    }
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
    /// `/session` — browse and switch saved JSONL sessions.
    Session,
    /// `/theme` — dark / light / monochrome presets applied live.
    Theme,
    /// `/scoped-models` — multi-toggle Ctrl+M cycle scope.
    ScopedModels,
    /// `/settings` — interactive settings menu (and its sub-selectors).
    Settings,
    /// `/tree` — navigate to an existing entry in the current session.
    Tree,
    /// Extension-provided selector; uses the same keyboard contract.
    Extension,
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
    /// Extension-provided editor currently occupying the input slot.
    active_extension_editor: std::sync::Mutex<Option<Arc<Editor>>>,
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
    /// Submitted-message history for ↑/↓ recall, most recent first (mirrors
    /// the TS editor `history` array). Bounded at [`HISTORY_LIMIT`].
    history: std::sync::Mutex<Vec<String>>,
    /// Browse index while recalling history: -1 = not browsing, 0 = most
    /// recent, 1 = older, … Reset to -1 on every submit.
    history_index: std::sync::Mutex<isize>,
    /// The editor text captured when entering browse mode, restored when the
    /// user navigates back past the newest entry (TS `historyDraft`).
    history_draft: std::sync::Mutex<Option<String>>,
    /// The previous turn's input token count, used by the cache-miss notice:
    /// a large input that reads nothing from cache after an established prefix
    /// means the prefix was re-billed (simplified `maybeShowCacheMissNotice`).
    last_input_tokens: std::sync::Mutex<i64>,
    /// The in-progress scoped-models selection while the `/scoped-models`
    /// selector is open (toggle per item, Esc saves). `None` when not editing.
    scoped_edit: std::sync::Mutex<Option<Vec<String>>>,
    /// B5e: the live assistant-markdown transformer, built from the current
    /// `RegistrySnapshot`'s `register_markdown_transformer` handlers. `None`
    /// when no markdown transformers are registered (identity render path).
    /// Swapped on `/reload` (a fresh snapshot ⇒ a fresh closure; the old
    /// closure no-ops once its snapshot's `active` flag flips false) and
    /// re-installed on the in-flight `current_assistant` so a reloaded plugin's
    /// transform takes effect on the visible streaming message immediately.
    /// New assistant components pick up whatever closure is current at
    /// construction time via [`install_markdown_transformer`].
    markdown_transformer: std::sync::Mutex<Option<MarkdownTransformer>>,
    /// Live extension registry used by message/entry renderer dispatch.
    extension_session: crate::session::ExtensionSessionCell,
}

/// How many submitted messages are kept for ↑ recall (mirrors the TS
/// editor's 100-entry cap).
const HISTORY_LIMIT: usize = 100;

/// A turn with at least this many input tokens is worth a cache-miss notice
/// when nothing was read from cache (matches the TS 20k threshold).
const CACHE_MISS_MIN_INPUT_TOKENS: i64 = 20_000;

/// Keep a few rows of overlap so page scrolling preserves visual context,
/// matching the upstream fullscreen viewport behavior.
const PAGE_SCROLL_OVERLAP: usize = 4;

/// Native pi scrolls a small chunk for each wheel notch rather than moving the
/// transcript one physical row at a time. Three lines stays precise while
/// avoiding the sluggish feel of the previous implementation.
const MOUSE_WHEEL_SCROLL_LINES: i32 = 3;

fn transcript_page_size(viewport_height: usize) -> i32 {
    viewport_height
        .saturating_sub(PAGE_SCROLL_OVERLAP)
        .max(1)
        .min(i32::MAX as usize) as i32
}

fn should_dispatch_key(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Release
}

/// Compact token count for the cache-miss notice: 1.2M / 34.5K / 900.
fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Record a submitted message for ↑ recall (mirrors TS `addToHistory`):
/// trims, skips empty + consecutive duplicates, caps at [`HISTORY_LIMIT`], and
/// resets the browse state so a fresh prompt never resumes mid-history.
fn push_history(state: &Arc<TuiState>, text: &str) {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    let mut history = state.history.lock().unwrap();
    if history.first() == Some(&trimmed) {
        return;
    }
    history.insert(0, trimmed);
    history.truncate(HISTORY_LIMIT);
    *state.history_index.lock().unwrap() = -1;
    *state.history_draft.lock().unwrap() = None;
}

/// Navigate message history. `direction` is -1 (↑, older) or 1 (↓, newer).
/// Mirrors TS `navigateHistory`: the first entry into browse mode stashes the
/// current editor text as the draft; navigating back past the newest entry
/// restores that draft.
fn navigate_history(state: &Arc<TuiState>, editor: &Arc<Editor>, direction: i32) {
    let history = state.history.lock().unwrap();
    if history.is_empty() {
        return;
    }
    let mut index = state.history_index.lock().unwrap();
    let new_index = *index - direction as isize;
    if new_index < -1 || new_index >= history.len() as isize {
        return;
    }
    if *index == -1 && new_index >= 0 {
        // Entering browse mode: stash the current input.
        *state.history_draft.lock().unwrap() = Some(editor.get_text());
    }
    *index = new_index;
    if new_index == -1 {
        // Exited browse mode: restore the draft (or clear if there was none).
        let draft = state.history_draft.lock().unwrap().take();
        match draft {
            Some(d) => {
                let len = d.len();
                editor.set_text(&d);
                editor.set_cursor(0, len);
            }
            None => editor.set_text(""),
        }
    } else {
        let text = history[new_index as usize].clone();
        let len = text.len();
        editor.set_text(&text);
        editor.set_cursor(0, len);
    }
}

impl TuiState {
    fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = status;
        self.apply_status(status);
    }

    /// Atomically reserve the single interactive run slot. The editor callback
    /// runs on a different thread from the async prompt loop, so checking and
    /// setting in separate steps would allow rapid Enter presses to queue more
    /// than one operation.
    fn try_start_working(&self) -> bool {
        let mut status = self.status.lock().unwrap();
        if *status != RunStatus::Idle {
            return false;
        }
        *status = RunStatus::Working;
        drop(status);
        self.apply_status(RunStatus::Working);
        true
    }

    fn apply_status(&self, status: RunStatus) {
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
                // Do not leave a frozen "Working" spinner on screen after the
                // render tick intentionally stops advancing in this state.
                self.loader.stop();
                self.status_container.clear();
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

    /// The bash panel has its own `Running...` spinner. Keep the global
    /// `Working...` loader out of the status slot while any bash tool is active
    /// so the same operation is not presented as two simultaneous loaders.
    fn sync_working_loader_with_bash(&self) {
        if *self.status.lock().unwrap() != RunStatus::Working {
            return;
        }

        self.status_container.clear();
        if self.bash_components.lock().unwrap().is_empty() {
            self.status_container.add_child(self.loader.clone());
        }
    }

    /// Whether a selector overlay is currently open (routes keys to it first).
    fn selector_open(&self) -> bool {
        self.active_selector.lock().unwrap().is_some()
    }

    fn extension_editor_open(&self) -> bool {
        self.active_extension_editor.lock().unwrap().is_some()
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

    /// B5e: read a clone of the current assistant-markdown transformer (if any).
    /// New assistant components call this at construction so they render with
    /// whatever plugin `register_markdown_transformer` handlers are live.
    fn markdown_transformer(&self) -> Option<MarkdownTransformer> {
        self.markdown_transformer.lock().unwrap().clone()
    }

    /// B5e: swap the live transformer. Used at startup (install the first
    /// closure built from the initial `RegistrySnapshot`) and on `/reload`
    /// (rebuild from the fresh snapshot). On a reload the reloaded plugin's
    /// transform should take effect on the VISIBLE streaming message too, so
    /// this re-installs on the in-flight `current_assistant` component — its
    /// `set_markdown_transformer` rebuilds the last blocks immediately. A
    /// `None` clears the transform (identity), e.g. a reload that unregisters
    /// every markdown transformer.
    fn set_markdown_transformer_with_reinstall(&self, transformer: Option<MarkdownTransformer>) {
        *self.markdown_transformer.lock().unwrap() = transformer.clone();
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_markdown_transformer(transformer);
        }
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
    theme: Option<&str>,
    reload_context: &crate::session::ReloadContext,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    // Resolve the active model once, up front. The full id feeds the TuiState
    // tracking field + the selectors/key loop (which run on a blocking thread
    // and can't await `lane.get_model()`); the short name feeds the footer.
    let lane_model_id = lane.get_model().await.map(|m| m.id).unwrap_or_default();
    let model_name = short_model_name(&lane_model_id);

    // Snapshot startup capabilities for the welcome screen. Both accessors
    // return defensive clones, so rendering this summary does not retain a
    // harness lock or trigger a second resource scan.
    let active_tool_names = lane.get_active_tools().await.unwrap_or_default();
    let resources_snapshot = harness.get_resources().await.unwrap_or_default();
    let skill_names: Vec<String> = resources_snapshot
        .skills
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|skill| skill.name.clone())
        .collect();

    // The cwd for @file autocomplete + session discovery.
    let cwd = std::env::current_dir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Channel between the key/callback threads and the main async loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<TuiMessage>();

    // Apply the saved theme before constructing transcript components. Some
    // components keep styled text, so doing this after the welcome banner left
    // the first screen in the dark palette until it was rebuilt.
    if let Some(preset) = match theme {
        Some("light") => Some(ThemePreset::Light),
        Some("monochrome") => Some(ThemePreset::Monochrome),
        Some("dark") => Some(ThemePreset::Dark),
        _ => None,
    } {
        apply_theme_preset(preset);
    }

    // ---- TUI + containers ----
    let terminal = Box::new(ProcessTerminal::new());
    let tui = Arc::new(TuiAltScreen::new(terminal, true, None));

    let chat_container = Arc::new(Container::new());
    add_welcome_message_with_capabilities(&chat_container, &active_tool_names, &skill_names);

    // First-launch gate: if `~/.rpi/.setup_done` is absent, show the welcome
    // banner + the earendil announcement once, then write the sentinel. The TS
    // original is a multi-step dialog (theme picker + analytics opt-in); this
    // v1 simplifies to a one-shot banner (theme still pickable via `/theme`,
    // analytics deferred — no telemetry wiring). See `extras.rs`.
    crate::extras::maybe_first_time_setup(&chat_container);

    // A --continue/--resume/--session launch opens on an existing JSONL
    // session — render its prior user/assistant transcript so the user sees
    // where they left off (tool executions are skipped: their live display
    // belongs to the current run, and replaying old results would be noise).
    let initial_transformer = build_markdown_transformer(
        reload_context
            .extension_session
            .lock()
            .unwrap()
            .snapshot_arc(),
    );
    // A normal launch creates a fresh session and must not replay records from
    // another/project harness. Only explicit restore/fork modes render prior
    // conversation history. This fixes stale prompts appearing every startup.
    if launch_restores_history(args) {
        render_session_history(
            &harness,
            &chat_container,
            initial_transformer.clone(),
            Some(reload_context.extension_session.clone()),
        )
        .await;
    }

    // `document_container` wraps the welcome header + chat so the scrollview
    // follows the whole transcript (mirrors TS `documentContainer`).
    let document_container = Arc::new(Container::new());
    document_container.add_child(chat_container.clone());

    let scroll_view = Arc::new(ScrollView::new(
        document_container.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            overscroll: OverscrollMode::Chain,
            // Native pi keeps transcript chrome out of the way. Our Auto mode
            // has no hide timer yet and therefore became effectively permanent
            // after the first wheel event, unlike the upstream experience.
            scrollbar: ScrollbarMode::Hidden,
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
    // Prompt templates discovered at session build (Part A2) are surfaced as
    // `/`-prefixed entries alongside the built-in slash commands: typing
    // `/<name>` in the editor expands the template (mirrors pi
    // `expandPromptTemplate`, `agent-session.ts:1124`). The description carries
    // the template's frontmatter description (or a fallback) so the autocomplete
    // popover shows what each template does.
    //
    // We snapshot the full resources once (skills + prompt-templates): the
    // autocomplete builder consumes the templates, and the `/context` command
    // (fired from the blocking submit handler, which can't `.await`) reads the
    // snapshot to render the discovered-resources panel without touching the
    // harness async accessor.
    let template_slash_commands: Vec<SlashCommandEntry> = resources_snapshot
        .prompt_templates
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|t| SlashCommandEntry {
            name: format!("/{}", t.name),
            description: t
                .description
                .clone()
                .unwrap_or_else(|| "Expand prompt template".to_string()),
        })
        .collect();
    let resources_arc: Arc<rpi_harness::types::AgentHarnessResources> =
        Arc::new(resources_snapshot);
    // Build the built-in command registry once — the single source of truth for
    // both dispatch and the built-in autocomplete entries. The discovered
    // prompt-template commands are merged into the autocomplete list separately
    // (they dispatch via template expansion, not the registry); built-ins come
    // first so they win on a fuzzy tie.
    let mut command_registry = build_builtin_registry();
    register_extension_commands(
        &mut command_registry,
        reload_context.extension_session.clone(),
    );
    let registry = Arc::new(command_registry);
    let mut all_slash_commands = registry.visible_entries();
    all_slash_commands.extend(template_slash_commands);
    let autocomplete = AutocompleteManager::new();
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            all_slash_commands,
        )));
        combined.add_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            cwd.clone(),
        )));
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
        active_extension_editor: std::sync::Mutex::new(None),
        autocomplete,
        autocomplete_container: autocomplete_container.clone(),
        theme_manager: Arc::new(ThemeManager::new()),
        tui: Some(tui.clone()),
        current_model_id: std::sync::Mutex::new(lane_model_id.clone()),
        show_images: std::sync::Mutex::new(true),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        last_input_tokens: std::sync::Mutex::new(0),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: std::sync::Mutex::new(initial_transformer),
        extension_session: reload_context.extension_session.clone(),
    });

    // Capture the model catalog + cwd for the selector builders + the key loop
    // (the callbacks fire on blocking threads and need owned data).
    let model_catalog_arc = Arc::new(model_catalog.clone());
    let lane_model_id = lane.get_model().await.map(|m| m.id).unwrap_or_default();

    // ---- Layout root (built ONCE; mirrors TS fullscreenLayoutRoot) ----
    // root = VStack[ scrollview(basis:0 grow:1 shrink:1 min:1), dock(shrink:1) ]
    // dock  = VStack[ status(auto), autocomplete(auto), editor_container(shrink:0 min:3), footer(auto) ]
    //
    // The scrollview gets `basis(0)` so the constrained stack allocator starts
    // it at zero height and grows it to fill the space the dock does not need
    // — this keeps the dock (editor borders + footer) pinned to the bottom and
    // never shrinks it below the editor's 3 rows (top + content + bottom). The
    // editor_container is `shrink(0).min_size(3)` so a tall transcript can
    // never clip the input panel below its minimum.
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
    //
    // The handler captures one `CommandContext` (the set of `*_for_cb` clones
    // the old version made individually) + the registry, then routes `/`-text
    // through `dispatch_slash` and sends plain text directly. Each command's
    // `execute` owns its own effects (selector open, `tx.send`, `tokio::spawn`,
    // chat mutation) — the handler itself stays a thin router.
    //
    // One `CommandContext` is built and cloned for both the submit handler and
    // the key loop (Ctrl+L routes `/model` through the same registry); all
    // fields are `Arc`/cheap, so the clones are free.
    let ctx = CommandContext {
        chat: chat_container.clone(),
        tui: tui.clone(),
        tx: tx.clone(),
        state: state.clone(),
        editor: editor.clone(),
        editor_container: editor_container.clone(),
        lane: lane.clone(),
        model_catalog: model_catalog_arc.clone(),
        lane_model_id: lane_model_id.clone(),
        cwd: cwd.clone(),
        resources: resources_arc.clone(),
        reload_context: Arc::new(reload_context.clone()),
    };
    let ctx_for_cb = ctx.clone();
    let registry_for_cb = registry.clone();
    editor.on_submit(Arc::new(move |text: &str| {
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        if text.starts_with('/') {
            dispatch_slash(text, &ctx_for_cb, &registry_for_cb);
            return;
        }

        let run_status = *ctx_for_cb.state.status.lock().unwrap();
        if run_status != RunStatus::Idle {
            let message = AgentMessage::User(UserMessage::new(text.to_string(), 0));
            let aborting = run_status == RunStatus::Aborting;
            let lane = ctx_for_cb.lane.clone();
            let chat = ctx_for_cb.chat.clone();
            let tui = ctx_for_cb.tui.clone();
            tokio::spawn(async move {
                // Queue immediately while the agent loop is still running.
                // Routing this through the TUI's main channel delayed it until
                // `prompt_text()` returned, after the loop's drain points had
                // passed, so the queued message appeared to disappear.
                let result = if aborting {
                    lane.next_run(message).await
                } else {
                    lane.steer(message).await
                };
                if let Err(error) = result {
                    add_error_message(&chat, &format!("Could not queue message: {error}"));
                    tui.request_render(false);
                }
            });
            add_note_message(
                &ctx_for_cb.chat,
                &format!("Queued steering message: {text}"),
            );
            ctx_for_cb.tui.request_render(false);
            return;
        }

        if !ctx_for_cb.state.try_start_working() {
            return;
        }

        add_user_message(&ctx_for_cb.chat, text);
        // A new prompt starts a fresh interaction at the tail even when the
        // user had scrolled up to inspect older output.
        if let Some(scroll) = ctx_for_cb.tui.get_primary_scroll_view() {
            scroll.scroll_to_end();
        }
        ctx_for_cb.tui.request_render(false);
        // Remember the message for ↑ recall (slash commands are not part of
        // the replayable message history).
        push_history(&ctx_for_cb.state, text);
        if ctx_for_cb
            .tx
            .send(TuiMessage::UserInput(text.to_string()))
            .is_err()
        {
            ctx_for_cb.state.set_status(RunStatus::Idle);
        }
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

    // ---- B5d: plugin→TUI reload bridge ----
    // A plugin's `runtime_action(Reload)` can't drive the reload synchronously
    // (its cdylib would be unmapped while the call frame is still on the stack).
    // Instead the `ActionBridge`'s reload callback signals `reload_context.mailbox`
    // (an `UnboundedSender<()>`); this task drains those signals and forwards
    // `TuiMessage::ReloadExtensions` into the main loop, which runs the shared
    // `reload_extension_resources` routine asynchronously. The mailbox is the
    // cycle-free seam: rpi-extensions carries only `()` (no `TuiMessage` type —
    // leaf DAG preserved); the TUI owns the receiver + the reload routine.
    let (reload_sig_tx, mut reload_sig_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    reload_context.mailbox.install(reload_sig_tx);
    let reload_tx = tx.clone();
    let reload_bridge_handle = tokio::spawn(async move {
        while reload_sig_rx.recv().await.is_some() {
            if reload_tx.send(TuiMessage::ReloadExtensions).is_err() {
                break; // main loop gone — stop forwarding
            }
        }
    });

    // ---- Render-tick task (advances the loader spinner while Working) ----
    //
    // The `Loader` only advances its frame on render; without a periodic
    // `request_render` the spinner visibly freezes between events.
    let tui_tick = tui.clone();
    let state_tick = state.clone();
    let tick_handle = tokio::spawn(async move {
        // 80ms — pi's loader DEFAULT_INTERVAL_MS (the spinner would visibly
        // stutter at the old 120ms).
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));
        interval.tick().await; // discard immediate
        loop {
            interval.tick().await;
            let working = *state_tick.status.lock().unwrap() == RunStatus::Working;
            if working {
                if state_tick.bash_components.lock().unwrap().is_empty() {
                    // Only the dock loader animates. Keep the already-rendered
                    // transcript instead of rebuilding a long history at 12.5
                    // frames per second.
                    tui_tick.request_render_reusing_scroll_content();
                } else {
                    // A running bash panel owns a loader inside the transcript.
                    tui_tick.request_render(false);
                }
            }
        }
    });

    // ---- Key dispatch loop (spawn_blocking crossterm read) ----
    let running = Arc::new(std::sync::Mutex::new(true));
    let running_key = running.clone();
    let tx_for_key = tx.clone();
    let tui_for_key = tui.clone();
    let editor_for_key = editor.clone();
    let scroll_for_key = scroll_view.clone();
    let lane_for_key = lane.clone();
    let state_for_key = state.clone();
    // Ctrl+L routes through the same registry as `/model` (one path, not two),
    // so the key loop needs the same `CommandContext` + registry the submit
    // handler uses. All fields are `Arc`/cheap, so this clone is free.
    let ctx_for_key = ctx.clone();
    let registry_for_key = registry.clone();

    let key_handle = tokio::task::spawn_blocking(move || {
        loop {
            if !*running_key.lock().unwrap() {
                break;
            }
            // `event::read()` blocks indefinitely. Poll first so shutdown can
            // stop and join this worker even when no further key arrives.
            match crossterm::event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    let _ = tx_for_key.send(TuiMessage::Exit);
                    break;
                }
            }
            let Ok(ev) = crossterm::event::read() else {
                let _ = tx_for_key.send(TuiMessage::Exit);
                break;
            };
            // `Event::Resize` is delivered as its own event (not a Key). With
            // `start_readerless` there is no competing terminal-reader thread to
            // handle it, so refresh the cached terminal size here and force a
            // full redraw so the constrained layout re-fits the new dimensions.
            if let Event::Resize(_cols, _rows) = ev {
                tui_for_key.refresh_size();
                continue;
            }
            // Mouse wheel scrolls the transcript (pi supports wheel
            // scrolling). Previously every non-Key event was dropped, so a
            // wheel had zero effect — "滚动还是不行".
            if let Event::Mouse(m) = ev {
                use crossterm::event::MouseEventKind;
                match m.kind {
                    MouseEventKind::ScrollUp => {
                        let delta = -MOUSE_WHEEL_SCROLL_LINES;
                        if scroll_for_key.scroll_by(delta) != delta {
                            tui_for_key.request_render_reusing_scroll_content();
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let delta = MOUSE_WHEEL_SCROLL_LINES;
                        if scroll_for_key.scroll_by(delta) != delta {
                            tui_for_key.request_render_reusing_scroll_content();
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let Event::Key(key) = ev else {
                continue;
            };
            // Drop releases but preserve Repeat so holding arrows, Backspace,
            // PageUp, etc. behaves naturally. Windows emits Press + Release
            // for a tap; terminals with keyboard enhancement may additionally
            // emit Repeat while a key is held.
            if !should_dispatch_key(key.kind) {
                continue;
            }

            if state_for_key.extension_editor_open()
                && key.modifiers == KeyModifiers::CONTROL
                && key.code == KeyCode::Char('c')
            {
                close_extension_editor(
                    &state_for_key,
                    &ctx_for_key.editor_container,
                    &editor_for_key,
                );
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // 0. Ctrl+C: copy the selection when the editor has one (pi
            //    `tui.input.copy`); otherwise it's the escape hatch — even
            //    with a selector open (a stuck run or a mis-open selector must
            //    never trap the user): abort an active run, else exit.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                if !state_for_key.selector_open() && editor_for_key.has_selection() {
                    editor_for_key.copy_selection();
                    continue;
                }
                let status = *state_for_key.status.lock().unwrap();
                match status {
                    RunStatus::Working => {
                        state_for_key.set_status(RunStatus::Aborting);
                        let lane = lane_for_key.clone();
                        tokio::spawn(async move {
                            let _ = lane.abort().await;
                        });
                    }
                    // A held Ctrl+C can emit Repeat immediately after Press.
                    // Keep waiting for the in-flight cancellation instead of
                    // treating that repeat as a request to exit the process.
                    RunStatus::Aborting => {}
                    RunStatus::Idle => {
                        let _ = tx_for_key.send(TuiMessage::Exit);
                    }
                }
                continue;
            }

            // 1. A selector overlay is open → route to it first. Only Esc
            //    (cancel) and Enter/Up/Down/Ctrl-K/J/P/N (navigate/select)
            //    escape to the selector; on done/cancel the selector callbacks
            //    restore the editor and clear `active_selector`.
            if state_for_key.selector_open() {
                // Esc always cancels the selector (even with modifiers off).
                // Route through `SelectList::handle_key(Esc)` so the list's
                // `on_cancel` fires (the `/scoped-models` toggle selector saves
                // its edits there) — the old shortcut called `close_selector`
                // directly and skipped the callback.
                if key.code == KeyCode::Esc {
                    let (selector, _kind) = state_for_key
                        .active_selector
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("selector_open guaranteed Some");
                    selector.handle_key(key);
                    continue;
                }
                let (selector, _kind) = state_for_key
                    .active_selector
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("selector_open guaranteed Some");
                selector.handle_key(key);
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // Extension editor occupies the same input slot as the native
            // editor. Esc cancels it; every other key is delivered to the
            // extension-owned editor instance.
            if state_for_key.extension_editor_open() {
                let extension_editor = state_for_key
                    .active_extension_editor
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("extension_editor_open guaranteed Some");
                if key.code == KeyCode::Esc {
                    close_extension_editor(
                        &state_for_key,
                        &ctx_for_key.editor_container,
                        &editor_for_key,
                    );
                } else {
                    extension_editor.handle_key(key);
                }
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // 2a. Ctrl+D: pi's deleteCharForward inside the editor (mirrors
            //     `tui.editor.deleteCharForward`), and EOF-quit on an empty
            //     editor. With a run active, abort it first (same as Ctrl+C)
            //     so the key is never a no-op while a stuck command runs.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('d') {
                let status = *state_for_key.status.lock().unwrap();
                match status {
                    RunStatus::Working => {
                        state_for_key.set_status(RunStatus::Aborting);
                        let lane = lane_for_key.clone();
                        tokio::spawn(async move {
                            let _ = lane.abort().await;
                        });
                        continue;
                    }
                    RunStatus::Aborting => continue,
                    RunStatus::Idle => {}
                }
                if !state_for_key.selector_open() && !editor_for_key.get_text().is_empty() {
                    // Editor holds text — delete the char forward (pi parity).
                    editor_for_key.handle_key(key);
                    refresh_autocomplete(&state_for_key, &editor_for_key);
                    tui_for_key.request_render_reusing_scroll_content();
                    continue;
                }
                let _ = tx_for_key.send(TuiMessage::Exit);
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
                let current = state_for_key.current_model_id();
                // Cycle within the `/scoped-models` set (settings.json) when
                // configured; otherwise the full catalog.
                let scope = scoped_catalog(&ctx_for_key.model_catalog, &current);
                if let Some(next) = cycle_next_model(&scope, &current) {
                    state_for_key.set_current_model(&next);
                    let lane = lane_for_key.clone();
                    tokio::spawn(async move {
                        let _ = lane.set_model(next).await;
                    });
                    tui_for_key.request_render_reusing_scroll_content();
                }
                continue;
            }

            // 3. Ctrl+L: open the model selector. Routed through the `/model`
            //    command so the hotkey and the slash command share one path
            //    (TS binds Ctrl+L to model-select).
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('l') {
                if let Some(cmd) = registry_for_key.find("/model") {
                    cmd.execute(&ctx_for_key, "");
                }
                continue;
            }

            // 4. Tab: accept the top autocomplete suggestion (if any).
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Tab {
                if accept_top_suggestion(&state_for_key, &editor_for_key) {
                    tui_for_key.request_render_reusing_scroll_content();
                }
                continue;
            }

            // 5. Global transcript scroll. PageUp/PageDown use the actual
            // viewport height with four rows of overlap (upstream behavior),
            // while Home/End jump to the transcript boundaries.
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::PageUp {
                let delta = -transcript_page_size(scroll_for_key.viewport_height());
                if scroll_for_key.scroll_by(delta) != delta {
                    tui_for_key.request_render_reusing_scroll_content();
                }
                continue;
            }
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::PageDown {
                let delta = transcript_page_size(scroll_for_key.viewport_height());
                if scroll_for_key.scroll_by(delta) != delta {
                    tui_for_key.request_render_reusing_scroll_content();
                }
                continue;
            }
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Home {
                scroll_for_key.scroll_to_start();
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::End {
                scroll_for_key.scroll_to_end();
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // 5b. ↑/↓ browse submitted-message history when the editor is
            //     EMPTY (a fresh prompt) — mirrors TS historyPrevious/Next
            //     without the surprise of replacing typed text. When the
            //     editor holds content, ↑/↓ fall through to cursor movement
            //     (typing "hello", pressing ↑ at the start, must never swap
            //     the draft for a history entry — reported as "text
            //     disappeared"). Once browsing, ↓ walks back and restores the
            //     draft.
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Up {
                let browsing = *state_for_key.history_index.lock().unwrap() != -1;
                if editor_for_key.get_text().is_empty() || browsing {
                    navigate_history(&state_for_key, &editor_for_key, -1);
                    tui_for_key.request_render_reusing_scroll_content();
                    continue;
                }
            }
            if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Down {
                let browsing = *state_for_key.history_index.lock().unwrap() != -1;
                if editor_for_key.get_text().is_empty() || browsing {
                    navigate_history(&state_for_key, &editor_for_key, 1);
                    tui_for_key.request_render_reusing_scroll_content();
                    continue;
                }
            }

            // Alt+Enter queues a follow-up while a run is active. It is
            // handled here because Editor treats only a bare Enter as submit;
            // idle Alt+Enter keeps the normal prompt behavior.
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter {
                let prompt = editor_for_key.get_text().trim().to_string();
                if prompt.is_empty() {
                    continue;
                }
                editor_for_key.clear();
                let status = *state_for_key.status.lock().unwrap();
                if status == RunStatus::Idle {
                    if state_for_key.try_start_working() {
                        add_user_message(&state_for_key.chat_container, &prompt);
                        push_history(&state_for_key, &prompt);
                        let _ = tx_for_key.send(TuiMessage::UserInput(prompt));
                    }
                } else {
                    add_note_message(
                        &state_for_key.chat_container,
                        &format!("Queued follow-up message: {prompt}"),
                    );
                    let message = AgentMessage::User(UserMessage::new(prompt, 0));
                    let lane = lane_for_key.clone();
                    let chat = state_for_key.chat_container.clone();
                    let tui = tui_for_key.clone();
                    tokio::spawn(async move {
                        if let Err(error) = lane.follow_up(message).await {
                            add_error_message(&chat, &format!("Could not queue message: {error}"));
                            tui.request_render(false);
                        }
                    });
                }
                tui_for_key.request_render(false);
                continue;
            }

            // 6. Otherwise forward to the editor + refresh autocomplete.
            editor_for_key.handle_key(key);
            refresh_autocomplete(&state_for_key, &editor_for_key);
            tui_for_key.request_render_reusing_scroll_content();
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
        match rx.recv().await {
            Some(TuiMessage::UserInput(prompt)) => {
                // Clear the editor so the next prompt starts fresh (the submit
                // handler runs on the blocking key thread and can't mutate the
                // editor state safely there; clearing here, on the async loop,
                // keeps it on one thread).
                editor.clear();
                run_prompt_streaming(&lane, &prompt, &tui, &state, drain_handle.is_some()).await;
            }
            Some(TuiMessage::OpenTree) => {
                if *state.status.lock().unwrap() != RunStatus::Idle {
                    add_note_message(
                        &chat_container,
                        "Wait for the current run to finish before opening the tree.",
                    );
                    tui.request_render(false);
                } else {
                    open_tree_selector(
                        &harness,
                        &state,
                        &editor_container,
                        &editor,
                        &tui,
                        &chat_container,
                        &tx,
                    )
                    .await;
                }
            }
            Some(TuiMessage::NavigateTree(entry_id)) => {
                match lane.navigate_tree(Some(&entry_id), false, None, None).await {
                    Ok(result) => match result.outcome {
                        rpi_harness::agent_harness::NavigationOutcome::Completed { .. } => {
                            chat_container.clear();
                            add_welcome_message(&chat_container);
                            render_session_history(
                                &harness,
                                &chat_container,
                                state.markdown_transformer(),
                                Some(state.extension_session.clone()),
                            )
                            .await;
                            add_note_message(
                                &chat_container,
                                "Moved to the selected session entry.",
                            );
                        }
                        rpi_harness::agent_harness::NavigationOutcome::Failed { error, .. } => {
                            add_error_message(&chat_container, &error.message);
                        }
                        _ => add_note_message(
                            &chat_container,
                            "The selected entry could not be opened.",
                        ),
                    },
                    Err(error) => add_error_message(
                        &chat_container,
                        &format!("Could not navigate session tree: {error}"),
                    ),
                }
                tui.request_render(false);
            }
            Some(TuiMessage::ClearChat) => {
                chat_container.clear();
                add_welcome_message(&chat_container);
                tui.request_render(false);
            }
            Some(TuiMessage::Compact) => {
                run_compact(&lane, &tui, &state).await;
            }
            Some(TuiMessage::Copy) => {
                copy_last_assistant(&state, &chat_container);
                tui.request_render(false);
            }
            Some(TuiMessage::Exit) => {
                *running.lock().unwrap() = false;
                break;
            }
            Some(TuiMessage::SwitchSession(id)) => {
                switch_to_session(&harness, &lane, &id, &cwd, &chat_container, &state).await;
                tui.request_render(false);
            }
            Some(TuiMessage::ImportSession(path)) => {
                import_session(&harness, &lane, &path, &cwd, &chat_container, &state).await;
                tui.request_render(false);
            }
            Some(TuiMessage::ShareSession) => {
                share_session(&harness, &chat_container).await;
                tui.request_render(false);
            }
            Some(TuiMessage::SetSessionName(name)) => {
                let outcome = harness.session().set_name(Some(&name)).await;
                match outcome {
                    Ok(_) => add_note_message(
                        &chat_container,
                        &format!("Session renamed to \"{name}\"."),
                    ),
                    Err(e) => add_error_message(
                        &chat_container,
                        &format!("Could not rename session: {e}"),
                    ),
                }
                tui.request_render(false);
            }
            Some(TuiMessage::ExportSession) => {
                export_session(&harness, &chat_container).await;
                tui.request_render(false);
            }
            Some(TuiMessage::ForkSession) => {
                fork_session(&harness, &cwd, &chat_container, &state).await;
                tui.request_render(false);
            }
            Some(TuiMessage::ReloadExtensions) => {
                // B5d: drive the shared reload routine on the async runtime,
                // then surface the outcome. `reload_context` was passed into
                // `interactive_tui` and is the same `Arc<ReloadContext>` the
                // `ReloadCommand` + the plugin mailbox both route through —
                // clone the `Arc` out so the borrow of `harness` (the main
                // loop's `&AgentHarness`) lives across the await.
                let reload_ctx = ctx.reload_context.clone();
                add_note_message(&chat_container, "Reloading extensions + resources…");
                tui.request_render(false);
                let outcome =
                    crate::session::reload_extension_resources(&harness, &reload_ctx).await;
                // B5e: the reload swapped a fresh `ExtensionSession` into the
                // context's cell. Rebuild the markdown transformer from that
                // fresh snapshot and install it on the in-flight streaming
                // component (so a reloaded plugin's transformer takes effect on
                // the visible message immediately) + future components (they
                // read `state.markdown_transformer()` at construction). The old
                // closure no-ops once its snapshot's `active` flag flips false
                // (reload already did that before the swap).
                let fresh_transformer = build_markdown_transformer(
                    reload_ctx.extension_session.lock().unwrap().snapshot_arc(),
                );
                state.set_markdown_transformer_with_reinstall(fresh_transformer);
                if outcome.had_warnings {
                    add_error_message(
                        &chat_container,
                        &format!(
                            "{} (with warnings — see stderr for details).",
                            outcome.summary
                        ),
                    );
                } else {
                    add_note_message(&chat_container, &outcome.summary);
                }
                tui.request_render(false);
            }
            None => break,
        }
    }

    // ---- Shutdown ----
    *running.lock().unwrap() = false;
    // The input worker checks `running` at least every 50ms. Join it before
    // restoring cooked mode so no late event read races terminal cleanup.
    let _ = key_handle.await;
    tick_handle.abort();
    if let Some(handle) = drain_handle {
        handle.abort();
    }
    // Drop the reload bridge: clearing the mailbox closes the signal channel,
    // the drain task's `recv` returns `None`, and the task exits. (Aborting is
    // redundant — the recv terminates — but cheap + makes shutdown explicit.)
    reload_context.mailbox.clear();
    reload_bridge_handle.abort();
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
            HarnessRunOutcome::Failed {
                error,
                final_message,
                ..
            } => {
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
                        add_assistant_message_blocking(
                            &state.chat_container,
                            &text,
                            state.markdown_transformer(),
                        );
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
            add_error_message(&state.chat_container, &format!("Compact failed: {e}"));
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
            &format!(
                "Clipboard unavailable. Last reply: {preview}{}",
                if text.chars().count() > 200 {
                    "…"
                } else {
                    ""
                }
            ),
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
/// `transformer` is the live assistant-markdown transformer (B5e); `None` is
/// the identity path. The blocking path only fires when `event_rx` is absent,
/// so it shares the same transformer the streaming path installs on its
/// components.
fn add_assistant_message_blocking(
    container: &Arc<Container>,
    text: &str,
    transformer: Option<MarkdownTransformer>,
) {
    if text.is_empty() {
        return;
    }
    let msg = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    if let Some(t) = &transformer {
        msg.set_markdown_transformer(Some(t.clone()));
    }
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

        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            // Finalize the assistant message for this turn.
            if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                if let AgentMessage::Assistant(a) = &message {
                    comp.update_blocks(&assistant_blocks(a));
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
                // B5e: install the live markdown transformer so the plugin's
                // `register_markdown_transformer` handlers apply from the very
                // first streamed delta. `set_streaming` before the transform
                // install is fine (transform fires on `update_blocks`, below).
                if let Some(t) = state.markdown_transformer() {
                    comp.set_markdown_transformer(Some(t));
                }
                comp.set_streaming(true);
                // Render text AND thinking blocks in order (the old path fed
                // only the concatenated text, so thinking blocks never showed).
                comp.update_blocks(&assistant_blocks(&a));
                chat.add_child(comp.clone());
                // Spacer(1) separates this assistant turn from the next entry;
                // the component itself adds no leading spacer.
                chat.add_child(Arc::new(Spacer::new(1)));
                *state.current_assistant.lock().unwrap() = Some(comp);
                tui.request_render(false);
            }
            AgentMessage::Custom(custom) => {
                let payload = serde_json::json!({
                    "customType": custom.role,
                    "content": custom.content,
                    "details": custom.data,
                    "expanded": false,
                    "outputPad": 1,
                });
                if let Some(component) = extension_message_component(
                    &state.extension_session,
                    &custom.role,
                    &payload,
                    state.markdown_transformer(),
                ) {
                    chat.add_child(component);
                    chat.add_child(Arc::new(Spacer::new(1)));
                    tui.request_render(false);
                } else {
                    add_note_message(chat, &custom_message_fallback(&custom));
                    tui.request_render(false);
                }
            }
            // User / ToolResult / Custom starts are echoed at submit time or
            // via the tool-execution components; ignore user/tool dupes.
            _ => {}
        },

        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                let mut saw_bash_tool_call = false;
                // Scan content for finalized tool calls → proactively create
                // tool components (TS shows the tool as soon as the assistant
                // emits the ToolCall; ToolExecutionStart coalesces if it
                // already exists).
                for c in &a.content {
                    if let Content::ToolCall(tc) = c {
                        if tc.name == "bash" {
                            saw_bash_tool_call = true;
                            // Bash has a dedicated component. Create it here as
                            // well as on ToolExecutionStart because the tool
                            // call can become visible in a MessageUpdate first.
                            // Keeping it in the bash map lets Start coalesce
                            // with this panel instead of appending a second one.
                            let command = tc
                                .arguments
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let mut bash = state.bash_components.lock().unwrap();
                            if !bash.contains_key(&tc.id) {
                                let comp = Arc::new(BashExecutionComponent::new(command));
                                chat.add_child(comp.clone());
                                bash.insert(tc.id.clone(), comp);
                            }
                        } else {
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
                }
                // MessageUpdate can expose the finalized bash call before
                // ToolExecutionStart arrives. Hide the global `Working…`
                // loader immediately when creating that bash panel; otherwise
                // it briefly appears alongside the panel's `Running…` spinner.
                if saw_bash_tool_call {
                    state.sync_working_loader_with_bash();
                }
                let _ = assistant_message_event; // snapshot already applied via `a`
                if let Some(comp) = state.current_assistant.lock().unwrap().as_ref() {
                    // Stream the full block list (text + thinking) each update
                    // so thinking blocks render live as they arrive.
                    comp.update_blocks(&assistant_blocks(a));
                }
                *state.last_assistant_text.lock().unwrap() = text;
                tui.request_render(false);
            }
        }

        AgentEvent::MessageEnd { message } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                if let Some(comp) = state.current_assistant.lock().unwrap().take() {
                    comp.update_blocks(&assistant_blocks(a));
                    comp.set_streaming(false);
                }
                // Cache the finalized text for `/copy`.
                if !text.is_empty() {
                    *state.last_assistant_text.lock().unwrap() = text;
                }
                // Cache-miss notice (simplified `maybeShowCacheMissNotice`):
                // the previous turn's input established a cacheable prefix; a
                // large input this turn that read nothing from cache means the
                // prefix was re-billed. No cost display — v1 has no per-run
                // cost tracking here.
                let usage = &a.usage;
                let prev_input = *state.last_input_tokens.lock().unwrap();
                if prev_input > 0
                    && usage.input >= CACHE_MISS_MIN_INPUT_TOKENS
                    && usage.cache_read == 0
                {
                    add_note_message(
                        &state.chat_container,
                        &format!(
                            "Cache miss: {} tokens re-billed",
                            format_tokens(usage.input)
                        ),
                    );
                }
                if let Some(text) = extension_usage_text(Some(&state.extension_session), usage) {
                    add_note_message(chat, &text);
                }
                *state.last_input_tokens.lock().unwrap() = usage.input;
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
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
                let mut bash_map = state.bash_components.lock().unwrap();
                if let Some(existing) = bash_map.get(&tool_call_id) {
                    // A ToolExecutionUpdate already created the panel (fast
                    // command — Update can arrive before Start); backfill the
                    // command header instead of adding a SECOND panel, which
                    // used to stack an empty "$ " box above the real one.
                    existing.set_command(&command);
                } else {
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    chat.add_child(comp.clone());
                    bash_map.insert(tool_call_id.clone(), comp);
                }
            } else {
                let comp = {
                    let mut tools = state.tool_components.lock().unwrap();
                    if let Some(existing) = tools.get(&tool_call_id) {
                        existing.set_args(&args.to_string());
                        existing.clone()
                    } else {
                        let comp =
                            Arc::new(ToolExecutionComponent::new(&tool_name, &args.to_string()));
                        comp.set_running();
                        chat.add_child(comp.clone());
                        tools.insert(tool_call_id.clone(), comp.clone());
                        comp
                    }
                };
                state.remember_tool(comp);
            }
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial_result,
            ..
        } => {
            if tool_name == "bash" {
                // Append the streamed chunk to the bash component's preview.
                // RAW text (no single-line collapsing) — the old
                // `summarize_tool_result` folded every newline into a `⏎`
                // glyph, cramming e.g. `ls -la`'s listing onto one line.
                let chunk = tool_result_text(&partial_result);
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
                // Raw multi-line text — read/ls-style tools must show their
                // full content, not the single-line ⏎-folded summary.
                comp.set_result(&tool_result_text(&partial_result), false);
                apply_edit_diff(comp, &tool_name, &partial_result.details, &tui);
                state.remember_tool(comp.clone());
            } else {
                // No component yet — create a running one so the partial shows.
                let comp = Arc::new(ToolExecutionComponent::new(&tool_name, ""));
                comp.set_running();
                comp.set_result(&tool_result_text(&partial_result), false);
                apply_edit_diff(&comp, &tool_name, &partial_result.details, &tui);
                chat.add_child(comp.clone());
                state
                    .tool_components
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), comp.clone());
                state.remember_tool(comp);
            }
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
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
                    comp.append_output(&tool_result_text(&result));
                    finalize_bash(&comp, &result, is_error);
                    chat.add_child(comp);
                }
            } else {
                let comp = state.tool_components.lock().unwrap().remove(&tool_call_id);
                if let Some(comp) = comp {
                    comp.set_result(&tool_result_text(&result), is_error);
                    apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                } else {
                    // Tool ended without a Start/Update (e.g. a very fast tool):
                    // render a finalized component directly.
                    let comp = Arc::new(ToolExecutionComponent::new(&tool_name, ""));
                    comp.set_result(&tool_result_text(&result), is_error);
                    apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                    chat.add_child(comp.clone());
                    state.remember_tool(comp);
                }
            }
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }
    }
}

/// Extract `BashToolDetails` (`truncation`, `full_output_path`) from a bash
/// tool result and mark the component complete. Mirrors the TS bash finalize
/// path; only the fields `BashExecutionComponent` needs are read.
fn finalize_bash(
    comp: &Arc<BashExecutionComponent>,
    result: &rpi_agent::AgentToolResult,
    is_error: bool,
) {
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

/// The raw multi-line text of a tool result (no single-line collapsing). The
/// bash panel needs the original line structure — the old path fed it through
/// [`summarize_tool_result`], which folded every newline into a `⏎` glyph and
/// crammed e.g. `ls -la`'s whole listing onto one line.
fn tool_result_text(result: &rpi_agent::AgentToolResult) -> String {
    use rpi_agent::TextContentOrImage;
    let mut parts: Vec<String> = Vec::new();
    for c in &result.content {
        if let TextContentOrImage::Text(t) = c {
            parts.push(t.text.clone());
        }
    }
    parts.join("\n")
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
fn close_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
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
        let label = if m.name.is_empty() {
            short_model_name(&m.id)
        } else {
            m.name.clone()
        };
        let marker = if m.id.eq_ignore_ascii_case(lane_model_id) {
            " (current)"
        } else {
            ""
        };
        items.push(
            SelectItem::new(&m.id, &label).with_description(&format!("{id}{marker}", id = m.id)),
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
            add_note_message(
                &chat_sel,
                &format!("Model {} not found in catalog.", item.label),
            );
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Model,
    );
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
    tx: &mpsc::UnboundedSender<TuiMessage>,
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
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        // Close the selector first, then ask the async loop to hot-switch:
        // opening the session file + swapping the harness backing is async
        // (repo list/open) and must not run on the blocking key thread.
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
        let _ = tx_sel.send(TuiMessage::SwitchSession(item.value.clone()));
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Session,
    );
}

fn custom_entry_display_text(
    custom_type: &str,
    data: Option<&serde_json::Value>,
) -> Option<String> {
    let data = data?;
    let text = data
        .get("summary")
        .or_else(|| data.get("text"))
        .or_else(|| data.get("output"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())?;
    let label = match custom_type {
        "compactionSummary" => "Compaction summary",
        "branchSummary" => "Branch summary",
        "bashExecution" => "Command output",
        other => other,
    };
    Some(format!("{label}: {text}"))
}

/// Open a selector for the current session's persisted entry tree. Selecting a
/// message moves the main lane leaf to that entry, then the caller reloads the
/// visible branch from durable storage.
async fn open_tree_selector(
    harness: &AgentHarness,
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    let entries = match harness
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
    {
        Ok(entries) => entries,
        Err(error) => {
            add_error_message(chat, &format!("Could not read session tree: {error}"));
            tui.request_render(false);
            return;
        }
    };
    let current = harness.session().get_leaf_id().await.ok().flatten();
    let items: Vec<SelectItem> = entries
        .iter()
        .map(|entry| {
            let marker = if current.as_deref() == Some(entry.id()) {
                " (current)"
            } else {
                ""
            };
            SelectItem::new(
                entry.id(),
                &format!("{} #{}{}", entry.entry_type(), entry.seq(), marker),
            )
            .with_description(&entry.id()[..entry.id().len().min(12)])
        })
        .collect();
    if items.is_empty() {
        add_note_message(chat, "The current session has no entries to navigate.");
        tui.request_render(false);
        return;
    }
    let list = Arc::new(SelectList::new(items, 12));
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let tx_sel = tx.clone();
    list.on_select(Arc::new(move |item| {
        let _ = tx_sel.send(TuiMessage::NavigateTree(item.value.clone()));
        close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
    }));
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Tree,
    );
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
        apply_theme_preset(preset);
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Theme,
    );
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
        items.push(SelectItem::new(name, name).with_description(thinking_level_description(*lvl)));
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
            add_note_message(
                &chat_sel,
                &format!("Unknown thinking level: {}.", item.label),
            );
            close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
            return;
        };
        let lane = lane_sel.clone();
        let footer_sel = state_sel.footer.clone();
        tokio::spawn(async move {
            let _ = lane.set_thinking_level(level).await;
        });
        // Reflect the chosen level in the footer's model suffix (pi parity:
        // `model • thinking off` / `model • medium`). The shown text for the
        // Off level is "off", matching the TS `thinkingLevel === "off"` branch.
        footer_sel.set_thinking_level(Some(thinking_level_name(level)));
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Thinking,
    );
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
        Ok(h) => h
            .block_on(async { lane.get_active_tools().await })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut items: Vec<SelectItem> = Vec::new();
    for name in crate::session::BUILTIN_TOOL_NAMES {
        let on = active.iter().any(|a| a == name);
        let label = if on {
            format!("{name} (on)")
        } else {
            (*name).to_string()
        };
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Tools,
    );
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
        SelectItem::new("yes", "Yes").with_description(if current {
            "Inline images (current)"
        } else {
            "Inline images"
        }),
        SelectItem::new("no", "No").with_description(if current {
            "Placeholder only"
        } else {
            "Placeholder only (current)"
        }),
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

    open_selector(
        state,
        editor_container,
        editor,
        tui,
        list,
        SelectorKind::Images,
    );
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
            format!(
                "{prefix}{} {}",
                accent.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
        } else {
            format!(
                "{prefix}{} {}",
                muted.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
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
    // Keep the text AFTER the replaced span (mid-line completion: replacing
    // `[start, end)` must not drop the rest of the line).
    replaced.push_str(&text[end..]);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    // New caret position: after the inserted text (byte offset; the editor
    // snaps `set_cursor` to a char boundary as a safety net).
    let new_cursor = replaced.len().min(
        start
            + top.text.len()
            + if top.insert_space && !top.text.ends_with('/') {
                1
            } else {
                0
            },
    );
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
    add_welcome_message_with_capabilities(container, &[], &[]);
}

/// Add the startup welcome header and a compact snapshot of active tools and
/// discovered skills. The snapshot reflects the harness configuration used by
/// the first turn, including tools contributed by extensions.
fn add_welcome_message_with_capabilities(
    container: &Arc<Container>,
    active_tools: &[String],
    skills: &[String],
) {
    let c = current_theme().colors;
    // Accent logotype + a dim tagline, separated from the rest by a thin
    // themed rule. Plain `Text("rpi interactive TUI")` was visually identical
    // to the body text, so the header didn't read as a header.
    let title = format!(
        "{} {}",
        c.accent.fg(&tui_bold("rpi")),
        c.muted.fg("interactive TUI")
    );
    container.add_child(Arc::new(Text::new(title, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        c.dim.fg("Type your message and press Enter to send."),
        1,
        0,
    )));
    let hint = c
        .dim
        .fg("Enter send · Shift+Enter newline · Ctrl+C abort · Esc abort · /help");
    container.add_child(Arc::new(Text::new(hint, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Tools", active_tools),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Skills", skills),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::new()));
}

fn welcome_capability_line(label: &str, names: &[String]) -> String {
    let c = current_theme().colors;
    let value = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(" · ")
    };
    format!(
        "{} {}",
        c.accent.fg(&format!("{label} ({})", names.len())),
        c.muted.fg(&value)
    )
}

/// Add the `/help` command listing to the chat container.
fn add_help_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    // Section header + a thin themed rule, then a two-column command table:
    // `cmd` in accent, `— desc` in muted. The old single-space layout made
    // the description column wander depending on command length.
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📚 Available Commands")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));

    let cmds: &[(&str, &str)] = &[
        ("/help, /?", "Show this help message"),
        ("/clear, /new", "Clear the conversation"),
        ("/exit, /quit, /q", "Exit the application"),
        ("/version, /v", "Show version information"),
        ("/model, /m", "Choose a model (live switch)"),
        ("/thinking, /think", "Set reasoning depth (selector)"),
        ("/tools", "Toggle built-in tools on/off"),
        ("/images", "Toggle inline image rendering"),
        ("/session", "List saved sessions"),
        ("/theme", "Choose a theme (selector)"),
        ("/compact", "Compact the conversation"),
        ("/copy", "Copy last reply to clipboard"),
        ("/hotkeys", "Show keyboard shortcuts"),
        ("/armin", "🐾 Easter egg"),
        ("/earendil", "Earendil announcement"),
    ];
    let cmd_w = cmds.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (cmd, desc) in cmds {
        let row = format!(
            "  {:<cmd_w$}  {}  {}",
            c.accent.fg(cmd),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/version` block to the chat container.
fn add_version_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📦 Version Information")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    // Use the crate version (kept in sync via `version.workspace = true`)
    // instead of the stale hardcoded "v0.1.2".
    container.add_child(Arc::new(Text::new(
        format!(
            "  {} {}",
            c.muted.fg("rpi-cli"),
            c.text.fg(&format!("v{}", crate::VERSION))
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        format!(
            "  {}",
            c.dim.fg("Rust implementation of pi coding agent TUI")
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/hotkeys` block to the chat container.
fn add_hotkeys_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("⌨️  Keyboard Shortcuts")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let keys: &[(&str, &str)] = &[
        ("Enter", "Send message"),
        ("Shift+Enter", "New line"),
        ("Tab", "Accept autocomplete suggestion"),
        ("Ctrl+A / Ctrl+E", "Line start / end"),
        (
            "Ctrl+K / Ctrl+U",
            "Kill to end / start of line (Ctrl+Y yanks)",
        ),
        ("Ctrl+- / Ctrl+R", "Undo / redo"),
        ("Ctrl+Y / Alt+Y", "Yank / yank-pop"),
        ("Alt+Backspace", "Kill previous word"),
        ("Ctrl+C", "Abort a run, or exit when idle"),
        ("Esc", "Abort a running prompt"),
        ("Ctrl+L", "Open model selector"),
        ("Ctrl+M", "Cycle to the next model (live)"),
        ("Ctrl+T", "Expand/collapse last tool result"),
        ("PageUp/Down", "Scroll transcript by one page"),
        ("Home / End", "Jump to transcript start / latest output"),
    ];
    let key_w = keys.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, desc) in keys {
        let row = format!(
            "  {:<key_w$}  {}  {}",
            c.accent.fg(key),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a user message echo to the chat container — a bordered `UserMessageComponent`
/// (surface-colored box with OSC133 prompt-boundary markers) replacing the old
/// plain `> text` echo. A trailing Spacer(1) separates it from the next
// transcript entry (every entry contributes one trailing spacer so
// consecutive turns are separated by exactly one blank line).
fn add_user_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(UserMessageComponent::new(text.to_string())));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add an error message to the chat container.
fn add_error_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.error.fg("✗"), c.error.fg(text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a neutral note (e.g. unsupported-command message) to the chat container.
fn add_note_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.info.fg("ℹ"), c.muted.fg(text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Render the `/context` panel: a transcript message listing the discovered
/// context files, skills, and prompt templates loaded for this session
/// (Part A resource discovery). Reads the harness resources snapshot captured
/// at TUI startup (the blocking submit handler can't `await get_resources()`.
///
/// Mirrors pi's context-panel intent (pi surfaces loaded resources on startup +
/// via `/reload`); here it's a transcript note rather than an overlay since the
/// resource set is session-static between `/reload`s (deferred).
fn show_context_panel(
    chat: &Arc<Container>,
    resources: &Arc<rpi_harness::types::AgentHarnessResources>,
) {
    let skills = resources.skills.as_deref().unwrap_or(&[]);
    let templates = resources.prompt_templates.as_deref().unwrap_or(&[]);
    let mut lines: Vec<String> = Vec::new();
    lines.push("📂 Discovered resources for this session:".into());

    if skills.is_empty() {
        lines.push(
            "  Skills: (none discovered — create .pi/skills/ or ~/.rpi/agent/skills/)".into(),
        );
    } else {
        lines.push(format!("  Skills ({}):", skills.len()));
        for s in skills {
            let marker = if s.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            let desc: String = s.description.chars().take(72).collect();
            lines.push(format!("    • {}{marker} — {desc}", s.name));
        }
    }

    if templates.is_empty() {
        lines.push(
            "  Prompt templates: (none — create .pi/prompts/ or ~/.rpi/agent/prompts/)".into(),
        );
    } else {
        lines.push(format!("  Prompt templates ({}):", templates.len()));
        for t in templates {
            let desc = t
                .description
                .as_deref()
                .unwrap_or("(no description)")
                .chars()
                .take(72)
                .collect::<String>();
            lines.push(format!("    • /{} — {desc}", t.name));
        }
    }
    lines.push("  Context files (AGENTS.md/CLAUDE.md) are injected from the ancestor walk;".into());
    lines.push("  SYSTEM.md / APPEND_SYSTEM.md feed the base + append prompt sections.".into());
    lines.push(
        "  Use --no-skills/-ns, --no-prompt-templates/-np, --no-context-files/-nc to suppress."
            .into(),
    );
    let body = lines.join("\n");
    container_note_block(chat, &body);
}

/// Append a multi-line neutral note (header line + body) to the chat container.
fn container_note_block(container: &Arc<Container>, body: &str) {
    for line in body.lines() {
        container.add_child(Arc::new(Text::new(line.to_string(), 1, 0)));
    }
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
    fn transcript_page_uses_viewport_with_overlap() {
        assert_eq!(transcript_page_size(24), 20);
        assert_eq!(transcript_page_size(4), 1);
        assert_eq!(transcript_page_size(0), 1);
    }

    #[test]
    fn key_repeat_is_dispatched_but_release_is_not() {
        assert!(should_dispatch_key(KeyEventKind::Press));
        assert!(should_dispatch_key(KeyEventKind::Repeat));
        assert!(!should_dispatch_key(KeyEventKind::Release));
    }

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
        assert!(
            all.contains("rpi"),
            "Welcome message not found. Rendered: {}",
            all
        );
        assert!(
            all.contains("Type your message"),
            "Help text not found. Rendered: {}",
            all
        );
    }

    #[test]
    fn test_chat_container_has_welcome_content() {
        let chat = Arc::new(Container::new());
        add_welcome_message_with_capabilities(
            &chat,
            &["read".into(), "bash".into(), "web_fetch".into()],
            &["rust-review".into(), "release".into()],
        );

        let lines = chat.render(80);
        let all: String = lines.join("\n");
        // Welcome title is "rpi" (accent bold) + "interactive TUI" (muted),
        // joined by an ANSI reset — strip ANSI before checking the substring.
        let plain = strip_ansi(&all);
        assert!(
            plain.contains("rpi"),
            "Welcome message not in chat container: {:?}",
            lines
        );
        assert!(plain.contains("Tools (3)"), "Tool count missing: {plain}");
        assert!(
            plain.contains("read · bash · web_fetch"),
            "Tool names missing: {plain}"
        );
        assert!(plain.contains("Skills (2)"), "Skill count missing: {plain}");
        assert!(
            plain.contains("rust-review · release"),
            "Skill names missing: {plain}"
        );
    }

    #[test]
    fn welcome_capabilities_show_empty_state() {
        let plain = strip_ansi(&welcome_capability_line("Skills", &[]));
        assert_eq!(plain, "Skills (0) none");
    }

    /// Reproduction for "Tab 补全了但显示没刷新": after `accept_top_suggestion`
    /// replaces the editor text, the NEXT rendered frame must show the
    /// completed text (" /model " with the caret after it), not the old
    /// prefix. Mirrors the real dock layout (autocomplete_container above the
    /// bordered editor) and drives the same accept path the Tab handler uses.
    #[test]
    fn tab_accept_suggestion_reflects_in_next_render() {
        use rpi_tui::render_layout_frame;

        let editor = Arc::new(Editor::new(
            EditorOptions {
                padding_x: 1,
                ..Default::default()
            },
            EditorStyle::default(),
            Arc::new(rpi_tui::Keybindings::new()),
        ));
        editor.set_focused(true);
        let editor_container = Arc::new(Container::new());
        editor_container.add_child(editor.clone());
        let autocomplete_container = Arc::new(Container::new());
        let footer = Arc::new(rpi_tui::Text::new("FOOTER", 0, 0));
        let dock = Arc::new(VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
            StackChild::Entry(
                StackEntry::new(editor_container.clone())
                    .shrink(0)
                    .min_size(3),
            ),
            StackChild::Entry(StackEntry::new(footer)),
        ]));

        // Simulate the user typing "/mo" (the popup shows suggestions).
        let mut manager = AutocompleteManager::new();
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(
            SlashCommandAutocompleteProvider::with_default_commands(),
        ));
        combined.add_provider(Arc::new(FilePathAutocompleteProvider::new()));
        manager.set_provider(Arc::new(combined));
        // Simulate typing "/mo" via the real insert path (advances the caret
        // by char length, like `handle_key` does).
        editor.insert("/mo");
        assert_eq!(editor.cursor_position(), (0, 3));

        let frame_before = render_layout_frame(dock.clone(), 80, 10);
        assert!(
            frame_before.lines.iter().any(|l| l.contains("/mo")),
            "precondition: editor shows the typed prefix. Frame rows:\n{}",
            frame_before
                .lines
                .iter()
                .map(|l| format!("  [{l}]"))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Tab: accept the top suggestion (the same code path as the key loop).
        let text = editor.get_text();
        let (_row, col) = editor.cursor_position();
        let cursor = col.min(text.len());
        let sugg = manager
            .get_suggestions(&text, cursor)
            .expect("slash suggestions for /mo");
        let top = sugg.items.first().expect("at least one suggestion");
        let start = sugg.start.min(text.len());
        let end = sugg.end.min(text.len());
        let mut replaced = String::new();
        replaced.push_str(&text[..start]);
        replaced.push_str(&top.text);
        replaced.push_str(&text[end..]);
        if top.insert_space && !replaced.ends_with('/') {
            replaced.push(' ');
        }
        editor.set_text(&replaced);
        editor.set_cursor(0, replaced.len().min(start + top.text.len()));
        autocomplete_container.clear();
        assert_eq!(editor.get_text(), "/model");

        // The next render MUST display the completed text.
        let frame_after = render_layout_frame(dock, 80, 10);
        let all: String = frame_after.lines.join("\n");
        assert!(
            all.contains("/model"),
            "completed text missing from next render. Got:\n{all}"
        );
        // The caret must sit AFTER the completed command (the snap_boundary
        // regression put it one char early: "/mode|l" with the final char
        // dangling past the caret).
        let editor_line = frame_after
            .lines
            .iter()
            .find(|l| l.contains("/model"))
            .expect("editor row with completed text");
        assert!(
            editor_line.contains(&format!("/model{}", rpi_tui::CURSOR_MARKER)),
            "caret must follow the full completed text. Got: {editor_line:?}"
        );
    }

    #[test]
    fn test_slash_command_dispatch() {
        // The registry is the single source of truth for dispatch: `find(token)`
        // returns the command (by name or alias) whose `name()` is the canonical
        // form, or `None` for an unknown token. This replaces the old enum-based
        // `handle_slash_command` assertions with equivalent registry lookups.
        let registry = build_builtin_registry();

        // Helper: a token resolves to the command with this canonical name.
        let resolves_to = |token: &str, canonical: &str| {
            let found = registry.find(token).expect("{token} should resolve");
            assert_eq!(
                found.name(),
                canonical,
                "{token} resolved to {} (expected {canonical})",
                found.name()
            );
        };

        resolves_to("/help", "/help");
        resolves_to("/?", "/help"); // alias → canonical
        resolves_to("/clear", "/clear");
        resolves_to("/new", "/clear"); // alias
        resolves_to("/q", "/exit"); // alias
        resolves_to("/quit", "/exit"); // alias
        resolves_to("/version", "/version");
        resolves_to("/v", "/version"); // alias
        resolves_to("/hotkeys", "/hotkeys");
        resolves_to("/model", "/model");
        resolves_to("/m", "/model"); // alias
        resolves_to("/theme", "/theme");
        resolves_to("/session", "/session");
        resolves_to("/resume", "/session"); // alias
        resolves_to("/compact", "/compact");
        resolves_to("/copy", "/copy");
        resolves_to("/thinking", "/thinking");
        resolves_to("/think", "/thinking"); // alias
        resolves_to("/tools", "/tools");
        resolves_to("/images", "/images");
        resolves_to("/armin", "/armin");
        resolves_to("/earendil", "/earendil");
        resolves_to("/context", "/context");
        // Out-of-v1-scope commands resolve to their own UnsupportedCommand entry.
        resolves_to("/settings", "/settings");
        resolves_to("/name", "/name");
        resolves_to("/export", "/export");

        // Unknown token → not found.
        assert!(registry.find("/nope").is_none(), "/nope should be unknown");
    }

    #[test]

    fn test_registry_visible_entries_cover_dispatch() {
        // The autocomplete list is derived from the registry, so every visible
        // command the dispatcher recognizes must appear in it — by construction,
        // but this guards against a future command being registered with
        // `visible()` / a non-empty description that the builder drops.
        let registry = build_builtin_registry();
        let names: Vec<String> = registry
            .visible_entries()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        for recognized in [
            "/help",
            "/clear",
            "/new",
            "/exit",
            "/quit",
            "/version",
            "/model",
            "/session",
            "/theme",
            "/compact",
            "/copy",
            "/hotkeys",
            "/tools",
            "/images",
            "/thinking",
            "/armin",
            "/earendil",
        ] {
            assert!(
                names.contains(&recognized.to_string()),
                "{recognized} missing from autocomplete list"
            );
        }
        // Hidden commands stay off the list.
        for hidden in ["/context", "/q", "/m", "/v", "/think", "/resume", "/?"] {
            assert!(
                !names.contains(&hidden.to_string()),
                "{hidden} should be hidden from autocomplete"
            );
        }
    }

    #[test]
    fn test_agent_event_mapping_creates_assistant_and_tool() {
        // Synthetic AgentEvent sequence → UI mutations, exercised against the
        // real drain handler with a no-op TUI stand-in.
        use rpi_ai::types::{
            StopReason, TextContent, TextContentType, ThinkingContent, ThinkingContentType,
            ToolCall, ToolCallType, Usage,
        };

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
            active_extension_editor: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            last_input_tokens: std::sync::Mutex::new(0),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
        });

        // The drain handler takes `Arc<TuiAltScreen>`, which needs a real
        // terminal; instead, exercise the *mutation* half directly against a
        // captured chat container via a synthetic message-start event's data.
        let assistant = AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![
                Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    thinking: "Reasoning about the reply.".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
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
        let comp = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        comp.set_streaming(true);
        comp.update_blocks(&assistant_blocks(&assistant));
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

        // Assert: the assistant component rendered the text + the thinking
        // block (the update_blocks path keeps thinking visible), and a tool
        // component was registered.
        let rendered = chat.render(80);
        let joined: String = rendered.join("\n");
        assert!(
            joined.contains("Hello."),
            "assistant text not rendered: {joined}"
        );
        assert!(
            joined.contains("Reasoning about the reply."),
            "thinking block not rendered: {joined}"
        );
        assert_eq!(state.tool_components.lock().unwrap().len(), 1);
        assert!(state.current_assistant.lock().unwrap().is_some());

        // Manually apply ToolExecutionEnd (mirrors drain).
        let ended = state.tool_components.lock().unwrap().remove("tc1").unwrap();
        ended.set_result("hi", false);
        assert!(state.tool_components.lock().unwrap().is_empty());

        // A running bash panel owns the visible spinner. The global loader is
        // hidden until the last concurrent bash tool completes, then restored
        // while the agent remains in the Working state.
        assert!(state.try_start_working());
        assert!(
            !state.try_start_working(),
            "a second submit must be rejected"
        );
        state.set_status(RunStatus::Idle);
        state.set_status(RunStatus::Working);
        assert_eq!(state.status_container.child_count(), 1);
        {
            let mut bash = state.bash_components.lock().unwrap();
            bash.insert(
                "bash-1".into(),
                Arc::new(BashExecutionComponent::new("one")),
            );
            bash.insert(
                "bash-2".into(),
                Arc::new(BashExecutionComponent::new("two")),
            );
        }
        state.sync_working_loader_with_bash();
        assert_eq!(state.status_container.child_count(), 0);
        state.bash_components.lock().unwrap().remove("bash-1");
        state.sync_working_loader_with_bash();
        assert_eq!(state.status_container.child_count(), 0);
        state.bash_components.lock().unwrap().remove("bash-2");
        state.sync_working_loader_with_bash();
        assert_eq!(state.status_container.child_count(), 1);

        state.set_status(RunStatus::Aborting);
        assert_eq!(state.status_container.child_count(), 0);
        assert!(!state.loader.is_running());
    }

    #[test]
    fn fresh_launch_does_not_restore_old_history() {
        let fresh = Args::default();
        assert!(!launch_restores_history(&fresh));

        let continued = Args {
            continue_session: true,
            ..Args::default()
        };
        assert!(launch_restores_history(&continued));

        let selected = Args {
            session: Some("session-id".into()),
            ..Args::default()
        };
        assert!(launch_restores_history(&selected));
    }

    #[test]
    fn test_short_model_name() {
        assert_eq!(
            short_model_name("anthropic:claude-sonnet-5"),
            "claude-sonnet-5"
        );
        assert_eq!(short_model_name("claude-sonnet-5"), "claude-sonnet-5");
    }

    #[test]
    fn test_cycle_next_model_wraps_around() {
        use rpi_ai::{Api, Model};
        let mk = |id: &str| {
            Model::new(
                id,
                id,
                Api::AnthropicMessages,
                "anthropic",
                "https://api.anthropic.com",
            )
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
            active_extension_editor: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            last_input_tokens: std::sync::Mutex::new(0),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
        });
        {
            let mut combined = CombinedAutocompleteProvider::new();
            combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
                build_builtin_registry().visible_entries(),
            )));
            state.autocomplete.set_provider(Arc::new(combined));
        }

        let editor = Arc::new(Editor::simple());
        editor.set_text("/he");
        editor.set_cursor(0, 3);
        refresh_autocomplete(&state, &editor);
        let lines = state.autocomplete_container.render(80);
        let joined: String = lines.join("\n");
        assert!(
            joined.contains("/help"),
            "slash suggestions not rendered: {joined}"
        );

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
            active_extension_editor: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            last_input_tokens: std::sync::Mutex::new(0),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
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
        open_selector(
            &state,
            &editor_container,
            &editor,
            &tui,
            list,
            SelectorKind::Theme,
        );
        assert!(state.selector_open());
        // list only (editor swapped out).
        assert_eq!(editor_container.child_count(), 1);

        close_selector(&state, &editor_container, &editor, &tui);
        assert!(!state.selector_open());
        // editor restored.
        assert_eq!(editor_container.child_count(), 1);
    }

    #[test]
    fn test_message_history_browse_restores_draft() {
        // ↑/↓ recall semantics (mirrors TS navigateHistory): push two
        // messages, browse older → newer → back past the newest restores the
        // draft the user was typing.
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
            active_extension_editor: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            last_input_tokens: std::sync::Mutex::new(0),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
        });
        let editor = Arc::new(Editor::simple());

        push_history(&state, "first message");
        push_history(&state, "second message");
        // Consecutive duplicate is skipped.
        push_history(&state, "second message");
        push_history(&state, "   "); // empty → skipped
        assert_eq!(state.history.lock().unwrap().len(), 2);
        assert_eq!(state.history.lock().unwrap()[0], "second message");

        // User starts typing a fresh prompt.
        editor.set_text("half-typed");
        editor.set_cursor(0, 11);

        // ↑ → most recent.
        navigate_history(&state, &editor, -1);
        assert_eq!(editor.get_text(), "second message");
        assert_eq!(*state.history_index.lock().unwrap(), 0);
        // ↑ → older.
        navigate_history(&state, &editor, -1);
        assert_eq!(editor.get_text(), "first message");
        assert_eq!(*state.history_index.lock().unwrap(), 1);
        // ↑ past the oldest → stays (no wrap).
        navigate_history(&state, &editor, -1);
        assert_eq!(editor.get_text(), "first message");
        // ↓ → newer.
        navigate_history(&state, &editor, 1);
        assert_eq!(editor.get_text(), "second message");
        // ↓ past the newest → restores the draft.
        navigate_history(&state, &editor, 1);
        assert_eq!(editor.get_text(), "half-typed");
        assert_eq!(*state.history_index.lock().unwrap(), -1);
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
            active_extension_editor: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            last_input_tokens: std::sync::Mutex::new(0),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
        });
        {
            let mut combined = CombinedAutocompleteProvider::new();
            combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
                build_builtin_registry().visible_entries(),
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
