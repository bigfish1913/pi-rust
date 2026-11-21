//! Supervised ordinary JSONL backend. No CLI resource loader, global cwd/env
//! mutation, second Agent loop, IPC, or durable input-ACK protocol lives here.

mod supervisor;
#[cfg(all(test, unix))]
mod tests;

use super::{AgentRecord, AgentSpec, CatalogError, CatalogGuard, HostCatalog, RunId};
use rpi_agent::AgentMessage;
use rpi_harness::{
    agent_harness::{
        ActiveOperationSnapshot, CompactionResult, HarnessWriteFailure, QueueResult, RunResult,
        UnsavedMessage,
    },
    session::{jsonl::JsonlSessionFacts, Session},
    types::AgentHarnessOptions,
};
use rpi_tools::OsExecutionEnv;
use std::{
    future::Future,
    io,
    sync::{Arc, Weak},
};
use tokio::sync::watch;

pub type BackendResult<T> = Result<T, BackendError>;

/// Display is deliberately fixed; source errors and reports may contain private
/// transcript/provider details and must not be serialized with Debug by IPC.
#[derive(Clone, Debug, thiserror::Error)]
pub enum BackendError {
    #[error("ordinary backend catalog operation failed")]
    Catalog(#[source] Arc<CatalogError>),
    #[error("ordinary backend Harness operation failed")]
    Harness(#[source] Arc<rpi_harness::result::HarnessError>),
    #[error("ordinary backend Session operation failed")]
    Session(#[source] Arc<rpi_harness::error::SessionError>),
    #[error("ordinary backend filesystem operation failed")]
    Io(#[source] Arc<io::Error>),
    #[error("ordinary backend binding rejected: {0}")]
    Binding(&'static str),
    #[error("ordinary backend is busy; no operation was accepted")]
    Busy,
    #[error("ordinary backend is closing or closed")]
    Closed,
    #[error("ordinary backend requires reconciliation")]
    Faulted,
    #[error("ordinary backend input exceeds its byte budget")]
    InputTooLarge,
    #[error("ordinary backend operation was cancelled before Harness admission")]
    CancelledBeforeAdmission,
    #[error("ordinary backend task panicked or stopped; inspect durable state")]
    TaskStopped,
    #[error("ordinary backend persistence failed or is uncertain; cannot unload safely")]
    Unsaved,
}

macro_rules! source_error {
    ($source:ty, $variant:ident) => {
        impl From<$source> for BackendError {
            fn from(error: $source) -> Self {
                Self::$variant(Arc::new(error))
            }
        }
    };
}
source_error!(CatalogError, Catalog);
source_error!(rpi_harness::result::HarnessError, Harness);
source_error!(rpi_harness::error::SessionError, Session);
source_error!(io::Error, Io);

/// Mandatory trusted assembly/preflight callback. Inspect the exact saved facts,
/// resolve resources/permissions/credentials per Agent, and reject unsupported
/// bindings before the prepared restore claims a writer or repairs a journal.
/// It must not call CLI `build_prepared` or mutate process globals. The supplied
/// options' Session is replaced by this backend's exact committed JSONL Session.
pub type OrdinaryBinder = Box<
    dyn FnOnce(
            &AgentSpec,
            Arc<OsExecutionEnv>,
            &JsonlSessionFacts,
        ) -> BackendResult<AgentHarnessOptions>
        + Send
        + 'static,
>;

#[derive(Clone, Copy, Debug)]
pub struct OrdinaryLimits {
    pub max_input_bytes: usize,
}
impl Default for OrdinaryLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum OrdinaryOperation {
    Prompt(Vec<AgentMessage>),
    ResumePending,
    ResumeDeferred(String),
    Compact(Option<String>),
}

#[derive(Clone, Debug)]
pub enum OrdinaryOutput {
    Run(RunResult),
    NoPendingResume,
    Compaction(CompactionResult),
}

#[derive(Clone, Copy, Debug)]
pub enum Delivery {
    Steer,
    FollowUp,
    NextRun,
}

#[derive(Clone, Debug)]
pub struct BackendReport {
    pub outcome: BackendResult<OrdinaryOutput>,
    pub write_failures: Vec<HarnessWriteFailure>,
    pub unsaved_messages: usize,
    /// A fallible Session path may return directly without a warning/recovery
    /// copy. Never treat zero warnings as proof that the operation was saved.
    pub persistence_uncertain: bool,
    /// Execution may have settled even when publishing Idle failed. Preserve
    /// both facts rather than replacing the actual run outcome with fake success.
    pub catalog_error: Option<BackendError>,
}

#[derive(Clone, Debug)]
pub struct BackendSnapshot {
    pub record: AgentRecord,
    pub ticket: Option<RunId>,
    pub active: Option<ActiveOperationSnapshot>,
    pub pending_resume: bool,
    /// Parked external provider work is busy even without an executing task.
    pub deferred: Option<String>,
    pub retiring: bool,
    pub closed: bool,
    pub fault: Option<BackendError>,
    pub last_report: Option<Arc<BackendReport>>,
}

type Completion = Option<BackendResult<Arc<BackendReport>>>;

/// In-memory operation ticket scoped to this Host lifetime. NOT a durable input
/// receipt or the Harness operation ID; the latter is in `OrdinaryOutput::Run`.
/// Dropping this observer never drops/cancels the supervised operation.
#[derive(Clone)]
pub struct RunTicket {
    pub id: RunId,
    completion: watch::Receiver<Completion>,
}
impl RunTicket {
    pub async fn wait(&self) -> BackendResult<Arc<BackendReport>> {
        let mut receiver = self.completion.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| BackendError::TaskStopped)?;
        }
    }
}

/// Host-owned lifetime, separate from weak client views. Explicitly close and
/// inspect errors before removing this owner from a future service registry.
/// Drop only starts best-effort cooperative cleanup; no forced task abort occurs.
#[must_use = "retain the Host owner independently of transient clients"]
pub struct OrdinaryAgent {
    inner: Arc<supervisor::Backend>,
}

impl OrdinaryAgent {
    pub async fn restore(
        catalog: HostCatalog,
        id: super::AgentId,
        guard: CatalogGuard,
        binder: OrdinaryBinder,
    ) -> BackendResult<Self> {
        Self::restore_with_limits(catalog, id, guard, OrdinaryLimits::default(), binder).await
    }

    pub async fn restore_with_limits(
        catalog: HostCatalog,
        id: super::AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binder: OrdinaryBinder,
    ) -> BackendResult<Self> {
        if limits.max_input_bytes == 0 {
            return Err(BackendError::Binding("input byte budget must be nonzero"));
        }
        // This task owns startup even if its awaiting caller disappears. An
        // unclaimed successful owner is dropped and cooperatively cleaned up.
        owned(async move { supervisor::restore(catalog, id, guard, limits, binder).await }).await
    }

    pub fn client(&self) -> OrdinaryClient {
        OrdinaryClient {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub async fn close(&self) -> BackendResult<()> {
        self.inner.retire();
        let inner = self.inner.clone();
        owned(async move { inner.close().await }).await
    }
}
impl Drop for OrdinaryAgent {
    fn drop(&mut self) {
        if self.inner.is_closed() {
            return;
        }
        self.inner.retire();
        let inner = self.inner.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = inner.close().await {
                    if !matches!(error, BackendError::Busy) {
                        tracing::warn!("ordinary backend owner dropped before clean shutdown; reconciliation required");
                    }
                }
            });
        } else {
            tracing::warn!("ordinary backend dropped outside its runtime; reconciliation required");
        }
    }
}

#[derive(Clone)]
pub struct OrdinaryClient {
    inner: Weak<supervisor::Backend>,
}
impl OrdinaryClient {
    fn upgrade(&self) -> BackendResult<Arc<supervisor::Backend>> {
        self.inner.upgrade().ok_or(BackendError::Closed)
    }

    pub async fn submit(&self, operation: OrdinaryOperation) -> BackendResult<RunTicket> {
        let inner = self.upgrade()?;
        inner.validate_operation(&operation)?;
        let guard = inner
            .control
            .clone()
            .try_lock_owned()
            .map_err(|_| BackendError::Busy)?;
        owned(async move { inner.submit(guard, operation).await }).await
    }

    pub async fn deliver(
        &self,
        message: AgentMessage,
        delivery: Delivery,
    ) -> BackendResult<QueueResult> {
        let inner = self.upgrade()?;
        inner.validate_message(&message)?;
        let guard = inner
            .control
            .clone()
            .try_lock_owned()
            .map_err(|_| BackendError::Busy)?;
        owned(async move { inner.deliver(guard, message, delivery).await }).await
    }

    /// Cooperative signal for this exact ticket, not proof of settlement. A late
    /// cancellation can never target a subsequent operation on the same Agent.
    pub async fn cancel(&self, ticket: RunId) -> BackendResult<bool> {
        self.upgrade()?.cancel(ticket)
    }

    /// Cheap volatile observation; does not scan transcripts or prove settlement.
    pub async fn snapshot(&self) -> BackendResult<BackendSnapshot> {
        self.upgrade()?.snapshot()
    }

    /// Explicit recovery/export surface. The caller owns the returned payload;
    /// a future IPC adapter must apply its own response/output byte budgets.
    pub fn unsaved_messages(&self) -> BackendResult<Vec<UnsavedMessage>> {
        Ok(self.upgrade()?.unsaved_messages())
    }
}

async fn owned<T: Send + 'static>(
    future: impl Future<Output = BackendResult<T>> + Send + 'static,
) -> BackendResult<T> {
    tokio::spawn(future)
        .await
        .map_err(|_| BackendError::TaskStopped)?
}
