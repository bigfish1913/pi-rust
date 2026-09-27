//! End-to-end smoke test for the **push-to-talk** host plumbing, driven through
//! the real `rpi_voice` cdylib.
//!
//! ## Why this test exists
//!
//! Push-to-talk spans two repos and an FFI boundary:
//!
//! 1. the plugin calls `register_shortcut("space", …)` to claim the key,
//! 2. it subscribes an `Input` event handler,
//! 3. the host TUI routes key presses/releases to that handler, and
//! 4. the handler's return code decides whether the key is swallowed.
//!
//! Steps 1–2 happen inside the cdylib and are invisible to unit tests in either
//! crate — a `register_shortcut` trampoline regression (or a plugin that forgets
//! to subscribe `Input`) would ship silently. This test loads the **actual built
//! cdylib** through the production `load_session` path and asserts the host ends
//! up with both registrations, so the wiring is verified end to end.
//!
//! ## Locating the cdylib
//!
//! The test binary lives under `<target>/<profile>/deps/`, so `current_exe()` →
//! up two levels is where `rpi_voice.{dll,so,dylib}` lands. We **skip** (not
//! fail) when it isn't built — `rpi-voice` lives in the *other* repo
//! (`rpi-package`), so a plain `cargo test` here must not go red just because
//! that crate wasn't compiled.
//!
//! Build it first:
//! ```sh
//! # in ../rpi-package
//! cargo build -p rpi-voice --release --features local-stt
//! ```
//! The test uses `RPI_VOICE_PTT_KEY` to assert the key name is honored rather
//! than hard-coding `space`.

use std::path::PathBuf;
use std::sync::Arc;

use rpi_extensions::{load_session, PluginDiagnostics, RegistrySnapshot};
use rpi_plugin_sdk::EventTag;

/// Diagnostics sink that records warnings (an ABI skip would land here).
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

fn cdylib_filename() -> &'static str {
    if cfg!(windows) {
        "rpi_voice.dll"
    } else if cfg!(target_os = "macos") {
        "librpi_voice.dylib"
    } else {
        "librpi_voice.so"
    }
}

/// Locate the built `rpi_voice` cdylib by walking up from the test binary.
/// Also accepts the built artifact directly out of the sibling repo's target
/// dir, since `rpi-voice` is a different workspace.
fn locate_cdylib() -> Option<PathBuf> {
    let name = cdylib_filename();

    // 1. This workspace's own target dir (if rpi-voice was ever built here).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(deps) = exe.parent() {
            for dir in [deps.parent(), Some(deps)] {
                if let Some(dir) = dir {
                    let candidate = dir.join(name);
                    if candidate.exists() {
                        return Some(candidate);
                    }
                    // rpi-package builds into its own (or the shared) target dir.
                    for sub in ["release", "debug"] {
                        let candidate = dir.join(sub).join(name);
                        if candidate.exists() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
    }

    // 2. The sibling repo's target dir, resolved relative to this crate.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest.parent()?.parent()?; // <pi-rust>/crates/rpi-extensions -> <pi-rust>
    let sibling = repo_root.parent()?.join("rpi-package");
    for profile in ["release", "debug"] {
        let candidate = sibling.join("target").join(profile).join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn load_voice() -> Option<(Arc<RecordingDiag>, Arc<RegistrySnapshot>)> {
    let src = locate_cdylib()?;
    // Isolate the scan: copy into a temp dir so unrelated cdylibs in a shared
    // target dir never produce diagnostics that would trip our assertions.
    let scratch = tempfile::tempdir().ok()?;
    let path = scratch.path().join(cdylib_filename());
    std::fs::copy(&src, &path).ok()?;
    eprintln!("ptt smoke: loading {}", src.display());

    let diag = Arc::new(RecordingDiag::default());
    let session = load_session(
        &[scratch.path().to_path_buf()],
        Arc::clone(&diag) as Arc<dyn PluginDiagnostics>,
        None,
    );

    // Take a shared snapshot before dropping the session: the snapshot holds the
    // plugin's fn pointers, and `snapshot_arc` is an owned handle to the merged
    // registry (so the session — and thus the cdylib — must outlive it).
    let snapshot = session.snapshot_arc();
    // Leak the session deliberately: dropping it unmaps the cdylib while the
    // snapshot still points into it. The OS reclaims everything at process exit,
    // and this test loads exactly one plugin once.
    std::mem::forget(session);
    snapshot.map(|s| (diag, s))
}

#[test]
fn voice_registers_ptt_shortcut_and_input_handler() {
    let Some((diag, snapshot)) = load_voice() else {
        eprintln!(
            "rpi_voice cdylib not built — skipping \
             (build it with `cargo build -p rpi-voice --release` in ../rpi-package)"
        );
        return;
    };

    let warned = diag.warnings.lock().unwrap().clone();
    assert!(warned.is_empty(), "unexpected load diagnostics: {warned:?}");

    // The key the plugin claimed. Defaults to `space`.
    let key = std::env::var("RPI_VOICE_PTT_KEY")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "space".to_string());

    // 1. The shortcut was recorded by the host's registry (this is the
    //    `register_shortcut` trampoline — previously a no-op stub).
    assert!(
        snapshot.has_shortcut(&key),
        "plugin did not claim `{key}`; shortcuts={:?}",
        snapshot.shortcuts()
    );
    let shortcut = snapshot
        .shortcuts()
        .iter()
        .find(|s| s.key == key)
        .expect("shortcut present");
    assert_eq!(shortcut.plugin, "rpi_voice");

    // 2. An `Input` handler is subscribed, which is how the plugin receives the
    //    press/release events the TUI routes for a claimed key.
    let handlers = snapshot.handlers_for(EventTag::Input);
    assert!(
        !handlers.is_empty(),
        "plugin did not subscribe the Input event"
    );

    // 3. Sanity: the extension still registers its slash command + TTS hook, so
    //    a regression that dropped unrelated registration would be caught too.
    assert!(
        snapshot.commands().iter().any(|c| c.name == "voice"),
        "the /voice command went missing"
    );
    assert_eq!(
        snapshot.handlers_for(EventTag::MessageEnd).len(),
        1,
        "the auto-TTS MessageEnd handler went missing"
    );
}

/// A non-claimed key must not be swallowed: `has_shortcut` is the TUI's routing
/// predicate, so verify it only answers true for the claimed key.
#[test]
fn only_the_claimed_key_is_routed() {
    let Some((_, snapshot)) = load_voice() else {
        return;
    };
    assert!(!snapshot.has_shortcut("tab"));
    assert!(!snapshot.has_shortcut("a"));
    assert!(!snapshot.has_shortcut("enter"));
}
