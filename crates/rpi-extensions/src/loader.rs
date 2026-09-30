//! The `libloading` loader: discover and register cdylib plugins.
//!
//! [`load_one`] loads a single cdylib and looks up the unified
//! `rpi_plugin_register` entrypoint (ABI v4). The entrypoint is called exactly
//! once; a nonzero return is reported via diagnostics.
//!
//! [`load_dir`] walks a directory for `.{dll,so,dylib}` files and loads each.
//! The returned [`LoadedPlugin`]s hold the `libloading::Library` (dropping them
//! unloads the cdylib — keep them alive for the session lifetime).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use thiserror::Error;

use rpi_plugin_sdk::{
    RpiPluginRegisterUnified, RPI_PLUGIN_ABI_VERSION_UNIFIED,
};

use crate::registry::{ExtensionRegistry, RegistrySnapshot};
use crate::{
    clear_current_api, set_current_api, ActionBridge, HostApi, NullDiagnostics, PluginDiagnostics,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error / skip reason from loading one plugin. `Skip` variants are non-fatal
/// (logged via diagnostics); `Fatal` means the load itself failed.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("could not open library {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error(
        "`rpi_plugin_register` was not found in {path}: {error}"
    )]
    Symbol {
        path: PathBuf,
        error: String,
    },
    #[error("register returned nonzero code {code} for {path}")]
    RegisterReturned { path: PathBuf, code: i32 },
    /// ABI version reported by the plugin mismatches the host's. Skip + diag.
    #[error(
        "ABI version mismatch in {path}: plugin built for {plugin_version}, host is {host_version}"
    )]
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
    /// ABI version (always [`RPI_PLUGIN_ABI_VERSION_UNIFIED`] for plugins
    /// loaded by this version of the loader).
    pub abi_version: u32,
    /// The registry snapshot built from this plugin's registrations. The host
    /// merges snapshots from all loaded plugins into one session registry.
    pub registry: ExtensionRegistry,
}

fn call_register(
    register: RpiPluginRegisterUnified,
    host_api: &Arc<HostApi>,
) -> i32 {
    // Leak the API struct. The SDK asks plugins to *copy* the function pointers
    // they need during `register`, but real-world plugins retain the `api`
    // pointer to call e.g. `runtime_action` later. The host already keeps the
    // `user_data` (the `ActionBridge`) alive for the whole session, so keeping
    // the table valid is consistent with that contract and prevents a
    // use-after-free for plugins that hold the pointer. Bounded: one struct per
    // plugin load (a leak of a few dozen bytes per load/reload).
    //
    // Defense in depth: the SDK's `export_plugin!` (and `register_entrypoint`)
    // already convert a plugin panic into `REGISTER_PANIC_STATUS` before it can
    // reach the `extern "C"` boundary. We still wrap the call so a plugin built
    // against a *newer* `C-unwind` entrypoint, or a panic raised by host vtable
    // construction, is contained rather than unwinding through the loader.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let api: &'static rpi_plugin_sdk::PluginApi =
            Box::leak(Box::new(host_api.build_vtable()));
        register(api as *const rpi_plugin_sdk::PluginApi)
    })) {
        Ok(code) => code,
        // Reaching here means the panic unwound *into the host* rather than
        // aborting at the plugin's `extern "C"` frame (e.g. a future
        // `C-unwind` entrypoint). Report it the same way the SDK does.
        Err(_) => rpi_plugin_sdk::REGISTER_PANIC_STATUS,
    }
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
/// keeps the stub (actions return `-1`). `rpi-cli` builds ONE master
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

    // 2. Look up the unified entrypoint.
    let register: RpiPluginRegisterUnified = unsafe {
        library
            .get::<RpiPluginRegisterUnified>(rpi_plugin_sdk::REGISTER_SYMBOL_UNIFIED)
            .map(|symbol| *symbol)
    }
    .map_err(|e| PluginLoadError::Symbol {
        path: path.clone(),
        error: e.to_string(),
    })?;
    let abi_version = RPI_PLUGIN_ABI_VERSION_UNIFIED;

    // Plugin display name (file stem) — stamped onto every registration the
    // plugin makes so host diagnostics (e.g. lifecycle veto messages) can name
    // the owning extension. Pure host-side; never crosses the ABI.
    let plugin_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown-plugin>".to_string());

    // 3. Build a fresh registry + HostApi + vtable for this plugin.
    let registry = ExtensionRegistry::new();
    let host_api = match action_bridge {
        Some(bridge) => HostApi::with_action_bridge(
            plugin_name.clone(),
            registry,
            Arc::clone(&diagnostics),
            bridge,
        ),
        None => HostApi::new(plugin_name, registry, Arc::clone(&diagnostics)),
    };
    // 4. Set the thread-local current api so the register trampolines can reach
    //    the registry. Register is synchronous + single-threaded per plugin.
    // SAFETY: `host_api` is alive for the duration of the register call (held on
    // this stack); we clear_current_api immediately after.
    unsafe { set_current_api(&host_api) };
    let rc = call_register(register, &host_api);
    clear_current_api();

    if rc != 0 {
        // The plugin refused (its register returned nonzero — e.g. it saw an
        // ABI version it didn't like), or its body panicked and the SDK
        // converted that into `REGISTER_PANIC_STATUS`. Skip + diag. The
        // registry may have partial registrations; we drop it (no harm — the
        // tools registered so far would reference a plugin that "failed", so
        // we honor the refusal and discard).
        if rc == rpi_plugin_sdk::REGISTER_PANIC_STATUS {
            diagnostics.warn(&format!(
                "plugin {} panicked during registration — skipped (the host is unaffected)",
                path.display()
            ));
        } else {
            diagnostics.warn(&format!(
                "plugin {} ABI v{} register returned code {} — skipped",
                path.display(),
                abi_version,
                rc
            ));
        }
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

    tracing::debug!(path = %path.display(), abi_version, "loaded native plugin");

    Ok(LoadedPlugin {
        library,
        path,
        abi_version,
        registry,
    })
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
    for p in plugins.iter_mut() {
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
    /// Assemble a session from already-loaded parts (explicit `--extension`
    /// files via `load_one` + `merge_registries`). Mirrors `load_session` but
    /// internal assembly so callers can build a session without a dir scan.
    pub fn from_parts(
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        loaded_paths: Vec<PathBuf>,
        action_bridge: Option<Arc<ActionBridge>>,
    ) -> Self {
        Self {
            keepalive,
            snapshot: Some(snapshot),
            loaded_paths,
            action_bridge,
        }
    }

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
/// here; `None` keeps the stub (used by tests / `--no-extensions` no-ops).
pub fn load_session(
    dirs: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> ExtensionSession {
    load_session_mixed(dirs, &[], diagnostics, action_bridge)
}

/// Load plugins from a mix of scanned dirs and explicit cdylib files
/// (the `--extension`/`-e` CLI paths), assembled into one session. Mirrors
/// `load_session` but additionally `load_one`s each explicit file.
pub fn load_session_mixed(
    dirs: &[PathBuf],
    files: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> ExtensionSession {
    let mut loaded: Vec<LoadedPlugin> = Vec::new();
    for dir in dirs {
        loaded.extend(load_dir(
            dir,
            Arc::clone(&diagnostics),
            action_bridge.clone(),
        ));
    }
    for f in files {
        if let Ok(plugin) = load_one(f, Arc::clone(&diagnostics), action_bridge.clone()) {
            loaded.push(plugin);
        }
    }
    if loaded.is_empty() {
        return ExtensionSession::none();
    }
    let loaded_paths: Vec<PathBuf> = loaded.iter().map(|p| p.path.clone()).collect();
    let session_registry = merge_registries(&mut loaded);
    let mut libs: Vec<Library> = Vec::with_capacity(loaded.len());
    for p in loaded {
        let LoadedPlugin {
            library,
            registry: _,
            path: _,
            abi_version: _,
        } = p;
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
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::Mutex;

    static UNIFIED_CALLS: AtomicUsize = AtomicUsize::new(0);
    static UNIFIED_SEEN_VERSION: AtomicUsize = AtomicUsize::new(0);
    static UNIFIED_RETURN: AtomicI32 = AtomicI32::new(0);

    extern "C" fn test_register_unified(api: *const rpi_plugin_sdk::PluginApi) -> i32 {
        if api.is_null() {
            return -99;
        }
        UNIFIED_CALLS.fetch_add(1, Ordering::SeqCst);
        UNIFIED_SEEN_VERSION.store(unsafe { (*api).abi_version as usize }, Ordering::SeqCst);
        UNIFIED_RETURN.load(Ordering::SeqCst)
    }

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

    fn test_host_api() -> Arc<HostApi> {
        HostApi::new(
            "test-plugin",
            ExtensionRegistry::new(),
            Arc::new(CapturingDiag::default()),
        )
    }

    struct CdylibFixture {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Drop for CdylibFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn build_cdylib_fixture(name: &str, source: &str) -> CdylibFixture {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "rpi-abi-loader-{}-{name}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create ABI fixture directory");
        let source_path = dir.join("fixture.rs");
        fs::write(&source_path, source).expect("write ABI fixture source");
        let filename = if cfg!(windows) {
            format!("{name}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{name}.dylib")
        } else {
            format!("lib{name}.so")
        };
        let path = dir.join(filename);
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--crate-name")
            .arg(name)
            .arg("--crate-type")
            .arg("cdylib")
            .arg("--edition")
            .arg("2021")
            .arg(&source_path)
            .arg("-o")
            .arg(&path)
            .output()
            .expect("run rustc for ABI fixture");
        assert!(
            output.status.success(),
            "fixture build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        CdylibFixture { dir, path }
    }

    #[test]
    fn loads_unified_plugin() {
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());

        let unified = build_cdylib_fixture(
            "abi_unified_only",
            &format!(
                "{PREFIX}#[no_mangle]\npub extern \"C\" fn rpi_plugin_register(api: *const c_void) -> i32 {{ if !api.is_null() {{ 0 }} else {{ 91 }} }}\n"
            ),
        );
        let loaded = load_one(&unified.path, Arc::clone(&diag), None).expect("load unified plugin");
        assert_eq!(loaded.abi_version, RPI_PLUGIN_ABI_VERSION_UNIFIED);
        drop(loaded);
        drop(unified);
    }

    #[test]
    fn unified_registration_failure_is_reported() {
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());

        let failed = build_cdylib_fixture(
            "abi_unified_failure",
            &format!(
                "{PREFIX}#[no_mangle]\npub extern \"C\" fn rpi_plugin_register(_: *const c_void) -> i32 {{ 73 }}\n"
            ),
        );
        let error = match load_one(&failed.path, diag, None) {
            Ok(_) => panic!("failed registration must be reported as error"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            PluginLoadError::RegisterReturned { code: 73, .. }
        ));
        drop(failed);
    }

    /// A plugin whose `extern "C"` register body panics must be skipped, not
    /// crash the host. This mirrors what the SDK's `export_plugin!` /
    /// `register_entrypoint` do: catch the panic *inside* the plugin and return
    /// `REGISTER_PANIC_STATUS` so the unwind never reaches the `extern "C"`
    /// boundary (which would abort the process).
    #[test]
    fn panicking_register_body_is_contained_and_skipped() {
        let diag = Arc::new(CapturingDiag::default());
        let diag_dyn: Arc<dyn PluginDiagnostics> = diag.clone();
        let panic_status = rpi_plugin_sdk::REGISTER_PANIC_STATUS;
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let source = format!(
            r#"{PREFIX}
fn body() -> i32 {{ panic!("register body exploded"); }}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const c_void) -> i32 {{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {{
        Ok(code) => code,
        Err(_) => {panic_status},
    }}
}}
"#
        );
        let fixture = build_cdylib_fixture("abi_unified_panic", &source);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = load_one(&fixture.path, diag_dyn, None);
        std::panic::set_hook(prev_hook);

        match result {
            Ok(_) => panic!("a panicking register must be skipped, not loaded"),
            Err(PluginLoadError::RegisterReturned { code, .. }) => {
                assert_eq!(code, panic_status);
            }
            Err(other) => panic!("expected RegisterReturned, got {other:?}"),
        }
        let warns = diag.warns.lock().unwrap().clone();
        assert!(
            warns
                .iter()
                .any(|w| w.contains("panicked during registration")),
            "diagnostic should mention the panic: {warns:?}"
        );
        drop(fixture);
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
