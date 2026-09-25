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
/// plugin calls the **host's** `free_string` (from [`PluginApiVt`]) for every
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
}

/// Number of event categories — `36` (33 Pi `on()` categories + 1 rpi-specific
/// [`EventTag::BeforeTuiStart`] + 2 UI prompt events). A test asserts
/// `EVENT_TAG_COUNT == 36` so a future edit that adds/removes a tag is caught.
pub const EVENT_TAG_COUNT: usize = 36;

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

// ---------------------------------------------------------------------------
// Runtime actions — uniform JSON-RPC dispatch by RuntimeActionId
// ---------------------------------------------------------------------------

/// Identifier for a host runtime action the plugin may invoke via
/// `PluginApiVt::runtime_action`. One slot dispatches all actions; the numeric
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
// PluginApiVt — host-provided vtable of fn pointers the plugin calls
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

/// ABI v1 runtime-action signature. The original SDK exposed a `#[repr(u32)]`
/// enum at this position; the legacy host view uses the ABI-equivalent raw
/// integer so it can reject unknown values before constructing an enum.
pub type LegacyRuntimeActionFn = extern "C" fn(
    action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32;

/// Frozen host vtable layout used by plugins exporting
/// [`LEGACY_REGISTER_SYMBOL`]. Do not add, remove, reorder, or retype fields in
/// this struct. New plugins use [`PluginApiVt`] and [`REGISTER_SYMBOL_V2`].
///
/// The v1 action slot accepts only the historical ids `0..=15`. Hosts must
/// validate the raw `u32` before dispatching it.
#[repr(C)]
pub struct LegacyPluginApiV1 {
    pub free_string: FreeStringFn,
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
    pub runtime_action: LegacyRuntimeActionFn,
    pub dispatch_event:
        Option<extern "C" fn(event: StablePluginEvent, user_data: *mut c_void) -> i32>,
    pub user_data: *mut c_void,
}

unsafe impl Send for LegacyPluginApiV1 {}
unsafe impl Sync for LegacyPluginApiV1 {}

/// The ABI v2 host-provided vtable, passed to `rpi_plugin_register_v2` as a
/// `*const`.
///
/// The plugin reads it during `register` and may copy fn pointers it needs (the
/// struct is POD/Copy). **Every slot is nullable**: a null fn pointer means the
/// host does not support that capability yet — the plugin MUST null-check
/// before calling and degrade gracefully. This keeps the vtable forward-
/// compatible across rpi versions without re-ABI bumps within one
/// `RPI_PLUGIN_ABI_VERSION`.
///
/// `user_data` is the host's opaque context, passed back to every host-provided
/// fn (so the host can recover its session/harness state). The plugin stores it
/// and passes it through unchanged.
#[repr(C)]
pub struct PluginApiVt {
    /// Host's `free_string` — the plugin calls this for every [`StbString`] it
    /// *receives* from the host (outputs of actions, event payloads, inputs to
    /// execute). Never null.
    pub free_string: FreeStringFn,

    // --- 8 registrars (plugin → host "register X into the host") ---
    /// Register a tool. `schema` + the four lifecycle fns + the plugin's own
    /// `free_string` (for the [`StbString`]s in `schema`). Returns `0` on
    /// success. Nullable: host not yet wired for tool registration.
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

    /// Register a slash command. Nullable.
    pub register_command: Option<
        extern "C" fn(
            name: StbStringRef,
            description: StbStringRef,
            handler: CommandHandlerFn,
        ) -> i32,
    >,

    /// Register a keyboard shortcut. Nullable.
    pub register_shortcut:
        Option<extern "C" fn(key: StbStringRef, description: StbStringRef) -> i32>,

    /// Register a CLI flag using its bare name (without leading `--`). The
    /// parsed value is read later with [`RuntimeActionId::GetCliFlag`].
    /// Nullable.
    pub register_flag: Option<extern "C" fn(name: StbStringRef, description: StbStringRef) -> i32>,

    /// Register a custom provider. The host stores `provider_id`/`base_url`/
    /// `api_style` + the plugin's `request_fn` + the plugin's own
    /// `plugin_free_string` (the `out` [`StbString`] `request_fn` *produces* is
    /// plugin-owned and the host must reclaim it — same ownership rule as
    /// `register_resources_discover`) + the plugin's `user_data` (which
    /// `request_fn` receives back unmodified on every call). Nullable (B5c).
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

    /// Register a message renderer. `plugin_free_string` reclaims the `out`
    /// [`StbString`] `render_fn` produces; `user_data` is passed back to it on
    /// every render call. Nullable; interactive TUI consumption accepts the
    /// host JSON component envelope (`text`/`lines`).
    pub register_message_renderer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,

    /// Register a markdown transformer. Same ownership shape as
    /// `register_message_renderer`. Nullable; markdown output is chained in
    /// assistant rendering.
    pub register_markdown_transformer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,

    /// Register an entry renderer. Same ownership shape as
    /// `register_message_renderer`. Nullable; interactive TUI consumption
    /// accepts the host JSON component envelope (`text`/`lines`).
    pub register_entry_renderer: Option<
        extern "C" fn(
            name: StbStringRef,
            render_fn: RenderFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,

    // --- on() event handler registration (the 33-category subscription) ---
    /// Subscribe a handler to one event `tag`. Nullable: host not yet wiring
    /// events. The host dispatches [`StablePluginEvent`]s of that tag to the
    /// handler (`catch_unwind`-wrapped).
    pub register_event_handler: Option<
        extern "C" fn(tag: EventTag, handler: EventHandlerFn, user_data: *mut c_void) -> i32,
    >,

    /// Register a `resources_discover` handler (B5b). The host stores `handler`
    /// + the plugin's own `plugin_free_string` (the `out` [`StbString`] the
    /// handler produces is plugin-owned and the host must reclaim it) +
    /// `user_data`. On discovery (`startup`/`reload`) the host fans the event to
    /// every registered handler in order, concatenating their returned
    /// `{skillPaths, promptPaths, themePaths}` (errors per-handler do NOT abort
    /// the fan-out). Nullable: a host without the resources-discover path leaves
    /// this null and the plugin must degrade (no dynamic resource contribution).
    pub register_resources_discover: Option<
        extern "C" fn(
            handler: ResourcesDiscoverFn,
            plugin_free_string: FreeStringFn,
            user_data: *mut c_void,
        ) -> i32,
    >,

    // --- runtime actions (17, uniform dispatch) ---
    /// Invoke a host runtime action. See [`RuntimeActionId`] / [`RuntimeActionFn`].
    /// Always present; a host without an action bridge installs a nonzero-returning
    /// stub so plugins can degrade gracefully.
    pub runtime_action: RuntimeActionFn,

    // --- event dispatch (plugin → host "emit an event upstream") ---
    /// Emit an event upstream (e.g. a tool announcing a custom UI event). The
    /// host forwards to interested subscribers. Ownership of the event's
    /// [`StbString`]s passes to the host (freed via `free_string`). Nullable.
    pub dispatch_event:
        Option<extern "C" fn(event: StablePluginEvent, user_data: *mut c_void) -> i32>,

    /// The host's opaque context, passed through to every host-provided fn.
    /// The plugin stores this and hands it back unmodified on each call.
    pub user_data: *mut c_void,
}

// SAFETY: the vtable is a POD struct of fn pointers + one raw `user_data`
// pointer. It is `Send`+`Sync` so the host can hand it to the plugin's register
// thread and the plugin can call its fns from the blocking driver thread; the
// host guarantees the `user_data` is valid across those calls.
unsafe impl Send for PluginApiVt {}
unsafe impl Sync for PluginApiVt {}

/// Type alias for the unified plugin API.
///
/// This is an alias for [`PluginApiVt`] to simplify the entrypoint signature.
/// New plugins should use [`register_entrypoint_unified`] with this type.
pub type PluginApi = PluginApiVt;

// ---------------------------------------------------------------------------
// Register contract
// ---------------------------------------------------------------------------

/// The ABI version this SDK publishes. ABI v2 adds the `GetCliFlag` runtime
/// action and makes [`RuntimeActionFn`]'s action parameter an explicitly
/// validated `u32`.
///
/// A host and plugin built for different ABI versions must never use each
/// other's [`PluginApiVt`]. [`register_entrypoint`] compares the scalar version
/// before dereferencing `api`, so an ABI v1 plugin presented to a v2 host (or a
/// v2 plugin presented to a v1 host) returns nonzero and is safely skipped
/// rather than reading a differently defined vtable.
pub const RPI_PLUGIN_ABI_VERSION: u32 = 2;

/// ABI version passed to the legacy `rpi_plugin_register` entrypoint.
pub const LEGACY_PLUGIN_ABI_VERSION: u32 = 1;

/// The ABI v2 symbol the host looks up first in each cdylib.
/// Must be an `extern "C" fn(*const PluginApiVt, u32) -> i32`.
pub const REGISTER_SYMBOL_V2: &[u8] = b"rpi_plugin_register_v2\0";

/// Alias for the current SDK's register symbol.
pub const REGISTER_SYMBOL: &[u8] = REGISTER_SYMBOL_V2;

/// The ABI v1 symbol used only when [`REGISTER_SYMBOL_V2`] is absent.
pub const LEGACY_REGISTER_SYMBOL: &[u8] = b"rpi_plugin_register\0";

/// Plugin entrypoint signature. The host loads the cdylib, looks up
/// `rpi_plugin_register_v2`, and calls it with the host `PluginApiVt` and the
/// host's current `RPI_PLUGIN_ABI_VERSION`.
///
/// Return `0` on successful registration; nonzero is a plugin-defined error
/// code (the host logs it and skips the plugin). The plugin must compare
/// `abi_version` before reading `api`; [`register_entrypoint`] implements this
/// ordering and safely rejects old/new ABI mixtures. A plugin should copy any
/// needed function pointers only inside the callback this helper invokes.
pub type RpiPluginRegister = extern "C" fn(api: *const PluginApiVt, abi_version: u32) -> i32;

/// ABI v1 plugin entrypoint signature.
pub type LegacyRpiPluginRegister =
    extern "C" fn(api: *const LegacyPluginApiV1, abi_version: u32) -> i32;

// ---------------------------------------------------------------------------
// ABI v3 — plugin declarations (extension block beside the frozen v2 vtable)
// ---------------------------------------------------------------------------

/// The ABI version the v3 entrypoint advertises.
pub const RPI_PLUGIN_ABI_VERSION_V3: u32 = 3;

/// The ABI v3 symbol the host looks up before [`REGISTER_SYMBOL_V2`].
pub const REGISTER_SYMBOL_V3: &[u8] = b"rpi_plugin_register_v3\0";

/// The ABI v3 **extension block**, passed to `rpi_plugin_register_v3` *in
/// addition to* the frozen v2 [`PluginApiVt`]. Keeping v3 as a separate side
/// struct means the v2 vtable layout never changes: a v2 plugin is unaffected,
/// a v3 plugin reads both.
///
/// Every slot is nullable. A null slot means the host does not support that
/// declaration yet — the plugin must null-check and degrade (skip the
/// declaration; the host applies defaults: priority `100`, all platforms).
#[repr(C)]
pub struct PluginApiVt3Ext {
    /// Declare this plugin's host-side preferences as a JSON object, e.g.
    /// `{"priority":60,"platforms":["linux","macos"]}`.
    ///
    /// - `priority` (integer, default `100`): dispatch order for this plugin's
    ///   handlers — **smaller runs first** (stable; registration order breaks
    ///   ties). Suggested bands: `0..=49` infrastructure, `50..=99`
    ///   connection, `100..=199` normal, `200+` post/cleanup.
    /// - `platforms` (array of strings, default all): platforms this plugin
    ///   supports; the host skips the plugin's handlers on any other platform.
    ///   Recognised: `"linux"`, `"macos"`, `"windows"`, `"android"`.
    ///
    /// Called during `register`. Returns `0` on success, nonzero on a handled
    /// error (host logs it and keeps the defaults).
    pub declare: Option<extern "C" fn(json: StbStringRef) -> i32>,
    /// Reserved for future v3 declarations. MUST be null-filled today; the host
    /// ignores these slots. Zeroing them lets a future host start using them
    /// without another ABI bump.
    pub _reserved: [*mut c_void; 3],
}
unsafe impl Send for PluginApiVt3Ext {}
unsafe impl Sync for PluginApiVt3Ext {}

impl PluginApiVt3Ext {
    /// A v3 ext block with every slot null (no declarations). Useful for tests
    /// and hosts that support no declarations yet.
    pub const fn empty() -> Self {
        Self {
            declare: None,
            _reserved: [ptr::null_mut(); 3],
        }
    }
}

/// ABI v3 plugin entrypoint signature: `(api, ext, abi_version)`.
pub type RpiPluginRegisterV3 =
    extern "C" fn(api: *const PluginApiVt, ext: *const PluginApiVt3Ext, abi_version: u32) -> i32;

// ---------------------------------------------------------------------------
// Unified ABI — single unversioned symbol, version in struct
// ---------------------------------------------------------------------------

/// The unified ABI version. This is the *only* ABI going forward; the version
/// lives inside the struct (first field) so future breaks can be detected by
/// reading the pointer, independent of entrypoint arity.
pub const RPI_PLUGIN_ABI_VERSION_UNIFIED: u32 = 4;

/// The unversioned symbol for the unified ABI. Old v1 plugins also exported
/// `rpi_plugin_register` with a different signature; repurposing the name
/// means leftover v1 `.dll`s will be mis-called (accepted risk).
pub const REGISTER_SYMBOL_UNIFIED: &[u8] = b"rpi_plugin_register\0";

/// The unified host-provided API struct. The first two fields (`abi_version`
/// and `struct_size`) are contract-identity markers that survive future
/// signature changes: a plugin reads them from the pointer before anything
/// else, so even if the entrypoint arity changes, the version check remains
/// robust.
///
/// This struct folds in v3's `declare` slot (previously in `PluginApiVt3Ext`),
/// so there's no side block — one struct, one entrypoint, forever.
#[repr(C)]
pub struct PluginApi {
    /// Contract identity. First field so a plugin can validate before reading
    /// anything else, independent of the entrypoint arity.
    pub abi_version: u32,
    /// `sizeof(PluginApi)` as the host built it. Lets a plugin detect a host
    /// that predates slots it wants (host_struct_size < plugin's expected
    /// offset). Future hosts may grow this struct; old plugins check
    /// `struct_size` before accessing new fields.
    pub struct_size: u32,

    // --- All fields from PluginApiVt (same order, same types) ---
    /// Host's `free_string` — the plugin calls this for every [`StbString`] it
    /// *receives* from the host (outputs of actions, event payloads, inputs to
    /// execute). Never null.
    pub free_string: FreeStringFn,

    // --- 8 registrars (plugin → host "register X into the host") ---
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

    // --- v3's declare slot, folded in ---
    /// Declare this plugin's host-side preferences as a JSON object, e.g.
    /// `{"priority":60,"platforms":["linux","macos"]}`. Nullable: host does
    /// not support declarations yet (plugin skips the call; host applies
    /// defaults: priority `100`, all platforms).
    pub declare: Option<extern "C" fn(json: StbStringRef) -> i32>,
}
unsafe impl Send for PluginApi {}
unsafe impl Sync for PluginApi {}

/// Unified ABI plugin entrypoint signature: `(api)`. The version is read from
/// `api->abi_version`; no positional arg.
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

/// Export an ABI v3 plugin entrypoint under `rpi_plugin_register_v3`.
///
/// The expression receives `(&PluginApiVt, &PluginApiVt3Ext)` and returns the
/// plugin-defined registration status code. Use this when the plugin wants to
/// declare a priority / supported platforms through [`PluginApiVt3Ext::declare`];
/// otherwise prefer `export_plugin_v2!`.
///
/// ```ignore
/// rpi_plugin_sdk::export_plugin_v3!(|api, ext| {
///     if let Some(declare) = ext.declare {
///         declare(rpi_plugin_sdk::StbStringRef::from_str(
///             r#"{"priority":60,"platforms":["linux"]}"#,
///         ));
///     }
///     // ... register tools and handlers through `api` ...
///     0
/// });
/// ```
#[macro_export]
macro_rules! export_plugin_v3 {
    ($body:expr) => {
        #[no_mangle]
        pub extern "C" fn rpi_plugin_register_v3(
            api: *const $crate::PluginApiVt,
            ext: *const $crate::PluginApiVt3Ext,
            abi_version: u32,
        ) -> i32 {
            // SAFETY: host guarantees `api`/`ext` are valid for this call.
            unsafe { $crate::register_entrypoint_v3(api, ext, abi_version, $body) }
        }
    };
}

/// v3 entrypoint helper: compares `abi_version` against
/// [`RPI_PLUGIN_ABI_VERSION_V3`] before dereferencing `api`/`ext`.
///
/// # Safety
///
/// `api` and `ext` must be valid, properly aligned pointers that remain valid
/// for the duration of `body`.
pub unsafe fn register_entrypoint_v3(
    api: *const PluginApiVt,
    ext: *const PluginApiVt3Ext,
    abi_version: u32,
    body: impl FnOnce(&PluginApiVt, &PluginApiVt3Ext) -> i32,
) -> i32 {
    if abi_version != RPI_PLUGIN_ABI_VERSION_V3 {
        return 1;
    }
    if api.is_null() {
        return 2;
    }
    if ext.is_null() {
        return 3;
    }
    // SAFETY: caller guarantees both pointers are valid; we checked null.
    let api = &*api;
    let ext = &*ext;
    // Contain panics: unwinding out of the plugin's `extern "C"` shim aborts
    // the process, so convert a panic into a status code the host can report.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(api, ext))) {
        Ok(code) => code,
        Err(_) => REGISTER_PANIC_STATUS,
    }
}

/// Export a unified ABI plugin entrypoint under `rpi_plugin_register`.
///
/// The unified ABI has no version suffix in the symbol name; the version lives
/// inside the [`PluginApi`] struct (first field `abi_version`). This is the
/// *only* ABI going forward; old versioned symbols (`rpi_plugin_register_v2`,
/// `_v3`) are kept temporarily for migration but are deprecated.
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
            unsafe { $crate::register_entrypoint_unified(api, $body) }
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
pub unsafe fn register_entrypoint_unified(
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

/// Export an ABI v2 plugin entrypoint under `rpi_plugin_register_v2`.
///
/// The expression receives `&PluginApiVt` and returns the plugin-defined
/// registration status code.
///
/// ```ignore
/// rpi_plugin_sdk::export_plugin_v2!(|api| {
///     // register tools and handlers through `api`
///     0
/// });
/// ```
#[macro_export]
macro_rules! export_plugin_v2 {
    ($body:expr) => {
        #[no_mangle]
        pub extern "C" fn rpi_plugin_register_v2(
            api: *const $crate::PluginApiVt,
            abi_version: u32,
        ) -> i32 {
            // SAFETY: host guarantees `api` is valid for this call.
            unsafe { $crate::register_entrypoint(api, abi_version, $body) }
        }
    };
}

/// Convenience for host + plugin: declare the register entrypoint.
///
/// A plugin crate writes:
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn rpi_plugin_register_v2(api: *const PluginApiVt, abi_version: u32) -> i32 {
///     rpi_plugin_sdk::register_entrypoint(api, abi_version, |api| {
///         // ... register tools / handlers using `api` ...
///         0
///     })
/// }
/// ```
/// The helper performs the version check (return nonzero on mismatch) and
/// null-checks `api` before invoking the plugin body.
/// # Safety
///
/// `api` must be a valid, properly aligned pointer to a `PluginApiVt` that
/// remains valid for the duration of the `body` call. The host is responsible
/// for ensuring this invariant.
pub unsafe fn register_entrypoint(
    api: *const PluginApiVt,
    abi_version: u32,
    body: impl FnOnce(&PluginApiVt) -> i32,
) -> i32 {
    if abi_version != RPI_PLUGIN_ABI_VERSION {
        // Mismatch: refuse to register. The host logs "ABI version mismatch"
        // and skips loading this plugin.
        return 1;
    }
    if api.is_null() {
        return 2;
    }
    // SAFETY: caller guarantees `api` is valid; we additionally check for null.
    let api = &*api;
    // Contain panics (see `REGISTER_PANIC_STATUS`): a panic in the plugin body
    // must not unwind out of the `extern "C"` entrypoint.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(api))) {
        Ok(code) => code,
        Err(_) => REGISTER_PANIC_STATUS,
    }
}

/// Unified entrypoint registration function.
///
/// This is a simplified version of [`register_entrypoint`] that doesn't require
/// an explicit ABI version parameter. It uses the current SDK's ABI version
/// internally.
///
/// # Safety
///
/// `api` must be a valid, properly aligned pointer to a [`PluginApi`] that
/// remains valid for the duration of the `body` call.
pub unsafe fn register_entrypoint_unified(
    api: *const PluginApi,
    body: impl FnOnce(&PluginApi) -> i32,
) -> i32 {
    register_entrypoint(api, RPI_PLUGIN_ABI_VERSION, body)
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
        // The body panics before touching the vtable, so an uninitialized (but
        // aligned, non-null) pointer is sufficient — it is only dereferenced to
        // build `&PluginApiVt`, never read.
        let mut vt = std::mem::MaybeUninit::<PluginApiVt>::uninit();
        let vt_ptr = vt.as_mut_ptr();
        let rc = unsafe {
            register_entrypoint(vt_ptr, RPI_PLUGIN_ABI_VERSION, |_api| {
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
    fn event_tag_count_is_36() {
        // Enumerate every tag; a compile-time + runtime guarantee that the
        // 36-category surface is intact (33 Pi on() categories + the
        // rpi-specific BeforeTuiStart + 2 UI prompt events).
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
        ];
        assert_eq!(tags.len(), EVENT_TAG_COUNT);
        assert_eq!(EVENT_TAG_COUNT, 36);
        // Distinct discriminants 0..35.
        let mut discs: Vec<u32> = tags.iter().map(|t| *t as u32).collect();
        discs.sort();
        assert_eq!(discs, (0..36).collect::<Vec<u32>>());
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
    fn plugin_api_vt_is_pod_and_sized() {
        // The vtable must be a plain old data struct: every fn pointer is
        // non-Drop, the struct has no Drop impl. We exercise that it can be
        // zeroed and read without UB.
        let vt = PluginApiVt {
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
        };
        // All optional slots are null → plugin must degrade.
        assert!(vt.register_tool.is_none());
        assert!(vt.register_event_handler.is_none());
        assert!(vt.register_resources_discover.is_none());
        // Copy (POD) — no UB from a plain copy.
        let _copy = vt;
        // `assert!(core::mem::needs_drop::<PluginApiVt>() == false)` — verified
        // by the absence of a Drop impl + all-Copy fields.
        assert!(!core::mem::needs_drop::<PluginApiVt>());
        assert!(!core::mem::needs_drop::<StbString>());
        assert!(!core::mem::needs_drop::<StepResult>());
        assert!(!core::mem::needs_drop::<StablePluginEvent>());
        assert!(!core::mem::needs_drop::<StableToolSchema>());
    }

    #[test]
    fn legacy_v1_layout_is_frozen_and_matches_v2_shared_slots() {
        use core::mem::{align_of, offset_of, size_of};

        let pointer_size = size_of::<*const ()>();
        assert_eq!(size_of::<LegacyPluginApiV1>(), 14 * pointer_size);
        assert_eq!(align_of::<LegacyPluginApiV1>(), align_of::<*const ()>());
        assert_eq!(size_of::<PluginApiVt>(), size_of::<LegacyPluginApiV1>());
        assert_eq!(align_of::<PluginApiVt>(), align_of::<LegacyPluginApiV1>());

        macro_rules! assert_same_offset {
            ($field:ident, $index:expr) => {
                assert_eq!(
                    offset_of!(LegacyPluginApiV1, $field),
                    $index * pointer_size,
                    concat!("unexpected ABI v1 offset for ", stringify!($field))
                );
                assert_eq!(
                    offset_of!(PluginApiVt, $field),
                    offset_of!(LegacyPluginApiV1, $field),
                    concat!("v1/v2 shared field moved: ", stringify!($field))
                );
            };
        }

        assert_same_offset!(free_string, 0);
        assert_same_offset!(register_tool, 1);
        assert_same_offset!(register_command, 2);
        assert_same_offset!(register_shortcut, 3);
        assert_same_offset!(register_flag, 4);
        assert_same_offset!(register_provider, 5);
        assert_same_offset!(register_message_renderer, 6);
        assert_same_offset!(register_markdown_transformer, 7);
        assert_same_offset!(register_entry_renderer, 8);
        assert_same_offset!(register_event_handler, 9);
        assert_same_offset!(register_resources_discover, 10);
        assert_same_offset!(runtime_action, 11);
        assert_same_offset!(dispatch_event, 12);
        assert_same_offset!(user_data, 13);
        assert!(!core::mem::needs_drop::<LegacyPluginApiV1>());
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
        ];

        for (raw, expected) in ids.into_iter().enumerate() {
            assert_eq!(RuntimeActionId::try_from(raw as u32), Ok(expected));
            assert_eq!(u32::from(expected), raw as u32);
        }
        // One past the highest defined id is rejected (nothing is silently
        // accepted), and so is the u32 ceiling.
        assert_eq!(RuntimeActionId::try_from(19), Err(UnknownRuntimeActionId(19)));
        assert_eq!(
            RuntimeActionId::try_from(u32::MAX),
            Err(UnknownRuntimeActionId(u32::MAX))
        );
    }

    #[test]
    fn register_entrypoint_version_mismatch_refuses() {
        let vt = PluginApiVt {
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
        };
        assert_eq!(RPI_PLUGIN_ABI_VERSION, 2);
        assert_eq!(LEGACY_PLUGIN_ABI_VERSION, 1);
        assert_eq!(REGISTER_SYMBOL, REGISTER_SYMBOL_V2);
        assert_ne!(REGISTER_SYMBOL_V2, LEGACY_REGISTER_SYMBOL);

        // An ABI v1 plugin/host mixture is refused before the v2 vtable is read.
        // A null pointer makes the ordering observable: checking `api` first
        // would return 2, while the required version-first path returns 1.
        let rc = unsafe { register_entrypoint(core::ptr::null(), 1, |_| {
            panic!("body must not run on version mismatch");
        }) };
        assert_eq!(rc, 1);

        // A future version is rejected by the same pre-dereference check.
        let rc = unsafe { register_entrypoint(&vt, RPI_PLUGIN_ABI_VERSION + 1, |_| {
            panic!("body must not run on version mismatch");
        }) };
        assert_ne!(rc, 0);

        // Right version → body runs, rc propagated.
        let rc = unsafe { register_entrypoint(&vt, RPI_PLUGIN_ABI_VERSION, |_| 0) };
        assert_eq!(rc, 0);
        let rc = unsafe { register_entrypoint(&vt, RPI_PLUGIN_ABI_VERSION, |_| 42) };
        assert_eq!(rc, 42);

        // Null api → refuse.
        let rc = unsafe { register_entrypoint(core::ptr::null(), RPI_PLUGIN_ABI_VERSION, |_| 0) };
        assert_ne!(rc, 0);
        let _ = reset_freed();
    }

    #[test]
    fn v3_ext_is_pod_and_entrypoint_validates() {
        // The v3 ext block is POD (fn pointer + reserved raw pointers, no Drop).
        assert!(!core::mem::needs_drop::<PluginApiVt3Ext>());
        assert_eq!(RPI_PLUGIN_ABI_VERSION_V3, 3);
        assert_ne!(REGISTER_SYMBOL_V3, REGISTER_SYMBOL_V2);
        let empty = PluginApiVt3Ext::empty();
        assert!(empty.declare.is_none());
        assert!(empty._reserved.iter().all(|p| p.is_null()));

        let vt = PluginApiVt {
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
        };
        let ext = PluginApiVt3Ext::empty();

        // Version mismatch → refused before the body runs (null api makes the
        // ordering observable: version-first returns 1, not the null-api code).
        let rc = unsafe {
            register_entrypoint_v3(core::ptr::null(), &ext, 2, |_, _| {
                panic!("body must not run on version mismatch");
            })
        };
        assert_eq!(rc, 1);

        // Null api / null ext → distinct refusals.
        assert_eq!(
            unsafe { register_entrypoint_v3(core::ptr::null(), &ext, 3, |_, _| 0) },
            2
        );
        assert_eq!(
            unsafe { register_entrypoint_v3(&vt, core::ptr::null(), 3, |_, _| 0) },
            3
        );

        // Right version + non-null → body runs, rc propagated.
        let rc = unsafe { register_entrypoint_v3(&vt, &ext, 3, |_, _| 0) };
        assert_eq!(rc, 0);
        let rc = unsafe { register_entrypoint_v3(&vt, &ext, 3, |_, _| 7) };
        assert_eq!(rc, 7);
        let _ = reset_freed();
    }
}
