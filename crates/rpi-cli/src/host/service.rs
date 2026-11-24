//! In-process owner coordinator. This is not the production binder, daemon,
//! IPC server or durable receipt layer. A daemon must retain this service and
//! call `shutdown` before releasing its runtime/catalog.

pub use super::runtime::OwnerPhase;
use super::{
    ordinary::{
        BackendError, OrdinaryAgent, OrdinaryBinder, OrdinaryBinding, OrdinaryClient,
        OrdinaryLimits,
    },
    runtime::{HostRuntime, OwnerEntry},
    AgentId, CatalogError, CatalogGuard, HostCatalog, RequestId, RunId,
};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use rpi_harness::agent_harness::HostAdmissionStatus;
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, MutexGuard,
    },
};
use tokio::sync::{RwLockReadGuard, RwLockWriteGuard};

type Result<T> = std::result::Result<T, ServiceError>;

/// Fixed Display messages are safe to map to IPC codes; Debug/source are not
/// wire payloads (they can contain private native backend diagnostics).
#[derive(Clone, Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("shared Host is draining or stopped")]
    Stopping,
    #[error("Agent already has a Host owner")]
    AlreadyOwned,
    #[error("Agent has no loaded Host owner")]
    NotLoaded,
    #[error("Agent owner is starting or closing")]
    Busy,
    #[error("Agent backend operation failed")]
    Backend(#[from] BackendError),
    #[error("Host catalog operation failed")]
    Catalog(#[source] Arc<CatalogError>),
    #[error("Host shutdown retained {owners} unsettled owner(s); reconcile and retry")]
    ShutdownIncomplete { owners: usize },
    #[error("Host service task panicked or stopped; inspect state before retrying")]
    TaskStopped,
}

type Entry = OwnerEntry<OrdinaryAgent, super::NativeExtensionCustody>;
struct Inner {
    catalog: HostCatalog,
    runtime: HostRuntime<OrdinaryAgent, super::NativeExtensionCustody>,
    administration_claimed: AtomicBool,
}
impl Inner {
    fn entries(&self) -> MutexGuard<'_, BTreeMap<AgentId, Entry>> {
        self.runtime.entries()
    }
}

/// Owns ordinary Agents independently of transient weak clients and request
/// futures. No mutex protecting this registry is held during a backend await.
/// Failed close keeps both the native owner and its resident/writer resources.
#[derive(Clone)]
pub struct HostService {
    inner: Arc<Inner>,
}

/// Borrowed by an owned callback, so it cannot escape as a 'static result.
/// Reserved methods never reacquire the fair lifecycle lock. Admission is
/// checked once; a later stopping signal waits for this existing transaction,
/// rather than revoking it between its journal/native/publication stages.
pub(crate) struct ServiceReservation<'a> {
    service: &'a HostService,
    _guard: RwLockReadGuard<'a, ()>,
}
impl ServiceReservation<'_> {
    pub(crate) fn catalog(&self) -> &HostCatalog {
        self.service.catalog()
    }
    pub(crate) async fn restore_bound(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binding: OrdinaryBinding,
    ) -> Result<OrdinaryClient> {
        self.service
            .restore_inner(self.catalog(), id, guard, limits, binding)
            .await
    }
    pub(crate) async fn cancel_guarded(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        ticket: RunId,
    ) -> Result<bool> {
        self.service
            .client_inner(id)?
            .cancel_guarded(guard, ticket)
            .await
            .map_err(ServiceError::Backend)
    }
    pub(crate) async fn cancel_submitted_guarded(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        submitted_request_id: RequestId,
    ) -> Result<bool> {
        self.service
            .client_inner(id)?
            .cancel_submitted_guarded(guard, &submitted_request_id.to_string())
            .await
            .map_err(ServiceError::Backend)
    }
    pub(crate) async fn close_guarded(&self, id: AgentId, guard: CatalogGuard) -> Result<()> {
        self.service.close_inner(id, Some(guard)).await
    }
}

/// Acquiring exclusivity alone has no stopping/drain effect for an
/// administrative request; the fresh journal owner must explicitly call drain.
pub(crate) struct StopReservation<'a> {
    service: &'a HostService,
    _guard: RwLockWriteGuard<'a, ()>,
    settled: bool,
}
impl<'a> StopReservation<'a> {
    pub(crate) fn catalog(&self) -> &HostCatalog {
        self.service.catalog()
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.service.is_closed()
    }
    /// Unlike consuming drain, keep custody on failure/panic so the coordinator
    /// can publish Uncertain while still fencing storage shutdown.
    pub(crate) async fn settle(&mut self) -> Result<()> {
        self.service.inner.runtime.begin_stopping();
        self.service.drain_inner().await?;
        self.settled = true;
        Ok(())
    }
    pub(crate) async fn drain(mut self) -> Result<DrainedService<'a>> {
        if !self.settled {
            self.settle().await?;
        }
        Ok(DrainedService { reservation: self })
    }
}

/// Only successful native settlement creates this token. It keeps exclusivity
/// and an open catalog for terminal journal publication. Dropping it without
/// closing storage leaves the service stopping but inspectable/retryable.
pub(crate) struct DrainedService<'a> {
    reservation: StopReservation<'a>,
}
impl DrainedService<'_> {
    pub(crate) fn catalog(&self) -> &HostCatalog {
        self.reservation.catalog()
    }
    pub(crate) async fn close_storage(self) -> Result<()> {
        self.catalog()
            .shutdown()
            .await
            .map_err(|error| ServiceError::Catalog(Arc::new(error)))?;
        self.reservation.service.inner.runtime.mark_closed();
        Ok(())
    }
}

impl HostService {
    /// The caller must not shut down another clone of this catalog while the
    /// service is live. Runtime and service lifetime belong to the daemon.
    pub fn new(catalog: HostCatalog) -> Self {
        Self {
            inner: Arc::new(Inner {
                catalog,
                runtime: HostRuntime::new(),
                administration_claimed: AtomicBool::new(false),
            }),
        }
    }

    /// Control queries may observe this catalog, but only the service owns its
    /// shutdown boundary. Never expose the catalog's mutation API over IPC.
    pub(crate) fn catalog(&self) -> &HostCatalog {
        &self.inner.catalog
    }

    /// Never handed over within one service lifetime, even if the facade is
    /// dropped. Detached/uncertain jobs may still hold custody in this epoch.
    pub(crate) fn claim_administration(&self) -> bool {
        self.inner
            .administration_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn owner_count(&self) -> usize {
        self.inner.runtime.owner_count()
    }
    pub fn phase(&self, id: AgentId) -> Option<OwnerPhase> {
        self.inner.runtime.phase(id)
    }
    pub fn is_stopping(&self) -> bool {
        self.inner.runtime.is_stopping()
    }
    pub fn is_closed(&self) -> bool {
        self.inner.runtime.is_closed()
    }

    /// Covers the entire trusted administrative callback, including journal
    /// prepare/finish, independently of its waiter's lifetime. Use only reserved
    /// native methods inside it, never public methods that reacquire lifecycle.
    pub(crate) async fn with_administration<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(ServiceReservation<'a>) -> BoxFuture<'a, Result<T>> + Send + 'static,
    {
        if self.is_stopping() {
            return Err(ServiceError::Stopping);
        }
        let service = self.clone();
        supervised(async move {
            let guard = service.inner.runtime.lifecycle.read().await;
            if service.is_stopping() {
                return Err(ServiceError::Stopping);
            }
            operation(ServiceReservation {
                service: &service,
                _guard: guard,
            })
            .await
        })
        .await
    }

    /// false: administrative admission, no stopping effect before fresh intent.
    /// true: unconditional shutdown, reject new work immediately and allow retry
    /// after failed settlement. This mode is trusted Rust input, never peer JSON.
    pub(crate) async fn with_stop_reservation<T, F>(
        &self,
        unconditional: bool,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(StopReservation<'a>) -> BoxFuture<'a, Result<T>> + Send + 'static,
    {
        if !unconditional && self.is_stopping() {
            return Err(ServiceError::Stopping);
        }
        let service = self.clone();
        supervised(async move {
            if unconditional {
                service.inner.runtime.begin_stopping();
            }
            let guard = service.inner.runtime.lifecycle.write().await;
            if !unconditional && (service.is_stopping() || service.is_closed()) {
                return Err(ServiceError::Stopping);
            }
            operation(StopReservation {
                service: &service,
                _guard: guard,
                settled: false,
            })
            .await
        })
        .await
    }

    /// Starting is supervised even when this request is abandoned. The owner
    /// is installed in the service before returning a weak client; it is never
    /// returned as an unclaimed owner that a cancelled caller could drop.
    pub async fn restore_ordinary(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binder: OrdinaryBinder,
    ) -> Result<OrdinaryClient> {
        self.restore_bound(id, guard, limits, OrdinaryBinding::legacy(binder))
            .await
    }

    /// Production bindings use an explicit closed environment shared by native
    /// storage, tools and skill execution, never the legacy process baseline.
    pub async fn restore_bound(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binding: OrdinaryBinding,
    ) -> Result<OrdinaryClient> {
        self.with_administration(move |reservation| {
            Box::pin(async move { reservation.restore_bound(id, guard, limits, binding).await })
        })
        .await
    }

    async fn restore_inner(
        &self,
        catalog: &HostCatalog,
        id: AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binding: OrdinaryBinding,
    ) -> Result<OrdinaryClient> {
        {
            let mut entries = self.inner.entries();
            if entries.contains_key(&id) {
                return Err(ServiceError::AlreadyOwned);
            }
            entries.insert(
                id,
                Entry {
                    phase: OwnerPhase::Starting,
                    owner: None,
                    failed_start: None,
                },
            );
        }
        let result =
            OrdinaryAgent::restore_bound(catalog.clone(), id, guard, limits, binding).await;
        let safe_failure = result.is_err()
            && !matches!(&result, Err(BackendError::ExtensionUnsettled(_)))
            && self.failed_start_is_inactive(id).await;
        let mut entries = self.inner.entries();
        match result {
            Ok(owner) => {
                let client = owner.client();
                entries.insert(
                    id,
                    Entry {
                        phase: OwnerPhase::Live,
                        owner: Some(Arc::new(owner)),
                        failed_start: None,
                    },
                );
                Ok(client)
            }
            Err(error) => {
                // Ordinary restore owns rollback/uncertainty classification;
                // do not synthesize catalog transitions or fake an Idle row.
                if safe_failure {
                    entries.remove(&id);
                } else {
                    entries.insert(
                        id,
                        Entry {
                            phase: OwnerPhase::StartFailed,
                            owner: None,
                            failed_start: match &error {
                                BackendError::ExtensionUnsettled(custody) => Some(custody.clone()),
                                _ => None,
                            },
                        },
                    );
                }
                Err(error.into())
            }
        }
    }

    /// Attach/detach only changes this weak observer, never native lifetime.
    /// This does not authorize arbitrary callers; the IPC authority layer must
    /// check the invocation's scope before exposing it.
    pub fn ordinary_client(&self, id: AgentId) -> Result<OrdinaryClient> {
        if self.is_stopping() {
            return Err(ServiceError::Stopping);
        }
        self.client_inner(id)
    }
    fn client_inner(&self, id: AgentId) -> Result<OrdinaryClient> {
        let entries = self.inner.entries();
        let entry = entries.get(&id).ok_or(ServiceError::NotLoaded)?;
        match entry.phase {
            OwnerPhase::Live => Ok(entry
                .owner
                .as_ref()
                .ok_or(ServiceError::TaskStopped)?
                .client()),
            OwnerPhase::Starting | OwnerPhase::Closing => Err(ServiceError::Busy),
            OwnerPhase::StartFailed | OwnerPhase::CloseFailed => {
                Err(ServiceError::Backend(BackendError::Faulted))
            }
        }
    }

    async fn failed_start_is_inactive(&self, id: AgentId) -> bool {
        match self.inner.catalog.get(id).await {
            Ok(record) => !record.state.is_resident(),
            Err(CatalogError::UnknownAgent) => true,
            Err(_) => false,
        }
    }

    /// Native admission/reconciliation query for one request. This is a read:
    /// it does not submit, replay or extend any admitted work.
    pub async fn admission(
        &self,
        id: AgentId,
        request_id: &str,
    ) -> Result<Option<HostAdmissionStatus>> {
        self.ordinary_client(id)?
            .admission(request_id)
            .await
            .map_err(ServiceError::Backend)
    }

    /// Bounded native admission listing for crash reconciliation.
    pub async fn admissions(&self, id: AgentId) -> Result<Vec<HostAdmissionStatus>> {
        self.ordinary_client(id)?
            .admissions()
            .await
            .map_err(ServiceError::Backend)
    }

    /// Trusted administrative signal only, not an authorization grant or a
    /// journal receipt. Shutdown fences the whole signal transaction. Native
    /// cancellation checks the owner-observed revision plus exact volatile
    /// ticket, not an authoritative catalog CAS; it cannot target a later run.
    pub async fn cancel_ordinary_guarded(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        ticket: RunId,
    ) -> Result<bool> {
        self.with_administration(move |reservation| {
            Box::pin(async move { reservation.cancel_guarded(id, guard, ticket).await })
        })
        .await
    }

    /// Stable-input counterpart of the volatile-ticket API. Only an active
    /// association in this owner lifetime can be signalled; history is never
    /// replayed/resumed. This signal alone is not a lifecycle journal receipt.
    pub async fn cancel_submitted_ordinary_guarded(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        submitted_request_id: RequestId,
    ) -> Result<bool> {
        self.with_administration(move |reservation| {
            Box::pin(async move {
                reservation
                    .cancel_submitted_guarded(id, guard, submitted_request_id)
                    .await
            })
        })
        .await
    }

    /// Administrative close uses native preconditions before retirement. A
    /// definite pre-effect rejection restores the original registry phase;
    /// uncertainty/settlement failure keeps the owner and marks CloseFailed.
    /// This is not a durable request receipt; the journal coordinator is separate.
    pub async fn close_ordinary_guarded(&self, id: AgentId, guard: CatalogGuard) -> Result<()> {
        self.with_administration(move |reservation| {
            Box::pin(async move { reservation.close_guarded(id, guard).await })
        })
        .await
    }

    /// Explicit close can be retried after a failed shutdown. A dropped waiter
    /// cannot cancel settlement or release an unsettled owner/capacity slot.
    pub async fn close_ordinary(&self, id: AgentId) -> Result<()> {
        let service = self.clone();
        supervised(async move {
            let _reservation = service.inner.runtime.lifecycle.read().await;
            service.close_inner(id, None).await
        })
        .await
    }
    async fn close_inner(&self, id: AgentId, guard: Option<CatalogGuard>) -> Result<()> {
        if self.phase(id) == Some(OwnerPhase::StartFailed) {
            // No native owner exists to enforce this administrative guard.
            // Only the existing unconditional reconciliation path may remove it.
            if guard.is_some() {
                return Err(ServiceError::Backend(BackendError::Faulted));
            }
            let custody = {
                let mut entries = self.inner.entries();
                let entry = entries.get_mut(&id).ok_or(ServiceError::NotLoaded)?;
                if entry.phase != OwnerPhase::StartFailed {
                    return Err(ServiceError::Busy);
                }
                let custody = entry.failed_start.clone();
                if custody.is_some() {
                    // Reserve this exact failed incarnation before ANY await.
                    // A second reconciler must never close a later fresh load.
                    entry.phase = OwnerPhase::Closing;
                }
                custody
            };
            if let Some(custody) = custody {
                let result = AssertUnwindSafe(async {
                    custody.close().await?;
                    custody.release_storage().await;
                    let record = self
                        .inner
                        .catalog
                        .get(id)
                        .await
                        .map_err(BackendError::from)?;
                    if record.state.is_resident() {
                        self.inner
                            .catalog
                            .transition(
                                id,
                                self.inner.catalog.guard(&record),
                                super::AgentState::Closed,
                            )
                            .await
                            .map_err(BackendError::from)?;
                    }
                    Ok::<(), ServiceError>(())
                })
                .catch_unwind()
                .await
                .unwrap_or(Err(ServiceError::TaskStopped));
                let mut entries = self.inner.entries();
                match &result {
                    Ok(()) => {
                        entries.remove(&id);
                    }
                    Err(_) => {
                        if let Some(entry) = entries.get_mut(&id) {
                            entry.phase = OwnerPhase::StartFailed;
                        }
                    }
                }
                return result;
            }
            if !self.failed_start_is_inactive(id).await {
                return Err(ServiceError::Backend(BackendError::Faulted));
            }
            let mut entries = self.inner.entries();
            // Another explicit reconciler may have finished while we awaited.
            // Never remove a new load that reused the same Agent identity.
            if entries.get(&id).map(|entry| entry.phase) != Some(OwnerPhase::StartFailed) {
                return Err(ServiceError::Busy);
            }
            entries.remove(&id);
            return Ok(());
        }
        let (owner, previous_phase) = {
            let mut entries = self.inner.entries();
            let entry = entries.get_mut(&id).ok_or(ServiceError::NotLoaded)?;
            if matches!(entry.phase, OwnerPhase::Starting | OwnerPhase::Closing) {
                return Err(ServiceError::Busy);
            }
            let owner = entry.owner.clone().ok_or(ServiceError::TaskStopped)?;
            let previous_phase = entry.phase;
            entry.phase = OwnerPhase::Closing;
            (owner, previous_phase)
        };
        // Contain a backend panic here as well: the owner stays in the map even
        // if native close cannot return, and a retry is never a fresh restore.
        let result = AssertUnwindSafe(async {
            match guard {
                Some(guard) => owner.close_guarded(guard).await,
                None => owner.close().await,
            }
        })
        .catch_unwind()
        .await;
        // Native Busy is before the close gate is acquired. GuardRejected is
        // explicitly pre-effect; generic Catalog/Harness errors are NOT assumed
        // harmless (they may follow cancellation or actual native cleanup).
        let rejected = guard.is_some()
            && matches!(
                &result,
                Ok(Err(BackendError::GuardRejected(_) | BackendError::Busy))
            );
        let mut entries = self.inner.entries();
        match result {
            Ok(Ok(())) => {
                entries.remove(&id);
                Ok(())
            }
            result => {
                if let Some(entry) = entries.get_mut(&id) {
                    entry.phase = if rejected {
                        previous_phase
                    } else {
                        OwnerPhase::CloseFailed
                    };
                }
                match result {
                    Ok(Err(error)) => Err(error.into()),
                    Err(_) => Err(ServiceError::TaskStopped),
                    Ok(Ok(())) => unreachable!(),
                }
            }
        }
    }

    /// Drain all loaded Agents concurrently, retaining failures. Catalog and
    /// singleton lock are released only when every native owner settled. This
    /// coordinator deliberately has no force-abort or lossy timeout shortcut.
    pub async fn shutdown(&self) -> Result<()> {
        self.with_stop_reservation(true, |reservation| {
            Box::pin(async move {
                if reservation.is_closed() {
                    return Ok(());
                }
                reservation.drain().await?.close_storage().await
            })
        })
        .await
    }

    /// Caller holds the exclusive stop token; do not close catalog here. The
    /// drained token keeps it usable for journal publication before shutdown.
    async fn drain_inner(&self) -> Result<()> {
        let ids: Vec<_> = self.inner.entries().keys().copied().collect();
        let mut closes = FuturesUnordered::new();
        for id in ids {
            closes.push(self.close_inner(id, None));
        }
        while closes.next().await.is_some() {}
        let owners = self.owner_count();
        if owners != 0 {
            return Err(ServiceError::ShutdownIncomplete { owners });
        }
        let status = self
            .inner
            .catalog
            .status()
            .await
            .map_err(|error| ServiceError::Catalog(Arc::new(error)))?;
        if status.resident != 0 {
            return Err(ServiceError::ShutdownIncomplete {
                owners: status.resident,
            });
        }
        Ok(())
    }
}

async fn supervised<T: Send + 'static>(
    future: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    // Dropping JoinHandle detaches; no caller ever receives an abort handle.
    tokio::spawn(AssertUnwindSafe(future).catch_unwind())
        .await
        .map_err(|_| ServiceError::TaskStopped)?
        .map_err(|_| ServiceError::TaskStopped)?
}
