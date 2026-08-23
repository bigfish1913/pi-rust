//! The extension registry — accumulates registrations during a plugin's
//! `rpi_plugin_register` call, then snapshots for the host session.
//!
//! `HostApi` (in [`crate`]) holds an `ExtensionRegistry` behind a `Mutex` while
//! the plugin's register trampolines run; after register completes the host
//! [`take`]s it and builds a [`RegistrySnapshot`] the session keeps for its
//! lifetime. Staleness across a session swap / `/reload` is guarded by a shared
//! `Arc<AtomicBool>` "active" flag ([`assert_active`]).
//!
//! [`take`]: crate::HostApi::take_registry

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rpi_agent::error::AgentError;
use rpi_plugin_sdk::{EventTag, EventHandlerFn, EVENT_TAG_COUNT};
use rpi_ai::types::Tool;

use crate::tool::PluginToolHandle;

// ---------------------------------------------------------------------------
// Registered-item records
// ---------------------------------------------------------------------------

/// A tool an extension registered: the provider-facing [`Tool`] schema + the
/// plugin's 4-function handle ([`PluginToolHandle`]) the host drives via
/// [`PluginToolAdapter`](crate::PluginToolAdapter).
///
/// `tool` is public so the host can merge schemas; `handle` is crate-private
/// (only [`PluginToolAdapter`](crate::PluginToolAdapter) drives it). `Clone`
/// derives because [`Tool`] is `Clone` and [`PluginToolHandle`] is `Copy`.
#[derive(Clone)]
pub struct ExtensionTool {
    /// The provider-facing tool definition (name/description/parameters).
    pub tool: Tool,
    pub(crate) handle: PluginToolHandle,
}

impl ExtensionTool {
    pub fn new(tool: Tool, handle: PluginToolHandle) -> Self {
        Self { tool, handle }
    }

    /// The plugin-side function handles backing this tool. `Copy` (fn pointers
    /// + a `FreeStringFn`), so handing it out is free. Public so the host
    /// (e.g. rpi-cli's session builder) can wrap a registered tool in a
    /// [`PluginToolAdapter`](crate::PluginToolAdapter) outside this crate.
    pub fn handle(&self) -> PluginToolHandle {
        self.handle
    }
}

/// A registered slash command (name + description). The handler fn is not yet
/// wired on the host (B5) — the registry records the metadata only.
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
}

/// A registered `on(tag)` event handler. `user_data` is the plugin's opaque
/// context, passed back unchanged on every dispatch.
///
/// SAFETY contract: the plugin guarantees `handler` is safe to call from any
/// thread (the host dispatches from the async emitter thread) and `user_data`
/// is valid for the registry's lifetime. The host never frees `user_data`
/// (plugin-owned).
#[derive(Clone, Copy)]
pub struct RegisteredHandler {
    pub handler: EventHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers are Send+Sync; `user_data` is an opaque plugin pointer the
// plugin warrants is thread-safe to pass to `handler` from any thread. The host
// only reads it through `handler`.
unsafe impl Send for RegisteredHandler {}
unsafe impl Sync for RegisteredHandler {}

/// One flat registration record, for iteration/diagnostics. Built on demand
/// from the typed vecs in [`RegistrySnapshot`].
#[derive(Clone)]
pub enum RegistryEntry {
    Tool(ExtensionTool),
    Command(RegisteredCommand),
}

// ---------------------------------------------------------------------------
// ExtensionRegistry — accumulated during register, then snapshotted
// ---------------------------------------------------------------------------

/// Accumulates registrations from one or more plugins' `rpi_plugin_register`
/// calls. Held by [`HostApi`](crate::HostApi) during register; the host then
/// [`take_registry`](crate::HostApi::take_registry) and builds a snapshot.
///
/// Merge semantics mirror pi: **first-registration-wins** on tool/command name
/// collision (a later registration for an existing name is dropped, not an
/// overwrite). Event handlers accumulate (multiple per tag — fan-out).
pub struct ExtensionRegistry {
    tools: Vec<ExtensionTool>,
    commands: Vec<RegisteredCommand>,
    /// `handlers[tag as usize]` — all handlers subscribed to that tag.
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    /// Shared staleness flag. `true` while the session owning this registry is
    /// active; set `false` on swap/`/reload`. Tool/event dispatch checks it.
    active: Arc<AtomicBool>,
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRegistry {
    /// Build an empty, active registry.
    pub fn new() -> Self {
        // `EVENT_TAG_COUNT` is 33; construct the array via `[const { Vec::new() };
        // N]` (stable since 1.63).
        let handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT] =
            [const { Vec::new() }; EVENT_TAG_COUNT];
        Self {
            tools: Vec::new(),
            commands: Vec::new(),
            handlers,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Register a tool. Returns `true` if a **prior** tool of the same name was
    /// kept (first-wins: the new one is dropped). Returns `false` on insert.
    pub fn register_tool(&mut self, tool: Tool, handle: PluginToolHandle) -> bool {
        if self.tools.iter().any(|t| t.tool.name == tool.name) {
            // First-wins: keep the prior, drop the new handle. We must destroy
            // the dropped handle's allocation? No — `handle` is just fn pointers
            // + a FreeStringFn (all Copy, no allocation). Dropping it is a no-op.
            // A real per-tool plugin allocation only exists after `execute`
            // produces a StepHandle; we never call execute for a dropped
            // registration, so nothing to free.
            return true;
        }
        self.tools.push(ExtensionTool::new(tool, handle));
        false
    }

    /// Register a slash command (first-wins by name). `true` if a prior was kept.
    pub fn register_command(&mut self, name: String, description: String) -> bool {
        if self.commands.iter().any(|c| c.name == name) {
            return true;
        }
        self.commands.push(RegisteredCommand { name, description });
        false
    }

    /// Subscribe a handler to `tag`. Multiple handlers per tag are kept
    /// (fan-out on dispatch). Always inserts; returns `false`.
    pub fn register_event_handler(
        &mut self,
        tag: EventTag,
        handler: EventHandlerFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        let idx = tag as usize;
        if idx < EVENT_TAG_COUNT {
            self.handlers[idx].push(RegisteredHandler { handler, user_data });
        }
        false
    }

    /// Build a snapshot the host session keeps. The registry's `active` flag is
    /// shared (Arc) so a later `invalidate` on the registry also invalidates the
    /// snapshot — important for cross-session staleness.
    pub fn snapshot(&self) -> RegistrySnapshot {
        RegistrySnapshot {
            tools: self.tools.iter().map(|t| ExtensionTool {
                tool: t.tool.clone(),
                handle: t.handle,
            }).collect(),
            commands: self.commands.clone(),
            handlers: self.handlers.clone(),
            active: Arc::clone(&self.active),
        }
    }

    /// Mark this registry (and any snapshot sharing its flag) as stale.
    pub fn invalidate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Whether this registry is still active (not invalidated).
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Absorb another registry's registrations into this one, first-wins on
    /// name (tools/commands) and appending (event handlers). Used by
    /// [`load_dir`](crate::loader::load_dir)/`merge_registries` to fold
    /// per-plugin registries into one session registry in load order. Consumes
    /// `other` (its `active` flag is discarded — the session registry's flag
    /// wins, since it owns session lifetime).
    pub(crate) fn absorb(&mut self, mut other: ExtensionRegistry) {
        for t in other.tools.drain(..) {
            if self.tools.iter().any(|x| x.tool.name == t.tool.name) {
                // first-wins: keep the prior, discard the new. Nothing to free
                // (handle is fn pointers). Emit no diagnostic here — the loader
                // may surface collisions if desired; v1 silent first-wins.
                continue;
            }
            self.tools.push(t);
        }
        for c in other.commands.drain(..) {
            if self.commands.iter().any(|x| x.name == c.name) {
                continue;
            }
            self.commands.push(c);
        }
        for (tag_idx, handlers) in other.handlers.iter_mut().enumerate() {
            self.handlers[tag_idx].append(handlers);
        }
    }
}

// ---------------------------------------------------------------------------
// RegistrySnapshot — the host session's immutable view
// ---------------------------------------------------------------------------

/// An immutable snapshot of an [`ExtensionRegistry`] the host session keeps for
/// its lifetime. Tools are wrapped in [`ExtensionTool`] so the host can build
/// [`PluginToolAdapter`](crate::PluginToolAdapter)s; event handlers are grouped
/// by tag for the [`ExtensionEmitter`](crate::ExtensionEmitter) to fan out.
///
/// The `active` flag is shared with the source registry, so invalidating the
/// registry (on session swap) invalidates this snapshot too.
pub struct RegistrySnapshot {
    tools: Vec<ExtensionTool>,
    commands: Vec<RegisteredCommand>,
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    active: Arc<AtomicBool>,
}

impl RegistrySnapshot {
    /// The extension tools (schema + handle), insertion order.
    pub fn tools(&self) -> &[ExtensionTool] {
        &self.tools
    }

    /// Consume the snapshot into the owned tool list (for building adapters).
    pub fn into_tools(self) -> Vec<ExtensionTool> {
        self.tools
    }

    /// The registered slash commands.
    pub fn commands(&self) -> &[RegisteredCommand] {
        &self.commands
    }

    /// Handlers subscribed to `tag` (empty slice if none).
    pub fn handlers_for(&self, tag: EventTag) -> &[RegisteredHandler] {
        let idx = tag as usize;
        if idx < EVENT_TAG_COUNT {
            &self.handlers[idx]
        } else {
            &[]
        }
    }

    /// A flat iterator of all registrations (tools + commands), for diagnostics.
    pub fn entries(&self) -> Vec<RegistryEntry> {
        let mut v: Vec<RegistryEntry> = Vec::new();
        for t in &self.tools {
            v.push(RegistryEntry::Tool(ExtensionTool {
                tool: t.tool.clone(),
                handle: t.handle,
            }));
        }
        for c in &self.commands {
            v.push(RegistryEntry::Command(c.clone()));
        }
        v
    }

    /// Whether this snapshot's session is still active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// The shared active flag (for [`assert_active`]).
    pub fn active_flag(&self) -> &Arc<AtomicBool> {
        &self.active
    }
}

/// Staleness guard: `true` while the owning session is still active. Called
/// before dispatching an extension event or driving an extension tool so a
/// stale registry (from a swapped-out session) can't act. Mirrors pi's
/// `ExtensionRuntimeState` active check.
pub fn assert_active(active: &Arc<AtomicBool>) -> bool {
    active.load(Ordering::SeqCst)
}

/// Build an `AgentError` for a stale-registry access.
pub(crate) fn stale_error() -> AgentError {
    AgentError::State("extensions registry is stale (session swapped/reloaded)".into())
}

// Suppress unused-fn warning until B3 wires dispatch through the snapshot.
#[allow(dead_code)]
fn _ensure_handler_accessor_used(snap: &RegistrySnapshot) {
    let _ = snap.handlers_for(EventTag::MessageEnd);
}
