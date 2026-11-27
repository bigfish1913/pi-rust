//! In-process owner coordinator. This is not the production binder, daemon,
//! IPC server or durable receipt layer. A daemon must retain this service and
//! call `shutdown` before releasing its runtime/catalog.

pub use super::runtime::OwnerPhase;
use super::{
    ordinary::{
        BackendError, OrdinaryAgent, OrdinaryBinder, OrdinaryBinding, OrdinaryClient,
        OrdinaryLimits, ReclaimClock,
    },
    runtime::{HostRuntime, OwnerEntry},
    AgentId, AgentState, CatalogError, CatalogGuard, HostCatalog, RequestId, RunId,
};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use rpi_harness::agent_harness::HostAdmissionStatus;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::BTreeMap,
    future::Future,
    io::Write,
    panic::AssertUnwindSafe,
    sync::{Arc, MutexGuard, Weak},
    time::Duration,
};
use tokio::{
    sync::{watch, RwLockReadGuard, RwLockWriteGuard},
    time::Instant,
};

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

enum NativeOwner {
    Ordinary(OrdinaryAgent),
    Mini(super::mini::MiniAgent),
}
impl NativeOwner {
    fn ordinary(&self) -> std::result::Result<&OrdinaryAgent, BackendError> {
        match self {
            Self::Ordinary(owner) => Ok(owner),
            Self::Mini(_) => Err(BackendError::Binding(
                "Mini has no persistent admission authority",
            )),
        }
    }
    async fn close(&self) -> std::result::Result<(), BackendError> {
        match self {
            Self::Ordinary(owner) => owner.close().await,
            Self::Mini(owner) => owner.close().await,
        }
    }
}
type Entry = OwnerEntry<NativeOwner, super::NativeExtensionCustody>;
#[cfg(unix)]
pub(super) type AreaAttachment = (super::area_rpc::Events, super::ordinary::AreaPeer);

struct OrdinaryLoad {
    client: OrdinaryClient,
    #[cfg(unix)]
    attachment: Option<std::result::Result<AreaAttachment, BackendError>>,
}

enum CloseIntent<'a> {
    Close,
    Guarded(CatalogGuard),
    Idle(&'a Weak<NativeOwner>),
}
struct Inner {
    catalog: HostCatalog,
    runtime: HostRuntime<NativeOwner, super::NativeExtensionCustody>,
    empty_since: Option<watch::Sender<Option<Instant>>>,
    #[cfg(unix)]
    administration_claimed: AtomicBool,
}
impl Inner {
    fn entries(&self) -> MutexGuard<'_, BTreeMap<AgentId, Entry>> {
        self.runtime.entries()
    }

    // Called under the owner table lock when its membership changes. Keep
    // the latest boundary even if the listener misses intermediate updates.
    fn update_empty_since(&self, entries: &BTreeMap<AgentId, Entry>) {
        if let Some(idle) = &self.empty_since {
            idle.send_if_modified(|since| match (entries.is_empty(), since.is_some()) {
                (true, false) => {
                    *since = Some(Instant::now());
                    true
                }
                (false, true) => {
                    *since = None;
                    true
                }
                _ => false,
            });
        }
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
            .restore_inner(id, guard, limits, binding, false)
            .await
            .map(|loaded| loaded.client)
    }

    #[cfg(unix)]
    pub(super) async fn restore_for_attach(
        &self,
        prepared: super::PreparedProfile,
        guard: CatalogGuard,
    ) -> Result<(OrdinaryClient, AreaAttachment)> {
        let loaded = self
            .service
            .restore_inner(
                prepared.spec.id,
                guard,
                prepared.limits,
                prepared.binding,
                true,
            )
            .await?;
        Ok((
            loaded.client,
            loaded.attachment.ok_or(ServiceError::TaskStopped)??,
        ))
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
    #[cfg(unix)]
    pub(crate) fn mini_client(&self, id: AgentId) -> Result<super::mini::MiniClient> {
        self.service.mini_client(id)
    }
    #[cfg(unix)]
    pub(crate) async fn close_mini(&self, id: AgentId) -> Result<()> {
        // Mode proof before entering the mode-independent native close path.
        // A CloseFailed Mini retains the same entry for explicit reconciliation.
        {
            let entries = self.service.inner.entries();
            let entry = entries.get(&id).ok_or(ServiceError::NotLoaded)?;
            if !matches!(entry.owner.as_deref(), Some(NativeOwner::Mini(_))) {
                return Err(
                    BackendError::Binding("Mini control cannot close a Persistent owner").into(),
                );
            }
        }
        self.service.close_inner(id, CloseIntent::Close).await
    }
    pub(crate) async fn close_guarded(&self, id: AgentId, guard: CatalogGuard) -> Result<()> {
        match self
            .service
            .close_inner(id, CloseIntent::Guarded(guard))
            .await
        {
            Err(ServiceError::NotLoaded) => {
                let record = self.catalog().get(id).await.map_err(BackendError::from)?;
                if record.state != AgentState::Unloaded {
                    return Err(ServiceError::NotLoaded);
                }
                // A concurrent restore must commit Starting before acquiring the
                // writer. This revision guard allows only one transition to win.
                self.catalog()
                    .transition(id, guard, AgentState::Closed)
                    .await
                    .map_err(|error| match error {
                        CatalogError::EpochConflict | CatalogError::RevisionConflict { .. } => {
                            BackendError::GuardRejected(Arc::new(error))
                        }
                        error => BackendError::from(error),
                    })?;
                Ok(())
            }
            result => result,
        }
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
        let empty_since = catalog
            .idle_timeout()
            .map(|_| watch::channel(Some(Instant::now())).0);
        Self {
            inner: Arc::new(Inner {
                catalog,
                runtime: HostRuntime::new(),
                empty_since,
                #[cfg(unix)]
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
    #[cfg(unix)]
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
        id: AgentId,
        guard: CatalogGuard,
        limits: OrdinaryLimits,
        binding: OrdinaryBinding,
        attach: bool,
    ) -> Result<OrdinaryLoad> {
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
            self.inner.update_empty_since(&entries);
        }
        let result = OrdinaryAgent::restore_bound_with_intent(
            self.catalog().clone(),
            id,
            guard,
            limits,
            binding,
            attach,
        )
        .await;
        // Lease the first peer before publishing the owner or arming the watcher.
        // Even a failed peer setup leaves startup settlement to the owner table.
        #[cfg(unix)]
        let attachment = match &result {
            Ok(owner) if attach => Some(attach_area_peer(&owner.client()).await),
            _ => None,
        };
        let safe_failure = result.is_err()
            && !matches!(&result, Err(BackendError::ExtensionUnsettled(_)))
            && self.failed_start_is_inactive(id).await;
        let mut entries = self.inner.entries();
        match result {
            Ok(owner) => {
                let client = owner.client();
                let clock = owner.reclaim_clock();
                let owner = Arc::new(NativeOwner::Ordinary(owner));
                if let Some(clock) = clock {
                    tokio::spawn(watch_idle(
                        Arc::downgrade(&self.inner),
                        id,
                        Arc::downgrade(&owner),
                        clock,
                    ));
                }
                entries.insert(
                    id,
                    Entry {
                        phase: OwnerPhase::Live,
                        owner: Some(owner),
                        failed_start: None,
                    },
                );
                Ok(OrdinaryLoad {
                    client,
                    #[cfg(unix)]
                    attachment,
                })
            }
            Err(error) => {
                // Ordinary restore owns rollback/uncertainty classification;
                // do not synthesize catalog transitions or fake an Idle row.
                if safe_failure {
                    entries.remove(&id);
                    self.inner.update_empty_since(&entries);
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

    /// Start one Session-free owner through the same reservation/table and
    /// stop barrier as persistent owners. Trusted Rust API only; daemon/wire
    /// Mini admission remains closed until its separate capability is complete.
    pub async fn start_mini(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        profile: super::bindings::NativeBindingProfile,
        limits: super::mini::MiniLimits,
    ) -> Result<super::mini::MiniClient> {
        self.with_administration(move |reservation| {
            Box::pin(async move {
                reservation
                    .service
                    .start_mini_inner(id, guard, profile, limits)
                    .await
            })
        })
        .await
    }

    async fn start_mini_inner(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        profile: super::bindings::NativeBindingProfile,
        limits: super::mini::MiniLimits,
    ) -> Result<super::mini::MiniClient> {
        let record = self
            .inner
            .catalog
            .get(id)
            .await
            .map_err(|error| ServiceError::Catalog(Arc::new(error)))?;
        if record.spec.mode != super::AgentMode::Mini {
            return Err(
                BackendError::Binding("Mini requires explicit Session-free metadata").into(),
            );
        }
        let custody = super::NativeExtensionCustody::new();
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
                    failed_start: Some(custody.clone()),
                },
            );
            self.inner.update_empty_since(&entries);
        }
        let result = AssertUnwindSafe(super::mini::MiniAgent::start(
            self.inner.catalog.clone(),
            id,
            guard,
            profile,
            limits,
            custody.clone(),
            self.inner.runtime.stopping_signal(),
        ))
        .catch_unwind()
        .await;
        // A panic cannot prove that staging/registration released its custody.
        let safe_failure = matches!(result, Ok(Err(_))) && self.failed_start_is_inactive(id).await;
        let mut entries = self.inner.entries();
        match result {
            Ok(Ok(owner)) => {
                let client = owner.client();
                entries.insert(
                    id,
                    Entry {
                        phase: OwnerPhase::Live,
                        owner: Some(Arc::new(NativeOwner::Mini(owner))),
                        failed_start: None,
                    },
                );
                Ok(client)
            }
            failure => {
                if safe_failure {
                    entries.remove(&id);
                    self.inner.update_empty_since(&entries);
                } else {
                    entries.insert(
                        id,
                        Entry {
                            phase: OwnerPhase::StartFailed,
                            owner: None,
                            failed_start: Some(custody),
                        },
                    );
                }
                match failure {
                    Ok(Err(error)) => Err(error.into()),
                    Err(_) => Err(ServiceError::TaskStopped),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    }

    /// Attach to this exact Mini incarnation; no latest-owner lookup occurs in
    /// the returned weak client, so an old client cannot retarget a later load.
    pub fn mini_client(&self, id: AgentId) -> Result<super::mini::MiniClient> {
        if self.is_stopping() {
            return Err(ServiceError::Stopping);
        }
        let entries = self.inner.entries();
        let entry = entries.get(&id).ok_or(ServiceError::NotLoaded)?;
        if entry.phase != OwnerPhase::Live {
            return Err(ServiceError::Busy);
        }
        match entry.owner.as_deref().ok_or(ServiceError::TaskStopped)? {
            NativeOwner::Mini(owner) => Ok(owner.client()),
            NativeOwner::Ordinary(_) => {
                Err(BackendError::Binding("persistent owner has no Mini authority").into())
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
                .ordinary()?
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
        self.close_agent(id).await
    }

    /// Mode-independent close on the sole native owner table. Mini reaches Lost
    /// only after its prompt and actual DLL custody settle, never on cancellation.
    pub async fn close_agent(&self, id: AgentId) -> Result<()> {
        let service = self.clone();
        supervised(async move {
            let _reservation = service.inner.runtime.lifecycle.read().await;
            service.close_inner(id, CloseIntent::Close).await
        })
        .await
    }
    async fn close_inner(&self, id: AgentId, intent: CloseIntent<'_>) -> Result<()> {
        if self.phase(id) == Some(OwnerPhase::StartFailed) {
            // No native owner exists to enforce this administrative guard.
            // Only the existing unconditional reconciliation path may remove it.
            if !matches!(intent, CloseIntent::Close) {
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
                                if record.spec.mode == super::AgentMode::Mini {
                                    super::AgentState::Lost
                                } else {
                                    super::AgentState::Closed
                                },
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
                        self.inner.update_empty_since(&entries);
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
            self.inner.update_empty_since(&entries);
            return Ok(());
        }
        let (owner, previous_phase) = {
            let mut entries = self.inner.entries();
            let entry = entries.get_mut(&id).ok_or(ServiceError::NotLoaded)?;
            if matches!(entry.phase, OwnerPhase::Starting | OwnerPhase::Closing) {
                return Err(ServiceError::Busy);
            }
            let owner = entry.owner.clone().ok_or(ServiceError::TaskStopped)?;
            if let CloseIntent::Idle(expected) = intent {
                if entry.phase != OwnerPhase::Live || expected.as_ptr() != Arc::as_ptr(&owner) {
                    return Err(ServiceError::NotLoaded);
                }
            }
            if !matches!(intent, CloseIntent::Close) {
                // The persistent administrative guard is not Mini authority.
                // Reject before changing phase or signalling any native run.
                owner.ordinary()?;
            }
            let previous_phase = entry.phase;
            entry.phase = OwnerPhase::Closing;
            (owner, previous_phase)
        };
        // Contain a backend panic here as well: the owner stays in the map even
        // if native close cannot return, and a retry is never a fresh restore.
        let result = AssertUnwindSafe(async {
            match intent {
                CloseIntent::Guarded(guard) => owner.ordinary()?.close_guarded(guard).await,
                CloseIntent::Idle(_) => owner.ordinary()?.unload_idle().await,
                CloseIntent::Close => owner.close().await,
            }
        })
        .catch_unwind()
        .await;
        // Native Busy is before the close gate is acquired. GuardRejected is
        // explicitly pre-effect; generic Catalog/Harness errors are NOT assumed
        // harmless (they may follow cancellation or actual native cleanup).
        let rejected = !matches!(intent, CloseIntent::Close)
            && matches!(
                &result,
                Ok(Err(BackendError::GuardRejected(_) | BackendError::Busy))
            );
        let mut entries = self.inner.entries();
        match result {
            Ok(Ok(())) => {
                entries.remove(&id);
                self.inner.update_empty_since(&entries);
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

    #[cfg(unix)]
    pub(super) fn empty_since(&self) -> Option<watch::Receiver<Option<Instant>>> {
        self.inner
            .empty_since
            .as_ref()
            .map(watch::Sender::subscribe)
    }

    /// The listener has observed an uninterrupted peer-free interval. Fence
    /// native admissions, then revalidate that no owner appeared in that time.
    #[cfg(unix)]
    pub(super) async fn shutdown_if_idle(&self, timeout: std::time::Duration) -> Result<bool> {
        let Ok(guard) = self.inner.runtime.lifecycle.try_write() else {
            return Ok(false);
        };
        let Some(since) = self
            .inner
            .empty_since
            .as_ref()
            .and_then(|value| *value.borrow())
        else {
            return Ok(false);
        };
        if self.is_stopping() || since.elapsed() < timeout {
            return Ok(false);
        }
        let status = self
            .catalog()
            .status()
            .await
            .map_err(|error| ServiceError::Catalog(Arc::new(error)))?;
        if status.resident != 0 || self.is_stopping() {
            return Ok(false);
        }
        self.inner.runtime.begin_stopping();
        DrainedService {
            reservation: StopReservation {
                service: self,
                _guard: guard,
                settled: true,
            },
        }
        .close_storage()
        .await?;
        Ok(true)
    }

    /// Caller holds the exclusive stop token; do not close catalog here. The
    /// drained token keeps it usable for journal publication before shutdown.
    async fn drain_inner(&self) -> Result<()> {
        let ids: Vec<_> = self.inner.entries().keys().copied().collect();
        let mut closes = FuturesUnordered::new();
        for id in ids {
            closes.push(self.close_during_shutdown(id));
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

    async fn close_during_shutdown(&self, id: AgentId) -> Result<()> {
        let grace = self
            .inner
            .entries()
            .get(&id)
            .and_then(|entry| entry.owner.as_ref()?.ordinary().ok()?.shutdown_grace());
        let closing = self.close_inner(id, CloseIntent::Close);
        let Some(grace) = grace else {
            return closing.await;
        };
        tokio::pin!(closing);
        tokio::select! {
            biased;
            result = &mut closing => return result,
            () = tokio::time::sleep(grace) => (),
        }
        // This is a diagnostic within the owned drain. The same close must
        // finish before the service can release its catalog and listener.
        let _ = writeln!(
            std::io::stderr().lock(),
            "Host Agent {id} exceeded shutdown grace ({} ms) and is still settling",
            grace.as_millis()
        );
        closing.await
    }
}

#[cfg(unix)]
pub(super) async fn attach_area_peer(
    client: &OrdinaryClient,
) -> std::result::Result<AreaAttachment, BackendError> {
    let (events, peer) = client.area_session(true).await?;
    Ok((events, peer.ok_or(BackendError::TaskStopped)?))
}

// A watcher holds no strong owner or service reference while awaiting a deadline.
// Expiry reserves the exact incarnation through the usual lifecycle fence.
async fn watch_idle(
    service: Weak<Inner>,
    id: AgentId,
    owner: Weak<NativeOwner>,
    mut clock: watch::Receiver<ReclaimClock>,
) {
    let mut retry_at = None;
    loop {
        let deadline = clock.borrow_and_update().deadline();
        if let Some(deadline) = deadline {
            let deadline = deadline.max(retry_at.unwrap_or(deadline));
            tokio::select! {
                biased;
                changed = clock.changed() => {
                    if changed.is_err() { return; }
                    retry_at = None;
                    continue;
                }
                () = tokio::time::sleep_until(deadline) => (),
            }
            let Some(inner) = service.upgrade() else {
                return;
            };
            let service = HostService { inner };
            let expected = owner.clone();
            let result = service
                .with_administration(move |reservation| {
                    Box::pin(async move {
                        reservation
                            .service
                            .close_inner(id, CloseIntent::Idle(&expected))
                            .await
                    })
                })
                .await;
            match result {
                Err(ServiceError::Busy | ServiceError::Backend(BackendError::Busy)) => {
                    retry_at = Some(Instant::now() + Duration::from_millis(100));
                }
                Err(ServiceError::NotLoaded | ServiceError::Stopping) | Ok(()) => return,
                Err(error) => {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "Host Agent {id} idle unload failed: {error}"
                    );
                    return;
                }
            }
        } else if clock.changed().await.is_err() {
            return;
        } else {
            retry_at = None;
        }
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
