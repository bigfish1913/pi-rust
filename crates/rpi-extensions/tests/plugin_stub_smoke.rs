//! End-to-end Part B2 smoke test: load the **real** `plugin-stub` cdylib
//! (`examples/plugin-stub`, built as a `.dll`/`.so`/`.dylib`) through the
//! production `load_session` path and drive its `echo` tool through the
//! `PluginToolAdapter` spawn_blocking bridge.
//!
//! This is the test the plan's verification §2 asks for: "adapter round-trips
//! a stub tool via the spawn_blocking bridge" — but against the **actual cdylib**
//! (not the in-process `extern "C"` stubs in `tool::tests`), so it exercises the
//! full `libloading` → `rpi_plugin_register` → `register_tool` trampoline →
//! keepalive → adapter path.
//!
//! ## Locating the cdylib
//!
//! The test binary lives under `<target>/debug/deps/` (or `release/deps/`), so
//! `current_exe()` → walk up two levels → `<target>/<profile>/` is where
//! `plugin_stub.{dll,so,dylib}` lands. We probe that dir (and the `deps/`
//! subdir as a fallback) and **skip** the test (not fail) if the cdylib isn't
//! there — building `plugin-stub` is an opt-in step (`cargo build -p
//! plugin-stub`), and we don't want a workspace `cargo test` to flip red just
//! because the example wasn't compiled.
//!
//! ## What's asserted
//!
//! 1. `load_session` loads exactly one plugin + one tool named `echo`.
//! 2. The `echo` adapter, driven via `AgentTool::execute`, returns
//!    `echo: <text>` and the 4-fn lifecycle completes (destroy fires — we can't
//!    observe destroy directly across the cdylib, but a clean return with no
//!    hang after the select! loop is the proof; the in-process tests assert the
//!    destroy-count invariant).
//! 3. A `MessageEnd` handler is registered in the snapshot (B3 will fire it;
//!    B2 only proves registration landed).
//! 4. (B5b) A `resources_discover` handler is registered, and
//!    `emit_resources_discover` fans the event to it → its canned skill path
//!    appears in the merged result, and the handler's process-global hit counter
//!    bumped. This is the plan's verification §3 B5 smoke: "plugin-stub
//!    registers a `resources_discover` handler → its skill path appears …".
//!
//! Run it after building the stub:
//! ```sh
//! cargo build -p plugin-stub && cargo test -p rpi-extensions --test plugin_stub_smoke
//! ```

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;


use rpi_agent::agent_tool::AgentTool;
use rpi_agent::types::{TextContentOrImage, ToolResultPartial};
use rpi_extensions::{PluginDiagnostics, PluginToolAdapter, load_session};
use rpi_plugin_sdk::EventTag;
use tokio_util::sync::CancellationToken;

/// Locate the built `plugin_stub` cdylib by walking up from the test binary's
/// own location (`<target>/<profile>/deps/`). Returns the cdylib path and the
/// dir it sits in (the dir is what `load_session` scans).
fn locate_stub() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    // exe ≈ <target>/debug/deps/rpi_extensions-<hash>.exe
    let deps = exe.parent()?; // .../deps
    let profile_dir = deps.parent()?; // <target>/debug  (or release)
    let cdylib = find_cdylib(profile_dir).or_else(|| find_cdylib(deps))?;
    let dir = cdylib.parent()?.to_path_buf();
    Some((cdylib, dir))
}

/// The cdylib filename per platform.
fn cdylib_filename() -> &'static str {
    if cfg!(windows) {
        "plugin_stub.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_stub.dylib"
    } else {
        "libplugin_stub.so"
    }
}

fn find_cdylib(dir: &std::path::Path) -> Option<PathBuf> {
    let p = dir.join(cdylib_filename());
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// A diagnostics sink that records warnings so the test can assert e.g. "no
/// ABI-mismatch skip happened".
#[derive(Default)]
struct RecordingDiag {
    warnings: std::sync::Mutex<Vec<String>>,
}
impl PluginDiagnostics for RecordingDiag {
    fn warn(&self, message: &str) {
        self.warnings.lock().unwrap().push(message.to_string());
    }
    fn unsupported(&self, _message: &str) {}
}

#[tokio::test]
async fn loads_real_cdylib_and_drives_echo_tool() {
    let Some((stub_path, dir)) = locate_stub() else {
        eprintln!("plugin_stub cdylib not built — skipping (run `cargo build -p plugin-stub`)");
        return;
    };
    eprintln!("smoke: loading {}", stub_path.display());

    let diag = Arc::new(RecordingDiag::default());
    let warnings = Arc::clone(&diag);
    let session = load_session(
        &[dir],
        Arc::clone(&diag) as Arc<dyn PluginDiagnostics>,
        None,
    );

    // No load warnings (ABI mismatch / skip would land here).
    let warned = warnings.warnings.lock().unwrap().clone();
    assert!(
        warned.is_empty(),
        "unexpected load diagnostics: {warned:?}"
    );
    assert!(!session.is_empty(), "expected the stub to load");
    assert_eq!(session.loaded_paths().len(), 1);

    let snapshot = session.snapshot().expect("snapshot present after a load");

    // One tool registered: echo.
    let tools = snapshot.tools();
    assert_eq!(tools.len(), 1, "echo should be the only registered tool");
    let echo = &tools[0];
    assert_eq!(echo.tool.name, "echo");

    // One event handler registered on MessageEnd (B2 minimum on() proof).
    let handlers = snapshot.handlers_for(EventTag::MessageEnd);
    assert_eq!(
        handlers.len(),
        1,
        "expected one MessageEnd handler registered"
    );

    // Drive the echo tool through the real adapter (spawn_blocking bridge over
    // the cdylib's fn pointers — the keepalive keeps the dll mapped).
    let adapter = PluginToolAdapter::new(echo.tool.clone(), echo.handle(), session.keepalive());
    let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
    let signal = CancellationToken::new();
    let result = adapter
        .execute("smoke_call_1", serde_json::json!({ "text": "hi" }), signal, on_update)
        .await
        .expect("echo execute should succeed");

    // The stub returns `echo: hi` as a single text block.
    assert_eq!(result.content.len(), 1, "result: {:?}", result.content);
    match &result.content[0] {
        TextContentOrImage::Text(t) => assert_eq!(t.text, "echo: hi"),
        other => panic!("expected text content, got {other:?}"),
    }
    assert!(!result.terminate, "echo should not terminate the session");

    // A second drive proves the adapter is reusable (destroy fired on the first
    // drive's handle; a fresh execute allocates a fresh handle).
    let on_update2: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
    let signal2 = CancellationToken::new();
    let _user: *mut c_void = std::ptr::null_mut();
    let _ = _user;
    let result2 = adapter
        .execute("smoke_call_2", serde_json::json!({ "text": "again" }), signal2, on_update2)
        .await
        .expect("second echo execute should succeed");
    match &result2.content[0] {
        TextContentOrImage::Text(t) => assert_eq!(t.text, "echo: again"),
        other => panic!("expected text content, got {other:?}"),
    }

    // ---- B3a: the ExtensionEmitter round-trips AgentEvent → handlers --------
    // The stub registered one `MessageEnd` handler that bumps a process-global
    // counter. Driving the emitter with a synthetic MessageEnd event proves the
    // full dispatch path (translate → catch_unwind fan-out → plugin handler) is
    // wired against the real cdylib. We read the counter back via the cdylib's
    // exported `plugin_stub_message_end_hits` accessor (looked up the same way
    // the loader looks up `rpi_plugin_register`).
    use rpi_extensions::ExtensionEmitter;
    use rpi_agent::events::AgentEmitter;
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::{AssistantMessage, Content, Usage};

    let snapshot = session.snapshot_arc().expect("snapshot present");
    let emitter = ExtensionEmitter::new(snapshot, session.keepalive());
    let am = AgentMessage::Assistant(Box::new(AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![Content::text("smoke")],
        api: rpi_ai::types::Api::AnthropicMessages,
        provider: "anthropic".to_string(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: rpi_ai::types::StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }));
    let before = stub_message_end_hits(&stub_path);
    emitter.try_emit(rpi_agent::AgentEvent::MessageEnd { message: am });
    let after = stub_message_end_hits(&stub_path);
    assert_eq!(
        after, before + 1,
        "MessageEnd handler in the real cdylib should have fired once"
    );

    // The session's keepalive is still alive (adapter holds a clone), so the
    // cdylib stays mapped; dropping the adapter + session unloads it.

    // ---- B5b: resources_discover round-trip through the real cdylib ----------
    // The stub registered a `resources_discover` handler that returns a canned
    // skill path. `emit_resources_discover` fans the event to every registered
    // handler in registration order, merges their `{skillPaths, promptPaths,
    // themePaths}` arrays, and returns the concat. We assert:
    //   (a) exactly one discover handler registered;
    //   (b) the handler fired (its process-global counter bumped);
    //   (c) its canned skill path appears in the merged result.
    // This is the plan verification §3 B5 smoke ("plugin-stub registers a
    // resources_discover handler → its skill path appears …").
    use rpi_extensions::emit_resources_discover;

    let snap = session.snapshot_arc().expect("snapshot present");
    let handlers = snap.resources_discover();
    assert_eq!(
        handlers.len(),
        1,
        "expected exactly one resources_discover handler registered"
    );

    let discover_before = stub_discover_hits(&stub_path);
    let discovered = emit_resources_discover("/cwd", "startup", &snap);
    let discover_after = stub_discover_hits(&stub_path);
    assert_eq!(
        discover_after,
        discover_before + 1,
        "resources_discover handler in the real cdylib should have fired once"
    );
    // The stub advertises exactly one skill path (its marker string). It must
    // land in the merged `skill_paths`; prompt/theme stay empty (the stub
    // returns only skillPaths).
    assert_eq!(
        discovered.skill_paths.len(),
        1,
        "one skill path from one handler"
    );
    assert_eq!(
        discovered.skill_paths[0], "plugin-stub-discovered/SKILL.md",
        "the canned path the stub advertises must round-trip unchanged"
    );
    assert!(
        discovered.prompt_paths.is_empty(),
        "stub returns no promptPaths"
    );
    assert!(
        discovered.theme_paths.is_empty(),
        "stub returns no themePaths"
    );
}

/// Read the stub's `plugin_stub_discover_hits` counter through the cdylib
/// (second mapping — refcounted, same pattern as `stub_message_end_hits`).
fn stub_discover_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe {
        lib.get(b"plugin_stub_discover_hits\0")
    } {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_message_end_hits` counter through the cdylib.
/// Loads the library fresh (a second mapping — cdylibs are refcounted on Windows
/// / dlopen-refcounted on Unix, so a second open is fine and the first open in
/// the session's keepalive stays alive). Returns 0 if the symbol isn't found
/// (defensive; the stub exports it, but a stale build might not).
fn stub_message_end_hits(stub_path: &std::path::Path) -> usize {
    // Hold the second mapping alive across the symbol call by keeping `lib`
    // in scope until after `sym()` returns (the Symbol borrows lib).
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe {
        lib.get(b"plugin_stub_message_end_hits\0")
    } {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}
