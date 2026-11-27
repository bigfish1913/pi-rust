//! SDK-owned C ABI trampolines for string inputs, output slots and deallocation.
//!
//! Raw validity and allocation ownership remain the ABI caller's contract.
//! Plugin bodies use Rust strings and an exclusive, lifetime-bound output
//! capability instead of manipulating pointers themselves.
use crate::{FreeStringFn, StbString, StepResult, StepResultTag};

/// Rust-side ownership of an ABI string and its producer's deallocator.
/// Not `Copy` or `Clone`: moving the value transfers cleanup responsibility.
///
/// ```compile_fail
/// use rpi_plugin_sdk::callbacks::OwnedString;
/// let owner = OwnedString::from_string("text".into());
/// let copy = owner;
/// drop(owner); // Ownership moved; a second release cannot be expressed safely.
/// ```
pub struct OwnedString {
    raw: StbString,
    free: Option<FreeStringFn>,
}

impl OwnedString {
    pub fn from_string(text: String) -> Self {
        Self {
            raw: StbString::from_string(text),
            free: Some(free_string),
        }
    }

    /// Adopt an owning string at an ABI boundary.
    ///
    /// # Safety
    /// The caller transfers unique ownership of a live buffer satisfying
    /// `StbString::to_string_lossy`'s requirements. No raw copy may subsequently
    /// be read, changed or freed while this owner exists. `free`, when present,
    /// must be this buffer's producer-side deallocator, callable from any thread.
    /// When absent, the allocation will not be reclaimed automatically.
    pub unsafe fn from_raw(raw: StbString, free: Option<FreeStringFn>) -> Self {
        Self { raw, free }
    }

    pub fn to_string_lossy(&self) -> String {
        self.as_str_lossy().into_owned()
    }

    /// Read the buffer without copying valid UTF-8. Invalid UTF-8 is replaced,
    /// matching [`Self::to_string_lossy`]. The view cannot outlive its owner.
    ///
    /// ```compile_fail
    /// use rpi_plugin_sdk::callbacks::OwnedString;
    /// let text = {
    ///     let owner = OwnedString::from_string("text".into());
    ///     owner.as_str_lossy()
    /// };
    /// println!("{text}");
    /// ```
    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        if self.raw.len == 0 || self.raw.ptr.is_null() {
            return std::borrow::Cow::Borrowed("");
        }
        // SAFETY: construction established exclusive ownership of a live buffer.
        // This borrow keeps the owner (and its producer-side deallocator) alive.
        let bytes = unsafe { std::slice::from_raw_parts(self.raw.ptr.cast::<u8>(), self.raw.len) };
        String::from_utf8_lossy(bytes)
    }

    /// Transfer ownership back across the ABI, without freeing the buffer.
    /// The receiver must use the same producer's deallocator exactly once.
    pub fn into_raw(self) -> StbString {
        let owner = std::mem::ManuallyDrop::new(self);
        owner.raw
    }
}

impl Drop for OwnedString {
    fn drop(&mut self) {
        // SAFETY: this non-copyable owner transfers its live allocation once.
        unsafe { self.raw.free_with(self.free) };
    }
}

pub struct Output<'a> {
    slot: Option<&'a mut StbString>,
    written: Option<OwnedString>,
}

impl<'a> Output<'a> {
    /// # Safety
    /// A non-null `slot` must be aligned, initialized, writable and exclusively borrowed
    /// for `'a`. The host reclaims a written string through this plugin's
    /// `free_string` callback after the handler returns.
    pub unsafe fn from_raw(slot: *mut StbString) -> Self {
        Self {
            slot: unsafe { slot.as_mut() },
            written: None,
        }
    }

    pub fn is_available(&self) -> bool {
        self.slot.is_some()
    }

    /// Allocate only when a slot exists. Repeated writes reclaim only outputs
    /// this view itself produced, never a value initially supplied by the host.
    pub fn write(&mut self, text: String) -> bool {
        self.replace(Some(text))
    }

    pub fn clear(&mut self) -> bool {
        self.replace(None)
    }

    fn replace(&mut self, text: Option<String>) -> bool {
        let Some(slot) = self.slot.as_deref_mut() else {
            return false;
        };
        let next = text.map(OwnedString::from_string);
        *slot = next
            .as_ref()
            .map(|owner| owner.raw)
            .unwrap_or_else(StbString::empty);
        // Replacing the owner reclaims only a previous SDK output, never an
        // initial host value. Raw slot aliases are inaccessible during this view.
        self.written = next;
        true
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        if let Some(owner) = self.written.take() {
            // The final output belongs to the ABI receiver, even when the
            // handler panics. Replaced outputs were reclaimed during writes.
            let _ = owner.into_raw();
        }
    }
}

/// ABI deallocation callback for strings produced by this SDK instance.
///
/// Install this as `FreeStringFn` in the plugin's registrars. The ABI caller
/// must pass ownership of a live `StbString::from_*` boxed-slice allocation
/// exactly once. Never use it for a borrowed input or another producer's string.
///
/// # Safety
/// Transfer a live SDK-owned boxed-slice allocation exactly once. Neither
/// another raw copy nor a borrowed input may be freed through this callback.
///
/// ```compile_fail
/// use rpi_plugin_sdk::{callbacks, StbString};
/// callbacks::free_string(StbString::from_string("text".into()));
/// ```
pub unsafe extern "C" fn free_string(value: StbString) {
    if value.is_empty() || value.ptr.is_null() {
        return;
    }
    // SAFETY: the ABI caller transfers the original boxed-slice allocation
    // exactly once; construction and this callback use the same allocator.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            value.ptr as *mut u8,
            value.len,
        )));
    }
}

/// ABI cleanup callback for a step result produced by this SDK instance.
/// Its live union arm and string allocation must follow the `StepResult` ABI.
///
/// # Safety
/// The tag must identify the initialized payload, whose live SDK-owned string
/// allocation is transferred exactly once.
pub unsafe extern "C" fn free_step_result(result: StepResult) {
    // SAFETY: the producer initializes the union arm indicated by its tag.
    let text = unsafe {
        match result.tag {
            StepResultTag::Pending => result.pending_payload().progress,
            StepResultTag::Done => result.done_payload().result,
            StepResultTag::Err => result.err_payload().message,
        }
    };
    // SAFETY: the caller transfers the live SDK-produced payload exactly once.
    unsafe { free_string(text) };
}

/// An owned copy of a plugin-produced step result, without ABI pointers.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Pending(String),
    Done(String),
    Err(String),
}

/// # Safety
/// The result must have a valid live union arm and transfer ownership of a
/// string produced by this SDK instance exactly once.
pub unsafe fn take_step_result(result: StepResult) -> Step {
    // SAFETY: the caller transfers an owned result from this SDK instance.
    unsafe { take_step_result_with(result, free_string) }
}

/// Copy and reclaim a result through the producing plugin's deallocator.
///
/// # Safety
/// The tag must select the initialized payload, which must transfer a live
/// owned string exactly once. `free` must be that string's producer-side
/// deallocator, callable from any thread.
pub unsafe fn take_step_result_with(result: StepResult, free: FreeStringFn) -> Step {
    // SAFETY: the producer's tag identifies its initialized payload.
    let raw = unsafe {
        match result.tag {
            StepResultTag::Pending => result.pending_payload().progress,
            StepResultTag::Done => result.done_payload().result,
            StepResultTag::Err => result.err_payload().message,
        }
    };
    // SAFETY: the caller transfers the selected allocation and its producer.
    let owned = unsafe { OwnedString::from_raw(raw, Some(free)) };
    let copy = owned.to_string_lossy();
    match result.tag {
        StepResultTag::Pending => Step::Pending(copy),
        StepResultTag::Done => Step::Done(copy),
        StepResultTag::Err => Step::Err(copy),
    }
}

/// Copy and reclaim a result returned by a plugin's tool callback. Use only
/// with a live, owned result allocated by this SDK instance, not host memory.
/// The invocation requires an explicit unsafe block; the macro does not prove
/// validity or ownership of an arbitrary expression.
///
/// ```compile_fail
/// use rpi_plugin_sdk::{StbString, StepResult};
/// let raw = StepResult::done(StbString::from_string("text".into()));
/// let owned = rpi_plugin_sdk::take_step_result!(raw);
/// ```
#[macro_export]
macro_rules! take_step_result {
    ($result:expr) => {{
        $crate::callbacks::take_step_result($result)
    }};
}

/// Copy and reclaim a plugin-owned tool result **without** writing `unsafe`.
///
/// [`take_step_result!`] is the raw form: it forwards to an `unsafe fn`, so
/// every caller needs its own `unsafe` block and its own SAFETY argument. This
/// macro is the same copy and reclaim with that contract asserted once, here —
/// use it from a host that drives a plugin's `poll`, or from a test that plays
/// the host, and keep [`take_step_result!`] only for a call site that must spell
/// the ownership argument out itself.
///
/// ```ignore
/// match rpi_plugin_sdk::take_step!(result) {
///     Step::Done(json) => parse(&json),
///     Step::Err(message) => Err(message),
///     Step::Pending(progress) => still_running(progress),
/// }
/// ```
#[macro_export]
macro_rules! take_step {
    ($result:expr) => {{
        let result = $result;
        // SAFETY: the caller received `result` from one plugin callback and
        // transfers it exactly once; the tag selects the live union arm and the
        // string is reclaimed through the producing SDK's deallocator.
        unsafe { $crate::callbacks::take_step_result(result) }
    }};
}

/// [`take_step!`] with an explicit producer deallocator, for a result that came
/// from another plugin instance.
#[macro_export]
macro_rules! take_step_with {
    ($result:expr, $free:expr) => {{
        let result = $result;
        // Bind the metavariable outside the `unsafe` block: clippy rejects
        // expanding a caller expression inside one.
        let free: $crate::FreeStringFn = $free;
        // SAFETY: as in `take_step!`; `free` is that plugin's producer-side
        // deallocator, callable from any thread.
        unsafe { $crate::callbacks::take_step_result_with(result, free) }
    }};
}

/// Call a runtime action and return `(status_code, response_body)`.
///
/// Without this macro every plugin that talks to the host has to reproduce the
/// ABI dance by hand — allocate an out [`StbString`](crate::StbString), call
/// `runtime_action`, read it with `StbString::to_string_lossy` (unsafe) and hand
/// the allocation back through the host's `free_string` (unsafe) — which forces
/// a `#![deny(unsafe_code)]` plugin to open a hole for plumbing that has nothing
/// to do with its own logic. The macro keeps that contract here, in the crate
/// that owns it, exactly like [`string_handler!`](crate::string_handler) does for the inbound
/// direction.
///
/// The body is returned verbatim (empty when the host wrote nothing): callers
/// decode it themselves, so this works with or without the `json` feature. A
/// nonzero status code is not an error — hosts use it for "this action does not
/// apply here" as well as for failures — so the pair is returned as-is.
///
/// ```ignore
/// let (rc, body) = rpi_plugin_sdk::call_action!(&api, RuntimeActionId::Abort, "{}");
/// ```
#[macro_export]
macro_rules! call_action {
    ($api:expr, $id:expr, $args:expr) => {{
        let api = $api;
        let mut out = $crate::StbString::empty();
        let rc = (api.runtime_action)(
            u32::from($id),
            $crate::StbStringRef::from_str($args),
            &mut out,
            api.user_data,
        );
        // SAFETY: the host wrote `out` for this invocation and transferred it to
        // the plugin; reading it and handing the allocation back through the
        // host's own `free_string` exactly once is the response contract.
        let text = unsafe { out.to_string_lossy() };
        unsafe { (api.free_string)(out) };
        (rc, text)
    }};
}

/// Read an ABI string, owned ([`StbString`]) or borrowed
/// ([`StbStringRef`](crate::StbStringRef)), into a `String`.
///
/// `#[doc(hidden)]`: this exists only so [`stb_text!`](crate::stb_text) can be
/// type-agnostic. It is not part of the documented plugin surface — a safe
/// accessor would let safe code forge an ABI string, so the macros stay the
/// only advertised entry point.
///
/// Public for macro expansion only; plugins should call the macro, never this
/// trait directly.
#[doc(hidden)]
pub trait AbiText {
    /// Copy the string out of the ABI.
    ///
    /// # Safety
    /// The value must be a live ABI string: plugin-owned memory from this SDK,
    /// or host memory valid for the current call. The copy happens immediately,
    /// so the result never borrows from it.
    unsafe fn abi_text_read(&self) -> String;
}

impl AbiText for crate::StbString {
    unsafe fn abi_text_read(&self) -> String {
        // SAFETY: the caller upholds the trait contract.
        unsafe { self.to_string_lossy() }
    }
}

impl AbiText for crate::StbStringRef {
    unsafe fn abi_text_read(&self) -> String {
        // SAFETY: the caller upholds the trait contract.
        unsafe { self.as_str() }.to_string()
    }
}

/// Copy an ABI string (owned or borrowed) without reclaiming it.
///
/// The reading half of [`call_action!`], for the strings a plugin can see but
/// does not own — an owned [`StbString`](crate::StbString) it is about to hand
/// back to the host, a `StableToolSchema` field it just built, or a borrowed
/// [`StbStringRef`](crate::StbStringRef) whose lifetime the host guarantees for
/// the current call. Use [`stb_take!`](crate::stb_take) when the plugin *does* own the
/// allocation.
#[macro_export]
macro_rules! stb_text {
    ($string:expr) => {{
        let string = $string;
        // SAFETY: the caller passes a live ABI string — plugin-owned memory from
        // this SDK, or host memory valid for the current call. Reading it copies.
        unsafe { $crate::callbacks::AbiText::abi_text_read(&string) }
    }};
}

/// Copy an owning ABI string and reclaim it with its producer's deallocator.
///
/// The counterpart of [`stb_text!`](crate::stb_text) for strings the plugin received and must
/// free: one read, one free, in that order, so the allocation cannot leak. The
/// deallocator is the one the host supplied with the value (for a tool `execute`
/// that is the fourth argument), never this process's `free`.
#[macro_export]
macro_rules! stb_take {
    ($string:expr, $free:expr) => {{
        let string = $string;
        // Bind the metavariable outside the `unsafe` block: clippy rejects
        // expanding a caller expression inside one.
        let free: Option<$crate::FreeStringFn> = $free;
        // SAFETY: as in `stb_text!`; the value is owning, so it is reclaimed
        // exactly once below with the producer's deallocator.
        let text = unsafe { string.to_string_lossy() };
        unsafe { string.free_with(free) };
        text
    }};
}

/// Discard an owning ABI string using its producer's deallocator without reading it.
/// The arguments follow [`stb_take!`](crate::stb_take): pass the owned value and
/// the producer's `Option<FreeStringFn>`, and do not use the raw value again.
#[macro_export]
macro_rules! stb_drop {
    ($string:expr, $free:expr) => {{
        let string: $crate::StbString = $string;
        let free: Option<$crate::FreeStringFn> = $free;
        // SAFETY: the caller transfers ownership and supplies its producer's free function.
        unsafe { string.free_with(free) };
    }};
}

/// Define a fake host's `register_tool` callback. The body receives a borrowed
/// [`StableToolSchema`](crate::StableToolSchema), the execute callback, and a
/// `(poll, cancel, destroy)` tuple. Bind a fourth argument for the plugin's
/// deallocator when the body needs to reclaim the schema strings.
///
/// The ABI caller must supply a live schema for the duration of the call. The
/// trampoline returns `1` for a null schema or a panic in the body.
#[macro_export]
macro_rules! tool_schema_handler {
    ($name:ident, |$schema:ident, $execute:ident, $data:pat_param| $body:block) => {
        $crate::tool_schema_handler!($name, |$schema, $execute, $data, _free| $body);
    };
    ($name:ident, |$schema:ident, $execute:ident, $data:pat_param, $free:ident| $body:block) => {
        extern "C" fn $name(
            raw: *const $crate::StableToolSchema,
            $execute: $crate::ToolExecuteFn,
            poll: $crate::ToolPollFn,
            cancel: $crate::ToolCancelFn,
            destroy: $crate::ToolDestroyFn,
            $free: $crate::FreeStringFn,
        ) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the ABI caller keeps the schema live during this call.
                let Some($schema) = (unsafe { raw.as_ref() }) else {
                    return 1;
                };
                let $data = (poll, cancel, destroy);
                $body
            })
        }
    };
}

/// Define a command or prompt callback with a `&str` input and `&mut Output`.
/// The four-argument form also supports runtime-action trampolines. A tuple
/// input `(cwd, reason)` supports resource-discovery callbacks. All forms copy
/// inputs inside the ABI boundary and contain handler panics.
#[macro_export]
macro_rules! string_handler {
    ($(#[$attr:meta])* $vis:vis fn $name:ident(($first:ident, $second:ident), $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name(first: $crate::StbStringRef, second: $crate::StbStringRef,
            slot: *mut $crate::StbString, $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the host keeps both input buffers and the exclusive
                // output slot valid until this ABI invocation returns.
                let first_text = unsafe { first.as_str() }.to_owned();
                let second_text = unsafe { second.as_str() }.to_owned();
                let $first = first_text.as_str();
                let $second = second_text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
    ($(#[$attr:meta])* $vis:vis fn $name:ident($input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name(raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the host keeps its input buffer and exclusive output
                // slot valid until this ABI invocation returns.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
    ($(#[$attr:meta])* $vis:vis fn $name:ident($action:ident, $input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name($action: u32, raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: as in the three-argument ABI trampoline above.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
}

/// Define a tool execute callback whose call id is an ordinary borrowed str.
/// The two-argument form gives the body an OwnedString that reclaims parameters
/// through the host's supplied deallocator. The three-argument form exposes
/// raw parameters and their deallocator for callers implementing their own ABI.
#[macro_export]
macro_rules! execute_handler {
    ($(#[$attr:meta])* fn $name:ident($id:ident, $params:ident) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name(
            raw: $crate::StbStringRef,
            params: $crate::StbString,
            free: Option<$crate::FreeStringFn>,
        ) -> $crate::StepHandle {
            $crate::guard_or(std::ptr::null_mut(), || {
                // SAFETY: execute transfers live host parameters and their
                // producer callback. Adopt before decoding the borrowed id so
                // a malformed id cannot leak the owned input.
                let $params = unsafe { $crate::callbacks::OwnedString::from_raw(params, free) };
                // SAFETY: the host keeps its borrowed call id live for this call.
                let text = unsafe { raw.as_str() }.to_owned();
                let $id = text.as_str();
                $body
            })
        }
    };
    ($(#[$attr:meta])* fn $name:ident($id:ident, $params:ident, $free:ident) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name(
            raw: $crate::StbStringRef,
            $params: $crate::StbString,
            $free: Option<$crate::FreeStringFn>,
        ) -> $crate::StepHandle {
            $crate::guard_or(std::ptr::null_mut(), || {
                // SAFETY: host input is borrowed for this ABI call only.
                let text = unsafe { raw.as_str() }.to_owned();
                let $id = text.as_str();
                $body
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StbStringRef;

    thread_local! {
        static OWNER_FREES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn counted_free(value: StbString) {
        if !value.is_empty() {
            OWNER_FREES.with(|count| count.set(count.get() + 1));
        }
        // SAFETY: these tests transfer their live SDK allocation exactly once.
        unsafe { free_string(value) };
    }

    execute_handler! { fn refusing_execute(_id, params) {
        assert_eq!(params.to_string_lossy(), "{}");
        std::ptr::null_mut()
    } }

    /// Just enough of a host for the outbound macros: the three fields
    /// `call_action!` touches. The macro is duck-typed, so a full `PluginApi`
    /// (26 fields) is unnecessary noise here.
    #[derive(Clone, Copy)]
    struct FakeHost {
        runtime_action: crate::RuntimeActionFn,
        free_string: crate::FreeStringFn,
        user_data: *mut std::ffi::c_void,
    }

    thread_local! {
        static LAST_ACTION: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
    }

    extern "C" fn echo_action(
        id: u32,
        _args: StbStringRef,
        out: *mut StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        LAST_ACTION.with(|last| last.set(id));
        // SAFETY: the caller provides a live out slot for this invocation.
        unsafe { *out = StbString::from_string(r#"{"aborted":false}"#.into()) };
        0
    }

    fn fake_host() -> FakeHost {
        FakeHost {
            runtime_action: echo_action,
            free_string: counted_free,
            user_data: std::ptr::null_mut(),
        }
    }

    #[test]
    fn call_action_returns_the_body_and_reclaims_it_once() {
        OWNER_FREES.with(|count| count.set(0));
        let host = fake_host();
        let (rc, body) = crate::call_action!(host, 27u32, "{}");
        assert_eq!(rc, 0);
        assert_eq!(body, r#"{"aborted":false}"#);
        assert_eq!(LAST_ACTION.with(std::cell::Cell::get), 27);
        // Exactly one transfer back to the host: no leak, no double free.
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn call_action_reports_a_nonzero_status_without_failing() {
        extern "C" fn refusing(
            _id: u32,
            _args: StbStringRef,
            out: *mut StbString,
            _user_data: *mut std::ffi::c_void,
        ) -> i32 {
            // SAFETY: as in `echo_action`; an error body is still a body.
            unsafe { *out = StbString::from_string(r#"{"error":"nope"}"#.into()) };
            1
        }
        OWNER_FREES.with(|count| count.set(0));
        let host = FakeHost {
            runtime_action: refusing,
            ..fake_host()
        };
        let (rc, body) = crate::call_action!(host, 30u32, "{}");
        assert_eq!(rc, 1);
        assert_eq!(body, r#"{"error":"nope"}"#);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn call_action_tolerates_a_host_that_writes_nothing() {
        extern "C" fn silent(
            _id: u32,
            _args: StbStringRef,
            _out: *mut StbString,
            _user_data: *mut std::ffi::c_void,
        ) -> i32 {
            -1 // no bridge / unsupported action: nothing written
        }
        OWNER_FREES.with(|count| count.set(0));
        let host = FakeHost {
            runtime_action: silent,
            ..fake_host()
        };
        let (rc, body) = crate::call_action!(host, 9u32, "{}");
        assert_eq!(rc, -1);
        assert_eq!(body, "");
        // Freeing the untouched empty slot is a no-op, not a double free.
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn stb_text_copies_without_freeing() {
        OWNER_FREES.with(|count| count.set(0));
        let value = StbString::from_string("hello".into());
        assert_eq!(crate::stb_text!(value), "hello");
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 0);
        // The caller still owns it; reclaim so the test does not leak.
        // SAFETY: the test owns this allocation.
        unsafe { value.free_with(Some(counted_free)) };
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn stb_take_reads_and_reclaims_in_one_step() {
        OWNER_FREES.with(|count| count.set(0));
        let value = StbString::from_string("bye".into());
        assert_eq!(crate::stb_take!(value, Some(counted_free)), "bye");
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn execute_trampoline_reclaims_parameters_on_refusal_and_invalid_id() {
        OWNER_FREES.with(|count| count.set(0));
        assert!(refusing_execute(
            StbStringRef::from_str("call"),
            StbString::from_string("{}".into()),
            Some(counted_free)
        )
        .is_null());
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
        let bytes = [0xff];
        let id = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        assert!(
            refusing_execute(id, StbString::from_string("{}".into()), Some(counted_free)).is_null()
        );
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn string_views_borrow_valid_utf8_and_keep_lossy_fallback_and_cleanup() {
        OWNER_FREES.with(|count| count.set(0));
        let raw = StbString::from_string("hello 🐈".into());
        let pointer = raw.ptr.cast::<u8>();
        // SAFETY: each fresh allocation is transferred to exactly one owner.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        let view = owner.as_str_lossy();
        assert!(matches!(view, std::borrow::Cow::Borrowed(_)));
        assert_eq!(view.as_ptr(), pointer);
        assert_eq!(view, "hello 🐈");
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 0);
        drop(view);
        drop(owner);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);

        let raw = StbString::from_vec(vec![b'a', 0xff, b'b']);
        // SAFETY: invalid UTF-8 is allowed by the adopted buffer's read contract.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        assert!(matches!(owner.as_str_lossy(), std::borrow::Cow::Owned(_)));
        assert_eq!(owner.as_str_lossy(), "a\u{fffd}b");
        assert_eq!(owner.to_string_lossy(), "a\u{fffd}b");
        drop(owner);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);

        // SAFETY: null/zero is the ABI's valid empty buffer.
        let empty = unsafe { OwnedString::from_raw(StbString::empty(), Some(counted_free)) };
        assert!(matches!(
            empty.as_str_lossy(),
            std::borrow::Cow::Borrowed("")
        ));
        drop(empty);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn owned_strings_free_once_on_move_and_unwinding() {
        OWNER_FREES.with(|count| count.set(0));
        let raw = StbString::from_string("live".into());
        // SAFETY: this fresh SDK allocation transfers to counted_free once.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        assert_eq!(owner.to_string_lossy(), "live");
        let moved = owner;
        assert_eq!(moved.to_string_lossy(), "live");
        drop(moved);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
        let result = std::panic::catch_unwind(|| {
            let raw = StbString::from_string("unwind".into());
            // SAFETY: another fresh allocation transfers to the same producer.
            let _owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
            panic!("host parsing failed");
        });
        assert!(result.is_err());
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn into_raw_transfers_cleanup_to_the_receiver() {
        let owner = OwnedString::from_string("transferred".into());
        let raw = owner.into_raw();
        // SAFETY: into_raw transferred the live allocation without freeing it.
        let received = unsafe { OwnedString::from_raw(raw, Some(free_string)) };
        assert_eq!(received.to_string_lossy(), "transferred");
        drop(received);
    }

    #[test]
    fn step_result_uses_the_supplied_producer_for_each_variant() {
        OWNER_FREES.with(|count| count.set(0));
        for (raw, expected) in [
            (
                StepResult::pending(StbString::from_string("progress".into())),
                Step::Pending("progress".into()),
            ),
            (
                StepResult::done(StbString::from_string("done".into())),
                Step::Done("done".into()),
            ),
            (
                StepResult::err(StbString::from_string("error".into())),
                Step::Err("error".into()),
            ),
        ] {
            // SAFETY: each tagged result transfers one live SDK allocation.
            assert_eq!(
                unsafe { take_step_result_with(raw, counted_free) },
                expected
            );
        }
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 3);
    }

    string_handler! { fn echo(input, output, _data) {
        i32::from(!output.write(input.to_owned()))
    } }
    string_handler! { fn replace_twice(input, output, _data) {
        output.write(input.to_owned());
        output.write("second".into());
        0
    } }
    string_handler! { fn panicking(_input, output, _data) {
        output.write("valid output before panic".into());
        panic!("callback failed");
    } }
    string_handler! { fn action(action, input, output, _data) {
        output.write(format!("{action}:{input}"));
        0
    } }
    string_handler! { fn discover((cwd, reason), output, _data) {
        i32::from(!output.write(format!("{cwd}:{reason}")))
    } }

    #[test]
    fn discovery_copies_both_inputs_and_handles_missing_output() {
        let mut output = StbString::empty();
        let cwd = StbStringRef::from_str("/项目");
        let reason = StbStringRef::from_str("reload");
        assert_eq!(discover(cwd, reason, &mut output, std::ptr::null_mut()), 0);
        // SAFETY: discover wrote a live SDK allocation; it has not been freed.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "/项目:reload"
        );
        assert_eq!(
            discover(cwd, reason, std::ptr::null_mut(), std::ptr::null_mut()),
            1
        );
    }

    #[test]
    fn malformed_second_discovery_input_does_not_write_output() {
        let bytes = [0xff];
        let reason = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        let mut output = StbString::empty();
        assert_eq!(
            discover(
                StbStringRef::from_str("/tmp"),
                reason,
                &mut output,
                std::ptr::null_mut()
            ),
            1
        );
        assert!(output.is_empty());
    }

    #[test]
    fn missing_output_is_reported_without_a_pointer_write() {
        assert_eq!(
            echo(
                StbStringRef::from_str("text"),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            1
        );
    }

    #[test]
    fn malformed_utf8_is_rejected_before_the_callback_writes_output() {
        let bytes = [0xff, 0xfe];
        let input = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        let mut output = StbString::empty();
        assert_eq!(echo(input, &mut output, std::ptr::null_mut()), 1);
        assert!(output.is_empty());
    }

    #[test]
    fn strings_are_copied_and_repeated_output_writes_are_reclaimed() {
        let mut output = StbString::empty();
        assert_eq!(
            echo(
                StbStringRef::from_str("你好"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "你好"
        );
        output = StbString::empty();
        assert_eq!(
            replace_twice(
                StbStringRef::from_str("first"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "second"
        );
    }

    #[test]
    fn panics_leave_a_valid_reclaimable_output() {
        let mut output = StbString::empty();
        assert_eq!(
            panicking(StbStringRef::empty(), &mut output, std::ptr::null_mut()),
            1
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "valid output before panic"
        );
    }

    #[test]
    fn runtime_action_signature_is_preserved() {
        let mut output = StbString::empty();
        assert_eq!(
            action(
                17,
                StbStringRef::from_str("{}"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "17:{}"
        );
    }

    #[test]
    fn writing_does_not_reclaim_an_initial_host_owned_value() {
        let initial = StbString::from_string("host-owned".into());
        let mut output = initial;
        assert_eq!(
            echo(
                StbStringRef::from_str("plugin-owned"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: initial is a live SDK allocation, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(initial, Some(free_string)) }.to_string_lossy(),
            "host-owned"
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "plugin-owned"
        );
    }

    #[test]
    fn every_step_result_variant_can_be_reclaimed() {
        // SAFETY: each initialized payload transfers its fresh SDK allocation.
        unsafe {
            free_step_result(StepResult::pending(StbString::from_string(
                "progress".into(),
            )));
            free_step_result(StepResult::done(StbString::from_string("done".into())));
            free_step_result(StepResult::err(StbString::from_string("error".into())));
            free_string(StbString::empty());
            assert_eq!(
                take_step_result!(StepResult::err(StbString::from_string("cancelled".into()))),
                Step::Err("cancelled".into())
            );
        }
    }
}
