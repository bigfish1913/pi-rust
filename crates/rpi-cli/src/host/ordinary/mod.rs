//! Supervised ordinary JSONL backend. No CLI resource loader, global cwd/env
//! mutation, second Agent loop, IPC, or durable input-ACK protocol lives here.

mod supervisor;
#[cfg(all(test, unix))]
mod tests;

use super::{AgentRecord, AgentSpec, CatalogError, CatalogGuard, HostCatalog, RunId};
use rpi_agent::AgentMessage;
use rpi_harness::{
    agent_harness::{
        ActiveOperationSnapshot, CompactionResult, HarnessWriteFailure, HostAdmissionReceipt,
        HostAdmissionStatus, QueueResult, RunResult, UnsavedMessage,
    },
    session::{jsonl::JsonlSessionFacts, types::HistoryIdentity, Session},
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
    /// Definite pre-effect rejection by the guarded control entry points. This
    /// is distinct from a catalog failure after cancellation/cleanup has begun.
    #[error("ordinary backend administrative guard was rejected; no action accepted")]
    GuardRejected(#[source] Arc<CatalogError>),
    #[error("ordinary backend Harness operation failed")]
    Harness(#[source] Arc<rpi_harness::result::HarnessError>),
    #[error("ordinary backend Session operation failed")]
    Session(#[source] Arc<rpi_harness::error::SessionError>),
    #[error("ordinary backend filesystem operation failed")]
    Io(#[source] Arc<io::Error>),
    #[error("ordinary backend binding rejected: {0}")]
    Binding(&'static str),
    /// A resource declaration may already be visible. This is not a pure
    /// validation rejection; reconcile saved facts before any retry.
    #[error("ordinary backend resource persistence failed or is uncertain")]
    ResourcePersistence,
    /// A DLL may still own workers/code/context and the real writer. Fixed
    /// diagnostic; the Arc is lifecycle custody, not serializable error data.
    #[error("ordinary backend extension cleanup is not proven; ownership retained")]
    ExtensionUnsettled(Arc<super::NativeExtensionCustody>),
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
    #[error("ordinary backend request admission was rejected: {0}")]
    Admission(&'static str),
    #[error("ordinary backend request admission outcome is uncertain; reconcile before retrying")]
    AdmissionUncertain,
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

/// Deferred, owned resource assembly. Constructing this future is part of pure
/// readonly validation; it must not start workers or perform registration until
/// polled by an actual restore. No writer/repair has occurred when it runs.
pub type OrdinaryAssembly = std::pin::Pin<
    Box<dyn std::future::Future<Output = BackendResult<AgentHarnessOptions>> + Send + 'static>,
>;

/// Pure validation of exact saved facts, returning an unpolled assembly plan.
/// It must reject metadata/resource/permission mismatches without loading code
/// or starting work. Actual restore repeats validation with fresh facts. This
/// trusted Rust interface is not a model/IPC capability or an exactly-once API.
pub type OrdinaryDeferredBinder = Box<
    dyn FnOnce(
            &AgentSpec,
            Arc<OsExecutionEnv>,
            &JsonlSessionFacts,
        ) -> BackendResult<OrdinaryAssembly>
        + Send
        + 'static,
>;

enum BindingResolver {
    Immediate(OrdinaryBinder),
    Deferred(OrdinaryDeferredBinder),
}

/// Explicit environment plus a trusted assembly callback. The environment is
/// shared by Session storage, builtin tools and skill execution for this Agent;
/// do not reuse it across Agents (its process cleanup ownership is per Agent).
pub struct OrdinaryBinding {
    environment: Option<Arc<OsExecutionEnv>>,
    binder: BindingResolver,
    // Closed native resource variants only; executable assembly additionally
    // requires the explicit owned DLL custody below, never a generic callback.
    pub(super) instruction_resources: Option<Arc<super::NativeBindingResources>>,
    pub(super) extensions: Option<Arc<super::NativeExtensionCustody>>,
    // Fault injection is test-only; production Session storage always uses the
    // explicitly bound OS environment, never a caller-supplied weak backend.
    #[cfg(test)]
    filesystem: Option<Arc<dyn rpi_tools::FileSystem>>,
}
impl OrdinaryBinding {
    pub fn new(environment: Arc<OsExecutionEnv>, binder: OrdinaryBinder) -> BackendResult<Self> {
        if !environment.has_explicit_environment() {
            return Err(BackendError::Binding(
                "explicit execution environment required",
            ));
        }
        Ok(Self {
            environment: Some(environment),
            binder: BindingResolver::Immediate(binder),
            instruction_resources: None,
            extensions: None,
            #[cfg(test)]
            filesystem: None,
        })
    }
    /// Split pure readonly validation from asynchronous owned assembly. Only
    /// restore polls the returned plan; preflight drops it without polling.
    /// Assembly must retain all callback/library ownership through failure and
    /// cleanup; the backend validates its complete options before commit_strict.
    pub fn new_deferred(
        environment: Arc<OsExecutionEnv>,
        validator: OrdinaryDeferredBinder,
    ) -> BackendResult<Self> {
        if !environment.has_explicit_environment() {
            return Err(BackendError::Binding(
                "explicit execution environment required",
            ));
        }
        Ok(Self {
            environment: Some(environment),
            binder: BindingResolver::Deferred(validator),
            instruction_resources: None,
            extensions: None,
            #[cfg(test)]
            filesystem: None,
        })
    }
    /// Attach the concrete DLL batch loaded by this binding's owned assembly.
    /// Its workers/callbacks/physical cleanup become part of the backend's
    /// writer and residency barrier, including failed startup. This trusted
    /// Rust API does not expose DLLs through profile/model/IPC input.
    pub fn with_extensions(mut self, custody: Arc<super::NativeExtensionCustody>) -> Self {
        self.extensions = Some(custody);
        self
    }

    /// Readonly native preflight before registration/Starting. Consumes this
    /// one-load environment; actual restore must use a fresh binding and repeat
    /// validation, not rely on facts surviving external filesystem changes.
    #[cfg(unix)]
    pub(crate) async fn preflight_session(self, spec: &super::AgentSpec) -> BackendResult<()> {
        supervisor::preflight(spec, self).await
    }
    #[cfg(unix)]
    pub(crate) fn execution_environment(&self) -> BackendResult<Arc<OsExecutionEnv>> {
        self.environment.clone().ok_or(BackendError::Binding(
            "explicit execution environment required",
        ))
    }
    pub(crate) fn legacy(binder: OrdinaryBinder) -> Self {
        Self {
            environment: None,
            binder: BindingResolver::Immediate(binder),
            instruction_resources: None,
            extensions: None,
            #[cfg(test)]
            filesystem: None,
        }
    }
}

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
    Run(Box<RunResult>),
    NoPendingResume,
    Compaction(Box<CompactionResult>),
    /// The exact request id was already durably admitted; this call did not
    /// execute it again. Query the admission status for execution facts.
    AlreadyAdmitted,
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
    /// Volatile input association for the active task or explicitly parked
    /// deferred operation. Not a durable receipt; absent after owner restore.
    pub submitted_request_id: Option<String>,
    /// Volatile signal for this supervised ticket, never proof of settlement.
    pub cancel_requested: bool,
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
        wait_completion(self.completion.clone()).await
    }
}

/// Native admission outcome, published independently of execution settlement.
/// `Pending` is observable only while the supervisor task is running.
#[derive(Clone, Debug)]
pub enum AdmissionState {
    Pending,
    Admitted(Arc<HostAdmissionReceipt>),
    Rejected(BackendError),
}

/// Ticket for a request whose input is admitted before execution. The two
/// waits are deliberately separate: `wait_admitted` returns as soon as the
/// native journal and its sync barrier cover the exact input, while `wait`
/// observes the supervised execution report.
#[derive(Clone, Debug)]
pub struct AdmittedRunTicket {
    pub id: RunId,
    admission: watch::Receiver<AdmissionState>,
    completion: watch::Receiver<Completion>,
}
impl AdmittedRunTicket {
    /// Wait for the durable input receipt. This never implies execution.
    pub async fn wait_admitted(&self) -> BackendResult<Arc<HostAdmissionReceipt>> {
        let mut receiver = self.admission.clone();
        loop {
            match receiver.borrow().clone() {
                AdmissionState::Pending => {}
                AdmissionState::Admitted(receipt) => return Ok(receipt),
                AdmissionState::Rejected(error) => return Err(error),
            }
            receiver
                .changed()
                .await
                .map_err(|_| BackendError::TaskStopped)?;
        }
    }

    pub async fn wait(&self) -> BackendResult<Arc<BackendReport>> {
        wait_completion(self.completion.clone()).await
    }
}

/// Fixed mapping from native admission errors to Host-safe diagnostics. Never
/// forward raw journal text, digests or messages to the client.
pub(super) fn admission_error(error: rpi_harness::durable::DurableError) -> BackendError {
    use rpi_harness::durable::DurableError as Native;
    match error {
        Native::Closed => BackendError::Closed,
        Native::Recovering => BackendError::Faulted,
        Native::LimitExceeded => BackendError::InputTooLarge,
        Native::InvalidRequest(_) => BackendError::Admission("invalid admission request"),
        Native::Conflict => BackendError::Admission("request identity conflicts with the journal"),
        Native::CommitUncertain(_) | Native::ReadFailure(_) => BackendError::AdmissionUncertain,
        _ => BackendError::Faulted,
    }
}

async fn wait_completion(
    mut receiver: watch::Receiver<Completion>,
) -> BackendResult<Arc<BackendReport>> {
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
        Self::restore_bound(catalog, id, guard, limits, OrdinaryBinding::legacy(binder)).await
    }

    pub async fn restore_bound(
        catalog: HostCatalog,
        id: super::AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binding: OrdinaryBinding,
    ) -> BackendResult<Self> {
        if limits.max_input_bytes == 0 {
            return Err(BackendError::Binding("input byte budget must be nonzero"));
        }
        // This task owns startup even if its awaiting caller disappears. An
        // unclaimed successful owner is dropped and cooperatively cleaned up.
        owned(async move { supervisor::restore(catalog, id, guard, limits, binding).await }).await
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

    /// Administrative close, unlike unconditional owner shutdown. Rejection of
    /// an epoch/revision guard cannot retire or cancel the current operation.
    /// Once accepted, settlement remains owned if its waiting caller disappears.
    /// This is not a durable request-id receipt or an IPC authorization grant.
    pub async fn close_guarded(&self, guard: CatalogGuard) -> BackendResult<()> {
        let inner = self.inner.clone();
        owned(async move { inner.close_guarded(guard).await }).await
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

    /// Admit the exact prompt under `request_id` before executing it. Only
    /// `wait_admitted` returning a receipt is a durable input claim.
    pub async fn submit_admitted(
        &self,
        host_id: String,
        request_id: String,
        messages: Vec<AgentMessage>,
    ) -> BackendResult<AdmittedRunTicket> {
        let inner = self.upgrade()?;
        inner.validate_messages(&messages)?;
        let guard = inner
            .control
            .clone()
            .try_lock_owned()
            .map_err(|_| BackendError::Busy)?;
        owned(async move {
            inner
                .submit_admitted(guard, host_id, request_id, messages)
                .await
        })
        .await
    }

    /// Read the native admission/reconciliation status for one request.
    pub async fn admission(&self, request_id: &str) -> BackendResult<Option<HostAdmissionStatus>> {
        let inner = self.upgrade()?;
        inner.admission(request_id).await
    }

    /// Read an already-linked native result and prove a physical barrier after
    /// its records/references were read. This does not execute or resume a run.
    pub async fn get_result(
        &self,
        run_id: &str,
        history: HistoryIdentity,
    ) -> BackendResult<Option<(RunResult, HistoryIdentity)>> {
        self.upgrade()?.get_result(run_id, history).await
    }

    /// Bounded listing of native admissions for reconciliation.
    pub async fn admissions(&self) -> BackendResult<Vec<HostAdmissionStatus>> {
        let inner = self.upgrade()?;
        inner.admissions().await
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

    /// Validate the owner-observed epoch/revision and exact volatile ticket in
    /// the same control critical section as cancellation. A successful signal
    /// is not settlement or a durable request receipt; no subsequent ticket is
    /// affected by a late cancellation of the addressed one.
    pub async fn cancel_guarded(&self, guard: CatalogGuard, ticket: RunId) -> BackendResult<bool> {
        self.upgrade()?.cancel_guarded(guard, ticket)
    }

    /// Signal only the active supervised task associated with this exact input
    /// identity. Duplicates cannot retarget it. A parked deferred operation or
    /// restored historical receipt returns false; no operation is resumed.
    pub async fn cancel_submitted_guarded(
        &self,
        guard: CatalogGuard,
        request_id: &str,
    ) -> BackendResult<bool> {
        self.upgrade()?.cancel_submitted_guarded(guard, request_id)
    }

    #[cfg(unix)]
    pub(in crate::host) async fn install_area_events(
        &self,
        events: super::area_rpc::Events,
    ) -> BackendResult<()> {
        self.upgrade()?.install_area_events(events).await
    }

    #[cfg(unix)]
    pub(in crate::host) async fn area_query(
        &self,
        kind: &str,
        msg: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.upgrade()
            .map_err(|error| error.to_string())?
            .area_query(kind, msg)
            .await
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
