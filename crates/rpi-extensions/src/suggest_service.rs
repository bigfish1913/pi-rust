//! Session-level suggestion service for extension argument suggesters.
//!
//! The TUI's `AutocompleteProvider::get_suggestions` is synchronous and must
//! return in microseconds, while a plugin suggester is an out-of-process-speed
//! FFI call that can take hundreds of milliseconds or hang. This module owns
//! everything that gap needs, so the TUI layer stays pure UI (param-region
//! detection, narrowing, popup semantics):
//!
//! - **one worker thread per session** owns all FFI calls; requests arrive on
//!   a bounded channel and are dropped (never queued up) when the worker is
//!   busy;
//! - **at most one in-flight call per command** (review #4): while a
//!   suggester call is still running, a new refresh for the same command is
//!   refused, so a hung plugin leaves at most one lingering thread per
//!   command instead of an unbounded pile. The refusal also makes result
//!   ordering a non-issue: for any command there is never an older and a
//!   newer request racing — the in-flight one IS the newest;
//! - **stale frames keep rendering** while a refresh is in flight (a popup
//!   that blinks off every refresh window is worse than a slightly old
//!   list); the fresh frame replaces it the moment it lands;
//! - **refresh cadence**: at most one call per refresh window per command
//!   (frame age check inside `refresh_if_needed`), never per keystroke;
//! - **reload safety by construction** (review #5): the service lives on
//!   [`crate::ExtensionSession`], and `/reload` builds a fresh session — a
//!   fresh service with an empty cache — so frames from the previous session
//!   are structurally unreachable, and no invalidation hook is needed;
//! - **string ownership** (review #3): every path reclaims the plugin-owned
//!   response exactly once — success (copied first, then freed), timeout (the
//!   FFI thread frees when its send fails), nonzero rc, panic, oversized and
//!   unparseable responses. `StbString` has no `Drop`; this is explicit;
//! - **size limits before the copy** (review #7): responses larger than
//!   [`MAX_RESPONSE_BYTES`] are rejected from the length header alone (before
//!   `to_string_lossy` copies), and parsed candidates are capped to
//!   [`MAX_ITEMS`] items of at most [`MAX_FIELD_CHARS`] characters each;
//! - **full-candidate-list contract** (review #6): requests always carry
//!   `"args":""` — the host narrows the cached full list locally per
//!   keystroke. See `rpi-plugin-sdk`'s `register_argument_suggester` docs;
//!   plugin-side filtering on `args` would poison the per-command cache.

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_plugin_sdk::{StbString, StbStringRef};

use crate::loader::PluginKeepalive;
use crate::registry::ArgumentSuggester;

/// How long the worker waits for one plugin call. Beyond this the frame is
/// abandoned: the response (if it ever arrives) is reclaimed with the
/// plugin's own `plugin_free_string` and never displayed.
pub const SUGGEST_TIMEOUT: Duration = Duration::from_millis(500);

/// How often the worker re-fetches a command's candidate list. Stale frames
/// keep rendering while the refresh runs (no popup blink); the window only
/// gates the background refresh, never the display.
const CACHE_TTL: Duration = Duration::from_millis(2_000);

/// Responses larger than this are rejected before the host copies them.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// At most this many candidates are parsed from one response.
const MAX_ITEMS: usize = 64;

/// At most this many characters per candidate field (counted in chars, so
/// CJK labels and cwd paths truncate cleanly with an ellipsis).
const MAX_FIELD_CHARS: usize = 256;

/// Request channel capacity. Bounded on purpose: a busy worker drops new
/// requests instead of queueing a backlog of stale refreshes.
const FFI_CHANNEL_CAPACITY: usize = 8;

/// One cached candidate row (parsed, already capped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestItem {
    /// What the completion inserts (`name@host`).
    pub value: String,
    /// What renders in the popup.
    pub label: Option<String>,
    /// Optional right-hand hint.
    pub description: Option<String>,
}

/// One command's cached frame. Shared with readers (`Arc`) so a keystroke
/// never clones the list — narrowing builds its own Vec on top.
struct Frame {
    items: Arc<Vec<SuggestItem>>,
    at: Instant,
}

struct Inner {
    cache: Mutex<HashMap<String, Frame>>,
    /// Commands with a call currently executing. At most one per command
    /// (review #4); freed when the call finishes — success, failure or panic
    /// — so a panicking plugin cannot wedge the slot.
    in_flight: Mutex<HashSet<String>>,
}

/// The session's suggestion service: worker thread and per-command cache.
/// Lives on [`crate::ExtensionSession`] (one per session), so provider
/// instances resolve it cheaply per call and a `/reload` gets a fresh
/// service with an empty cache for free.
pub struct SuggestService {
    inner: Arc<Inner>,
    submit: Mutex<mpsc::SyncSender<Call>>,
    /// Refresh window per command (frame age above this re-triggers a call).
    ttl: Duration,
}

impl Default for SuggestService {
    fn default() -> Self {
        Self::new()
    }
}

impl SuggestService {
    /// Spawn the worker thread with the default refresh window. The
    /// keepalive travels with each request (see [`Call`]), not with the
    /// worker itself.
    pub fn new() -> Self {
        Self::with_ttl(CACHE_TTL)
    }

    /// [`Self::new`] with a configurable refresh window.
    pub fn with_ttl(ttl: Duration) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Call>(FFI_CHANNEL_CAPACITY);
        let inner = Arc::new(Inner {
            cache: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
        });
        let worker_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("ext-suggest-worker".into())
            .spawn(move || worker_loop(worker_inner, receiver))
            .expect("spawn ext-suggest-worker");
        Self {
            inner,
            submit: Mutex::new(sender),
            ttl,
        }
    }

    /// Submit the background refresh for `command`'s suggester when the
    /// cached frame is missing or older than the refresh window. **Never
    /// blocks and never spawns more than one call per command** (per-command
    /// slot, review #4); the cached frame keeps rendering meanwhile.
    pub fn refresh_if_needed(
        &self,
        suggester: &ArgumentSuggester,
        keepalive: Arc<PluginKeepalive>,
    ) {
        {
            let map = self.inner.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(frame) = map.get(&suggester.command) {
                if frame.at.elapsed() < self.ttl {
                    return; // frame is fresh enough
                }
            }
        }
        {
            let mut in_flight = self
                .inner
                .in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !in_flight.insert(suggester.command.clone()) {
                return; // one call per command at a time
            }
        }
        let request = Call {
            suggester: suggester.clone(),
            keepalive,
        };
        if self
            .submit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_send(request)
            .is_err()
        {
            // Worker gone (service dropped) or channel full: release the slot
            // so a later refresh isn't refused forever.
            self.inner
                .in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&suggester.command);
        }
    }

    /// The cached frame for `command`, whatever its age — a slightly stale
    /// list renders rather than a popup that blinks off mid-word; freshness
    /// is the background refresh's job (`refresh_if_needed`).
    pub fn cached(&self, command: &str) -> Option<Arc<Vec<SuggestItem>>> {
        let map = self.inner.cache.lock().unwrap_or_else(|p| p.into_inner());
        Some(Arc::clone(&map.get(command)?.items))
    }
}

/// One refresh request for the worker. The whole suggester rides along (it is
/// `Clone` + `Send` + `Sync` by its SAFETY contract), and the keepalive rides
/// with the call: a `/reload` that swaps the session cannot unload the old
/// cdylib under a still-running callback.
struct Call {
    suggester: ArgumentSuggester,
    keepalive: Arc<PluginKeepalive>,
}

/// RAII for the per-command in-flight slot. Held by the FFI THREAD (not the
/// bounded wait in `call_plugin`): the slot frees only when the FFI thread
/// actually finishes — handler returned, late response reclaimed, thread about
/// to exit — so a hung plugin leaves at most ONE lingering thread per command
/// even across timeouts (review round 4, issue #2). Drop is panic-safe: the
/// thread's own stack unwinds it if anything before the explicit release
/// panics.
struct SlotGuard {
    inner: Arc<Inner>,
    command: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.inner
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.command);
    }
}

/// Worker loop: pull requests, run one bounded FFI call each, store the frame
/// on success. Exits when the service is dropped (all senders gone → `recv`
/// errors), so a `/reload` that abandons the old session also retires its
/// worker.
fn worker_loop(inner: Arc<Inner>, requests: mpsc::Receiver<Call>) {
    while let Ok(request) = requests.recv() {
        let command = request.suggester.command.clone();
        // The in-flight slot was inserted by `refresh_if_needed`; its guard is
        // created inside `call_plugin` and MOVED INTO the FFI thread, so the
        // slot frees only when that thread finishes (success, timeout + late
        // reclaim, or panic). The loop itself must not touch the slot.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            call_plugin(
                &request.suggester,
                request.keepalive,
                Arc::clone(&inner),
                command.clone(),
            )
        }));
        if let Ok(Some(items)) = outcome {
            if let Ok(mut cache) = inner.cache.lock() {
                cache.insert(
                    command,
                    Frame {
                        items: Arc::new(items),
                        at: Instant::now(),
                    },
                );
            }
        }
        // Failure paths: nothing is stored; the provider keeps serving the
        // previous frame (if any) via `cached`.
    }
}

/// Run ONE plugin suggester call with a bounded wait for the response, then
/// return. The `keepalive` Arc is CLONED into the detached FFI thread, which
/// holds it for its entire lifetime (review round 4, issue #1): even after
/// the bounded wait times out and this function returns, the thread keeps the
/// old session's cdylib loaded until the late handler has returned and the
/// late response has been reclaimed. The worker only drops its own clone
/// after `call_plugin` returns; the thread's clone outlives it.
///
/// The in-flight slot is NOT released when the bounded wait gives up: the
/// `SlotGuard` is handed to the FFI thread, which releases the command's slot
/// only when the handler truly finished (so a hung plugin can never pile up
/// a second concurrent thread for the same command).
///
/// Every exit path reclaims the plugin-owned response exactly once.
fn call_plugin(
    suggester: &ArgumentSuggester,
    keepalive: Arc<PluginKeepalive>,
    inner: Arc<Inner>,
    command: String,
) -> Option<Vec<SuggestItem>> {
    // Full-candidate-list contract: empty args, host narrows locally (review
    // round 3 #6 — the SDK doc is the normative description).
    let request = serde_json::json!({
        "command": format!("/{}", suggester.command),
        "args": "",
    })
    .to_string();
    let (sender, receiver) = mpsc::channel::<StbString>();
    // One thread per call (bounded by the worker's one-call-at-a-time-per-
    // command slot): the plugin runs on its own stack so a hung handler only
    // costs this thread, and the bounded `recv_timeout` below stays honest.
    // The handler call and the hand-off share one scope: the handler writes
    // THIS scope's `out`, and exactly one exit below owns the written buffer
    // (send success → the receiver frees it after copying; send failure /
    // nonzero rc / panic → freed here). The suggester is CLONED into the
    // thread as a whole — field-projecting a raw pointer out of a borrowed
    // bundle would make the closure capture the pointer and fail `Send`.
    let call = suggester.clone();
    // The keepalive clone + the slot guard move INTO the thread: the cdylib
    // stays mapped until the late handler returned and its response was
    // reclaimed, and the command's slot frees only then (not at timeout).
    let thread_keepalive = Arc::clone(&keepalive);
    let slot = SlotGuard { inner, command };
    let _spawned = std::thread::Builder::new()
        .name("ext-suggest-ffi".into())
        // The whole bundle moves via the fn-argument boundary (a closure body
        // that projects `call.user_data` would capture the raw pointer field
        // and fail `Send`). `slot` is dropped by `run_ffi_call` itself, after
        // the hand-off/free has happened, so the ordering is exact.
        .spawn(move || run_ffi_call(call, request, sender, thread_keepalive, slot));
    match _spawned {
        // The thread owns the slot (and the keepalive) from here on.
        Ok(_) => {}
        // No thread, no callback: release the slot right here. The keepalive
        // clone drops with this scope.
        Err(_) => return None,
    }

    let owned = match receiver.recv_timeout(SUGGEST_TIMEOUT) {
        Ok(owned) => owned,
        // Timeout: the FFI thread stays alive holding the keepalive; when the
        // plugin eventually returns, its send fails and THAT thread frees the
        // buffer with the plugin's own free (see above).
        Err(_) => return None,
    };
    // Size limit BEFORE the copy (review #7): the length header alone decides,
    // no 300 KB lossy copy of a junk response.
    if owned.len > MAX_RESPONSE_BYTES {
        unsafe {
            owned.free_with(Some(suggester.plugin_free_string));
        }
        return None;
    }
    // Copy first (into_owned — the bytes leave the plugin buffer), then
    // reclaim the plugin allocation exactly once on every remaining path.
    // SAFETY: the FFI thread established exclusive ownership of a live,
    // unchanged plugin buffer; this only reads it.
    let text = unsafe { owned.to_string_lossy() };
    unsafe {
        owned.free_with(Some(suggester.plugin_free_string));
    }
    parse_items(&text)
}

/// The FFI thread's body: invoke the plugin handler once and hand the
/// written buffer to `sender` — or reclaim it here when the hand-off is
/// impossible (receiver gone after a timeout, nonzero rc, panic). Holds the
/// session keepalive for its whole lifetime (the clone moved in by the
/// caller), so a `/reload` cannot unload the old cdylib under this callback
/// or the late-response free below. Drops the command's slot guard LAST,
/// after the handler and any reclaim have finished.
fn run_ffi_call(
    call: ArgumentSuggester,
    request: String,
    sender: mpsc::Sender<StbString>,
    _keepalive: Arc<PluginKeepalive>,
    slot: SlotGuard,
) {
    let mut out = StbString::empty();
    let rc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (call.handler)(StbStringRef::from_str(&request), &mut out, call.user_data)
    }));
    let released = match rc {
        Ok(0) => {
            if sender.send(out).is_err() {
                // Receiver gone (caller timed out): reclaim the undelivered
                // buffer here.
                unsafe {
                    out.free_with(Some(call.plugin_free_string));
                }
            }
            true
        }
        // Nonzero rc or panic: the buffer (possibly written before the
        // failure) is reclaimed here; nothing is displayed.
        _ => {
            unsafe {
                out.free_with(Some(call.plugin_free_string));
            }
            true
        }
    };
    // The slot frees only now — after the handler returned and any late
    // response was reclaimed. The invariant always holds (every branch sets
    // true); the explicit drop documents the ordering. If the handler itself
    // panicked, the unwind was contained above and this line still runs.
    debug_assert!(released);
    drop(slot);
}

/// Parse `{"items":[{"value","label","description"}]}` into capped rows.
/// Caps: at most [`MAX_ITEMS`] rows, each field at most [`MAX_FIELD_CHARS`]
/// characters (truncated with `…`, on char boundaries). Rows without a
/// non-empty `value` are skipped.
fn parse_items(text: &str) -> Option<Vec<SuggestItem>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let items = value.get("items")?.as_array()?;
    let mut out = Vec::with_capacity(items.len().min(MAX_ITEMS));
    for item in items
        .iter()
        .filter_map(|item| {
            let field = |key: &str| item.get(key).and_then(|v| v.as_str()).map(truncate_field);
            let value_text = field("value").unwrap_or_default();
            (!value_text.is_empty()).then_some(SuggestItem {
                value: value_text,
                label: field("label"),
                description: field("description"),
            })
        })
        .take(MAX_ITEMS)
    {
        out.push(item);
    }
    (!out.is_empty()).then_some(out)
}

fn truncate_field(text: &str) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text.to_string();
    }
    // Total length INCLUDING the ellipsis stays at MAX_FIELD_CHARS, exactly
    // what the SDK doc promises (a 256-char field, not 257).
    let cut: String = text.chars().take(MAX_FIELD_CHARS - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::ArgumentSuggesterFn;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// Counting wrapper around the SDK's real free: tracks that every
    /// plugin-owned buffer was reclaimed exactly once (leak regression).
    static FREED: AtomicUsize = AtomicUsize::new(0);
    /// Serializes the count-sensitive tests: they assert exact `FREED`
    /// deltas, so they must not interleave (tests run in parallel threads).
    static COUNT_LOCK: Mutex<()> = Mutex::new(());
    unsafe extern "C" fn counting_free(value: StbString) {
        unsafe { rpi_plugin_sdk::callbacks::free_string(value) };
        FREED.fetch_add(1, Ordering::SeqCst);
    }

    extern "C" fn fast_handler(
        request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        let request = unsafe { request_json.as_str() };
        // The full-list contract: the host always asks with empty args.
        assert!(
            serde_json::from_str::<serde_json::Value>(request)
                .ok()
                .and_then(|v| v.get("args").and_then(|v| v.as_str().map(str::to_string)))
                .is_some_and(|args| args.is_empty()),
            "host must send empty args (full-candidate-list contract)"
        );
        let payload = serde_json::json!({
            "items": [
                {"value": "alpha@h", "label": "1. alpha（当前）", "description": "/home"},
                {"value": "beta@h", "label": "2. beta", "description": "/data"},
            ]
        });
        unsafe {
            *out = StbString::from_string(payload.to_string());
        }
        0
    }

    /// Writes a payload, then sleeps past the timeout: exercises the late-
    /// response reclaim path (the FFI thread's send fails after the caller
    /// gave up).
    extern "C" fn slow_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        unsafe {
            *out = StbString::from_string(r#"{"items":[{"value":"late"}]}"#.to_string());
        }
        std::thread::sleep(SUGGEST_TIMEOUT + Duration::from_millis(150));
        0
    }

    /// Writes a buffer, then reports failure — the buffer must still be
    /// reclaimed by the host. This is also the PANIC path's observable form:
    /// an unwinding panic cannot cross `extern "C"` (it would abort the
    /// host), so real SDK plugins contain panics via `guard_or` and surface
    /// them as a nonzero rc after having written `out`.
    extern "C" fn nonzero_after_write_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        unsafe {
            *out = StbString::from_string(
                "{\"items\":[{\"value\":\"written-then-error\"}]}".to_string(),
            );
        }
        1
    }

    /// Returns an oversized payload (> MAX_RESPONSE_BYTES): must be rejected
    /// from the length header, before the host copies it.
    extern "C" fn oversized_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        let big = "a".repeat(MAX_RESPONSE_BYTES + 1024);
        unsafe {
            *out = StbString::from_string(format!(r#"{{"items":[{{"value":"{big}"}}]}}"#));
        }
        0
    }

    /// Counts its invocations in `user_data` (`&AtomicU64`), sleeps 100 ms and
    /// answers `v<n>@h`: deterministic observations of in-flight behavior.
    extern "C" fn counted_slow_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        ud: *mut c_void,
    ) -> i32 {
        let counter = unsafe { &*(ud as *const AtomicU64) };
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            *out = StbString::from_string(
                serde_json::json!({"items": [{"value": format!("v{n}@h")}]}).to_string(),
            );
        }
        0
    }

    /// Answers rc=0 with a malformed payload: the buffer must be reclaimed
    /// (copied, then freed) and no frame may be stored.
    extern "C" fn parse_garbage_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        _ud: *mut c_void,
    ) -> i32 {
        unsafe {
            *out = StbString::from_string("not json at all".to_string());
        }
        0
    }

    /// Direct `call_plugin` entry for the reclaim-path tests: fresh Inner, no
    /// competing slots. The keepalive Arc moves in as it does in production.
    fn call_direct(suggester: &ArgumentSuggester) -> Option<Vec<SuggestItem>> {
        call_plugin(
            suggester,
            PluginKeepalive::empty(),
            Arc::new(Inner {
                cache: Mutex::new(HashMap::new()),
                in_flight: Mutex::new(HashSet::new()),
            }),
            suggester.command.clone(),
        )
    }

    fn suggester_for(handler: ArgumentSuggesterFn) -> ArgumentSuggester {
        ArgumentSuggester {
            command: "msg".into(),
            handler,
            plugin_free_string: counting_free,
            user_data: std::ptr::null_mut(),
        }
    }

    #[test]
    fn fast_plugin_answers_through_the_full_ffi_path() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let items = call_direct(&suggester_for(fast_handler)).expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].value, "alpha@h");
        assert_eq!(items[0].label.as_deref(), Some("1. alpha（当前）"));
    }

    #[test]
    fn slow_plugin_returns_none_within_budget_and_late_buffer_is_reclaimed() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = FREED.load(Ordering::SeqCst);
        let start = Instant::now();
        let result = call_direct(&suggester_for(slow_handler));
        let elapsed = start.elapsed();
        assert!(result.is_none());
        assert!(
            elapsed < SUGGEST_TIMEOUT + Duration::from_millis(300),
            "call blocked {elapsed:?}"
        );
        // Give the FFI thread time to finish its sleep and reclaim its buffer.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            FREED.load(Ordering::SeqCst),
            before + 1,
            "the late response must be reclaimed exactly once"
        );
    }

    #[test]
    fn nonzero_rc_plugin_still_reclaims_the_buffer() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = FREED.load(Ordering::SeqCst);
        let result = call_direct(&suggester_for(nonzero_after_write_handler));
        assert!(result.is_none());
        assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn oversized_response_is_rejected_before_the_copy() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = FREED.load(Ordering::SeqCst);
        let result = call_direct(&suggester_for(oversized_handler));
        assert!(result.is_none());
        assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn parse_failure_response_is_reclaimed_and_yields_no_frame() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = FREED.load(Ordering::SeqCst);
        let result = call_direct(&suggester_for(parse_garbage_handler));
        assert!(result.is_none());
        assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn parse_caps_items_and_truncates_fields_on_char_boundaries() {
        let many: Vec<String> = (0..MAX_ITEMS + 10)
            .map(|i| format!(r#"{{"value":"inst{i}@h"}}"#))
            .collect();
        let text = format!(r#"{{"items":[{}]}}"#, many.join(","));
        let items = parse_items(&text).expect("items");
        assert_eq!(items.len(), MAX_ITEMS);

        // A >MAX_FIELD_CHARS CJK value truncates to exactly MAX_FIELD_CHARS
        // characters INCLUDING the ellipsis marker, never mid-codepoint.
        let text = format!(
            r#"{{"items":[{{"value":"{}"}}]}}"#,
            "值".repeat(MAX_FIELD_CHARS + 50)
        );
        let items = parse_items(&text).expect("items");
        assert_eq!(items[0].value.chars().count(), MAX_FIELD_CHARS);
        assert!(items[0].value.ends_with('…'));
    }

    #[test]
    fn parse_maps_fields_and_skips_empty_values() {
        let text = r#"{"items":[
            {"value":"apmprod@h","label":"1. apmprod（微信）","description":"/export/home"},
            {"value":"","label":"skipped"},
            {"value":"office@h"}
        ]}"#;
        let items = parse_items(text).expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].description.as_deref(), Some("/export/home"));
        assert_eq!(items[1].label, None);
    }

    #[test]
    fn parse_rejects_malformed_payloads() {
        assert!(parse_items("not json").is_none());
        assert!(parse_items(r#"{"no_items":true}"#).is_none());
        assert!(parse_items(r#"{"items":[{"label":"no value"}]}"#).is_none());
        assert!(parse_items(r#"{"items":[]}"#).is_none());
    }

    fn wait_for(service: &SuggestService, command: &str, secs: u64) -> Arc<Vec<SuggestItem>> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(frame) = service.cached(command) {
                return frame;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("frame never landed for {command:?}");
    }

    fn wait_for_value(
        service: &SuggestService,
        command: &str,
        value: &str,
        secs: u64,
    ) -> Arc<Vec<SuggestItem>> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(frame) = service.cached(command) {
                if frame[0].value == value {
                    return frame;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("frame {value:?} never landed for {command:?}");
    }

    #[test]
    fn service_refresh_populates_the_cache() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let service = SuggestService::new();
        assert!(service.cached("msg").is_none());
        service.refresh_if_needed(&suggester_for(fast_handler), PluginKeepalive::empty());
        let frame = wait_for(&service, "msg", 2);
        assert_eq!(frame.len(), 2);
    }

    #[test]
    fn service_refuses_a_second_call_while_one_is_in_flight_then_recovers() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let mut suggester = suggester_for(counted_slow_handler);
        suggester.user_data = &CALLS as *const _ as *mut c_void;

        // Tiny TTL so the second refresh isn't skipped for freshness.
        let service = SuggestService::with_ttl(Duration::from_millis(30));
        service.refresh_if_needed(&suggester, PluginKeepalive::empty()); // call 1
        std::thread::sleep(Duration::from_millis(30)); // call 1 is sleeping in the handler
        let before = CALLS.load(Ordering::SeqCst);
        service.refresh_if_needed(&suggester, PluginKeepalive::empty()); // refused
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before,
            "a refresh must be refused while the command's call is in flight"
        );

        // Call 1 lands (v1): the slot freed and the frame renders.
        let frame = wait_for_value(&service, "msg", "v1@h", 2);
        assert_eq!(frame[0].value, "v1@h");

        // Past the tiny TTL, the refresh fires again and lands v2.
        std::thread::sleep(Duration::from_millis(40));
        service.refresh_if_needed(&suggester, PluginKeepalive::empty());
        let frame = wait_for_value(&service, "msg", "v2@h", 2);
        assert_eq!(frame[0].value, "v2@h");
    }

    /// A handler that sleeps LONGER than the 500 ms budget: the bounded wait
    /// gives up while the handler is still running. The slot must NOT free at
    /// timeout — it frees only when the late handler actually returns — so a
    /// repeated refresh during that window cannot start a second call
    /// (review round 4, issue #2: timeout threads must stay bounded at one
    /// per command).
    extern "C" fn hung_handler(
        _request_json: StbStringRef,
        out: *mut StbString,
        ud: *mut c_void,
    ) -> i32 {
        let counter = unsafe { &*(ud as *const AtomicU64) };
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::sleep(SUGGEST_TIMEOUT + Duration::from_millis(200));
        unsafe {
            *out = StbString::from_string(
                serde_json::json!({"items": [{"value": format!("h{n}@h")}]}).to_string(),
            );
        }
        0
    }

    #[test]
    fn timeout_does_not_free_the_slot_so_hung_plugins_stay_single_threaded() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let mut suggester = suggester_for(hung_handler);
        suggester.user_data = &CALLS as *const _ as *mut c_void;

        // TTL shorter than the handler's sleep: every loop tick would refresh
        // if the timeout freed the slot.
        let service = SuggestService::with_ttl(Duration::from_millis(50));
        service.refresh_if_needed(&suggester, PluginKeepalive::empty()); // call 1
                                                                         // While call 1 is hung (its handler sleeps past the budget, the FFI
                                                                         // thread still owns the slot), every refresh must be refused — the
                                                                         // slot does NOT free at timeout, only when the late handler returns.
        let hang_window = SUGGEST_TIMEOUT + Duration::from_millis(150); // mid-sleep
        let deadline = Instant::now() + hang_window;
        let mut attempts = 0u32;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50)); // TTL expires repeatedly
            service.refresh_if_needed(&suggester, PluginKeepalive::empty());
            attempts += 1;
        }
        assert!(
            attempts >= 2,
            "test must have attempted refreshes during the hang"
        );
        // The handler is STILL sleeping here (sleep ends at ~700ms, window
        // ends at ~650ms): a second call would already have been counted.
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "a hung handler must not admit a second concurrent call for its command"
        );

        // After the late handler returns (past 700ms) and frees the slot, a
        // fresh refresh runs a second call. The late call 1 frame is
        // discarded by design (bounded wait already gave up); only the call
        // COUNT matters here.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && CALLS.load(Ordering::SeqCst) < 2 {
            std::thread::sleep(Duration::from_millis(20));
            service.refresh_if_needed(&suggester, PluginKeepalive::empty());
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn service_skips_the_refresh_while_the_frame_is_fresh() {
        let _serial = COUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let mut suggester = suggester_for(counted_slow_handler);
        suggester.user_data = &CALLS as *const _ as *mut c_void;

        let service = SuggestService::with_ttl(Duration::from_secs(60));
        service.refresh_if_needed(&suggester, PluginKeepalive::empty()); // call 1
        wait_for_value(&service, "msg", "v1@h", 2);
        // Inside the 60 s window: repeated refresh_if_needed must not call
        // the plugin again (at most once per refresh window per command).
        for _ in 0..10 {
            service.refresh_if_needed(&suggester, PluginKeepalive::empty());
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "no call inside the window");
    }

    // ---- real-cdylib race test (review round 4, issue #1) ----
    //
    // The reclaim tests above use in-binary handlers, so they cannot catch a
    // use-after-unload: there is no cdylib to unmap. This one compiles a REAL
    // plugin with rustc, registers a SLOW suggester through the real ABI,
    // lets the bounded wait time out, then starts shutting the session down
    // while the late handler is still running. With the keepalive clone in
    // the FFI thread, the library must stay mapped (its still_mapped symbol
    // keeps resolving) until the late response was reclaimed; the late free
    // must not fault.
    #[cfg(unix)]
    mod real_dylib_race {
        use super::*;
        use crate::loader::load_session_mixed;
        use crate::NullDiagnostics;
        use std::path::PathBuf;
        use std::process::Command;
        use std::sync::atomic::AtomicU32;

        struct Fixture {
            dir: PathBuf,
            path: PathBuf,
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        /// Shared log the fixture writes to (via its exported `race_marker`),
        /// so the test observes the late handler's lifetime without touching
        /// plugin internals.
        struct Log {
            path: PathBuf,
        }

        impl Log {
            fn read(&self) -> String {
                std::fs::read_to_string(&self.path).unwrap_or_default()
            }
        }

        fn build_slow_suggester_fixture(name: &str, log: &Log) -> Fixture {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rpi-suggest-race-{}-{}-{name}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source_path = dir.join("fixture.rs");
            std::fs::write(&source_path, slow_suggester_source(log)).unwrap();
            let path = dir.join(format!("lib{name}.dylib"));
            let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .arg("--crate-name")
                .arg(name)
                .arg("--crate-type")
                .arg("cdylib")
                .arg("--edition")
                .arg("2021")
                .arg("-L")
                .arg(".")
                .arg(&source_path)
                .arg("-o")
                .arg(&path)
                .output()
                .expect("run rustc for suggester race fixture");
            assert!(
                output.status.success(),
                "fixture build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Fixture { dir, path }
        }

        fn slow_suggester_source(log: &Log) -> String {
            let log_path = log.path.to_string_lossy();
            // The ABI contract: request is a borrowed (ptr,len) pair, out is a
            // (ptr,len) output slot, user_data is the register-time bridge.
            // Slot offsets match rpi-plugin-sdk's PluginApi; the fixture only
            // reads what it registers through.
            format!(
                r#"
use std::ffi::{{c_char, c_void}};

#[repr(C)]
struct StbRef {{ ptr: *const c_char, len: usize }}
#[repr(C)]
struct StbOut {{ ptr: *mut c_char, len: usize }}

fn append_log(tag: &str) {{
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open({log_path:?}).unwrap();
    writeln!(f, "{{tag}}").unwrap();
}}

extern "C" fn slow_suggester(
    _request: StbRef,
    out: *mut StbOut,
    _ud: *mut c_void,
) -> i32 {{
    append_log("handler-begin");
    // Sleep past the host's 500 ms budget; the host must NOT unmap this
    // library while this thread is inside the handler or the late free.
    std::thread::sleep(std::time::Duration::from_millis(900));
    // A still-mapped library answers this exported symbol.
    let mapped = still_mapped();
    append_log(if mapped == 42 {{ "handler-still-mapped" }} else {{ "handler-unmapped" }});
    let payload = b"{{\"items\":[{{\"value\":\"late\"}}]}}";
    unsafe {{
        let buf = libc_malloc(payload.len());
        std::ptr::copy_nonoverlapping(payload.as_ptr(), buf as *mut u8, payload.len());
        (*out).ptr = buf as *mut c_char;
        (*out).len = payload.len() - 1;
    }}
    append_log("handler-written");
    0
}}

extern "C" fn free_string(value: StbOut) {{
    if value.ptr.is_null() {{ return; }}
    unsafe {{ libc_free(value.ptr as *mut c_void) }};
    append_log("late-free");
}}

#[no_mangle]
pub extern "C" fn still_mapped() -> i32 {{ 42 }}

unsafe extern "C" {{
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}}
use {{malloc as libc_malloc, free as libc_free}};

#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    // PluginApi.register_argument_suggester offset (appended last in the ABI).
    const OFF: usize = {offset};
    type RegisterFn = extern "C" fn(
        command: StbRef,
        handler: extern "C" fn(StbRef, *mut StbOut, *mut c_void) -> i32,
        plugin_free_string: extern "C" fn(StbOut),
        user_data: *mut c_void,
    ) -> i32;
    let slot = unsafe {{ std::ptr::read_unaligned(api.add(OFF) as *const Option<RegisterFn>) }};
    match slot {{
        Some(register) => {{
            let cmd = b"msg\\0";
            let request = StbRef {{ ptr: cmd.as_ptr() as *const c_char, len: 3 }};
            let rc = register(request, slow_suggester, free_string, std::ptr::null_mut());
            append_log(if rc == 0 {{ "registered" }} else {{ "register-rejected" }});
            rc
        }}
        None => -1,
    }}
}}
"#,
                log_path = log_path,
                offset =
                    std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_argument_suggester),
            )
        }

        fn wait_until(log: &Log, tag: &str, secs: u64) -> bool {
            {
                let deadline = Instant::now() + Duration::from_secs(secs);
                while Instant::now() < deadline {
                    {
                        if log.read().contains(tag) {
                            {
                                return true;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                false
            }
        }

        #[test]
        fn late_handler_keeps_the_cdylib_mapped_until_the_response_is_reclaimed() {
            {
                let log_dir = std::env::temp_dir()
                    .join(format!("rpi-suggest-race-log-{}", std::process::id()));
                std::fs::create_dir_all(&log_dir).unwrap();
                let log = Log {
                    path: log_dir.join("events.log"),
                };
                let _ = std::fs::remove_file(&log.path);

                let fixture = build_slow_suggester_fixture("suggest_race", &log);
                let session = load_session_mixed(
                    &[],
                    std::slice::from_ref(&fixture.path),
                    Arc::new(NullDiagnostics),
                    None,
                );
                assert!(
                    log.read().contains("registered"),
                    "fixture must register cleanly"
                );

                // Resolve the REAL suggester fn pointers from the loaded library.
                let snapshot = session.snapshot_arc().expect("snapshot");
                let suggester = snapshot
                    .argument_suggester("msg")
                    .cloned()
                    .expect("suggester registered");
                let service = SuggestService::new();
                let keepalive = session.keepalive();

                // Fire the call: the handler sleeps 900 ms, the host gives up at
                // 500 ms. Then drop the session — its own keepalive clone dies,
                // and the worker drops its clone when the bounded wait returns.
                // ONLY the FFI thread's clone keeps the library mapped from here.
                service.refresh_if_needed(&suggester, Arc::clone(&keepalive));
                assert!(wait_until(&log, "handler-begin", 3), "handler must start");
                std::thread::sleep(SUGGEST_TIMEOUT + Duration::from_millis(100)); // bounded wait gave up
                drop(service); // worker thread exits; its keepalive clone dies
                drop(keepalive);
                drop(session); // session shutdown + unload attempt

                // The late handler is STILL running (sleep ends ~900 ms). With
                // the fix it is running inside a still-mapped library; without,
                // this is a use-after-unload race the still_mapped() call inside
                // the handler often loses.
                assert!(
                    wait_until(&log, "handler-still-mapped", 5),
                    "late handler must run against a still-mapped library, log={}",
                    log.read()
                );
                // The late response was written and reclaimed through the
                // PLUGIN's own free fn (in the same late-mapped library).
                assert!(wait_until(&log, "handler-written", 3));
                assert!(wait_until(&log, "late-free", 3));
                let _ = std::fs::remove_dir_all(&log_dir);
            }
        }
    }
}
