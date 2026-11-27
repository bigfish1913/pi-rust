//! Non-owning plugin cleanup evidence for one CLI preparation and its reloads.
//! This ledger grants nothing and is not proof of process/worker cleanup by itself.
use rpi_extensions::{PluginCleanupReceipt, PluginCleanupStatus, PluginDiagnostics};
use std::sync::{Arc, Mutex};

const MAX_PENDING_RECEIPTS: usize = 1024;
#[derive(Default)]
struct State {
    receipts: Vec<PluginCleanupReceipt>,
    sealed: bool,
    unknown: bool,
}
#[derive(Default)]
pub(crate) struct PluginCleanupLedger {
    state: Mutex<State>,
}
impl PluginCleanupLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    // Staged resource owner will await this AFTER relinquishing every generation's
    // executable references. Call only under its admission/retirement barrier.
    #[allow(dead_code)]
    pub(super) async fn wait_plugins_released(
        &self,
    ) -> Result<(), super::in_process::InProcessError> {
        let receipts = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.sealed = true;
            if state.unknown {
                return Err(super::in_process::InProcessError::Unsettled);
            }
            state.receipts.clone()
        };
        for receipt in receipts {
            if receipt.wait().await != PluginCleanupStatus::Released {
                return Err(super::in_process::InProcessError::Unsettled);
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.unknown {
            return Err(super::in_process::InProcessError::Unsettled);
        }
        state.receipts.clear();
        Ok(())
    }
}
impl PluginDiagnostics for PluginCleanupLedger {
    // Preserve the CLI's existing NullDiagnostics rendering policy.
    fn warn(&self, _: &str) {}
    fn unsupported(&self, _: &str) {}
    fn track_cleanup(&self, receipt: PluginCleanupReceipt) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // Released observations may be compacted, never Pending/Retained ones.
        state
            .receipts
            .retain(|r| r.status() != PluginCleanupStatus::Released);
        if state.sealed || state.receipts.len() >= MAX_PENDING_RECEIPTS {
            // Never silently lose evidence or claim an already-sealed set covers
            // a late load. Unknown is sticky, including after cancelled waiters.
            state.unknown = true;
        }
        if state.receipts.len() < MAX_PENDING_RECEIPTS {
            state.receipts.push(receipt);
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
