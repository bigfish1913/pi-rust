//! B5a — the plugin→host `runtime_action` bridge (inverted FFI).
//!
//! A plugin invokes `PluginApiVt::runtime_action` to drive the harness (send a
//! message, switch models, fork a session, reload extensions, …). Unlike the
//! register trampolines — which run synchronously inside `rpi_plugin_register`
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
use std::sync::{mpsc, Arc};

use rpi_plugin_sdk::{RuntimeActionId, StbString, StbStringRef};
use tokio::runtime::Handle;

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
/// The 16 methods map 1:1 to [`RuntimeActionId`]; `dispatch` below is the
/// exhaustive switch that connects the FFI id to the method.
#[async_trait::async_trait]
pub trait RuntimeActionHost: Send + Sync {
    /// `SendMessage` — drive a full agent run from an assistant/user message.
    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SendUserMessage` — drive a run from a user-text message.
    async fn send_user_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
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
    async fn get_thinking_level(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetThinkingLevel` — set the thinking level.
    async fn set_thinking_level(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `Compact` — compact the session transcript.
    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetSystemPrompt` — the live composed system prompt.
    async fn get_system_prompt(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
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
}

/// The host-side bridge carried in [`PluginApiVt::user_data`] so
/// [`trampoline_runtime_action`] can recover the harness state from any thread.
///
/// `runtime: Handle` is captured at build time (the host is on the runtime when
/// it constructs the bridge) — this is the foreign-thread fix. `host` is the
/// `rpi-cli` impl over the harness. `reload` is a CLI-owned callback that
/// re-runs extension discovery + Part-A loaders (a CLI concern, NOT a harness
/// op) — wired by `rpi-cli` in B5d; `None` until then.
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
    pub(crate) reload: Option<Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
}

impl ActionBridge {
    /// Build a bridge. The `Handle` MUST be captured from a thread running the
    /// target runtime (pi-cli builds the bridge on the async main thread).
    pub fn new(runtime: Handle, host: Arc<dyn RuntimeActionHost>) -> Arc<Self> {
        Arc::new(Self { runtime, host, reload: None })
    }

    /// Same as [`new`](Self::new) with a reload callback (B5d wires this).
    pub fn with_reload(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self { runtime, host, reload: Some(reload) })
    }
}

// `Handle` is Send+Sync, `Arc<dyn RuntimeActionHost>` (with `Send + Sync` bound)
// is Send+Sync, and the reload closure is `Send + Sync` — so `ActionBridge` is
// naturally Send+Sync; no manual unsafe impl needed.

/// Dispatch one action to the host. Async — runs on the bridge's runtime.
/// Handles all 16 ids; `Reload` delegates to [`RuntimeActionHost::reload`]
/// (the "no callback configured" fallback). When the bridge has a reload
/// callback, the spawn site intercepts `Reload` and awaits the callback
/// instead (a CLI concern, not a harness op) — this helper is the plain
/// host-only path.
async fn dispatch(
    host: &Arc<dyn RuntimeActionHost>,
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
/// - `-1` — no bridge present (`user_data` null; should not happen when wired).
/// - `-2` — the spawned task dropped its sender without sending (runtime
///   shutdown / dispatch panic); no result available.
///
/// The whole body is `catch_unwind`-wrapped — a panic across FFI ⇒ abort (same
/// policy as the tool partial callback, `tool.rs:206`).
pub extern "C" fn trampoline_runtime_action(
    action: RuntimeActionId,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_action(action, args_json, out, user_data)
    }));
    match outcome {
        Ok(rc) => rc,
        Err(_) => {
            tracing::error!(
                "runtime_action trampoline panicked — aborting (cannot unwind across FFI)"
            );
            std::process::abort();
        }
    }
}

/// Inner synchronous driver (split out so the `catch_unwind` wrapper is clean).
fn run_action(
    action: RuntimeActionId,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    if user_data.is_null() {
        return -1;
    }
    // SAFETY: the host guarantees `user_data` points at a live `ActionBridge`.
    // `rpi-cli` builds one `Arc<ActionBridge>` per session and keeps it for the
    // harness lifetime; `Arc::as_ptr` is pointer-stable while any clone lives.
    // We only borrow for the duration of this call.
    let bridge: &ActionBridge = unsafe { &*(user_data as *const ActionBridge) };

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
    let reload_cb = bridge.reload.clone();
    bridge.runtime.spawn(async move {
        let r = if action == RuntimeActionId::Reload {
            if let Some(cb) = reload_cb {
                cb().await;
                Ok(serde_json::Value::Null)
            } else {
                host.reload(args).await
            }
        } else {
            dispatch(&host, action, args).await
        };
        // If the plugin thread already moved on (dropped rx), discard — a send
        // error is NOT a host fault.
        let _ = tx.send(r);
    });

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
        async fn send_user_message(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn append_entry(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_session_name(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_active_tools(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_active_tools(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_model(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_thinking_level(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_thinking_level(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn compact(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_system_prompt(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.saw.lock().unwrap().push(RuntimeActionId::GetSystemPrompt);
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
            RuntimeActionId::GetSystemPrompt,
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
            RuntimeActionId::GetSystemPrompt,
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
            RuntimeActionId::Reload,
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
}
