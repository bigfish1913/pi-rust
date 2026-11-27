//! Concrete DLL custody for an owned ordinary binding. No discovery or CLI bridge.
//!
//! This is a trusted Rust assembly boundary, not a resource/permission grant.
//! Profiles must not expose DLLs until artifact freezing and capability checks
//! also exist. Callback scopes belong in the actual FFI worker, not its waiter.

use super::ordinary::{BackendError, BackendResult};
use rpi_extensions::{
    ExtensionSession, PluginCleanupReceipt, PluginCleanupStatus, PluginDiagnostics,
    PluginLoadCustody,
};
use rpi_harness::session::jsonl::JsonlSessionStorage;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{watch, Mutex as AsyncMutex};

struct State {
    claimed: bool,
    attempted: bool,
    loading: bool,
    unknown: bool,
    closing: bool,
    calls: usize,
    session: Option<ExtensionSession>,
    failed: Option<PluginLoadCustody>,
    receipts: Vec<PluginCleanupReceipt>,
    storage: Option<JsonlSessionStorage>,
    catalog: Option<super::HostCatalog>,
}

/// Host ownership of one explicit DLL batch, including failed registration.
/// Debug never reveals artifact paths, plugin state or Session data.
/// Harness hooks must retain only a Weak reference; each actual worker takes
/// a call scope, and shutdown drops the session before waiting on its receipts.
pub struct NativeExtensionCustody {
    state: Mutex<State>,
    changed: watch::Sender<()>,
    close_gate: AsyncMutex<()>,
}
impl std::fmt::Debug for NativeExtensionCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeExtensionCustody { .. }")
    }
}

impl NativeExtensionCustody {
    pub fn new() -> Arc<Self> {
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            state: Mutex::new(State {
                claimed: false,
                attempted: false,
                loading: false,
                unknown: false,
                closing: false,
                calls: 0,
                session: None,
                failed: None,
                receipts: Vec::new(),
                storage: None,
                catalog: None,
            }),
            changed,
            close_gate: AsyncMutex::new(()),
        })
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
    /// Invoke only from owned assembly, never readonly validation. The caller
    /// supplies trusted frozen/staged files and must validate capabilities before
    /// publishing hooks. No ActionBridge, scanner or ambient registry is used.
    /// Losing this waiter does not discard a partially loaded batch.
    pub async fn load_owned(
        self: &Arc<Self>,
        files: Vec<PathBuf>,
        diagnostics: Arc<dyn PluginDiagnostics>,
        artifact_guard: Option<Arc<dyn Send + Sync>>,
    ) -> BackendResult<()> {
        self.load_owned_checked(files, diagnostics, artifact_guard, |_, _| true)
            .await
    }

    /// Exact per-plugin data-only check, before SDK batch merge/publication.
    /// Rejection and validator panic retain the current and all preceding
    /// plugins in the same supervised cleanup path, never partial success.
    pub async fn load_owned_checked(
        self: &Arc<Self>,
        files: Vec<PathBuf>,
        diagnostics: Arc<dyn PluginDiagnostics>,
        artifact_guard: Option<Arc<dyn Send + Sync>>,
        validate: impl FnMut(usize, &rpi_extensions::RegistrationInventory) -> bool + Send + 'static,
    ) -> BackendResult<()> {
        {
            let mut state = self.state();
            if !state.claimed || state.attempted || state.closing {
                return Err(BackendError::Binding("DLL custody is not reusable"));
            }
            state.attempted = true;
            state.loading = true;
        }
        let owner = self.clone();
        let loading = LoadingLease(owner.clone());
        tokio::task::spawn_blocking(move || {
            let _loading = loading;
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rpi_extensions::load_explicit_session_owned_checked(
                    &files,
                    diagnostics,
                    artifact_guard,
                    validate,
                )
            }));
            let result = {
                let mut state = owner.state();
                state.loading = false;
                match loaded {
                    Ok(Ok(session)) => {
                        state.receipts = session.cleanup_receipts();
                        state.session = Some(session);
                        Ok(())
                    }
                    Ok(Err(failure)) => {
                        state.receipts = failure.custody.cleanup_receipts();
                        state.failed = Some(failure.custody);
                        Err(BackendError::Binding("explicit DLL assembly failed"))
                    }
                    Err(_) => {
                        // A panic may have dropped a loaded handle into async
                        // cleanup without handing us its receipt. Never guess.
                        state.unknown = true;
                        Err(BackendError::TaskStopped)
                    }
                }
            };
            owner.changed.send_replace(());
            result
        })
        .await
        .map_err(|_| BackendError::TaskStopped)?
    }

    /// This value MUST move into the actual callback worker. It holds a session
    /// keepalive until the callback and all plugin-owned frees have returned.
    pub fn call(self: &Arc<Self>) -> BackendResult<NativeExtensionCall> {
        let mut state = self.state();
        if state.closing || state.loading || state.unknown {
            return Err(BackendError::Closed);
        }
        let session = state.session.clone().ok_or(BackendError::Closed)?;
        state.calls = state.calls.checked_add(1).ok_or(BackendError::Faulted)?;
        Ok(NativeExtensionCall {
            session,
            _lease: CallLease(self.clone()),
        })
    }

    pub(super) fn claim(&self, catalog: super::HostCatalog) -> BackendResult<()> {
        let mut state = self.state();
        if state.claimed || state.attempted || state.closing {
            return Err(BackendError::Binding("DLL custody is not reusable"));
        }
        state.claimed = true;
        state.catalog = Some(catalog);
        Ok(())
    }

    pub(super) fn retain_storage(&self, storage: JsonlSessionStorage) {
        self.state().storage = Some(storage);
    }

    /// Seal callbacks, wait for physical FFI scopes/loading, then relinquish all
    /// session/custody keepalives and await final SDK/OS/context cleanup. This
    /// operation is called by the supervised owner, never an unowned IPC waiter.
    pub(super) async fn close(self: &Arc<Self>) -> BackendResult<()> {
        let _close = self.close_gate.lock().await;
        let mut changed = self.changed.subscribe();
        let (session, failed, receipts, unknown) = loop {
            let ready = {
                let mut state = self.state();
                state.closing = true;
                if state.loading || state.calls != 0 {
                    None
                } else {
                    Some((
                        state.session.take(),
                        state.failed.take(),
                        state.receipts.clone(),
                        state.unknown,
                    ))
                }
            };
            if let Some(ready) = ready {
                break ready;
            }
            let _ = changed.changed().await;
        };
        // Last keepalive Drop queues cleanup. Receipts own no code/context and
        // therefore cannot themselves keep the DLL loaded indefinitely.
        drop(session);
        drop(failed);
        let mut retained = unknown;
        for receipt in receipts {
            retained |= receipt.wait().await != PluginCleanupStatus::Released;
        }
        if retained {
            Err(BackendError::ExtensionUnsettled(self.clone()))
        } else {
            Ok(())
        }
    }

    /// Failed-start rollback uses this only AFTER close proved DLL settlement.
    /// The clone keeps the real writer alive across errors/panics in assembly.
    pub(super) async fn release_storage(&self) {
        let storage = self.state().storage.take();
        if let Some(storage) = storage {
            storage.drain().await;
            storage.release_writer_lease();
        }
    }
}

impl Drop for NativeExtensionCustody {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        let unproven = state.unknown
            || state.loading
            || state.calls != 0
            || state
                .receipts
                .iter()
                .any(|receipt| receipt.status() != PluginCleanupStatus::Released);
        if unproven {
            // Last-owner Drop is not a settlement operation. If explicit
            // shutdown failed/was omitted, keep both the singleton exclusion
            // and any real writer for process lifetime rather than let a new
            // Host owner race code which may still be alive. Normal supervised
            // close reaches Released first and never takes this fail-closed path.
            if let Some(storage) = state.storage.take() {
                std::mem::forget(storage);
            }
            if let Some(catalog) = state.catalog.take() {
                std::mem::forget(catalog);
            }
        }
    }
}

/// A scoped session view. Field order drops the session BEFORE publishing that
/// the callback finished; close must not observe zero with a keepalive in flight.
pub struct NativeExtensionCall {
    session: ExtensionSession,
    _lease: CallLease,
}
impl NativeExtensionCall {
    pub fn session(&self) -> &ExtensionSession {
        &self.session
    }
}
struct LoadingLease(Arc<NativeExtensionCustody>);
impl Drop for LoadingLease {
    fn drop(&mut self) {
        let mut state = self.0.state();
        if state.loading {
            // A queued worker can be destroyed without running during runtime
            // shutdown. Do not leave close waiting for a nonexistent reporter.
            state.loading = false;
            state.unknown = true;
            self.0.changed.send_replace(());
        }
    }
}
struct CallLease(Arc<NativeExtensionCustody>);
impl Drop for CallLease {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.calls -= 1;
        self.0.changed.send_replace(());
    }
}
