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
use rpi_ai::types::Tool;
use rpi_plugin_sdk::{
    CommandHandlerFn, EventHandlerFn, EventTag, FreeStringFn, ProviderRequestFn, RenderFn,
    ResourcesDiscoverFn, EVENT_TAG_COUNT,
};

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

/// A registered slash command and its plugin callback.
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub handler: CommandHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}

// SAFETY: the plugin owns the callback/context and promises they remain valid
// for the loaded library lifetime, matching the other registered callbacks.
unsafe impl Send for RegisteredCommand {}
unsafe impl Sync for RegisteredCommand {}

/// A CLI flag declared by a native extension. Values are supplied by the
/// host's parsed `Args::unknown_flags` map and read by the plugin through the
/// `RuntimeActionId::GetCliFlag` action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredFlag {
    pub name: String,
    pub description: String,
}

/// A registered `on(tag)` event handler. `user_data` is the plugin's opaque
/// context, passed back unchanged on every dispatch. `plugin` is the display
/// name of the owning extension (host-side only — stamped from the cdylib path
/// at registration; not part of the ABI), used for lifecycle veto diagnostics
/// ("extension `foo` aborted `before_tui_start`").
///
/// `priority`/`platforms` are the owning plugin's **declaration** (P2): the
/// snapshot sorts handlers by `priority` (smaller first, stable), and dispatch
/// skips a handler whose non-empty `platforms` excludes the host platform.
/// Both default to `100` / empty (all platforms); a plugin sets them via the
/// ABI v3 `declare` ext block.
///
/// SAFETY contract: the plugin guarantees `handler` is safe to call from any
/// thread (the host dispatches from the async emitter thread) and `user_data`
/// is valid for the registry's lifetime. The host never frees `user_data`
/// (plugin-owned).
#[derive(Clone)]
pub struct RegisteredHandler {
    /// Display name of the owning extension (from the cdylib file stem).
    pub plugin: String,
    /// Dispatch order (smaller runs first). Default `100`.
    pub priority: i32,
    /// Platforms this handler's plugin supports (empty = all). Matched against
    /// [`current_platform`].
    pub platforms: Vec<String>,
    pub handler: EventHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers are Send+Sync; `user_data` is an opaque plugin pointer the
// plugin warrants is thread-safe to pass to `handler` from any thread. The host
// only reads it through `handler`.
unsafe impl Send for RegisteredHandler {}
unsafe impl Sync for RegisteredHandler {}

/// A registered `resources_discover` handler (B5b). The `out` [`StbString`] the
/// handler produces is **plugin-owned**, so the host reclaims it via the
/// plugin's own `plugin_free_string` traveled alongside. `user_data` is the
/// plugin's opaque context. SAFETY: same as [`RegisteredHandler`] — the plugin
/// warrants `handler` is callable from any thread and `user_data` is valid for
/// the registry's lifetime; the host never frees `user_data`.
#[derive(Clone, Copy)]
pub struct ResourcesDiscoverHandler {
    pub handler: ResourcesDiscoverFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers + an opaque plugin pointer the plugin warrants is
// thread-safe; the host only reads `user_data` through `handler`.
unsafe impl Send for ResourcesDiscoverHandler {}
unsafe impl Sync for ResourcesDiscoverHandler {}

/// A registered custom provider (B5c). The host wraps `request_fn` in a
/// [`PluggableProvider`](crate::PluggableProvider) impl of `rpi_ai::Provider`;
/// its `stream_simple` drives `request_fn` on `spawn_blocking` (the sync fn
/// can't own a chunked stream), reads the **plugin-owned** `out` JSON (a full
/// assistant message), reclaims it via `plugin_free_string`, parses it to an
/// [`AssistantMessage`](rpi_ai::types::AssistantMessage), and emits it as one
/// terminal `Done` chunk (v1 one-shot, documented divergence from pi's async
/// streaming). `provider_id`/`base_url`/`api_style` carry the provider's
/// identity (copied from the borrowed `StbStringRef`s at registration); the fn
/// pointers + `user_data` live as long as the plugin (keepalive-mapped).
/// `user_data` is the plugin's opaque context, passed back on every
/// `request_fn` call.
///
/// SAFETY: the plugin warrants `request_fn` is callable from any thread (the
/// host calls it from a `spawn_blocking` pool thread) and `user_data` is valid
/// for the plugin's lifetime; the host never frees `user_data`.
#[derive(Clone)]
pub struct RegisteredProvider {
    pub provider_id: String,
    pub base_url: String,
    pub api_style: String,
    pub request_fn: ProviderRequestFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers + owned `String`s + an opaque plugin pointer the plugin
// warrants is thread-safe; the host never frees `user_data`.
unsafe impl Send for RegisteredProvider {}
unsafe impl Sync for RegisteredProvider {}

/// A registered message/markdown/entry renderer (B5c). The interactive TUI
/// consumes all three kinds through the JSON component adapter. `render_fn` produces a
/// **plugin-owned** `out` [`StbString`] the host reclaims via
/// `plugin_free_string`; `user_data` is passed back on every render call. `name`
/// is copied from the borrowed `StbStringRef` at registration.
///
/// SAFETY: same as [`RegisteredProvider`].
#[derive(Clone)]
pub struct RegisteredRenderer {
    pub name: String,
    pub kind: RegisteredRendererKind,
    pub render_fn: RenderFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}

/// Which render path this renderer targets — mirrors the three distinct
/// `register_*` slots (`register_message_renderer` /
/// `register_markdown_transformer` / `register_entry_renderer`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RegisteredRendererKind {
    Message,
    Markdown,
    Entry,
}
// SAFETY: same as [`RegisteredProvider`] — fn pointers + owned `String` + an
// opaque plugin pointer the plugin warrants is thread-safe.
unsafe impl Send for RegisteredRenderer {}
unsafe impl Sync for RegisteredRenderer {}

/// One flat registration record, for iteration/diagnostics. Built on demand
/// from the typed vecs in [`RegistrySnapshot`].
#[derive(Clone)]
pub enum RegistryEntry {
    Tool(ExtensionTool),
    Command(RegisteredCommand),
    Flag(RegisteredFlag),
}

// ---------------------------------------------------------------------------
// P2 — plugin declarations (priority + platforms)
// ---------------------------------------------------------------------------

/// Default dispatch priority for a plugin that declares none. Handlers run
/// smallest-priority-first; ties break by registration order.
pub const DEFAULT_PRIORITY: i32 = 100;

/// The host platform, as matched against a plugin's declared `platforms`.
/// Mirrors the lifescope `Platform` set.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "linux") {
        // Termux reports `target_os = "android"` above; a plain Linux is here.
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// Whether a handler with the given declared `platforms` should run on this
/// host. Empty `platforms` = all platforms; a non-empty list requires the host
/// platform to be present (`"all"` is accepted as a wildcard).
pub fn platform_allows(platforms: &[String]) -> bool {
    if platforms.is_empty() {
        return true;
    }
    let here = current_platform();
    platforms
        .iter()
        .any(|p| p.eq_ignore_ascii_case("all") || p.eq_ignore_ascii_case(here))
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
    flags: Vec<RegisteredFlag>,
    /// `handlers[tag as usize]` — all handlers subscribed to that tag.
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    /// `resources_discover` handlers (B5b). Fan-out on discovery, in registration
    /// order. Unlike event handlers (which key off a tag in a fixed array), these
    /// are a single flat list — `resources_discover` has its own out-param
    /// signature and is never dispatched through the fire-and-forget event path.
    resources_discover: Vec<ResourcesDiscoverHandler>,
    /// Registered custom providers (B5c). The host wraps each in a
    /// [`PluggableProvider`](crate::PluggableProvider). First-registration-wins on
    /// `provider_id`.
    providers: Vec<RegisteredProvider>,
    /// Registered renderers (B5c), split by kind at registration. First-wins on
    /// `(kind, name)`. The host records these now; TUI consumption is B5e.
    renderers: Vec<RegisteredRenderer>,
    /// P2: the registering plugin's declared dispatch priority. Stamped onto
    /// every handler it registers; a later `declare` also retro-updates them.
    priority: i32,
    /// P2: the registering plugin's declared platforms (empty = all).
    platforms: Vec<String>,
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
        let handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT] =
            std::array::from_fn(|_| Vec::new());
        Self {
            tools: Vec::new(),
            commands: Vec::new(),
            flags: Vec::new(),
            handlers,
            resources_discover: Vec::new(),
            providers: Vec::new(),
            renderers: Vec::new(),
            priority: DEFAULT_PRIORITY,
            platforms: Vec::new(),
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
    pub fn register_command(
        &mut self,
        name: String,
        description: String,
        handler: CommandHandlerFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        if self.commands.iter().any(|c| c.name == name) {
            return true;
        }
        self.commands.push(RegisteredCommand {
            name,
            description,
            handler,
            user_data,
        });
        false
    }

    /// Register a CLI flag (first-wins by name). Returns `true` when an
    /// earlier extension already declared the same flag.
    pub fn register_flag(&mut self, name: String, description: String) -> bool {
        if self.flags.iter().any(|flag| flag.name == name) {
            return true;
        }
        self.flags.push(RegisteredFlag { name, description });
        false
    }

    /// Subscribe a handler to `tag`. Multiple handlers per tag are kept
    /// (fan-out on dispatch). Always inserts; returns `false`. `plugin` is the
    /// owning extension's display name (host-side only, for diagnostics).
    pub fn register_event_handler(
        &mut self,
        plugin: String,
        tag: EventTag,
        handler: EventHandlerFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        let idx = tag as usize;
        if idx < EVENT_TAG_COUNT {
            // Stamp the registering plugin's current declaration (P2). A plugin
            // typically declares BEFORE registering handlers; `apply_declaration`
            // also retro-updates any handler registered before the declare call.
            self.handlers[idx].push(RegisteredHandler {
                plugin,
                priority: self.priority,
                platforms: self.platforms.clone(),
                handler,
                user_data,
            });
        }
        false
    }

    /// Apply a plugin's declaration (P2) parsed from the ABI v3 `declare` JSON
    /// (`{"priority":60,"platforms":["linux","macos"]}`). Unknown keys are
    /// ignored; malformed values leave the corresponding field unchanged.
    /// Retro-updates any handlers already registered (so declare order doesn't
    /// matter). Returns the number of fields applied.
    pub fn apply_declaration(
        &mut self,
        priority: Option<i32>,
        platforms: Option<Vec<String>>,
    ) -> usize {
        let mut applied = 0;
        if let Some(priority) = priority {
            self.priority = priority;
            for tag_handlers in self.handlers.iter_mut() {
                for h in tag_handlers.iter_mut() {
                    h.priority = priority;
                }
            }
            applied += 1;
        }
        if let Some(platforms) = platforms {
            self.platforms = platforms.clone();
            for tag_handlers in self.handlers.iter_mut() {
                for h in tag_handlers.iter_mut() {
                    h.platforms = platforms.clone();
                }
            }
            applied += 1;
        }
        applied
    }

    /// Parse a v3 `declare` JSON payload and apply it (P2). Recognised keys:
    /// `priority` (integer) and `platforms` (array of strings). Returns `true`
    /// if the payload parsed as JSON (even with no recognised keys), `false` on
    /// a parse error.
    pub fn apply_declaration_json(&mut self, json: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return false;
        };
        let priority = value
            .get("priority")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let platforms = value
            .get("platforms")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            });
        self.apply_declaration(priority, platforms);
        true
    }

    /// Register a `resources_discover` handler (B5b). Multiple handlers are kept
    /// (fan-out on discovery, registration order). Always inserts; returns `false`.
    pub fn register_resources_discover(
        &mut self,
        handler: ResourcesDiscoverFn,
        plugin_free_string: FreeStringFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        self.resources_discover.push(ResourcesDiscoverHandler {
            handler,
            plugin_free_string,
            user_data,
        });
        false
    }

    /// Register a custom provider (B5c). First-registration-wins on `provider_id`
    /// (a later registration for an existing id is dropped, mirroring tool/command
    /// first-wins). Returns `true` if a prior provider of the same id was kept.
    pub fn register_provider(&mut self, provider: RegisteredProvider) -> bool {
        if self
            .providers
            .iter()
            .any(|p| p.provider_id == provider.provider_id)
        {
            return true;
        }
        self.providers.push(provider);
        false
    }

    /// Register a renderer (B5c — message/markdown/entry). First-wins on
    /// `(kind, name)`. Returns `true` if a prior renderer of the same kind+name
    /// was kept.
    pub fn register_renderer(&mut self, renderer: RegisteredRenderer) -> bool {
        if self
            .renderers
            .iter()
            .any(|r| r.kind == renderer.kind && r.name == renderer.name)
        {
            return true;
        }
        self.renderers.push(renderer);
        false
    }

    /// Build a snapshot the host session keeps. The registry's `active` flag is
    /// shared (Arc) so a later `invalidate` on the registry also invalidates the
    /// snapshot — important for cross-session staleness.
    pub fn snapshot(&self) -> RegistrySnapshot {
        // P2: dispatch order is priority-first (smaller first). `sort_by_key` is
        // stable, so equal priorities keep registration order (the prior
        // contract). Sorting per tag here means every snapshot — including the
        // merged session snapshot built via `absorb` — is pre-ordered.
        let mut handlers = self.handlers.clone();
        for tag_handlers in handlers.iter_mut() {
            tag_handlers.sort_by_key(|h| h.priority);
        }
        RegistrySnapshot {
            tools: self
                .tools
                .iter()
                .map(|t| ExtensionTool {
                    tool: t.tool.clone(),
                    handle: t.handle,
                })
                .collect(),
            commands: self.commands.clone(),
            flags: self.flags.clone(),
            handlers,
            resources_discover: self.resources_discover.clone(),
            providers: self.providers.clone(),
            renderers: self.renderers.clone(),
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

    /// P2: the plugin's declared dispatch priority (default [`DEFAULT_PRIORITY`]).
    pub fn declared_priority(&self) -> i32 {
        self.priority
    }

    /// P2: the plugin's declared platforms (empty = all).
    pub fn declared_platforms(&self) -> &[String] {
        &self.platforms
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
        for flag in other.flags.drain(..) {
            if self.flags.iter().any(|existing| existing.name == flag.name) {
                continue;
            }
            self.flags.push(flag);
        }
        for (tag_idx, handlers) in other.handlers.iter_mut().enumerate() {
            self.handlers[tag_idx].append(handlers);
        }
        self.resources_discover
            .append(&mut other.resources_discover);
        for p in other.providers.drain(..) {
            if self
                .providers
                .iter()
                .any(|x| x.provider_id == p.provider_id)
            {
                continue;
            }
            self.providers.push(p);
        }
        for r in other.renderers.drain(..) {
            if self
                .renderers
                .iter()
                .any(|x| x.kind == r.kind && x.name == r.name)
            {
                continue;
            }
            self.renderers.push(r);
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
    flags: Vec<RegisteredFlag>,
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    resources_discover: Vec<ResourcesDiscoverHandler>,
    providers: Vec<RegisteredProvider>,
    renderers: Vec<RegisteredRenderer>,
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

    /// The registered CLI flags, insertion order.
    pub fn flags(&self) -> &[RegisteredFlag] {
        &self.flags
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

    /// The `resources_discover` handlers, registration order (B5b). Empty when
    /// no plugin registered a discovery handler.
    pub fn resources_discover(&self) -> &[ResourcesDiscoverHandler] {
        &self.resources_discover
    }

    /// The registered custom providers (B5c), registration order. The host wraps
    /// each in a [`PluggableProvider`](crate::PluggableProvider). Empty when no
    /// plugin registered a provider.
    pub fn providers(&self) -> &[RegisteredProvider] {
        &self.providers
    }

    /// All registered renderers (B5c), registration order, across all three
    /// kinds.
    pub fn renderers(&self) -> &[RegisteredRenderer] {
        &self.renderers
    }

    /// The registered renderers of a specific kind (B5c). The TUI dispatches
    /// message and entry renderers through the same JSON adapter used by the
    /// markdown transformer.
    pub fn renderers_of(&self, kind: RegisteredRendererKind) -> Vec<RegisteredRenderer> {
        self.renderers
            .iter()
            .filter(|r| r.kind == kind)
            .cloned()
            .collect()
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
        for flag in &self.flags {
            v.push(RegistryEntry::Flag(flag.clone()));
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
#[allow(dead_code)]
pub(crate) fn stale_error() -> AgentError {
    AgentError::State("extensions registry is stale (session swapped/reloaded)".into())
}

// Suppress unused-fn warning until B3 wires dispatch through the snapshot.
#[allow(dead_code)]
fn _ensure_handler_accessor_used(snap: &RegistrySnapshot) {
    let _ = snap.handlers_for(EventTag::MessageEnd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_flags_snapshot_and_merge_first_wins() {
        let mut first = ExtensionRegistry::new();
        assert!(!first.register_flag("server".into(), "Start server".into()));
        assert!(first.register_flag("server".into(), "Different description".into()));

        let mut second = ExtensionRegistry::new();
        assert!(!second.register_flag("server".into(), "Second server".into()));
        assert!(!second.register_flag("port".into(), "Listen port".into()));

        first.absorb(second);
        let snapshot = first.snapshot();
        let flags = snapshot.flags();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].name, "server");
        assert_eq!(flags[0].description, "Start server");
        assert_eq!(flags[1].name, "port");
    }

    extern "C" fn noop_handler(
        _ev: rpi_plugin_sdk::StablePluginEvent,
        _ud: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    fn handler_of<'a>(
        snap: &'a RegistrySnapshot,
        tag: EventTag,
        i: usize,
    ) -> &'a RegisteredHandler {
        &snap.handlers_for(tag)[i]
    }

    /// Two plugins declaring different priorities: the merged snapshot orders
    /// the lower-priority plugin's handler first (smaller runs first).
    #[test]
    fn priority_orders_handlers_after_merge() {
        let tag = EventTag::SessionStart;

        let mut late = ExtensionRegistry::new();
        late.apply_declaration(Some(200), None);
        late.register_event_handler("late".into(), tag, noop_handler, std::ptr::null_mut());

        let mut early = ExtensionRegistry::new();
        early.apply_declaration(Some(50), None);
        early.register_event_handler("early".into(), tag, noop_handler, std::ptr::null_mut());

        // Absorb in the "wrong" order — snapshot must still sort by priority.
        let mut session = ExtensionRegistry::new();
        session.absorb(late);
        session.absorb(early);
        let snap = session.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).plugin, "early");
        assert_eq!(handler_of(&snap, tag, 1).plugin, "late");
    }

    /// Equal priorities keep registration order (stable sort).
    #[test]
    fn equal_priority_is_stable() {
        let tag = EventTag::TurnEnd;
        let mut reg = ExtensionRegistry::new();
        reg.register_event_handler("a".into(), tag, noop_handler, std::ptr::null_mut());
        reg.register_event_handler("b".into(), tag, noop_handler, std::ptr::null_mut());
        let snap = reg.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).plugin, "a");
        assert_eq!(handler_of(&snap, tag, 1).plugin, "b");
    }

    /// A declaration applied AFTER handlers exist retro-updates them (declare
    /// order does not matter).
    #[test]
    fn declaration_retro_updates_existing_handlers() {
        let tag = EventTag::SessionStart;
        let mut reg = ExtensionRegistry::new();
        reg.register_event_handler("p".into(), tag, noop_handler, std::ptr::null_mut());
        assert_eq!(
            handler_of(&reg.snapshot(), tag, 0).priority,
            DEFAULT_PRIORITY
        );

        assert!(reg.apply_declaration_json(r#"{"priority":7,"platforms":["linux"]}"#));
        let snap = reg.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).priority, 7);
        assert_eq!(
            handler_of(&snap, tag, 0).platforms,
            vec!["linux".to_string()]
        );
    }

    /// Malformed declaration JSON is rejected (returns `false`); recognised keys
    /// apply.
    #[test]
    fn apply_declaration_json_parses_and_rejects() {
        let mut reg = ExtensionRegistry::new();
        assert!(!reg.apply_declaration_json("not json"));
        assert!(reg.apply_declaration_json("{}"));
        assert!(reg.apply_declaration_json(r#"{"priority":3}"#));
        assert_eq!(reg.apply_declaration(Some(3), None), 1);
    }

    #[test]
    fn platform_allows_matches_host_and_wildcard() {
        assert!(platform_allows(&[]));
        assert!(platform_allows(&["all".to_string()]));
        assert!(platform_allows(&["ALL".to_string()]));
        assert!(platform_allows(&[current_platform().to_string()]));
        assert!(!platform_allows(&["nonexistent-os".to_string()]));
        // Empty host subset is fine; a non-matching list is not.
        assert!(!platform_allows(&["definitely-not-this-host".to_string()]));
    }
}
