//! Trusted in-process administrative facade. No IPC parser or peer-minted
//! authority. Intent, native effects and result publication are not atomic;
//! uncertain work is inspect-only, never automatically replayed.
use super::{
    lifecycle::{
        LifecycleAction, LifecycleFailure, LifecycleReceipt, LifecycleRequest, LifecycleResult,
        LifecycleState,
    },
    ordinary::BackendError,
    service::{HostService, OwnerPhase, ServiceError, ServiceReservation, StopReservation},
    AgentId, AgentState, CatalogError, CatalogGuard, ControlScopeId, PreparedProfile, ProfileRef,
    RequestId, TrustedProfileSet, MAX_TRUSTED_PROFILES,
};
use futures::FutureExt;
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    DefaultIdGenerator, SystemClock,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const MAX_ADMINISTRATIVE_JOBS: usize = 64;
pub type AdministrativeResult = Result<LifecycleReceipt, AdministrativeError>;

/// Fixed labels only: never return private backend diagnostics to a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdministrativeError {
    #[error("administrative request is not authorized")]
    Unauthorized,
    #[error("administrative request or trusted preflight was rejected")]
    InvalidRequest,
    #[error("administrative request conflicts with existing custody")]
    Conflict,
    #[error("administrative target was not found")]
    NotFound,
    #[error("administrative Host epoch is stale")]
    StaleEpoch,
    #[error("administrative Agent revision is stale")]
    StaleRevision,
    #[error("administrative target is busy")]
    Busy,
    #[error("administrative capacity is exhausted")]
    Capacity,
    #[error("Host is stopping")]
    Stopping,
    #[error("unbound lifecycle execution is not supported")]
    Unsupported,
    #[error("administrative outcome is uncertain; inspect without replaying")]
    OutcomeUnknown,
}
impl From<CatalogError> for AdministrativeError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::UnknownAgent => Self::NotFound,
            CatalogError::EpochConflict => Self::StaleEpoch,
            CatalogError::RevisionConflict { .. } => Self::StaleRevision,
            CatalogError::AlreadyRegistered
            | CatalogError::SessionAlreadyRegistered
            | CatalogError::LifecycleConflict => Self::Conflict,
            CatalogError::LifecycleBusy => Self::Busy,
            CatalogError::LifecycleCapacity
            | CatalogError::Policy(super::HostError::AtCapacity { .. }) => Self::Capacity,
            _ => Self::OutcomeUnknown,
        }
    }
}

/// Trusted native permissions, all denied by default. Not Deserialize: a
/// request's scope UUID, profile nonce or JSON cannot mint these rights.
#[derive(Clone, Copy, Default)]
pub struct AdministrativePermissions {
    pub create: bool,
    pub restore: bool,
    pub cancel: bool,
    pub close: bool,
    pub stop: bool,
}
/// Issued by the Host owner, not a client/model. Immutable and deliberately not
/// Debug/Serialize. Dropping a grant does not revoke an already accepted job.
pub struct AdministrativeGrant {
    scope: ControlScopeId,
    agents: BTreeSet<AgentId>,
    permissions: AdministrativePermissions,
}
impl AdministrativeGrant {
    pub fn new(
        scope: ControlScopeId,
        agents: BTreeSet<AgentId>,
        permissions: AdministrativePermissions,
    ) -> Result<Self, AdministrativeError> {
        if agents.len() > MAX_TRUSTED_PROFILES {
            return Err(AdministrativeError::InvalidRequest);
        }
        Ok(Self {
            scope,
            agents,
            permissions,
        })
    }
    pub fn scope(&self) -> ControlScopeId {
        self.scope
    }
    pub(super) fn can_restore(&self, id: AgentId) -> bool {
        self.permissions.restore && self.agents.contains(&id)
    }
    fn authorize(&self, request: &LifecycleRequest) -> Result<(), AdministrativeError> {
        let permitted = match request.action {
            LifecycleAction::Create { .. } | LifecycleAction::CreateProfile { .. } => {
                self.permissions.create
            }
            LifecycleAction::Restore { .. } | LifecycleAction::RestoreProfile { .. } => {
                self.permissions.restore
            }
            LifecycleAction::Cancel { .. } | LifecycleAction::CancelSubmitted { .. } => {
                self.permissions.cancel
            }
            LifecycleAction::Close { .. } => self.permissions.close,
            LifecycleAction::Stop {} => self.permissions.stop,
        };
        if request.scope != self.scope
            || !permitted
            || request
                .action
                .agent()
                .is_some_and(|id| !self.agents.contains(&id))
        {
            return Err(AdministrativeError::Unauthorized);
        }
        request
            .validate()
            .map_err(|_| AdministrativeError::InvalidRequest)
    }
}

type Key = (ControlScopeId, RequestId);
type Observer = watch::Receiver<Option<AdministrativeResult>>;
struct Job {
    request: LifecycleRequest,
    observer: Observer,
}
struct Inner {
    service: HostService,
    profiles: TrustedProfileSet,
    limit: usize,
    jobs: Mutex<BTreeMap<Key, Job>>,
    // At most one immutable, already-checkpointed successful Stop result. The
    // catalog is then closed, so duplicate Stop cannot query its worker again.
    settled_receipt: Mutex<Option<LifecycleReceipt>>,
    // Volatile completion of this owner's fresh Stop, never restored from
    // history. A NativeSettled receipt alone is not storage/transport exit.
    stop_completed: CancellationToken,
}
impl Inner {
    fn jobs(&self) -> MutexGuard<'_, BTreeMap<Key, Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}
/// Retain for the entire service lifetime. Only one facade can claim a service;
/// dropping it never hands an epoch's live/uncertain custody to another facade.
/// The bounded map owns jobs, not waiters. No global mutex is held across await.
#[derive(Clone)]
pub struct AdministrativeCoordinator {
    inner: Arc<Inner>,
}
impl AdministrativeCoordinator {
    pub fn new(
        service: HostService,
        profiles: TrustedProfileSet,
        limit: NonZeroUsize,
    ) -> Result<Self, AdministrativeError> {
        if limit.get() > MAX_ADMINISTRATIVE_JOBS {
            return Err(AdministrativeError::InvalidRequest);
        }
        if service.is_stopping() {
            return Err(AdministrativeError::Stopping);
        }
        if !service.claim_administration() {
            return Err(AdministrativeError::Conflict);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                service,
                profiles,
                limit: limit.get(),
                jobs: Mutex::new(BTreeMap::new()),
                settled_receipt: Mutex::new(None),
                stop_completed: CancellationToken::new(),
            }),
        })
    }
    pub fn active_jobs(&self) -> usize {
        self.inner.jobs().len()
    }
    pub(crate) fn stop_completed(&self) -> CancellationToken {
        self.inner.stop_completed.clone()
    }
    /// Native owner discovery only, not a client-facing grant/list operation.
    pub fn profile_references(&self) -> impl ExactSizeIterator<Item = ProfileRef> + '_ {
        self.inner.profiles.references()
    }

    fn settled_receipt(
        &self,
        request: &LifecycleRequest,
    ) -> Result<Option<LifecycleReceipt>, AdministrativeError> {
        let cached = self
            .inner
            .settled_receipt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match cached.as_ref().filter(|receipt| {
            receipt.request.scope == request.scope
                && receipt.request.request_id == request.request_id
        }) {
            Some(receipt) if receipt.request != *request => Err(AdministrativeError::Conflict),
            receipt => Ok(receipt.cloned()),
        }
    }

    /// Authenticated full-parameter reconciliation, including historical refs
    /// no longer in the current frozen set. A found row grants no execution.
    pub async fn receipt(
        &self,
        grant: &AdministrativeGrant,
        request: &LifecycleRequest,
    ) -> Result<Option<LifecycleReceipt>, AdministrativeError> {
        grant.authorize(request)?;
        if let Some(receipt) = self.settled_receipt(request)? {
            return Ok(Some(receipt));
        }
        let receipt = self
            .inner
            .service
            .catalog()
            .lifecycle_receipt(request.scope, request.request_id)
            .await?;
        if receipt
            .as_ref()
            .is_some_and(|receipt| receipt.request != *request)
        {
            return Err(AdministrativeError::Conflict);
        }
        Ok(receipt)
    }
    fn existing(
        &self,
        request: &LifecycleRequest,
    ) -> Result<Option<Observer>, AdministrativeError> {
        let jobs = self.inner.jobs();
        jobs.get(&(request.scope, request.request_id))
            .map(|job| {
                if job.request != *request {
                    Err(AdministrativeError::Conflict)
                } else {
                    Ok(job.observer.clone())
                }
            })
            .transpose()
    }
    pub async fn execute(
        &self,
        grant: &AdministrativeGrant,
        request: LifecycleRequest,
    ) -> AdministrativeResult {
        grant.authorize(&request)?;
        if let Some(observer) = self.existing(&request)? {
            return observe(observer).await;
        }
        // Do not resolve the current profile until old custody has been read.
        let previous = self.receipt(grant, &request).await?;
        let key = (request.scope, request.request_id);
        let (observer, sender) = {
            let mut jobs = self.inner.jobs();
            if let Some(job) = jobs.get(&key) {
                if job.request != request {
                    return Err(AdministrativeError::Conflict);
                }
                (job.observer.clone(), None)
            } else {
                if let Some(receipt) = previous {
                    return Ok(receipt);
                }
                if jobs.len() >= self.inner.limit {
                    return Err(AdministrativeError::Capacity);
                }
                let (sender, observer) = watch::channel(None);
                jobs.insert(
                    key,
                    Job {
                        request: request.clone(),
                        observer: observer.clone(),
                    },
                );
                (observer, Some(sender))
            }
        };
        if let Some(sender) = sender {
            let owner = self.clone();
            // No await between claiming the local slot and spawning its owner.
            tokio::spawn(async move {
                let result = AssertUnwindSafe(owner.run(request))
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(AdministrativeError::OutcomeUnknown));
                sender.send_replace(Some(result));
                owner.inner.jobs().remove(&key);
            });
        }
        observe(observer).await
    }
    pub(super) async fn prepare_attach(
        &self,
        reference: ProfileRef,
    ) -> Result<PreparedProfile, BackendError> {
        self.inner.profiles.prepare(reference).await
    }

    async fn run(&self, request: LifecycleRequest) -> AdministrativeResult {
        if let Some(receipt) = self.settled_receipt(&request)? {
            return Ok(receipt);
        }
        let owner = self.clone();
        let result = if matches!(request.action, LifecycleAction::Stop {}) {
            self.inner
                .service
                .with_stop_reservation(false, move |reserved| {
                    Box::pin(async move { Ok(owner.run_stop(reserved, request).await) })
                })
                .await
        } else {
            self.inner
                .service
                .with_administration(move |reserved| {
                    Box::pin(async move { Ok(owner.run_reserved(reserved, request).await) })
                })
                .await
        };
        result.map_err(|error| match error {
            ServiceError::Stopping => AdministrativeError::Stopping,
            _ => AdministrativeError::OutcomeUnknown,
        })?
    }
    async fn preflight(
        &self,
        reserved: &ServiceReservation<'_>,
        request: &LifecycleRequest,
    ) -> Result<Option<PreparedProfile>, AdministrativeError> {
        let catalog = reserved.catalog();
        if request.epoch != catalog.epoch() {
            return Err(AdministrativeError::StaleEpoch);
        }
        match request.action {
            LifecycleAction::Create { .. } | LifecycleAction::Restore { .. } => {
                Err(AdministrativeError::Unsupported)
            }
            LifecycleAction::CreateProfile { profile }
            | LifecycleAction::RestoreProfile { profile, .. } => {
                let prepared = self
                    .inner
                    .profiles
                    .prepare(profile)
                    .await
                    .map_err(|_| AdministrativeError::InvalidRequest)?;
                match request.action {
                    LifecycleAction::CreateProfile { .. } => {
                        match catalog.get(profile.agent_id).await {
                            Err(CatalogError::UnknownAgent) => {}
                            Ok(_) => return Err(AdministrativeError::Conflict),
                            Err(error) => return Err(error.into()),
                        }
                        super::profile::canonical_workspace(&prepared.spec)
                            .map_err(|_| AdministrativeError::InvalidRequest)?;
                        let session = prepared
                            .spec
                            .session
                            .as_ref()
                            .ok_or(AdministrativeError::InvalidRequest)?;
                        super::profile::session_parent(&session.path)
                            .map_err(|_| AdministrativeError::InvalidRequest)?;
                        match tokio::fs::symlink_metadata(&session.path).await {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Ok(_) => return Err(AdministrativeError::Conflict),
                            Err(_) => return Err(AdministrativeError::InvalidRequest),
                        }
                        Ok(Some(prepared))
                    }
                    _ => {
                        let record = catalog.get(profile.agent_id).await?;
                        if record.spec != prepared.spec {
                            return Err(AdministrativeError::InvalidRequest);
                        }
                        if Some(record.revision) != request.action.revision() {
                            return Err(AdministrativeError::StaleRevision);
                        }
                        if self.inner.service.phase(profile.agent_id).is_some() {
                            return Err(AdministrativeError::Busy);
                        }
                        prepared
                            .binding
                            .preflight_session(&prepared.spec)
                            .await
                            .map_err(|_| AdministrativeError::InvalidRequest)?;
                        // A fresh environment, never cached by the frozen set. The
                        // real restore repeats readonly validation before writer.
                        Ok(Some(
                            self.inner
                                .profiles
                                .prepare(profile)
                                .await
                                .map_err(|_| AdministrativeError::InvalidRequest)?,
                        ))
                    }
                }
            }
            _ => Ok(None),
        }
    }
    async fn run_reserved(
        &self,
        reserved: ServiceReservation<'_>,
        request: LifecycleRequest,
    ) -> AdministrativeResult {
        // An entry lookup may have seen None before an earlier identical job
        // finished and vacated the map. Reconcile again inside the reservation,
        // before stale profile/owner checks can misclassify that duplicate.
        if let Some(receipt) = reserved
            .catalog()
            .lifecycle_receipt(request.scope, request.request_id)
            .await?
        {
            if receipt.request != request {
                return Err(AdministrativeError::Conflict);
            }
            return Ok(receipt);
        }
        let prepared = self.preflight(&reserved, &request).await?;
        let admission = reserved
            .catalog()
            .prepare_lifecycle(request.clone())
            .await?;
        if !admission.fresh {
            return Ok(admission.receipt);
        }
        // Only this fresh branch may publish an outcome. An uncertain prepare
        // error above never takes over a visible row or writes a fake Rejected.
        let result = AssertUnwindSafe(self.act(&reserved, &request, prepared))
            .catch_unwind()
            .await;
        let status = match result {
            Ok(Ok(result)) => LifecycleState::Succeeded { result },
            Ok(Err(status)) => status,
            Err(_) => LifecycleState::Uncertain {
                code: LifecycleFailure::Internal,
            },
        };
        Ok(reserved.catalog().finish_lifecycle(request, status).await?)
    }
    async fn act(
        &self,
        reserved: &ServiceReservation<'_>,
        request: &LifecycleRequest,
        prepared: Option<PreparedProfile>,
    ) -> Result<LifecycleResult, LifecycleState> {
        let guard = CatalogGuard {
            epoch: request.epoch,
            revision: request.action.revision().unwrap_or(1),
        };
        match request.action {
            LifecycleAction::CreateProfile { .. } | LifecycleAction::RestoreProfile { .. } => {
                let prepared = prepared.ok_or_else(internal_uncertain)?;
                self.load(
                    reserved,
                    prepared,
                    matches!(request.action, LifecycleAction::CreateProfile { .. }),
                    guard,
                )
                .await
                .map(|revision| LifecycleResult::Loaded { revision })
                // Creating files/registering/Starting may have happened.
                .map_err(|_| internal_uncertain())
            }
            LifecycleAction::Cancel {
                agent_id,
                ticket_id,
                ..
            } => reserved
                .cancel_guarded(agent_id, guard, ticket_id)
                .await
                .map(|signalled| LifecycleResult::CancelSignalled { signalled })
                .map_err(classify_guarded),
            LifecycleAction::CancelSubmitted {
                agent_id,
                submitted_request_id,
                ..
            } => reserved
                .cancel_submitted_guarded(agent_id, guard, submitted_request_id)
                .await
                .map(|signalled| LifecycleResult::CancelSignalled { signalled })
                .map_err(classify_guarded),
            LifecycleAction::Close { agent_id, .. } => {
                reserved
                    .close_guarded(agent_id, guard)
                    .await
                    .map_err(classify_guarded)?;
                let record = reserved
                    .catalog()
                    .get(agent_id)
                    .await
                    .map_err(|_| internal_uncertain())?;
                if record.state != AgentState::Closed {
                    return Err(internal_uncertain());
                }
                Ok(LifecycleResult::Closed {
                    revision: record.revision,
                })
            }
            _ => Err(internal_uncertain()),
        }
    }
    async fn load(
        &self,
        reserved: &ServiceReservation<'_>,
        prepared: PreparedProfile,
        create: bool,
        guard: CatalogGuard,
    ) -> Result<u64, AdministrativeError> {
        let PreparedProfile {
            reference,
            spec,
            limits,
            binding,
        } = prepared;
        let (record, binding) = if create {
            let session = spec
                .session
                .as_ref()
                .ok_or(AdministrativeError::InvalidRequest)?;
            let path = session
                .path
                .to_str()
                .ok_or(AdministrativeError::InvalidRequest)?;
            let env = binding
                .execution_environment()
                .map_err(|_| AdministrativeError::InvalidRequest)?;
            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            let storage = JsonlSessionStorage::create_new(
                env,
                path,
                JsonlV4Header::new(
                    session.id.clone(),
                    created_at,
                    format!("{}", spec.launch.cwd.display()),
                    None,
                    None,
                    None,
                ),
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .map_err(|_| AdministrativeError::OutcomeUnknown)?;
            storage.drain().await;
            storage.release_writer_lease();
            binding
                .preflight_session(&spec)
                .await
                .map_err(|_| AdministrativeError::OutcomeUnknown)?;
            let binding = self
                .inner
                .profiles
                .prepare(reference)
                .await
                .map_err(|_| AdministrativeError::OutcomeUnknown)?
                .binding;
            (reserved.catalog().register(spec).await?, binding)
        } else {
            let record = reserved.catalog().get(spec.id).await?;
            if record.spec != spec || record.revision != guard.revision {
                return Err(AdministrativeError::Conflict);
            }
            (record, binding)
        };
        let id = record.spec.id;
        reserved
            .restore_bound(
                id,
                if create {
                    reserved.catalog().guard(&record)
                } else {
                    guard
                },
                limits,
                binding,
            )
            .await
            .map_err(|_| AdministrativeError::OutcomeUnknown)?;
        let loaded = reserved.catalog().get(id).await?;
        if self.inner.service.phase(id) != Some(OwnerPhase::Live) || !loaded.state.is_resident() {
            return Err(AdministrativeError::OutcomeUnknown);
        }
        Ok(loaded.revision)
    }
    async fn run_stop(
        &self,
        mut reserved: StopReservation<'_>,
        request: LifecycleRequest,
    ) -> AdministrativeResult {
        let admission = reserved
            .catalog()
            .prepare_lifecycle(request.clone())
            .await?;
        if !admission.fresh {
            return Ok(admission.receipt);
        }
        // Keep the write reservation even when drain fails or panics, so an
        // external unconditional shutdown cannot close storage before finish.
        let settled = AssertUnwindSafe(reserved.settle()).catch_unwind().await;
        if !matches!(settled, Ok(Ok(()))) {
            return Ok(reserved
                .catalog()
                .finish_lifecycle(request, internal_uncertain())
                .await?);
        }
        let drained = reserved
            .drain()
            .await
            .map_err(|_| AdministrativeError::OutcomeUnknown)?;
        let receipt = drained
            .catalog()
            .finish_lifecycle(
                request,
                LifecycleState::Succeeded {
                    result: LifecycleResult::NativeSettled {},
                },
            )
            .await?;
        *self
            .inner
            .settled_receipt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(receipt.clone());
        drained
            .close_storage()
            .await
            .map_err(|_| AdministrativeError::OutcomeUnknown)?;
        self.inner.stop_completed.cancel();
        Ok(receipt)
    }
}
async fn observe(mut observer: Observer) -> AdministrativeResult {
    loop {
        let result = observer.borrow().clone();
        if let Some(result) = result {
            return result;
        }
        observer
            .changed()
            .await
            .map_err(|_| AdministrativeError::OutcomeUnknown)?;
    }
}
fn internal_uncertain() -> LifecycleState {
    LifecycleState::Uncertain {
        code: LifecycleFailure::Internal,
    }
}

#[cfg(test)]
#[path = "administration_tests.rs"]
mod tests;
fn classify_guarded(error: ServiceError) -> LifecycleState {
    let code = match error {
        ServiceError::NotLoaded => LifecycleFailure::NotFound,
        ServiceError::Busy
        | ServiceError::AlreadyOwned
        | ServiceError::Backend(BackendError::Busy) => LifecycleFailure::Busy,
        ServiceError::Backend(BackendError::GuardRejected(error)) => match error.as_ref() {
            CatalogError::EpochConflict => LifecycleFailure::StaleEpoch,
            CatalogError::RevisionConflict { .. } => LifecycleFailure::StaleRevision,
            _ => return internal_uncertain(),
        },
        _ => return internal_uncertain(),
    };
    LifecycleState::Rejected { code }
}
