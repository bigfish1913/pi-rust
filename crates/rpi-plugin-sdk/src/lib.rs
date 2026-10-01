//! Stable `#[repr(C)]` ABI contract for rpi **Rust-native (cdylib) plugins**.
//!
//! rpi loads extensions as compiled Rust cdylibs (`.dll`/`.so`/`.dylib`) via
//! `libloading` — **not** TS/jiti. Because we control both sides, the plugin is
//! Rust, but the *boundary* is still a hand-defined C ABI: the two sides may be
//! compiled with different Rust versions / crate versions, so no Rust type with
//! a non-`C` repr or a `Drop` impl may cross. This crate defines exactly those
//! crossing types and the registration contract.
//!
//! ## Soundness rules (load-bearing — verified by an adversarial review)
//!
//! 1. **Every crossing type is `#[repr(C)]`.** Enums used in unions carry
//!    `#[repr(u32)]` so the discriminant width is pinned.
//! 2. **No `Drop` type crosses.** `Vec`/`String`/`serde_json::Value`/`Option` of
//!    those / `Result` never appear in the ABI. Owned data crosses as
//!    [`StbString`] (ptr+len) with an **explicit `free_string`** the producer
//!    exports. [`StbString`] is `Copy` (raw pointers are `Copy`); copying
//!    duplicates the *pointer*, not the allocation, so each allocation is freed
//!    **exactly once** by the side that received it (see [`StbString`] docs).
//! 3. **Owned → JSON round-trip.** Structured host data ([`StableJsonValue`],
//!    tool params, `AgentToolResult`, events) crosses as a JSON string in a
//!    [`StbString`]. `serde_json` with `preserve_order` + `arbitrary_precision`
//!    must be enabled **consistently on host AND plugin** or integers >
//!    `u64`/`i64` lose precision and object keys may reorder — documented as a
//!    an ABI-wide limit. Tool args from the model rarely carry overflow ints, but it is
//!    never silent.
//! 4. **Unions are all-`Copy` payloads.** [`EventPayload`] / [`StepResultPayload`]
//!    variants are `#[repr(C)]` structs of primitives or [`StbString`] only, so
//!    the union is `Copy`-able and a wrong-variant read is `unsafe` (caller
//!    discriminates by `tag`).
//! 5. **Unwinding never crosses the ABI.** Every host→plugin and plugin→host
//!    call is `extern "C"`; both sides wrap dispatch in `catch_unwind`
//!    (abort-on-unwind / log-and-drop). A poisoned mutex or panicking emitter
//!    cannot unwind into the other side.
//!
//! ## Lifetime: the 4-function handle
//!
//! A registered tool drives an execution through **four** plugin-exported
//! functions (see [`ToolExecuteFn`] / [`ToolPollFn`] / [`ToolCancelFn`] /
//! [`ToolDestroyFn`]) — `execute`→[`StepHandle`] (plugin-allocates), `poll`
//! (non-blocking, **borrows** the handle, returns [`StepResult`]), `cancel`
//! (sets an internal `AtomicBool` flag; **idempotent; does NOT free;
//! thread-safe**), `destroy` (frees; **idempotent; called exactly once by the
//! blocking driver**). `cancel` ≠ `destroy`: conflating them is a UAF /
//! double-free. The adapter's blocking driver calls `poll` in a loop until
//! `Done`/`Err`, forwards `Pending` partials, and calls `destroy` once on exit.
//!
//! `poll` is **non-blocking** and MUST observe the cancel flag and return
//! `Done`/`Err` within a bounded number of polls; otherwise a cancelled call
//! leaks a `spawn_blocking` thread forever (those tasks run to completion
//! regardless of outer-future drop).
//!
//! The crate is `std` (not `no_std`): the ABI *types* are `#[repr(C)]` POD with
//! no `Drop` — that is what makes the boundary sound — but plugins and the host
//! are ordinary binaries with `std`, so the constructor/reader helpers and tests
//! use `String`/`Vec`/`serde_json` directly. The `json` feature keeps
//! `serde_json` optional for a plugin that wants to skip it.

use core::ffi::c_char;
use core::ffi::c_void;
use core::ptr;

// ---------------------------------------------------------------------------
// StbString — owned UTF-8 crossing as ptr+len with an explicit free
// ---------------------------------------------------------------------------

/// An owned UTF-8 string crossing the ABI as a `(ptr, len)` pair.
///
/// `Copy` (raw pointers are `Copy`): copying a `StbString` duplicates the
/// **pointer**, not the allocation. The **receiver** of a `StbString` owns the
/// allocation and MUST free it **exactly once** by calling the producer's
/// [`FreeStringFn`] (or [`StbString::free_with`] / [`StbString::free_host`]).
/// Never `free` a `StbString` you did not receive as an owner (e.g. one built
/// from a borrow via [`StbString::from_ref`], which is non-owning — its `free`
/// is a no-op only if the producer guarantees the buffer outlives the call; in
/// practice inputs cross as [`StbStringRef`] instead).
///
/// **Construction ownership contract:**
/// - [`StbString::from_owned`] — takes a `Box<[u8]>` the caller allocated; the
///   `StbString` now owns it; `free` deallocates.
/// - [`StbString::from_boxed_str`] / [`StbString::from_string`] — convenience
///   over `from_owned` (host side, needs `alloc`).
/// - [`StbString::empty`] — null/0; `free` is a no-op.
///
/// **Null `ptr` ⇒ empty** (`len` MUST be 0). A null pointer is never
/// dereferenced.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbString {
    /// UTF-8 bytes. Null when `len == 0` (empty string).
    pub ptr: *mut c_char,
    /// Byte length (NOT a NUL terminator — the buffer is NOT NUL-terminated).
    pub len: usize,
}

// SAFETY: `StbString` is a plain `(ptr, len)` of raw pointers — no ownership
// transferred across threads by the type itself, and lifetime/aliasing is the
// caller's contract (documented above). It is `Send`+`Sync` so the host and the
// blocking driver can pass it across threads; the *allocation* ownership rules
// above still apply regardless of thread.
unsafe impl Send for StbString {}
unsafe impl Sync for StbString {}

impl StbString {
    /// An empty string: null pointer, zero length. `free` is a no-op.
    pub const fn empty() -> Self {
        Self {
            ptr: ptr::null_mut(),
            len: 0,
        }
    }

    /// Whether this is the empty/null string.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// Allocating constructors + safe reader need `std` (Box/Vec/String). The ABI
// type itself (`StbString { ptr, len }`) is POD with no `Drop` — that is what
// makes the boundary sound. These helpers are available whenever `std` is (the
// crate is std-using). When the `json` feature is off a plugin still gets these
// because the crate links std; the gate here keeps `serde_json` truly optional.
#[cfg(any(feature = "json", test))]
impl StbString {
    /// Wrap an allocation the caller already boxed. The `StbString` takes
    /// ownership: a later `free_with(free_fn)` will deallocate it via the
    /// producer's `free_string`.
    ///
    /// The buffer MUST be UTF-8.
    pub fn from_owned(buf: Box<[u8]>) -> Self {
        let len = buf.len();
        // Stabilize the pointer via `Box::into_raw`; the free fn reconstructs a
        // slice from ptr+len and drops it. Store the element pointer as
        // `*mut c_char` (u8 ↔ c_char on every platform rpi targets). Ownership
        // moves into the `StbString` (we do NOT drop here).
        let ptr = Box::into_raw(buf) as *mut [u8] as *mut u8 as *mut c_char;
        let _ = len;
        Self { ptr, len }
    }

    /// Convenience: from a `String`, transferring ownership. After this the
    /// passed `String` is consumed and must not be reused.
    pub fn from_string(s: String) -> Self {
        Self::from_vec(s.into_bytes())
    }

    /// Convenience: from a `Vec<u8>` (UTF-8), transferring ownership.
    pub fn from_vec(v: Vec<u8>) -> Self {
        Self::from_owned(v.into_boxed_slice())
    }

    /// Convenience: from a boxed `str`.
    pub fn from_boxed_str(s: Box<str>) -> Self {
        let string: String = s.into();
        Self::from_vec(string.into_bytes())
    }

    /// Copy this `StbString`'s bytes into an owned `String` (**without** freeing
    /// the original — the caller still owns the original allocation). Use this
    /// to *read* a received `StbString` into safe Rust; then `free` the original.
    pub fn to_string_lossy(&self) -> String {
        if self.len == 0 || self.ptr.is_null() {
            return String::new();
        }
        // SAFETY: the producer guarantees `ptr` is valid for `len` bytes and the
        // bytes are UTF-8. We only read (no free) here.
        let slice = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) };
        String::from_utf8_lossy(slice).into_owned()
    }
}

/// A **borrowed**, non-owning view of a string passed as an *input* to an FFI
/// call. The callee MUST NOT free it and MUST NOT retain it past the call.
///
/// Built from a `&str` on the calling side; the buffer is valid for the
/// duration of the call (the caller's borrow).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbStringRef {
    /// UTF-8 bytes (valid for the call; NUL-terminated not required).
    pub ptr: *const c_char,
    /// Byte length.
    pub len: usize,
}

unsafe impl Send for StbStringRef {}
unsafe impl Sync for StbStringRef {}

impl StbStringRef {
    /// Empty input.
    pub const fn empty() -> Self {
        Self {
            ptr: ptr::null(),
            len: 0,
        }
    }

    /// Borrow a `&str` for the duration of a call. The caller must outlive the
    /// call (normal borrow rules).
    pub fn from_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr() as *const c_char,
            len: s.len(),
        }
    }

    /// Read into a safe `&str` for the callee's lifetime `'a`.
    ///
    /// # Safety
    /// The caller guarantees `ptr` is valid for `len` bytes and they are UTF-8,
    /// and the borrow survives `'a`.
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        if self.len == 0 || self.ptr.is_null() {
            return "";
        }
        let slice = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) };
        unsafe { core::str::from_utf8_unchecked(slice) }
    }
}

/// Function pointer a plugin exports to free a [`StbString`] it produced.
/// Idempotent: freeing an already-freed or empty `StbString` is a no-op.
///
/// The host calls this for every [`StbString`] it receives from the plugin; the
/// plugin calls the **host's** `free_string` (from [`PluginApi`]) for every
/// [`StbString`] it receives from the host.
pub type FreeStringFn = extern "C" fn(s: StbString);

impl StbString {
    /// Free this `StbString` via the given `free_string` fn, if non-null and
    /// non-empty. Consumes ownership (the value is `Copy`, but semantically the
    /// caller relinquishes the allocation).
    ///
    /// After this call the bytes are invalid; do not use the `StbString` again.
    pub fn free_with(self, free_fn: Option<FreeStringFn>) {
        if let Some(free_fn) = free_fn {
            if !self.is_empty() {
                free_fn(self);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// StableJsonValue — JSON round-trip helpers (json feature)
// ---------------------------------------------------------------------------

/// Helpers to cross structured data as JSON-in-`StbString`. See the module docs
/// for the precision/order limit.
#[cfg(any(feature = "json", doc))]
pub mod json {
    use super::StbString;
    use serde_json::Value;

    /// Serialize a `serde_json::Value` into an owning [`StbString`] the receiver
    /// must `free` via the producer's `free_string`.
    pub fn to_stable(value: &Value, free_fn: Option<super::FreeStringFn>) -> StbString {
        let s = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
        let stb = StbString::from_string(s);
        // `free_fn` is advisory metadata the receiver needs; the StbString
        // itself carries only ptr+len. Stash nothing — the receiver must know
        // which free fn to use (host's vs plugin's) by direction.
        let _ = free_fn;
        stb
    }

    /// Parse a received [`StbString`] back into a `Value`. Does **not** free the
    /// input — the caller still owns it.
    pub fn from_stable(s: &StbString) -> Value {
        let text = s.to_string_lossy();
        if text.is_empty() {
            return Value::Null;
        }
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// StableToolSchema — the provider-facing tool definition
// ---------------------------------------------------------------------------

/// A tool's provider-facing schema crossing the ABI. `name` / `description` are
/// raw strings; `parameters` is a JSON Schema serialized to a JSON string (the
/// host parses it into its native `schemars::Schema`).
///
/// All three are owning [`StbString`]s the **plugin** produced; the **host**
/// frees them via the plugin's `free_string` (passed to [`ToolExecuteFn`] /
/// `register_tool`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StableToolSchema {
    pub name: StbString,
    pub description: StbString,
    /// JSON-encoded JSON Schema for the tool's `parameters`.
    pub parameters: StbString,
}

unsafe impl Send for StableToolSchema {}
unsafe impl Sync for StableToolSchema {}

// ---------------------------------------------------------------------------
// Host context injected into plugin tool arguments
// ---------------------------------------------------------------------------

/// Reserved key the host adds to a **plugin** tool call's arguments.
///
/// The model never writes this field and never sees it: the host injects it
/// after the model's arguments are parsed and before the plugin's
/// [`ToolExecuteFn`] runs, so a plugin can tell *which* project and *which*
/// session it is serving without the model having to supply either. (Built-in
/// tools do not need it — they receive the host's `ExecutionToolContext`
/// directly. A plugin tool has no such channel.)
///
/// The value is an object; today:
///
/// ```json
/// { "cwd": "D:\\Projects\\pi-rust",
///   "sessionId": "01adb026-7230-708b-855b-91eab6b056b8" }
/// ```
///
/// Both fields are optional and either may be absent: `cwd` when the host
/// cannot resolve one, `sessionId` for a session that has no id (ephemeral
/// sessions) and on hosts that predate this field. The key itself may be absent
/// entirely. Treat everything under it as best-effort, and prefer a
/// project-scoped fallback when `sessionId` is missing rather than refusing to
/// work.
///
/// **Consequence for plugin parameter types:** a plugin must tolerate unknown
/// top-level keys in its arguments. Reading them out of a `serde_json::Value`
/// does that naturally; a `#[serde(deny_unknown_fields)]` parameter struct
/// would reject this key, so do not use one for tool parameters.
///
/// The host's value wins if the model happens to send the same key: this is
/// host truth, not a hint to be merged.
pub const HOST_CONTEXT_KEY: &str = "__rpi";

// ---------------------------------------------------------------------------
// StepResult — the poll() return: Pending | Done | Err (explicit tag + union)
// ---------------------------------------------------------------------------

/// Opaque, plugin-allocated handle for one tool execution drive. Produced by
/// [`ToolExecuteFn`], polled by [`ToolPollFn`], cancelled by [`ToolCancelFn`],
/// freed by [`ToolDestroyFn`] (exactly once, idempotent).
///
/// The handle's interior layout is entirely plugin-private; the host treats it
/// as an opaque pointer.
pub type StepHandle = *mut c_void;

/// Discriminant for [`StepResult`]. `#[repr(u32)]` pins the width so the union
/// payload is sound across compilers.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResultTag {
    /// `poll` has no terminal result yet; the partial payload may carry progress.
    Pending = 0,
    /// Terminal success; `done.result` is the JSON `AgentToolResult`.
    Done = 1,
    /// Terminal failure; `err.message` is a UTF-8 error string.
    Err = 2,
}

/// A partial/progress result emitted during `Pending`. `progress` is a JSON
/// `AgentToolResult` (the same shape `on_update` carries) — the host forwards it
/// to the adapter's `on_update` callback. May be empty.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbPending {
    pub progress: StbString,
}

/// Terminal success payload. `result` is a JSON `AgentToolResult`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbDone {
    pub result: StbString,
}

/// Terminal failure payload. `message` is a UTF-8 error string.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StbErr {
    pub message: StbString,
}

/// The `poll()` return value. Read the `payload` variant matching `tag`.
///
/// All payload variants are `#[repr(C)]` structs of [`StbString`] (Copy), so the
/// union is `Copy`. A wrong-variant read is `unsafe`; always match on `tag`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union StepResultPayload {
    pub pending: StbPending,
    pub done: StbDone,
    pub err: StbErr,
}

/// Return value of [`ToolPollFn`]. The blocking driver matches on `tag`, reads
/// the matching payload, and breaks the loop on `Done`/`Err`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StepResult {
    pub tag: StepResultTag,
    pub payload: StepResultPayload,
}

impl StepResult {
    /// Build a `Pending` with a progress JSON string (may be empty).
    pub fn pending(progress: StbString) -> Self {
        Self {
            tag: StepResultTag::Pending,
            payload: StepResultPayload {
                pending: StbPending { progress },
            },
        }
    }

    /// Build a `Done` with the terminal JSON `AgentToolResult`.
    pub fn done(result: StbString) -> Self {
        Self {
            tag: StepResultTag::Done,
            payload: StepResultPayload {
                done: StbDone { result },
            },
        }
    }

    /// Build an `Err` with an error message.
    pub fn err(message: StbString) -> Self {
        Self {
            tag: StepResultTag::Err,
            payload: StepResultPayload {
                err: StbErr { message },
            },
        }
    }

    /// Access the `pending` payload. Caller MUST guarantee `tag == Pending`.
    ///
    /// # Safety
    /// Undefined behavior if `tag != StepResultTag::Pending`.
    pub unsafe fn pending_payload(&self) -> &StbPending {
        unsafe { &self.payload.pending }
    }

    /// Access the `done` payload. Caller MUST guarantee `tag == Done`.
    ///
    /// # Safety
    /// Undefined behavior if `tag != StepResultTag::Done`.
    pub unsafe fn done_payload(&self) -> &StbDone {
        unsafe { &self.payload.done }
    }

    /// Access the `err` payload. Caller MUST guarantee `tag == Err`.
    ///
    /// # Safety
    /// Undefined behavior if `tag != StepResultTag::Err`.
    pub unsafe fn err_payload(&self) -> &StbErr {
        unsafe { &self.payload.err }
    }
}

// ---------------------------------------------------------------------------
// The 4-function tool lifecycle fn-pointer types
// ---------------------------------------------------------------------------

/// Partial-result callback the **blocking driver** passes to `poll`, wrapped in
/// `catch_unwind` on the host side. The plugin invokes it **synchronously
/// inside `poll()`** when it has a `Pending` partial — never retained, never
/// invoked after `Done`/`Err`.
///
/// `partial` is a JSON `AgentToolResult`; ownership passes to the callback (the
/// host frees it via the host's `free_string`).
pub type ToolPartialCb = extern "C" fn(partial: StbString, user_data: *mut c_void);

/// `execute(tool_call_id, params) -> StepHandle`. Plugin-allocates a drive
/// handle and begins the work (non-blocking — the real progress comes via
/// `poll`). `tool_call_id` is a borrowed [`StbStringRef`] (valid for the call);
/// `params` is an owning JSON string of the tool-call arguments (the plugin
/// frees it via the host's `free_string`). Returns null on allocation failure.
pub type ToolExecuteFn = extern "C" fn(
    tool_call_id: StbStringRef,
    params: StbString,
    free_params: Option<FreeStringFn>,
) -> StepHandle;

/// `poll(handle, partial_cb, user_data) -> StepResult`. **Non-blocking.** Must
/// observe the cancel flag (set by [`ToolCancelFn`]) and return `Done`/`Err`
/// within a bounded number of polls. Borrows `handle` (does not free it).
pub type ToolPollFn = extern "C" fn(
    handle: StepHandle,
    partial_cb: Option<ToolPartialCb>,
    user_data: *mut c_void,
) -> StepResult;

/// `cancel(handle)`. Sets an internal `AtomicBool` (SeqCst) cancel flag.
/// **Idempotent, thread-safe, does NOT free.** The poll loop observes it.
pub type ToolCancelFn = extern "C" fn(handle: StepHandle);

/// `destroy(handle)`. Frees the handle. **Idempotent; called exactly once by the
/// blocking driver on exit** (after the loop sees `Done`/`Err`, or after cancel
/// propagated). Null handle is a no-op.
pub type ToolDestroyFn = extern "C" fn(handle: StepHandle);

// ---------------------------------------------------------------------------
// StablePluginEvent — 33 on() categories (explicit tag + union)
// ---------------------------------------------------------------------------

/// Discriminant for [`StablePluginEvent`], one variant per pi `on()` category
/// (33 total) plus one rpi-specific category (34 total). `#[repr(u32)]` pins
/// the discriminant width.
///
/// The 33 Pi categories (verified against `extensions/types.ts:1203-1244`):
/// project_trust, resources_discover, session_start, session_info_changed,
/// session_before_switch, session_before_fork, session_before_compact,
/// session_compact, session_shutdown, session_before_tree, session_tree,
/// context, before_provider_request, before_provider_headers,
/// after_provider_response, before_agent_start, agent_start, agent_end,
/// agent_settled, turn_start, turn_end, message_start, message_update,
/// message_end, tool_execution_start, tool_execution_update,
/// tool_execution_end, model_select, thinking_level_select, tool_call,
/// tool_result, user_bash, input.
///
/// `BeforeTuiStart` is **rpi-specific** (not a Pi category): dispatched once
/// before the interactive TUI is initialized, so extensions can prepare
/// (connect a server, load a resource) before the UI starts. Appended last so
/// existing discriminants keep their values (ABI-safe for already-compiled
/// plugins).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTag {
    ProjectTrust = 0,
    ResourcesDiscover = 1,
    SessionStart = 2,
    SessionInfoChanged = 3,
    SessionBeforeSwitch = 4,
    SessionBeforeFork = 5,
    SessionBeforeCompact = 6,
    SessionCompact = 7,
    SessionShutdown = 8,
    SessionBeforeTree = 9,
    SessionTree = 10,
    Context = 11,
    BeforeProviderRequest = 12,
    BeforeProviderHeaders = 13,
    AfterProviderResponse = 14,
    /// The start of a run, carrying the user prompt (`{"prompt", "imageCount",
    /// "sessionId", "host"}`). Fire-and-forget like every other tag: subscribe to
    /// *observe* the turn. To **change** the system prompt for the run, register a
    /// handler with `register_before_agent_start` — that slot carries an `out`
    /// pointer, which this fire-and-forget signature cannot.
    BeforeAgentStart = 15,
    AgentStart = 16,
    AgentEnd = 17,
    AgentSettled = 18,
    TurnStart = 19,
    TurnEnd = 20,
    MessageStart = 21,
    MessageUpdate = 22,
    MessageEnd = 23,
    ToolExecutionStart = 24,
    ToolExecutionUpdate = 25,
    ToolExecutionEnd = 26,
    ModelSelect = 27,
    ThinkingLevelSelect = 28,
    ToolCall = 29,
    ToolResult = 30,
    UserBash = 31,
    Input = 32,
    /// rpi-specific: dispatched before the interactive TUI initializes (not a
    /// Pi `on()` category). Appended last so existing discriminants are stable.
    BeforeTuiStart = 33,
    /// Pi `on()` category: dispatched when the TUI shows an interactive prompt
    /// (selector, dialog, etc.) that blocks agent work.
    UiPromptStart = 34,
    /// Pi `on()` category: dispatched when the TUI prompt is dismissed.
    UiPromptEnd = 35,
    /// rpi-specific (not a Pi `on()` category): dispatched whenever the
    /// interactive prompt editor's **text changes** — typing, pasting, deleting,
    /// or a programmatic rewrite. Appended last so existing discriminants are
    /// stable. The payload is `{"chars":N,"empty":bool}`; the draft text itself
    /// is deliberately not shipped, because subscribers (a voice extension
    /// barging in on playback) only need to know *that* the user is composing.
    EditorChange = 36,
}

/// Number of event categories — `37` (33 Pi `on()` categories + 1 rpi-specific
/// [`EventTag::BeforeTuiStart`] + 2 UI prompt events + 1 rpi-specific
/// [`EventTag::EditorChange`]). A test asserts
/// `EVENT_TAG_COUNT == 37` so a future edit that adds/removes a tag is caught.
pub const EVENT_TAG_COUNT: usize = 37;

/// The application name of the host that loads plugins: `"rpi"`.
///
/// This is the **contract's** copy of the host identity, and the host's
/// `APP_NAME` is defined in terms of it, so the two cannot drift. An extension
/// that labels its own output (a trace name, a tag, a `service.name`) should
/// derive from this rather than hardcode a brand — it has no other way to learn
/// what it is embedded in.
///
/// [`HOST_VERSION`] is the matching version. Both are also handed to plugins at
/// runtime in the `BeforeAgentStart` payload's `host` object, which is what an
/// extension should prefer: that value always describes the process actually
/// running, whereas these constants describe the host the plugin was *built*
/// against.
pub const HOST_NAME: &str = "rpi";

/// Version of the [`HOST_NAME`] host that this SDK ships with.
///
/// Because every workspace crate shares one version, this is the host's version
/// as of this SDK release — good for a user agent or a fallback label, and
/// superseded at runtime by the `host.version` field of the `BeforeAgentStart`
/// payload.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment variable carrying the id of the session this process serves.
///
/// The host **rewrites** it whenever a session becomes active — at startup and
/// on every in-process swap (`/import`, fork, switch) — so the value always
/// names the session currently being served. It is not an embedder override:
/// a value inherited from a parent process would otherwise make a child `rpi`
/// report its ancestor's session forever. Embedders that need to pin the id set
/// it after the session is active rather than before.
///
/// The id is a plain UTF-8 string (no NUL), which is what makes writing it with
/// `std::env::set_var` sound.
pub const SESSION_ID_ENV: &str = "RPI_SESSION_ID";

/// Environment variable a tracing extension publishes to name the trace a
/// *nested* `rpi` should attach to, and reads back to decide it is a subagent.
///
/// The name is `rpi-langfuse`'s (it owns the `LANGFUSE_` prefix), but the
/// variable is declared here because **three** parties have to agree on it and
/// they live in different crates, two of them in different repositories:
///
/// - the extension writes it (so a launcher can attach a nested run),
/// - the host's `bash`/`powershell` must *not* pass it on to a shell (or a plain
///   `rpi` started from one mistakes itself for a subagent), and
/// - a launcher may set it deliberately, which is the supported nesting path.
///
/// Declaring it once here is what keeps those three in step. The host builds its
/// exclusion list from these declarations, so dropping or renaming the family
/// ([`SUBAGENT_PARENT_ENV`]) is a compile error there rather than a silently
/// re-opened leak, and a newly added member is excluded without the host having
/// to be told. What the compiler still cannot check is the string itself, since
/// `rpi-langfuse` lives in another repository and matches on the *value* — that
/// is pinned by a test. See [`SUBAGENT_PARENT_ENV`] for the whole family.
pub const SUBAGENT_PARENT_TRACE_ID_ENV: &str = "LANGFUSE_PI_PARENT_TRACE_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. The parent turn's root observation id.
pub const SUBAGENT_PARENT_SPAN_ID_ENV: &str = "LANGFUSE_PI_PARENT_SPAN_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. The parent's session id.
pub const SUBAGENT_PARENT_SESSION_ID_ENV: &str = "LANGFUSE_PI_PARENT_SESSION_ID";

/// See [`SUBAGENT_PARENT_TRACE_ID_ENV`]. Nesting depth, so a grandchild knows
/// how far down it is.
pub const SUBAGENT_PARENT_DEPTH_ENV: &str = "LANGFUSE_PI_PARENT_DEPTH";

/// Every [`SUBAGENT_PARENT_TRACE_ID_ENV`] sibling, as one slice.
///
/// A host that excludes this channel from a spawned shell should cover the whole
/// slice rather than restating the names, and assert it does — a new entry here
/// that is not excluded is the failure mode this array exists to make testable.
pub const SUBAGENT_PARENT_ENV: &[&str] = &[
    SUBAGENT_PARENT_TRACE_ID_ENV,
    SUBAGENT_PARENT_SPAN_ID_ENV,
    SUBAGENT_PARENT_SESSION_ID_ENV,
    SUBAGENT_PARENT_DEPTH_ENV,
];

/// No-payload marker for events that carry none (e.g. `session_shutdown`).
/// Carries a dummy byte so the empty-struct isn't flagged FFI-unsafe by
/// `improper_ctypes` (zero-sized C structs are rejected regardless of `repr(C)`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventEmpty {
    _opaque: u8,
}

impl EventEmpty {
    /// The one no-payload instance.
    pub const INSTANCE: EventEmpty = EventEmpty { _opaque: 0 };
}

impl Default for EventEmpty {
    fn default() -> Self {
        Self::INSTANCE
    }
}

/// A serialized message payload (`message_start`/`update`/`end`, tool-result
/// messages). `message` is a JSON `AgentMessage`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventMessage {
    pub message: StbString,
}

/// A tool-call payload (`tool_call`, `tool_execution_start`/`update`).
/// `tool_call_id` + `tool_name` are raw strings; `params` is the JSON args.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventToolCall {
    pub tool_call_id: StbString,
    pub tool_name: StbString,
    pub params: StbString,
}

/// A tool-result payload (`tool_result`, `tool_execution_end`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventToolResult {
    pub tool_call_id: StbString,
    pub tool_name: StbString,
    pub result: StbString,
    pub is_error: u8,
}

/// An error/failure payload.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventError {
    pub message: StbString,
}

/// A generic JSON-data payload for the long-tail events whose structured shape
/// the host serializes wholesale (`context`, `before_provider_request`, model
/// select, resources_discover response, etc.). The plugin reads the fields it
/// needs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventData {
    pub data: StbString,
}

/// Payload union for [`StablePluginEvent`]. All variants are `#[repr(C)]` structs
/// of [`StbString`] / primitives (Copy), so the union is Copy. Discriminate by
/// [`StablePluginEvent::tag`] before reading.
#[repr(C)]
#[derive(Clone, Copy)]
pub union EventPayload {
    pub empty: EventEmpty,
    pub message: EventMessage,
    pub tool_call: EventToolCall,
    pub tool_result: EventToolResult,
    pub error: EventError,
    pub data: EventData,
}

/// One event dispatched to a plugin handler. The host translates its native
/// `AgentEvent` / `HarnessEvent` into this and calls every registered handler
/// for the `tag` (dispatch wrapped in `catch_unwind`). Ownership of the
/// [`StbString`]s passes to the handler; the handler frees them via the host's
/// `free_string`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StablePluginEvent {
    pub tag: EventTag,
    pub payload: EventPayload,
}

impl StablePluginEvent {
    /// Build a no-payload event.
    pub fn empty(tag: EventTag) -> Self {
        Self {
            tag,
            payload: EventPayload {
                empty: EventEmpty::INSTANCE,
            },
        }
    }

    /// Build a message event.
    pub fn message(tag: EventTag, message: StbString) -> Self {
        debug_assert!(matches!(
            tag,
            EventTag::MessageStart | EventTag::MessageUpdate | EventTag::MessageEnd
        ));
        Self {
            tag,
            payload: EventPayload {
                message: EventMessage { message },
            },
        }
    }

    /// Build a tool-call event.
    pub fn tool_call(
        tag: EventTag,
        tool_call_id: StbString,
        tool_name: StbString,
        params: StbString,
    ) -> Self {
        debug_assert!(matches!(
            tag,
            EventTag::ToolCall | EventTag::ToolExecutionStart | EventTag::ToolExecutionUpdate
        ));
        Self {
            tag,
            payload: EventPayload {
                tool_call: EventToolCall {
                    tool_call_id,
                    tool_name,
                    params,
                },
            },
        }
    }

    /// Build a tool-result event.
    pub fn tool_result(
        tag: EventTag,
        tool_call_id: StbString,
        tool_name: StbString,
        result: StbString,
        is_error: bool,
    ) -> Self {
        debug_assert!(matches!(
            tag,
            EventTag::ToolResult | EventTag::ToolExecutionEnd
        ));
        Self {
            tag,
            payload: EventPayload {
                tool_result: EventToolResult {
                    tool_call_id,
                    tool_name,
                    result,
                    is_error: is_error as u8,
                },
            },
        }
    }

    /// Build an error event.
    pub fn error(tag: EventTag, message: StbString) -> Self {
        Self {
            tag,
            payload: EventPayload {
                error: EventError { message },
            },
        }
    }

    /// Build a generic data event (JSON in `data`).
    pub fn data(tag: EventTag, data: StbString) -> Self {
        Self {
            tag,
            payload: EventPayload {
                data: EventData { data },
            },
        }
    }
}

/// Return codes an [`EventHandlerFn`] may return.
///
/// `0` means success; the host continues to the next handler. A **nonzero**
/// return means the handler handled an error — the host logs it and (for the
/// ordinary agent/observe fan-out) continues to other handlers (one handler's
/// error does not abort the fan-out, mirroring pi's per-handler try/catch).
///
/// [`EVENT_HANDLER_ABORT`] is the **veto** code: the handler wants to stop the
/// current lifecycle phase. It is honored only by the host's lifecycle
/// dispatch (e.g. `before_tui_start` / `session_start` / `session_shutdown`);
/// the ordinary observe fan-out treats it like any other nonzero (log +
/// continue), because the agent loop must not be vetoed mid-run.
pub const EVENT_HANDLER_CONTINUE: i32 = 0;
/// The handler handled an error but the fan-out should continue (default
/// nonzero semantics — equivalent to any other nonzero code).
pub const EVENT_HANDLER_ERROR: i32 = 1;
/// **Veto**: stop the current lifecycle phase. Honored only by the host's
/// lifecycle dispatch; the observe fan-out logs + continues (see the module
/// docs on [`EVENT_HANDLER_CONTINUE`]).
pub const EVENT_HANDLER_ABORT: i32 = 2;
/// **Claim**: the handler consumed this event and the host should not fall back
/// to its own default handling. Honored only by dispatch paths that ask
/// extensions to arbitrate (currently `EventTag::Input` key routing, via
/// [`EVENT_HANDLER_CLAIMED`]); the ordinary observe fan-out treats it like any
/// other nonzero (log + continue).
///
/// Lets a handler subscribe to a key yet decline it while its feature is off:
/// return [`EVENT_HANDLER_CONTINUE`] to pass the key through to normal editor
/// handling, or this to swallow it.
pub const EVENT_HANDLER_CLAIMED: i32 = 3;

/// Handler fn pointer registered via `register_event_handler(tag, handler)`.
/// `user_data` is the plugin's opaque context. Return `0` on success; nonzero
/// signals a handled error (the host logs it; dispatch continues to other
/// handlers — one handler's error does not abort the fan-out). The one
/// exception: when the host dispatches a **lifecycle** event through its
/// veto-aware path, returning [`EVENT_HANDLER_ABORT`] stops the fan-out and
/// aborts that phase (see the constants above).
pub type EventHandlerFn = extern "C" fn(event: StablePluginEvent, user_data: *mut c_void) -> i32;

/// `resources_discover` handler signature (B5b). Unlike [`EventHandlerFn`]
/// (fire-and-forget, `i32` only), this carries an owning `out` so the plugin
/// can hand `{skillPaths, promptPaths, themePaths}` back to the host. `cwd` and
/// `reason` are borrowed inputs ([`StbStringRef`]); `out` is plugin-produced
/// and reclaimed via the `plugin_free_string` the host stored alongside the
/// handler at registration. `user_data` is the plugin's opaque context. Returns
/// `0` on success (host reads `out`); nonzero on a handled error (host logs +
/// skips this handler, fan-out continues — mirrors pi `runner.ts:1179-1188`).
pub type ResourcesDiscoverFn = extern "C" fn(
    cwd: StbStringRef,
    reason: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32;

/// `before_agent_start` handler signature. Unlike [`EventHandlerFn`]
/// (fire-and-forget, `i32` only), this carries an owning `out` so the plugin can
/// hand back a system prompt to install for the run — pi's
/// `BeforeAgentStartEventResult` (`extensions/types.ts:1394`), whose
/// `systemPrompt` becomes `forceSystemPrompt` (`runner.ts:1347`).
///
/// `event_json` is a borrowed envelope with the same fields the
/// [`EventTag::BeforeAgentStart`] broadcast carries (`prompt`, `imageCount`,
/// `sessionId`, `host`) plus the live `systemPrompt` the run would otherwise
/// use. `out` is plugin-produced JSON `{"systemPrompt": "..."}` (or an empty
/// object for "no change"), reclaimed via the `plugin_free_string` the host
/// stored at registration.
///
/// Returns `0` on success (host reads `out`); nonzero on a handled error (host
/// logs the plugin's message and leaves the prompt untouched, then continues the
/// fan-out — mirrors pi, which isolates a throwing handler).
///
/// **Replacement, not append**: the returned text becomes the system prompt. A
/// plugin that wants to append reads `event_json.systemPrompt` and returns the
/// two concatenated, which is what pi requires of its extensions too.
/// SAFETY: the plugin warrants `user_data` is valid for the registry's lifetime
/// and the handler is callable from any thread.
pub type BeforeAgentStartFn =
    extern "C" fn(event_json: StbStringRef, out: *mut StbString, user_data: *mut c_void) -> i32;

// ---------------------------------------------------------------------------
// Runtime actions — uniform JSON-RPC dispatch by RuntimeActionId
// ---------------------------------------------------------------------------

/// Identifier for a host runtime action the plugin may invoke via
/// `PluginApi::runtime_action`. One slot dispatches all actions; the numeric
/// id crosses the FFI boundary as a `u32` and the host validates it with
/// [`TryFrom<u32>`] before constructing this enum. Args/results cross as JSON
/// strings.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeActionId {
    SendMessage = 0,
    SendUserMessage = 1,
    AppendEntry = 2,
    SetSessionName = 3,
    GetActiveTools = 4,
    SetActiveTools = 5,
    SetModel = 6,
    GetThinkingLevel = 7,
    SetThinkingLevel = 8,
    Compact = 9,
    GetSystemPrompt = 10,
    NewSession = 11,
    Fork = 12,
    NavigateTree = 13,
    SwitchSession = 14,
    Reload = 15,
    /// Read a parsed CLI flag by name. Args: `{"name":"flag"}`; result:
    /// `{"value": <bool|string|null>}`.
    GetCliFlag = 16,
    /// Open/poll/cancel a host-provided interactive UI request. The payload is
    /// a JSON object with `op` (`open`, `poll`, or `cancel`) and a unique
    /// `requestId`; hosts without an attached TUI return an explicit error.
    UiDialog = 17,
    /// Publish a short status string for the host's UI (TUI footer).
    /// Args: `{"key":"langfuse","value":"langfuse ✓ (trace sent)"}`; an
    /// empty/absent `value` clears that key. Result: `{"ok":true,"changed":bool}`.
    /// Headless hosts accept the write but have nothing to render.
    SetStatus = 18,
    /// Put text into the interactive editor (the prompt input box) instead of
    /// sending it. Args: `{"text":"...", "mode":"append"|"replace",
    /// "autoSendMs": 3000}` (omit/`0` for manual send only). Result:
    /// `{"ok":true,"chars":N}`. A headless host has no editor and accepts the
    /// write without rendering it, mirroring [`RuntimeActionId::SetStatus`].
    SetEditorText = 19,
}

/// Error returned when a plugin passes a numeric runtime-action id that this
/// ABI does not define.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownRuntimeActionId(pub u32);

impl core::fmt::Display for UnknownRuntimeActionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unknown runtime action id {}", self.0)
    }
}

impl std::error::Error for UnknownRuntimeActionId {}

impl TryFrom<u32> for RuntimeActionId {
    type Error = UnknownRuntimeActionId;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::SendMessage),
            1 => Ok(Self::SendUserMessage),
            2 => Ok(Self::AppendEntry),
            3 => Ok(Self::SetSessionName),
            4 => Ok(Self::GetActiveTools),
            5 => Ok(Self::SetActiveTools),
            6 => Ok(Self::SetModel),
            7 => Ok(Self::GetThinkingLevel),
            8 => Ok(Self::SetThinkingLevel),
            9 => Ok(Self::Compact),
            10 => Ok(Self::GetSystemPrompt),
            11 => Ok(Self::NewSession),
            12 => Ok(Self::Fork),
            13 => Ok(Self::NavigateTree),
            14 => Ok(Self::SwitchSession),
            15 => Ok(Self::Reload),
            16 => Ok(Self::GetCliFlag),
            17 => Ok(Self::UiDialog),
            18 => Ok(Self::SetStatus),
            19 => Ok(Self::SetEditorText),
            other => Err(UnknownRuntimeActionId(other)),
        }
    }
}

impl From<RuntimeActionId> for u32 {
    fn from(value: RuntimeActionId) -> Self {
        value as u32
    }
}

/// Runtime-action signature: `runtime_action(action_id, args_json, out,
/// user_data) -> i32`. `args_json` is a borrowed input ([`StbStringRef`]); `out`
/// is an owning output ([`StbString`]) the host produces and the plugin frees
/// via the host's `free_string`. `action_id` is deliberately a raw `u32`, not a
/// Rust enum: an unknown value must be rejected as a normal protocol error
/// rather than materializing an invalid enum discriminant. Returns `0` on
/// success, nonzero on error.
pub type RuntimeActionFn = extern "C" fn(
    action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32;

// ---------------------------------------------------------------------------
// PluginApi — host-provided API struct the plugin calls
// ---------------------------------------------------------------------------

/// A generic command-handler fn (for `register_command`). `args_json` is a
/// borrowed `{"args":"...","command":"/..."}` envelope; `out` is owning
/// JSON output reclaimed with the host `free_string`. The TUI understands
/// `{kind:"message",text}`, `{kind:"selector",items:[...]}`,
/// `{kind:"editor",initialText}`, and `{kind:"input",title,placeholder}`
/// responses; selector/editor/input submissions call the same handler with an
/// `action` field in `args`.
pub type CommandHandlerFn =
    extern "C" fn(args_json: StbStringRef, out: *mut StbString, user_data: *mut c_void) -> i32;

/// A render/transform fn (for the renderer registrars). `input_json` is borrowed;
/// `out` is owning output the plugin frees via host `free_string`. Markdown
/// handlers return `{markdown:"..."}`; message/entry handlers return
/// `{text:"...",markdown?:true}` or `{lines:["..."]}` for terminal UI.
pub type RenderFn =
    extern "C" fn(input_json: StbStringRef, out: *mut StbString, user_data: *mut c_void) -> i32;

/// A provider-injection factory fn (for `register_provider`). `req_json` is a
/// borrowed request envelope; `out` is an owning response the plugin frees.
/// The host wraps this into a `Provider` impl (B4/B5).
pub type ProviderRequestFn =
    extern "C" fn(req_json: StbStringRef, out: *mut StbString, user_data: *mut c_void) -> i32;

// ---------------------------------------------------------------------------
// Unified ABI — single struct, version inside
// ---------------------------------------------------------------------------

/// The unified ABI version.
pub const RPI_PLUGIN_ABI_VERSION_UNIFIED: u32 = 4;

/// The unversioned symbol for the unified ABI.
pub const REGISTER_SYMBOL_UNIFIED: &[u8] = b"rpi_plugin_register\0";

/// The host-provided API struct. The first two fields (`abi_version`
/// and `struct_size`) are contract-identity markers that survive future
/// signature changes: a plugin reads them from the pointer before anything
/// else, so even if the entrypoint arity changes, the version check remains
/// robust.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginApi {
    /// Contract identity. First field so a plugin can validate before reading
    /// anything else, independent of the entrypoint arity.
    pub abi_version: u32,
    /// `sizeof(PluginApi)` as the host built it. Lets a plugin detect a host
    /// that predates slots it wants (host_struct_size < plugin's expected
    /// offset). Future hosts may grow this struct; old plugins check
    /// `struct_size` before accessing new fields.
    pub struct_size: u32,

    // --- Host API ---
    /// Host's `free_string` — the plugin calls this for every [`StbString`] it
    /// *receives* from the host (outputs of actions, event payloads, inputs to
    /// execute). Never null.
    pub free_string: FreeStringFn,

    // --- registrars (plugin → host "register X into the host") ---
    pub register_tool: Option<
        extern "C" fn(
            schema: *const StableToolSchema,
            execute_fn: ToolExecuteFn,
            poll_fn: ToolPollFn,
            cancel_fn: ToolCancelFn,
            destroy_fn: ToolDestroyFn,
            plugin_free_string: FreeStringFn,
        ) -> i32,
    >,
    pub register_command: Option<
        extern "C" fn(
            name: StbStringRef,
            description: StbStringRef,
            handler: CommandHandlerFn,
        ) -> i32,
    >,
    pub register_shortcut:
        Option<extern "C" fn(key: StbStringRef, description: StbStringRef) -> i32>,
    pub register_flag: Option<extern "C" fn(name: StbStringRef, description: StbStringRef) -> i32>,
    pub register_provider: Option<
        extern "C" fn(
            provider_id: StbStringRef,
            base_url: StbStringRef,
            api_style: StbStringRef,
            request_fn: ProviderRequestFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
    pub register_message_renderer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
    pub register_markdown_transformer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
    pub register_entry_renderer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
    pub register_event_handler: Option<
        extern "C" fn(tag: EventTag, handler: EventHandlerFn, user_data: *mut c_void) -> i32,
    >,
    pub register_resources_discover: Option<
        extern "C" fn(
            handler: ResourcesDiscoverFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
    pub runtime_action: RuntimeActionFn,
    pub dispatch_event:
        Option<extern "C" fn(event: StablePluginEvent, user_data: *mut c_void) -> i32>,
    pub user_data: *mut c_void,

    // --- declarations ---
    /// Declare this plugin's host-side preferences as a JSON object, e.g.
    /// `{"priority":60,"platforms":["linux","macos"]}`. Nullable: host does
    /// not support declarations yet (plugin skips the call; host applies
    /// defaults: priority `100`, all platforms).
    pub declare: Option<extern "C" fn(json: StbStringRef) -> i32>,

    /// Register a system-prompt transformer for the run (pi's
    /// `before_agent_start` returning `systemPrompt`).
    ///
    /// **Appended last on purpose.** A plugin compiled against a shorter
    /// `PluginApi` indexes every earlier field by offset, so a slot inserted in
    /// the middle would make an old plugin read the new host's fields at the
    /// wrong offsets. Appending keeps every existing offset valid, which is what
    /// lets this ship without an ABI version bump; a plugin checks
    /// `struct_size` (or the slot for `None`) before using it.
    ///
    /// Nullable: a host that predates this slot leaves it `None`, the plugin
    /// skips the call, and the prompt is left alone — exactly that host's
    /// behavior before this slot existed.
    pub register_before_agent_start: Option<
        extern "C" fn(
            handler: BeforeAgentStartFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,
}
unsafe impl Send for PluginApi {}
unsafe impl Sync for PluginApi {}

/// Unified ABI plugin entrypoint signature: `(api)`. The version is read from
/// `api->abi_version`.
pub type RpiPluginRegisterUnified = extern "C" fn(api: *const PluginApi) -> i32;

// ===========================================================================
// Panic containment
// ===========================================================================

/// Status code a plugin's register entrypoint returns when its body panicked.
///
/// A panic must never unwind past the plugin's `extern "C"` entrypoint: Rust
/// turns such an unwind into a **process abort** (`panic in a function that
/// cannot unwind`), which would take down the whole host. The SDK catches the
/// panic and returns this code instead, so the host can skip the plugin with a
/// diagnostic.
pub const REGISTER_PANIC_STATUS: i32 = 70;

/// Run `body`, converting a panic into [`Err`] instead of letting it unwind.
///
/// Plugin authors must wrap the body of **every** `extern "C"` entrypoint they
/// export (tool `execute`/`poll`/`cancel`, event handlers, provider/render
/// callbacks, runtime actions) with this (or an equivalent `catch_unwind`).
/// Unwinding out of an `extern "C"` function aborts the process.
pub fn guard<R>(body: impl FnOnce() -> R) -> Result<R, Box<dyn std::any::Any + Send>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
}

/// Like [`guard`], but returns `fallback` on panic. Handy for entrypoints that
/// must return a value rather than a `Result`.
pub fn guard_or<R>(fallback: R, body: impl FnOnce() -> R) -> R {
    guard(body).unwrap_or(fallback)
}

/// Export a unified ABI plugin entrypoint under `rpi_plugin_register`.
///
/// The unified ABI has no version suffix in the symbol name; the version lives
/// inside the [`PluginApi`] struct (first field `abi_version`).
///
/// The expression receives `&PluginApi` and returns the plugin-defined
/// registration status code.
///
/// ```ignore
/// rpi_plugin_sdk::export_plugin!(|api| {
///     if let Some(declare) = api.declare {
///         declare(rpi_plugin_sdk::StbStringRef::from_str(
///             r#"{"priority":60,"platforms":["linux"]}"#,
///         ));
///     }
///     // ... register tools and handlers through `api` ...
///     0
/// });
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($body:expr) => {
        #[no_mangle]
        pub extern "C" fn rpi_plugin_register(api: *const $crate::PluginApi) -> i32 {
            // SAFETY: host guarantees `api` is valid for this call.
            unsafe { $crate::register_entrypoint(api, $body) }
        }
    };
}

/// Unified ABI entrypoint helper: reads `abi_version` and `struct_size` from
/// the struct pointer before dereferencing anything else. This survives future
/// signature changes because the pointer is always the first argument.
///
/// # Safety
///
/// `api` must be a valid, properly aligned pointer to a [`PluginApi`] that
/// remains valid for the duration of `body`.
pub unsafe fn register_entrypoint(
    api: *const PluginApi,
    body: impl FnOnce(&PluginApi) -> i32,
) -> i32 {
    if api.is_null() {
        return 2;
    }
    // SAFETY: caller guarantees `api` is valid; we check null above.
    // Read the version and size fields first — they're at known offsets
    // regardless of future struct growth.
    let abi_version = (*api).abi_version;
    let struct_size = (*api).struct_size;
    if abi_version != RPI_PLUGIN_ABI_VERSION_UNIFIED {
        // Mismatch: refuse to register. The host logs "ABI version mismatch"
        // and skips loading this plugin.
        return 1;
    }
    if (struct_size as usize) < core::mem::size_of::<PluginApi>() {
        // Host predates a field the plugin expects; degrade gracefully.
        return 3;
    }
    let api = &*api;
    // Contain panics: unwinding out of the plugin's `extern "C"` shim aborts
    // the process, so convert a panic into a status code the host can report.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(api))) {
        Ok(code) => code,
        Err(_) => REGISTER_PANIC_STATUS,
    }
}

// ===========================================================================
// Tests (need std + serde_json)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A panicking `register` body must NOT unwind out of the entrypoint shim;
    /// it must come back as `REGISTER_PANIC_STATUS` so the host can skip the
    /// plugin instead of the process aborting.
    #[test]
    fn register_entrypoint_contains_panics() {
        let mut api = std::mem::MaybeUninit::<PluginApi>::uninit();
        let api_ptr = api.as_mut_ptr();
        // Set the version/size fields so the entrypoint gets past the checks
        // and into the body (which panics).
        unsafe {
            (*api_ptr).abi_version = RPI_PLUGIN_ABI_VERSION_UNIFIED;
            (*api_ptr).struct_size = core::mem::size_of::<PluginApi>() as u32;
        }
        let rc = unsafe {
            register_entrypoint(api_ptr, |_api| {
                panic!("plugin register exploded");
            })
        };
        assert_eq!(rc, REGISTER_PANIC_STATUS);
    }

    #[test]
    fn guard_returns_value_and_contains_panic() {
        assert_eq!(guard(|| 41 + 1).ok(), Some(42));
        assert!(guard(|| -> i32 { panic!("nope") }).is_err());
        assert_eq!(guard_or(-1, || -> i32 { panic!("nope") }), -1);
    }

    // A test allocator + free fn so we can verify the own/free contract
    // without a real plugin's free_string.
    std::thread_local! {
        static FREED: std::cell::Cell<usize> = std::cell::Cell::new(0);
    }

    extern "C" fn test_free(s: StbString) {
        if s.is_empty() || s.ptr.is_null() {
            return;
        }
        // Reconstruct the boxed slice and drop it.
        unsafe {
            let slice = core::slice::from_raw_parts_mut(s.ptr as *mut u8, s.len);
            let _ = Box::from_raw(slice as *mut [u8]);
        }
        FREED.with(|freed| freed.set(freed.get() + 1));
    }

    fn reset_freed() -> usize {
        FREED.with(|freed| freed.replace(0))
    }

    fn freed_count() -> usize {
        FREED.with(std::cell::Cell::get)
    }

    // A no-op `runtime_action` impl for the vtable-construction tests (closures
    // can't coerce to `extern "C" fn`, so we use a real fn).
    extern "C" fn noop_runtime_action(
        _action_id: u32,
        _args: StbStringRef,
        _out: *mut StbString,
        _user_data: *mut c_void,
    ) -> i32 {
        0
    }

    #[test]
    fn stbstring_round_trip_and_free_once() {
        let prev = reset_freed();
        let _ = prev;
        let s = StbString::from_string("hello, pi".to_string());
        assert_eq!(s.len, 9);
        assert_eq!(s.to_string_lossy(), "hello, pi");
        s.free_with(Some(test_free));
        assert_eq!(freed_count(), 1);
    }

    #[test]
    fn empty_stbstring_free_is_noop() {
        let _ = reset_freed();
        StbString::empty().free_with(Some(test_free));
        assert_eq!(freed_count(), 0);
    }

    #[test]
    fn json_round_trip_preserves_structure() {
        let val = serde_json::json!({ "name": "echo", "args": [1, 2, 3], "ok": true });
        let stb = json::to_stable(&val, None);
        let back = json::from_stable(&stb);
        assert_eq!(val, back);
        stb.free_with(Some(test_free));
        let _ = reset_freed();
    }

    #[test]
    fn step_result_done_round_trip() {
        let result_json = StbString::from_string(r#"{"content":[{"text":"hi"}]}"#.to_string());
        let sr = StepResult::done(result_json);
        assert_eq!(sr.tag, StepResultTag::Done);
        // SAFETY: tag == Done.
        let done = unsafe { sr.done_payload() };
        assert_eq!(
            done.result.to_string_lossy(),
            r#"{"content":[{"text":"hi"}]}"#
        );
        done.result.free_with(Some(test_free));
        let _ = reset_freed();
    }

    #[test]
    fn step_result_pending_and_err() {
        let prog = StbString::from_string("...".to_string());
        let srp = StepResult::pending(prog);
        assert_eq!(srp.tag, StepResultTag::Pending);
        // SAFETY: tag == Pending.
        unsafe {
            assert_eq!(srp.pending_payload().progress.to_string_lossy(), "...");
        }
        unsafe { srp.pending_payload().progress.free_with(Some(test_free)) };

        let msg = StbString::from_string("boom".to_string());
        let sre = StepResult::err(msg);
        assert_eq!(sre.tag, StepResultTag::Err);
        // SAFETY: tag == Err.
        unsafe {
            assert_eq!(sre.err_payload().message.to_string_lossy(), "boom");
            sre.err_payload().message.free_with(Some(test_free));
        }
        let _ = reset_freed();
    }

    #[test]
    fn event_tag_count_is_37() {
        // Enumerate every tag; a compile-time + runtime guarantee that the
        // 37-category surface is intact (33 Pi on() categories + the
        // rpi-specific BeforeTuiStart + 2 UI prompt events + EditorChange).
        let tags = [
            EventTag::ProjectTrust,
            EventTag::ResourcesDiscover,
            EventTag::SessionStart,
            EventTag::SessionInfoChanged,
            EventTag::SessionBeforeSwitch,
            EventTag::SessionBeforeFork,
            EventTag::SessionBeforeCompact,
            EventTag::SessionCompact,
            EventTag::SessionShutdown,
            EventTag::SessionBeforeTree,
            EventTag::SessionTree,
            EventTag::Context,
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
            EventTag::BeforeAgentStart,
            EventTag::AgentStart,
            EventTag::AgentEnd,
            EventTag::AgentSettled,
            EventTag::TurnStart,
            EventTag::TurnEnd,
            EventTag::MessageStart,
            EventTag::MessageUpdate,
            EventTag::MessageEnd,
            EventTag::ToolExecutionStart,
            EventTag::ToolExecutionUpdate,
            EventTag::ToolExecutionEnd,
            EventTag::ModelSelect,
            EventTag::ThinkingLevelSelect,
            EventTag::ToolCall,
            EventTag::ToolResult,
            EventTag::UserBash,
            EventTag::Input,
            EventTag::BeforeTuiStart,
            EventTag::UiPromptStart,
            EventTag::UiPromptEnd,
            EventTag::EditorChange,
        ];
        assert_eq!(tags.len(), EVENT_TAG_COUNT);
        assert_eq!(EVENT_TAG_COUNT, 37);
        // Distinct discriminants 0..36.
        let mut discs: Vec<u32> = tags.iter().map(|t| *t as u32).collect();
        discs.sort();
        assert_eq!(discs, (0..37).collect::<Vec<u32>>());
    }

    #[test]
    fn event_payloads_construct_and_free() {
        let m = StbString::from_string("msg".to_string());
        let ev = StablePluginEvent::message(EventTag::MessageEnd, m);
        assert_eq!(ev.tag, EventTag::MessageEnd);
        // SAFETY: tag == MessageEnd (message variant).
        unsafe {
            assert_eq!(ev.payload.message.message.to_string_lossy(), "msg");
            ev.payload.message.message.free_with(Some(test_free));
        }

        let tc = StablePluginEvent::tool_call(
            EventTag::ToolCall,
            StbString::from_string("call_1".to_string()),
            StbString::from_string("echo".to_string()),
            StbString::from_string("{}".to_string()),
        );
        // SAFETY: tag == ToolCall (tool_call variant).
        unsafe {
            assert_eq!(tc.payload.tool_call.tool_name.to_string_lossy(), "echo");
            tc.payload.tool_call.tool_call_id.free_with(Some(test_free));
            tc.payload.tool_call.tool_name.free_with(Some(test_free));
            tc.payload.tool_call.params.free_with(Some(test_free));
        }
        let _ = reset_freed();
    }

    #[test]
    fn plugin_api_is_pod_and_sized() {
        // The API struct is a plain old data struct: every fn pointer is
        // non-Drop, the struct has no Drop impl.
        let api = PluginApi {
            abi_version: RPI_PLUGIN_ABI_VERSION_UNIFIED,
            struct_size: core::mem::size_of::<PluginApi>() as u32,
            free_string: test_free,
            register_tool: None,
            register_command: None,
            register_shortcut: None,
            register_flag: None,
            register_provider: None,
            register_message_renderer: None,
            register_markdown_transformer: None,
            register_entry_renderer: None,
            register_event_handler: None,
            register_resources_discover: None,
            runtime_action: noop_runtime_action,
            dispatch_event: None,
            user_data: core::ptr::null_mut(),
            declare: None,
            register_before_agent_start: None,
        };
        // All optional slots are null → plugin must degrade.
        assert!(api.register_tool.is_none());
        assert!(api.register_event_handler.is_none());
        assert!(api.register_resources_discover.is_none());
        assert!(api.register_before_agent_start.is_none());
        assert!(api.register_before_agent_start.is_none());
        // Copy (POD) — no UB from a plain copy.
        let _copy = api;
        assert!(!core::mem::needs_drop::<PluginApi>());
        assert!(!core::mem::needs_drop::<StbString>());
        assert!(!core::mem::needs_drop::<StepResult>());
        assert!(!core::mem::needs_drop::<StablePluginEvent>());
        assert!(!core::mem::needs_drop::<StableToolSchema>());
    }

    /// New vtable slots must be **appended**, never inserted.
    ///
    /// A plugin compiled against a shorter `PluginApi` reads every field by
    /// offset. Inserting a slot in the middle would make that plugin read the
    /// new host's fields at the wrong offsets — silently, since both sides just
    /// see pointers. Appending is what lets a slot ship without an ABI version
    /// bump. This pins the three fields at the end and the identity markers at
    /// the front, so a future edit that inserts in the middle fails here.
    #[test]
    fn vtable_grows_only_at_the_end() {
        use core::mem::{align_of, size_of};

        // Identity markers stay first: a plugin reads these before anything
        // else, independent of the entrypoint arity.
        assert_eq!(core::mem::offset_of!(PluginApi, abi_version), 0);
        assert_eq!(
            core::mem::offset_of!(PluginApi, struct_size),
            size_of::<u32>()
        );

        // The most recently added slot is last, and the two before it are
        // adjacent — i.e. nothing was wedged in between them.
        let declare = core::mem::offset_of!(PluginApi, declare);
        let new_slot = core::mem::offset_of!(PluginApi, register_before_agent_start);
        assert_eq!(
            new_slot,
            declare + size_of::<Option<extern "C" fn(StbStringRef) -> i32>>(),
            "register_before_agent_start must be appended right after `declare`; \
             inserting a slot earlier shifts every later offset and breaks \
             plugins built against the previous layout"
        );
        assert_eq!(align_of::<PluginApi>(), align_of::<*const ()>());
    }

    #[test]
    fn runtime_action_ids_are_explicitly_validated() {
        let ids = [
            RuntimeActionId::SendMessage,
            RuntimeActionId::SendUserMessage,
            RuntimeActionId::AppendEntry,
            RuntimeActionId::SetSessionName,
            RuntimeActionId::GetActiveTools,
            RuntimeActionId::SetActiveTools,
            RuntimeActionId::SetModel,
            RuntimeActionId::GetThinkingLevel,
            RuntimeActionId::SetThinkingLevel,
            RuntimeActionId::Compact,
            RuntimeActionId::GetSystemPrompt,
            RuntimeActionId::NewSession,
            RuntimeActionId::Fork,
            RuntimeActionId::NavigateTree,
            RuntimeActionId::SwitchSession,
            RuntimeActionId::Reload,
            RuntimeActionId::GetCliFlag,
            RuntimeActionId::UiDialog,
            RuntimeActionId::SetStatus,
            RuntimeActionId::SetEditorText,
        ];

        for (raw, expected) in ids.into_iter().enumerate() {
            assert_eq!(RuntimeActionId::try_from(raw as u32), Ok(expected));
            assert_eq!(u32::from(expected), raw as u32);
        }
        // One past the highest defined id is rejected (nothing is silently
        // accepted), and so is the u32 ceiling.
        assert_eq!(
            RuntimeActionId::try_from(20),
            Err(UnknownRuntimeActionId(20))
        );
        assert_eq!(
            RuntimeActionId::try_from(u32::MAX),
            Err(UnknownRuntimeActionId(u32::MAX))
        );
    }

    #[test]
    fn register_entrypoint_version_mismatch_refuses() {
        let api = PluginApi {
            abi_version: RPI_PLUGIN_ABI_VERSION_UNIFIED,
            struct_size: core::mem::size_of::<PluginApi>() as u32,
            free_string: test_free,
            register_tool: None,
            register_command: None,
            register_shortcut: None,
            register_flag: None,
            register_provider: None,
            register_message_renderer: None,
            register_markdown_transformer: None,
            register_entry_renderer: None,
            register_event_handler: None,
            register_resources_discover: None,
            runtime_action: noop_runtime_action,
            dispatch_event: None,
            user_data: core::ptr::null_mut(),
            declare: None,
            register_before_agent_start: None,
        };
        assert_eq!(RPI_PLUGIN_ABI_VERSION_UNIFIED, 4);

        // A future version is rejected by the pre-dereference check.
        let mut future_api = api;
        future_api.abi_version = RPI_PLUGIN_ABI_VERSION_UNIFIED + 1;
        let rc = unsafe {
            register_entrypoint(&future_api, |_| {
                panic!("body must not run on version mismatch");
            })
        };
        assert_eq!(rc, 1);

        // Right version → body runs, rc propagated.
        let rc = unsafe { register_entrypoint(&api, |_| 0) };
        assert_eq!(rc, 0);
        let rc = unsafe { register_entrypoint(&api, |_| 42) };
        assert_eq!(rc, 42);

        // Null api → refuse.
        let rc = unsafe { register_entrypoint(core::ptr::null(), |_| 0) };
        assert_ne!(rc, 0);
        let _ = reset_freed();
    }
}
