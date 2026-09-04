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
//! 3. Registers a **`resources_discover`** handler (B5b) that hands back a
//!    canned skill path — proving the discover→host round-trip (the host fans
//!    the event out to every registered handler, the plugin returns
//!    `{skillPaths:[...]}`, the host merges the arrays and feeds Part A's skill
//!    loader). This is the "三者同交付" coherence point: a plugin's discovered
//!    paths reuse the same loader as static `.pi/skills` dirs.
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
// The provider/render fn signatures are referenced transitively by the
// `register_provider`/`register_markdown_transformer` vtable slots (the host
// passes our `extern "C" fn`s as `ProviderRequestFn`/`RenderFn`-typed params).
// Naming the types here keeps the import live and documents the contract.
#[allow(unused_imports)]
use rpi_plugin_sdk::{ProviderRequestFn, RenderFn};

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
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let value = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let drive = Box::new(EchoDrive {
        text: value,
        polls: 0,
    });
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
            let progress =
                StbString::from_string(r#"{"content":[{"type":"text","text":"..."}]}"#.to_string());
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
extern "C" fn on_message_end(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
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
// The resources_discover handler (B5b) — proves the discover round-trip
// ---------------------------------------------------------------------------

/// A canned skill path the discover handler advertises. The host merges this
/// into its skill-loading list (alongside static `.pi/skills` dirs) and the path
/// flows into the system prompt's `<available_skills>` block. The path points
/// at a `SKILL.md` shipped alongside the cdylib in the build dir — the host
/// smoke test asserts it appears in the discover result.
///
/// `discovered_echo` lives as a pre-canned skill the plugin "discovers" so the
/// round-trip has something concrete to assert without the plugin needing write
/// access at runtime. The fn below is `#[no_mangle]` so the host test can read
/// the exact path it expects back (mirrors `plugin_stub_message_end_hits`).
#[no_mangle]
pub extern "C" fn plugin_stub_discover_skill_path() -> *const u8 {
    DISCOVER_SKILL.as_ptr()
}

/// How many times the discover handler has been invoked (host smoke asserts ≥1).
#[no_mangle]
pub extern "C" fn plugin_stub_discover_hits() -> usize {
    DISCOVER_HITS.load(Ordering::SeqCst)
}

/// The canned skill path this stub advertises via `resources_discover`. A
/// `static` so the handler can hand a pointer back without allocating (and
/// `plugin_stub_discover_skill_path` returns its head). The path is a fixed
/// string the test resolves against the cdylib's own directory at runtime —
/// see `plugin_stub_discover_skills_dir`.
static DISCOVER_SKILL: &[u8] = b"plugin-stub-discovered/SKILL.md";
static DISCOVER_HITS: AtomicUsize = AtomicUsize::new(0);

/// `resources_discover(cwd, reason, out, user_data) -> i32`.
///
/// Receives `cwd` + `reason` (`"startup"` or `"reload"`) as borrowed
/// [`StbStringRef`]s and writes a JSON `{skillPaths:[...]}` payload into the
/// `out` [`StbString`] (plugin-owned; the host reclaims it via the
/// `plugin_free_string` we handed the host at registration). Returns `0`.
///
/// The payload carries one skill path — a fixed marker string the host smoke
/// test asserts appears in `emit_resources_discover`'s merged result. We model
/// the real contract (a plugin hands back paths it discovered) without the
/// plugin needing filesystem access at runtime.
extern "C" fn on_resources_discover(
    _cwd: rpi_plugin_sdk::StbStringRef,
    _reason: rpi_plugin_sdk::StbStringRef,
    out: *mut rpi_plugin_sdk::StbString,
    _user_data: *mut c_void,
) -> i32 {
    DISCOVER_HITS.fetch_add(1, Ordering::SeqCst);
    // Build the JSON payload the host parses. `skillPaths` only (prompt/theme
    // omitted — lenient host treats missing fields as empty).
    let path = std::str::from_utf8(DISCOVER_SKILL).unwrap_or("");
    let json = format!(r#"{{"skillPaths":["{}"]}}"#, path);
    unsafe {
        *out = StbString::from_string(json);
    }
    0
}

// ---------------------------------------------------------------------------
// The register provider (B5c) — proves the provider-injection round-trip
// ---------------------------------------------------------------------------

/// How many times the stub's `ProviderRequestFn` was called. The host smoke test
/// reads this through the cdylib to assert the provider was driven. A
/// process-global atomic because `extern "C" fn` cannot capture.
static PROVIDER_REQUEST_HITS: AtomicUsize = AtomicUsize::new(0);

/// Read the `ProviderRequestFn` hit counter (host smoke test uses this).
#[no_mangle]
pub extern "C" fn plugin_stub_provider_request_hits() -> usize {
    PROVIDER_REQUEST_HITS.load(Ordering::SeqCst)
}

/// The stub's `ProviderRequestFn`: `req_json` is a borrowed request envelope
/// (`{model, context, options}`); `out` is an owning response we write a full
/// assistant-message JSON into (the host parses it + reclaims it via our
/// `plugin_free_string`). v1 one-shot: the stub returns the complete message as
/// one terminal payload (the host's `PluggableProvider` emits it as a single
/// `Done` chunk).
///
/// We return a canned text message so the round-trip has something concrete to
/// assert (the model the host routes here is `stub-model` — see the smoke test).
extern "C" fn on_provider_request(
    _req_json: StbStringRef,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    PROVIDER_REQUEST_HITS.fetch_add(1, Ordering::SeqCst);
    let json = r#"{"role":"assistant","content":[{"type":"text","text":"from-stub-provider"}],"api":"faux","provider":"stub-provider","model":"stub-model","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
    unsafe {
        *out = StbString::from_string(json.to_string());
    }
    0
}

// ---------------------------------------------------------------------------
// The markdown-transformer renderer (B5c) — proves the renderer registrar
// ---------------------------------------------------------------------------

/// How many times the stub's markdown `RenderFn` was called. The host smoke test
/// reads this to assert registration landed (B5e wires the TUI consumption; for
/// now registration + exposure is the B5c smoke — calling it is a unit-level
/// check the smoke optionally performs).
static MARKDOWN_TRANSFORM_HITS: AtomicUsize = AtomicUsize::new(0);

/// Read the markdown-transformer hit counter (host smoke test uses this).
#[no_mangle]
pub extern "C" fn plugin_stub_markdown_transform_hits() -> usize {
    MARKDOWN_TRANSFORM_HITS.load(Ordering::SeqCst)
}

/// The stub's markdown `RenderFn`: `input_json` is a borrowed
/// `{ "markdown": "..." }`; `out` gets the transformed markdown JSON
/// `{ "markdown": "<uppercased>" }` (a trivial transform so the round-trip has a
/// concrete assertion). The host reclaims `out` via our `plugin_free_string`.
extern "C" fn on_markdown_transform(
    input_json: StbStringRef,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    MARKDOWN_TRANSFORM_HITS.fetch_add(1, Ordering::SeqCst);
    // Parse the input (borrowed — must NOT free). lenient: missing `markdown` ⇒ "".
    let input = unsafe { input_json.as_str() };
    let parsed: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let md = parsed
        .get("markdown")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let json = format!(r#"{{"markdown":"{}"}}"#, escape_json_string(&md));
    unsafe {
        *out = StbString::from_string(json);
    }
    0
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

        // Register the resources_discover handler (B5b). The host stores our
        // handler + our `plugin_free_string` (the `out` StbString we produce is
        // plugin-owned; the host reclaims it via that fn) + our `user_data`. On
        // discovery the host fans the event out to every registered handler and
        // merges their returned `{skillPaths, promptPaths, themePaths}`.
        let Some(register_resources_discover) = api.register_resources_discover else {
            // Host without the discover path — degrade (the smoke test host
            // always wires it; a non-wiring host logs + we continue).
            return 3;
        };
        let rc = register_resources_discover(
            on_resources_discover,
            plugin_free_string,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }

        // Register a custom provider (B5c). The host wraps `on_provider_request`
        // in a `PluggableProvider` impl of `rpi_ai::Provider` and injects it into
        // `AgentHarnessOptions.models`; a catalog model whose `provider` field is
        // `"stub-provider"` routes to it. The host calls `request_fn` on
        // `spawn_blocking` (it's sync), reclaims the plugin-owned `out` via our
        // `plugin_free_string`, parses it as an assistant message, emits it as one
        // terminal chunk.
        let Some(register_provider) = api.register_provider else {
            // Host without the provider path — degrade.
            return 4;
        };
        let rc = register_provider(
            StbStringRef::from_str("stub-provider"),
            StbStringRef::from_str("https://stub.example"),
            StbStringRef::from_str("faux"),
            on_provider_request,
            plugin_free_string,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }

        // Register a markdown transformer (B5c). The host records it now; TUI
        // consumption is B5e (the render path calls `RenderFn` at display time).
        // We register one to prove the registrar + registry storage + snapshot
        // exposure round-trips.
        let Some(register_markdown_transformer) = api.register_markdown_transformer else {
            return 5;
        };
        let rc = register_markdown_transformer(
            StbStringRef::from_str("stub-uppercase"),
            on_markdown_transform,
            plugin_free_string,
            std::ptr::null_mut(),
        );
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
    StepResultTag::Pending as u32 == 0
        && StepResultTag::Done as u32 == 1
        && StepResultTag::Err as u32 == 2
);
