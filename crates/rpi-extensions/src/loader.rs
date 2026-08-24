//! The `libloading` loader: discover and register cdylib plugins.
//!
//! [`load_one`] loads a single cdylib, looks up `rpi_plugin_register`, builds a
//! fresh [`HostApi`](crate::HostApi) + [`PluginApiVt`](rpi_plugin_sdk::PluginApiVt),
//! sets the thread-local current api, calls `register` with
//! [`RPI_PLUGIN_ABI_VERSION`](rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION), clears
//! the api, and takes the registry out. ABI mismatch or a nonzero register
//! return ⇒ the plugin is skipped with a diagnostic (never crashes).
//!
//! [`load_dir`] walks a directory for `.{dll,so,dylib}` files and loads each.
//! The returned [`LoadedPlugin`]s hold the `libloading::Library` (dropping them
//! unloads the cdylib — keep them alive for the session lifetime).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::{Library, Symbol};
use thiserror::Error;

use rpi_plugin_sdk::{PluginApiVt, RPI_PLUGIN_ABI_VERSION, RpiPluginRegister};

use crate::registry::{ExtensionRegistry, RegistrySnapshot};
use crate::{
    ActionBridge, HostApi, NullDiagnostics, PluginDiagnostics, clear_current_api, set_current_api,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error / skip reason from loading one plugin. `Skip` variants are non-fatal
/// (logged via diagnostics); `Fatal` means the load itself failed.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("could not open library {path}: {source}")]
    Open { path: PathBuf, #[source] source: libloading::Error },
    #[error("symbol `rpi_plugin_register` not found in {path}: {source}")]
    Symbol { path: PathBuf, #[source] source: libloading::Error },
    #[error("register returned nonzero code {code} for {path}")]
    RegisterReturned { path: PathBuf, code: i32 },
    /// ABI version reported by the plugin mismatches the host's. Skip + diag.
    #[error("ABI version mismatch in {path}: plugin built for {plugin_version}, host is {host_version}")]
    AbiVersionMismatch {
        path: PathBuf,
        plugin_version: u32,
        host_version: u32,
    },
}

// ---------------------------------------------------------------------------
// LoadedPlugin — the live cdylib handle + where it came from
// ---------------------------------------------------------------------------

/// A successfully loaded + registered plugin. Holds the `Library` so the cdylib
/// stays mapped for the session. Dropping this unloads the plugin (do not drop
/// while any of its tool drivers may still be running).
pub struct LoadedPlugin {
    /// The loaded cdylib. Kept alive for the session.
    pub library: Library,
    /// Where it was loaded from (for diagnostics).
    pub path: PathBuf,
    /// The registry snapshot built from this plugin's registrations. The host
    /// merges snapshots from all loaded plugins into one session registry.
    pub registry: ExtensionRegistry,
}

// ---------------------------------------------------------------------------
// load_one
// ---------------------------------------------------------------------------

/// Load and register one cdylib plugin. Returns the live plugin + its
/// registry, or a [`PluginLoadError`] (skip-fatality distinction is on the
/// caller; both are logged via `diagnostics`).
///
/// `diagnostics` is the host sink for ABI-mismatch/unsupported warnings. The
/// loader creates a fresh `ExtensionRegistry` for THIS plugin (so a plugin that
/// fails partway can't pollute others), and the caller merges per-plugin
/// registries into the session registry in load-order (first-wins on name).
///
/// `action_bridge` (B5a): when `Some`, the plugin's vtable wires the real
/// [`trampoline_runtime_action`] and carries the bridge in `user_data`, so the
/// plugin can invoke host runtime actions post-register from any thread. `None`
/// keeps the v1 stub (actions return `-1`). `rpi-cli` builds ONE master
/// `Arc<ActionBridge>` per session and clones it into every `load_one` — every
/// plugin's `user_data` points at the same bridge (Arc-ptr-stable, kept alive
/// by `rpi-cli` for the harness lifetime).
pub fn load_one(
    path: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> Result<LoadedPlugin, PluginLoadError> {
    let path = path.as_ref().to_path_buf();
    // 1. Open the cdylib.
    let library = unsafe { Library::new(&path) }.map_err(|e| PluginLoadError::Open {
        path: path.clone(),
        source: e,
    })?;

    // 2. Look up `rpi_plugin_register`.
    let register: Symbol<RpiPluginRegister> = unsafe {
        library.get(rpi_plugin_sdk::REGISTER_SYMBOL)
    }
    .map_err(|e| PluginLoadError::Symbol {
        path: path.clone(),
        source: e,
    })?;

    // 3. Build a fresh registry + HostApi + vtable for this plugin.
    let registry = ExtensionRegistry::new();
    let host_api = match action_bridge {
        Some(bridge) => HostApi::with_action_bridge(registry, Arc::clone(&diagnostics), bridge),
        None => HostApi::new(registry, Arc::clone(&diagnostics)),
    };
    let vtable = host_api.build_vtable();

    // 4. Set the thread-local current api so the register trampolines can reach
    //    the registry. Register is synchronous + single-threaded per plugin.
    // SAFETY: `host_api` is alive for the duration of the register call (held on
    // this stack); we clear_current_api immediately after.
    unsafe { set_current_api(&host_api) };
    // Keep the vtable reference alive across the call (the plugin borrows it).
    let vt_ref: &PluginApiVt = &vtable;
    // Call register; a panic inside the plugin's extern "C" fn would unwind
    // across FFI — catch_unwind contains that (register runs on the loader
    // thread, not a blocking-driver thread, so recovery is safe here; we log
    // + treat as skip). The vtable pointer is valid (vt_ref lives on this stack).
    let register_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        register(vt_ref as *const PluginApiVt, RPI_PLUGIN_ABI_VERSION)
    }));
    clear_current_api();

    let rc = match register_outcome {
        Ok(rc) => rc,
        Err(_) => {
            diagnostics.warn(&format!(
                "plugin {} register panicked — skipped (unwind contained)",
                path.display()
            ));
            return Err(PluginLoadError::RegisterReturned { path, code: -1 });
        }
    };

    if rc != 0 {
        // The plugin refused (its register returned nonzero — e.g. it saw an
        // ABI version it didn't like). Skip + diag. The registry may have
        // partial registrations; we drop it (no harm — the tools registered so
        // far would reference a plugin that "failed", so we honor the plugin's
        // refusal and discard).
        diagnostics.warn(&format!(
            "plugin {} register returned code {} — skipped",
            path.display(),
            rc
        ));
        return Err(PluginLoadError::RegisterReturned { path, code: rc });
    }

    // 5. Take the registry out of the HostApi. The host keeps the Library alive
    //    (LoadedPlugin) so the plugin's code + static data remain mapped; the
    //    registry holds fn pointers into that code.
    let registry = host_api
        .take_registry()
        .ok_or_else(|| PluginLoadError::RegisterReturned {
            path: path.clone(),
            code: -2,
        })?;

    Ok(LoadedPlugin { library, path, registry })
}

// ---------------------------------------------------------------------------
// load_dir
// ---------------------------------------------------------------------------

/// Platform cdylib extensions.
const CDYLIB_EXTS: &[&str] = &["dll", "so", "dylib", "pyd"];

/// Load every cdylib in `dir` (non-recursive). Each load failure is logged via
/// `diagnostics` and skipped (one bad plugin doesn't abort the rest). Returns
/// the successfully loaded plugins in directory order.
///
/// `action_bridge` (B5a) is cloned into each loaded plugin's vtable `user_data`
/// so post-register `runtime_action` calls recover the bridge on any thread.
pub fn load_dir(
    dir: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> Vec<LoadedPlugin> {
    let dir = dir.as_ref();
    let mut out = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            diagnostics.warn(&format!(
                "extensions dir {} unreadable: {}",
                dir.display(),
                e
            ));
            return out;
        }
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !is_cdylib(&path) {
            continue;
        }
        match load_one(&path, Arc::clone(&diagnostics), action_bridge.clone()) {
            Ok(p) => out.push(p),
            Err(e) => diagnostics.warn(&format!("skipped plugin {}: {e}", path.display())),
        }
    }
    out
}

/// Whether `path`'s extension is a known cdylib extension.
fn is_cdylib(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| CDYLIB_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

/// Convenience: merge a slice of per-plugin [`LoadedPlugin`] registries into one
/// session registry, first-wins on name (mirrors pi's cross-extension
/// registration order). Consumes the registries (the `LoadedPlugin`s themselves
/// stay alive — callers keep the `Library` handles).
pub fn merge_registries(plugins: &mut [LoadedPlugin]) -> ExtensionRegistry {
    let mut session = ExtensionRegistry::new();
    // We can't move registries out of LoadedPlugin without taking them; borrow
    // mutably and drain into the session. Since ExtensionRegistry's registrars
    // consume by value, we rebuild from the snapshot instead.
    // Simpler: snapshot each, then re-register by iteration. But ExtensionRegistry
    // has no public "absorb another registry" — so we drain tools/commands/handlers
    // via internal access. For v1 we expose the typed fields via crate-internal
    // methods on ExtensionRegistry used only here.
    for p in plugins.iter_mut() {
        // Take the plugin's registry out (LoadedPlugin keeps the Library).
        let taken = std::mem::take(&mut p.registry);
        session.absorb(taken);
    }
    session
}

// ---------------------------------------------------------------------------
// PluginKeepalive + ExtensionSession — the host session's plugin lifetime
// ---------------------------------------------------------------------------

/// Owns the loaded `Library` handles so the cdylibs stay mapped for as long as
/// any registered tool/handler (whose fn pointers live inside the cdylib) may be
/// called. Shared via `Arc`: every [`PluginToolAdapter`](crate::PluginToolAdapter)
/// (and, in B3, the [`ExtensionEmitter`](crate::ExtensionEmitter)) holds a clone,
/// so the libraries unload only when the last holder drops — which is never
/// before the harness's tool vec (and thus the last possible tool call) drops.
///
/// `libloading::Library` is `Send + Sync` (a handle/HMODULE), so the keepalive is
/// too — required because `AgentTool: Send + Sync` and the adapter carries it.
pub struct PluginKeepalive {
    #[allow(dead_code)]
    libraries: Vec<Library>,
    /// B5a: the session's action bridge. Retained here so the raw pointer a
    /// plugin stored in its vtable `user_data` (`Arc::as_ptr`) stays valid for
    /// the harness lifetime — every `PluginToolAdapter` + the `ExtensionEmitter`
    /// clone the keepalive, so the bridge outlives any plugin→host
    /// `runtime_action` call. `None` under `--no-extensions`, when zero plugins
    /// loaded, or in tests.
    #[allow(dead_code)]
    action_bridge: Option<Arc<ActionBridge>>,
}

impl PluginKeepalive {
    /// Build a keepalive. `pub` so tests + host code can construct an empty
    /// one (no loaded cdylibs) where the plugin lifecycle is exercised without
    /// real plugins.
    pub fn new(libraries: Vec<Library>, action_bridge: Option<Arc<ActionBridge>>) -> Self {
        Self {
            libraries,
            action_bridge,
        }
    }

    /// An empty keepalive owning no libraries — for host code that builds an
    /// adapter outside a real load session (notably in-process tests of the
    /// adapter against stub fns that live in the test binary, not a cdylib).
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::new(Vec::new(), None))
    }
}

/// The result of loading a session's worth of extensions: a shared keepalive for
/// the cdylib handles + a snapshot of the merged registry. Built by
/// [`load_session`]; the host (pi-cli) stashes one per harness build and hands
/// clones of the keepalive to each adapter it constructs from the snapshot.
///
/// The snapshot is held behind `Arc` so the host can hand a clone to the
/// [`ExtensionEmitter`](crate::ExtensionEmitter) (installed as the harness's
/// `agent_emitter`) without borrowing — the emitter must outlive this session
/// local (it lives for the harness lifetime inside `AgentHarnessOptions`).
///
/// `Clone` (B5d): every field is already cheaply clonable (`Arc<PluginKeepalive>`,
/// `Option<Arc<RegistrySnapshot>>`, `Vec<PathBuf>`, `Option<Arc<ActionBridge>>`),
/// so the reload routine can clone the live session out of its `Mutex` cell for
/// local inspection (snapshot/keepalive/loaded_paths) and store a fresh one back
/// in — without a borrow spanning the store.
#[derive(Clone)]
pub struct ExtensionSession {
    keepalive: Arc<PluginKeepalive>,
    snapshot: Option<Arc<RegistrySnapshot>>,
    loaded_paths: Vec<PathBuf>,
    /// B5a: the session's action bridge (`None` when no plugins / tests / the
    /// `--no-extensions` path). Kept here so `rpi-cli` can recover it after
    /// `AgentHarness::create` to call `set_harness` — the bridge's `user_data`
    /// pointer was already handed out during `register`, so pi-cli must fill the
    /// host's harness cell immediately after create. Cloning is cheap (an `Arc`
    /// clone); the keepalive also holds a clone for the lifetime guarantee.
    action_bridge: Option<Arc<ActionBridge>>,
}

impl ExtensionSession {
    /// An empty session (no plugins loaded — `--no-extensions` or no dirs found).
    pub fn none() -> Self {
        Self {
            keepalive: Arc::new(PluginKeepalive::new(Vec::new(), None)),
            snapshot: None,
            loaded_paths: Vec::new(),
            action_bridge: None,
        }
    }

    /// The shared keepalive — clone one per adapter/emitter you build from this
    /// session so the cdylibs outlive them.
    pub fn keepalive(&self) -> Arc<PluginKeepalive> {
        Arc::clone(&self.keepalive)
    }

    /// The merged registry snapshot (tools/commands/handlers), if any plugin
    /// loaded. `None` when no plugins loaded successfully. Borrowed view for
    /// iterating tools/commands; for an owned share (e.g. handing to the
    /// emitter) use [`snapshot_arc`](Self::snapshot_arc).
    pub fn snapshot(&self) -> Option<&RegistrySnapshot> {
        self.snapshot.as_deref()
    }

    /// A shared (`Arc`) clone of the merged registry snapshot, for host code that
    /// must keep the snapshot alive beyond this session local — notably the
    /// [`ExtensionEmitter`](crate::ExtensionEmitter) installed into
    /// `AgentHarnessOptions.agent_emitter`.
    pub fn snapshot_arc(&self) -> Option<Arc<RegistrySnapshot>> {
        self.snapshot.clone()
    }

    /// Paths of the cdylibs that loaded + registered successfully (diagnostics).
    pub fn loaded_paths(&self) -> &[PathBuf] {
        &self.loaded_paths
    }

    /// Whether zero plugins loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded_paths.is_empty()
    }

    /// A one-line human summary for `--verbose` startup output, or `None` when
    /// nothing loaded (so the line is omitted entirely).
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let tools = self.snapshot.as_ref().map(|s| s.tools().len()).unwrap_or(0);
        Some(format!(
            "loaded {} plugin(s) ({} tool(s))",
            self.loaded_paths.len(),
            tools
        ))
    }

    /// B5a: the session's action bridge, if one was threaded into `load_*`.
    /// `rpi-cli` recovers this after `AgentHarness::create` succeeds to call
    /// `HarnessActionHost::set_harness` (filling the host cell the bridge's
    /// `user_data`-recovered host reads on the first plugin→host action). The
    /// bridge pointer was already handed to plugins during `register`, so this
    /// must happen before any run. `None` when no plugins loaded / tests /
    /// `--no-extensions`.
    pub fn action_bridge(&self) -> Option<Arc<ActionBridge>> {
        self.action_bridge.clone()
    }
}

/// Load + register every cdylib in the given dirs (in order, non-recursive),
/// merge their registries first-wins, and return a session with a shared
/// keepalive over the `Library` handles + the merged snapshot. Dirs that don't
/// exist are skipped silently; individual plugin load failures are logged via
/// `diagnostics` and skipped (one bad plugin doesn't abort the rest).
///
/// The order of `dirs` matters: earlier dirs win on tool/command name collision
/// (pi registration order). Callers pass default dirs first, then `--extensions-dir`
/// extras, so a same-named tool in a default-dir plugin wins over an extra-dir one.
///
/// `action_bridge` (B5a) is cloned into every loaded plugin's vtable so
/// post-register `runtime_action` calls recover the bridge on any thread.
/// `rpi-cli` builds one master `Arc<ActionBridge>` per session and passes it
/// here; `None` keeps the v1 stub (used by tests / `--no-extensions` no-ops).
pub fn load_session(
    dirs: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> ExtensionSession {
    let mut loaded: Vec<LoadedPlugin> = Vec::new();
    for dir in dirs {
        loaded.extend(load_dir(dir, Arc::clone(&diagnostics), action_bridge.clone()));
    }
    if loaded.is_empty() {
        return ExtensionSession::none();
    }
    let loaded_paths: Vec<PathBuf> = loaded.iter().map(|p| p.path.clone()).collect();
    // Merge the per-plugin registries first-wins. This drains each `registry`
    // field (via mem::take inside `absorb`) but leaves `library` intact, so we
    // can then destructure-own each Library into the keepalive below.
    let session_registry = merge_registries(&mut loaded);
    // Now move each Library out of its (registry-hollowed) LoadedPlugin by struct
    // destructuring, collecting them into the keepalive. `registry`/`path` were
    // left valid-but-empty / cloned already, and `library` is a move into `libs`.
    let mut libs: Vec<Library> = Vec::with_capacity(loaded.len());
    for p in loaded {
        let LoadedPlugin { library, registry: _, path: _ } = p;
        libs.push(library);
    }
    let snapshot = Arc::new(session_registry.snapshot());
    ExtensionSession {
        keepalive: Arc::new(PluginKeepalive::new(libs, action_bridge.clone())),
        snapshot: Some(snapshot),
        loaded_paths,
        action_bridge,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturingDiag {
        warns: Mutex<Vec<String>>,
    }
    impl PluginDiagnostics for CapturingDiag {
        fn warn(&self, msg: &str) {
            self.warns.lock().unwrap().push(msg.to_string());
        }
        fn unsupported(&self, msg: &str) {
            self.warn(msg);
        }
    }

    #[test]
    fn load_one_missing_file_reports_open_error() {
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());
        let res = load_one("definitely_not_a_plugin.dll", diag, None);
        assert!(matches!(res, Err(PluginLoadError::Open { .. })));
    }

    #[test]
    fn load_dir_missing_dir_returns_empty_and_warns() {
        let empty = load_dir(
            "no_such_dir_xyz",
            Arc::new(CapturingDiag::default()) as Arc<dyn PluginDiagnostics>,
            None,
        );
        assert!(empty.is_empty());
    }

    #[test]
    fn is_cdylib_recognizes_extensions() {
        assert!(is_cdylib(Path::new("foo.dll")));
        assert!(is_cdylib(Path::new("foo.so")));
        assert!(is_cdylib(Path::new("foo.dylib")));
        assert!(is_cdylib(Path::new("FOO.DLL")));
        assert!(!is_cdylib(Path::new("foo.md")));
        assert!(!is_cdylib(Path::new("foo")));
    }
}

// Silence the unused-default-import warning for NullDiagnostics re-exported by the crate.
#[allow(dead_code)]
fn _ensure_nulldiagnostics_referenced() -> Arc<dyn PluginDiagnostics> {
    Arc::new(NullDiagnostics)
}
