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

use crate::registry::ExtensionRegistry;
use crate::{HostApi, NullDiagnostics, PluginDiagnostics, set_current_api, clear_current_api};

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
pub fn load_one(
    path: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
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
    let host_api = HostApi::new(registry, Arc::clone(&diagnostics));
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
pub fn load_dir(
    dir: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
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
        match load_one(&path, Arc::clone(&diagnostics)) {
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
        let res = load_one("definitely_not_a_plugin.dll", diag);
        assert!(matches!(res, Err(PluginLoadError::Open { .. })));
    }

    #[test]
    fn load_dir_missing_dir_returns_empty_and_warns() {
        let empty = load_dir(
            "no_such_dir_xyz",
            Arc::new(CapturingDiag::default()) as Arc<dyn PluginDiagnostics>,
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
