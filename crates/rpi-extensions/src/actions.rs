//! B5a — the plugin→host `runtime_action` bridge (inverted FFI).
//!
//! A plugin invokes `PluginApiVt::runtime_action` to drive the harness (send a
//! message, switch models, fork a session, reload extensions, …). Unlike the
//! register trampolines — which run synchronously inside the selected register
//! entrypoint
//! and recover host state via the thread-local `CURRENT_HOST_API` —
//! `runtime_action` is called **post-register**, from (a) a `spawn_blocking`
//! pool thread mid-tool-drive, or (b) a foreign thread the plugin spawned
//! itself. Neither has the thread-local set, and the host cannot predict which
//! threads a plugin will call from. Thread-local is the wrong tool here.
//!
//! The verified-correct recovery channel is [`PluginApiVt::user_data`]: it is
//! `Send+Sync`, populated at vtable build, passed back unchanged on every call,
//! and the SDK designates it "the host's opaque context". Today every register
//! trampoline ignores `user_data` (the register path uses the thread-local), so
//! repurposing `user_data` for the action bridge breaks nothing.
//!
//! [`ActionBridge`] holds a [`tokio::runtime::Handle`] **captured at build
//! time** (the host is on the runtime when it constructs the bridge) — the fix
//! for the foreign-thread case: `Handle::spawn` works from any thread, no
//! ambient runtime needed. The real [`trampoline_runtime_action`] derefs
//! `user_data` as `&ActionBridge`, drives the host's async dispatch on the
//! runtime via a `std::sync::mpsc::sync_channel(1)`, and parks the plugin thread
//! on `rx.recv()` (STD — not `tokio::oneshot`, whose `recv` needs a runtime the
//! plugin's foreign thread lacks).
//!
//! ## Cycle-free leaf DAG
//!
//! [`RuntimeActionHost`] is defined here (NOT in `rpi-harness`) so
//! `rpi-extensions` stays a leaf: the trait names only JSON + primitives — no
//! `rpi-harness` types cross. The host impl (`HarnessActionHost`) lives in
//! `rpi-cli`, where it can name the harness freely; `rpi-extensions` only
//! carries the async surface. This preserves the documented DAG
//! (`lib.rs:10-16`: `rpi-extensions` does NOT depend on `rpi-harness`).

use std::ffi::c_void;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use rpi_plugin_sdk::{RuntimeActionId, StbString, StbStringRef};
use tokio::runtime::Handle;

/// A pending host UI request emitted by a plugin runtime action.
///
/// The request and tool-call ids are intentionally separate: a single tool
/// call may ask more than one question, while concurrent tool calls must never
/// consume one another's answers. `raw` retains the complete action envelope;
/// `ui` is the normalized UI payload that TUI renderers generally need.
#[derive(Clone, Debug, PartialEq)]
pub struct UiDialogRequest {
    pub request_id: String,
    pub tool_call_id: Option<String>,
    pub ui: serde_json::Value,
    pub raw: serde_json::Value,
}

struct UiDialogEntry {
    request: UiDialogRequest,
    answer: Option<serde_json::Value>,
    cancelled: bool,
}

struct UiDialogState {
    attached: bool,
    pending: VecDeque<String>,
    active: HashMap<String, UiDialogEntry>,
}

/// Thread-safe mailbox connecting plugin runtime actions to an interactive
/// host (normally the TUI). It deliberately carries only JSON and primitives,
/// so the extension crate remains independent of the UI crate.
#[derive(Clone)]
pub struct UiDialogMailbox {
    state: Arc<Mutex<UiDialogState>>,
}

impl Default for UiDialogMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl UiDialogMailbox {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(UiDialogState {
                attached: false,
                pending: VecDeque::new(),
                active: HashMap::new(),
            })),
        }
    }

    /// Mark an interactive consumer as available. Re-attaching is idempotent.
    pub fn attach(&self) {
        self.state.lock().expect("ui dialog mailbox poisoned").attached = true;
    }

    /// Detach the consumer and cancel every outstanding request. This prevents
    /// a TUI restart/reload from leaving plugin polls blocked forever.
    pub fn detach(&self) {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        state.attached = false;
        state.pending.clear();
        for entry in state.active.values_mut() {
            entry.cancelled = true;
            entry.answer = None;
        }
    }

    pub fn is_attached(&self) -> bool {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .attached
    }

    /// Remove and return the next request for presentation by the TUI.
    /// Cancelled/answered requests are skipped; duplicate `open` calls never
    /// enqueue the same request id twice.
    pub fn take_pending(&self) -> Option<UiDialogRequest> {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        while let Some(request_id) = state.pending.pop_front() {
            if let Some(entry) = state.active.get(&request_id) {
                if !entry.cancelled && entry.answer.is_none() {
                    return Some(entry.request.clone());
                }
            }
        }
        None
    }

    pub fn pending_len(&self) -> usize {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .pending
            .len()
    }

    /// Supply an answer for a request. Repeating the same operation is
    /// idempotent; answering a cancelled/unknown request is an explicit error.
    pub fn respond(&self, request_id: &str, answer: serde_json::Value) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let entry = state
            .active
            .get_mut(request_id)
            .ok_or_else(|| format!("unknown UI request `{request_id}`"))?;
        if entry.cancelled {
            return Err(format!("UI request `{request_id}` was cancelled"));
        }
        if entry.answer.is_none() {
            entry.answer = Some(answer);
        }
        Ok(())
    }

    /// Mark one request cancelled. The terminal state remains visible to the
    /// plugin's next `poll`, then is reclaimed by that poll.
    pub fn cancel(&self, request_id: &str) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let entry = state
            .active
            .get_mut(request_id)
            .ok_or_else(|| format!("unknown UI request `{request_id}`"))?;
        entry.cancelled = true;
        entry.answer = None;
        state.pending.retain(|id| id != request_id);
        Ok(())
    }

    /// Cancel all requests currently known to the mailbox.
    pub fn cancel_all(&self) {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        state.pending.clear();
        for entry in state.active.values_mut() {
            entry.cancelled = true;
            entry.answer = None;
        }
    }

    /// Poll a request and consume a terminal answer/cancellation. Pending
    /// polls are cheap and preserve the request for subsequent calls.
    pub fn poll(&self, request_id: &str) -> Result<serde_json::Value, String> {
        let mut state = self.state.lock().map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let terminal = match state.active.get(request_id) {
            Some(entry) if entry.cancelled => Some(serde_json::json!({
                "status": "cancelled",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
            })),
            Some(entry) if entry.answer.is_some() => Some(serde_json::json!({
                "status": "answered",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
                "answer": entry.answer.clone().unwrap_or(serde_json::Value::Null),
            })),
            Some(entry) => Some(serde_json::json!({
                "status": "pending",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
            })),
            None => None,
        };
        let Some(result) = terminal else {
            return Err(format!("unknown UI request `{request_id}`"));
        };
        if result.get("status").and_then(serde_json::Value::as_str) != Some("pending") {
            state.active.remove(request_id);
        }
        Ok(result)
    }

    /// Parse and execute the JSON action envelope used by runtime action 17.
    /// Supported operations are `open`, `poll`, `cancel`; `response` is also
    /// accepted as a convenience for hosts that proxy answers through JSON.
    pub fn handle(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let op = args.get("op").and_then(serde_json::Value::as_str).unwrap_or("open");
        let request_id = || {
            args.get("requestId")
                .or_else(|| args.get("request_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| "missing non-empty `requestId`".to_string())
        };
        match op {
            "open" => {
                let id = request_id()?;
                let mut state = self.state.lock().map_err(|_| "ui dialog mailbox poisoned".to_string())?;
                if !state.attached {
                    return Err("ask_user requires an interactive UI".to_string());
                }
                if let Some(entry) = state.active.get(&id) {
                    return Ok(if entry.cancelled {
                        serde_json::json!({"status":"cancelled","requestId":id})
                    } else if let Some(answer) = &entry.answer {
                        serde_json::json!({"status":"answered","requestId":id,"answer":answer})
                    } else {
                        serde_json::json!({"status":"pending","requestId":id})
                    });
                }
                let tool_call_id = args
                    .get("toolCallId")
                    .or_else(|| args.get("tool_call_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let ui = args.get("ui").cloned().unwrap_or_else(|| args.clone());
                state.active.insert(
                    id.clone(),
                    UiDialogEntry {
                        request: UiDialogRequest {
                            request_id: id.clone(),
                            tool_call_id: tool_call_id.clone(),
                            ui,
                            raw: args,
                        },
                        answer: None,
                        cancelled: false,
                    },
                );
                state.pending.push_back(id.clone());
                Ok(serde_json::json!({"status":"pending","requestId":id,"toolCallId":tool_call_id}))
            }
            "poll" => self.poll(&request_id()?),
            "cancel" => {
                let id = request_id()?;
                self.cancel(&id)?;
                Ok(serde_json::json!({"status":"cancelled","requestId":id}))
            }
            "response" | "respond" => {
                let id = request_id()?;
                let answer = args
                    .get("answer")
                    .or_else(|| args.get("value"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.respond(&id, answer)?;
                Ok(serde_json::json!({"status":"answered","requestId":id}))
            }
            other => Err(format!("unknown UI dialog operation `{other}`")),
        }
    }
}

/// The host-side implementation the bridge delegates to. Defined in
/// `rpi-extensions` (NOT `rpi-harness`) so the crate DAG stays a leaf: this is a
/// trait the **host** (`rpi-cli`) implements over the harness — `rpi-extensions`
/// only names the async surface + carries JSON params/results. No `rpi-harness`
/// types appear in the trait.
///
/// Each method corresponds to one [`RuntimeActionId`] variant. Complex args
/// arrive as parsed JSON (`serde_json::Value`); complex results return as
/// `serde_json::Value`. The host maps its native types (Model, AgentMessage, …)
/// to/from JSON at the impl boundary. Errors are `String` (become the action's
/// nonzero `i32` + `{"error": msg}` JSON on the plugin side).
///
/// The 18 methods map 1:1 to [`RuntimeActionId`]; `dispatch` below is the
/// exhaustive switch that connects the FFI id to the method.
#[async_trait::async_trait]
pub trait RuntimeActionHost: Send + Sync {
    /// `SendMessage` — drive a full agent run from an assistant/user message.
    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SendUserMessage` — drive a run from a user-text message.
    async fn send_user_message(&self, args: serde_json::Value)
        -> Result<serde_json::Value, String>;
    /// `AppendEntry` — append a raw entry to the session transcript (no run).
    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetSessionName` — set the session's display name.
    async fn set_session_name(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetActiveTools` — the active tool-name list.
    async fn get_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetActiveTools` — replace the active tool-name list.
    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetModel` — switch the active model (by id; host resolves to a `Model`).
    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetThinkingLevel` — the current thinking level.
    async fn get_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `SetThinkingLevel` — set the thinking level.
    async fn set_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `Compact` — compact the session transcript.
    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetSystemPrompt` — the live composed system prompt.
    async fn get_system_prompt(&self, args: serde_json::Value)
        -> Result<serde_json::Value, String>;
    /// `NewSession` — start a fresh session and switch to it.
    async fn new_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `Fork` — fork the current session and switch to the fork.
    async fn fork(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `NavigateTree` — navigate/rewind the session tree.
    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SwitchSession` — hot-switch to an existing session by id.
    async fn switch_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `Reload` — re-run extension discovery + Part-A resource loaders (a CLI
    /// concern; the host impl wires it to the `ActionBridge.reload` callback in
    /// B5d — until then this returns an "unsupported" error string).
    async fn reload(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;

    /// `GetCliFlag` — read a parsed extension CLI flag. Args are
    /// `{"name":"flag"}` and the result is `{"value": <bool|string|null>}`.
    async fn get_cli_flag(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("CLI flag lookup is not configured".to_string())
    }

    /// `UiDialog` fallback for hosts that do not install an interactive
    /// mailbox. The default is deliberately an error so headless runs cannot
    /// accidentally report a fabricated user answer.
    async fn ui_dialog(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("ask_user requires an interactive UI".to_string())
    }
}

/// The host-side bridge carried in [`PluginApiVt::user_data`] so
/// [`trampoline_runtime_action`] can recover the harness state from any thread.
///
/// `runtime: Handle` is captured at build time (the host is on the runtime when
/// it constructs the bridge) — this is the foreign-thread fix. `host` is the
/// `rpi-cli` impl over the harness. `reload` is a CLI-owned callback that
/// re-runs extension discovery + Part-A loaders (a CLI concern, NOT a harness
/// op) — wired by `rpi-cli` in B5d via [`reload_callback_from_mailbox`].
///
/// ## Staleness (B5d)
///
/// `active: Arc<AtomicBool>` is shared with a [`ReloadMailbox`]-driven swap
/// site. A `/reload` (TUI command OR a plugin's `runtime_action(Reload)`) builds
/// a fresh `ExtensionSession` + a fresh `ActionBridge`, calls
/// [`invalidate`](Self::invalidate) on the old bridge, and swaps the new one in.
/// In-flight `runtime_action` calls that recovered the OLD bridge from
/// `user_data` (the pointer a plugin stored during the prior `register`) then
/// hit the staleness guard in [`run_action`] and fail with a structured error
/// instead of driving a half-swapped harness. (Plugins load fresh on reload,
/// handing them the NEW bridge pointer; the guard only catches the race window
/// where an old call is still parked on `rx.recv()`.)
///
/// Held behind `Arc` (pointer-stable for the bridge's lifetime via
/// [`Arc::as_ptr`]); `rpi-cli` keeps one clone for the session lifetime so the
/// pointer a plugin stored during register stays valid post-register. (The
/// transient `HostApi` built per `load_one` holds a clone only during register
/// — when it drops after `take_registry`, the master `Arc` in `rpi-cli` keeps
/// the allocation alive.)
pub struct ActionBridge {
    /// Captured at build time from a thread running the target runtime.
    pub(crate) runtime: Handle,
    /// The host impl (`HarnessActionHost` in rpi-cli).
    pub(crate) host: Arc<dyn RuntimeActionHost>,
    /// B5d reload callback; `None` until the TUI wires `/reload`.
    pub(crate) reload:
        Option<Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
    /// Session-scoped UI mailbox. It is cloned into reload-created bridges so
    /// a TUI attachment and in-flight requests survive extension reloads.
    ui_dialog: UiDialogMailbox,
    /// Session-scoped extension status registry (`SetStatus` runtime action).
    /// Shared with the TUI, which polls [`ExtensionStatusMailbox::revision`]
    /// and repaints the footer only when an extension actually wrote.
    ext_status: crate::status::ExtensionStatusMailbox,
    /// B5d staleness flag. Shared so [`invalidate`] flips it for every clone.
    /// `true` while this bridge is the live session's bridge.
    active: Arc<AtomicBool>,
}

impl ActionBridge {
    /// Build a bridge. The `Handle` MUST be captured from a thread running the
    /// target runtime (pi-cli builds the bridge on the async main thread).
    pub fn new(runtime: Handle, host: Arc<dyn RuntimeActionHost>) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            host,
            reload: None,
            ui_dialog: UiDialogMailbox::new(),
            ext_status: crate::status::ExtensionStatusMailbox::new(),
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Same as [`new`](Self::new) with a reload callback (B5d wires this via
    /// [`reload_callback_from_mailbox`]).
    pub fn with_reload(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
    ) -> Arc<Self> {
        Self::with_reload_and_ui(
            runtime,
            host,
            reload,
            UiDialogMailbox::new(),
            crate::status::ExtensionStatusMailbox::new(),
        )
    }

    /// Build a bridge with an explicit session-scoped UI mailbox.
    pub fn with_ui_dialog(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        ui_dialog: UiDialogMailbox,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            host,
            reload: None,
            ui_dialog,
            ext_status: crate::status::ExtensionStatusMailbox::new(),
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    /// `with_reload` variant that preserves a mailbox across bridge swaps.
    pub fn with_reload_and_ui(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
        ui_dialog: UiDialogMailbox,
        ext_status: crate::status::ExtensionStatusMailbox,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            host,
            reload: Some(reload),
            ui_dialog,
            ext_status,
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Clone the session mailbox for TUI attachment or a reload-created bridge.
    pub fn ui_dialog_mailbox(&self) -> UiDialogMailbox {
        self.ui_dialog.clone()
    }

    /// Clone the session status registry (TUI rendering + reload-created bridges).
    pub fn extension_status_mailbox(&self) -> crate::status::ExtensionStatusMailbox {
        self.ext_status.clone()
    }

    /// Mark this bridge stale (B5d). A `/reload` that swaps in a fresh bridge
    /// calls this on the old one so in-flight `runtime_action` calls parked on
    /// the old `user_data` pointer fail fast with a staleness error instead of
    /// driving the swapped-out session. Idempotent.
    pub fn invalidate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Whether this bridge is still the live session's bridge.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// B5d: clone the host impl so a `/reload` can build a FRESH `ActionBridge`
    /// over the SAME `RuntimeActionHost` (the host's harness cell already points
    /// at the live harness — the harness is NOT rebuilt on reload — so the host
    /// is reusable across reloads; only the bridge's staleness flag + reload
    /// callback differ). The fresh bridge gets a fresh `active` flag (true) +
    /// the reload callback the TUI installed; the old bridge is `invalidate`d.
    pub fn clone_host(&self) -> Arc<dyn RuntimeActionHost> {
        Arc::clone(&self.host)
    }
}

/// A reload-signal mail slot (B5d). The reload callback (built by
/// [`reload_callback_from_mailbox`]) captures a clone; the TUI installs a
/// `tokio` unbounded sender after it starts. When a plugin calls
/// `runtime_action(Reload)`, the callback signals `()` (if a TUI is installed)
/// and the TUI performs the reload **asynchronously** — the plugin's call
/// returns `Ok(null)` immediately, so the calling plugin's cdylib is NOT
/// unmapped while its `runtime_action` frame is still on the stack (the reload,
/// which drops the old keepalive, happens after the call returns). This breaks
/// the self-unmapping race a synchronous plugin-initiated reload would have.
///
/// rpi-extensions carries only `()` (no pi-cli `TuiMessage` type) — preserving
/// the leaf DAG. The TUI owns the receiver + the actual reload routine.
#[derive(Clone)]
pub struct ReloadMailbox {
    tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>>,
}

impl Default for ReloadMailbox {
    fn default() -> Self {
        Self {
            tx: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl ReloadMailbox {
    /// A fresh empty mail slot (no TUI installed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the TUI's reload-signal sender (after the TUI starts). Replaces
    /// any prior sender. The TUI drops the receiver on shutdown; a sender held
    /// here keeps the channel half-open, so [`clear`] on shutdown is advised.
    pub fn install(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        *self.tx.lock().unwrap() = Some(tx);
    }

    /// Signal a reload (plugin-initiated via `runtime_action(Reload)`).
    /// `Ok(())` if a TUI is installed (the signal was enqueued; the TUI may
    /// still be mid-reload). `Err(())` if no TUI is installed (the host returns
    /// a "reload not available" error to the plugin).
    pub fn signal(&self) -> Result<(), ()> {
        let g = self.tx.lock().unwrap();
        match &*g {
            Some(tx) => {
                let _ = tx.send(());
                Ok(())
            }
            None => Err(()),
        }
    }

    /// Drop the installed sender (TUI shutdown). Idempotent.
    pub fn clear(&self) {
        *self.tx.lock().unwrap() = None;
    }
}

/// Build the reload callback the bridge carries, backed by a [`ReloadMailbox`].
/// When a plugin calls `runtime_action(Reload)`, the bridge's spawn site awaits
/// this callback, which signals the TUI (if installed) and returns; the plugin
/// receives `Ok(null)` and the TUI performs the reload asynchronously. If no
/// TUI is installed, the callback returns without signalling and the host's
/// [`RuntimeActionHost::reload`] fallback surfaces the "not configured" error.
pub fn reload_callback_from_mailbox(
    mailbox: ReloadMailbox,
) -> Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync> {
    Arc::new(move || {
        let m = mailbox.clone();
        Box::pin(async move {
            let _ = m.signal();
        })
    })
}

// `Handle` is Send+Sync, `Arc<dyn RuntimeActionHost>` (with `Send + Sync` bound)
// is Send+Sync, and the reload closure is `Send + Sync` — so `ActionBridge` is
// naturally Send+Sync; no manual unsafe impl needed.

/// Dispatch one action to the host. Async — runs on the bridge's runtime.
/// Handles all 18 ids; `Reload` delegates to [`RuntimeActionHost::reload`]
/// (the "no callback configured" fallback). When the bridge has a reload
/// callback, the spawn site intercepts `Reload` and awaits the callback
/// instead (a CLI concern, not a harness op) — this helper is the plain
/// host-only path.
async fn dispatch(
    host: &Arc<dyn RuntimeActionHost>,
    ui_dialog: &UiDialogMailbox,
    ext_status: &crate::status::ExtensionStatusMailbox,
    action: RuntimeActionId,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match action {
        RuntimeActionId::SendMessage => host.send_message(args).await,
        RuntimeActionId::SendUserMessage => host.send_user_message(args).await,
        RuntimeActionId::AppendEntry => host.append_entry(args).await,
        RuntimeActionId::SetSessionName => host.set_session_name(args).await,
        RuntimeActionId::GetActiveTools => host.get_active_tools(args).await,
        RuntimeActionId::SetActiveTools => host.set_active_tools(args).await,
        RuntimeActionId::SetModel => host.set_model(args).await,
        RuntimeActionId::GetThinkingLevel => host.get_thinking_level(args).await,
        RuntimeActionId::SetThinkingLevel => host.set_thinking_level(args).await,
        RuntimeActionId::Compact => host.compact(args).await,
        RuntimeActionId::GetSystemPrompt => host.get_system_prompt(args).await,
        RuntimeActionId::NewSession => host.new_session(args).await,
        RuntimeActionId::Fork => host.fork(args).await,
        RuntimeActionId::NavigateTree => host.navigate_tree(args).await,
        RuntimeActionId::SwitchSession => host.switch_session(args).await,
        RuntimeActionId::Reload => host.reload(args).await,
        RuntimeActionId::GetCliFlag => host.get_cli_flag(args).await,
        RuntimeActionId::UiDialog => ui_dialog.handle(args),
        // UI-only action: handled by the bridge (no harness call), exactly like
        // `UiDialog`. Written straight into the shared registry the TUI polls.
        RuntimeActionId::SetStatus => ext_status.handle(args),
    }
}

/// The real `runtime_action` trampoline — replaces `stub_runtime_action` when a
/// bridge is present (see [`HostApi::build_vtable`](crate::HostApi)).
///
/// Recovers `&ActionBridge` from `user_data`, parses `args_json`, drives the
/// host's async dispatch on the bridge's runtime via `Handle::spawn`, and parks
/// the plugin thread on a std `mpsc` `recv` (works from ANY thread — no ambient
/// runtime needed, which is the load-bearing property for foreign plugin
/// threads).
///
/// ## Return codes
/// - `0` — success; `*out` written with the result JSON (host-owned
///   [`StbString`]; the plugin frees it via the host `free_string` from the
///   vtable).
/// - `1` — host-level error; `*out` written with `{"error": msg}` JSON.
/// - `2` — unknown numeric action id; `*out` contains a structured error and
///   no host method is dispatched.
/// - `-1` — no bridge present (`user_data` null; should not happen when wired).
/// - `-2` — the spawned task dropped its sender without sending (runtime
///   shutdown / dispatch panic); no result available.
///
/// The whole body is `catch_unwind`-wrapped — a panic across FFI ⇒ abort (same
/// policy as the tool partial callback, `tool.rs:206`).
pub extern "C" fn trampoline_runtime_action(
    action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_action(
            action_id,
            // Highest id this ABI defines — bump when adding an action.
            u32::from(RuntimeActionId::SetStatus),
            args_json,
            out,
            user_data,
        )
    }));
    match outcome {
        Ok(rc) => rc,
        Err(_) => {
            // A panic in the host-side dispatch was caught. Do not abort the
            // process: report a host-level error to the caller (documented
            // code `1`) with a structured payload when `out` is writable.
            tracing::error!("runtime_action trampoline panicked — reporting host error");
            if !out.is_null() {
                unsafe {
                    *out = StbString::from_string(
                        r#"{"error":"runtime_action panicked"}"#.to_string(),
                    );
                }
            }
            1
        }
    }
}

/// Inner synchronous driver (split out so the `catch_unwind` wrapper is clean).
fn run_action(
    action_id: u32,
    max_action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    // Validate the raw FFI integer before dereferencing host state or entering
    // the enum-based dispatch. Unknown values are ordinary protocol errors;
    // they never become invalid Rust enum discriminants.
    let action = match RuntimeActionId::try_from(action_id) {
        Ok(action) if action_id <= max_action_id => action,
        Ok(_) | Err(_) => {
            if !out.is_null() {
                let json = serde_json::json!({
                    "error": format!("unknown runtime action id {action_id}")
                })
                .to_string();
                // SAFETY: `out` is non-null and points to the caller-provided
                // output slot. The plugin reclaims this host allocation via
                // `host_free_string` from the vtable.
                unsafe {
                    *out = StbString::from_string(json);
                }
            }
            return 2;
        }
    };

    if user_data.is_null() {
        return -1;
    }
    // SAFETY: the host guarantees `user_data` points at a live `ActionBridge`.
    // `rpi-cli` builds one `Arc<ActionBridge>` per session and keeps it for the
    // harness lifetime; `Arc::as_ptr` is pointer-stable while any clone lives.
    // We only borrow for the duration of this call.
    let bridge: &ActionBridge = unsafe { &*(user_data as *const ActionBridge) };

    // B5d staleness guard: a `/reload` that swapped in a fresh bridge calls
    // `invalidate` on the old one. A plugin that still holds the old pointer
    // (stored during the prior `register`) must not drive the swapped-out
    // session. Surface a structured "stale bridge" error so the plugin's
    // `runtime_action` returns nonzero + `{"error": ...}` instead of racing
    // the swap. (The new bridge's pointer was handed to the reloaded plugins;
    // this guard only catches the race window where an old call is still parked.)
    if !bridge.is_active() {
        if out.is_null() {
            return 1;
        }
        let json = serde_json::json!({
            "error": "runtime_action on a stale ActionBridge (session reloaded/swapped)"
        })
        .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        return 1;
    }

    // Parse args. An empty/invalid JSON blob collapses to `{}` — getters ignore
    // args; setters that require a field surface a clear error string.
    let args_str = unsafe { args_json.as_str() };
    let args: serde_json::Value = if args_str.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(args_str)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    };

    // Drive the host's async dispatch on the bridge's runtime. STD mpsc so the
    // plugin thread can recv from any thread (no ambient runtime). sync_channel
    // (1): bounded; the send completes once the value is delivered. rx.recv()
    // parks the plugin thread until the spawn finishes (or its sender drops).
    let (tx, rx) = mpsc::sync_channel::<Result<serde_json::Value, String>>(1);
    // Clone the bridge's host + reload callback into the spawned task. We pass
    // an owned `Arc<dyn RuntimeActionHost>` plus a reload-option snapshot so the
    // `dispatch` helper has everything it needs without borrowing `bridge`.
    let host = Arc::clone(&bridge.host);
    let ui_dialog = bridge.ui_dialog.clone();
    let ext_status = bridge.ext_status.clone();
    let reload_cb = bridge.reload.clone();
    if action == RuntimeActionId::SetStatus {
        // Same reasoning as `UiDialog`: this action never touches the harness,
        // so it must not need a Tokio worker. A plugin calling it from inside a
        // single-threaded runtime (a tool poll callback, say) would otherwise
        // park that executor and deadlock.
        std::thread::spawn(move || {
            let _ = tx.send(ext_status.handle(args));
        });
    } else if action == RuntimeActionId::UiDialog {
        // UiDialog is intentionally synchronous at the plugin ABI boundary:
        // `open` waits until the TUI answers. Never park that wait inside the
        // Tokio worker that services the bridge. A SEARCH/extension tool that
        // invokes `ui_request/open` from a single-thread runtime would
        // otherwise block the executor before the TUI can deliver the answer.
        std::thread::spawn(move || {
            let _ = tx.send(ui_dialog.handle(args));
        });
    } else {
        bridge.runtime.spawn(async move {
            let r = if action == RuntimeActionId::Reload {
                if let Some(cb) = reload_cb {
                    cb().await;
                    Ok(serde_json::Value::Null)
                } else {
                    host.reload(args).await
                }
            } else {
                dispatch(&host, &ui_dialog, &ext_status, action, args).await
            };
            // If the plugin thread already moved on (dropped rx), discard — a
            // send error is NOT a host fault.
            let _ = tx.send(r);
        });
    }

    let result = match rx.recv() {
        Ok(r) => r,
        Err(_) => {
            // Spawned task dropped the sender without sending: runtime shutdown
            // or the dispatch future panicked (caught inside dispatch? no —
            // dispatch is plain async, a panic would propagate to the spawn and
            // drop the sender). No result to return.
            return -2;
        }
    };

    // Write the result (or error) into `*out` as a host-owned StbString. The
    // plugin frees it via the vtable's `free_string` (= `host_free_string`).
    if out.is_null() {
        // Nothing to write to; still report the outcome via the return code.
        return match result {
            Ok(_) => 0,
            Err(_) => 1,
        };
    }

    let (rc, payload) = match result {
        Ok(value) => {
            let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
            (0, json)
        }
        Err(msg) => {
            let json = serde_json::json!({ "error": msg }).to_string();
            (1, json)
        }
    };
    // SAFETY: `out` is a valid `*mut StbString` the plugin provided for this
    // call (null checked above). `from_string` allocates a `Box<[u8]>` the host
    // owns; the plugin reclaims it via `host_free_string` (which reconstructs
    // the Box from ptr+len — matches `from_string`'s allocation, same pattern
    // used by translate.rs/tool.rs).
    unsafe {
        *out = StbString::from_string(payload);
    }
    rc
}

// ===========================================================================
// Tests — round-trip the real trampoline + dispatch + Handle::spawn + mpsc
// against a mock host. This is the B5a unit proof: plugin→host `runtime_action`
// recovers the `ActionBridge` via `user_data`, drives the host's async method on
// the runtime, parks the caller on `rx.recv()`, and writes the result JSON back
// through `*out` (freed via `host_free_string`). No cdylib needed — the trampoline
// is the same `extern "C" fn` a plugin's vtable carries.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A minimal `RuntimeActionHost` that answers `get_system_prompt` with a
    /// canned string and `reload` with Ok(null); every other action returns an
    /// "unimplemented" error. Enough to prove the dispatch switch + the async
    /// spawn + the mpsc round-trip + the StbString write.
    struct MockHost {
        prompt: String,
        saw: Mutex<Vec<RuntimeActionId>>,
    }

    #[async_trait::async_trait]
    impl RuntimeActionHost for MockHost {
        async fn send_message(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn send_user_message(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn append_entry(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_session_name(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_active_tools(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_active_tools(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_model(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_thinking_level(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_thinking_level(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn compact(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_system_prompt(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.saw
                .lock()
                .unwrap()
                .push(RuntimeActionId::GetSystemPrompt);
            Ok(serde_json::json!({ "prompt": self.prompt }))
        }
        async fn new_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn fork(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn navigate_tree(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn switch_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn reload(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            self.saw.lock().unwrap().push(RuntimeActionId::Reload);
            Ok(serde_json::Value::Null)
        }
        async fn get_cli_flag(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            self.saw.lock().unwrap().push(RuntimeActionId::GetCliFlag);
            Ok(serde_json::json!({ "value": true }))
        }
    }

    /// NOTE: these tests use `flavor = "multi_thread"`. The trampoline parks the
    /// caller on a std `mpsc::rx.recv()` (sync blocking) while the dispatch runs
    /// via `Handle::spawn` on the runtime. Under a current-thread runtime the
    /// test's own worker is the only thread that can poll the spawned task —
    /// blocking it on `recv` self-deadlocks. In the real host the caller is a
    /// plugin / `spawn_blocking` thread (never a runtime worker), so there is no
    /// deadlock; multi_thread here mirrors that (another worker runs the spawn
    /// while the test thread parks).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_round_trips_get_system_prompt() {
        let host = Arc::new(MockHost {
            prompt: "hello from host".to_string(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::new(runtime, host_dyn);
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        // Build a StbStringRef for the args. `{}` — getters ignore it.
        let args_str = "{}";
        let args_ref = StbStringRef::from_str(args_str);

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        assert_eq!(rc, 0, "success return code");

        // Read the result JSON back + reclaim the host-owned StbString.
        let json_text = out.to_string_lossy();
        let parsed: serde_json::Value = serde_json::from_str(&json_text).expect("valid json");
        assert_eq!(parsed["prompt"], "hello from host");
        crate::host_free_string(out);

        // The host saw exactly the one action. `host_for_assert` is a clone of
        // the `Arc<MockHost>` kept before it was coerced to the trait object.
        let saw = host_for_assert.saw.lock().unwrap().clone();
        assert_eq!(saw, vec![RuntimeActionId::GetSystemPrompt]);
    }

    #[tokio::test]
    async fn trampoline_null_user_data_returns_minus_one() {
        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            std::ptr::null_mut(),
        );
        assert_eq!(rc, -1, "null user_data ⇒ no bridge");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_intercepts_reload_when_bridge_has_callback() {
        // A bridge with a reload callback: the spawn site intercepts Reload and
        // runs the callback instead of the host's `reload` method. Proves the
        // bridge-level special-casing (the host impl is the fallback only).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reload_calls = Arc::new(AtomicUsize::new(0));
        let reload_calls_for_cb = Arc::clone(&reload_calls);
        let reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync> =
            Arc::new(move || {
                let c = Arc::clone(&reload_calls_for_cb);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            });
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::with_reload(runtime, host_dyn, reload);
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::Reload.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        assert_eq!(rc, 0);
        // The callback ran once; the host's `reload` did NOT.
        assert_eq!(reload_calls.load(Ordering::SeqCst), 1);
        assert!(host_for_assert.saw.lock().unwrap().is_empty());
        crate::host_free_string(out);
    }

    /// B5d: an invalidated bridge rejects `runtime_action` with a stale-bridge
    /// error instead of dispatching. A `/reload` calls `invalidate` on the old
    /// bridge; an in-flight call that still holds the old pointer must fail
    /// fast rather than drive the swapped-out session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_rejects_stale_bridge_with_invalidate() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::new(runtime, host_dyn);
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        // Invalidate (as a `/reload` would on the old bridge).
        bridge.invalidate();
        assert!(!bridge.is_active());

        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        // Nonzero (error), and the host method never ran (no dispatch).
        assert_eq!(rc, 1, "stale bridge ⇒ error return code");
        let json_text = out.to_string_lossy();
        assert!(
            json_text.contains("stale"),
            "stale-bridge error payload: {json_text}"
        );
        crate::host_free_string(out);
        assert!(
            host_for_assert.saw.lock().unwrap().is_empty(),
            "host dispatch must NOT run on a stale bridge"
        );
    }

    #[tokio::test]
    async fn trampoline_rejects_unknown_numeric_id_without_dispatch() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host_dyn);
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            0xFFFF_FFFE,
            StbStringRef::from_str("{}"),
            &mut out,
            user_data,
        );

        assert_eq!(rc, 2, "unknown action id must be a protocol error");
        let payload: serde_json::Value =
            serde_json::from_str(&out.to_string_lossy()).expect("structured error JSON");
        assert_eq!(payload["error"], "unknown runtime action id 4294967294");
        crate::host_free_string(out);
        assert!(
            host_for_assert.saw.lock().unwrap().is_empty(),
            "unknown action id must not reach host dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_trampoline_accepts_new_action() {
        let host = Arc::new(MockHost {
            prompt: "v2".to_string(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host_dyn);
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        let mut v2_out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(
                RuntimeActionId::GetCliFlag.into(),
                StbStringRef::from_str(r#"{"name":"flag"}"#),
                &mut v2_out,
                user_data,
            ),
            0
        );
        crate::host_free_string(v2_out);

        assert_eq!(
            *host_for_assert.saw.lock().unwrap(),
            vec![RuntimeActionId::GetCliFlag]
        );
    }

    /// `ReloadMailbox` + `reload_callback_from_mailbox` round-trip: signalling
    /// fires the installed receiver; an uninstalled mailbox yields `Err` (the
    /// host's "not configured" fallback). Exercises the B5d reload-signal path
    /// the TUI installs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_mailbox_signals_installed_receiver() {
        let mailbox = ReloadMailbox::new();
        // No receiver installed yet ⇒ signal fails.
        assert!(matches!(mailbox.signal(), Err(())));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        mailbox.install(tx);
        let cb = reload_callback_from_mailbox(mailbox.clone());
        cb().await;
        assert_eq!(
            rx.recv().await,
            Some(()),
            "installed receiver saw the signal"
        );
        mailbox.clear();
    }

    // ---- UiDialogMailbox (runtime action 17 / ask_user) ----

    fn open_args(id: &str, tool_call: &str) -> serde_json::Value {
        serde_json::json!({
            "op": "open",
            "requestId": id,
            "toolCallId": tool_call,
            "ui": {"kind": "selector", "questions": [
                {"id": "q1", "question": "Which target?", "options": ["Linux"]}
            ]},
        })
    }

    /// Headless hosts (nothing attached) must fail loudly rather than fabricate
    /// a user answer.
    #[test]
    fn ui_dialog_open_requires_an_attached_ui() {
        let mailbox = UiDialogMailbox::new();
        let error = mailbox.handle(open_args("r1", "c1")).unwrap_err();
        assert!(error.contains("interactive UI"), "got: {error}");
    }

    #[test]
    fn ui_dialog_open_poll_answer_round_trip() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();

        let opened = mailbox.handle(open_args("r1", "c1")).unwrap();
        assert_eq!(opened["status"], "pending");
        assert_eq!(opened["toolCallId"], "c1");

        let request = mailbox.take_pending().expect("request visible to TUI");
        assert_eq!(request.request_id, "r1");
        assert_eq!(request.tool_call_id.as_deref(), Some("c1"));
        // A pending poll is non-destructive and remains available.
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "pending");
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "pending");

        mailbox
            .respond("r1", serde_json::json!("7890"))
            .expect("respond");
        let answered = mailbox.poll("r1").unwrap();
        assert_eq!(answered["status"], "answered");
        assert_eq!(answered["answer"], "7890");
        // Polling consumes the terminal entry.
        assert!(mailbox.poll("r1").is_err());
    }

    #[test]
    fn ui_dialog_cancel_is_reported_once() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.cancel("r1").unwrap();
        let cancelled = mailbox.poll("r1").unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert!(mailbox.poll("r1").is_err());
    }

    #[test]
    fn ui_dialog_concurrent_requests_do_not_cross_answers() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.handle(open_args("r2", "c2")).unwrap();

        let first = mailbox.take_pending().unwrap().request_id;
        let second = mailbox.take_pending().unwrap().request_id;
        assert_ne!(first, second);

        // Answer the SECOND request first; the first must stay pending.
        mailbox.respond(&second, serde_json::json!("second")).unwrap();
        assert_eq!(mailbox.poll(&first).unwrap()["status"], "pending");
        let second_answer = mailbox.poll(&second).unwrap();
        assert_eq!(second_answer["answer"], "second");

        mailbox.respond(&first, serde_json::json!("first")).unwrap();
        assert_eq!(mailbox.poll(&first).unwrap()["answer"], "first");
    }

    #[test]
    fn ui_dialog_detach_cancels_outstanding() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.detach();
        assert!(!mailbox.is_attached());
        assert_eq!(mailbox.pending_len(), 0);
        // Re-opening without a consumer is an explicit error again.
        assert!(mailbox.handle(open_args("r2", "c2")).is_err());
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "cancelled");
    }

    #[test]
    fn ui_dialog_duplicate_open_does_not_reenqueue() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        let again = mailbox.handle(open_args("r1", "c1")).unwrap();
        assert_eq!(again["status"], "pending");
        assert_eq!(mailbox.pending_len(), 1);
        mailbox.take_pending().unwrap();
        assert!(mailbox.take_pending().is_none());
    }

    /// The v2 trampoline must route action 17 into the session mailbox rather
    /// than rejecting it as unknown, and a detached mailbox must surface the
    /// explicit "requires an interactive UI" error (headless contract).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dispatch_routes_ui_dialog_action_17() {
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let bridge = ActionBridge::with_ui_dialog(
            tokio::runtime::Handle::current(),
            host,
            UiDialogMailbox::new(),
        );
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            17,
            StbStringRef::from_str(r#"{"op":"open","requestId":"r1","ui":{}}"#),
            &mut out,
            user_data,
        );
        assert_eq!(rc, 1);
        let payload: serde_json::Value =
            serde_json::from_str(&out.to_string_lossy()).expect("structured error JSON");
        assert!(
            payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("interactive UI")),
            "unexpected payload: {payload}"
        );
        crate::host_free_string(out);

        // A genuinely unknown id is still rejected (no silent acceptance).
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(19, StbStringRef::from_str("{}"), &mut out, user_data),
            2
        );
        crate::host_free_string(out);
    }

    /// Action 18 (`SetStatus`) is bridge-local, like `UiDialog`: it writes the
    /// shared registry the TUI polls and never touches the harness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dispatch_routes_set_status_action_18() {
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let status = crate::status::ExtensionStatusMailbox::new();
        let bridge = ActionBridge::with_reload_and_ui(
            tokio::runtime::Handle::current(),
            host,
            Arc::new(|| Box::pin(async {})),
            UiDialogMailbox::new(),
            status.clone(),
        );
        let user_data = Arc::as_ptr(&bridge) as *mut c_void;

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            18,
            StbStringRef::from_str(r#"{"key":"langfuse","value":"langfuse ✓ (trace sent)"}"#),
            &mut out,
            user_data,
        );
        assert_eq!(rc, 0);
        assert_eq!(status.text(), "langfuse ✓ (trace sent)");
        crate::host_free_string(out);

        // Clearing through the same action empties the line again.
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(
                18,
                StbStringRef::from_str(r#"{"key":"langfuse","value":""}"#),
                &mut out,
                user_data
            ),
            0
        );
        assert_eq!(status.text(), "");
        crate::host_free_string(out);

        // A missing key is a host-level error (rc=1), not an unknown action.
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(18, StbStringRef::from_str("{}"), &mut out, user_data),
            1
        );
        crate::host_free_string(out);
    }
}
