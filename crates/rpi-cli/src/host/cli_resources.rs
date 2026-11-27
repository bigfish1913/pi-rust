//! Retirement of the existing CLI's plugin bindings, not a second loader.
//! Call close only after Harness close/retirement, frontend joins, and settlement
//! of EVERY other user of these resource leases (including native child agents).
//! This does NOT settle OS tool processes, detached tasks, or dev build workers.
use super::{cli_cleanup::PluginCleanupLedger, in_process::InProcessError};
use crate::{
    resource_management::Lease,
    session::{ActionBridgeCell, ExtensionSessionCell},
};
use rpi_ai::Provider;
use rpi_extensions::ExtensionSession;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Providers = Arc<RwLock<Vec<Arc<dyn Provider>>>>;

pub(crate) struct CliPluginResources {
    ledger: Arc<PluginCleanupLedger>,
    leases: Mutex<Option<Arc<Vec<Lease>>>>,
    sessions: ExtensionSessionCell,
    bridges: ActionBridgeCell,
    providers: Providers,
    // Reload mutates matching session/bridge/provider cells, so candidates must
    // serialize with each other as well as with final retirement.
    gate: Arc<AsyncMutex<()>>,
    closing: AtomicBool,
}
/// Keeps the real resource lease and reload admission together across awaits.
pub(crate) struct CliReloadPermit {
    _admission: OwnedMutexGuard<()>,
    leases: Arc<Vec<Lease>>,
}
impl CliReloadPermit {
    pub(crate) fn host_guard(&self) -> Arc<dyn Send + Sync> {
        self.leases.clone()
    }
}
impl CliPluginResources {
    pub(crate) fn new(
        ledger: Arc<PluginCleanupLedger>,
        leases: Arc<Vec<Lease>>,
        sessions: ExtensionSessionCell,
        bridges: ActionBridgeCell,
        providers: Providers,
    ) -> Arc<Self> {
        Arc::new(Self {
            ledger,
            leases: Mutex::new(Some(leases)),
            sessions,
            bridges,
            providers,
            gate: Arc::new(AsyncMutex::new(())),
            closing: AtomicBool::new(false),
        })
    }
    pub(crate) async fn begin_reload(&self) -> Option<CliReloadPermit> {
        if self.closing.load(Ordering::SeqCst) {
            return None;
        }
        let admission = self.gate.clone().lock_owned().await;
        if self.closing.load(Ordering::SeqCst) {
            return None;
        }
        let leases = self
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .clone();
        Some(CliReloadPermit {
            _admission: admission,
            leases,
        })
    }
    #[cfg(unix)]
    pub(crate) async fn close_for_local_rpc(&self) -> Result<(), String> {
        self.close_plugins_and_leases()
            .await
            .map_err(|error| error.to_string())
    }

    // Retires the native plugin resources once the owning frontend and its
    // operations have joined.
    #[allow(dead_code)]
    pub(super) async fn close_plugins_and_leases(&self) -> Result<(), InProcessError> {
        self.closing.store(true, Ordering::SeqCst);
        let _exclusive = self.gate.lock().await;
        let bridge = self
            .bridges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let session = std::mem::replace(
            &mut *self.sessions.lock().unwrap_or_else(|e| e.into_inner()),
            ExtensionSession::none(),
        );
        let providers =
            std::mem::take(&mut *self.providers.write().unwrap_or_else(|e| e.into_inner()));
        // Never call foreign code/destructors while holding cells/Provider locks.
        if let Some(snapshot) = session.snapshot_arc() {
            snapshot.active_flag().store(false, Ordering::SeqCst);
        }
        if let Some(bridge) = &bridge {
            bridge.invalidate();
        }
        session.begin_shutdown();
        drop((providers, session, bridge));
        // Includes old generations, failed candidates and outstanding external
        // keepalives. Err/panic/cancellation leaves the real leases in self.
        self.ledger.wait_plugins_released().await?;
        let leases = self.leases.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(leases);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
