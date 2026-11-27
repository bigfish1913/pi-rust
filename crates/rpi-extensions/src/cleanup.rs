//! Read-only evidence from the final plugin cleanup worker.
//!
//! A receipt deliberately owns neither the library nor its callback context.
//! Waiting (or abandoning a waiter) cannot initiate or cancel cleanup.

use tokio::sync::watch;

/// State of one load's final library/context cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginCleanupStatus {
    /// A keepalive still exists, or the final cleanup worker has not finished.
    Pending,
    /// SDK shutdown succeeded and this load's library handle, API and host
    /// guard have been released. This does not prove that unrelated OS loader
    /// handles to the same artifact do not exist.
    Released,
    /// Cleanup failed or lost its completion reporter. No release proof exists;
    /// the host must retain lifecycle custody and must not infer quiescence.
    Retained,
}

/// A cloneable, cancellation-safe observer of one plugin's final cleanup.
#[derive(Clone, Debug)]
pub struct PluginCleanupReceipt {
    state: watch::Receiver<PluginCleanupStatus>,
}

impl PluginCleanupReceipt {
    pub fn status(&self) -> PluginCleanupStatus {
        read_status(&self.state)
    }

    /// Wait for release evidence or a conservative retained outcome. A hung
    /// plugin may remain pending indefinitely; a caller's timeout is NOT release.
    pub async fn wait(&self) -> PluginCleanupStatus {
        let mut state = self.state.clone();
        loop {
            let status = read_status(&state);
            if status != PluginCleanupStatus::Pending {
                return status;
            }
            // If a cleanup thread panics, sender closure is classified as
            // retained on the next iteration, never as successful shutdown.
            let _ = state.changed().await;
        }
    }
}

fn read_status(state: &watch::Receiver<PluginCleanupStatus>) -> PluginCleanupStatus {
    // Test closure FIRST, then read the final value: reading Pending before a
    // concurrent Released+sender-drop could otherwise falsely report Retained.
    let closed = state.has_changed().is_err();
    let status = *state.borrow();
    if status == PluginCleanupStatus::Pending && closed {
        PluginCleanupStatus::Retained
    } else {
        status
    }
}

#[derive(Clone)]
pub(crate) struct PluginCleanupReporter(watch::Sender<PluginCleanupStatus>);

impl PluginCleanupReporter {
    pub(crate) fn new() -> Self {
        let (sender, _) = watch::channel(PluginCleanupStatus::Pending);
        Self(sender)
    }

    pub(crate) fn receipt(&self) -> PluginCleanupReceipt {
        PluginCleanupReceipt {
            state: self.0.subscribe(),
        }
    }

    pub(crate) fn released(&self) {
        self.0.send_replace(PluginCleanupStatus::Released);
    }

    pub(crate) fn retained(&self) {
        self.0.send_replace(PluginCleanupStatus::Retained);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lost_reporter_is_not_release_and_terminal_state_survives_closure() {
        let reporter = PluginCleanupReporter::new();
        let receipt = reporter.receipt();
        assert_eq!(receipt.status(), PluginCleanupStatus::Pending);
        drop(reporter);
        assert_eq!(receipt.status(), PluginCleanupStatus::Retained);
        assert_eq!(receipt.wait().await, PluginCleanupStatus::Retained);

        let reporter = PluginCleanupReporter::new();
        let receipt = reporter.receipt();
        reporter.released();
        drop(reporter);
        assert_eq!(receipt.status(), PluginCleanupStatus::Released);
        assert_eq!(receipt.wait().await, PluginCleanupStatus::Released);
    }

    #[tokio::test]
    async fn concurrent_terminal_publish_and_sender_drop_preserve_release_proof() {
        for _ in 0..128 {
            let reporter = PluginCleanupReporter::new();
            let receipt = reporter.receipt();
            let worker = std::thread::spawn(move || reporter.released());
            assert_eq!(receipt.wait().await, PluginCleanupStatus::Released);
            worker.join().unwrap();
            assert_eq!(receipt.status(), PluginCleanupStatus::Released);
        }
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_change_cleanup() {
        let reporter = PluginCleanupReporter::new();
        let receipt = reporter.receipt();
        let waiter = tokio::spawn({
            let receipt = receipt.clone();
            async move { receipt.wait().await }
        });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(receipt.status(), PluginCleanupStatus::Pending);
        reporter.retained();
        assert_eq!(receipt.wait().await, PluginCleanupStatus::Retained);
    }
}
