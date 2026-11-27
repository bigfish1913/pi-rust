//! Private adapter for already-built, exclusively handed-over Harnesses.
//! No construction/discovery, catalog, IPC, replay or extra Agent loop.
//! The optional trusted cleanup barrier runs after Harness close but before
//! writer release. It must prove actual resource settlement, not merely a
//! lifecycle acknowledgement. Concrete CLI/process/DLL integration is still
//! pending; bindings with external work require such a barrier. Callers must
//! not keep driving raw Harness clones after successful attachment.

use super::{
    runtime::{HostRuntime, OwnerEntry, OwnerPhase},
    AgentId,
};
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane, HarnessWriteFailure, RunResult},
    result::HarnessError,
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, InProcessError>;
pub(super) type HarnessRuntime = HostRuntime<ManagedHarness>;

/// Trusted, retry-safe cleanup over retained resources. Ok means every callback
/// and external worker has actually settled; uncertainty must return an error.
/// A rejected attach borrows (does not consume) this cleanup capability.
/// Do not capture a strong runtime reference and create an ownership cycle.
pub(super) type HarnessCleanup =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Clone, Debug, thiserror::Error)]
pub(super) enum InProcessError {
    #[error("Host or Agent is stopping")]
    Stopping,
    #[error("Agent has no live Host owner")]
    NotLoaded,
    #[error("Agent or Session already has a Host owner")]
    AlreadyOwned,
    #[error("Agent is busy")]
    Busy,
    #[error("Harness binding has no exclusive durable writer")]
    WriterRequired,
    #[error("native settlement is not proved; retain owner and reconcile")]
    Unsettled,
    #[error("owned Host task panicked or stopped")]
    TaskStopped,
    #[error("Harness operation failed")]
    Harness(#[source] Arc<HarnessError>),
}
impl From<HarnessError> for InProcessError {
    fn from(value: HarnessError) -> Self {
        Self::Harness(Arc::new(value))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CloseStage {
    #[default]
    Open,
    HarnessClosed,
    BindingsRetired,
    ResourcesReleased,
    Complete,
}

#[derive(Default)]
struct Control {
    retiring: bool,
    close_stage: CloseStage,
    uncertain: bool,
    active: Option<CancellationToken>,
    write_failures: Vec<HarnessWriteFailure>,
}
pub(super) struct ManagedHarness {
    harness: AgentHarness,
    cleanup: Option<HarnessCleanup>,
    operation: AsyncMutex<()>,
    control: Mutex<Control>,
}
impl ManagedHarness {
    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
    fn require_saved(&self) -> Result<()> {
        let failures = self.harness.take_write_failures();
        if !self.harness.unsaved_messages().is_empty() || !failures.is_empty() {
            let mut control = self.control();
            control.write_failures.extend(failures);
            control.uncertain = true;
            return Err(InProcessError::Unsettled);
        }
        Ok(())
    }
    fn retire(&self) {
        let mut control = self.control();
        control.retiring = true;
        if let Some(cancel) = &control.active {
            cancel.cancel();
        }
    }
    async fn prompt(&self, text: String) -> Result<RunResult> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| InProcessError::Busy)?;
        let cancel = {
            let mut control = self.control();
            if control.retiring {
                return Err(InProcessError::Stopping);
            }
            if control.uncertain {
                return Err(InProcessError::Unsettled);
            }
            let cancel = CancellationToken::new();
            control.active = Some(cancel.clone());
            cancel
        };
        // Same cooperative pre-admission cancellation pattern as Ordinary.
        // Never abort/drop the executing Harness future on cancellation.
        let execute = AssertUnwindSafe(self.harness.prompt_text(&text, vec![])).catch_unwind();
        tokio::pin!(execute);
        let request_cancel = async {
            cancel.cancelled().await;
            loop {
                if let Some(active) = self.harness.active_operation_snapshot() {
                    if self
                        .harness
                        .request_abort(&active.run_id)
                        .await
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::pin!(request_cancel);
        let result = tokio::select! {
            result = &mut execute => result,
            _ = &mut request_cancel => execute.await,
        };
        let result = result
            .map_err(|_| InProcessError::TaskStopped)
            .and_then(|result| result.map_err(InProcessError::from));
        let settled = self.harness.wait_for_idle().await;
        self.harness.drain_session().await;
        let result = settled.map_err(InProcessError::from).and(result);
        let write_failures = self.harness.take_write_failures();
        let mut control = self.control();
        control.active = None;
        control.uncertain |= !write_failures.is_empty()
            || !self.harness.unsaved_messages().is_empty()
            || matches!(&result, Err(InProcessError::TaskStopped))
            || matches!(&result, Err(InProcessError::Harness(error)) if matches!(error.as_ref(), HarnessError::Io { .. } | HarnessError::SessionStorage { .. }));
        control.write_failures.extend(write_failures);
        result
    }
    async fn close(&self) -> Result<()> {
        self.retire();
        let _operation = self.operation.lock().await;
        {
            let control = self.control();
            if control.close_stage == CloseStage::Complete {
                return Ok(());
            }
            if control.uncertain {
                return Err(InProcessError::Unsettled);
            }
        }
        let stage = self.control().close_stage;
        if stage == CloseStage::Open {
            self.harness.wait_for_idle().await?;
            self.harness.drain_session().await;
            self.require_saved()?;
            self.harness.close().await?;
            self.control().close_stage = CloseStage::HarnessClosed;
        }
        let stage = self.control().close_stage;
        if stage == CloseStage::HarnessClosed {
            // close only seals operations. Relinquish this Harness's own
            // executable references before waiting for final plugin cleanup.
            self.harness.retire_runtime_bindings().await?;
            self.control().close_stage = CloseStage::BindingsRetired;
        }
        let stage = self.control().close_stage;
        if stage != CloseStage::ResourcesReleased {
            if let Some(cleanup) = &self.cleanup {
                cleanup().await?;
            }
            self.control().close_stage = CloseStage::ResourcesReleased;
        }
        let storage = self.harness.session().storage();
        storage.drain().await;
        // Resource teardown can itself finish accepted writes. Recheck after
        // its barrier and drain, before surrendering the durable writer.
        self.require_saved()?;
        storage.release_writer_lease();
        if storage.writer_lease_held() {
            return Err(InProcessError::Unsettled);
        }
        self.control().close_stage = CloseStage::Complete;
        Ok(())
    }
}

pub(super) struct HarnessClient {
    runtime: Weak<HarnessRuntime>,
    owner: Weak<ManagedHarness>,
    id: AgentId,
}
impl HarnessClient {
    /// Native diagnostics remain inspectable even after failed close/stop.
    pub fn write_failures(&self) -> Result<Vec<HarnessWriteFailure>> {
        let runtime = self.runtime.upgrade().ok_or(InProcessError::NotLoaded)?;
        let owner = runtime
            .entries()
            .get(&self.id)
            .and_then(|entry| entry.owner.clone())
            .ok_or(InProcessError::NotLoaded)?;
        if !self.owner.ptr_eq(&Arc::downgrade(&owner)) {
            return Err(InProcessError::NotLoaded);
        }
        let failures = owner.control().write_failures.clone();
        Ok(failures)
    }
    pub async fn prompt_text(&self, text: String) -> Result<RunResult> {
        let runtime = self.runtime.upgrade().ok_or(InProcessError::NotLoaded)?;
        let id = self.id;
        let expected = self.owner.clone();
        owned(async move {
            let owner = runtime.live_owner(id)?;
            if !expected.ptr_eq(&Arc::downgrade(&owner)) {
                return Err(InProcessError::NotLoaded);
            }
            owner.prompt(text).await
        })
        .await
    }
    /// Signal the current managed call only; it cannot poison a later call.
    pub fn cancel(&self) -> Result<bool> {
        let runtime = self.runtime.upgrade().ok_or(InProcessError::NotLoaded)?;
        let owner = runtime.live_owner(self.id)?;
        if !self.owner.ptr_eq(&Arc::downgrade(&owner)) {
            return Err(InProcessError::NotLoaded);
        }
        let control = owner.control();
        if let Some(cancel) = &control.active {
            cancel.cancel();
            return Ok(true);
        }
        Ok(false)
    }
}

impl HarnessRuntime {
    /// Rejection returns the original Harness so the caller keeps cleanup
    /// responsibility. A clone is never registered as a second Session owner.
    pub fn attach(
        self: &Arc<Self>,
        id: AgentId,
        harness: AgentHarness,
    ) -> std::result::Result<HarnessClient, (InProcessError, Box<AgentHarness>)> {
        self.attach_inner(id, harness, None)
    }
    pub fn attach_with_cleanup(
        self: &Arc<Self>,
        id: AgentId,
        harness: AgentHarness,
        cleanup: &HarnessCleanup,
    ) -> std::result::Result<HarnessClient, (InProcessError, Box<AgentHarness>)> {
        self.attach_inner(id, harness, Some(cleanup))
    }
    fn attach_inner(
        self: &Arc<Self>,
        id: AgentId,
        harness: AgentHarness,
        cleanup: Option<&HarnessCleanup>,
    ) -> std::result::Result<HarnessClient, (InProcessError, Box<AgentHarness>)> {
        let mut entries = self.entries();
        let error = if self.is_stopping() {
            Some(InProcessError::Stopping)
        } else if entries.contains_key(&id)
            || entries.values().any(|entry| {
                entry
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.harness.session_id() == harness.session_id())
            })
        {
            Some(InProcessError::AlreadyOwned)
        } else if harness.active_operation_snapshot().is_some() {
            Some(InProcessError::Busy)
        } else {
            let storage = harness.session().storage();
            (!storage.is_ephemeral() && !storage.writer_lease_held())
                .then_some(InProcessError::WriterRequired)
        };
        if let Some(error) = error {
            return Err((error, Box::new(harness)));
        }
        let owner = Arc::new(ManagedHarness {
            harness,
            cleanup: cleanup.cloned(),
            operation: AsyncMutex::new(()),
            control: Mutex::new(Control::default()),
        });
        entries.insert(
            id,
            OwnerEntry {
                phase: OwnerPhase::Live,
                owner: Some(owner.clone()),
                failed_start: None,
            },
        );
        Ok(HarnessClient {
            runtime: Arc::downgrade(self),
            owner: Arc::downgrade(&owner),
            id,
        })
    }
    fn live_owner(&self, id: AgentId) -> Result<Arc<ManagedHarness>> {
        if self.is_stopping() {
            return Err(InProcessError::Stopping);
        }
        let entries = self.entries();
        let entry = entries.get(&id).ok_or(InProcessError::NotLoaded)?;
        if entry.phase != OwnerPhase::Live {
            return Err(InProcessError::Stopping);
        }
        entry.owner.clone().ok_or(InProcessError::Unsettled)
    }
    pub async fn close_harness(self: &Arc<Self>, id: AgentId) -> Result<()> {
        let runtime = self.clone();
        owned(async move {
            let _reservation = runtime.lifecycle.read().await;
            runtime.close_harness_inner(id).await
        })
        .await
    }
    async fn close_harness_inner(&self, id: AgentId) -> Result<()> {
        let owner = {
            let mut entries = self.entries();
            let entry = entries.get_mut(&id).ok_or(InProcessError::NotLoaded)?;
            if entry.phase == OwnerPhase::Closing {
                return Err(InProcessError::Busy);
            }
            let owner = entry.owner.clone().ok_or(InProcessError::Unsettled)?;
            owner.retire();
            entry.phase = OwnerPhase::Closing;
            owner
        };
        let result = AssertUnwindSafe(owner.close())
            .catch_unwind()
            .await
            .unwrap_or(Err(InProcessError::TaskStopped));
        let mut entries = self.entries();
        if result.is_ok() {
            entries.remove(&id);
        } else if let Some(entry) = entries.get_mut(&id) {
            entry.phase = OwnerPhase::CloseFailed;
        }
        result
    }
    pub async fn shutdown_harnesses(self: &Arc<Self>) -> Result<()> {
        self.begin_stopping();
        let runtime = self.clone();
        owned(async move {
            // Signal before waiting for the lifecycle writer, including calls
            // that have not yet reached native admission.
            for entry in runtime.entries().values() {
                if let Some(owner) = &entry.owner {
                    owner.retire();
                }
            }
            let _reservation = runtime.lifecycle.write().await;
            if runtime.is_closed() {
                return Ok(());
            }
            let ids: Vec<_> = runtime.entries().keys().copied().collect();
            let mut closes = FuturesUnordered::new();
            for id in ids {
                closes.push(runtime.close_harness_inner(id));
            }
            while closes.next().await.is_some() {}
            if runtime.owner_count() != 0 {
                return Err(InProcessError::Unsettled);
            }
            runtime.mark_closed();
            Ok(())
        })
        .await
    }
}

async fn owned<T: Send + 'static>(
    future: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    tokio::spawn(AssertUnwindSafe(future).catch_unwind())
        .await
        .map_err(|_| InProcessError::TaskStopped)?
        .map_err(|_| InProcessError::TaskStopped)?
}

#[cfg(test)]
mod tests;
