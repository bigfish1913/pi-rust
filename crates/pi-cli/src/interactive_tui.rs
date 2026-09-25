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

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::sync::{mpsc as std_mpsc, Arc, Mutex};

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use rpi_agent::{AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, Content, UserMessage};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::types::{Entry, EntryOrder, EntryQuery};
use rpi_tui::scroll_view::{OverscrollMode, ScrollbarMode};
#[cfg(test)]
use rpi_tui::strip_ansi;
use rpi_tui::{
    apply_theme_preset, render_diff, AltScreenSearch, AssistantBlock, AssistantMessageComponent,
    AssistantMessageOptions, AutocompleteManager, AutocompleteSuggestions, BashExecutionComponent,
    BashTruncation, CombinedAutocompleteProvider, Component, Container, DynamicBorder, Editor,
    EditorOptions, EditorStyle, FilePathAutocompleteProvider, Focusable, FollowMode,
    FooterComponent, Image, ImageOptions, Input, Loader, Markdown, ProcessTerminal, ScrollView,
    ScrollViewOptions, SearchBar, SearchableSelectList, SelectItem, SelectList, SettingItem,
    SettingsList, SlashCommand as SlashCommandEntry, SlashCommandAutocompleteProvider, Spacer,
    StackChild, StackEntry, StatusIndicator, Text, ThemeManager, ThemePreset,
    ToolExecutionComponent, ToolStatus, TuiAltScreen, UserMessageComponent, VStack, WorkingState,
    TUI,
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

/// A synchronous rendezvous between the Node runtime-request thread and the
/// blocking TUI key loop. Node's `ctx.ui.*` methods are promises, so the host
/// request must remain pending while the user interacts with the native
/// component. The key loop owns opening/closing components; this bridge only
/// carries JSON results and cancellation state across the threads.
#[derive(Clone, Default)]
struct JsDialogBridge {
    pending: Arc<Mutex<VecDeque<JsDialogPending>>>,
    active: Arc<Mutex<HashMap<String, JsDialogActive>>>,
    /// The one dialog currently installed in the TUI input slot. Other
    /// requests may remain active while a command is waiting, but a cancel
    /// notification must never close whichever dialog happens to be visible.
    visible: Arc<Mutex<Option<String>>>,
    cancelled_before_open: Arc<Mutex<HashSet<String>>>,
    closed: Arc<Mutex<bool>>,
}

struct JsDialogPending {
    request: JsDialogRequest,
    result: std_mpsc::Sender<serde_json::Value>,
}

struct JsDialogActive {
    result: std_mpsc::Sender<serde_json::Value>,
    cancel_requested: bool,
}

#[derive(Clone, Debug)]
struct JsDialogRequest {
    id: String,
    method: String,
    title: String,
    message: String,
    options: Vec<String>,
    placeholder: Option<String>,
    prefill: Option<String>,
}

impl JsDialogRequest {
    fn parse(args: &serde_json::Value) -> Result<Self, String> {
        let id = args
            .get("dialogId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("ui.dialog missing dialogId")?
            .to_string();
        let method = args
            .get("method")
            .and_then(serde_json::Value::as_str)
            .ok_or("ui.dialog missing method")?
            .to_string();
        if !matches!(method.as_str(), "select" | "confirm" | "input" | "editor") {
            return Err(format!("unsupported UI dialog method: {method}"));
        }
        let options = args
            .get("options")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            method,
            title: args
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            message: args
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            options,
            placeholder: args
                .get("placeholder")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
            prefill: args
                .get("prefill")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

impl JsDialogBridge {
    fn handle_runtime_request(
        &self,
        action: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match action {
            "ui.dialog" => self.wait_for_dialog(args),
            "ui.dialog.cancel" => {
                let id = args
                    .get("dialogId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("ui.dialog.cancel missing dialogId")?;
                self.cancel(id);
                Ok(serde_json::json!(true))
            }
            _ => Err(format!("unsupported capability: {action}")),
        }
    }

    fn wait_for_dialog(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let request = JsDialogRequest::parse(&args)?;
        let (sender, receiver) = std_mpsc::channel();
        // Hold the closed flag while enqueueing. `cancel_all` takes this same
        // lock before draining pending requests, so shutdown cannot observe
        // an empty queue and then have this request arrive behind the drain.
        let _closed = self
            .closed
            .lock()
            .map_err(|_| "JS dialog bridge poisoned")?;
        if *_closed {
            return Ok(serde_json::json!({ "cancelled": true }));
        }
        let cancelled_before_open = self
            .cancelled_before_open
            .lock()
            .map_err(|_| "JS dialog cancellation state poisoned")?
            .remove(&request.id);
        if cancelled_before_open {
            return Ok(serde_json::json!({ "cancelled": true }));
        }
        // Do not hold the cancellation-state lock while taking `pending`:
        // `take_pending` takes those locks in the opposite order.
        self.pending
            .lock()
            .map_err(|_| "JS dialog pending state poisoned")?
            .push_back(JsDialogPending {
                request,
                result: sender,
            });
        drop(_closed);
        receiver
            .recv()
            .map_err(|_| "JS dialog closed before it received an answer".to_string())
    }

    /// Move one request to the active set. The caller invokes this only when
    /// the TUI has no other modal occupying the editor slot.
    fn take_pending(&self) -> Option<JsDialogRequest> {
        loop {
            let pending = self.pending.lock().ok()?.pop_front()?;
            let mut active = self.active.lock().ok()?;
            // Check cancellation while holding the active lock and insert the
            // entry in the same critical section. `cancel()` checks `active`
            // before recording a pre-open cancellation, so it will either see
            // this entry or leave a marker that we consume here. Checking the
            // marker before acquiring `active` had a small race where a cancel
            // could land between the check and insertion and strand the dialog.
            if self
                .cancelled_before_open
                .lock()
                .ok()?
                .remove(&pending.request.id)
            {
                drop(active);
                let _ = pending
                    .result
                    .send(serde_json::json!({ "cancelled": true }));
                continue;
            }
            active.insert(
                pending.request.id.clone(),
                JsDialogActive {
                    result: pending.result,
                    cancel_requested: false,
                },
            );
            if let Ok(mut visible) = self.visible.lock() {
                *visible = Some(pending.request.id.clone());
            }
            return Some(pending.request);
        }
    }

    fn respond(&self, id: &str, result: serde_json::Value) {
        if let Ok(mut active) = self.active.lock() {
            if let Some(entry) = active.remove(id) {
                let _ = entry.result.send(result);
            }
        }
        if let Ok(mut visible) = self.visible.lock() {
            if visible.as_deref() == Some(id) {
                *visible = None;
            }
        }
    }

    fn cancel(&self, id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(index) = pending.iter().position(|item| item.request.id == id) {
                if let Some(item) = pending.remove(index) {
                    let _ = item.result.send(serde_json::json!({ "cancelled": true }));
                    return;
                }
            }
        }
        if let Ok(mut active) = self.active.lock() {
            if let Some(entry) = active.get_mut(id) {
                if !entry.cancel_requested {
                    entry.cancel_requested = true;
                    let _ = entry.result.send(serde_json::json!({ "cancelled": true }));
                }
                return;
            }
        }
        if let Ok(mut cancelled) = self.cancelled_before_open.lock() {
            cancelled.insert(id.to_string());
        }
    }

    fn cancelled_active_ids(&self) -> Vec<String> {
        self.active
            .lock()
            .map(|active| {
                active
                    .iter()
                    .filter_map(|(id, entry)| entry.cancel_requested.then_some(id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_visible(&self, id: &str) -> bool {
        self.visible
            .lock()
            .map(|visible| visible.as_deref() == Some(id))
            .unwrap_or(false)
    }

    fn finish(&self, id: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(id);
        }
        if let Ok(mut visible) = self.visible.lock() {
            if visible.as_deref() == Some(id) {
                *visible = None;
            }
        }
    }

    fn cancel_all(&self) {
        // Keep the closed lock through the queue drains. `wait_for_dialog`
        // holds it while enqueueing, making the shutdown check + enqueue an
        // atomic operation with respect to this drain.
        let Ok(mut closed) = self.closed.lock() else {
            return;
        };
        *closed = true;
        if let Ok(mut pending) = self.pending.lock() {
            for item in pending.drain(..) {
                let _ = item.result.send(serde_json::json!({ "cancelled": true }));
            }
        }
        if let Ok(mut active) = self.active.lock() {
            for (_, entry) in active.drain() {
                let _ = entry.result.send(serde_json::json!({ "cancelled": true }));
            }
        }
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
        drop(closed);
    }

    /// Cancel requests owned by one interrupted prompt preparation while
    /// keeping the bridge available to a replacement Node host.
    fn cancel_open_requests(&self) {
        // Keep enqueueing closed until the old host and its preparation
        // worker have stopped. Otherwise a late runtime request can land just
        // after the drain and strand its handler thread.
        let Ok(mut closed) = self.closed.lock() else {
            return;
        };
        *closed = true;
        if let Ok(mut pending) = self.pending.lock() {
            for item in pending.drain(..) {
                let _ = item.result.send(serde_json::json!({ "cancelled": true }));
            }
        }
        if let Ok(mut active) = self.active.lock() {
            for (_, entry) in active.drain() {
                let _ = entry.result.send(serde_json::json!({ "cancelled": true }));
            }
        }
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
        if let Ok(mut cancelled) = self.cancelled_before_open.lock() {
            cancelled.clear();
        }
        drop(closed);
    }

    fn reopen(&self) {
        if let Ok(mut closed) = self.closed.lock() {
            *closed = false;
        }
    }
}

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
    /// Package resources resolved at startup. An empty set means Pi package
    /// loading was not explicitly enabled and must remain disabled for all
    /// interactive theme selectors.
    package_resources: Arc<crate::packages::PackageResources>,
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
    /// The product-layer session (`AgentSession`). Commands that need durable
    /// reads, export, or usage stats go through this instead of reaching into
    /// the harness directly, so the product surface is one object.
    session: Arc<crate::agent_session::AgentSession>,
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

/// Dispatch a ui_prompt_start event to extensions.
fn dispatch_ui_prompt_event(state: &TuiState) {
    use rpi_plugin_sdk::EventTag;

    if let Ok(session) = state.extension_session.lock() {
        if let Some(snapshot) = session.snapshot_arc() {
            rpi_extensions::dispatch_empty_event(&snapshot, EventTag::UiPromptStart);
        }
    }
}

/// Dispatch a ui_prompt_end event to extensions.
fn dispatch_ui_prompt_event_end(state: &TuiState) {
    use rpi_plugin_sdk::EventTag;

    if let Ok(session) = state.extension_session.lock() {
        if let Some(snapshot) = session.snapshot_arc() {
            rpi_extensions::dispatch_empty_event(&snapshot, EventTag::UiPromptEnd);
        }
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

/// Current wall-clock time in milliseconds since the Unix epoch. Used for
/// `bashExecution` message timestamps (agent-loop messages carry real times, so
/// the transcript record must too).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse a submitted line into a user shell command.
///
/// Mirrors native pi's `text.startsWith("!")` handling
/// (`interactive-mode.ts:3225`): `!cmd` runs and is recorded in the session
/// context, `!!cmd` is excluded from it. Returns `None` for a bare `!` / `!!`
/// (or whitespace only), so the line falls through to the normal prompt path.
fn parse_user_bash(text: &str) -> Option<(&str, bool)> {
    let rest = text.strip_prefix('!')?;
    let (command, exclude_from_context) = match rest.strip_prefix('!') {
        Some(command) => (command, true),
        None => (rest, false),
    };
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some((command, exclude_from_context))
}

/// Run a `!command` submitted from the editor.
///
/// The command executes out-of-band — it never reaches the model. Output
/// streams into a [`BashExecutionComponent`] in the transcript and the
/// terminal result is handed to the main loop, which persists it as a
/// `bashExecution` message (skipped from context for `!!`). Mirrors
/// `handleBashCommand` (`interactive-mode.ts:6729`).
fn start_user_bash(ctx: &CommandContext, command: &str, exclude_from_context: bool) {
    let comp = Arc::new(BashExecutionComponent::new_with_context(
        command,
        exclude_from_context,
    ));
    comp.set_expanded(*ctx.state.tool_outputs_expanded.lock().unwrap());
    ctx.chat.add_child(comp.clone());

    // Register the cancellation slot before spawning so an Esc arriving on the
    // key thread can never miss the run (native pi's `_bashAbortControllers`).
    let cancel = ctx.state.begin_user_bash();
    ctx.tui.request_render(false);

    let env: Arc<dyn rpi_tools::ExecutionEnv> =
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(ctx.cwd.clone()));
    let tui = ctx.tui.clone();
    let tx = ctx.tx.clone();
    let cwd = ctx.cwd.clone();
    let command_owned = command.to_string();

    tokio::spawn(async move {
        let comp_chunk = comp.clone();
        let tui_chunk = tui.clone();
        let on_chunk: Box<
            dyn FnMut(&str, &dyn Fn() -> rpi_tools::shell_output::ShellCaptureProgress) + Send,
        > = Box::new(move |_chunk, get_progress| {
            // The capture layer reports the complete tail captured so far, not
            // a delta; `append_output` replaces its snapshot accordingly.
            comp_chunk.append_output(&get_progress().output);
            tui_chunk.request_render(false);
        });
        let options = rpi_tools::shell_output::ShellCaptureOptions {
            cwd: Some(cwd),
            cancel: Some(&cancel),
            on_chunk: Some(on_chunk),
            return_execution_errors: true,
            ..Default::default()
        };
        let result =
            rpi_tools::shell_output::execute_shell_with_capture(&env, &command_owned, options)
                .await;

        // `Ok` carries the real exit code/cancellation; `Err` is a transport
        // level failure (spawn failure, invalid env) which we surface inline.
        let (exit_code, cancelled, output, truncated, full_output_path) = match result {
            Ok(capture) => (
                capture.exit_code,
                capture.cancelled,
                capture.output.clone(),
                capture.truncated,
                capture
                    .full_output_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            ),
            Err(error) => (
                None,
                false,
                format!("Bash command failed: {error}"),
                false,
                None,
            ),
        };
        comp.append_output(&output);
        comp.set_complete(
            exit_code,
            cancelled,
            BashTruncation {
                truncated,
                full_output_path: full_output_path.clone(),
            },
        );
        tui.render_now(false);

        let _ = tx.send(TuiMessage::UserBashFinished(UserBashReport {
            command: command_owned,
            output,
            exit_code,
            cancelled,
            truncated,
            full_output_path,
            exclude_from_context,
        }));
    });
}

/// Encode a crossterm key into the raw key data consumed by the Node TUI
/// compatibility layer. Plain keys retain the usual terminal sequences;
/// modified functional keys use Kitty CSI-u so Shift/Alt/Ctrl combinations are
/// not collapsed into their unmodified equivalent (notably Shift+Enter).
fn key_event_to_input(key: crossterm::event::KeyEvent) -> String {
    use crossterm::event::{KeyCode, KeyModifiers};

    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let super_key = modifiers.contains(KeyModifiers::SUPER);

    if modifiers == KeyModifiers::NONE {
        return match key.code {
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Enter => "\r".into(),
            KeyCode::Esc => "\x1b".into(),
            KeyCode::Backspace => "\x7f".into(),
            KeyCode::Tab => "\t".into(),
            // Crossterm represents Shift+Tab as `BackTab` on both Unix
            // (`ESC[Z`) and Windows. Preserve the canonical terminal form
            // so the Node keybinding matcher sees `shift+tab`.
            KeyCode::BackTab => "\x1b[Z".into(),
            KeyCode::Up => "\x1b[A".into(),
            KeyCode::Down => "\x1b[B".into(),
            KeyCode::Right => "\x1b[C".into(),
            KeyCode::Left => "\x1b[D".into(),
            KeyCode::Home => "\x1b[H".into(),
            KeyCode::End => "\x1b[F".into(),
            KeyCode::PageUp => "\x1b[5~".into(),
            KeyCode::PageDown => "\x1b[6~".into(),
            KeyCode::Delete => "\x1b[3~".into(),
            KeyCode::Insert => "\x1b[2~".into(),
            KeyCode::F(n) => format!("\x1b[{}~", 10 + n as u16),
            _ => String::new(),
        };
    }

    // Legacy control bytes are what the native `matchesKey` implementation
    // expects for the common Ctrl+letter actions (Ctrl+C, Ctrl+O, Ctrl+J...).
    if ctrl && !shift && !alt && !super_key {
        if let KeyCode::Char(ch) = key.code {
            if let Some(code) = control_code(ch) {
                return char::from(code).to_string();
            }
        }
    }

    // Legacy Alt+character input is unambiguous when no other modifier is
    // present and is accepted by pi's `matchesKey` fallback parser.
    if alt && !ctrl && !shift && !super_key {
        if let KeyCode::Char(ch) = key.code {
            return format!("\x1b{ch}");
        }
    }

    // Crossterm has already resolved the keyboard layout for character events
    // (for example, Windows reports Shift+1 as `Char('!')`). Pass that actual
    // character through unchanged so custom components receive text instead
    // of a CSI-u escape sequence. Functional keys and combined modifiers use
    // CSI-u below so their modifier identity remains available to keybindings.
    if shift && !ctrl && !alt && !super_key {
        if let KeyCode::Char(ch) = key.code {
            return ch.to_string();
        }
    }

    if let Some(sequence) = modified_functional_sequence(key.code, modifiers) {
        return sequence;
    }
    let Some(codepoint) = key_codepoint(key.code, ctrl) else {
        return String::new();
    };
    kitty_key_sequence(codepoint, modifiers)
}

fn control_code(ch: char) -> Option<u8> {
    let ch = ch.to_ascii_lowercase();
    Some(match ch {
        '@' | ' ' => 0,
        'a'..='z' => (ch as u8) & 0x1f,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' | '-' => 0x1f,
        _ => return None,
    })
}

fn key_codepoint(code: crossterm::event::KeyCode, ctrl: bool) -> Option<u32> {
    use crossterm::event::KeyCode;
    Some(match code {
        KeyCode::Char(ch) => {
            if ctrl {
                ch.to_ascii_lowercase() as u32
            } else {
                ch as u32
            }
        }
        KeyCode::Enter => 13,
        KeyCode::Esc => 27,
        KeyCode::Backspace => 127,
        KeyCode::Tab => 9,
        // Keep modified BackTab combinations representable through CSI-u;
        // the unmodified/SHIFT form is handled as the legacy `ESC[Z` above.
        KeyCode::BackTab => 9,
        _ => return None,
    })
}

fn modified_functional_sequence(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};
    // `BackTab` is already a semantic Shift+Tab event. Crossterm normally
    // includes SHIFT in its modifier bits, but preserving the legacy sequence
    // for a synthetic event without that bit keeps the adapter portable.
    if code == KeyCode::BackTab
        && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return Some("\x1b[Z".into());
    }
    let modifier = kitty_modifier(modifiers);
    let sequence = match code {
        KeyCode::Up => format!("\x1b[1;{modifier}A"),
        KeyCode::Down => format!("\x1b[1;{modifier}B"),
        KeyCode::Right => format!("\x1b[1;{modifier}C"),
        KeyCode::Left => format!("\x1b[1;{modifier}D"),
        KeyCode::Home => format!("\x1b[1;{modifier}H"),
        KeyCode::End => format!("\x1b[1;{modifier}F"),
        KeyCode::Insert => format!("\x1b[2;{modifier}~"),
        KeyCode::Delete => format!("\x1b[3;{modifier}~"),
        KeyCode::PageUp => format!("\x1b[5;{modifier}~"),
        KeyCode::PageDown => format!("\x1b[6;{modifier}~"),
        _ => return None,
    };
    Some(sequence)
}

fn kitty_modifier(modifiers: crossterm::event::KeyModifiers) -> u8 {
    use crossterm::event::KeyModifiers;
    let mut modifier = 1u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        modifier += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        modifier += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        modifier += 4;
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        modifier += 8;
    }
    modifier
}

fn kitty_key_sequence(codepoint: u32, modifiers: crossterm::event::KeyModifiers) -> String {
    let modifier = kitty_modifier(modifiers);
    format!("\x1b[{codepoint};{modifier}u")
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

struct JsExtensionCommand {
    name: String,
    session: crate::js_extensions::JsExtensionSession,
}

impl SlashCommand for JsExtensionCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &'static str {
        "JS extension command"
    }
    fn description_owned(&self) -> String {
        "JS extension command".to_string()
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        // JS commands may own the terminal for their entire lifetime (for
        // example pi-btw's fullscreen side thread). Running them inline here
        // would block the crossterm key thread, so no input could reach the
        // extension while it is waiting for `ui.custom()` to complete.
        let session = self.session.clone();
        let command = self.name.trim_start_matches('/').to_string();
        let args = args.to_string();
        let ctx = ctx.clone();
        // The command runs off the key thread. Keep the startup snapshot so a
        // late editorText result cannot overwrite text typed while the command
        // was in flight.
        let initial_editor_text = ctx.editor.get_text();
        tokio::task::spawn_blocking(move || {
            match session.invoke_command_with_context(
                &command,
                &args,
                serde_json::json!({"editorText": initial_editor_text}),
            ) {
                Ok(value) => {
                    if let Some(editor_text) = value.get("editorText").and_then(|v| v.as_str()) {
                        if ctx.editor.get_text() == initial_editor_text
                            && editor_text != initial_editor_text
                        {
                            let cursor = editor_text.chars().count();
                            ctx.editor.set_text(editor_text);
                            ctx.editor.set_cursor(0, cursor);
                        }
                    }
                    if let Some(notifications) =
                        value.get("notifications").and_then(|v| v.as_array())
                    {
                        for notification in notifications {
                            let message = notification
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if message.is_empty() {
                                continue;
                            }
                            match notification.get("level").and_then(|v| v.as_str()) {
                                Some("error") => add_error_message(&ctx.chat, message),
                                _ => add_note_message(&ctx.chat, message),
                            }
                        }
                    }
                    let result = value.get("result").unwrap_or(&value);
                    let text = result
                        .get("text")
                        .and_then(|item| item.as_str())
                        .map(str::to_string)
                        .or_else(|| result.as_str().map(str::to_string))
                        .filter(|text| !text.is_empty() && text != "null");
                    if let Some(text) = text {
                        add_note_message(&ctx.chat, &text);
                    }
                }
                Err(error) => {
                    add_error_message(&ctx.chat, &format!("JS extension command failed: {error}"))
                }
            }
            ctx.tui.request_render(false);
        });
    }
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
    // A cancellation continuation may intentionally return JSON null. Native
    // pi resolves the pending promise with `undefined` and does not add a
    // visible "null" message to the transcript.
    if value.is_null() {
        ctx.tui.request_render(false);
        return;
    }
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
        // Native pi exposes `ctx.ui.input(title, placeholder)` separately
        // from the multiline editor. Render it as a focused single-line
        // dialog in the swapped input slot.
        Some("input") => open_extension_input(ctx, session, command_name, value),
        Some(other) => {
            add_error_message(&ctx.chat, &format!("Unsupported extension UI: {other}"));
            ctx.tui.request_render(false);
        }
    }
}

/// Render the title used by the native pi extension dialogs.  Keeping it in
/// the swapped editor container makes the question stay visible while the
/// extension waits for the answer, instead of adding a transient chat note.
fn extension_dialog_title(title: &str, bold: bool) -> Arc<Text> {
    let colors = current_theme().colors;
    let text = if bold {
        tui_bold(title)
    } else {
        title.to_string()
    };
    Arc::new(Text::new(colors.accent.fg(&text), 1, 0))
}

fn extension_dialog_hint(label: &str) -> Arc<Text> {
    Arc::new(Text::new(current_theme().colors.muted.fg(label), 1, 0))
}

/// Markdown-aware version of `extension_dialog_title` for extension dialogs
/// that may contain markdown content (tables, lists, bold, etc).
fn extension_dialog_title_md(title: &str, bold: bool) -> Arc<dyn Component> {
    let text = if bold {
        format!("**{}**", title)
    } else {
        title.to_string()
    };
    Arc::new(Markdown::new(text, 1, 0))
}

/// Markdown-aware version of `extension_dialog_hint` for extension dialogs
/// that may contain markdown content.
fn extension_dialog_hint_md(label: &str) -> Arc<dyn Component> {
    Arc::new(Markdown::new(label, 1, 0))
}

/// Take and run the cancellation callback for the active extension dialog.
/// Taking it before invoking the callback breaks the temporary Arc cycle: the
/// callback owns the command context so it can process a follow-up result.
fn run_extension_cancel(state: &Arc<TuiState>) -> bool {
    let callback = state.active_extension_cancel.lock().unwrap().take();
    if let Some(callback) = callback {
        callback();
        true
    } else {
        false
    }
}

fn open_extension_input(
    ctx: &CommandContext,
    session: crate::session::ExtensionSessionCell,
    command_name: String,
    value: serde_json::Value,
) {
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Input");
    let input = value
        .get("placeholder")
        .and_then(|v| v.as_str())
        .map(Input::with_placeholder)
        .unwrap_or_default();
    let input = Arc::new(input);
    if let Some(initial) = value
        .get("initialText")
        .or_else(|| value.get("text"))
        .and_then(|v| v.as_str())
    {
        input.set_value(initial);
    }
    input.set_focused(true);

    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(input.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint("Enter submit · Esc/Ctrl+C cancel"));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let session_submit = session.clone();
    let command_submit = command_name.clone();
    let ctx_submit = ctx.clone();
    input.on_submit(Arc::new(move |text| {
        let args = serde_json::json!({ "action": "input", "value": text, "text": text });
        let result = invoke_extension_command(
            &session_submit,
            &command_submit,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state, &ec, &original, &tui);
        handle_extension_ui_result(
            result,
            &ctx_submit,
            session_submit.clone(),
            command_submit.clone(),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));

    ctx.tui.set_focus(Some(input));
    ctx.tui.request_render(false);
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
                    // Native pi's selector accepts `string[]`; the Rust ABI
                    // also permits `{value,label,description}` objects.
                    let value = if let Some(value) = item.as_str() {
                        value
                    } else {
                        item.get("value")?.as_str()?
                    };
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
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Select");
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, true));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(list.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "↑↓ navigate · Enter select · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

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
        if !run_extension_cancel(&state_cancel) {
            close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        }
    }));
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));
    open_selector_with_view(
        &ctx.state,
        &ctx.editor_container,
        &ctx.editor,
        &ctx.tui,
        SelectorView::List(list),
        frame,
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
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("Editor");
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
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(title, false));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(editor.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "Enter submit · Shift+Enter newline · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = Some(editor.clone());
    *ctx.state.active_extension_input.lock().unwrap() = None;
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let original = ctx.editor.clone();
    let tui = ctx.tui.clone();
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
        close_extension_editor(&state, &ec, &original, &tui);
        handle_extension_ui_result(
            result,
            &ctx_submit,
            session_submit.clone(),
            command_submit.clone(),
        );
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let original_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let session_cancel = session.clone();
    let command_cancel = command_name.clone();
    let ctx_cancel = ctx.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        let args = serde_json::json!({ "action": "cancel" });
        let result = invoke_extension_command(
            &session_cancel,
            &command_cancel,
            &serde_json::to_string(&args).unwrap_or_default(),
        );
        close_extension_editor(&state_cancel, &ec_cancel, &original_cancel, &tui_cancel);
        handle_extension_ui_result(
            result,
            &ctx_cancel,
            session_cancel.clone(),
            command_cancel.clone(),
        );
    }));

    ctx.tui.set_focus(Some(editor));
    ctx.tui.request_render(false);
}

fn close_extension_editor(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
) {
    editor_container.clear();
    editor_container.add_child(editor.clone());
    state.autocomplete_container.clear();
    *state.active_extension_editor.lock().unwrap() = None;
    *state.active_extension_input.lock().unwrap() = None;
    *state.active_extension_cancel.lock().unwrap() = None;
    editor.set_focused(true);
    tui.set_focus(Some(editor.clone()));
    tui.request_render(false);
}

/// Open one Node `ctx.ui.*` request in the native editor slot. The Node host
/// waits on the runtime response while these callbacks resolve the bridge on
/// Enter, selection, or cancellation.
fn open_js_dialog(ctx: &CommandContext, bridge: Arc<JsDialogBridge>, request: JsDialogRequest) {
    match request.method.as_str() {
        "select" => open_js_selector(ctx, bridge, request, false),
        "confirm" => open_js_selector(ctx, bridge, request, true),
        "input" => open_js_input(ctx, bridge, request),
        "editor" => open_js_editor(ctx, bridge, request),
        _ => {
            bridge.respond(&request.id, serde_json::json!({ "cancelled": true }));
        }
    }
}

fn open_js_selector(
    ctx: &CommandContext,
    bridge: Arc<JsDialogBridge>,
    request: JsDialogRequest,
    confirm: bool,
) {
    let values = if confirm {
        vec!["Yes".to_string(), "No".to_string()]
    } else {
        request.options.clone()
    };
    if values.is_empty() {
        bridge.respond(&request.id, serde_json::json!({ "cancelled": true }));
        return;
    }
    let items = values
        .iter()
        .map(|value| SelectItem::new(value, value))
        .collect::<Vec<_>>();
    let list = Arc::new(SelectList::new(items, 10));
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(
        if request.title.is_empty() {
            if confirm {
                "Confirm"
            } else {
                "Select"
            }
        } else {
            request.title.as_str()
        },
        true,
    ));
    if !request.message.is_empty() {
        frame.add_child(Arc::new(Spacer::new(1)));
        frame.add_child(Arc::new(Text::new(request.message.clone(), 1, 0)));
    }
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(list.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "↑↓ navigate · Enter select · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    let id = request.id.clone();
    let bridge_select = bridge.clone();
    let state_select = ctx.state.clone();
    let ec_select = ctx.editor_container.clone();
    let editor_select = ctx.editor.clone();
    let tui_select = ctx.tui.clone();
    list.on_select(Arc::new(move |item| {
        let result = if confirm {
            serde_json::json!({ "confirmed": item.value == "Yes" })
        } else {
            serde_json::json!({ "value": item.value })
        };
        bridge_select.respond(&id, result);
        close_selector(&state_select, &ec_select, &editor_select, &tui_select);
    }));

    let id_cancel = request.id.clone();
    let bridge_cancel = bridge.clone();
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    list.on_cancel(Arc::new(move || {
        bridge_cancel.respond(&id_cancel, serde_json::json!({ "cancelled": true }));
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    let id_abort = request.id.clone();
    let bridge_abort = bridge.clone();
    let state_abort = ctx.state.clone();
    let ec_abort = ctx.editor_container.clone();
    let editor_abort = ctx.editor.clone();
    let tui_abort = ctx.tui.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        bridge_abort.respond(&id_abort, serde_json::json!({ "cancelled": true }));
        close_selector(&state_abort, &ec_abort, &editor_abort, &tui_abort);
    }));

    open_selector_with_view(
        &ctx.state,
        &ctx.editor_container,
        &ctx.editor,
        &ctx.tui,
        SelectorView::List(list),
        frame,
        SelectorKind::Extension,
    );
}

fn open_js_input(ctx: &CommandContext, bridge: Arc<JsDialogBridge>, request: JsDialogRequest) {
    let input = request
        .placeholder
        .as_deref()
        .map(Input::with_placeholder)
        .unwrap_or_default();
    let input = Arc::new(input);
    input.set_focused(true);

    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(
        if request.title.is_empty() {
            "Input"
        } else {
            &request.title
        },
        false,
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(input.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint("Enter submit · Esc/Ctrl+C cancel"));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let id = request.id.clone();
    let bridge_submit = bridge.clone();
    let state_submit = ctx.state.clone();
    let ec_submit = ctx.editor_container.clone();
    let editor_submit = ctx.editor.clone();
    let tui_submit = ctx.tui.clone();
    input.on_submit(Arc::new(move |value| {
        bridge_submit.respond(&id, serde_json::json!({ "value": value }));
        close_extension_editor(&state_submit, &ec_submit, &editor_submit, &tui_submit);
    }));

    let id_cancel = request.id.clone();
    let bridge_cancel = bridge.clone();
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        bridge_cancel.respond(&id_cancel, serde_json::json!({ "cancelled": true }));
        close_extension_editor(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    ctx.tui.set_focus(Some(input));
    ctx.tui.request_render(false);
}

fn open_js_editor(ctx: &CommandContext, bridge: Arc<JsDialogBridge>, request: JsDialogRequest) {
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 0,
            initial_text: request.prefill.clone(),
            ..Default::default()
        },
        EditorStyle {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        },
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);

    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_title(
        if request.title.is_empty() {
            "Editor"
        } else {
            &request.title
        },
        false,
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(editor.clone());
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(
        "Enter submit · Shift+Enter newline · Esc/Ctrl+C cancel",
    ));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));

    *ctx.state.active_extension_editor.lock().unwrap() = Some(editor.clone());
    *ctx.state.active_extension_input.lock().unwrap() = None;
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);

    let id = request.id.clone();
    let bridge_submit = bridge.clone();
    let state_submit = ctx.state.clone();
    let ec_submit = ctx.editor_container.clone();
    let editor_submit = ctx.editor.clone();
    let tui_submit = ctx.tui.clone();
    editor.on_submit(Arc::new(move |value| {
        bridge_submit.respond(&id, serde_json::json!({ "value": value }));
        close_extension_editor(&state_submit, &ec_submit, &editor_submit, &tui_submit);
    }));

    let id_cancel = request.id.clone();
    let bridge_cancel = bridge.clone();
    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        bridge_cancel.respond(&id_cancel, serde_json::json!({ "cancelled": true }));
        close_extension_editor(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
    ctx.tui.set_focus(Some(editor));
    ctx.tui.request_render(false);
}

fn cancel_js_dialog_ui(ctx: &CommandContext, bridge: &Arc<JsDialogBridge>) {
    for id in bridge.cancelled_active_ids() {
        // Several commands can ask for a dialog concurrently. Only the id
        // currently occupying the TUI slot may close the visible component;
        // an older cancellation must leave a newer ask dialog untouched.
        if !bridge.is_visible(&id) {
            bridge.finish(&id);
            continue;
        }
        if let Some((selector, _)) = ctx.state.active_selector.lock().unwrap().clone() {
            selector.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        } else if ctx.state.extension_dialog_open() {
            if !run_extension_cancel(&ctx.state) {
                close_extension_editor(&ctx.state, &ctx.editor_container, &ctx.editor, &ctx.tui);
            }
        }
        // `respond` normally removes the active entry from the callback. The
        // fallback path above can run before a callback was installed, so
        // always discard the id after routing the cancellation.
        bridge.finish(&id);
    }
}

// ===========================================================================
// ask_user — plugin UI-dialog bridge (runtime action 17)
// ===========================================================================

/// Sentinel option value that switches a freeform-capable selector into the
/// single-line input slot (native `allowFreeform` affordance).
const ASK_USER_FREEFORM: &str = "\u{0}ask-user-freeform";

/// One in-flight `ask_user` request being shown in the TUI. Unlike
/// [`JsDialogBridge`], the plugin side is a synchronous `poll` loop that reads
/// answers back through the shared [`rpi_extensions::UiDialogMailbox`], so this
/// bridge only tracks which request id currently occupies the input slot.
#[derive(Clone)]
struct AskUserBridge {
    mailbox: rpi_extensions::UiDialogMailbox,
    visible: Arc<Mutex<Option<String>>>,
}

impl AskUserBridge {
    fn new(mailbox: rpi_extensions::UiDialogMailbox) -> Self {
        Self {
            mailbox,
            visible: Arc::new(Mutex::new(None)),
        }
    }

    /// Mark the interactive consumer as available. Idempotent.
    fn attach(&self) {
        self.mailbox.attach();
    }

    /// Take the next pending request and mark it visible. Only the single key
    /// loop calls this while no other dialog is open, so the visible-id
    /// bookkeeping is race-free without a separate lock.
    fn take_pending(&self) -> Option<rpi_extensions::UiDialogRequest> {
        let mut visible = self.visible.lock().ok()?;
        if visible.is_some() {
            return None;
        }
        let request = self.mailbox.take_pending()?;
        *visible = Some(request.request_id.clone());
        Some(request)
    }

    fn clear_visible(&self, id: &str) {
        if let Ok(mut visible) = self.visible.lock() {
            if visible.as_deref() == Some(id) {
                *visible = None;
            }
        }
    }

    /// Record an answer for `id` and release the input slot.
    fn respond(&self, id: &str, answer: serde_json::Value) {
        let _ = self.mailbox.respond(id, answer);
        self.clear_visible(id);
    }

    /// Cancel `id` and release the input slot.
    fn cancel(&self, id: &str) {
        let _ = self.mailbox.cancel(id);
        self.clear_visible(id);
    }

    /// Cancel every outstanding request (run abort). Pending prompts from the
    /// aborted turn must not surface in a later turn.
    fn cancel_all(&self) {
        self.mailbox.cancel_all();
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
    }

    /// Detach + cancel everything (TUI shutdown) so a parked plugin `poll`
    /// observes a terminal state instead of blocking on a dead session.
    fn shutdown(&self) {
        self.mailbox.detach();
        if let Ok(mut visible) = self.visible.lock() {
            *visible = None;
        }
    }
}

/// Parsed presentation data for one `ask_user` question. Supports both the
/// flat native question contract and the earlier `questions[0]` alias, plus a
/// `confirm` summary.
#[derive(Clone)]
struct AskUserPrompt {
    question_id: String,
    question: String,
    header: Option<String>,
    context: Option<String>,
    options: Vec<(String, Option<String>)>,
    allow_multiple: bool,
    allow_freeform: bool,
    suggest: Option<String>,
    kind: String,
}

fn ask_user_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

fn ask_user_bool(value: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_bool))
}

fn ask_user_options(value: &serde_json::Value) -> Vec<(String, Option<String>)> {
    let mut options = Vec::new();
    let Some(items) = value.get("options").and_then(serde_json::Value::as_array) else {
        return options;
    };
    for item in items {
        if let Some(title) = item.as_str() {
            let title = title.trim();
            if !title.is_empty() {
                options.push((title.to_string(), None));
            }
            continue;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let title = ["title", "label", "text", "value", "name"]
            .iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let Some(title) = title else { continue };
        let description = object
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        options.push((title.to_string(), description));
    }
    options
}

fn parse_ask_user_prompt(request: &rpi_extensions::UiDialogRequest) -> AskUserPrompt {
    let ui = &request.ui;
    let kind = ui
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("selector")
        .to_string();
    // A request may carry a single flat question or a `questions[]` array. The
    // plugin emits one question per request, but accepting `questions[0]` keeps
    // the host compatible with hosts/plugins that batch.
    let nested = ui
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.first());
    let source = nested.filter(|value| value.is_object()).unwrap_or(ui);

    let question = ask_user_str(source, &["question", "summary", "title"])
        .or_else(|| ask_user_str(ui, &["question", "summary", "title"]))
        .unwrap_or_default();
    let question_id = ask_user_str(source, &["id", "questionId"])
        .or_else(|| ask_user_str(ui, &["id", "questionId"]))
        .unwrap_or_else(|| request.request_id.clone());
    let header = ask_user_str(source, &["header"]).or_else(|| ask_user_str(ui, &["header"]));
    let context = ask_user_str(source, &["context", "message"])
        .or_else(|| ask_user_str(ui, &["context", "message"]));
    let mut options = ask_user_options(source);
    if options.is_empty() {
        options = ask_user_options(ui);
    }
    if kind == "confirm" && options.is_empty() {
        options = vec![("Yes".to_string(), None), ("No".to_string(), None)];
    }
    let allow_multiple = ask_user_bool(source, &["allowMultiple", "allow_multiple", "multiple"])
        .or_else(|| ask_user_bool(ui, &["allowMultiple", "allow_multiple", "multiple"]))
        .unwrap_or(false);
    // Freeform defaults on when there is nothing to pick from, matching the
    // "no options ⇒ free input" contract.
    let allow_freeform =
        ask_user_bool(source, &["allowFreeform", "allow_freeform", "allow_custom"])
            .or_else(|| ask_user_bool(ui, &["allowFreeform", "allow_freeform", "allow_custom"]))
            .unwrap_or(options.is_empty());
    let suggest = ask_user_str(source, &["suggest", "placeholder"])
        .or_else(|| ask_user_str(ui, &["suggest", "placeholder"]));

    AskUserPrompt {
        question_id,
        question,
        header,
        context,
        options,
        allow_multiple,
        allow_freeform,
        suggest,
        kind,
    }
}

/// Build the shared header frame for an ask-user prompt.
/// Uses markdown-aware rendering for question and context content.
fn ask_user_frame(prompt: &AskUserPrompt, body: Arc<dyn Component>, hint: &str) -> Arc<Container> {
    let frame = Arc::new(Container::new());
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame.add_child(Arc::new(Spacer::new(1)));
    if let Some(header) = prompt.header.as_deref() {
        frame.add_child(extension_dialog_title_md(header, true));
    }
    if !prompt.question.trim().is_empty() {
        frame.add_child(extension_dialog_title_md(
            &prompt.question,
            prompt.header.is_none(),
        ));
    }
    if let Some(context) = prompt.context.as_deref() {
        frame.add_child(extension_dialog_hint_md(context));
    }
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(body);
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(extension_dialog_hint(hint));
    frame.add_child(Arc::new(Spacer::new(1)));
    frame.add_child(Arc::new(DynamicBorder::new()));
    frame
}

/// Open one `ask_user` request in the TUI input slot.
fn open_ask_user_dialog(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
) {
    let prompt = parse_ask_user_prompt(&request);
    if prompt.kind == "input" || prompt.options.is_empty() {
        open_ask_user_input(ctx, bridge, request, prompt);
    } else {
        open_ask_user_selector(ctx, bridge, request, prompt);
    }
}

fn open_ask_user_selector(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
    prompt: AskUserPrompt,
) {
    let multi = prompt.allow_multiple && prompt.kind != "confirm";
    let mut items: Vec<SelectItem> = Vec::new();
    for (title, description) in &prompt.options {
        let item = SelectItem::new(title, title);
        let item = match description.as_deref() {
            Some(description) => item.with_description(description),
            None => item,
        };
        items.push(item);
    }
    // A freeform-capable single selector gets an extra sentinel row that opens
    // the input slot for a typed answer.
    if prompt.allow_freeform && !multi && prompt.kind != "confirm" {
        items.push(SelectItem::new(ASK_USER_FREEFORM, "Type another answer…"));
    }
    let list = if multi {
        Arc::new(SelectList::new_multi(items, 10))
    } else {
        Arc::new(SelectList::new(items, 10))
    };

    let request_id = request.request_id.clone();
    let state = ctx.state.clone();
    let editor_container = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();

    // Single-select: respond with the chosen title, or fall through to the
    // input slot for the freeform sentinel.
    let bridge_select = bridge.clone();
    let request_for_freeform = request.clone();
    let prompt_for_freeform = AskUserPrompt {
        question_id: prompt.question_id.clone(),
        question: prompt.question.clone(),
        header: prompt.header.clone(),
        context: prompt.context.clone(),
        options: Vec::new(),
        allow_multiple: false,
        allow_freeform: true,
        suggest: prompt.suggest.clone(),
        kind: "input".to_string(),
    };
    let state_select = state.clone();
    let ec_select = editor_container.clone();
    let editor_select = editor.clone();
    let tui_select = tui.clone();
    let ctx_select = ctx.clone();
    list.on_select(Arc::new(move |item: &SelectItem| {
        if item.value == ASK_USER_FREEFORM {
            close_selector(&state_select, &ec_select, &editor_select, &tui_select);
            open_ask_user_input(
                &ctx_select,
                bridge_select.clone(),
                request_for_freeform.clone(),
                prompt_for_freeform.clone(),
            );
            return;
        }
        bridge_select.respond(
            &request_id,
            serde_json::json!({
                "values": [item.display_value()],
                "kind": "selector",
            }),
        );
        close_selector(&state_select, &ec_select, &editor_select, &tui_select);
    }));

    if multi {
        let bridge_multi = bridge.clone();
        let request_id_multi = request.request_id.clone();
        let state_multi = state.clone();
        let ec_multi = editor_container.clone();
        let editor_multi = editor.clone();
        let tui_multi = tui.clone();
        list.on_multi_select(Arc::new(move |selected: &[SelectItem]| {
            let values: Vec<String> = selected
                .iter()
                .map(|item| item.display_value().to_string())
                .collect();
            bridge_multi.respond(
                &request_id_multi,
                serde_json::json!({"values": values, "kind": "selector"}),
            );
            close_selector(&state_multi, &ec_multi, &editor_multi, &tui_multi);
        }));
    }

    let bridge_cancel = bridge.clone();
    let request_id_cancel = request.request_id.clone();
    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        bridge_cancel.cancel(&request_id_cancel);
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    let hint = if multi {
        "Space toggle · Enter submit · Esc cancel"
    } else {
        "Enter select · Esc cancel"
    };
    let frame = ask_user_frame(&prompt, list.clone(), hint);
    open_selector_with_view(
        &state,
        &editor_container,
        &editor,
        &tui,
        SelectorView::List(list),
        frame,
        SelectorKind::Extension,
    );
}

fn open_ask_user_input(
    ctx: &CommandContext,
    bridge: AskUserBridge,
    request: rpi_extensions::UiDialogRequest,
    prompt: AskUserPrompt,
) {
    let placeholder = prompt
        .suggest
        .clone()
        .unwrap_or_else(|| "Type your answer…".to_string());
    let input = Arc::new(Input::with_placeholder(&placeholder));
    input.set_focused(true);

    let frame = ask_user_frame(&prompt, input.clone(), "Enter submit · Esc cancel");
    *ctx.state.active_extension_editor.lock().unwrap() = None;
    *ctx.state.active_extension_input.lock().unwrap() = Some(input.clone());
    ctx.editor_container.clear();
    ctx.editor_container.add_child(frame);
    ctx.tui.set_focus(Some(input.clone()));
    ctx.tui.request_render(false);

    let state = ctx.state.clone();
    let ec = ctx.editor_container.clone();
    let editor = ctx.editor.clone();
    let tui = ctx.tui.clone();
    let bridge_submit = bridge.clone();
    let request_id = request.request_id.clone();
    input.on_submit(Arc::new(move |text: &str| {
        bridge_submit.respond(
            &request_id,
            serde_json::json!({
                "values": [text],
                "value": text,
                "text": text,
                "kind": "input",
            }),
        );
        close_extension_editor(&state, &ec, &editor, &tui);
    }));

    let state_cancel = ctx.state.clone();
    let ec_cancel = ctx.editor_container.clone();
    let editor_cancel = ctx.editor.clone();
    let tui_cancel = ctx.tui.clone();
    let bridge_cancel = bridge.clone();
    let request_id_cancel = request.request_id.clone();
    *ctx.state.active_extension_cancel.lock().unwrap() = Some(Arc::new(move || {
        bridge_cancel.cancel(&request_id_cancel);
        close_extension_editor(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));
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
        ctx.state.cancel_js_preparation();
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

struct ChangelogCommand;
impl SlashCommand for ChangelogCommand {
    fn name(&self) -> &'static str {
        "/changelog"
    }
    fn description(&self) -> &'static str {
        "Show recent release changes"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        add_changelog_message(&ctx.chat);
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
            //
            // Native pi only switches on an EXACT match; a term that matches
            // nothing falls through to the selector with the query prefilled
            // instead of erroring out (`showModelSelector(searchTerm)`).
            if let Some(model) = find_model_selector_match(&ctx.model_catalog, term) {
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
            (!term.is_empty()).then_some(term),
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
        open_theme_selector(
            &ctx.state,
            &ctx.editor_container,
            &ctx.editor,
            &ctx.tui,
            &ctx.cwd,
            &ctx.package_resources,
        );
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
        "Export session (supports: md, html, jsonl)"
    }
    fn execute(&self, ctx: &CommandContext, args: &str) {
        let format = args.trim().to_lowercase();
        if format.is_empty() || format == "md" || format == "markdown" {
            let _ = ctx.tx.send(TuiMessage::ExportSession);
        } else if format == "html" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Html,
            ));
        } else if format == "jsonl" {
            let _ = ctx.tx.send(TuiMessage::ExportSessionWithFormat(
                crate::export::ExportFormat::Jsonl,
            ));
        } else {
            // Invalid format, show error
            add_error_message(
                &ctx.chat,
                &format!(
                    "Unknown export format: {}. Supported: md, html, jsonl",
                    args.trim()
                ),
            );
            ctx.tui.request_render(false);
        }
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

fn set_project_trust_for_command(
    cwd: &std::path::Path,
    value: Option<bool>,
) -> Result<(), crate::config::ConfigError> {
    crate::config::set_project_trust(cwd, value)
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
        match set_project_trust_for_command(&ctx.cwd, value) {
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
            &ctx.cwd,
            &ctx.package_resources,
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
        // Live session stats are async, so ask the main loop to append them.
        let _ = ctx.tx.send(TuiMessage::ShowUsage);
        ctx.tui.request_render(false);
    }
}

/// `/usage` — token/cost/cache totals read through the product-layer
/// `AgentSession`. This is the same numbers the footer shows, but expanded and
/// with the cache-hit breakdown that the one-line footer cannot fit.
struct UsageCommand;
impl SlashCommand for UsageCommand {
    fn name(&self) -> &'static str {
        "/usage"
    }
    fn description(&self) -> &'static str {
        "Show token, cost, and cache totals for this session"
    }
    fn execute(&self, ctx: &CommandContext, _args: &str) {
        let _ = ctx.tx.send(TuiMessage::ShowUsage);
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
    r.register(Arc::new(ChangelogCommand));
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
    r.register(Arc::new(UsageCommand));
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

fn register_js_extension_commands(
    registry: &mut CommandRegistry,
    session: Option<crate::js_extensions::JsExtensionSession>,
) {
    let Some(session) = session else {
        return;
    };
    for command in &session.commands {
        let name = if command.starts_with('/') {
            command.clone()
        } else {
            format!("/{command}")
        };
        if registry.find(&name).is_none() {
            registry.register(Arc::new(JsExtensionCommand {
                name,
                session: session.clone(),
            }));
        }
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
    /// Export session with specific format
    ExportSessionWithFormat(crate::export::ExportFormat),
    /// Show live usage/context stats (from `/usage` and `/context`).
    ShowUsage,
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
    /// Result returned after Ctrl+G edits a temporary file in an external
    /// editor. Handling it on the async loop keeps editor mutation single-
    /// threaded with the rest of the TUI state.
    ExternalEditorResult(Result<String, String>),
    /// A user-initiated `!command` / `!!command` shell run finished. Routed to
    /// the main loop so the `bashExecution` transcript record is persisted in
    /// order relative to agent runs (native pi's `recordBashResult` /
    /// `_pendingBashMessages`).
    UserBashFinished(UserBashReport),
}

/// Terminal result of a user-initiated `!command` shell run.
///
/// Carries everything needed to build the persisted `bashExecution` message
/// plus the `!!` context-exclusion flag. See `start_user_bash`.
struct UserBashReport {
    command: String,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    exclude_from_context: bool,
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
fn tool_result_message_text(msg: &rpi_ai::types::ToolResultMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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

/// Interactive `/settings` menu.
///
/// A [`SettingsList`] of editable settings (native pi's `SettingsSelectorComponent`):
/// Enter/Space cycles a row's value in place, and the theme/model/thinking/scope
/// rows open a sub-selector that replaces the active selector (the
/// `active_selector` slot is single). Value changes apply immediately where the
/// running TUI can honor them and are always persisted to `settings.json`, so
/// rows marked "applies on restart" take effect next launch.
fn open_settings_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    lane_model_id: &str,
    chat: &Arc<Container>,
    cwd: &std::path::Path,
    package_resources: &Arc<crate::packages::PackageResources>,
) {
    let settings = crate::settings::load_settings().unwrap_or_default();
    let items = settings_menu_items(&settings, state, lane_model_id);
    let list = Arc::new(SettingsList::new(items));

    // ---- submenu rows ----
    // These reuse the existing sub-selectors, which already persist their own
    // choices. The settings menu is replaced by the sub-selector (single
    // `active_selector` slot); cancelling the sub-selector returns to the editor.
    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let lane_sel = lane.clone();
    let catalog_sel = catalog.to_vec();
    let lane_model_sel = lane_model_id.to_string();
    let chat_sel = chat.clone();
    let cwd_sel = cwd.to_path_buf();
    let package_resources_sel = package_resources.clone();
    list.on_select(Arc::new(move |item| match item.key.as_str() {
        "theme" => open_settings_theme_selector(
            &state_sel,
            &ec_sel,
            &editor_sel,
            &tui_sel,
            &chat_sel,
            &cwd_sel,
            &package_resources_sel,
        ),
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
    }));

    // ---- value rows ----
    // Apply live where possible, then persist so the choice survives a restart.
    let state_change = state.clone();
    let chat_change = chat.clone();
    let tui_change = tui.clone();
    list.on_change(Arc::new(move |key, value| {
        let mut settings = crate::settings::load_settings().unwrap_or_default();
        let applied = apply_setting_change(&state_change, key, value, &mut settings);
        let saved = crate::settings::save_settings(&settings);
        if let Some(note) = applied {
            let suffix = if saved.is_ok() { "" } else { " (not saved)" };
            add_note_message(&chat_change, &format!("{note}{suffix}"));
        }
        tui_change.request_render(false);
    }));

    let state_cancel = state.clone();
    let ec_cancel = editor_container.clone();
    let editor_cancel = editor.clone();
    let tui_cancel = tui.clone();
    list.on_cancel(Arc::new(move || {
        close_selector(&state_cancel, &ec_cancel, &editor_cancel, &tui_cancel);
    }));

    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::Settings(list.clone()),
        list,
        SelectorKind::Settings,
    );
}

/// Build the `/settings` rows from the loaded settings plus the live TUI state.
///
/// Descriptions carry "(applies on restart)" for values resolved once at
/// startup, so the menu never implies an effect the running session cannot
/// apply.
fn settings_menu_items(
    settings: &crate::settings::Settings,
    state: &Arc<TuiState>,
    lane_model_id: &str,
) -> Vec<SettingItem> {
    let current_theme = settings.theme.clone().unwrap_or_else(|| "dark".to_string());
    let current_model = settings
        .default_model
        .clone()
        .unwrap_or_else(|| lane_model_id.to_string());
    let current_thinking = settings
        .default_thinking_level
        .clone()
        .unwrap_or_else(|| "(default)".to_string());
    let scoped_desc = match &settings.scoped_models {
        Some(list) if !list.is_empty() => format!("{} model(s)", list.len()),
        _ => "all models".to_string(),
    };
    let hide_thinking = state.hide_thinking();
    let show_images = state.show_images.lock().map(|guard| *guard).unwrap_or(true);
    let show_cache_miss = settings.show_cache_miss_notices.unwrap_or(false);
    let http_idle = settings
        .http_idle_timeout_ms()
        .map(|ms| {
            if ms == 0 {
                "disabled".to_string()
            } else {
                format!("{}s", ms / 1000)
            }
        })
        .unwrap_or_else(|| "5m".to_string());

    vec![
        SettingItem::new("theme", "Theme", &current_theme)
            .with_description("Color theme for the interface")
            .with_submenu(),
        SettingItem::new("model", "Default model", &current_model)
            .with_description("Saved default model for new sessions")
            .with_submenu(),
        SettingItem::new("thinking", "Default thinking", &current_thinking)
            .with_description("Saved default thinking level")
            .with_submenu(),
        SettingItem::new("scoped-models", "Cycle scope", &scoped_desc)
            .with_description("Models enabled for Ctrl+M cycling")
            .with_submenu(),
        SettingItem::new(
            "hide-thinking",
            "Hide thinking",
            bool_setting_value(hide_thinking),
        )
        .with_description("Hide thinking blocks in assistant responses")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "show-images",
            "Show images",
            bool_setting_value(show_images),
        )
        .with_description("Render images inline in the terminal")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "cache-miss-notices",
            "Cache miss notices",
            bool_setting_value(show_cache_miss),
        )
        .with_description("Transcript notices for prompt-cache costs")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "quiet-startup",
            "Quiet startup",
            bool_setting_value(settings.quiet_startup.unwrap_or(false)),
        )
        .with_description("Disable the verbose startup listing (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "terminal-progress",
            "Terminal progress",
            bool_setting_value(settings.show_terminal_progress().unwrap_or(true)),
        )
        .with_description("OSC 9;4 progress in the terminal tab bar (applies on restart)")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            bool_setting_value(settings.fullscreen_copy_on_select.unwrap_or(true)),
        )
        .with_description("Copy selected text automatically in fullscreen mode")
        .with_values(&["true", "false"]),
        SettingItem::new(
            "double-escape-action",
            "Double-escape action",
            settings
                .double_escape_action
                .clone()
                .unwrap_or_else(|| "tree".to_string())
                .as_str(),
        )
        .with_description("Esc Esc with an empty editor (applies on restart)")
        .with_values(&["tree", "fork", "none"]),
        SettingItem::new(
            "editor-padding",
            "Editor padding",
            settings.editor_padding_x.unwrap_or(1).to_string().as_str(),
        )
        .with_description("Horizontal editor padding, 0-3 (applies on restart)")
        .with_values(&["0", "1", "2", "3"]),
        SettingItem::new(
            "autocomplete-max-items",
            "Autocomplete max items",
            settings
                .autocomplete_max_visible
                .unwrap_or(5)
                .to_string()
                .as_str(),
        )
        .with_description("Max autocomplete rows, 3-20 (applies on restart)")
        .with_values(&["3", "5", "7", "10", "15", "20"]),
        SettingItem::new("http-idle-timeout", "HTTP idle timeout", &http_idle)
            .with_description("Idle gap allowed while awaiting HTTP data (applies on restart)")
            .with_values(&["30s", "1m", "5m", "disabled"]),
    ]
}

/// `"true"` / `"false"` for a settings row value.
fn bool_setting_value(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Apply one `/settings` value change to the running TUI and to `settings`.
///
/// Returns a note to show in the transcript when the change is user-visible;
/// `None` for changes that only take effect on the next launch.
fn apply_setting_change(
    state: &Arc<TuiState>,
    key: &str,
    value: &str,
    settings: &mut crate::settings::Settings,
) -> Option<String> {
    let as_bool = || value == "true";
    match key {
        // Live: hide/show thinking blocks in the transcript immediately.
        "hide-thinking" => {
            state.set_hide_thinking(as_bool());
            settings.hide_thinking_block = Some(as_bool());
            Some(format!(
                "Thinking blocks {}.",
                if as_bool() { "hidden" } else { "shown" }
            ))
        }
        // Live: the image flag is consulted when images would be rendered.
        "show-images" => {
            if let Ok(mut guard) = state.show_images.lock() {
                *guard = as_bool();
            }
            settings.show_images = Some(as_bool());
            Some(format!(
                "Inline images {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Live: read from `state` once per cache-miss notice.
        "cache-miss-notices" => {
            if let Ok(mut guard) = state.cache_miss_notices.lock() {
                *guard = as_bool();
            }
            settings.show_cache_miss_notices = Some(as_bool());
            Some(format!(
                "Cache miss notices {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Live: read from settings on each selection mouse-up.
        "fullscreen-copy-on-select" => {
            settings.fullscreen_copy_on_select = Some(as_bool());
            Some(format!(
                "Copy on select {}.",
                if as_bool() { "enabled" } else { "disabled" }
            ))
        }
        // Persisted only: resolved once at startup.
        "quiet-startup" => {
            settings.quiet_startup = Some(as_bool());
            None
        }
        "terminal-progress" => {
            settings.show_terminal_progress = Some(as_bool());
            None
        }
        "double-escape-action" => {
            settings.double_escape_action = Some(value.to_string());
            None
        }
        "editor-padding" => {
            settings.editor_padding_x = value.parse().ok();
            None
        }
        "autocomplete-max-items" => {
            settings.autocomplete_max_visible = value.parse().ok();
            None
        }
        "http-idle-timeout" => {
            settings.http_idle_timeout = Some(match value {
                "disabled" => serde_json::Value::String("disabled".to_string()),
                "30s" => serde_json::json!(30_000),
                "1m" => serde_json::json!(60_000),
                _ => serde_json::json!(300_000),
            });
            None
        }
        _ => None,
    }
}

/// Apply a theme choice AND persist it to settings.json (`/settings` → Theme).
fn open_settings_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    chat: &Arc<Container>,
    cwd: &std::path::Path,
    package_resources: &Arc<crate::packages::PackageResources>,
) {
    let mut items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    if state.themes_enabled {
        for path in package_resources.theme_files() {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                items.push(SelectItem::new(name, name).with_description("Package theme"));
            }
        }
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = chat.clone();
    let cwd_sel = cwd.to_path_buf();
    let package_resources_sel = package_resources.clone();
    list.on_select(Arc::new(move |item| {
        let preset = match item.value.as_str() {
            "light" => Some(ThemePreset::Light),
            "monochrome" => Some(ThemePreset::Monochrome),
            "dark" => Some(ThemePreset::Dark),
            name => {
                if state_sel.themes_enabled {
                    if let Ok(custom) = crate::packages::load_theme_with_resources(
                        &cwd_sel,
                        name,
                        &package_resources_sel,
                    ) {
                        rpi_tui::global_theme_manager().set(custom.clone());
                        state_sel.theme_manager.set(custom);
                    }
                }
                add_note_message(&chat_sel, &format!("Theme set to {}.", item.label));
                close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
                tui_sel.render_now(true);
                return;
            }
        };
        let Some(preset) = preset else { return };
        apply_theme_preset(preset);
        state_sel.theme_manager.apply_preset(preset);
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
    let items = model_selector_items(catalog, lane_model_id);
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

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Default model"),
        Some("Type to filter by name, provider, or id"),
        list,
        SelectorKind::Settings,
    );
}

/// Convert the authenticated runtime catalog into selector rows. Keep the
/// model id as the value so `/model <id>` and the selection callback share one
/// lookup path, while making the provider visible for OpenAI-compatible
/// gateways where the same model id may exist at multiple endpoints.
fn model_selector_items(catalog: &[rpi_ai::Model], lane_model_id: &str) -> Vec<SelectItem> {
    let mut seen = std::collections::HashSet::new();
    catalog
        .iter()
        .filter(|m| {
            seen.insert((
                m.api.clone(),
                m.provider.to_ascii_lowercase(),
                m.id.to_ascii_lowercase(),
            ))
        })
        .map(|m| {
            let label = if m.name.is_empty() {
                short_model_name(&m.id)
            } else {
                m.name.clone()
            };
            let identity = if matches!(m.api, rpi_ai::Api::AnthropicMessages)
                && m.provider.eq_ignore_ascii_case("anthropic")
            {
                m.id.clone()
            } else {
                format!("{}/{}", m.provider, m.id)
            };
            let marker = if m.id.eq_ignore_ascii_case(lane_model_id) {
                " (current)"
            } else {
                ""
            };
            // Native pi ranks provider-prefixed queries first, so the bare id
            // is not the leading token (`getModelSelectorSearchText`).
            let name = if m.name.is_empty() {
                String::new()
            } else {
                format!(" {}", m.name)
            };
            let search_text = format!(
                "{} {}/{} {} {}{}",
                m.provider, m.provider, m.id, m.provider, m.id, name
            );
            SelectItem::new(&m.id, &label)
                .with_description(&format!("{identity}{marker}"))
                .with_search_text(&search_text)
        })
        .collect()
}

/// Resolve a selector input by either bare model id or the qualified
/// `provider/model` identity shown for gateway models. This keeps manual
/// `/model ...` input consistent with the rows rendered by the selector.
fn find_model_selector_match(catalog: &[rpi_ai::Model], input: &str) -> Option<rpi_ai::Model> {
    let (provider, id) = input
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .map_or((None, input), |(provider, id)| (Some(provider), id));
    catalog
        .iter()
        .find(|model| {
            model.id.eq_ignore_ascii_case(id)
                && provider.map_or(true, |provider| {
                    model.provider.eq_ignore_ascii_case(provider)
                        || (provider.eq_ignore_ascii_case("anthropic")
                            && matches!(model.api, rpi_ai::Api::AnthropicMessages))
                })
        })
        .cloned()
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

// Session export is driven through the product-layer `AgentSession`
// (`crate::agent_session`), which owns format selection + default filenames —
// the TUI loop calls `AgentSession::export*` directly, so there is no separate
// helper here.

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
                    if let Some(error) = assistant_error_text(a) {
                        add_error_message(chat, &error);
                    }
                    rendered_any = true;
                }
                AgentMessage::ToolResult(result) => {
                    // Tool results are persisted as separate message entries,
                    // not as part of the assistant text. Restore them as
                    // completed tool panels so resumed sessions show the
                    // command output as well as the user's prompts.
                    let comp = Arc::new(ToolExecutionComponent::new(&result.tool_name, ""));
                    comp.set_result(&tool_result_message_text(result), result.is_error);
                    chat.add_child(comp);
                    chat.add_child(Arc::new(Spacer::new(1)));
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

/// Path to the enclosing repository's `.git/HEAD`, if any.
fn find_git_head_path(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let head = current.join(".git").join("HEAD");
        if head.exists() {
            return Some(head);
        }
        dir = current.parent();
    }
    None
}

/// Current git branch for `cwd`, or `None` outside a repository / on error.
/// Reads `.git/HEAD` directly (no `git` subprocess) so the footer can call it
/// cheaply at startup.
fn git_branch_for(cwd: &std::path::Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let head = current.join(".git").join("HEAD");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            let trimmed = contents.trim();
            if let Some(reference) = trimmed.strip_prefix("ref: ") {
                let branch = reference
                    .rsplit('/')
                    .next()
                    .unwrap_or(reference)
                    .trim()
                    .to_string();
                if !branch.is_empty() {
                    return Some(branch);
                }
            }
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(7).collect());
            }
        }
        dir = current.parent();
    }
    None
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

/// The component receiving keys while a selector is open.
///
/// Plain selectors route straight to the [`SelectList`]; searchable ones route
/// through [`SearchableSelectList`] so typing filters instead of being ignored.
/// Both variants expose the underlying list for callback registration and
/// selection inspection.
#[derive(Clone)]
enum SelectorView {
    /// A bare `SelectList` (no search box).
    List(Arc<SelectList>),
    /// A `SelectList` wrapped with a fuzzy-filter input.
    Searchable(Arc<SearchableSelectList>),
    /// The `/settings` menu: value rows cycle in place, submenu rows open a
    /// nested selector.
    Settings(Arc<SettingsList>),
}

impl SelectorView {
    /// Route a key to the appropriate child.
    fn handle_key(&self, key: KeyEvent) {
        match self {
            Self::List(list) => list.handle_key(key),
            Self::Searchable(searchable) => searchable.handle_key(key),
            Self::Settings(settings) => settings.handle_key(key),
        }
    }
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
    /// Whether package/custom themes may be selected in this session.
    themes_enabled: bool,
    /// Persisted display preference toggled by Ctrl+T.
    hide_thinking: std::sync::Mutex<bool>,
    /// Global tool-output expansion preference toggled by Ctrl+O.
    tool_outputs_expanded: std::sync::Mutex<bool>,
    /// Whether the native-style terminal progress indicator is enabled.
    show_terminal_progress: bool,
    /// Run status for the status indicator + interrupt routing.
    status: std::sync::Mutex<RunStatus>,
    /// Extension status registry (`SetStatus` runtime action). The render tick
    /// repaints the footer only when `ext_status_revision` moved.
    ext_status: rpi_extensions::ExtensionStatusMailbox,
    /// Revision of `ext_status` as of the last footer write.
    ext_status_revision: std::sync::atomic::AtomicU64,
    /// Cancellation signal for the short phase that starts the persistent JS
    /// host and runs `before_agent_start`. The key thread can trigger this
    /// directly while the async message loop is awaiting the blocking worker.
    js_preparation_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// Cancellation signal for a user-initiated `!command` / `!!command` shell
    /// run. `Some` while the command executes; Esc/Ctrl+C cancels it. `None`
    /// when no user bash is running.
    user_bash_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// `bashExecution` messages produced by `!command` while an agent run was
    /// in flight. Flushed to the lane on `AgentEnd` so transcript order matches
    /// native pi's `_pendingBashMessages` (never spliced mid-turn).
    pending_bash_messages: std::sync::Mutex<Vec<AgentMessage>>,
    /// The footer, updated live by the drain task.
    footer: Arc<FooterComponent>,
    /// The status-container (status slot in the dock) — cleared/filled with a
    /// loader while a run is active.
    status_container: Arc<Container>,
    /// The chat transcript container.
    chat_container: Arc<Container>,
    /// The active loader shown while `Working`.
    loader: Arc<Loader>,
    /// The editor, so status changes can drive its top-border working
    /// indicator (the spinner + elapsed time shown inside the input box border).
    editor: Arc<Editor>,
    /// The last finalized assistant text (for `/copy`). Updated by the drain
    /// task on `MessageEnd` / `AgentEnd`.
    last_assistant_text: std::sync::Mutex<String>,
    /// The active selector overlay, swapped into the editor slot. `Some` while
    /// a selector is open; the key loop routes to it first and restores the
    /// editor on done/cancel.
    active_selector: std::sync::Mutex<Option<(SelectorView, SelectorKind)>>,
    /// Extension-provided editor currently occupying the input slot.
    active_extension_editor: std::sync::Mutex<Option<Arc<Editor>>>,
    /// Single-line input currently occupying the input slot for an extension.
    active_extension_input: std::sync::Mutex<Option<Arc<Input>>>,
    /// Callback used to resolve an extension dialog with a cancellation action.
    active_extension_cancel: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// The autocomplete manager (slash + @file providers) consulted on every
    /// editor keystroke.
    autocomplete: AutocompleteManager,
    /// The container rendered above the editor holding the live autocomplete
    /// suggestion list (cleared when there are no suggestions).
    autocomplete_container: Arc<Container>,
    /// Maximum number of autocomplete rows rendered above the editor.
    autocomplete_max_visible: usize,
    /// Images queued from clipboard paste and attached to the next prompt.
    pending_images: std::sync::Mutex<Vec<rpi_ai::types::ImageContent>>,
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
    /// Transcript notices for prompt-cache costs and provider recovery
    /// diagnostics (native pi `showCacheMissNotices`, default `false`).
    cache_miss_notices: std::sync::Mutex<bool>,
    /// Submitted-message history for ↑/↓ recall, most recent first (mirrors
    /// the TS editor `history` array). Bounded at [`HISTORY_LIMIT`].
    history: std::sync::Mutex<Vec<String>>,
    /// Browse index while recalling history: -1 = not browsing, 0 = most
    /// recent, 1 = older, … Reset to -1 on every submit.
    history_index: std::sync::Mutex<isize>,
    /// The editor text captured when entering browse mode, restored when the
    /// user navigates back past the newest entry (TS `historyDraft`).
    history_draft: std::sync::Mutex<Option<String>>,
    /// Full cache-waste tracker (pi `detectCacheMiss`/`cache-stats.ts`):
    /// counts and prices prompt-cache misses across turns using the same
    /// noise floor, idle-gap, and model-change logic as the batch scan.
    cache_tracker: std::sync::Mutex<rpi_harness::cache_stats::CacheMissTracker>,
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
    /// Transcript search handler (Ctrl+Shift+F).
    search: Arc<AltScreenSearch>,
    /// Search bar component shown when search is active.
    search_bar: Arc<SearchBar>,
    /// Dock slot listing queued steering/follow-up messages while a run is
    /// active (native pi's `pendingMessagesContainer`). Empty when nothing is
    /// queued, so it renders zero rows and never affects the layout.
    pending_container: Arc<Container>,
    /// Display text for the `app.message.dequeue` binding (e.g. `Alt+Q`),
    /// shown in the pending-messages hint row.
    dequeue_hint: String,
    /// Last queue snapshot rendered into `pending_container`. Lets
    /// `set_pending_queue` skip redundant rebuilds + frame requests when the
    /// polled queue is unchanged.
    pending_snapshot: std::sync::Mutex<rpi_harness::agent_harness::QueuedMessages>,
    /// Fullscreen selection tracking for auto-copy.
    selection_start: std::sync::Mutex<Option<(u16, u16)>>,
    selection_end: std::sync::Mutex<Option<(u16, u16)>>,
}

/// How many submitted messages are kept for ↑ recall (mirrors the TS
/// editor's 100-entry cap).
const HISTORY_LIMIT: usize = 100;

/// Keep a few rows of overlap so page scrolling preserves visual context,
/// matching the upstream fullscreen viewport behavior.
const PAGE_SCROLL_OVERLAP: usize = 4;

/// Native pi scrolls a small chunk for each wheel notch rather than moving the
/// transcript one physical row at a time. Three lines stays precise while
/// avoiding the sluggish feel of the previous implementation.
const MOUSE_WHEEL_SCROLL_LINES: i32 = 3;

/// Parse the compact key notation used by native Pi settings (for example
/// `ctrl+g`, `shift+tab`, or `escape`) into crossterm's representation.
fn parse_configured_key(value: &str) -> Option<rpi_tui::KeyCombo> {
    let mut modifiers = KeyModifiers::NONE;
    let mut key = None;
    for part in value.trim().to_ascii_lowercase().split('+') {
        match part {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "alt" | "option" => modifiers |= KeyModifiers::ALT,
            "super" | "cmd" | "command" | "meta" => modifiers |= KeyModifiers::SUPER,
            part if !part.is_empty() => key = Some(part.to_string()),
            _ => {}
        }
    }
    let key = key?;
    let code = match key.as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                return Some(rpi_tui::KeyCombo::new(
                    KeyCode::BackTab,
                    modifiers & !KeyModifiers::SHIFT,
                ));
            }
            KeyCode::Tab
        }
        "backspace" | "back" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" | "arrowup" => KeyCode::Up,
        "down" | "arrowdown" => KeyCode::Down,
        "left" | "arrowleft" => KeyCode::Left,
        "right" | "arrowright" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page-up" => KeyCode::PageUp,
        "pagedown" | "page-down" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),
        value if value.chars().count() == 1 => KeyCode::Char(value.chars().next().unwrap()),
        _ => return None,
    };
    Some(rpi_tui::KeyCombo::new(code, modifiers))
}

fn configured_keybindings() -> Arc<rpi_tui::Keybindings> {
    let mut bindings = rpi_tui::Keybindings::new();
    let settings = crate::settings::load_settings().unwrap_or_default();
    let Some(overrides) = settings.keybindings else {
        rpi_tui::set_keybindings(bindings.clone());
        return Arc::new(bindings);
    };
    let known: &[(&str, rpi_tui::KeybindingId)] = &[
        ("app.interrupt", rpi_tui::keybindings::keys::INTERRUPT),
        ("app.clear", rpi_tui::keybindings::keys::CLEAR),
        ("app.exit", rpi_tui::keybindings::keys::EXIT),
        ("app.model.select", rpi_tui::keybindings::keys::MODEL_SELECT),
        (
            "app.model.cycleForward",
            rpi_tui::keybindings::keys::MODEL_CYCLE_FORWARD,
        ),
        ("app.tools.expand", rpi_tui::keybindings::keys::TOOLS_EXPAND),
        (
            "app.thinking.toggle",
            rpi_tui::keybindings::keys::THINKING_TOGGLE,
        ),
        (
            "app.editor.external",
            rpi_tui::keybindings::keys::EXTERNAL_EDITOR,
        ),
        (
            "app.thinking.cycle",
            rpi_tui::keybindings::keys::THINKING_CYCLE,
        ),
        (
            "app.clipboard.pasteImage",
            rpi_tui::keybindings::keys::PASTE_IMAGE,
        ),
        ("app.message.dequeue", rpi_tui::keybindings::keys::DEQUEUE),
    ];
    for (name, id) in known {
        let Some(value) = overrides.get(*name) else {
            continue;
        };
        let values: Vec<String> = match value {
            serde_json::Value::String(value) => vec![value.clone()],
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::Null => Vec::new(),
            _ => continue,
        };
        let combos: Vec<_> = values
            .iter()
            .filter_map(|value| parse_configured_key(value))
            .collect();
        if values.is_empty() || !combos.is_empty() {
            bindings.set(id, combos);
        }
    }
    rpi_tui::set_keybindings(bindings.clone());
    Arc::new(bindings)
}

fn keybinding_matches(
    bindings: &rpi_tui::Keybindings,
    event: &crossterm::event::KeyEvent,
    id: rpi_tui::KeybindingId,
) -> bool {
    if bindings.matches(event, id) {
        return true;
    }
    // crossterm reports Shift+Tab as BackTab on some terminals and as Tab
    // plus Shift on others. Treat both forms as the same configured action.
    if event.code == KeyCode::BackTab {
        let normalized =
            crossterm::event::KeyEvent::new(KeyCode::Tab, event.modifiers | KeyModifiers::SHIFT);
        bindings.matches(&normalized, id)
    } else {
        false
    }
}

/// Display text for the `app.message.dequeue` binding (e.g. `Alt+Q`), used in
/// the pending-messages hint row. Mirrors native pi's `getAppKeyDisplay`;
/// falls back to native pi's platform default when nothing is configured.
fn pending_dequeue_hint(bindings: &rpi_tui::Keybindings) -> String {
    let keys = bindings.get_keys(rpi_tui::keybindings::keys::DEQUEUE);
    if keys.is_empty() {
        return if cfg!(windows) { "Alt+Q" } else { "Alt+Up" }.to_string();
    }
    keys.iter()
        .map(|combo| combo.display())
        .collect::<Vec<_>>()
        .join("/")
}

fn double_escape_trigger(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_some_and(|previous| {
        now.duration_since(previous) <= std::time::Duration::from_millis(500)
    })
}

/// How long to wait for queued console input before treating a bare Enter as a
/// real submit. Pasted input is already in the queue, so this only needs to
/// cover scheduler latency — small enough that a human Enter feels instant.
const PASTE_PROBE: std::time::Duration = std::time::Duration::from_millis(4);

/// Maximum gap between a pasted character and the next pasted key. A human
/// cannot deliver two keypresses this fast, so anything tighter is paste.
const PASTE_BURST_GAP: std::time::Duration = std::time::Duration::from_millis(20);

/// Whether a bare Enter is pasted content (insert a newline) rather than a
/// submit.
///
/// `crossterm` 0.27 only implements bracketed paste on Unix; on Windows the
/// console event source never produces `Event::Paste`, so a pasted block
/// arrives as plain key events with a bare Enter per line. Two timing signals
/// separate a pasted Enter from a typed one:
///
/// - `more_queued`: real key input is already sitting in the console input
///   queue. Callers must feed this through [`probe_paste_input`], which drops
///   the key's own Release events — Windows queues press+release together over
///   RDP, and a bare poll cannot tell that Release apart from paste content.
/// - `last_text_key_at`: the Enter lands within [`PASTE_BURST_GAP`] of the
///   preceding pasted character, which also catches a paste that *ends* with a
///   newline (nothing queued behind it, but it was not typed by hand).
///
/// NOTE: the caller gates this on `cfg!(windows)`. On Unix, bracketed paste
/// delivers real pastes as `Event::Paste`, and the timing heuristic would
/// misclassify a remote/mobile client's coalesced text+Enter burst as paste
/// ("回车变成了换行").
fn enter_is_paste_burst(
    last_text_key_at: Option<std::time::Instant>,
    now: std::time::Instant,
    more_queued: bool,
) -> bool {
    if more_queued {
        return true;
    }
    last_text_key_at.is_some_and(|previous| now.duration_since(previous) < PASTE_BURST_GAP)
}

/// Decide whether more *real* input is queued behind the current key, for the
/// Windows paste-burst check.
///
/// Windows emits a `KeyEventKind::Release` for every press, usually queued
/// right behind the `Press`. A bare non-blocking poll therefore reports "more
/// input" for a lone Enter — which made a remote/RDP Enter look like a paste
/// burst. This drains and discards release events and returns the first
/// *meaningful* event it read (stashed so the caller's loop still processes it)
/// plus whether any real input was queued. `next_event` returns `None` when
/// nothing is available within the probe window.
fn probe_paste_input(
    pending: Option<Event>,
    mut next_event: impl FnMut() -> Option<Event>,
) -> (bool, Option<Event>) {
    let mut pending = pending;
    let mut more_queued = pending.is_some();
    while !more_queued {
        match next_event() {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Release => continue,
            Some(ev) => {
                pending = Some(ev);
                more_queued = true;
            }
            None => break,
        }
    }
    (more_queued, pending)
}

/// Milliseconds since the last text key, for the [`enter_is_paste_burst`] trace
/// line (`-` when there was no preceding text key).
fn gap_ms(last_text_key_at: Option<std::time::Instant>) -> String {
    match last_text_key_at {
        Some(previous) => std::time::Instant::now()
            .duration_since(previous)
            .as_millis()
            .to_string(),
        None => "-".to_string(),
    }
}

fn transcript_page_size(viewport_height: usize) -> i32 {
    viewport_height
        .saturating_sub(PAGE_SCROLL_OVERLAP)
        .max(1)
        .min(i32::MAX as usize) as i32
}

fn should_dispatch_key(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Release
}

/// Last-resort quit, run on the key worker thread.
///
/// The async loop owns `run_prompt_streaming(..).await` for the whole run, so a
/// queued [`TuiMessage::Exit`] is not read while a run is in flight. When the
/// user asks to quit a second time — the first Ctrl+C only reached `Aborting` —
/// treat the run as wedged, restore the terminal, and exit.
///
/// `130` is the conventional `128 + SIGINT` status, so a wrapper script can tell
/// a forced quit from a clean one.
fn emergency_exit(tui: &Arc<TuiAltScreen>) -> ! {
    // Best effort: leave the alternate buffer and restore cooked mode so the
    // user's shell is usable afterwards. `stop` also joins the render
    // scheduler, so no in-flight frame can repaint over the restored screen.
    tui.stop(Default::default());
    eprintln!("\n\x1b[2mrpi: aborted (forced quit).\x1b[0m");
    std::process::exit(130);
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
    fn begin_js_preparation(&self) -> CancellationToken {
        let cancellation = CancellationToken::new();
        if let Some(previous) = self
            .js_preparation_cancel
            .lock()
            .unwrap()
            .replace(cancellation.clone())
        {
            previous.cancel();
        }
        cancellation
    }

    fn finish_js_preparation(&self) {
        self.js_preparation_cancel.lock().unwrap().take();
    }

    fn cancel_js_preparation(&self) -> bool {
        let cancellation = self.js_preparation_cancel.lock().unwrap().take();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    /// Whether a user-initiated `!command` shell run is currently active.
    fn user_bash_running(&self) -> bool {
        self.user_bash_cancel.lock().unwrap().is_some()
    }

    /// Register a cancellation token for a new user bash run, cancelling any
    /// previous one (mirrors `begin_js_preparation`).
    fn begin_user_bash(&self) -> CancellationToken {
        let cancellation = CancellationToken::new();
        if let Some(previous) = self
            .user_bash_cancel
            .lock()
            .unwrap()
            .replace(cancellation.clone())
        {
            previous.cancel();
        }
        cancellation
    }

    /// Clear the user-bash slot. Called when the command finishes so the
    /// running guard and the Esc router both see idle state.
    fn finish_user_bash(&self) {
        self.user_bash_cancel.lock().unwrap().take();
    }

    /// Cancel a running user bash command. Returns `true` when one was active
    /// (mirrors `cancel_js_preparation`). The spawned task clears the slot
    /// itself once `execute_shell_with_capture` returns.
    fn cancel_user_bash(&self) -> bool {
        let cancellation = self.user_bash_cancel.lock().unwrap().as_ref().cloned();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = status;
        self.apply_status(status);
    }

    /// The dock container listing queued messages (native pi's
    /// `pendingMessagesContainer`).
    fn pending_container(&self) -> &Arc<Container> {
        &self.pending_container
    }

    /// Render the current queue into the pending dock slot. Mirrors native
    /// pi's `updatePendingMessagesDisplay`: `Steering: <text>` /
    /// `Follow-up: <text>` rows (dim) plus a `↳ <key> to edit all queued
    /// messages` hint. A no-op when the snapshot is unchanged, so the poll
    /// caller can invoke it freely without forcing frames.
    fn set_pending_queue(&self, messages: rpi_harness::agent_harness::QueuedMessages) {
        {
            let mut current = self.pending_snapshot.lock().unwrap();
            if *current == messages {
                return;
            }
            *current = messages.clone();
        }
        let container = &self.pending_container;
        container.clear();
        if messages.is_empty() {
            return;
        }
        let colors = current_theme().colors;
        container.add_child(Arc::new(Spacer::new(1)));
        for text in &messages.steering {
            container.add_child(Arc::new(Text::new(
                colors.muted.fg(&format!("Steering: {text}")),
                1,
                0,
            )));
        }
        for text in &messages.follow_up {
            container.add_child(Arc::new(Text::new(
                colors.muted.fg(&format!("Follow-up: {text}")),
                1,
                0,
            )));
        }
        container.add_child(Arc::new(Text::new(
            colors.muted.fg(&format!(
                "↳ {} to edit all queued messages",
                self.dequeue_hint
            )),
            1,
            0,
        )));
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
                // The status dock already shows the active loader. Keep the
                // footer focused on the model and shortcuts instead of
                // repeating a second `Working…` at the bottom of the TUI.
                self.footer.set_status("");
                // Reflect the in-flight turn in the terminal window/tab title
                // (OSC 2). No-op when `tui` is absent (unit tests).
                if let Some(tui) = &self.tui {
                    tui.set_title("🦀π rpi ⟳");
                }
                self.status_container.clear();
                // The working indicator renders inside the editor's top border.
                // `show_terminal_progress` disables it entirely.
                if self.show_terminal_progress {
                    self.editor.set_working(Some(WorkingState {
                        frame: 0,
                        started_at: std::time::Instant::now(),
                        message: "Working…".to_string(),
                    }));
                } else {
                    self.editor.set_working(None);
                }
            }
            RunStatus::Aborting => {
                self.footer.set_status("Aborting…");
                // Do not leave a frozen "Working" spinner on screen after the
                // render tick intentionally stops advancing in this state.
                self.loader.stop();
                self.status_container.clear();
                self.editor.set_working(None);
            }
            RunStatus::Idle => {
                self.footer.set_status("");
                if let Some(tui) = &self.tui {
                    tui.set_title("🦀π rpi");
                }
                self.loader.stop();
                self.status_container.clear();
                self.editor.set_working(None);
            }
        }
    }

    /// Push the extension status registry into the footer when it changed.
    /// Returns `true` when the footer was rewritten (the caller then requests a
    /// repaint — an idle session would otherwise never redraw).
    fn sync_extension_status(&self) -> bool {
        let revision = self.ext_status.revision();
        if self
            .ext_status_revision
            .load(std::sync::atomic::Ordering::SeqCst)
            == revision
        {
            return false;
        }
        self.ext_status_revision
            .store(revision, std::sync::atomic::Ordering::SeqCst);
        self.footer.set_extension_status(&self.ext_status.text());
        true
    }

    /// The bash panel has its own `Running...` spinner. Keep the global
    /// `Working...` loader out of the status slot while any bash tool is active
    /// so the same operation is not presented as two simultaneous loaders.
    fn sync_working_loader_with_bash(&self) {
        if *self.status.lock().unwrap() != RunStatus::Working {
            return;
        }

        // The working indicator now lives inside the editor's top border and
        // stays visible for the whole Working state, so no status-slot swap is
        // needed when bash panels (which render their own spinner in the
        // transcript) appear or disappear.
        self.status_container.clear();
    }

    fn show_retry(&self, attempt: u32, max_retries: u32, delay_ms: u64) {
        *self.status.lock().unwrap() = RunStatus::Working;
        self.footer.set_status("");
        if let Some(tui) = &self.tui {
            tui.set_title("🦀π rpi ↻");
        }
        self.loader.stop();
        self.status_container.clear();
        self.status_container
            .add_child(Arc::new(StatusIndicator::retry(
                attempt,
                max_retries,
                std::time::Duration::from_millis(delay_ms),
            )));
    }

    /// Whether a selector overlay is currently open (routes keys to it first).
    fn selector_open(&self) -> bool {
        self.active_selector.lock().unwrap().is_some()
    }

    fn extension_editor_open(&self) -> bool {
        self.active_extension_editor.lock().unwrap().is_some()
    }

    fn extension_input_open(&self) -> bool {
        self.active_extension_input.lock().unwrap().is_some()
    }

    fn extension_dialog_open(&self) -> bool {
        self.extension_editor_open() || self.extension_input_open()
    }

    fn set_hide_thinking(&self, hide: bool) {
        *self.hide_thinking.lock().unwrap() = hide;
        if let Some(comp) = self.current_assistant.lock().unwrap().as_ref() {
            comp.set_hide_thinking(hide);
        }
    }

    fn hide_thinking(&self) -> bool {
        *self.hide_thinking.lock().unwrap()
    }

    fn toggle_thinking(&self) -> bool {
        let next = !self.hide_thinking();
        self.set_hide_thinking(next);
        next
    }

    fn toggle_tool_outputs(&self) -> bool {
        let next = !*self.tool_outputs_expanded.lock().unwrap();
        *self.tool_outputs_expanded.lock().unwrap() = next;
        for comp in self.tool_components.lock().unwrap().values() {
            comp.set_expanded(next);
        }
        for comp in self.bash_components.lock().unwrap().values() {
            comp.set_expanded(next);
        }
        next
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
        // A model switch changes the context window, so the `%/{window}` badge
        // must rescale against the same last-reported token count.
        self.footer.set_context_window(model.context_window as i64);
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

    fn queue_image(&self, image: rpi_ai::types::ImageContent) {
        self.pending_images.lock().unwrap().push(image);
    }

    fn take_pending_images(&self) -> Vec<rpi_ai::types::ImageContent> {
        std::mem::take(&mut *self.pending_images.lock().unwrap())
    }
}

// ===========================================================================
// interactive_tui — the entry point
// ===========================================================================

#[derive(Debug, PartialEq)]
struct TuiStartupSettings {
    editor_padding_x: usize,
    autocomplete_max_visible: usize,
    hide_thinking: bool,
    quiet_startup: bool,
    show_terminal_progress: bool,
    /// Render images inline (`terminal.showImages` / flat `showImages`).
    show_images: bool,
    /// Transcript notices for prompt-cache costs (`showCacheMissNotices`).
    cache_miss_notices: bool,
}

fn preferred_project_setting<T>(
    project_settings: &[crate::settings::Settings],
    field: impl Fn(&crate::settings::Settings) -> Option<T>,
) -> Option<T> {
    project_settings.iter().find_map(field)
}

fn should_show_startup_listing(verbose: bool, quiet_startup: bool) -> bool {
    verbose || !quiet_startup
}

fn git_only_update_resources(
    mut resources: crate::packages::PackageResources,
) -> crate::packages::PackageResources {
    resources
        .packages
        .retain(|package| matches!(package.source, crate::packages::PackageSource::Git));
    resources
}

/// Resolve TUI-only settings with native Pi's project-over-global precedence.
/// `project_settings` must be ordered `.rpi` then `.pi`; an explicit `Some`
/// wins even when the value is `false` or zero.
fn resolve_tui_startup_settings(
    global: &crate::settings::Settings,
    project_settings: &[crate::settings::Settings],
    project_trusted: bool,
) -> TuiStartupSettings {
    let project_settings = if project_trusted {
        project_settings
    } else {
        &[]
    };
    let editor_padding_x =
        preferred_project_setting(project_settings, |settings| settings.editor_padding_x)
            .or(global.editor_padding_x)
            .unwrap_or(1)
            .min(16);
    let autocomplete_max_visible = preferred_project_setting(project_settings, |settings| {
        settings.autocomplete_max_visible
    })
    .or(global.autocomplete_max_visible)
    .unwrap_or(5)
    .clamp(1, 20);
    let hide_thinking =
        preferred_project_setting(project_settings, |settings| settings.hide_thinking_block)
            .or(global.hide_thinking_block)
            .unwrap_or(false);
    let quiet_startup =
        preferred_project_setting(project_settings, |settings| settings.quiet_startup)
            .or(global.quiet_startup)
            .unwrap_or(false);
    let show_terminal_progress = preferred_project_setting(project_settings, |settings| {
        settings.show_terminal_progress()
    })
    .or_else(|| global.show_terminal_progress())
    .unwrap_or(true);
    let show_images =
        preferred_project_setting(project_settings, |settings| settings.show_images())
            .or_else(|| global.show_images())
            .unwrap_or(true);
    let cache_miss_notices = preferred_project_setting(project_settings, |settings| {
        settings.show_cache_miss_notices
    })
    .or(global.show_cache_miss_notices)
    .unwrap_or(false);

    TuiStartupSettings {
        editor_padding_x,
        autocomplete_max_visible,
        hide_thinking,
        quiet_startup,
        show_terminal_progress,
        show_images,
        cache_miss_notices,
    }
}

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
    initial_images: Vec<rpi_ai::types::ImageContent>,
    theme: Option<&str>,
    no_themes: bool,
    reload_context: &crate::session::ReloadContext,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // Reuse the startup snapshot captured before provider/session setup. An
    // extension may mutate the process cwd while loading; that must not change
    // which project settings or package update roots this TUI observes.
    let cwd = reload_context.cwd.clone();
    let saved_settings = crate::settings::load_settings().unwrap_or_default();
    let project_trusted = reload_context.project_trusted;
    let project_settings = if project_trusted {
        crate::settings::load_project_settings(&cwd)
    } else {
        Vec::new()
    };
    let tui_settings =
        resolve_tui_startup_settings(&saved_settings, &project_settings, project_trusted);
    let editor_padding_x = tui_settings.editor_padding_x;
    let autocomplete_max_visible = tui_settings.autocomplete_max_visible;
    let mut hide_thinking = tui_settings.hide_thinking;
    let show_terminal_progress = tui_settings.show_terminal_progress;
    let show_images = tui_settings.show_images;
    let cache_miss_notices = tui_settings.cache_miss_notices;
    let quiet_startup = tui_settings.quiet_startup;
    let update_checks_enabled =
        !args.offline && std::env::var_os("RPI_DISABLE_UPDATE_CHECK").is_none();

    // Session display preferences (durable, latest-wins) override the
    // settings-file defaults. These are per-session facts stored as custom
    // entries (see `rpi_harness::session::values`), so resuming a session with
    // `-c`/`-r` restores the thinking/tool-output display the user last chose
    // there — independent of global settings. Absent keys leave the settings
    // value untouched.
    let session_values = rpi_harness::session::SessionValues::load(harness.session())
        .await
        .unwrap_or_default();
    if let Some(persisted) = session_values.get_as::<bool>("display.hide_thinking") {
        hide_thinking = persisted;
    }

    // Resolve the active model once, up front. The full id feeds the TuiState
    // tracking field + the selectors/key loop (which run on a blocking thread
    // and can't await `lane.get_model()`); the short name feeds the footer.
    let lane_model_id = lane.get_model().await.map(|m| m.id).unwrap_or_default();
    let model_name = short_model_name(&lane_model_id);

    // Snapshot startup capabilities for the welcome screen. Both accessors
    // return defensive clones, so rendering this summary does not retain a
    // harness lock or trigger a second resource scan.
    let mut active_tool_names = lane.get_active_tools().await.unwrap_or_default();
    // AgentHarness uses an empty active-name list as the default "all tools"
    // state. Do not expose that implementation sentinel as `Tools (0) none`
    // in the welcome banner (it is especially visible on the first prompt).
    if active_tool_names.is_empty() {
        active_tool_names = harness
            .get_tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|tool| tool.tool.schema().name.clone())
            .collect();
    }
    let resources_snapshot = harness.get_resources().await.unwrap_or_default();
    let skill_names: Vec<String> = resources_snapshot
        .skills
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|skill| skill.name.clone())
        .collect();

    // Resolve package resources for @file autocomplete + session discovery.
    let package_resources = reload_context.package_resources.clone();

    // Channel between the key/callback threads and the main async loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<TuiMessage>();

    // Apply the saved theme before constructing transcript components. Some
    // components keep styled text, so doing this after the welcome banner left
    // the first screen in the dark palette until it was rebuilt.
    let theme_manager = Arc::new(ThemeManager::new());
    if let Some(preset) = match theme {
        Some("light") => Some(ThemePreset::Light),
        Some("monochrome") => Some(ThemePreset::Monochrome),
        Some("dark") => Some(ThemePreset::Dark),
        _ => None,
    } {
        apply_theme_preset(preset);
        theme_manager.apply_preset(preset);
    } else if !no_themes {
        if let Some(name) = theme {
            match crate::packages::load_theme_with_resources(&cwd, name, &package_resources) {
                Ok(custom) => {
                    rpi_tui::global_theme_manager().set(custom.clone());
                    theme_manager.set(custom);
                }
                Err(error) => {
                    eprintln!("warning: could not load package theme `{name}`: {error}");
                }
            }
        }
    }

    // ---- TUI + containers ----
    let terminal = Box::new(ProcessTerminal::new());
    let tui = Arc::new(TuiAltScreen::new(terminal, true, None));
    let js_dialog_bridge = Arc::new(JsDialogBridge::default());
    // Plugin `ask_user` prompts (runtime action 17). Attaching marks this
    // session as interactive so a detached/headless host fails loudly instead
    // of returning a fabricated answer. The mailbox is session-scoped (held in
    // `ReloadContext`) so in-flight prompts survive a `/reload`.
    let ask_user_bridge = AskUserBridge::new(reload_context.ui_dialog.clone());
    ask_user_bridge.attach();
    tui.set_main_screen_mode(matches!(args.tui_mode, crate::args::TuiMode::Regular));

    if let Some(js) = &reload_context.js_extension_session {
        if let Err(error) = js.install_ui_runtime(tui.clone()) {
            eprintln!("warning: could not enable JS custom UI bridge: {error}");
        } else if let Some(js_active) = js.active_tools() {
            // Apply the discovery-time JS subset while preserving Rust
            // built-ins already active in the harness lane. The real TUI
            // lifecycle reconciliation runs after the key worker starts below.
            let js_names = js.tool_names();
            let mut active = lane.get_active_tools().await.unwrap_or_default();
            active.retain(|name| {
                crate::session::tool_name_allowed(name, args)
                    && !js_names.iter().any(|js_name| js_name == name)
            });
            active.extend(js_active.into_iter().filter(|name| {
                js_names.iter().any(|js_name| js_name == name)
                    && crate::session::tool_name_allowed(name, args)
            }));
            active = crate::session::filter_active_tool_names(active, args);
            let _ = lane.set_active_tools(active).await;
            active_tool_names = lane.get_active_tools().await.unwrap_or_default();
        }
    }

    let chat_container = Arc::new(Container::new());
    if let Some(js) = &reload_context.js_extension_session {
        // Tool contexts do not have a command-result envelope. Route
        // `ctx.ui.notify()` through the live transcript so notifications from
        // tools such as ask_user_question are visible immediately.
        let chat_notify = chat_container.clone();
        let tui_notify = tui.clone();
        if let Err(error) = js.add_runtime_handler(Arc::new(move |action, args| {
            if action != "ui.notify" {
                return Err(format!("unsupported capability: {action}"));
            }
            let message = args
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !message.is_empty() {
                if args.get("level").and_then(serde_json::Value::as_str) == Some("error") {
                    add_error_message(&chat_notify, message);
                } else {
                    add_note_message(&chat_notify, message);
                }
                tui_notify.request_render(false);
            }
            Ok(serde_json::json!(true))
        })) {
            if args.verbose {
                eprintln!("warning: could not enable JS notification bridge: {error}");
            }
        }
    }
    if should_show_startup_listing(args.verbose, quiet_startup) {
        add_welcome_message_with_capabilities(&chat_container, &active_tool_names, &skill_names);
    }

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
    // Add bottom padding to the output area for visual breathing room.
    document_container.add_child(Arc::new(Spacer::new(1)));

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
    let keybindings = configured_keybindings();
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: editor_padding_x,
            autocomplete_max_visible,
            ..Default::default()
        },
        EditorStyle::default(),
        keybindings.clone(),
    ));

    // Bash-mode border: a `!`-prefixed draft recolors the editor border to the
    // bash accent (native pi's `updateEditorBorderColor` / `isBashMode`). The
    // editor fires `on_change` for keystrokes and for programmatic
    // `clear`/`set_text`, so the color also resets after a `!command` submits.
    {
        let editor_for_change = editor.clone();
        editor.on_change(Arc::new(move |text: &str| {
            let colors = current_theme().colors;
            let is_bash = text.trim_start().starts_with('!');
            editor_for_change.set_border_color(if is_bash {
                Some(colors.bash_mode)
            } else {
                None
            });
        }));
    }

    // ---- Footer + status ----
    let footer = Arc::new(FooterComponent::new());
    footer.set_model(&model_name);
    footer.set_cwd(&cwd.to_string_lossy());
    footer.set_git_branch(git_branch_for(&cwd).as_deref());
    if let Some(m) = model_catalog.iter().find(|m| m.id == lane_model_id) {
        footer.set_context_window(m.context_window as i64);
    }
    footer.set_hints("Enter: Send | Shift+Enter: New line | Ctrl+C: Abort/Exit | Esc: Abort | Ctrl+L: Model | Ctrl+M: Cycle | Ctrl+T: Expand tool | /help");

    // Live git-branch refresh (native fs-watch on `.git/HEAD`): a checkout or
    // commit updates the footer's branch without a manual refresh.
    let _branch_watcher = find_git_head_path(&cwd).map(|head| {
        let footer_ref = footer.clone();
        let cwd_ref = cwd.clone();
        crate::fs_watch::watch_path(head, move || {
            footer_ref.set_git_branch(git_branch_for(&cwd_ref).as_deref());
        })
    });

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
    register_js_extension_commands(
        &mut command_registry,
        reload_context.js_extension_session.clone(),
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

    let tool_outputs_expanded = session_values
        .get_as::<bool>("display.tool_outputs_expanded")
        .unwrap_or(false);
    let state = Arc::new(TuiState {
        current_assistant: std::sync::Mutex::new(None),
        tool_components: std::sync::Mutex::new(HashMap::new()),
        bash_components: std::sync::Mutex::new(HashMap::new()),
        themes_enabled: !no_themes,
        hide_thinking: std::sync::Mutex::new(hide_thinking),
        tool_outputs_expanded: std::sync::Mutex::new(tool_outputs_expanded),
        show_terminal_progress,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: reload_context.ext_status.clone(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: footer.clone(),
        status_container: status_container.clone(),
        chat_container: chat_container.clone(),
        loader: loader.clone(),
        editor: editor.clone(),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete,
        autocomplete_container: autocomplete_container.clone(),
        autocomplete_max_visible,
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager,
        tui: Some(tui.clone()),
        current_model_id: std::sync::Mutex::new(lane_model_id.clone()),
        show_images: std::sync::Mutex::new(show_images),
        cache_miss_notices: std::sync::Mutex::new(cache_miss_notices),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: std::sync::Mutex::new(initial_transformer),
        extension_session: reload_context.extension_session.clone(),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: pending_dequeue_hint(&keybindings),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
        selection_start: std::sync::Mutex::new(None),
        selection_end: std::sync::Mutex::new(None),
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
        StackChild::Entry(StackEntry::new(state.pending_container().clone())),
        StackChild::Entry(StackEntry::new(status_container.clone())),
        StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
        // NOTE: no Spacer here. The scroll content already ends with a blank line
        // (`document_container`), and a second one in the dock stacked into a
        // two-row gap between the last message and the input box. One row of
        // breathing room is the whole safe area now; the transcript keeps its
        // own padding so a long message never touches the editor border.
        StackChild::Entry(
            StackEntry::new(editor_container.clone())
                .shrink(0)
                .min_size(3),
        ),
        StackChild::Entry(StackEntry::new(state.search_bar.clone())),
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
    //
    // The product-layer `AgentSession` wraps a clone of the harness (the
    // harness handle is cheap and shares the session/bus), so `/usage`,
    // `/export`, and future product commands go through one surface.
    let scoped_models = args
        .model
        .as_deref()
        .map(crate::agent_session::ScopedModel::parse)
        .into_iter()
        .collect();
    let agent_session = Arc::new(crate::agent_session::AgentSession::new(
        harness.clone(),
        model_catalog.clone(),
        scoped_models,
        cwd.clone(),
    ));
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
        package_resources: package_resources.clone(),
        resources: resources_arc.clone(),
        reload_context: Arc::new(reload_context.clone()),
        session: agent_session.clone(),
    };

    if let Some(js) = &reload_context.js_extension_session {
        let bridge = js_dialog_bridge.clone();
        if let Err(error) = js.install_ui_dialog_runtime(Arc::new(move |action, args| {
            bridge.handle_runtime_request(action, args)
        })) {
            eprintln!("warning: could not enable JS dialog UI bridge: {error}");
        }
    }

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

        // User-initiated shell mode: `!command` runs in the background and is
        // recorded in the session context; `!!command` is excluded from it.
        // Handled before the agent prompt path so it never reaches the model.
        // Mirrors `interactive-mode.ts:3225`.
        if let Some((command, exclude_from_context)) = parse_user_bash(text) {
            if ctx_for_cb.state.user_bash_running() {
                add_error_message(
                    &ctx_for_cb.chat,
                    "A bash command is already running. Press Esc to cancel it first.",
                );
                ctx_for_cb.tui.request_render(false);
                return;
            }
            start_user_bash(&ctx_for_cb, command, exclude_from_context);
            return;
        }

        let run_status = *ctx_for_cb.state.status.lock().unwrap();
        if run_status != RunStatus::Idle {
            let message = AgentMessage::User(UserMessage::new(text.to_string(), 0));
            let lane = ctx_for_cb.lane.clone();
            let chat = ctx_for_cb.chat.clone();
            let tui = ctx_for_cb.tui.clone();
            let state = ctx_for_cb.state.clone();
            tokio::spawn(async move {
                // Queue immediately while the agent loop is still running.
                // Routing this through the TUI's main channel delayed it until
                // `prompt_text()` returned, after the loop's drain points had
                // passed, so the queued message appeared to disappear.
                //
                // Native pi's `steer()` enqueues unconditionally, even after
                // the run ends, and the message stays in the queue for the
                // next run. This matches the TS design and avoids the race
                // between `activeRun` clearing and the status check.
                if let Err(error) = lane.steer(message).await {
                    add_error_message(&chat, &format!("Could not queue message: {error}"));
                    tui.render_now(false);
                    return;
                }
                // Surface the queued entry in the pending dock immediately;
                // the drain-time refresh only fires on the next agent event.
                refresh_pending_messages(&state, &lane).await;
                tui.request_render(false);
            });
            // A queued prompt is echoed only once the loop actually consumes
            // it: the run's `AgentEvent::MessageStart` renders the user bubble
            // (native pi renders `message_start` for user messages). Until
            // then it is surfaced by the pending-messages display, so the
            // transcript never shows a message that is still waiting.
            ctx_for_cb.tui.request_render(false);
            return;
        }

        if !ctx_for_cb.state.try_start_working() {
            return;
        }

        // Render the user bubble here, NOT from `AgentEvent::MessageStart`.
        //
        // The harness persists the prompt itself and then drives
        // `run_agent_loop` with an EMPTY prompts vec (so the prompt is not
        // double-counted in the provider context), which means the loop emits
        // no `message_start` for it — see
        // `directly_sent_prompt_emits_no_user_message_start` in
        // `crates/pi-harness/tests/harness_run_e2e.rs`. Queued steering /
        // follow-up prompts DO get a `message_start` (the loop drains those
        // itself), so those are deliberately rendered from the event instead.
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

    // Turn on the native-pi frame throttle before the first frame: from here on
    // `request_render(false)` parks a frame and the scheduler thread paints at
    // most one per `MIN_RENDER_INTERVAL_MS` (16 ms). A provider that streams one
    // `MessageUpdate` per delta would otherwise force a full transcript
    // re-render + full-viewport terminal write per token, which is what made
    // long output saturate the terminal (and its GPU compositor). Immediate
    // renders (selectors, the first frame, `render_now(..)`) are unaffected.
    tui.start_render_scheduler();
    tui.start_readerless();

    // Consume local self-update results even when network checks are disabled.
    // Keep package discovery separate so package managers and Git remotes
    // cannot delay an rpi update notice or a prior helper failure.
    let mut update_check_handles = Vec::new();
    let rpi_chat = chat_container.clone();
    let rpi_tui = tui.clone();
    update_check_handles.push(tokio::spawn(async move {
        let report = crate::updates::check_rpi_startup().await;
        if !report.is_empty() {
            add_update_notices(&rpi_chat, &report);
        }
        rpi_tui.request_render(false);
    }));

    if update_checks_enabled {
        let update_args = args.clone();
        let update_cwd = cwd.clone();
        let update_project_trusted = project_trusted;
        let chat = chat_container.clone();
        let tui = tui.clone();
        update_check_handles.push(tokio::spawn(async move {
            let resources = crate::session::package_resources_for_update_check(
                &update_args,
                &update_cwd,
                update_project_trusted,
            );
            let report = match crate::npm::NpmCommand::resolve(&update_cwd, update_project_trusted)
            {
                Ok(npm_command) => {
                    crate::updates::check_package_startup_with_resources_and_npm_command_in_cwd(
                        Some(&resources),
                        &npm_command,
                        update_project_trusted.then_some(update_cwd.as_path()),
                    )
                    .await
                }
                Err(error) => {
                    add_error_message(
                        &chat,
                        &format!("npm package update checks disabled: {error}"),
                    );
                    let git_resources = git_only_update_resources(resources);
                    crate::updates::check_package_startup_with_resources(Some(&git_resources)).await
                }
            };
            if !report.is_empty() {
                add_update_notices(&chat, &report);
            }
            tui.request_render(false);
        }));
    }

    // ---- Streaming drain task ----
    let drain_handle = if let Some(rx) = event_rx {
        let tui_drain = tui.clone();
        let state_drain = state.clone();
        let chat_drain = chat_container.clone();
        let lane_drain = lane.clone();
        Some(tokio::spawn(async move {
            drain_agent_events(rx, tui_drain, state_drain, chat_drain, lane_drain).await;
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
        // 80ms per frame — native pi's `DEFAULT_INTERVAL_MS`, i.e. a full
        // 10-frame cycle every 800ms. The tick only *requests a repaint*; the
        // frame is advanced by `Editor::render` (mirroring `Loader::render`),
        // so streaming output — which schedules a frame per token — spins it
        // faster, and this interval is the floor when the model is silent.
        //
        // Advancing here as well would double-step every tick and make the idle
        // spinner run at 40ms/frame, twice native pi's rate.
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(
            rpi_tui::loader::SPINNER_FRAME_MS,
        ));
        interval.tick().await; // discard immediate
        loop {
            interval.tick().await;
            // Extension status (langfuse ✓ …, …) — cheap revision check, and the
            // only reason an idle session repaints its footer.
            if state_tick.sync_extension_status() {
                tui_tick.request_render(false);
            }
            let working = *state_tick.status.lock().unwrap() == RunStatus::Working;
            if working {
                // A live transcript panel — a running bash command *or* a
                // running tool call — computes its elapsed readout from
                // `Instant::now()` at render time. A frame that reuses the
                // cached scroll content never rebuilds it, so the readout
                // froze at the second of the last full rebuild (reported: a
                // `search` call stuck at "2.0s" with no way to tell it was
                // still running). Rebuild while any such panel is live.
                let bash_present = !state_tick.bash_components.lock().unwrap().is_empty();
                let has_live_panel = {
                    let tools = state_tick.tool_components.lock().unwrap();
                    transcript_has_live_panel(bash_present, &tools)
                };
                if has_live_panel {
                    tui_tick.request_render(false);
                } else {
                    // Only the dock loader animates. Keep the already-rendered
                    // transcript instead of rebuilding a long history at 12.5
                    // frames per second.
                    tui_tick.request_render_reusing_scroll_content();
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
    let model_catalog_for_key = model_catalog_arc.clone();
    let js_for_key = reload_context.js_extension_session.clone();
    let js_dialog_for_key = js_dialog_bridge.clone();
    let ask_user_for_key = ask_user_bridge.clone();
    // Ctrl+L routes through the same registry as `/model` (one path, not two),
    // so the key loop needs the same `CommandContext` + registry the submit
    // handler uses. All fields are `Arc`/cheap, so this clone is free.
    let ctx_for_key = ctx.clone();
    let registry_for_key = registry.clone();
    let keybindings_for_key = keybindings.clone();
    let double_escape_action = crate::settings::load_settings()
        .ok()
        .and_then(|settings| settings.double_escape_action)
        .unwrap_or_else(|| "tree".to_string())
        .to_ascii_lowercase();

    let key_handle = tokio::task::spawn_blocking(move || {
        let mut last_escape_time = None;
        // Paste-burst tracking (see the bare-Enter guard below): the instant of
        // the most recent key event that could have been pasted text.
        let mut last_text_key_at: Option<std::time::Instant> = None;
        // One event peeked at by the paste-burst probe (which must discard a
        // key's Release but never lose a meaningful event it read). Restored
        // at the top of the next loop iteration.
        let mut pending_event: Option<Event> = None;
        if crate::key_trace::enabled() {
            crate::key_trace::note(&format!(
                "--- rpi TUI key trace start pid={} TERM={} raw_mode={} ---",
                std::process::id(),
                std::env::var("TERM").unwrap_or_else(|_| "(unset)".to_string()),
                crossterm::terminal::is_raw_mode_enabled().unwrap_or(false),
            ));
        }
        loop {
            if !*running_key.lock().unwrap() {
                break;
            }
            if !state_for_key.selector_open()
                && !state_for_key.extension_dialog_open()
                && js_for_key.as_ref().map_or(true, |js| !js.custom_active())
            {
                if let Some(request) = js_dialog_for_key.take_pending() {
                    open_js_dialog(&ctx_for_key, js_dialog_for_key.clone(), request);
                } else if let Some(request) = ask_user_for_key.take_pending() {
                    open_ask_user_dialog(&ctx_for_key, ask_user_for_key.clone(), request);
                }
            }
            cancel_js_dialog_ui(&ctx_for_key, &js_dialog_for_key);
            let ev = if let Some(ev) = pending_event.take() {
                ev
            } else {
                // `event::read()` blocks indefinitely. Poll first so shutdown can
                // stop and join this worker even when no further key arrives.
                match crossterm::event::poll(std::time::Duration::from_millis(50)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => {
                        state_for_key.cancel_js_preparation();
                        let _ = tx_for_key.send(TuiMessage::Exit);
                        break;
                    }
                }
                let Ok(ev) = crossterm::event::read() else {
                    state_for_key.cancel_js_preparation();
                    let _ = tx_for_key.send(TuiMessage::Exit);
                    break;
                };
                ev
            };
            // `Event::Resize` is delivered as its own event (not a Key). With
            // `start_readerless` there is no competing terminal-reader thread to
            // handle it, so refresh the cached terminal size here and force a
            // full redraw so the constrained layout re-fits the new dimensions.
            if let Event::Resize(_cols, _rows) = ev {
                tui_for_key.refresh_size();
                if let Some(js) = &js_for_key {
                    if js.custom_active() {
                        let _ = js.send_custom_resize(_cols as usize, _rows as usize);
                        tui_for_key.request_render(false);
                    }
                }
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
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        // Start selection tracking. `selection_end` doubles as the
                        // "a real drag happened" marker: a plain tap never sets
                        // it, so it can never overwrite the clipboard.
                        *state_for_key.selection_start.lock().unwrap() = Some((m.column, m.row));
                        *state_for_key.selection_end.lock().unwrap() = None;
                    }
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        // Motion with the button held = a genuine selection drag.
                        // (Terminals only report this with button-event mouse
                        // tracking, which `ProcessTerminal` enables.)
                        if state_for_key.selection_start.lock().unwrap().is_some() {
                            *state_for_key.selection_end.lock().unwrap() = Some((m.column, m.row));
                        }
                    }
                    MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        // End selection and auto-copy if enabled
                        if let Some(start) = *state_for_key.selection_start.lock().unwrap() {
                            let end = (m.column, m.row);

                            // Only auto-copy when the pointer actually MOVED with
                            // the button held (`Drag` set `selection_end`). A plain
                            // click/tap — even one whose Down/Up cells differ by a
                            // pixel over RDP — must not touch the clipboard: it used
                            // to fire `extract_selected_text`, which copies whatever
                            // rendered TUI line sat under the pointer, silently
                            // clobbering the user's real clipboard so the next paste
                            // returned that stale text.
                            let is_drag = state_for_key.selection_end.lock().unwrap().is_some();
                            *state_for_key.selection_end.lock().unwrap() = Some(end);

                            // Check if auto-copy is enabled
                            let auto_copy = crate::settings::load_settings()
                                .ok()
                                .and_then(|s| s.fullscreen_copy_on_select)
                                .unwrap_or(true);

                            if auto_copy && is_drag {
                                // Try to extract selected text from chat container
                                if let Some(selected_text) =
                                    extract_selected_text(&state_for_key.chat_container, start, end)
                                {
                                    if !selected_text.trim().is_empty() {
                                        let _ = copy_to_clipboard(&selected_text);
                                    }
                                }
                            }
                        }
                        *state_for_key.selection_start.lock().unwrap() = None;
                        *state_for_key.selection_end.lock().unwrap() = None;
                    }
                    _ => {}
                }
                continue;
            }
            let Event::Key(key) = ev else {
                if let Event::Paste(text) = ev {
                    crate::key_trace::note(&format!(
                        "paste event ({} bytes, bracketed paste supported)",
                        text.len()
                    ));
                    let candidate = text.trim().trim_matches(['\"', '\'']);
                    let path = std::path::PathBuf::from(candidate);
                    if !candidate.chars().any(|c| c == '\n' || c == '\r') && path.is_file() {
                        if let Ok(Some(image)) = crate::app::image_content_from_path(&path) {
                            add_image_preview(&state_for_key.chat_container, &image);
                            state_for_key.queue_image(image);
                            add_note_message(
                                &state_for_key.chat_container,
                                "Dropped image attached to the next prompt.",
                            );
                            tui_for_key.request_render(false);
                            continue;
                        }
                    }
                    editor_for_key.insert(&text);
                    refresh_autocomplete(&state_for_key, &editor_for_key);
                    tui_for_key.request_render_reusing_scroll_content();
                }
                continue;
            };
            // Drop releases but preserve Repeat so holding arrows, Backspace,
            // PageUp, etc. behaves naturally. Windows emits Press + Release
            // for a tap; terminals with keyboard enhancement may additionally
            // emit Repeat while a key is held.
            if !should_dispatch_key(key.kind) {
                continue;
            }
            // Diagnostic trace (off unless RPI_DEBUG_KEYS is set): records what
            // actually arrived before any routing decision, so a client that
            // sends LF for Enter is visible as `Char('j') mods=CONTROL`.
            crate::key_trace::key(&key, "");

            // Prompt preparation runs on a blocking worker before the agent
            // lane owns the turn. Cancel it directly: an abort queued only to
            // the lane cannot wake a JS factory or lifecycle hook that never
            // resolves. This check precedes custom/dialog routing because
            // those components may themselves have been opened by the hook.
            let prompt_abort = (key.modifiers == KeyModifiers::CONTROL
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')))
                || (key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc);
            if prompt_abort && state_for_key.cancel_js_preparation() {
                state_for_key.set_status(RunStatus::Aborting);
                if !run_extension_cancel(&state_for_key) && state_for_key.extension_dialog_open() {
                    close_extension_editor(
                        &state_for_key,
                        &ctx_for_key.editor_container,
                        &editor_for_key,
                        &tui_for_key,
                    );
                }
                js_dialog_for_key.cancel_open_requests();
                tui_for_key.set_render_suspended(false);
                tui_for_key.request_render(false);
                continue;
            }

            if let Some(js) = &js_for_key {
                if js.custom_active() {
                    let visible = js.custom_accepts_input();
                    let data = key_event_to_input(key);
                    if !data.is_empty() {
                        // A visible custom owns the whole key stream, so its
                        // acknowledgement is unnecessary and would add a
                        // synchronous round-trip to every keystroke. Hidden
                        // overlays need the consume result to decide whether the
                        // outer editor should see the key.
                        if visible {
                            let _ = js.send_custom_input(&data);
                            continue;
                        }
                        let consumed = js.send_custom_input_with_consumed(&data).unwrap_or(false);
                        // A hidden component only keeps raw listeners alive (for
                        // example ask_user_question's reopen shortcut); an
                        // unconsumed key continues through the outer editor.
                        if consumed {
                            tui_for_key.request_render_reusing_scroll_content();
                            continue;
                        }
                    }
                }
            }

            // Ctrl+C cancels an open selector before it reaches the global
            // abort/exit handler. Route through Esc so selector callbacks run.
            if key.modifiers == KeyModifiers::CONTROL
                && key.code == KeyCode::Char('c')
                && state_for_key.selector_open()
            {
                let selector = state_for_key
                    .active_selector
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("selector_open guaranteed Some")
                    .0;
                selector.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // Extension dialogs own the input slot while awaiting a result.
            // Esc and Ctrl+C both resolve the pending command with cancel;
            // all other keys go to the active native editor/input widget.
            if state_for_key.extension_dialog_open() {
                let cancel = key.code == KeyCode::Esc
                    || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c'));
                if cancel {
                    if !run_extension_cancel(&state_for_key) {
                        close_extension_editor(
                            &state_for_key,
                            &ctx_for_key.editor_container,
                            &editor_for_key,
                            &tui_for_key,
                        );
                    }
                } else if let Some(extension_editor) = state_for_key
                    .active_extension_editor
                    .lock()
                    .unwrap()
                    .clone()
                {
                    extension_editor.handle_key(key);
                } else if let Some(extension_input) =
                    state_for_key.active_extension_input.lock().unwrap().clone()
                {
                    extension_input.handle_key(key);
                }
                tui_for_key.request_render_reusing_scroll_content();
                continue;
            }

            // 0. Ctrl+C: copy the selection when the editor has one (pi
            //    `tui.input.copy`); otherwise abort an active run, or exit
            //    when idle. Open selectors and extension dialogs are handled
            //    above so their cancellation callbacks get first chance.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::CLEAR,
            ) {
                if !state_for_key.selector_open() && editor_for_key.has_selection() {
                    editor_for_key.copy_selection();
                    continue;
                }
                let status = *state_for_key.status.lock().unwrap();
                match status {
                    RunStatus::Working => {
                        state_for_key.set_status(RunStatus::Aborting);
                        // Drop any outstanding ask_user prompt from this turn.
                        ask_user_for_key.cancel_all();
                        // Restore the editor if an extension dialog was occupying it.
                        if !run_extension_cancel(&state_for_key)
                            && state_for_key.extension_dialog_open()
                        {
                            close_extension_editor(
                                &state_for_key,
                                &ctx_for_key.editor_container,
                                &editor_for_key,
                                &tui_for_key,
                            );
                        }
                        let lane = lane_for_key.clone();
                        tokio::spawn(async move {
                            let _ = lane.abort().await;
                        });
                    }
                    // A held Ctrl+C can emit Repeat immediately after Press.
                    // Keep waiting for the in-flight cancellation instead of
                    // treating that repeat as a request to exit the process.
                    RunStatus::Aborting => {
                        // A second, *deliberate* Ctrl+C. The first press only
                        // got as far as `Aborting`, which means the abort has
                        // not landed yet — and the async loop is still inside
                        // `run_prompt_streaming(..).await`, so a queued
                        // `TuiMessage::Exit` would not be read until the run
                        // returns, which is exactly what is not happening.
                        //
                        // Exit here instead: the TUI must never be unquittable
                        // (a provider request or tool that never returns would
                        // otherwise trap the user forever). Auto-repeat is
                        // filtered out, so holding the key through a slow abort
                        // cannot kill the process by accident.
                        if key.kind != KeyEventKind::Repeat {
                            emergency_exit(&tui_for_key);
                        }
                    }
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

            // 2a. Ctrl+D: pi's deleteCharForward inside the editor (mirrors
            //     `tui.editor.deleteCharForward`), and EOF-quit on an empty
            //     editor. With a run active, abort it first (same as Ctrl+C)
            //     so the key is never a no-op while a stuck command runs.
            if keybinding_matches(&keybindings_for_key, &key, rpi_tui::keybindings::keys::EXIT) {
                let status = *state_for_key.status.lock().unwrap();
                match status {
                    RunStatus::Working => {
                        state_for_key.set_status(RunStatus::Aborting);
                        ask_user_for_key.cancel_all();
                        if !run_extension_cancel(&state_for_key)
                            && state_for_key.extension_dialog_open()
                        {
                            close_extension_editor(
                                &state_for_key,
                                &ctx_for_key.editor_container,
                                &editor_for_key,
                                &tui_for_key,
                            );
                        }
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
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::INTERRUPT,
            ) {
                let status = *state_for_key.status.lock().unwrap();
                if status == RunStatus::Working {
                    state_for_key.set_status(RunStatus::Aborting);
                    ask_user_for_key.cancel_all();
                    if !run_extension_cancel(&state_for_key)
                        && state_for_key.extension_dialog_open()
                    {
                        close_extension_editor(
                            &state_for_key,
                            &ctx_for_key.editor_container,
                            &editor_for_key,
                            &tui_for_key,
                        );
                    }
                    let lane = lane_for_key.clone();
                    tokio::spawn(async move {
                        let _ = lane.abort().await;
                    });
                    continue;
                }
                // A running `!command` takes Esc next (native pi's `onEscape`
                // precedence: streaming → bash → bash-mode editor →
                // double-escape). The run slot stays occupied until the capture
                // resolves, so a second Esc is a no-op rather than a
                // double-escape trigger.
                if state_for_key.user_bash_running() {
                    state_for_key.cancel_user_bash();
                    tui_for_key.request_render(false);
                    continue;
                }
                if status == RunStatus::Idle
                    && editor_for_key.get_text().trim().is_empty()
                    && double_escape_action != "none"
                {
                    let now = std::time::Instant::now();
                    if double_escape_trigger(last_escape_time, now) {
                        last_escape_time = None;
                        match double_escape_action.as_str() {
                            "tree" => {
                                let _ = tx_for_key.send(TuiMessage::OpenTree);
                            }
                            "fork" => {
                                let _ = tx_for_key.send(TuiMessage::ForkSession);
                            }
                            _ => {}
                        }
                    } else {
                        last_escape_time = Some(now);
                    }
                }
                continue;
            }

            // 2c. Ctrl+G: edit the current draft in the user's external
            // editor, matching native Pi's VISUAL/EDITOR fallback chain.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::EXTERNAL_EDITOR,
            ) {
                launch_external_editor(editor_for_key.get_text(), tx_for_key.clone());
                continue;
            }

            // 2d. Ctrl+O: toggle all tool output panels between compact and
            // expanded rendering (native Pi's global output toggle).
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::TOOLS_EXPAND,
            ) {
                let expanded = state_for_key.toggle_tool_outputs();
                // Persist per-session so a later `-c`/`-r` restores it.
                let session = ctx_for_key.session.clone();
                tokio::spawn(async move {
                    let _ = session
                        .set_value("display.tool_outputs_expanded", serde_json::json!(expanded))
                        .await;
                });
                tui_for_key.request_render(false);
                continue;
            }

            // 2e. Ctrl+T: toggle visibility of reasoning/thinking blocks.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::THINKING_TOGGLE,
            ) {
                let hide = state_for_key.toggle_thinking();
                // Persist per-session so a later `-c`/`-r` restores it.
                let session = ctx_for_key.session.clone();
                tokio::spawn(async move {
                    let _ = session
                        .set_value("display.hide_thinking", serde_json::json!(hide))
                        .await;
                });
                tui_for_key.request_render(false);
                continue;
            }

            // 2f. Ctrl+M: cycle to the next model in the catalog after the one
            //     currently tracked in `current_model_id`, apply it live via
            //     `lane.set_model` (takes effect on the next user message — the
            //     in-flight run's config is already snapshotted), and update the
            //     footer. `set_model` is async so it runs on a spawned task.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::MODEL_CYCLE_FORWARD,
            ) {
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

            // 2f. Shift+Tab / BackTab: cycle the current model's supported
            // thinking levels, matching native Pi's thinking-level shortcut.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::THINKING_CYCLE,
            ) {
                let lane = lane_for_key.clone();
                let catalog = model_catalog_for_key.clone();
                let state = state_for_key.clone();
                tokio::spawn(async move {
                    let Ok(current_model) = lane.get_model().await else {
                        return;
                    };
                    let levels = catalog
                        .iter()
                        .find(|model| {
                            model.provider == current_model.provider && model.id == current_model.id
                        })
                        .map(|model| model.supported_thinking_levels())
                        .unwrap_or_else(|| vec![rpi_ai::types::ThinkingLevel::Medium]);
                    if levels.is_empty() {
                        return;
                    }
                    let current = lane
                        .get_thinking_level()
                        .await
                        .unwrap_or(rpi_ai::types::ThinkingLevel::Medium);
                    let next = levels
                        .iter()
                        .position(|level| *level == current)
                        .map(|index| levels[(index + 1) % levels.len()])
                        .unwrap_or(levels[0]);
                    if lane.set_thinking_level(next).await.is_ok() {
                        state
                            .footer
                            .set_thinking_level(Some(thinking_level_name(next)));
                        state.tui.as_ref().map(|tui| tui.request_render(false));
                    }
                });
                continue;
            }

            // Ctrl+V (or a configured paste-image key): if the system
            // clipboard holds an image, queue it for the next prompt;
            // otherwise paste the clipboard TEXT into the editor. Previously
            // a text-only clipboard fell through to the editor's Ctrl+V,
            // which yanks from the internal KILL RING — not the system
            // clipboard. On platforms without bracketed-paste `Event::Paste`
            // (Windows console, some remote clients) that meant Ctrl+V pasted
            // stale/other content instead of the user's real clipboard
            // ("不是系统的粘贴的内容").
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::PASTE_IMAGE,
            ) {
                match read_clipboard_image() {
                    Ok(Some(image)) => {
                        add_image_preview(&state_for_key.chat_container, &image);
                        state_for_key.queue_image(image);
                        add_note_message(
                            &state_for_key.chat_container,
                            "Clipboard image attached to the next prompt.",
                        );
                        tui_for_key.request_render(false);
                        continue;
                    }
                    Ok(None) | Err(_) => {}
                }
                // The arboard text fallback is Windows-only. On Unix a real
                // paste arrives as `Event::Paste` (bracketed paste is enabled
                // by `ProcessTerminal::enter_raw_mode`), so reading the
                // SERVER's clipboard here would paste stale TUI content on a
                // remote/mobile client — copy-on-select can have written the
                // welcome "Skills (…)" line into the server clipboard, and
                // Ctrl+V on the client returns that instead of the user's own
                // clipboard ("不是系统的粘贴的内容"). On Windows the console
                // never emits `Event::Paste`, so the arboard read is the only
                // paste source and must stay.
                if cfg!(windows) {
                    if let Some(text) = read_clipboard_text() {
                        if !text.is_empty() {
                            editor_for_key.insert(&text);
                            refresh_autocomplete(&state_for_key, &editor_for_key);
                            tui_for_key.request_render_reusing_scroll_content();
                            continue;
                        }
                    }
                }
            }

            // 3. Ctrl+Shift+F: open transcript search
            if key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && key.code == KeyCode::Char('F')
            {
                state_for_key.search.activate();
                state_for_key.search_bar.set_visible(true);
                state_for_key.search_bar.set_query("");
                state_for_key.search_bar.set_match_info(0, 0);
                tui_for_key.request_render(false);
                continue;
            }

            // Handle search mode input
            if state_for_key.search.is_active() {
                let search_bar = state_for_key.search_bar.clone();
                match key.code {
                    KeyCode::Esc => {
                        state_for_key.search.deactivate();
                        search_bar.set_visible(false);
                        tui_for_key.request_render(false);
                        continue;
                    }
                    KeyCode::Enter => {
                        if key.modifiers.contains(KeyModifiers::SHIFT) {
                            state_for_key.search.previous_match();
                        } else {
                            state_for_key.search.next_match();
                        }
                        // Update search bar with current match info
                        let match_index = state_for_key.search.get_match_index();
                        let match_count = state_for_key.search.get_match_count();
                        search_bar.set_match_info(match_index, match_count);
                        tui_for_key.request_render(false);
                        continue;
                    }
                    KeyCode::Backspace => {
                        state_for_key.search.backspace();
                        // Re-search with updated query
                        let lines = collect_transcript_lines(&state_for_key.chat_container);
                        state_for_key.search.find_matches(&lines);
                        // Update search bar
                        let query = state_for_key.search.get_query();
                        let match_count = state_for_key.search.get_match_count();
                        search_bar.set_query(&query);
                        search_bar.set_match_info(0, match_count);
                        tui_for_key.request_render(false);
                        continue;
                    }
                    KeyCode::Char(c) => {
                        state_for_key.search.append_char(c);
                        // Re-search with updated query
                        let lines = collect_transcript_lines(&state_for_key.chat_container);
                        state_for_key.search.find_matches(&lines);
                        // Update search bar
                        let query = state_for_key.search.get_query();
                        let match_count = state_for_key.search.get_match_count();
                        search_bar.set_query(&query);
                        search_bar.set_match_info(0, match_count);
                        tui_for_key.request_render(false);
                        continue;
                    }
                    _ => continue,
                }
            }

            // 3. Ctrl+L: open the model selector. Routed through the `/model`
            //    command so the hotkey and the slash command share one path
            //    (TS binds Ctrl+L to model-select).
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::MODEL_SELECT,
            ) {
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

            // `app.message.dequeue` (Alt+Q on Windows, Alt+Up elsewhere):
            // restore every queued steering/follow-up message into the editor
            // so it can be edited before resending. Mirrors native pi's
            // `handleDequeue`; the pending-messages display shows the hint.
            if keybinding_matches(
                &keybindings_for_key,
                &key,
                rpi_tui::keybindings::keys::DEQUEUE,
            ) {
                // Handled here, on the key worker — NOT via the mailbox. The
                // async main loop parks inside `run_prompt_streaming(..).await`
                // for the whole run, and a queue only exists *while* a run is
                // in flight, so a `TuiMessage::Dequeue` sat unread until the
                // run finished (after the lane had already consumed the queue).
                // Alt+Q therefore looked like a no-op while it was most needed.
                // Spawn the drain so it lands while the user is still looking at
                // the queue, exactly like the Alt+Enter `follow_up` path above.
                tokio::spawn(restore_queued_messages_to_editor(
                    lane_for_key.clone(),
                    editor_for_key.clone(),
                    state_for_key.clone(),
                    tui_for_key.clone(),
                ));
                continue;
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
                        // Direct send: the harness emits no user `message_start`
                        // for it, so render the bubble here (see the submit
                        // handler in `interactive_tui`).
                        add_user_message(&state_for_key.chat_container, &prompt);
                        push_history(&state_for_key, &prompt);
                        let _ = tx_for_key.send(TuiMessage::UserInput(prompt));
                    }
                } else {
                    // Follow-ups are echoed by the loop's
                    // `AgentEvent::MessageStart` once they are consumed; while
                    // they wait, the pending-messages display shows them.
                    let message = AgentMessage::User(UserMessage::new(prompt, 0));
                    let lane = lane_for_key.clone();
                    let chat = state_for_key.chat_container.clone();
                    let tui = tui_for_key.clone();
                    let state = state_for_key.clone();
                    tokio::spawn(async move {
                        if let Err(error) = lane.follow_up(message).await {
                            add_error_message(&chat, &format!("Could not queue message: {error}"));
                            tui.request_render(false);
                            return;
                        }
                        refresh_pending_messages(&state, &lane).await;
                        tui.request_render(false);
                    });
                }
                tui_for_key.request_render(false);
                continue;
            }

            // ---- Paste-burst coalescing (Windows only) ----------------------
            // `crossterm` 0.27 implements bracketed paste ONLY on Unix: its
            // Windows console event source (`ReadConsoleInputW`) never emits
            // `Event::Paste`, so `?2004h` buys nothing there and a pasted block
            // arrives as ordinary key events with a bare Enter per line. The
            // editor treats a bare Enter as "submit", so one paste became N
            // messages. See [`enter_is_paste_burst`] for the discriminator.
            //
            // On Unix the timing heuristic is both unnecessary AND harmful: a
            // remote/mobile client (SSH, soft keyboard) coalesces typed text +
            // Enter into one network burst, so the Enter lands within
            // [`PASTE_BURST_GAP`] of the last character and the heuristic
            // misclassifies it as pasted content — "回车变成了换行". Real pastes
            // on Unix arrive as `Event::Paste` (bracketed paste is enabled by
            // `ProcessTerminal::enter_raw_mode`), so gating to Windows restores
            // remote Enter without losing the Windows paste fix.
            if cfg!(windows)
                && key.modifiers.is_empty()
                && matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
                )
            {
                // Probe for a *real* queued key. Windows queues each key's
                // Release event directly behind its Press, and a bare
                // non-blocking poll cannot tell the two apart — treating that
                // Release as "more input" made every Enter on a remote/RDP
                // console look like a paste burst ("回车变成了换行"). See
                // [`probe_paste_input`].
                let (more_queued, stashed) = probe_paste_input(pending_event.take(), || {
                    if crossterm::event::poll(PASTE_PROBE).unwrap_or(false) {
                        crossterm::event::read().ok()
                    } else {
                        None
                    }
                });
                pending_event = stashed;
                if enter_is_paste_burst(last_text_key_at, std::time::Instant::now(), more_queued) {
                    crate::key_trace::note(&format!(
                        "enter-decision -> newline (paste burst) gap_ms={} more_queued={more_queued}",
                        gap_ms(last_text_key_at),
                    ));
                    // Pasted newline: insert it and keep the remaining queued
                    // events flowing through this same path.
                    editor_for_key.insert("\n");
                    last_text_key_at = Some(std::time::Instant::now());
                    refresh_autocomplete(&state_for_key, &editor_for_key);
                    tui_for_key.request_render_reusing_scroll_content();
                    continue;
                }
                crate::key_trace::note(&format!(
                    "enter-decision -> submit gap_ms={} more_queued={more_queued}",
                    gap_ms(last_text_key_at),
                ));
                // A real submit ends the burst so a follow-up Enter is not
                // mistaken for paste continuation.
                last_text_key_at = None;
            } else if matches!(
                key.code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Tab
            ) && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                // Unmodified text keys extend the burst. Ctrl/Alt chords
                // (Ctrl+C, Alt+Enter, …) are commands, not paste content.
                last_text_key_at = Some(std::time::Instant::now());
            } else {
                // Arrows, Esc, Ctrl/Alt chords, PageUp/Down, … — not paste
                // content; break the burst.
                last_text_key_at = None;
            }

            // 6. Otherwise forward to the editor + refresh autocomplete. The
            //    editor maps unmodified Enter and the encodings a remote client
            //    may send for it (`Char('\n')`/`Char('\r')`, and the raw LF
            //    crossterm reports as `Ctrl+J`) to submit, so a remote Enter
            //    sends instead of inserting a newline.
            editor_for_key.handle_key(key);
            refresh_autocomplete(&state_for_key, &editor_for_key);
            tui_for_key.request_render_reusing_scroll_content();
        }
    });

    // ---- Session lifecycle (P1): SessionStart ----
    // The TUI + key worker are up and the session is ready to accept input.
    // Notify subscribed extensions now so they can initialize session-scoped
    // state. A veto here is **advisory** (the session is already up) — warn and
    // continue. No-op when no extension subscribes.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::SessionStart,
    )
    .await
    {
        tracing::warn!("[rpi] extension vetoed SessionStart (advisory): {reason}");
    }

    // ---- Initial prompts (run before reading from the channel) ----
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }
    // ---- Continue a run that recovery repaired, the way native pi does ----
    // This happens before any user input, and only when nothing was passed on the
    // command line: an explicit `-p` means the user has already said what they
    // want next. Driven here rather than inside the harness so the run's output
    // goes through the same streaming/render path as any other run, and so
    // startup is never blocked by a run the user cannot see.
    if prompts.is_empty() && lane.has_pending_resume().await {
        run_prompt_streaming(
            &lane,
            "",
            true,
            &tui,
            &state,
            drain_handle.is_some(),
            reload_context.js_extension_session.as_ref(),
            &js_dialog_bridge,
            args,
            Vec::new(),
        )
        .await;
    }

    let mut images = initial_images;
    for prompt in prompts {
        if !*running.lock().unwrap() {
            break;
        }
        // The harness emits no user `message_start` for a directly-sent prompt
        // (it drives the loop with an empty prompts vec), so the initial
        // `-p` / `--prompt` bubbles are rendered here.
        add_user_message(&chat_container, &prompt);
        tui.request_render(false);
        run_prompt_streaming(
            &lane,
            &prompt,
            false,
            &tui,
            &state,
            drain_handle.is_some(),
            reload_context.js_extension_session.as_ref(),
            &js_dialog_bridge,
            args,
            std::mem::take(&mut images),
        )
        .await;
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
                let prompt_images = state.take_pending_images();
                if !prompt_images.is_empty() {
                    add_note_message(
                        &chat_container,
                        &format!("Attached {} image(s) to this prompt.", prompt_images.len()),
                    );
                }
                run_prompt_streaming(
                    &lane,
                    &prompt,
                    false,
                    &tui,
                    &state,
                    drain_handle.is_some(),
                    reload_context.js_extension_session.as_ref(),
                    &js_dialog_bridge,
                    args,
                    prompt_images,
                )
                .await;
            }
            Some(TuiMessage::ExternalEditorResult(result)) => {
                match result {
                    Ok(text) => {
                        let cursor = text.chars().count();
                        editor.set_text(&text);
                        editor.set_cursor(0, cursor);
                        add_note_message(&chat_container, "Draft updated from external editor.");
                    }
                    Err(error) => add_error_message(&chat_container, &error),
                }
                tui.request_render(false);
            }
            Some(TuiMessage::UserBashFinished(report)) => {
                // Free the run slot first: the guard and the Esc router both
                // read it, so a finished command must never block the next one.
                state.finish_user_bash();
                let message = rpi_harness::messages::bash_execution_message(
                    report.command,
                    report.output,
                    report.exit_code,
                    report.cancelled,
                    report.truncated,
                    report.full_output_path,
                    Some(report.exclude_from_context),
                    now_ms(),
                );
                // While a run is in flight the message is deferred so it is not
                // spliced into the middle of a turn's tool sequence; it is
                // flushed at `AgentEnd` (native pi `_pendingBashMessages`).
                if *state.status.lock().unwrap() == RunStatus::Idle {
                    if let Err(error) = lane.append_message(message).await {
                        add_error_message(
                            &chat_container,
                            &format!("Could not record bash result: {error}"),
                        );
                        tui.request_render(false);
                    }
                } else {
                    state.pending_bash_messages.lock().unwrap().push(message);
                }
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
                let file_name = agent_session
                    .default_export_file_name(crate::export::ExportFormat::Markdown)
                    .await;
                let path = cwd.join(&file_name);
                match agent_session.export_to_markdown(&path).await {
                    Ok(_) => add_note_message(
                        &chat_container,
                        &format!("Exported session to {}", path.display()),
                    ),
                    Err(e) => {
                        add_error_message(&chat_container, &format!("Could not write export: {e}"))
                    }
                }
                tui.request_render(false);
            }
            Some(TuiMessage::ExportSessionWithFormat(format)) => {
                let file_name = agent_session.default_export_file_name(format.clone()).await;
                let path = cwd.join(&file_name);
                match agent_session.export(format, &path).await {
                    Ok(_) => add_note_message(
                        &chat_container,
                        &format!("Exported session to {}", path.display()),
                    ),
                    Err(e) => {
                        add_error_message(&chat_container, &format!("Could not write export: {e}"))
                    }
                }
                tui.request_render(false);
            }
            Some(TuiMessage::ShowUsage) => {
                let stats = agent_session.usage_stats().await.ok();
                let total = agent_session.total_usage().await.ok();
                let snapshot = agent_session.snapshot().await.ok();
                match stats {
                    Some(stats) => {
                        let mut lines = vec![
                            "## Usage".to_string(),
                            String::new(),
                            format!("- messages: {}", stats.message_count),
                            format!("- total tokens: {}", stats.total_tokens),
                            format!("- cached tokens: {}", stats.cached_tokens),
                            format!("- uncached tokens: {}", stats.uncached_tokens),
                            format!("- cost: ${:.4}", stats.cost_total),
                        ];
                        match stats.cache_hit_ratio() {
                            Some(ratio) => {
                                lines.push(format!("- cache hit ratio: {:.1}%", ratio * 100.0))
                            }
                            None => lines.push("- cache hit ratio: n/a (no input yet)".to_string()),
                        }
                        if let Some(total) = total {
                            lines.push(String::new());
                            lines.push(format!(
                                "- last cumulative input/output: {} / {}",
                                total.input, total.output
                            ));
                        }
                        if let Some(snapshot) = snapshot {
                            lines.push(String::new());
                            lines.push(format!("- lane: `{}`", snapshot.lane));
                            lines.push(format!(
                                "- leaf: `{}`",
                                snapshot.leaf_id.clone().unwrap_or_else(|| "(empty)".into())
                            ));
                            if let Some(active) = &snapshot.active {
                                lines.push(format!(
                                    "- active operation: {} (`{}`)",
                                    active.kind.as_str(),
                                    active.run_id
                                ));
                            }
                        }
                        add_note_message(&chat_container, &lines.join("\n"));
                    }
                    None => {
                        add_error_message(&chat_container, "Could not read session usage stats.")
                    }
                }
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
                if outcome.had_errors {
                    // Hard failure: the summary itself carries the error detail
                    // (e.g. "Settings reload failed: …"), so render it as an
                    // error without an extra stderr pointer.
                    add_error_message(&chat_container, &outcome.summary);
                } else if outcome.had_warnings {
                    // Reload succeeded but produced soft diagnostics (skill
                    // shadowing, name validation, …). The details are printed
                    // to stderr; keep this as an informational note so a
                    // successful reload isn't shown as a red ✗ error.
                    add_note_message(
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
    // Wake any Node `ctx.ui.*` request that is still waiting on the dialog
    // bridge before joining the key worker and restoring the terminal.
    js_dialog_bridge.cancel_all();
    ask_user_bridge.shutdown();
    *running.lock().unwrap() = false;
    for handle in update_check_handles {
        handle.abort();
        let _ = handle.await;
    }
    // A hidden custom input listener can leave the key worker blocked on a
    // synchronous Node response. Stop the host first so transport shutdown
    // wakes that request before we join the worker.
    if let Some(js) = &reload_context.js_extension_session {
        js.shutdown();
    }
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

    // Native pi prints how to resume the session after an interactive exit so
    // a terminal that scrolled away can be recovered. Mirrors
    // `formatResumeCommand` (id + `--session-dir` only when non-default).
    let session_id = harness.session().storage().metadata().id;
    if let Some(command) = format_resume_command(&session_id, &cwd, args.session_dir.as_deref()) {
        println!("\x1b[2mTo resume this session:\x1b[0m {command}");
    }
    println!("\nGoodbye!");
    let _ = args;

    0
}

// ===========================================================================
// Run a single prompt (streaming or blocking)
// ===========================================================================

/// Prepare the session's persistent Node host immediately before a real prompt
/// enters the agent loop. The first call starts the lazy host; later calls run
/// `before_agent_start` again on that host so each prompt sees current state.
/// Keeping startup here leaves an idle TUI free of a Node child while still
/// giving the lifecycle hook the fully installed UI bridge.
async fn ensure_js_runtime_before_prompt(
    js: Option<&crate::js_extensions::JsExtensionSession>,
    lane: &Arc<dyn AgentLane>,
    state: &Arc<TuiState>,
    dialog_bridge: &JsDialogBridge,
    args: &Args,
) -> bool {
    let Some(js) = js else {
        return true;
    };
    let cancellation = state.begin_js_preparation();
    let worker_cancellation = cancellation.clone();
    let js_for_start = js.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        js_for_start.prepare_for_prompt_with_cancellation(&worker_cancellation)
    });
    let result = tokio::select! {
        result = &mut worker => result,
        _ = cancellation.cancelled() => {
            dialog_bridge.cancel_open_requests();
            let js_for_cancel = js.clone();
            let _ = tokio::task::spawn_blocking(move || {
                js_for_cancel.cancel_prompt_preparation();
            }).await;
            worker.await
        }
    };
    let was_cancelled = cancellation.is_cancelled();
    if was_cancelled {
        dialog_bridge.cancel_open_requests();
        let js_for_cancel = js.clone();
        let _ = tokio::task::spawn_blocking(move || {
            js_for_cancel.cancel_prompt_preparation();
        })
        .await;
        state.finish_js_preparation();
        dialog_bridge.reopen();
        return false;
    }
    state.finish_js_preparation();
    match result {
        Ok(Ok(())) => {
            // The lifecycle hook can change the JS-only active tool set once
            // it sees the real TUI context. Merge that subset with the Rust
            // built-ins while applying the command-line tool policy.
            if let Some(js_active) = js.active_tools() {
                let js_names = js.tool_names();
                let mut active = lane.get_active_tools().await.unwrap_or_default();
                active.retain(|name| {
                    crate::session::tool_name_allowed(name, args)
                        && !js_names.iter().any(|js_name| js_name == name)
                });
                active.extend(js_active.into_iter().filter(|name| {
                    js_names.iter().any(|js_name| js_name == name)
                        && crate::session::tool_name_allowed(name, args)
                }));
                active = crate::session::filter_active_tool_names(active, args);
                let _ = lane.set_active_tools(active).await;
            }
        }
        Ok(Err(error)) => {
            if args.verbose {
                eprintln!("warning: could not start JS extension runtime: {error}");
            }
        }
        Err(error) => {
            if args.verbose {
                eprintln!("warning: JS extension runtime worker failed: {error}");
            }
        }
    }
    true
}

fn launch_external_editor(draft: String, tx: mpsc::UnboundedSender<TuiMessage>) {
    std::thread::spawn(move || {
        let file = match tempfile::Builder::new()
            .prefix("rpi-draft-")
            .suffix(".md")
            .tempfile()
        {
            Ok(file) => file,
            Err(error) => {
                let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                    "Could not create editor file: {error}"
                ))));
                return;
            }
        };
        if let Err(error) = std::fs::write(file.path(), draft.as_bytes()) {
            let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                "Could not write editor file: {error}"
            ))));
            return;
        }
        let editor = std::env::var("RPI_EXTERNAL_EDITOR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });
        let status = std::process::Command::new(&editor)
            .arg(file.path())
            .status();
        let result = match status {
            Ok(status) if status.success() => std::fs::read_to_string(file.path())
                .map_err(|error| format!("Could not read editor file: {error}")),
            Ok(status) => Err(format!("External editor exited with {status}")),
            Err(error) => Err(format!(
                "Could not launch external editor `{editor}`: {error}"
            )),
        };
        let _ = tx.send(TuiMessage::ExternalEditorResult(result));
    });
}

/// Apply the authoritative run result when it wins the race with the async
/// event drain, then detach the live component from further partial updates.
fn reconcile_streamed_assistant_completion(
    current_assistant: &Mutex<Option<Arc<AssistantMessageComponent>>>,
    last_assistant_text: &Mutex<String>,
    final_message: Option<&AssistantMessage>,
) {
    let final_blocks = final_message.map(assistant_blocks);
    let final_text = final_message.map(assistant_text);
    let component = current_assistant.lock().unwrap().take();

    if let Some(component) = component {
        if let Some(blocks) = final_blocks.as_deref() {
            component.update_blocks(blocks);
        }
        component.set_streaming(false);
    }

    if let Some(text) = final_text.filter(|text| !text.is_empty()) {
        *last_assistant_text.lock().unwrap() = text;
    }
}

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and the completed outcome
/// reconciles its final snapshot. When false (no `event_rx`), this falls back
/// to the blocking await-final-text path.
async fn run_prompt_streaming(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    resume: bool,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    streaming: bool,
    js: Option<&crate::js_extensions::JsExtensionSession>,
    dialog_bridge: &JsDialogBridge,
    args: &Args,
    images: Vec<rpi_ai::types::ImageContent>,
) {
    // The persistent Node host is intentionally started at the first real
    // prompt. By this point the TUI key worker and all UI/runtime handlers are
    // live, so a `before_agent_start` hook may safely open a native dialog. A
    // session with no prompt never starts Node merely to render its welcome
    // screen; JS commands/tools still trigger the same lazy ensure path.
    // Preparation is part of the active turn. Mark it working before Node can
    // block so Ctrl+C, Ctrl+D, and Esc all retain their documented abort
    // semantics for initial argv prompts as well as editor submissions.
    state.set_status(RunStatus::Working);
    tui.request_render(false);
    if !ensure_js_runtime_before_prompt(js, lane, state, dialog_bridge, args).await {
        state.set_status(RunStatus::Idle);
        tui.request_render(false);
        return;
    }

    let outcome = if resume {
        // Continuing an interrupted run: no new user turn, just the provider call
        // the interrupted run never reached. Native pi behaves the same way.
        match lane.resume_pending().await {
            Ok(Some(result)) => Ok(result),
            // Nothing to resume after all: leave the status alone and let the
            // normal prompt path take over.
            Ok(None) => {
                state.set_status(RunStatus::Idle);
                tui.request_render(false);
                return;
            }
            Err(error) => Err(error),
        }
    } else {
        lane.prompt_text(prompt, images).await
    };

    if streaming {
        // Broadcast delivery is asynchronous: the harness result can resolve
        // before the drain task processes MessageEnd. Reconcile from the
        // authoritative outcome before detaching the component so the final
        // streamed tail cannot be left at an earlier partial snapshot.
        let final_message = match &outcome {
            Ok(result) => match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
                HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
                HarnessRunOutcome::Suspended { .. } => None,
            },
            Err(_) => None,
        };
        reconcile_streamed_assistant_completion(
            &state.current_assistant,
            &state.last_assistant_text,
            final_message,
        );
    }

    state.set_status(RunStatus::Idle);
    // Guarantee the pending-messages display reconciles at the end of every
    // run/prompt — covers aborts and the non-streaming path, where no
    // `AgentEnd` event necessarily reaches the drain task.
    refresh_pending_messages(state, lane).await;

    // Authoritative context-badge fallback. The streaming drain normally
    // updates it from `MessageEnd`, but a `Lagged` broadcast or the blocking
    // path can skip that event; reconcile from the run outcome so the badge
    // always reflects the newest real response.
    if let Ok(result) = &outcome {
        let final_message: Option<&AssistantMessage> = match &result.outcome {
            HarnessRunOutcome::Completed { final_message, .. }
            | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
            HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
            HarnessRunOutcome::Suspended { .. } => None,
        };
        if let Some(a) = final_message {
            record_context_usage(&state.footer, a);
        }
    }

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
                // A final assistant error is emitted by the event drain only
                // in streaming mode. In regular mode there is no drain task,
                // so suppressing this branch merely hides 405/auth/network
                // diagnostics from the user.
                let already_rendered = streaming && final_message.is_some();
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
            // The last assistant usage describes the pre-compaction context;
            // reset the badge to `?` until the next response reports the new
            // (much smaller) prompt size — mirrors pi's `percent: null`.
            state.footer.set_context_tokens(None);
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

/// Extract text from the chat container within the given screen coordinates.
/// Returns None if the selection is invalid or empty.
fn extract_selected_text(
    chat_container: &Arc<Container>,
    start: (u16, u16),
    end: (u16, u16),
) -> Option<String> {
    use rpi_tui::Component;

    // Get the rendered lines from the chat container
    let lines = chat_container.render(80); // Use a reasonable width
    if lines.is_empty() {
        return None;
    }

    // Normalize coordinates (ensure start is before end)
    let (start_row, end_row) = if start.1 <= end.1 {
        (start.1 as usize, end.1 as usize)
    } else {
        (end.1 as usize, start.1 as usize)
    };

    // Clamp to valid range
    let start_row = start_row.min(lines.len().saturating_sub(1));
    let end_row = end_row.min(lines.len().saturating_sub(1));

    if start_row > end_row {
        return None;
    }

    // Extract the selected lines
    let selected_lines: Vec<String> = lines[start_row..=end_row].to_vec();

    if selected_lines.is_empty() {
        return None;
    }

    Some(selected_lines.join("\n"))
}

/// Best-effort clipboard write. Enabled only with the `clipboard` feature
/// (`arboard`) on non-Android platforms; otherwise returns `false` so the
/// caller degrades to a hint.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// Best-effort clipboard text read for Ctrl+V paste. The optional `clipboard`
/// feature keeps headless builds free of platform clipboard dependencies;
/// on Android the OS owns clipboard access so this degrades to `None`.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
fn read_clipboard_text() -> Option<String> {
    None
}

/// Read a clipboard bitmap and normalize it to PNG for the provider-neutral
/// `ImageContent` contract. The optional clipboard feature keeps headless
/// builds free of platform clipboard dependencies.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let width =
        u32::try_from(image.width).map_err(|_| "clipboard image is too wide".to_string())?;
    let height =
        u32::try_from(image.height).map_err(|_| "clipboard image is too tall".to_string())?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err("clipboard image dimensions are outside the supported range".into());
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| e.to_string())?;
    }
    Ok(Some(rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: "image/png".into(),
    }))
}

fn add_image_preview(chat: &Arc<Container>, image: &rpi_ai::types::ImageContent) {
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data) {
        let mut options = ImageOptions::default();
        options.width = Some(48);
        options.alt_text = Some("Attached image".into());
        chat.add_child(Arc::new(Image::from_data(bytes, options)));
        chat.add_child(Arc::new(Spacer::new(1)));
    }
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    Ok(None)
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
/// Re-read the lane's queue and update the pending-messages dock slot.
/// Cheap (two short mutex locks) and a no-op when the snapshot is unchanged,
/// so it is safe to call on every relevant event.
async fn refresh_pending_messages(state: &Arc<TuiState>, lane: &Arc<dyn AgentLane>) {
    if let Ok(queued) = lane.queued_messages().await {
        state.set_pending_queue(queued);
    }
}

/// Whether the transcript holds a panel that repaints its elapsed readout from
/// the clock on every frame: a running bash command, or a tool call still in
/// [`ToolStatus::Running`]. Such a panel needs a real transcript rebuild each
/// render tick — a frame that reuses the cached scroll content would leave its
/// timer frozen at the second of the last full rebuild.
fn transcript_has_live_panel(
    bash_components_present: bool,
    tool_components: &HashMap<String, Arc<ToolExecutionComponent>>,
) -> bool {
    bash_components_present
        || tool_components
            .values()
            .any(|component| component.status() == ToolStatus::Running)
}

/// Prepend restored queued messages to the draft the user may already have
/// typed (native pi's `restoreQueuedMessagesToEditor` places them first).
fn merge_queued_into_draft(queued_text: &str, current: &str) -> String {
    if current.trim().is_empty() {
        queued_text.to_string()
    } else {
        format!("{queued_text}\n\n{current}")
    }
}

/// `app.message.dequeue`: drain every queued steering/follow-up message out of
/// the lane and restore it to the editor so the user can edit before resending
/// (native pi's `restoreQueuedMessagesToEditor`).
///
/// Runs on a spawned task rather than through the [`TuiMessage`] mailbox: the
/// async main loop owns `run_prompt_streaming(..).await` for the whole run, and
/// a queue only exists *while* a run is in flight — so a `TuiMessage::Dequeue`
/// was never serviced until the run ended, after the lane had already consumed
/// the queue. That made Alt+Q a no-op while it was most needed.
async fn restore_queued_messages_to_editor(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) {
    match lane.clear_queue().await {
        Ok(queued) => {
            let all: Vec<String> = queued
                .steering
                .iter()
                .chain(queued.follow_up.iter())
                .cloned()
                .collect();
            if !all.is_empty() {
                let queued_text = all.join("\n\n");
                let current = editor.get_text();
                let combined = merge_queued_into_draft(&queued_text, &current);
                let cursor = combined.chars().count();
                editor.set_text(&combined);
                editor.set_cursor(0, cursor);
            }
            // The lane drained the queue; refresh so the dock drops the rows.
            refresh_pending_messages(&state, &lane).await;
        }
        Err(error) => {
            add_error_message(
                &state.chat_container,
                &format!("Could not restore queued messages: {error}"),
            );
        }
    }
    tui.request_render(false);
}

async fn drain_agent_events(
    mut rx: broadcast::Receiver<AgentEvent>,
    tui: Arc<TuiAltScreen>,
    state: Arc<TuiState>,
    chat: Arc<Container>,
    lane: Arc<dyn AgentLane>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => handle_agent_event(event, &tui, &state, &chat, &lane).await,
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
    lane: &Arc<dyn AgentLane>,
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
            // Flush any `!command` results that completed mid-run: the run's
            // tool sequence is closed, so appending now keeps transcript order
            // intact (native pi's `flushPendingBashMessages`).
            let pending: Vec<AgentMessage> =
                std::mem::take(&mut *state.pending_bash_messages.lock().unwrap());
            for message in pending {
                if let Err(error) = lane.append_message(message).await {
                    add_error_message(chat, &format!("Could not record bash result: {error}"));
                }
            }
            // The run drained the queue; refresh so consumed entries drop out
            // of the pending-messages display.
            refresh_pending_messages(state, lane).await;
            tui.request_render(false);
        }

        AgentEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            ..
        } => {
            state.show_retry(attempt, max_retries, delay_ms);
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
                comp.set_hide_thinking(state.hide_thinking());
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
            // Queued (steering / follow-up) user messages are rendered here,
            // not at queue time, so a message that is still waiting never looks
            // delivered. The loop drains those itself and emits
            // `message_start` for them (`agent_loop.rs` pending-message drain).
            //
            // A DIRECTLY-sent prompt does NOT reach this arm: the harness
            // persists it up front and runs the loop with an empty prompts vec,
            // so no `message_start` is emitted for it — the submit handler
            // renders that bubble instead.
            AgentMessage::User(user) => {
                add_user_message(chat, &user_message_text(&user));
                // A consumed entry must drop out of the pending display.
                refresh_pending_messages(state, lane).await;
                tui.request_render(false);
            }
            // ToolResult / other starts are echoed via the tool-execution
            // components; ignore the dupes.
            _ => {}
        },

        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            {
                // `message` is the shared partial snapshot
                // (`Arc<AssistantMessage>`), so read through it instead of
                // cloning the whole growing message once per delta. The bare
                // block preserves the original nesting.
                let a: &rpi_ai::types::AssistantMessage = &message;
                let text = assistant_text(a);
                let mut saw_bash_tool_call = false;
                // Scan content for finalized tool calls → proactively create
                // tool components (TS shows the tool as soon as the assistant
                // emits the ToolCall; ToolExecutionStart coalesces if it
                // already exists).
                for c in &a.content {
                    if let Content::ToolCall(tc) = c {
                        // Streaming providers may expose a placeholder tool
                        // call before its name has arrived. It is not a real
                        // tool panel and must not leave an empty first row.
                        if tc.name.trim().is_empty() {
                            continue;
                        }
                        if tc.name == "bash" {
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
                            // Streaming tool-call arguments may still be `{}`
                            // here. Do not create a running bash panel until
                            // the lifecycle start event provides the command;
                            // otherwise the spinner renders first and the
                            // actual `$ command` header appears one frame
                            // later.
                            if command.trim().is_empty() {
                                continue;
                            }
                            saw_bash_tool_call = true;
                            let mut bash = state.bash_components.lock().unwrap();
                            if let Some(existing) = bash.get(&tc.id) {
                                // Tool-call arguments arrive incrementally.
                                // Refresh the running panel from every full
                                // assistant snapshot instead of leaving its
                                // header on the first partial command.
                                existing.set_command(command);
                            } else {
                                let comp = Arc::new(BashExecutionComponent::new(command));
                                comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                                chat.add_child(comp.clone());
                                bash.insert(tc.id.clone(), comp);
                            }
                        } else {
                            let mut tools = state.tool_components.lock().unwrap();
                            let display_args = if is_ask_user_tool(&tc.name) {
                                ask_user_args_display(&tc.arguments)
                            } else {
                                tc.arguments.to_string()
                            };
                            if let Some(existing) = tools.get(&tc.id) {
                                // Regular tool arguments stream incrementally
                                // too, so refresh their live header as soon as
                                // a more complete snapshot arrives.
                                if !display_args.trim().is_empty() && display_args.trim() != "{}" {
                                    existing.set_args(&display_args);
                                }
                            } else {
                                let comp =
                                    Arc::new(ToolExecutionComponent::new(&tc.name, &display_args));
                                if is_ask_user_tool(&tc.name) {
                                    comp.set_display_title("ASK USER");
                                }
                                comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
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
                // Cache-miss notice (`maybeShowCacheMissNotice`): the previous
                // turn's input established a cacheable prefix; a prompt that
                // re-reads less than it should means the prefix was re-billed.
                // Uses the shared cache-stats logic (noise floor, idle gap,
                // model change) and prices the miss when known.
                let usage = &a.usage;
                // Footer usage totals + cache hit rate (pi footer.ts).
                state.footer.add_usage(
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.cost.total,
                );
                let denom = usage.input + usage.cache_read + usage.cache_write;
                if denom > 0 && (usage.cache_read + usage.cache_write) > 0 {
                    state
                        .footer
                        .set_cache_hit_rate(Some(usage.cache_read as f64 / denom as f64 * 100.0));
                }
                // Context-window badge (`?/512k` → `63.2%/512k`): the provider
                // reports the prompt token count for this request, which is the
                // context size the footer displays.
                record_context_usage(&state.footer, a);
                let miss = state
                    .cache_tracker
                    .lock()
                    .unwrap()
                    .observe(a, &rpi_harness::cache_stats::NoPrices);
                // Native pi gates these behind `showCacheMissNotices`
                // (default `false`); the tracker still runs so the footer's
                // cache-hit rate stays accurate either way.
                let cache_miss_notices = *state.cache_miss_notices.lock().unwrap();
                if let Some(miss) = miss.filter(|_| cache_miss_notices) {
                    let cost = if miss.missed_cost > 0.0 {
                        format!(" (~${:.4})", miss.missed_cost)
                    } else {
                        String::new()
                    };
                    add_note_message(
                        &state.chat_container,
                        &format!(
                            "Cache miss: {} tokens re-billed{}",
                            format_tokens(miss.missed_tokens),
                            cost
                        ),
                    );
                }
                if let Some(text) = extension_usage_text(Some(&state.extension_session), usage) {
                    add_note_message(chat, &text);
                }
                // Error assistant messages carry the provider diagnostic in
                // `error_message`, not in text content. The assistant
                // component is empty for these messages, so surface the
                // diagnostic as a visible error row in the transcript.
                if let Some(error) = assistant_error_text(a) {
                    add_error_message(chat, &error);
                }
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            // Ignore placeholder lifecycle events emitted before the
            // provider has supplied a tool name.
            if tool_name.trim().is_empty() {
                return;
            }
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
                // Bash arguments can still be `{}` when the lifecycle event
                // races the streamed tool-call argument finalization. Defer
                // the panel until a later start event carries the command.
                if command.trim().is_empty() {
                    return;
                }
                let mut bash_map = state.bash_components.lock().unwrap();
                if let Some(existing) = bash_map.get(&tool_call_id) {
                    // A ToolExecutionUpdate already created the panel (fast
                    // command — Update can arrive before Start); backfill the
                    // command header instead of adding a SECOND panel, which
                    // used to stack an empty "$ " box above the real one.
                    existing.set_command(&command);
                } else {
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                    chat.add_child(comp.clone());
                    bash_map.insert(tool_call_id.clone(), comp);
                }
            } else {
                let _comp = {
                    let mut tools = state.tool_components.lock().unwrap();
                    if let Some(existing) = tools.get(&tool_call_id) {
                        if is_ask_user_tool(&tool_name) {
                            existing.set_display_title("ASK USER");
                            existing.set_args(&ask_user_args_display(&args));
                        } else {
                            existing.set_args(&args.to_string());
                        }
                        existing.clone()
                    } else {
                        let display_args = if is_ask_user_tool(&tool_name) {
                            ask_user_args_display(&args)
                        } else {
                            args.to_string()
                        };
                        let comp = Arc::new(ToolExecutionComponent::new(&tool_name, &display_args));
                        if is_ask_user_tool(&tool_name) {
                            comp.set_display_title("ASK USER");
                        }
                        comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                        // A `read` of a SKILL.md renders as native Pi's
                        // `[skill] <name>` invocation box (custom-message
                        // background, collapsed to one line, Ctrl+O expands the
                        // skill markdown) instead of a generic READ tool panel.
                        if let Some(skill) = skill_tool_name(&tool_name, &args) {
                            comp.set_skill_name(skill);
                        }
                        comp.set_running();
                        chat.add_child(comp.clone());
                        tools.insert(tool_call_id.clone(), comp.clone());
                        comp
                    }
                };
            }
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => {
            if tool_name.trim().is_empty() {
                return;
            }
            let partial_text = if is_ask_user_tool(&tool_name) {
                ask_user_progress_text(&args, &partial_result)
            } else {
                tool_result_text(&partial_result)
            };
            let has_partial_payload = tool_update_has_payload(&partial_text, &partial_result);
            if tool_name == "bash" {
                // Append the streamed chunk to the bash component's preview.
                // RAW text (no single-line collapsing) — the old
                // `summarize_tool_result` folded every newline into a `⏎`
                // glyph, cramming e.g. `ls -la`'s listing onto one line.
                let chunk = partial_text;
                if let Some(bash) = state.bash_components.lock().unwrap().get(&tool_call_id) {
                    // Lifecycle updates can contain a more complete args object
                    // than the snapshot that created this running panel.
                    if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
                        bash.set_command(command);
                    }
                    if has_partial_payload {
                        bash.append_output(&chunk);
                    }
                } else if has_partial_payload {
                    // ToolExecutionStart is emitted before a tool can run.
                    // Ignore an out-of-order partial until that event gives us
                    // the real command, rather than showing a spinner above an
                    // empty `$ ` header. Normal updates are handled by the
                    // component created in ToolExecutionStart.
                }
            } else if let Some(comp) = state.tool_components.lock().unwrap().get(&tool_call_id) {
                if let Some(skill) = skill_tool_name(&tool_name, &args) {
                    comp.set_skill_name(skill);
                }
                if is_ask_user_tool(&tool_name) {
                    comp.set_display_title("ASK USER");
                    comp.set_args(&ask_user_args_display(&args));
                } else if args != serde_json::Value::Null && args != serde_json::json!({}) {
                    comp.set_args(&args.to_string());
                }
                // Raw multi-line text — read/ls-style tools must show their
                // full content, not the single-line ⏎-folded summary.
                if has_partial_payload {
                    comp.set_result(&partial_text, false);
                    apply_edit_diff(comp, &tool_name, &partial_result.details, &tui);
                }
            } else if has_partial_payload {
                // No component yet — create a running one so the partial shows.
                // Empty callbacks are common before ToolExecutionStart; wait
                // for Start so the first panel has the real arguments instead
                // of an empty `TOOLS` box.
                let display_args = if is_ask_user_tool(&tool_name) {
                    ask_user_args_display(&args)
                } else {
                    args.to_string()
                };
                let comp = Arc::new(ToolExecutionComponent::new(&tool_name, &display_args));
                if is_ask_user_tool(&tool_name) {
                    comp.set_display_title("ASK USER");
                }
                comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                if let Some(skill) = skill_tool_name(&tool_name, &args) {
                    comp.set_skill_name(skill);
                }
                comp.set_running();
                comp.set_result(&partial_text, false);
                apply_edit_diff(&comp, &tool_name, &partial_result.details, &tui);
                chat.add_child(comp.clone());
                state
                    .tool_components
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), comp.clone());
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
            if tool_name.trim().is_empty() {
                return;
            }
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
                    if command.trim().is_empty() && tool_result_text(&result).trim().is_empty() {
                        state.sync_working_loader_with_bash();
                        // Edge case: empty bash result. Still render immediately
                        // to clear the panel state for the user.
                        tui.render_now(false);
                        return;
                    }
                    let comp = Arc::new(BashExecutionComponent::new(command));
                    comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                    comp.append_output(&tool_result_text(&result));
                    finalize_bash(&comp, &result, is_error);
                    chat.add_child(comp);
                }
            } else {
                let comp = state.tool_components.lock().unwrap().remove(&tool_call_id);
                if let Some(comp) = comp {
                    let result_text = if is_ask_user_tool(&tool_name) {
                        ask_user_result_text(&result)
                    } else {
                        tool_result_text(&result)
                    };
                    comp.set_result(&result_text, is_error);
                    if !is_ask_user_tool(&tool_name) {
                        apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                    }
                } else {
                    // Tool ended without a Start/Update (e.g. a very fast tool):
                    // render a finalized component directly.
                    let comp = Arc::new(ToolExecutionComponent::new(
                        &tool_name,
                        &if is_ask_user_tool(&tool_name) {
                            ask_user_args_display(&serde_json::Value::Null)
                        } else {
                            "".to_string()
                        },
                    ));
                    if is_ask_user_tool(&tool_name) {
                        comp.set_display_title("ASK USER");
                    }
                    comp.set_expanded(*state.tool_outputs_expanded.lock().unwrap());
                    let result_text = if is_ask_user_tool(&tool_name) {
                        ask_user_result_text(&result)
                    } else {
                        tool_result_text(&result)
                    };
                    comp.set_result(&result_text, is_error);
                    if !is_ask_user_tool(&tool_name) {
                        apply_edit_diff(&comp, &tool_name, &result.details, &tui);
                    }
                    chat.add_child(comp.clone());
                }
            }
            state.sync_working_loader_with_bash();
            // Bash tool completion is user-visible: immediate render ensures the
            // result is displayed even when the scheduler thread is busy or the
            // throttle window has not elapsed. This single immediate frame does
            // not regress the streaming-output throttle that keeps CPU/GPU use
            // low (bash completions are discrete events, not per-token bursts).
            tui.render_now(false);
        }
    }
}

/// Update the footer's context-window badge from one assistant response.
///
/// `Usage::context_tokens()` is the prompt size the provider reported for that
/// request — the value pi's footer renders as `{percent}%/{window}`. Aborted
/// and errored turns are skipped (their usage is not a real context
/// measurement, mirroring pi's `lastAssistantUsageInfo`) so the last valid
/// count stays on screen instead of flickering to `?`.
fn record_context_usage(footer: &rpi_tui::FooterComponent, message: &rpi_ai::AssistantMessage) {
    if message.stop_reason == rpi_ai::StopReason::Aborted
        || message.stop_reason == rpi_ai::StopReason::Error
    {
        return;
    }
    let tokens = message.usage.context_tokens();
    if tokens > 0 {
        footer.set_context_tokens(Some(tokens));
    }
}

/// Return the diagnostic carried by a failed or aborted provider request.
/// Providers may omit `error_message`; keep a stable fallback so a terminal
/// request failure can never render as an empty transcript turn.
fn assistant_error_text(message: &rpi_ai::AssistantMessage) -> Option<String> {
    let fallback = match message.stop_reason {
        rpi_ai::StopReason::Error => "Provider request failed.",
        rpi_ai::StopReason::Aborted => "Request aborted.",
        _ => return None,
    };
    Some(
        message
            .error_message
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback)
            .to_string(),
    )
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
    // Some tool backends attach the authoritative command to result details.
    // Backfill it before completing so this is a fallback, never the first
    // opportunity for the UI to show the whole command.
    if let Some(command) = result.details.get("command").and_then(|v| v.as_str()) {
        comp.set_command(command);
    }
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

/// The skill name when `tool_name` is a `read` of a `SKILL.md` file, else
/// `None`. The name is the `SKILL.md` parent directory's basename (matching
/// native Pi's skill-file convention). Ordinary markdown/document reads
/// return `None` and remain regular `READ` tool panels.
fn skill_tool_name(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name != "read" {
        return None;
    }
    let path = args.get("path").and_then(|value| value.as_str())?;
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next()?;
    if !file_name.eq_ignore_ascii_case("SKILL.md") {
        return None;
    }
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .nth(1)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
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

/// Empty progress callbacks are valid (notably before a tool's start event),
/// but they do not contain anything useful to render. Defer those callbacks so
/// the first tool panel is created from `ToolExecutionStart` with real args.
fn tool_update_has_payload(text: &str, result: &rpi_agent::AgentToolResult) -> bool {
    !text.trim().is_empty() || !result.details.is_null()
}

/// Tool names rendered through the dedicated `ASK USER` panel instead of a
/// generic tool box. Native `ask_user` plus the JS `ask_user_question` alias.
fn is_ask_user_tool(tool_name: &str) -> bool {
    matches!(tool_name, "ask_user" | "ask_user_question")
}

/// Render ask_user tool-call ARGUMENTS as a readable question. The raw
/// transport JSON must never leak into the transcript.
fn ask_user_args_display(args: &serde_json::Value) -> String {
    if args.is_null() {
        return String::new();
    }
    if let Some(summary) = args
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return format!("Confirmation requested: {summary}");
    }

    let mut lines: Vec<String> = Vec::new();
    let questions: Vec<&serde_json::Value> = args
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .filter(|items| !items.is_empty())
        .map(|items| items.iter().collect())
        .unwrap_or_else(|| vec![args]);

    for question in questions {
        let prompt = question
            .get("question")
            .and_then(serde_json::Value::as_str)
            .or_else(|| args.get("question").and_then(serde_json::Value::as_str))
            .map(str::trim)
            .unwrap_or_default();
        if prompt.is_empty() {
            continue;
        }
        if let Some(header) = question
            .get("header")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            lines.push(format!("{header}: {prompt}"));
        } else {
            lines.push(prompt.to_string());
        }
        let context = question
            .get("context")
            .and_then(serde_json::Value::as_str)
            .or_else(|| args.get("context").and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty());
        if let Some(context) = context {
            lines.push(format!("  {context}"));
        }
        let options = question
            .get("options")
            .and_then(serde_json::Value::as_array)
            .or_else(|| args.get("options").and_then(serde_json::Value::as_array));
        if let Some(options) = options {
            let labels: Vec<String> = options
                .iter()
                .filter_map(|option| {
                    option
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| option.as_str())
                        .map(str::trim)
                        .filter(|label| !label.is_empty())
                        .map(str::to_string)
                })
                .collect();
            if !labels.is_empty() {
                lines.push(format!("Choices: {}", labels.join(", ")));
            }
        }
        if let Some(suggest) = question
            .get("suggest")
            .and_then(serde_json::Value::as_str)
            .or_else(|| args.get("suggest").and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            lines.push(format!("Suggestions: {suggest}"));
        }
    }

    lines.join("\n")
}

/// Progress text while an ask_user tool is pending. Prefer the plugin's own
/// readable `content`, otherwise show the question so the panel is not blank.
fn ask_user_progress_text(args: &serde_json::Value, result: &rpi_agent::AgentToolResult) -> String {
    let text = tool_result_text(result);
    if !text.trim().is_empty() {
        return text;
    }
    let args_text = ask_user_args_display(args);
    if args_text.trim().is_empty() {
        "Waiting for your answer…".to_string()
    } else {
        args_text
    }
}

/// Final result text for an ask_user tool. The plugin returns a human-readable
/// answer summary in `content`; never surface the transport JSON.
fn ask_user_result_text(result: &rpi_agent::AgentToolResult) -> String {
    let text = tool_result_text(result);
    if text.trim().is_empty() {
        "Answer recorded.".to_string()
    } else {
        text
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
    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::List(list.clone()),
        list,
        kind,
    );
}

/// Open a searchable selector: the list plus a fuzzy-filter search box.
///
/// Returns the wrapper so callers can prefill the query (native pi's
/// `initialSearchInput`). Mirrors `SelectSubmenu` with `searchable: true`.
fn open_searchable_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    title: Option<&str>,
    description: Option<&str>,
    list: Arc<SelectList>,
    kind: SelectorKind,
) -> Arc<SearchableSelectList> {
    let searchable = Arc::new(SearchableSelectList::new(title, description, list));
    open_selector_with_view(
        state,
        editor_container,
        editor,
        tui,
        SelectorView::Searchable(searchable.clone()),
        searchable.clone(),
        kind,
    );
    // `TuiAltScreen::set_focus` only records the focused component; components
    // manage their own focus flag. Without this the search caret never renders
    // (and IME candidate positioning stays on the editor's old caret).
    searchable.set_focused(true);
    searchable
}

/// Open a selector with an optional framed view. Native extension selectors
/// wrap the list with a title and hint while built-in selectors keep the list
/// as the complete view.
///
/// `view_router` receives keys (a bare list, or a searchable wrapper) while
/// `view` is what renders — the two are the same object for every built-in
/// selector, but extension selectors frame a plain list.
fn open_selector_with_view<V: Component + 'static>(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    view_router: SelectorView,
    view: Arc<V>,
    kind: SelectorKind,
) {
    // Dispatch ui_prompt_start event
    dispatch_ui_prompt_event(state);

    // Unfocus the editor so its cursor marker doesn't render behind the list.
    editor.set_focused(false);
    // A selector replaces the editor slot. Drop stale slash/@file
    // suggestions so they cannot reappear after the selector closes.
    state.autocomplete_container.clear();
    // Swap: clear the container and add the selector view.
    editor_container.clear();
    editor_container.add_child(view.clone());
    *state.active_selector.lock().unwrap() = Some((view_router, kind));
    let focused: Arc<dyn Component> = view;
    tui.set_focus(Some(focused));
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
    // Dispatch ui_prompt_end event
    dispatch_ui_prompt_event_end(state);

    editor_container.clear();
    editor_container.add_child(editor.clone());
    state.autocomplete_container.clear();
    editor.set_focused(true);
    *state.active_selector.lock().unwrap() = None;
    *state.active_extension_cancel.lock().unwrap() = None;
    tui.set_focus(Some(editor.clone()));
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
    initial_search: Option<&str>,
) -> Option<Arc<SearchableSelectList>> {
    let items = model_selector_items(catalog, lane_model_id);
    if items.is_empty() {
        add_note_message(
            chat,
            "No models in the catalog. Use --model at startup to select one.",
        );
        tui.request_render(false);
        return None;
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

    let searchable = open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Select model"),
        Some("Type to filter by name, provider, or id"),
        list,
        SelectorKind::Model,
    );
    if let Some(term) = initial_search {
        searchable.set_search_text(term);
    }
    Some(searchable)
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
/// default session dir (`<cwd>/.rpi/sessions`, with legacy `.pi/sessions`
/// fallback). Selecting reports "restore not
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

    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Resume session"),
        Some("Type to filter by session id"),
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
            let label = if current.as_deref() == Some(entry.id()) {
                format!("{} #{} (current)", entry.entry_type(), entry.seq())
            } else {
                format!("{} #{}", entry.entry_type(), entry.seq())
            };
            let short_id = &entry.id()[..entry.id().len().min(12)];
            // Search on the entry type, sequence, and id so `/tree` stays
            // usable in long sessions.
            let search_text = format!("{} {} {}", entry.entry_type(), entry.seq(), entry.id());
            SelectItem::new(entry.id(), &label)
                .with_description(short_id)
                .with_search_text(&search_text)
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
    open_searchable_selector(
        state,
        editor_container,
        editor,
        tui,
        Some("Session tree"),
        Some("Type to filter by entry type, sequence, or id"),
        list,
        SelectorKind::Tree,
    );
}

/// Build + open the `/theme` selector. Built-in presets and enabled package
/// themes are shown; selecting applies the theme live and re-renders.
fn open_theme_selector(
    state: &Arc<TuiState>,
    editor_container: &Arc<Container>,
    editor: &Arc<Editor>,
    tui: &Arc<TuiAltScreen>,
    cwd: &std::path::Path,
    package_resources: &Arc<crate::packages::PackageResources>,
) {
    let mut items = vec![
        SelectItem::new("dark", "Dark").with_description("Default dark theme"),
        SelectItem::new("light", "Light").with_description("Light background"),
        SelectItem::new("monochrome", "Monochrome").with_description("No color accents"),
    ];
    if state.themes_enabled {
        for path in package_resources.theme_files() {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                items.push(SelectItem::new(name, name).with_description("Package theme"));
            }
        }
    }
    let list = Arc::new(SelectList::new(items, 10));

    let state_sel = state.clone();
    let ec_sel = editor_container.clone();
    let editor_sel = editor.clone();
    let tui_sel = tui.clone();
    let chat_sel = state.chat_container.clone();
    let cwd_sel = cwd.to_path_buf();
    let package_resources_sel = package_resources.clone();
    list.on_select(Arc::new(move |item| {
        let preset = match item.value.as_str() {
            "light" => Some(ThemePreset::Light),
            "monochrome" => Some(ThemePreset::Monochrome),
            "dark" => Some(ThemePreset::Dark),
            name => {
                if state_sel.themes_enabled {
                    if let Ok(custom) = crate::packages::load_theme_with_resources(
                        &cwd_sel,
                        name,
                        &package_resources_sel,
                    ) {
                        rpi_tui::global_theme_manager().set(custom.clone());
                        state_sel.theme_manager.set(custom);
                    }
                }
                add_note_message(&chat_sel, &format!("Theme set to {}.", item.label));
                close_selector(&state_sel, &ec_sel, &editor_sel, &tui_sel);
                tui_sel.render_now(true);
                return;
            }
        };
        let Some(preset) = preset else { return };
        apply_theme_preset(preset);
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
    let mut active = match tokio::runtime::Handle::try_current() {
        Ok(h) => h
            .block_on(async { lane.get_active_tools().await })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    // An empty active set is the harness sentinel for "all registered tools"
    // (the selector only exposes built-ins). Expand it before rendering and
    // toggling so the first `/tools` visit does not show every tool as off or
    // accidentally reduce the active set to the one item selected.
    if active.is_empty() {
        active = crate::session::BUILTIN_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
    }
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
    let cursor = editor_cursor_offset(editor, &text);
    let suggestions = state.autocomplete.get_suggestions(&text, cursor);
    render_autocomplete(state, suggestions);
}

/// Convert the editor's logical `(row, byte-column)` caret into the absolute
/// byte offset expected by autocomplete providers.
fn editor_cursor_offset(editor: &Editor, text: &str) -> usize {
    let (row, col) = editor.cursor_position();
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == row {
            return (offset + col.min(line.len())).min(text.len());
        }
        offset = offset.saturating_add(line.len() + 1);
    }
    text.len()
}

/// Restore an editor caret from an absolute byte offset after autocomplete
/// replaces a span in a multi-line draft.
fn set_editor_cursor_offset(editor: &Editor, text: &str, offset: usize) {
    let offset = offset.min(text.len());
    let mut consumed = 0;
    for (row, line) in text.split('\n').enumerate() {
        let end = consumed + line.len();
        if offset <= end {
            editor.set_cursor(row, offset - consumed);
            return;
        }
        consumed = end + 1;
    }
    let last_row = text.bytes().filter(|byte| *byte == b'\n').count();
    editor.set_cursor(
        last_row,
        text.rsplit('\n').next().map(str::len).unwrap_or(0),
    );
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
    // Cap the list so the dock doesn't swallow the transcript.
    let accent = state.theme_manager.get().colors.accent;
    let muted = state.theme_manager.get().colors.muted;
    for (i, item) in sugg
        .items
        .iter()
        .take(state.autocomplete_max_visible)
        .enumerate()
    {
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
    let cursor = editor_cursor_offset(editor, &text);
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
    set_editor_cursor_offset(editor, &replaced, new_cursor);
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
    // The three-bar brand mark, matching the site favicon
    // (website/favicon.svg): two dim outer bars flanking a taller accent bar,
    // bottom-aligned. The SVG's heights (13/19/9) are rounded to the five rows
    // a terminal header can afford. Using block characters rather than the
    // previous emoji avoids the east-asian-width ambiguity that made the old
    // crab-and-box logo render inconsistently across terminals.
    let bar = c.dim.fg("██");
    let bar_accent = c.accent.fg("██");
    let logo = format!(
        "{}\n{}\n{}\n{}\n{}",
        format!("     {bar_accent}"),
        format!("     {bar_accent}"),
        format!("  {bar} {bar_accent}"),
        format!("  {bar} {bar_accent} {bar}"),
        format!(
            "  {bar} {bar_accent} {bar}   {} {}",
            c.accent.fg(&tui_bold("rpi")),
            c.muted.fg("· rust")
        ),
    );
    container.add_child(Arc::new(Text::new(logo, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    // The mark carries the product name, so the line under it is the tagline
    // rather than a second logotype.
    let title = c.muted.fg("interactive TUI · library-first agent runtime");
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

/// Append update notices inside the live transcript. Fullscreen mode clears
/// pre-TUI stdout/stderr, so update state must be represented by components.
fn add_update_notices(container: &Arc<Container>, report: &crate::updates::UpdateReport) {
    let colors = current_theme().colors;
    let group = Arc::new(Container::new());

    for warning in &report.warnings {
        let body = format!(
            "{}\n{} {}{}",
            colors.error.fg(&warning.message),
            colors.muted.fg("Run"),
            colors.accent.fg(&warning.command),
            colors.muted.fg(" to retry.")
        );
        add_update_panel(&group, "Update Failed", &body, colors.error);
    }

    if let Some(notice) = report.notices.iter().find(|notice| notice.name == "rpi") {
        let body = format!(
            "{} {}{}",
            colors
                .muted
                .fg(&format!("New version {} is available. Run", notice.latest)),
            colors.accent.fg(&notice.command),
            colors.muted.fg(".")
        );
        add_update_panel(&group, "Update Available", &body, colors.warning);
    }

    let package_notices = report
        .notices
        .iter()
        .filter(|notice| notice.name != "rpi")
        .collect::<Vec<_>>();
    if !package_notices.is_empty() {
        let command = package_notices[0].command.as_str();
        let mut lines = vec![format!(
            "{} {}{}",
            colors.muted.fg("Package updates are available. Run"),
            colors.accent.fg(command),
            colors.muted.fg(".")
        )];
        lines.push(colors.muted.fg("Packages:"));
        lines.extend(
            package_notices
                .into_iter()
                .map(|notice| format!("- {} {} -> {}", notice.name, notice.current, notice.latest)),
        );
        add_update_panel(
            &group,
            "Package Updates Available",
            &lines.join("\n"),
            colors.warning,
        );
    }

    // Other transcript producers append concurrently. Add the fully built
    // group in one operation so card borders and content cannot interleave
    // with user, assistant, tool, or extension messages.
    if group.child_count() > 0 {
        container.add_child(group);
    }
}

fn add_update_panel(container: &Arc<Container>, title: &str, body: &str, color: rpi_tui::Color) {
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
    container.add_child(Arc::new(Text::new(
        format!("{}\n{body}", color.fg(&tui_bold(title))),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
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
        ("/changelog", "Show recent release changes"),
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

/// Add a compact `/changelog` block to the chat container. Keep this local to
/// the binary so the command remains useful in installed builds without a
/// source checkout or a network request.
fn add_changelog_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("Recent Changes")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let entries = [
        (
            "Native parity phase 1",
            "models, images, trust, export, and JSON events",
        ),
        (
            "TUI controls",
            "external editor, thinking levels, and tool output toggles",
        ),
        (
            "Provider auth",
            "OpenAI-compatible API key aliases and gateway headers",
        ),
    ];
    for (release, summary) in entries {
        let row = format!("  {}  {}", c.accent.fg(release), c.muted.fg(summary));
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.dim.fg("Version"), c.text.fg(crate::VERSION)),
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
        ("Ctrl+O", "Expand/collapse all tool output"),
        ("Ctrl+T", "Show/hide reasoning blocks"),
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
/// Collect all rendered lines from the chat container for search.
fn collect_transcript_lines(chat_container: &Arc<Container>) -> Vec<String> {
    let width = 80; // Default width for search; actual width varies by terminal
    chat_container.render(width)
}

/// Build the shell command that resumes `session_id` in this project, or
/// `None` when there is nothing to resume. Mirrors native pi's
/// `formatResumeCommand`: `rpi --session <id>`, prefixing
/// `--session-dir <dir>` only when a non-default directory was requested.
fn format_resume_command(
    session_id: &str,
    cwd: &std::path::Path,
    session_dir: Option<&std::path::Path>,
) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut args = vec![crate::APP_NAME.to_string()];
    if let Some(dir) = session_dir {
        let default = crate::session::default_session_dir(cwd);
        if dir != default.as_path() {
            args.push("--session-dir".to_string());
            args.push(quote_shell_arg(&dir.to_string_lossy()));
        }
    }
    args.push("--session".to_string());
    args.push(session_id.to_string());
    Some(args.join(" "))
}

/// Quote a shell argument when it contains whitespace or shell metacharacters
/// (mirrors native pi's `quoteIfNeeded`).
fn quote_shell_arg(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || "\"'`$&|;<>()*?[]{}!~#\\".contains(c));
    if needs_quoting {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn add_error_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    let text = sanitize_error_message(text);
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.error.fg("✗"), c.error.fg(&text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Keep provider diagnostics printable in the transcript. HTTP error bodies
/// may contain carriage returns, terminal escapes, or an unexpectedly large
/// JSON payload; letting those bytes reach the renderer can corrupt the input
/// row or make the whole frame exceed terminal limits.
fn sanitize_error_message(text: &str) -> String {
    const MAX_ERROR_CHARS: usize = 16 * 1024;
    let mut result = String::with_capacity(text.len().min(MAX_ERROR_CHARS));
    let mut count = 0;
    // 0 = normal, 1 = escape introducer, 2 = CSI, 3 = OSC.
    let mut escape_mode = 0u8;
    for ch in text.chars() {
        if escape_mode != 0 {
            match escape_mode {
                1 if ch == '[' => escape_mode = 2,
                1 if ch == ']' => escape_mode = 3,
                1 if ch == '\x07' || ('@'..='~').contains(&ch) => escape_mode = 0,
                2 if ('@'..='~').contains(&ch) => escape_mode = 0,
                3 if ch == '\x07' => escape_mode = 0,
                _ => {}
            }
            continue;
        }
        if ch == '\x1b' {
            escape_mode = 1;
            continue;
        }
        if count >= MAX_ERROR_CHARS {
            result.push_str("…");
            break;
        }
        match ch {
            '\n' | '\t' => {
                result.push(ch);
                count += 1;
            }
            '\r' => {}
            c if c.is_control() => {}
            c => {
                result.push(c);
                count += 1;
            }
        }
    }
    if result.trim().is_empty() {
        "Provider request failed.".to_string()
    } else {
        result
    }
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
            "  Skills: (none discovered — create .rpi/skills/ (.pi/skills also works) or ~/.rpi/agent/skills/)".into(),
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
            "  Prompt templates: (none — create .rpi/prompts/ (.pi/prompts also works) or ~/.rpi/agent/prompts/)".into(),
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

    /// Minimal `TuiState` for unit tests that only touch state flags. Mirrors
    /// the explicit literals other tests build, but keeps one copy in sync.
    fn test_tui_state() -> Arc<TuiState> {
        Arc::new(TuiState {
            current_assistant: std::sync::Mutex::new(None),
            tool_components: std::sync::Mutex::new(HashMap::new()),
            bash_components: std::sync::Mutex::new(HashMap::new()),
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
        })
    }

    #[test]
    fn live_panel_repaints_while_a_tool_or_bash_is_running() {
        // No panels → only the dock loader animates (reuse the cached
        // transcript instead of rebuilding a long history).
        let empty: HashMap<String, Arc<ToolExecutionComponent>> = HashMap::new();
        assert!(!transcript_has_live_panel(false, &empty));

        // A running tool call is a live panel: its elapsed readout is computed
        // at render time, so it must force a transcript rebuild every tick.
        let mut tools = HashMap::new();
        let tool = Arc::new(ToolExecutionComponent::new("search", "{}"));
        tool.set_running();
        tools.insert("tc1".to_string(), tool);
        assert!(transcript_has_live_panel(false, &tools));

        // A finished tool no longer needs the repaint, but a running bash
        // command still does.
        tools.get("tc1").unwrap().set_result("done", false);
        assert!(!transcript_has_live_panel(false, &tools));
        assert!(transcript_has_live_panel(true, &tools));
    }

    #[test]
    fn dequeue_merge_preserves_a_typed_draft() {
        assert_eq!(merge_queued_into_draft("queued", ""), "queued");
        assert_eq!(merge_queued_into_draft("queued", "   "), "queued");
        assert_eq!(
            merge_queued_into_draft("queued", "typed"),
            "queued\n\ntyped"
        );
    }

    #[test]
    fn settings_menu_covers_the_wired_settings_surface() {
        let state = test_tui_state();
        let settings = crate::settings::Settings::default();
        let items = settings_menu_items(&settings, &state, "claude-sonnet-5");
        let keys: Vec<&str> = items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "theme",
                "model",
                "thinking",
                "scoped-models",
                "hide-thinking",
                "show-images",
                "cache-miss-notices",
                "quiet-startup",
                "terminal-progress",
                "fullscreen-copy-on-select",
                "double-escape-action",
                "editor-padding",
                "autocomplete-max-items",
                "http-idle-timeout",
            ]
        );
        // Submenu rows must not cycle their displayed value.
        for item in items.iter().filter(|item| item.has_submenu) {
            assert!(
                item.values.is_empty(),
                "{} should be submenu-only",
                item.key
            );
        }
        // Value rows advertise a cycle that contains the current value.
        for item in items.iter().filter(|item| !item.has_submenu) {
            assert!(!item.values.is_empty(), "{} has no values", item.key);
            assert!(
                item.values.iter().any(|value| value == &item.value),
                "{} current value {} not in {:?}",
                item.key,
                item.value,
                item.values
            );
        }
        // The default model row falls back to the running lane's model.
        assert_eq!(items[1].value, "claude-sonnet-5");
        // Native defaults: cache-miss notices off, terminal progress on.
        assert_eq!(items[6].value, "false");
        assert_eq!(items[8].value, "true");
    }

    #[test]
    fn apply_setting_change_updates_settings_and_live_state() {
        let state = test_tui_state();
        let mut settings = crate::settings::Settings::default();

        // Live: hide-thinking flips both the runtime flag and the persisted one.
        let note = apply_setting_change(&state, "hide-thinking", "true", &mut settings);
        assert!(state.hide_thinking());
        assert_eq!(settings.hide_thinking_block, Some(true));
        assert!(note.expect("user-visible note").contains("hidden"));

        // Live: show-images writes the runtime mutex.
        apply_setting_change(&state, "show-images", "false", &mut settings);
        assert!(!*state.show_images.lock().unwrap());
        assert_eq!(settings.show_images, Some(false));

        // Live: cache-miss notices.
        apply_setting_change(&state, "cache-miss-notices", "true", &mut settings);
        assert!(*state.cache_miss_notices.lock().unwrap());
        assert_eq!(settings.show_cache_miss_notices, Some(true));

        // Persist-only rows change settings without a transcript note.
        assert_eq!(
            apply_setting_change(&state, "editor-padding", "3", &mut settings),
            None
        );
        assert_eq!(settings.editor_padding_x, Some(3));
        apply_setting_change(&state, "double-escape-action", "none", &mut settings);
        assert_eq!(settings.double_escape_action.as_deref(), Some("none"));
        apply_setting_change(&state, "autocomplete-max-items", "20", &mut settings);
        assert_eq!(settings.autocomplete_max_visible, Some(20));

        // HTTP idle timeout maps labels onto the numeric/`disabled` forms.
        apply_setting_change(&state, "http-idle-timeout", "disabled", &mut settings);
        assert_eq!(settings.http_idle_timeout_ms(), Some(0));
        apply_setting_change(&state, "http-idle-timeout", "1m", &mut settings);
        assert_eq!(settings.http_idle_timeout_ms(), Some(60_000));

        // Unknown keys are ignored rather than panicking.
        assert_eq!(
            apply_setting_change(&state, "nope", "x", &mut settings),
            None
        );
    }

    #[test]
    fn add_user_message_renders_a_visible_user_bubble() {
        // The submit handler renders directly-sent prompts through this helper
        // (the harness emits no user `message_start` for them), so a silent
        // failure here would make every prompt vanish from the transcript.
        let chat = Arc::new(Container::new());
        add_user_message(&chat, "hello from the user");
        let rendered = crate::interactive_tui::collect_transcript_lines(&chat);
        let plain = strip_ansi(&rendered.join("\n"));
        assert!(
            plain.contains("hello from the user"),
            "user bubble must show the prompt text; got: {plain:?}"
        );
    }

    #[test]
    fn verbose_overrides_quiet_startup_listing() {
        assert!(should_show_startup_listing(false, false));
        assert!(should_show_startup_listing(true, false));
        assert!(should_show_startup_listing(true, true));
        assert!(!should_show_startup_listing(false, true));
    }

    #[test]
    fn resume_command_omits_the_default_session_dir() {
        let cwd = std::path::Path::new("/proj/demo");
        assert_eq!(format_resume_command("", cwd, None), None);
        assert_eq!(
            format_resume_command("abc-123", cwd, None).as_deref(),
            Some("rpi --session abc-123")
        );
        // The default dir carries no flag (mirrors native pi).
        let default = crate::session::default_session_dir(cwd);
        assert_eq!(
            format_resume_command("abc-123", cwd, Some(default.as_path())).as_deref(),
            Some("rpi --session abc-123")
        );
        // A custom dir is echoed, quoting when needed.
        let custom = std::path::Path::new("/tmp/my sessions");
        assert_eq!(
            format_resume_command("abc-123", cwd, Some(custom)).as_deref(),
            Some("rpi --session-dir \"/tmp/my sessions\" --session abc-123")
        );
    }

    #[test]
    fn quote_shell_arg_quotes_only_when_needed() {
        assert_eq!(quote_shell_arg("plain"), "plain");
        assert_eq!(quote_shell_arg("with space"), "\"with space\"");
        assert_eq!(quote_shell_arg("a&b"), "\"a&b\"");
    }

    #[test]
    fn invalid_npm_command_fallback_keeps_only_git_package_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let git_root = tmp.path().join(".pi/git/github.com/example/repo");
        let local_root = tmp.path().join("local-package");
        std::fs::create_dir_all(git_root.join(".git")).unwrap();
        std::fs::create_dir_all(&local_root).unwrap();
        std::fs::write(
            git_root.join("package.json"),
            r#"{"name":"git-demo","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            local_root.join("package.json"),
            r#"{"name":"local-demo","version":"1.0.0"}"#,
        )
        .unwrap();
        let resources = crate::packages::discover(
            tmp.path(),
            &[
                "git:github.com/example/repo".to_string(),
                local_root.to_string_lossy().into_owned(),
            ],
        );
        assert_eq!(resources.packages.len(), 2);

        let filtered = git_only_update_resources(resources);

        assert_eq!(filtered.packages.len(), 1);
        assert!(matches!(
            filtered.packages[0].source,
            crate::packages::PackageSource::Git
        ));
    }

    #[test]
    fn trust_command_uses_the_session_cwd_after_process_chdir() {
        const CHILD_ENV: &str = "RPI_TEST_TRUST_COMMAND_CHILD";
        const SESSION_CWD_ENV: &str = "RPI_TEST_TRUST_COMMAND_SESSION_CWD";
        const TEST_NAME: &str =
            "interactive_tui::tests::trust_command_uses_the_session_cwd_after_process_chdir";

        if std::env::var_os(CHILD_ENV).is_some() {
            let session_cwd = std::path::PathBuf::from(
                std::env::var_os(SESSION_CWD_ENV).expect("child session cwd should be configured"),
            );
            let changed_cwd = std::env::current_dir().unwrap();

            set_project_trust_for_command(&session_cwd, Some(true)).unwrap();

            assert_eq!(
                crate::config::project_trust_decision(&session_cwd).unwrap(),
                Some(true)
            );
            assert_eq!(
                crate::config::project_trust_decision(&changed_cwd).unwrap(),
                None
            );
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let session_cwd = tmp.path().join("session-project");
        let changed_cwd = tmp.path().join("extension-cwd");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&session_cwd).unwrap();
        std::fs::create_dir_all(&changed_cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(SESSION_CWD_ENV, &session_cwd)
            .env(crate::config::CONFIG_DIR_ENV, &agent_dir)
            .current_dir(&changed_cwd)
            .status()
            .unwrap();

        assert!(status.success(), "child test process failed: {status}");
    }

    #[test]
    fn tui_startup_settings_prefer_rpi_project_fields_over_pi() {
        let global = crate::settings::Settings {
            editor_padding_x: Some(3),
            autocomplete_max_visible: Some(6),
            hide_thinking_block: Some(false),
            quiet_startup: Some(true),
            show_terminal_progress: Some(true),
            ..Default::default()
        };
        let rpi_project = crate::settings::Settings {
            editor_padding_x: Some(0),
            hide_thinking_block: Some(true),
            quiet_startup: Some(false),
            show_terminal_progress: Some(false),
            ..Default::default()
        };
        let pi_project = crate::settings::Settings {
            editor_padding_x: Some(9),
            autocomplete_max_visible: Some(12),
            quiet_startup: Some(true),
            ..Default::default()
        };

        assert_eq!(
            resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true),
            TuiStartupSettings {
                editor_padding_x: 0,
                autocomplete_max_visible: 12,
                hide_thinking: true,
                quiet_startup: false,
                show_terminal_progress: false,
                show_images: true,
                cache_miss_notices: false,
            }
        );
    }

    #[test]
    fn tui_startup_settings_fall_back_from_rpi_to_pi_per_field() {
        let global = crate::settings::Settings {
            quiet_startup: Some(false),
            ..Default::default()
        };
        let rpi_project = crate::settings::Settings::default();
        let pi_project = crate::settings::Settings {
            quiet_startup: Some(true),
            ..Default::default()
        };

        let resolved = resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true);

        assert!(resolved.quiet_startup);
    }

    #[test]
    fn tui_startup_settings_use_global_values_without_project_fields() {
        let global = crate::settings::Settings {
            editor_padding_x: Some(7),
            autocomplete_max_visible: Some(8),
            hide_thinking_block: Some(true),
            quiet_startup: Some(true),
            show_terminal_progress: Some(false),
            show_images: Some(false),
            show_cache_miss_notices: Some(true),
            ..Default::default()
        };

        assert_eq!(
            resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true,),
            TuiStartupSettings {
                editor_padding_x: 7,
                autocomplete_max_visible: 8,
                hide_thinking: true,
                quiet_startup: true,
                show_terminal_progress: false,
                show_images: false,
                cache_miss_notices: true,
            }
        );
    }

    #[test]
    fn tui_startup_settings_read_native_nested_terminal_block() {
        // Native pi nests these under `terminal`; rpi's flat keys stay
        // compatible and the nested block is the fallback.
        let global = crate::settings::Settings {
            terminal: Some(crate::settings::TerminalSettings {
                show_images: Some(false),
                show_terminal_progress: Some(false),
                clear_on_shrink: None,
            }),
            ..Default::default()
        };
        let resolved =
            resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
        assert!(!resolved.show_images);
        assert!(!resolved.show_terminal_progress);
    }

    #[test]
    fn tui_startup_settings_flat_keys_win_over_nested_terminal_block() {
        let global = crate::settings::Settings {
            show_images: Some(true),
            terminal: Some(crate::settings::TerminalSettings {
                show_images: Some(false),
                show_terminal_progress: None,
                clear_on_shrink: None,
            }),
            ..Default::default()
        };
        let resolved =
            resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
        assert!(resolved.show_images);
    }

    #[test]
    fn tui_startup_settings_ignore_untrusted_project_values() {
        let global = crate::settings::Settings {
            quiet_startup: Some(false),
            show_terminal_progress: Some(true),
            ..Default::default()
        };
        let project = crate::settings::Settings {
            quiet_startup: Some(true),
            show_terminal_progress: Some(false),
            ..Default::default()
        };

        let resolved = resolve_tui_startup_settings(&global, &[project], false);

        assert!(!resolved.quiet_startup);
        assert!(resolved.show_terminal_progress);
    }

    #[test]
    fn transcript_page_uses_viewport_with_overlap() {
        assert_eq!(transcript_page_size(24), 20);
        assert_eq!(transcript_page_size(4), 1);
        assert_eq!(transcript_page_size(0), 1);
    }

    #[test]
    fn parse_user_bash_distinguishes_single_and_double_bang() {
        // `!cmd` runs and stays in context.
        assert_eq!(parse_user_bash("!ls -la"), Some(("ls -la", false)));
        // `!!cmd` runs but is excluded from context.
        assert_eq!(parse_user_bash("!!ls -la"), Some(("ls -la", true)));
        // Surrounding whitespace on the command is trimmed, the `!` prefix is not.
        assert_eq!(parse_user_bash("!  echo hi  "), Some(("echo hi", false)));
        assert_eq!(parse_user_bash("!!  echo hi"), Some(("echo hi", true)));
    }

    #[test]
    fn parse_user_bash_ignores_bare_bang_and_plain_text() {
        // A bare `!` / `!!` has no command: fall through to the prompt path
        // instead of executing an empty shell line.
        assert_eq!(parse_user_bash("!"), None);
        assert_eq!(parse_user_bash("!!"), None);
        assert_eq!(parse_user_bash("!   "), None);
        assert_eq!(parse_user_bash("hello"), None);
        assert_eq!(parse_user_bash(""), None);
        // `/` keeps routing to the slash-command registry.
        assert_eq!(parse_user_bash("/model"), None);
    }

    #[test]
    fn user_bash_run_slot_guards_and_cancels() {
        let state = test_tui_state();
        assert!(!state.user_bash_running());
        let cancel = state.begin_user_bash();
        assert!(state.user_bash_running());
        // Esc cancels without freeing the slot; the spawned task clears it when
        // the capture resolves (native pi's `isBashRunning`).
        assert!(state.cancel_user_bash());
        assert!(state.user_bash_running());
        assert!(cancel.is_cancelled());
        state.finish_user_bash();
        assert!(!state.user_bash_running());
        // Nothing to cancel once idle.
        assert!(!state.cancel_user_bash());
    }

    #[test]
    fn key_repeat_is_dispatched_but_release_is_not() {
        assert!(should_dispatch_key(KeyEventKind::Press));
        assert!(should_dispatch_key(KeyEventKind::Repeat));
        assert!(!should_dispatch_key(KeyEventKind::Release));
    }

    #[test]
    fn key_event_encoding_matches_pi_keybinding_protocol() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            key_event_to_input(key(KeyCode::Enter, KeyModifiers::NONE)),
            "\r"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            "\x1b[13;2u"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Tab, KeyModifiers::SHIFT)),
            "\x1b[9;2u"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            "\x1b[Z"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::BackTab, KeyModifiers::NONE)),
            "\x1b[Z"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "\x03"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            "\x0f"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Char('!'), KeyModifiers::SHIFT)),
            "!"
        );
        assert_eq!(
            key_event_to_input(key(KeyCode::Char('1'), KeyModifiers::SHIFT)),
            "1"
        );
    }

    #[test]
    fn dialog_cancel_before_open_is_consumed_without_stranding_request() {
        let bridge = JsDialogBridge::default();
        let (sender, receiver) = std_mpsc::channel();
        bridge.pending.lock().unwrap().push_back(JsDialogPending {
            request: JsDialogRequest {
                id: "dialog-1".into(),
                method: "input".into(),
                title: String::new(),
                message: String::new(),
                options: Vec::new(),
                placeholder: None,
                prefill: None,
            },
            result: sender,
        });

        // Model the cancellation arriving after the queue entry has been
        // removed but before the TUI has installed the native widget.
        bridge.cancel("dialog-1");
        assert!(bridge.take_pending().is_none());
        assert_eq!(
            receiver.recv().unwrap(),
            serde_json::json!({ "cancelled": true })
        );
        assert!(bridge.cancelled_before_open.lock().unwrap().is_empty());
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
    fn welcome_header_renders_the_brand_mark() {
        let chat = Arc::new(Container::new());
        add_welcome_message(&chat);
        let lines: Vec<String> = chat.render(80).iter().map(|l| strip_ansi(l)).collect();
        let all = lines.join("\n");

        // The three-bar mark: the bottom row carries all three bars, and the
        // wordmark sits on its baseline next to the middle bar.
        assert!(
            lines.iter().any(|l| l.contains("██ ██ ██")),
            "brand mark missing: {all}"
        );
        assert!(
            all.contains("rpi · rust"),
            "the mark must be labelled with the Rust wordmark: {all}"
        );

        // Regression guard: the previous logo used an emoji plus a π glyph,
        // whose east-asian width is ambiguous and made the header render
        // inconsistently across terminals.
        assert!(
            !all.contains('🍣') && !all.contains('π'),
            "the header must stay within unambiguous single-width characters: {all}"
        );
    }

    #[test]
    fn update_notices_render_inside_the_transcript() {
        let chat = Arc::new(Container::new());
        let report = crate::updates::UpdateReport {
            notices: vec![
                crate::updates::UpdateNotice {
                    name: "rpi".into(),
                    current: "0.1.10".into(),
                    latest: "0.1.11".into(),
                    command: "rpi update".into(),
                },
                crate::updates::UpdateNotice {
                    name: "rpi-search".into(),
                    current: "0.1.0".into(),
                    latest: "0.1.1".into(),
                    command: "rpi pi-package update".into(),
                },
            ],
            warnings: vec![crate::updates::UpdateWarning {
                message: "The previously scheduled rpi update failed: access denied".into(),
                command: "rpi update".into(),
            }],
        };

        add_update_notices(&chat, &report);

        assert_eq!(chat.child_count(), 1);
        let plain = strip_ansi(&chat.render(80).join("\n"));
        assert!(plain.contains("Update Failed"), "{plain}");
        assert!(
            plain.contains("rpi update failed: access denied"),
            "{plain}"
        );
        assert!(plain.contains("Update Available"), "{plain}");
        assert!(plain.contains("New version 0.1.11 is available"), "{plain}");
        assert!(plain.contains("rpi update"), "{plain}");
        assert!(plain.contains("Package Updates Available"), "{plain}");
        assert!(plain.contains("rpi pi-package update"), "{plain}");
        assert!(plain.contains("- rpi-search 0.1.0 -> 0.1.1"), "{plain}");
    }

    #[test]
    fn empty_update_report_does_not_add_transcript_content() {
        let chat = Arc::new(Container::new());

        add_update_notices(&chat, &crate::updates::UpdateReport::default());

        assert_eq!(chat.child_count(), 0);
    }

    #[test]
    fn skill_reads_are_detected_by_path() {
        let name = skill_tool_name(
            "read",
            &serde_json::json!({"path": "C:/work/.rpi/skills/release/SKILL.md"}),
        );
        assert_eq!(name.as_deref(), Some("release"));

        let name = skill_tool_name("read", &serde_json::json!({"path": "/docs/README.md"}));
        assert!(name.is_none());

        // Only `read` (not other tools) triggers the skill box.
        assert!(skill_tool_name("grep", &serde_json::json!({"path": "/s/x/SKILL.md"})).is_none());
    }

    #[test]
    fn welcome_capabilities_show_empty_state() {
        let plain = strip_ansi(&welcome_capability_line("Skills", &[]));
        assert_eq!(plain, "Skills (0) none");
    }

    #[test]
    fn empty_tool_progress_is_deferred_until_start() {
        let empty = rpi_agent::AgentToolResult::default();
        assert!(!tool_update_has_payload("", &empty));

        let text = rpi_agent::AgentToolResult::text("partial output");
        assert!(tool_update_has_payload("partial output", &text));

        let details = rpi_agent::AgentToolResult {
            details: serde_json::json!({"path": "src/lib.rs"}),
            ..Default::default()
        };
        assert!(tool_update_has_payload("", &details));
    }

    #[test]
    fn completed_stream_reconciles_the_final_tail_before_detaching() {
        let component = Arc::new(AssistantMessageComponent::new(
            AssistantMessageOptions::default(),
        ));
        component.set_streaming(true);
        component.update_blocks(&[AssistantBlock::Text("partial response".into())]);

        let current = Mutex::new(Some(component.clone()));
        let cached = Mutex::new("partial response".to_string());
        let mut final_message = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "faux-model", 0);
        final_message.content = vec![Content::text(
            "partial response with the previously missing final tail",
        )];
        final_message.stop_reason = rpi_ai::types::StopReason::Stop;

        reconcile_streamed_assistant_completion(&current, &cached, Some(&final_message));

        assert!(current.lock().unwrap().is_none());
        assert_eq!(
            cached.lock().unwrap().as_str(),
            "partial response with the previously missing final tail"
        );
        let rendered = strip_ansi(&component.render(100).join("\n"));
        assert!(
            rendered.contains("previously missing final tail"),
            "{rendered}"
        );
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
        let manager = AutocompleteManager::new();
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
        let cursor = editor_cursor_offset(&editor, &text);
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
        let new_cursor = start + top.text.len();
        editor.set_text(&replaced);
        set_editor_cursor_offset(&editor, &replaced, new_cursor);
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
    fn multiline_autocomplete_preserves_row_and_column() {
        let editor = Arc::new(Editor::simple());
        editor.set_text("first\n/mo");
        editor.set_cursor(1, 3);

        let text = editor.get_text();
        assert_eq!(editor_cursor_offset(&editor, &text), 9);

        let replaced = "first\n/model";
        editor.set_text(replaced);
        set_editor_cursor_offset(&editor, replaced, 12);
        assert_eq!(editor.cursor_position(), (1, 6));
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
        resolves_to("/changelog", "/changelog");
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
            "/changelog",
            "/model",
            "/session",
            "/theme",
            "/compact",
            "/copy",
            "/hotkeys",
            "/tools",
            "/images",
            "/thinking",
            "/usage",
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
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
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
        // The active loader now renders inside the editor's top border.
        assert!(state.editor.working().is_some());
        assert_eq!(state.status_container.child_count(), 0);
        assert!(state.footer.get_status().is_empty());
        state.show_retry(3, 10, 8_000);
        let retry_status = strip_ansi(&state.status_container.render(80).join("\n"));
        assert!(retry_status.contains("Retrying (3/10)"));
        state.set_status(RunStatus::Working);
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
        // The editor-border indicator stays visible across bash panels, so the
        // status container stays empty.
        assert_eq!(state.status_container.child_count(), 0);
        assert!(state.editor.working().is_some());

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
    fn context_badge_ignores_aborted_and_error_turns() {
        use rpi_ai::types::{Api, StopReason, Usage};

        let footer = Arc::new(FooterComponent::new());
        footer.set_context_window(100_000);

        // A real response fills in the percent (`?` -> `25.0%/100k`).
        let mut ok = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
        ok.stop_reason = StopReason::Stop;
        ok.usage = Usage {
            input: 25_000,
            ..Usage::zero()
        };
        record_context_usage(&footer, &ok);
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("25.0%/100k"), "stats: {stats}");

        // An aborted turn must not clobber the last good value.
        let mut aborted =
            rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
        aborted.stop_reason = StopReason::Aborted;
        aborted.usage = Usage {
            input: 90_000,
            ..Usage::zero()
        };
        record_context_usage(&footer, &aborted);
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("25.0%/100k"), "stats: {stats}");

        // An errored turn likewise (its usage is not a real measurement).
        let mut errored =
            rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
        errored.stop_reason = StopReason::Error;
        errored.usage = Usage {
            input: 90_000,
            ..Usage::zero()
        };
        record_context_usage(&footer, &errored);
        let stats = strip_ansi(&footer.render(120).join("\n"));
        assert!(stats.contains("25.0%/100k"), "stats: {stats}");
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
    fn model_selector_items_are_deduplicated_and_provider_qualified() {
        use rpi_ai::{Api, Model};

        let mut gateway = Model::new(
            "gpt-5.6-sol",
            "GPT 5.6 Sol",
            Api::OpenaiCompletions,
            "routeryo-copy",
            "https://gateway.example.com",
        );
        let duplicate = gateway.clone();
        let anthropic = Model::new(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        gateway.headers = Some(std::collections::BTreeMap::from([(
            "authorization".into(),
            "Bearer test".into(),
        )]));

        let items = model_selector_items(&[gateway, duplicate, anthropic], "gpt-5.6-sol");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].value, "gpt-5.6-sol");
        assert_eq!(items[0].label, "GPT 5.6 Sol");
        assert_eq!(
            items[0].description.as_deref(),
            Some("routeryo-copy/gpt-5.6-sol (current)")
        );
        assert_eq!(items[1].description.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn model_selector_search_text_covers_provider_name_and_qualified_id() {
        use rpi_ai::{Api, Model};

        let anthropic = Model::new(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let items = model_selector_items(&[anthropic], "");
        let search = items[0].search_key();
        // Mirrors native `getModelSelectorSearchText`: provider-prefixed first,
        // then the display name.
        assert!(search.starts_with("anthropic anthropic/claude-sonnet-5"));
        assert!(search.contains("Claude Sonnet 5"), "{search}");

        // A provider query and a display-name query both match through the
        // real fuzzy filter used by the searchable selector.
        use rpi_tui::fuzzy_filter;
        for query in ["anthropic", "sonnet", "Claude Sonnet", "claude-sonnet-5"] {
            let matched = fuzzy_filter(&items, query, |item| item.search_key());
            assert_eq!(matched.len(), 1, "query {query:?} matched nothing");
        }
    }

    #[test]
    fn model_selector_match_accepts_bare_and_qualified_ids() {
        use rpi_ai::{Api, Model};

        let gateway = Model::new(
            "gpt-5.6-sol",
            "GPT 5.6 Sol",
            Api::OpenaiCompletions,
            "routeryo-copy",
            "https://gateway.example.com",
        );
        let anthropic = Model::new(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let catalog = [gateway, anthropic];
        assert_eq!(
            find_model_selector_match(&catalog, "gpt-5.6-sol")
                .unwrap()
                .provider,
            "routeryo-copy"
        );
        assert_eq!(
            find_model_selector_match(&catalog, "routeryo-copy/gpt-5.6-sol")
                .unwrap()
                .id,
            "gpt-5.6-sol"
        );
        assert_eq!(
            find_model_selector_match(&catalog, "anthropic/claude-sonnet-5")
                .unwrap()
                .id,
            "claude-sonnet-5"
        );
        assert!(find_model_selector_match(&catalog, "other/gpt-5.6-sol").is_none());
    }

    #[test]
    fn assistant_error_text_keeps_terminal_provider_diagnostic_visible() {
        use rpi_ai::types::{AssistantMessage, AssistantRole, StopReason, Usage};

        let failed = AssistantMessage {
            role: AssistantRole,
            content: Vec::new(),
            api: rpi_ai::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some("upstream returned 401".into()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        };
        assert_eq!(
            assistant_error_text(&failed).as_deref(),
            Some("upstream returned 401")
        );

        let mut no_detail = failed;
        no_detail.error_message = Some("  ".into());
        assert_eq!(
            assistant_error_text(&no_detail).as_deref(),
            Some("Provider request failed.")
        );

        let mut aborted = no_detail;
        aborted.stop_reason = StopReason::Aborted;
        aborted.error_message = Some("abort error: Request aborted".into());
        assert_eq!(
            assistant_error_text(&aborted).as_deref(),
            Some("abort error: Request aborted")
        );

        aborted.error_message = None;
        assert_eq!(
            assistant_error_text(&aborted).as_deref(),
            Some("Request aborted.")
        );
    }

    #[test]
    fn sanitize_error_message_keeps_diagnostics_without_terminal_controls() {
        assert_eq!(
            sanitize_error_message("405\r\nMethod Not Allowed\x1b[2J"),
            "405\nMethod Not Allowed"
        );
        assert_eq!(sanitize_error_message("\0\tmessage"), "\tmessage");
        assert_eq!(sanitize_error_message("   "), "Provider request failed.");
        let long = "x".repeat(20_000);
        let cleaned = sanitize_error_message(&long);
        assert!(cleaned.chars().count() <= 16 * 1024 + 1);
        assert!(cleaned.ends_with('…'));
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
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
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
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
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
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
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
            themes_enabled: true,
            hide_thinking: std::sync::Mutex::new(false),
            tool_outputs_expanded: std::sync::Mutex::new(false),
            show_terminal_progress: true,
            status: std::sync::Mutex::new(RunStatus::Idle),
            ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
            ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
            js_preparation_cancel: std::sync::Mutex::new(None),
            user_bash_cancel: std::sync::Mutex::new(None),
            pending_bash_messages: std::sync::Mutex::new(Vec::new()),
            footer: Arc::new(FooterComponent::new()),
            status_container: Arc::new(Container::new()),
            chat_container: Arc::new(Container::new()),
            loader: Arc::new(Loader::new()),
            editor: Arc::new(Editor::simple()),
            last_assistant_text: std::sync::Mutex::new(String::new()),
            active_selector: std::sync::Mutex::new(None),
            active_extension_editor: std::sync::Mutex::new(None),
            active_extension_input: std::sync::Mutex::new(None),
            active_extension_cancel: std::sync::Mutex::new(None),
            autocomplete: AutocompleteManager::new(),
            autocomplete_container: Arc::new(Container::new()),
            autocomplete_max_visible: 5,
            pending_images: std::sync::Mutex::new(Vec::new()),
            theme_manager: Arc::new(ThemeManager::new()),
            tui: None,
            current_model_id: std::sync::Mutex::new(String::new()),
            show_images: std::sync::Mutex::new(true),
            cache_miss_notices: std::sync::Mutex::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            history_index: std::sync::Mutex::new(-1),
            history_draft: std::sync::Mutex::new(None),
            cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
            scoped_edit: std::sync::Mutex::new(None),
            markdown_transformer: std::sync::Mutex::new(None),
            extension_session: Arc::new(std::sync::Mutex::new(
                rpi_extensions::ExtensionSession::none(),
            )),
            search: Arc::new(AltScreenSearch::new()),
            search_bar: Arc::new(SearchBar::new()),
            pending_container: Arc::new(Container::new()),
            dequeue_hint: String::new(),
            pending_snapshot: std::sync::Mutex::new(
                rpi_harness::agent_harness::QueuedMessages::default(),
            ),
            selection_start: std::sync::Mutex::new(None),
            selection_end: std::sync::Mutex::new(None),
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

    #[test]
    fn configured_key_parser_supports_native_notation() {
        let combo = parse_configured_key("Ctrl+G").expect("ctrl+g should parse");
        assert_eq!(combo.code, KeyCode::Char('g'));
        assert!(combo.modifiers.contains(KeyModifiers::CONTROL));
        let combo = parse_configured_key("shift+tab").expect("shift+tab should parse");
        assert_eq!(combo.code, KeyCode::BackTab);
    }

    #[test]
    fn double_escape_trigger_has_half_second_window() {
        let now = std::time::Instant::now();
        assert!(!double_escape_trigger(None, now));
        assert!(double_escape_trigger(
            Some(now - std::time::Duration::from_millis(500)),
            now
        ));
        assert!(!double_escape_trigger(
            Some(now - std::time::Duration::from_millis(501)),
            now
        ));
    }

    #[test]
    fn pasted_multi_line_block_becomes_one_prompt_not_many() {
        // End-to-end shape of the fix: replay the key stream a Windows paste
        // produces ("l1\nl2\nl3" + a trailing Enter) through the same pieces
        // the key loop uses — the burst discriminator for each Enter and the
        // editor for the text — and assert that *no* submit fired while the
        // block arrived, then exactly one submit on the user's own Enter.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let editor = rpi_tui::Editor::simple();
        let submits = Arc::new(AtomicUsize::new(0));
        let submits_for_cb = submits.clone();
        editor.on_submit(Arc::new(move |_text: &str| {
            submits_for_cb.fetch_add(1, Ordering::SeqCst);
        }));

        // Every pasted key is delivered back-to-back, so `now` is unchanged and
        // the console queue is never empty until the paste is drained.
        let now = std::time::Instant::now();
        let mut last_text_key_at: Option<std::time::Instant> = None;
        let block = ["l1", "l2", "l3"];
        for (index, line) in block.iter().enumerate() {
            for ch in line.chars() {
                editor.insert(&ch.to_string());
                last_text_key_at = Some(now);
            }
            if index + 1 < block.len() {
                // The pasted newline: more input is still queued behind it.
                let more_queued = true;
                assert!(
                    enter_is_paste_burst(last_text_key_at, now, more_queued),
                    "pasted newline {index} must not submit"
                );
                editor.insert("\n");
                last_text_key_at = Some(now);
            }
        }
        assert_eq!(editor.get_text(), "l1\nl2\nl3");
        assert_eq!(submits.load(Ordering::SeqCst), 0, "paste must not submit");

        // The user's own Enter: a real gap, nothing queued.
        let typed_at = now + PASTE_BURST_GAP + std::time::Duration::from_millis(80);
        assert!(!enter_is_paste_burst(last_text_key_at, typed_at, false));
        editor.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            submits.load(Ordering::SeqCst),
            1,
            "typed Enter submits once"
        );
    }

    #[test]
    fn pasted_win_crlf_text_has_no_stray_carriage_returns() {
        // A Windows paste delivers CRLF; the editor must fold it to LF so the
        // rendered block is clean and the cursor arithmetic stays sound.
        let editor = rpi_tui::Editor::simple();
        editor.insert("alpha\r\nbeta\r\n");
        assert_eq!(editor.get_text(), "alpha\nbeta\n");
        assert!(!editor.get_text().contains('\r'));
    }

    #[test]
    fn pasted_enter_is_not_a_submit() {
        let now = std::time::Instant::now();

        // Queued input behind the Enter => paste, regardless of history.
        assert!(enter_is_paste_burst(None, now, true));

        // Enter immediately after pasted characters => paste (also covers a
        // paste whose final line ends with a newline, where nothing is queued).
        assert!(enter_is_paste_burst(
            Some(now - std::time::Duration::from_millis(1)),
            now,
            false
        ));
        assert!(enter_is_paste_burst(
            Some(now - (PASTE_BURST_GAP - std::time::Duration::from_millis(1))),
            now,
            false
        ));

        // A typed Enter: nothing queued and a real gap since the last key.
        assert!(!enter_is_paste_burst(None, now, false));
        assert!(!enter_is_paste_burst(
            Some(now - PASTE_BURST_GAP),
            now,
            false
        ));
        assert!(!enter_is_paste_burst(
            Some(now - std::time::Duration::from_millis(120)),
            now,
            false
        ));
    }

    #[test]
    fn paste_probe_ignores_the_enter_keys_own_release() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let release =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);

        // Windows queues press+release together over RDP: the release behind a
        // lone Enter must NOT count as queued paste content.
        let mut queued = vec![Event::Key(release)].into_iter();
        let (more, stashed) = probe_paste_input(None, || queued.next());
        assert!(!more, "a key release alone is not queued paste input");
        assert!(stashed.is_none());

        // A real follow-up key (the next pasted line) IS queued input and is
        // stashed for the caller instead of being lost.
        let next = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut queued = vec![Event::Key(release), Event::Key(next)].into_iter();
        let (more, stashed) = probe_paste_input(None, || queued.next());
        assert!(more, "a queued key press is paste continuation");
        assert!(matches!(stashed, Some(Event::Key(_))));

        // Nothing queued at all.
        let (more, stashed) = probe_paste_input(None, || None);
        assert!(!more);
        assert!(stashed.is_none());

        // An already-stashed event counts as queued input, without reading more.
        let (more, stashed) = probe_paste_input(Some(Event::Key(next)), || {
            panic!("must not read when an event is already stashed")
        });
        assert!(more);
        assert!(matches!(stashed, Some(Event::Key(_))));
    }

    // ---- ask_user TUI bridge ----

    #[test]
    fn ask_user_tool_names_are_recognized() {
        assert!(is_ask_user_tool("ask_user"));
        assert!(is_ask_user_tool("ask_user_question"));
        assert!(!is_ask_user_tool("bash"));
    }

    #[test]
    fn ask_user_args_display_never_leaks_transport_json() {
        let text = ask_user_args_display(&serde_json::json!({
            "question": "你的代理端口是多少？",
            "context": "本机",
            "options": ["7890", {"title": "7897", "description": "clash"}],
            "suggest": "1080"
        }));
        assert!(text.contains("你的代理端口是多少？"));
        assert!(text.contains("本机"));
        assert!(text.contains("Choices: 7890, 7897"));
        assert!(text.contains("Suggestions: 1080"));
        assert!(!text.trim_start().starts_with('{'));

        let multi = ask_user_args_display(&serde_json::json!({
            "questions": [
                {"id": "a", "question": "Q1"},
                {"id": "b", "header": "H", "question": "Q2"}
            ]
        }));
        assert!(multi.contains("Q1"));
        assert!(multi.contains("H: Q2"));

        assert_eq!(
            ask_user_args_display(&serde_json::json!({"type": "confirm", "summary": "Deploy?"})),
            "Confirmation requested: Deploy?"
        );
        assert!(ask_user_args_display(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn ask_user_result_and_progress_prefer_readable_content() {
        let waiting = rpi_agent::AgentToolResult::text("Waiting for your answer\n端口?");
        assert!(
            ask_user_progress_text(&serde_json::json!({"question": "端口?"}), &waiting)
                .contains("端口?")
        );
        let empty = rpi_agent::AgentToolResult::default();
        assert!(
            ask_user_progress_text(&serde_json::json!({"question": "端口?"}), &empty)
                .contains("端口?")
        );
        let answered = rpi_agent::AgentToolResult::text("端口?: 7897");
        assert_eq!(ask_user_result_text(&answered), "端口?: 7897");
        assert_eq!(ask_user_result_text(&empty), "Answer recorded.");
    }

    #[test]
    fn ask_user_prompt_parser_supports_flat_and_questions_shapes() {
        let flat = rpi_extensions::UiDialogRequest {
            request_id: "r1".into(),
            tool_call_id: Some("c1".into()),
            ui: serde_json::json!({
                "kind": "selector",
                "id": "proxy_port",
                "header": "Proxy",
                "question": "端口?",
                "options": [{"title": "7890", "description": "text"}, {"title": "7897"}],
                "allowMultiple": true,
                "allowFreeform": false,
                "suggest": "1080"
            }),
            raw: serde_json::json!({}),
        };
        let prompt = parse_ask_user_prompt(&flat);
        assert_eq!(prompt.question_id, "proxy_port");
        assert_eq!(prompt.header.as_deref(), Some("Proxy"));
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].1.as_deref(), Some("text"));
        assert!(prompt.allow_multiple);
        assert!(!prompt.allow_freeform);
        assert_eq!(prompt.suggest.as_deref(), Some("1080"));

        let alias = rpi_extensions::UiDialogRequest {
            ui: serde_json::json!({
                "questions": [{"id": "q", "question": "Q?", "options": []}]
            }),
            ..flat.clone()
        };
        let prompt = parse_ask_user_prompt(&alias);
        assert_eq!(prompt.question_id, "q");
        assert!(prompt.allow_freeform, "no options defaults to freeform");

        let confirm = rpi_extensions::UiDialogRequest {
            ui: serde_json::json!({"kind": "confirm", "question": "Go?"}),
            ..flat.clone()
        };
        let prompt = parse_ask_user_prompt(&confirm);
        assert_eq!(prompt.kind, "confirm");
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].0, "Yes");
    }

    #[test]
    fn ask_user_bridge_answers_and_cancels_by_request_id() {
        let mailbox = rpi_extensions::UiDialogMailbox::new();
        mailbox.attach();
        let bridge = AskUserBridge::new(mailbox.clone());

        // Two concurrent requests must not cross answers.
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "r1", "toolCallId": "call-r1",
                "ui": {"kind": "input", "question": "Port?"}
            }))
            .unwrap();
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "r2", "toolCallId": "call-r2",
                "ui": {"kind": "input", "question": "Level?"}
            }))
            .unwrap();

        let first = bridge.take_pending().expect("r1 visible");
        assert_eq!(first.request_id, "r1");
        assert!(
            bridge.take_pending().is_none(),
            "only one request occupies the input slot"
        );
        bridge.respond("r1", serde_json::json!({"text": "7897"}));
        assert_eq!(mailbox.poll("r1").unwrap()["answer"]["text"], "7897");

        let second = bridge.take_pending().expect("r2 visible");
        assert_eq!(second.request_id, "r2");
        bridge.cancel("r2");
        assert_eq!(mailbox.poll("r2").unwrap()["status"], "cancelled");

        // cancel_all drops anything still queued.
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "r3",
                "ui": {"kind": "input", "question": "X?"}
            }))
            .unwrap();
        bridge.cancel_all();
        assert_eq!(mailbox.poll("r3").unwrap()["status"], "cancelled");

        // A detached mailbox rejects new prompts (headless contract).
        bridge.shutdown();
        assert!(mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "r4", "ui": {"kind": "input"}
            }))
            .is_err());
    }
}
