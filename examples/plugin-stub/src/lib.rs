//! `plugin-stub` — a minimal **Rust cdylib** rpi plugin.
//!
//! This is the Part B2 ABI smoke target. The host loads it via `rpi-extensions`
//! (`--extensions-dir examples/plugin-stub`'), which looks up
//! `rpi_plugin_register` and calls it with the host vtable. In `register` this
//! plugin:
//!
//! 1. Registers an **`echo`** tool — params `{ "text": string }`, returns that
//!    text prefixed with `echo: `. It drives the full 4-function lifecycle
//!    (`execute`→handle, `poll`, `cancel`, `destroy`) so the host's
//!    `PluginToolAdapter` spawn_blocking bridge is exercised end-to-end against
//!    a real cdylib (not just the in-process test stub).
//! 2. Registers one **event handler** on `MessageEnd` that records the last
//!    message it saw into a process-global slot — proving the 33-category `on()`
//!    dispatch path is wired for at least one tag (the full fan-out to every
//!    `AgentEvent`-folded tag lands in B3).
//!
//! It depends on **`rpi-plugin-sdk` only** — never on `rpi-extensions`,
//! `rpi-harness`, or `rpi-cli` (a cdylib plugin must not link the host; the host
//! links the plugin). The SDK is the ABI contract both sides share.
//!
//! ## Run
//!
//! ```sh
//! cargo build -p plugin-stub
//! rpi --extensions-dir examples/plugin-stub/target/.../echo.dll -p 'echo "hi"'
//! ```
//! (`echo.dll`/`libecho.so`/`libecho.dylib` depending on platform — the loader
//! recognizes all of `.{dll,so,dylib,pyd}`.)

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use rpi_plugin_sdk::{
    register_entrypoint, EventTag, FreeStringFn, PluginApiVt, StableToolSchema, StbString,
    StbStringRef, StepHandle, StepResult, StepResultTag, ToolPartialCb, RPI_PLUGIN_ABI_VERSION,
};

// ---------------------------------------------------------------------------
// The echo tool's per-drive state — what execute() allocates and destroy() frees.
// ---------------------------------------------------------------------------

/// One echo drive. `polls` counts how many times `poll` was called; we return
/// `Done` after one poll (echo is a single-shot tool — there's no real async
/// work to drive). Kept slightly non-trivial so the bridge's poll loop + the
/// partial-callback path are both touched.
struct EchoDrive {
    text: String,
    polls: usize,
}

// ---------------------------------------------------------------------------
// The echo tool's 4 lifecycle fns (extern "C", no captured state)
// ---------------------------------------------------------------------------

/// `execute(tool_call_id, params, free_params) -> StepHandle`.
///
/// `params` is an owning JSON string the host produced; the plugin frees it via
/// the host's `free_params`. We parse out the `text` field (missing ⇒ empty),
/// then allocate the drive state.
extern "C" fn echo_execute(
    _tool_call_id: StbStringRef,
    params: StbString,
    free_params: Option<FreeStringFn>,
) -> StepHandle {
    // Read + free the input params (host-produced → plugin frees via host fn).
    let text = {
        let s = params.to_string_lossy();
        params.free_with(free_params);
        s
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let value = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let drive = Box::new(EchoDrive { text: value, polls: 0 });
    Box::into_raw(drive) as StepHandle
}

/// `poll(handle, partial_cb, user_data) -> StepResult`. **Non-blocking.**
///
/// First poll: emit a `Pending` partial via the callback ("..."), then return
/// `Pending` once more with an empty progress (exercising the inline-progress
/// path too). Second poll: return `Done` with the echoed text.
///
/// `partial_cb` is invoked **synchronously inside poll** (the SDK contract).
/// `user_data` is the host-provided pointer bound to the callback; pass it
/// through unchanged.
extern "C" fn echo_poll(
    handle: StepHandle,
    partial_cb: Option<ToolPartialCb>,
    user_data: *mut c_void,
) -> StepResult {
    // SAFETY: the host guarantees `handle` is the `*mut EchoDrive` we produced
    // in execute(), valid until destroy().
    let drive = unsafe { &mut *(handle as *mut EchoDrive) };
    drive.polls += 1;

    if drive.polls == 1 {
        // Emit a partial progress result through the callback path.
        if let Some(cb) = partial_cb {
            let progress = StbString::from_string(
                r#"{"content":[{"type":"text","text":"..."}]}"#.to_string(),
            );
            // The host frees the partial's StbString (it is plugin-produced here,
            // but the host's partial_cb_trampoline reclaims via host_free_string).
            cb(progress, user_data);
        }
        // Return Pending with empty inline progress (separate from the callback
        // partial already sent). Empty StbString → the host skips it.
        return StepResult::pending(StbString::empty());
    }

    // Second poll: terminal Done with the echoed text.
    let body = format!(
        r#"{{"content":[{{"type":"text","text":"echo: {}"}}]}}"#,
        escape_json_string(&drive.text)
    );
    StepResult::done(StbString::from_string(body))
}

/// `cancel(handle)`. Idempotent + thread-safe + does NOT free. Echo has nothing
/// to cancel (it's two instant polls), so this is a no-op; we still must export
/// it because the host requires all four fns.
extern "C" fn echo_cancel(_handle: StepHandle) {}

/// `destroy(handle)`. Frees the drive state. Idempotent (null ⇒ no-op). Called
/// exactly once by the host's blocking driver.
extern "C" fn echo_destroy(handle: StepHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: handle is the `*mut EchoDrive` from execute(); the host calls
    // destroy exactly once after the poll loop ends.
    unsafe {
        let _ = Box::from_raw(handle as *mut EchoDrive);
    }
}

/// The plugin's own `free_string` for the [`StbString`]s it *produces* (the
/// terminal `Done` result body, the partial progress). The host calls it to
/// reclaim those. Idempotent on empty/null.
extern "C" fn plugin_free_string(s: StbString) {
    if s.is_empty() || s.ptr.is_null() {
        return;
    }
    // SAFETY: we produced `s` via `StbString::from_string` (a `Box<[u8]>`); the
    // ptr+len reconstruct the same allocation.
    unsafe {
        let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
        let _ = Box::from_raw(slice as *const [u8] as *mut [u8]);
    }
}

// ---------------------------------------------------------------------------
// The event handler — proves the on() dispatch path is wired (B2 minimum).
// ---------------------------------------------------------------------------

/// How many `MessageEnd` events the handler has received. Read from the host
/// (rpi-cli's smoke test asserts it fires when a run produces an assistant
/// message). A process-global atomic because `extern "C" fn` cannot capture.
static MESSAGE_END_HITS: AtomicUsize = AtomicUsize::new(0);

/// The handler the plugin registers for `MessageEnd`. Receives a
/// [`rpi_plugin_sdk::StablePluginEvent`]; per the SDK contract the handler MUST
/// NOT free the event's strings (the host frees exactly once after the fan-out).
/// Returns 0 (success); nonzero would be logged but not abort the fan-out.
extern "C" fn on_message_end(_event: rpi_plugin_sdk::StablePluginEvent, _user_data: *mut c_void) -> i32 {
    MESSAGE_END_HITS.fetch_add(1, Ordering::SeqCst);
    0
}

/// Read the `MessageEnd` hit counter (host smoke test uses this). Returns the
/// number of times the handler has fired in this process.
#[no_mangle]
pub extern "C" fn plugin_stub_message_end_hits() -> usize {
    MESSAGE_END_HITS.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// The register entrypoint
// ---------------------------------------------------------------------------

/// The cdylib entrypoint the host looks up. `register_entrypoint` does the ABI
/// version check (refuses on mismatch) + null-checks `api`, then runs our body.
///
/// Return 0 on success; nonzero ⇒ the host logs + skips this plugin.
#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const PluginApiVt, abi_version: u32) -> i32 {
    register_entrypoint(api, abi_version, |api| {
        // Register the echo tool. Build an owning StableToolSchema (name +
        // description + parameters-as-JSON); the host frees the schema's strings
        // via our `plugin_free_string` after copying them out.
        let schema = Box::new(StableToolSchema {
            name: StbString::from_string("echo".to_string()),
            description: StbString::from_string(
                "Echoes back the provided text (a B2 ABI smoke-test tool).".to_string(),
            ),
            parameters: StbString::from_string(
                r#"{"type":"object","properties":{"text":{"type":"string","description":"The text to echo back."}},"required":["text"]}"#
                    .to_string(),
            ),
        });
        let schema_ptr = &*schema as *const StableToolSchema;

        let Some(register_tool) = api.register_tool else {
            // Host not yet wiring tool registration — refuse so the host logs a
            // diagnostic (should not happen against a B2+ host).
            return 1;
        };
        let rc = register_tool(
            schema_ptr,
            echo_execute,
            echo_poll,
            echo_cancel,
            echo_destroy,
            plugin_free_string,
        );
        // The host copied the schema strings out + freed them via
        // plugin_free_string inside the trampoline; we can drop our Box now
        // (the StbStrings inside were already freed — dropping the Box just
        // releases the container, the freed fields are Copy ptr+len, no double-free).
        drop(schema);
        if rc != 0 {
            return rc;
        }

        // Register the MessageEnd event handler (the on() surface).
        let Some(register_event_handler) = api.register_event_handler else {
            return 2;
        };
        let rc = register_event_handler(EventTag::MessageEnd, on_message_end, std::ptr::null_mut());
        if rc != 0 {
            return rc;
        }

        // RPI_PLUGIN_ABI_VERSION referenced here so a future `pub use` doesn't
        // trip an unused-import when the version check moves fully into the SDK.
        let _ = RPI_PLUGIN_ABI_VERSION;
        0
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Escape a `&str` for safe embedding inside a JSON string literal. We hand-build
/// the echo result JSON (above) rather than depend on serde in the cdylib's hot
/// path, so we must escape the user text ourselves.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// A compile-time assertion that StepResultTag is the repr we expect — guards a
// silent layout drift in the SDK that would corrupt the poll() return.
#[allow(dead_code)]
const _ASSERT_STEP_TAG_REPR: () = assert!(
    StepResultTag::Pending as u32 == 0 && StepResultTag::Done as u32 == 1 && StepResultTag::Err as u32 == 2
);
