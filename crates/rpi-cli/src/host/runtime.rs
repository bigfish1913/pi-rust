//! In-memory ownership shared by Host adapters. No catalog, profiles, IPC,
//! environment discovery or Agent loop is needed to create this core.
//!
//! One Host has one authoritative table. Adapters provide independently built
//! owners and retain failed-start resources until native settlement is proved.
//! The core never treats dropping an observer (or this map) as asynchronous
//! shutdown; its owner must cancel, wait, drain and explicitly finish closing.

use super::AgentId;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};
use tokio::sync::RwLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPhase {
    Starting,
    /// Startup failed without proof that all reserved resources were released.
    /// No new start is permitted until explicit reconciliation/close.
    StartFailed,
    Live,
    Closing,
    /// Native settlement failed. Keep ownership for an explicit close retry.
    CloseFailed,
}

pub(super) struct OwnerEntry<Owner, StartupResidue> {
    pub phase: OwnerPhase,
    pub owner: Option<Arc<Owner>>,
    pub failed_start: Option<Arc<StartupResidue>>,
}

/// Not Clone: sharing a Host means sharing this exact table through its owner,
/// never copying members into a second registry. No entry lock crosses await.
pub(super) struct HostRuntime<Owner, StartupResidue = ()> {
    entries: Mutex<BTreeMap<AgentId, OwnerEntry<Owner, StartupResidue>>>,
    // A transaction holds a reader; whole-Host stop holds the fair writer.
    // Admission checks stopping both before and after acquiring its reader.
    pub lifecycle: RwLock<()>,
    stopping: Arc<AtomicBool>,
    closed: AtomicBool,
}

impl<Owner, StartupResidue> HostRuntime<Owner, StartupResidue> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            lifecycle: RwLock::new(()),
            stopping: Arc::new(AtomicBool::new(false)),
            closed: AtomicBool::new(false),
        }
    }

    pub fn entries(&self) -> MutexGuard<'_, BTreeMap<AgentId, OwnerEntry<Owner, StartupResidue>>> {
        // A panic must not silently discard live owners or cleanup custody.
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn owner_count(&self) -> usize {
        self.entries().len()
    }

    pub fn phase(&self, id: AgentId) -> Option<OwnerPhase> {
        self.entries().get(&id).map(|entry| entry.phase)
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Existing Mini observers share this exact admission seal, not a copied
    /// stopping flag or a parallel owner table.
    pub fn stopping_signal(&self) -> Arc<AtomicBool> {
        self.stopping.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Signal admission rejection; existing readers still own their transaction.
    /// This is not proof of cancellation, native settlement or storage closure.
    pub fn begin_stopping(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    /// Adapter holds the exclusive lifecycle reservation and has established
    /// native settlement plus any required external storage shutdown boundary.
    pub fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
