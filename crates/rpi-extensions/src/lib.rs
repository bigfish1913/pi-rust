//! Rust-native (cdylib) plugin loader + `AgentTool` adapter for rpi.
//!
//! This is Part B1 of the extension-alignment plan. `rpi` loads extensions as
//! **compiled Rust cdylibs** (`.dll`/`.so`/`.dylib`) via `libloading` — **not**
//! TS/jiti — because we control both sides and the plugin is Rust
//! (`动态加载可以加载rs 的代码 没必要是 ts`). The ABI contract lives in
//! [`rpi_plugin_sdk`]; this crate is the **host side** that loads plugins and
//! bridges their tools/events into rpi's native async types.
//!
//! ## Crate DAG position
//!
//! Depends on **`rpi-plugin-sdk` + `rpi-ai` + `rpi-agent` only** (NOT
//! `rpi-harness`). The harness consumes this crate's adapters as trait objects
//! via `AgentHarnessOptions` injection, so `rpi-harness` never imports
//! `rpi-extensions` — cycle-free (verified by adversarial review of the first
//! draft, which wrongly added a `rpi-harness` dep).
//!
//! ## The async-across-ABI bridge (load-bearing — corrected vs first draft)
//!
//! A plugin tool drives an execution through **four** plugin-exported fns
//! (`execute`→handle, `poll`, `cancel`, `destroy`) — see
//! [`rpi_plugin_sdk::ToolExecuteFn`] etc. The host's [`PluginToolAdapter`]
//! impls `AgentTool::execute` by:
//!
//! 1. **NOT owning a runtime.** Acquire the ambient runtime
//!    (`tokio::runtime::Handle::try_current()`) — the adapter only ever runs
//!    inside the agent loop's runtime.
//! 2. Set up an **unbounded mpsc** for `ToolResultPartial` (async side drains +
//!    forwards to `on_update`) and a **oneshot** for the terminal
//!    `AgentToolResult`.
//! 3. `handle.spawn_blocking(move || drive(plugin, cancel_flag, partial_tx,
//!    done_tx))` — the blocking driver loops `poll` until `Done`/`Err`,
//!    forwarding `Pending` partials through the mpsc (via a `catch_unwind`
//!    trampoline so a panicking partial callback can't unwind across FFI),
//!    then sends the terminal result + calls `destroy` **exactly once** on exit.
//! 4. The async future `select!`s between the oneshot (terminal) and
//!    `signal.cancelled()` (the child token). On cancel: **set an `AtomicBool`
//!    cancel flag (SeqCst)** so the blocking driver observes it; **keep
//!    awaiting the oneshot** (never drop the driver — `spawn_blocking` tasks
//!    run to completion regardless of outer-future drop, so dropping is a
//!    thread leak).
//! 5. `Drop`/drop-guard sets **only the cancel flag** — never calls `destroy`
//!    synchronously (the driver may be mid-`poll`); `destroy` is called exactly
//!    once by the blocking driver.
//!
//! `cancel` ≠ `destroy`: the first draft conflated them → UAF/double-free. Here
//! `cancel` is an idempotent thread-safe flag-set; `destroy` is the single
//! free, owned by the driver.
//!
//! ## Events
//!
//! [`ExtensionEmitter`] impls `AgentEmitter` by subscribing to the host's
//! `broadcast::Sender<AgentEvent>`, translating each `AgentEvent` → a
//! [`rpi_plugin_sdk::StablePluginEvent`], and dispatching to every registered
//! handler for the event's tag — all dispatch wrapped in `catch_unwind`. The
//! 33-category `on()` surface (B3) is driven through this emitter; the
//! 10 already-emitted `AgentEvent` variants fold into the matching tags now,
//! and the remaining tags light up as B3/B4/B5 add the emission points.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use rpi_plugin_sdk::{
    EventHandlerFn, EventTag, FreeStringFn, PluginApiVt, ProviderRequestFn, RenderFn,
    ResourcesDiscoverFn, RuntimeActionFn, StablePluginEvent, StableToolSchema, StbString,
    StbStringRef, ToolCancelFn, ToolDestroyFn, ToolExecuteFn, ToolPollFn,
};
use thiserror::Error;

pub use actions::{
    reload_callback_from_mailbox, trampoline_runtime_action, ActionBridge, ReloadMailbox,
    RuntimeActionHost,
};
pub use loader::{
    load_dir, load_one, load_session, load_session_mixed, merge_registries, ExtensionSession,
    LoadedPlugin, PluginKeepalive, PluginLoadError,
};
pub use provider::PluggableProvider;
pub use provider_hooks::ExtensionProviderHooks;
pub use registry::{
    assert_active, ExtensionRegistry, ExtensionTool, RegisteredHandler, RegisteredProvider,
    RegisteredRenderer, RegisteredRendererKind, RegistryEntry, RegistrySnapshot,
    ResourcesDiscoverHandler,
};
pub use resources::{emit_resources_discover, DiscoveredResources};
pub use tool::{PluginToolAdapter, PluginToolHandle};
pub use translate::{ExtensionEmitter, TeeEmitter};

mod actions;
mod loader;
mod provider;
mod provider_hooks;
mod registry;
mod resources;
mod tool;
mod translate;

/// A diagnostics/event sink the host wires so the loader + adapter can report
/// plugin-load skips, ABI mismatches, panics caught at the FFI boundary, etc.
/// Mirrors the `diagnostic` channel pi surfaces for extensions.
pub trait PluginDiagnostics: Send + Sync {
    /// A non-fatal warning (e.g. "skipped plugin foo: ABI version mismatch").
    fn warn(&self, message: &str);
    /// A plugin that registered something but a registration slot is not yet
    /// wired on the host (e.g. an event tag the host does not emit yet).
    fn unsupported(&self, message: &str);
}

/// A no-op diagnostics sink (the default when the host does not supply one).
#[derive(Default)]
pub struct NullDiagnostics;

impl PluginDiagnostics for NullDiagnostics {
    fn warn(&self, _message: &str) {}
    fn unsupported(&self, _message: &str) {}
}

/// Error returned by [`PluginToolAdapter::execute`] when the plugin side failed
/// (terminal `Err` from `poll`, or the drive handle was never produced).
#[derive(Debug, Error)]
pub enum PluginToolError {
    #[error("plugin execute returned a null handle (allocation failure)")]
    NullHandle,
    #[error("plugin error: {0}")]
    Plugin(String),
    #[error("runtime unavailable: {0}")]
    NoRuntime(String),
}

/// The host's `free_string` for [`StbString`]s the host *produces* and hands to
/// the plugin (event payloads, action outputs, execute params when the host
/// owns them). The plugin frees what it *receives* via this; the host frees
/// what it *receives* via the plugin's `free_string` (stored per-tool).
///
/// Reconstructs the `Box<[u8]>` from ptr+len and drops it. Idempotent on
/// empty/null.
pub extern "C" fn host_free_string(s: StbString) {
    if s.is_empty() || s.ptr.is_null() {
        return;
    }
    // SAFETY: the host produced this StbString via `StbString::from_owned`/
    // `from_string` (a `Box<[u8]>`), so ptr+len reconstruct the same allocation.
    unsafe {
        let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
        let _ = Box::from_raw(slice as *const [u8] as *mut [u8]);
    }
}

// ===========================================================================
// HostApi — the PluginApiVt the host builds and hands to each plugin's register
// ===========================================================================

/// The host state a [`PluginApiVt`] closes over. Held behind `Arc` so the fn
/// pointers (which are `extern "C"`,不好做闭包) can recover the host state via
/// the `user_data` slot — but since `extern "C" fn` cannot capture, the host
/// stores per-registration receiver state in the [`HostApi`] itself keyed by
/// nothing (single registry per host), and the fns are thin trampolines that
/// read a process-global-attached registry. In v1 we keep it simple: the host
/// builds one `HostApi` per load session; the `register_*` trampolines forward
/// into it.
///
/// This struct is `Send + Sync` (registry is `Mutex`-guarded).
pub struct HostApi {
    registry: Mutex<Option<ExtensionRegistry>>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    /// B5a: the plugin→host action bridge, carried in
    /// [`PluginApiVt::user_data`] so [`trampoline_runtime_action`] can recover
    /// the harness state from ANY thread a plugin calls from (post-register, no
    /// thread-local). `None` keeps the v1 stub `runtime_action` and the register
    /// `user_data` (the `HostApi` pointer) — so older call sites that don't pass
    /// a bridge behave exactly as before.
    action_bridge: Option<Arc<ActionBridge>>,
}

impl HostApi {
    /// Build a fresh host API bound to the given registry + diagnostics. v1
    /// (no action bridge): `runtime_action` stays the stub returning `-1`.
    pub fn new(registry: ExtensionRegistry, diagnostics: Arc<dyn PluginDiagnostics>) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Some(registry)),
            diagnostics,
            action_bridge: None,
        })
    }

    /// B5a: build a host API that wires the real [`trampoline_runtime_action`]
    /// via the bridge. The bridge is cloned into every plugin's vtable
    /// `user_data` so post-register `runtime_action` calls recover it on any
    /// thread. `rpi-cli` keeps one master `Arc<ActionBridge>` per session; the
    /// `HostApi` here holds a clone only for the duration of `load_one` (it is
    /// dropped after `take_registry`, but the master arc in `rpi-cli` keeps the
    /// pointer valid).
    pub fn with_action_bridge(
        registry: ExtensionRegistry,
        diagnostics: Arc<dyn PluginDiagnostics>,
        action_bridge: Arc<ActionBridge>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Some(registry)),
            diagnostics,
            action_bridge: Some(action_bridge),
        })
    }

    fn with_registry<R>(&self, f: impl FnOnce(&mut ExtensionRegistry) -> R) -> Option<R> {
        let mut guard = self.registry.lock().expect("host api registry lock");
        guard.as_mut().map(f)
    }

    /// Detach the registry (call after registration completes so the host can
    /// move it out for use). The `Mutex<Option<...>>` is left `None`.
    pub fn take_registry(&self) -> Option<ExtensionRegistry> {
        self.registry.lock().expect("host api registry lock").take()
    }

    /// Build the C vtable the host passes to a plugin's `rpi_plugin_register`.
    ///
    /// Every optional registrar slot currently resolves to a real `extern "C"`
    /// trampoline that forwards into `self`'s registry (so a plugin that calls
    /// `register_tool` / `register_command` / renderer registration now sees
    /// its registration land). `runtime_action` resolves
    /// to the real [`trampoline_runtime_action`] when a bridge is present (B5a),
    /// else the stub returning `-1` (v1).
    ///
    /// **`user_data`**: the register trampolines recover host state via the
    /// thread-local `CURRENT_HOST_API` (set for the duration of register in
    /// `load_one`) — they ignore `user_data`. Post-register `runtime_action`
    /// cannot use the thread-local (plugin calls from foreign threads), so when
    /// a bridge is present `user_data` is repurposed to point at the
    /// `ActionBridge` (the SDK-designated "host's opaque context"). The bridge's
    /// master `Arc` is kept by `rpi-cli` for the harness lifetime, so the
    /// pointer a plugin stores during register stays valid.
    pub fn build_vtable(self: &Arc<Self>) -> PluginApiVt {
        // When a bridge is present, user_data carries it (post-register action
        // recovery). Otherwise keep the register-path HostApi pointer (harmless
        // — register trampolines use the thread-local and ignore user_data).
        // Cast both arms to the `RuntimeActionFn` pointer type — distinct fn
        // items have unique types even with identical signatures, so the match
        // needs a common fn-pointer type to unify on (the vtable field is
        // `RuntimeActionFn`, a bare `extern "C" fn` alias, not an `Option`).
        let (runtime_action_fn, ud) = match &self.action_bridge {
            Some(bridge) => (
                trampoline_runtime_action as RuntimeActionFn,
                Arc::as_ptr(bridge) as *mut c_void,
            ),
            None => (
                stub_runtime_action as RuntimeActionFn,
                Arc::as_ptr(self) as *mut c_void,
            ),
        };
        PluginApiVt {
            free_string: host_free_string,
            register_tool: Some(trampoline_register_tool),
            register_command: Some(trampoline_register_command),
            register_shortcut: Some(trampoline_register_shortcut),
            register_flag: Some(trampoline_register_flag),
            register_provider: Some(trampoline_register_provider), // B5c
            register_message_renderer: Some(trampoline_register_message_renderer), // B5c
            register_markdown_transformer: Some(trampoline_register_markdown_transformer), // B5c
            register_entry_renderer: Some(trampoline_register_entry_renderer), // B5c
            register_event_handler: Some(trampoline_register_event_handler),
            register_resources_discover: Some(trampoline_register_resources_discover),
            runtime_action: runtime_action_fn,
            dispatch_event: Some(trampoline_dispatch_event),
            user_data: ud,
        }
    }
}

// Thread-local "current host api" for the duration of a `rpi_plugin_register`
// call. Set by [`load_one`] before calling register, cleared after. This is the
// sound way to let `extern "C" fn` trampolines (which cannot capture) reach the
// host's registry: the register call is synchronous and single-threaded per
// plugin, so a thread-local is unambiguous.
thread_local! {
    static CURRENT_HOST_API: std::cell::Cell<*const HostApi> = std::cell::Cell::new(std::ptr::null());
}

/// SAFETY: must be called only while the `Arc<HostApi>` pointed to by `api` is
/// kept alive (i.e. inside `with_current_api`). Sets the thread-local current
/// api pointer.
unsafe fn set_current_api(api: &Arc<HostApi>) {
    CURRENT_HOST_API.with(|c| c.set(Arc::as_ptr(api) as *const HostApi));
}

fn clear_current_api() {
    CURRENT_HOST_API.with(|c| c.set(std::ptr::null()));
}

/// Run `f` with the current thread-local host api borrowed. No-op (returns
/// `false`) if no api is current (e.g. a trampoline called outside register).
fn with_current_api<R>(f: impl FnOnce(&HostApi) -> R) -> Option<R> {
    let ptr = CURRENT_HOST_API.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the `Arc<HostApi>` is alive for the duration of register (kept by
    // `load_one`'s stack), and this thread set it; borrowing here is sound.
    let api = unsafe { &*ptr };
    Some(f(api))
}

/// True if a current host api is set (cheap check used by trampolines that only
/// need presence, not the api itself).
fn current_api_present() -> bool {
    let ptr = CURRENT_HOST_API.with(|c| c.get());
    !ptr.is_null()
}

// --- the registrar trampolines (extern "C", forward into the current api) ---

extern "C" fn trampoline_register_tool(
    schema: *const StableToolSchema,
    execute_fn: ToolExecuteFn,
    poll_fn: ToolPollFn,
    cancel_fn: ToolCancelFn,
    destroy_fn: ToolDestroyFn,
    plugin_free_string: FreeStringFn,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    if schema.is_null() {
        return 1;
    }
    // SAFETY: the plugin guarantees `schema` is valid for the call; we copy the
    // strings out immediately (under the borrow) and free them via the plugin's
    // `plugin_free_string` right after, so no retention past the call.
    let (name, description, parameters_value, schema_owned) = unsafe {
        let s = &*schema;
        (
            s.name.to_string_lossy(),
            s.description.to_string_lossy(),
            serde_json::from_str::<serde_json::Value>(&s.parameters.to_string_lossy())
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
            *s,
        )
    };
    // Free the schema strings via the plugin's free fn (plugin produced them).
    // `FreeStringFn` is `extern "C" fn(StbString)` — calling a bare fn pointer
    // is safe (no `unsafe` needed); the idempotent free contract is on the plugin.
    plugin_free_string(schema_owned.name);
    plugin_free_string(schema_owned.description);
    plugin_free_string(schema_owned.parameters);
    let tool = rpi_ai::types::Tool {
        name,
        description,
        parameters: rpi_ai::types::Schema::new(parameters_value),
        constrained_sampling: None,
    };
    let handle = PluginToolHandle {
        execute_fn,
        poll_fn,
        cancel_fn,
        destroy_fn,
        plugin_free_string,
    };
    // `register_tool` returns `bool` (true on overwrite of a prior same-named
    // tool). For the plugin a name collision is not a hard failure — always
    // return 0 (success) when the registry accepted it; -1 only if the registry
    // was already detached.
    let ok =
        with_current_api(
            |api| match api.with_registry(|reg| reg.register_tool(tool, handle)) {
                Some(_) => true,
                None => false,
            },
        );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_command(
    name: StbStringRef,
    description: StbStringRef,
    handler: rpi_plugin_sdk::CommandHandlerFn,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the refs are valid for this call.
    let (name, description) =
        unsafe { (name.as_str().to_string(), description.as_str().to_string()) };
    let ok = with_current_api(|api| {
        // The v1 command ABI predates an explicit context parameter. Keep a
        // null context for compatibility; plugins that need host state can use
        // `runtime_action` from their command callback.
        match api.with_registry(|reg| {
            reg.register_command(name, description, handler, std::ptr::null_mut())
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_shortcut(_key: StbStringRef, _description: StbStringRef) -> i32 {
    // v1: shortcuts are a TUI concern (B5). Record nothing; return ok so the
    // plugin doesn't error, but note via diagnostics it's unsupported.
    with_current_api(|api| api.diagnostics.unsupported("register_shortcut (TUI — B5)"));
    0
}

extern "C" fn trampoline_register_flag(_name: StbStringRef, _description: StbStringRef) -> i32 {
    with_current_api(|api| api.diagnostics.unsupported("register_flag (CLI args — B5)"));
    0
}

extern "C" fn trampoline_register_event_handler(
    tag: EventTag,
    handler: EventHandlerFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| reg.register_event_handler(tag, handler, user_data)) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// B5c: `register_provider` trampoline. Runs synchronously inside a plugin's
/// `register` call (thread-local `CURRENT_HOST_API` is set). The plugin hands its
/// `provider_id`/`base_url`/`api_style` (borrowed `StbStringRef`s), its
/// `request_fn`, its own `plugin_free_string` (the `out` StbString `request_fn`
/// later produces is plugin-owned — the host reclaims it via this fn), and its
/// opaque `user_data`. We copy the id/base_url/api_style to owned `String`s
/// (they're the provider's identity, read when building the
/// [`PluggableProvider`](crate::PluggableProvider) — we can't keep the borrowed
/// refs past register), then store the full record in the registry.
extern "C" fn trampoline_register_provider(
    provider_id: StbStringRef,
    base_url: StbStringRef,
    api_style: StbStringRef,
    request_fn: ProviderRequestFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the refs are valid for this call.
    let record = crate::registry::RegisteredProvider {
        provider_id: unsafe { provider_id.as_str().to_string() },
        base_url: unsafe { base_url.as_str().to_string() },
        api_style: unsafe { api_style.as_str().to_string() },
        request_fn,
        plugin_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_provider(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// B5c: the three renderer registrars share a single recording helper; each
/// trampoline below fixes its [`RegisteredRendererKind`] and forwards here.
fn register_renderer_common(
    name: StbStringRef,
    kind: crate::registry::RegisteredRendererKind,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the ref is valid for this call.
    let record = crate::registry::RegisteredRenderer {
        name: unsafe { name.as_str().to_string() },
        kind,
        render_fn,
        plugin_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_renderer(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_message_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Message,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_markdown_transformer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Markdown,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_entry_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Entry,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_dispatch_event(_event: StablePluginEvent, _user_data: *mut c_void) -> i32 {
    // v1: a plugin emitting an event upstream — we accept it (return 0) but do
    // not yet forward to host subscribers (no upstream channel wired). B5 wires
    // the reverse-direction event bus.
    0
}

/// B5b: `register_resources_discover` trampoline. Runs synchronously inside a
/// plugin's `register` call (so the thread-local `CURRENT_HOST_API` is set → the
/// register-path recovery works, same as the other `trampoline_register_*` fns).
/// The plugin hands its `handler`, its own `plugin_free_string` (the `out`
/// StbString the handler later produces is plugin-owned — the host must reclaim
/// it via this fn), and its opaque `user_data`. The host stores all three in the
/// registry and later fans the discovery event out via `emit_resources_discover`.
extern "C" fn trampoline_register_resources_discover(
    handler: ResourcesDiscoverFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_resources_discover(handler, plugin_free_string, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// v1 fallback: kept for [`HostApi`]s built without an [`ActionBridge`] (the
/// old `HostApi::new` path). Return -1 (`EPERM`-ish) so a plugin can detect
/// "unsupported" without crashing. When a bridge is present, `build_vtable`
/// installs [`trampoline_runtime_action`] instead.
extern "C" fn stub_runtime_action(
    _action: rpi_plugin_sdk::RuntimeActionId,
    _args: StbStringRef,
    _out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    -1
}

// Keep the `RuntimeActionFn` type name referenced for doc clarity / future wiring.
#[allow(dead_code)]
type _RuntimeActionFnDoc = RuntimeActionFn;
