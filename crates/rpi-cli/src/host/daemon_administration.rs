//! Daemon-owned administrative authority. Only private Host configuration or a
//! trusted Rust owner constructs these inputs; never deserialize a peer grant.
use super::{
    administration::{
        AdministrativeCoordinator, AdministrativeError, AdministrativeGrant,
        AdministrativePermissions, MAX_ADMINISTRATIVE_JOBS,
    },
    daemon::DaemonError,
    lifecycle::{LifecycleReceipt, LifecycleRequest},
    recovery::Bindings,
    service::HostService,
    AgentId, ControlScopeId, HostStartupAgent, ProfileRef, TrustedProfileSet, MAX_TRUSTED_PROFILES,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
};

/// Trusted native assembly input, not a wire DTO. Startup profiles are added to
/// the same frozen set; do not duplicate them here to grant restore authority.
pub struct DaemonAdministrationConfig {
    pub scope: ControlScopeId,
    pub allowed_agents: BTreeSet<AgentId>,
    pub permissions: AdministrativePermissions,
    pub max_jobs: NonZeroUsize,
    /// Configured for dynamic use, never automatically started or registered.
    pub profiles: Vec<HostStartupAgent>,
}

/// Cloneable native handle with fixed authority. No caller can supply a grant,
/// credentials, tool policy, alternate scope or mutable profile configuration.
/// This is not yet an IPC handler and grants no listener/daemon exit proof.
#[derive(Clone)]
pub struct DaemonAdministration {
    coordinator: AdministrativeCoordinator,
    grant: Arc<AdministrativeGrant>,
    references: Arc<[ProfileRef]>,
}
impl DaemonAdministration {
    pub fn scope(&self) -> ControlScopeId {
        self.grant.scope()
    }
    pub fn profile_references(&self) -> impl ExactSizeIterator<Item = ProfileRef> + '_ {
        self.references.iter().copied()
    }
    pub(super) async fn prepare_attach(
        &self,
        id: AgentId,
    ) -> Result<super::PreparedProfile, super::ordinary::BackendError> {
        let reference = self
            .profile_references()
            .find(|reference| reference.agent_id == id && self.grant.can_restore(id))
            .ok_or(super::ordinary::BackendError::Binding(
                "Session attach Restore is not authorized",
            ))?;
        self.coordinator.prepare_attach(reference).await
    }
    pub fn active_jobs(&self) -> usize {
        self.coordinator.active_jobs()
    }
    pub(crate) fn stop_completed(&self) -> tokio_util::sync::CancellationToken {
        self.coordinator.stop_completed()
    }
    pub async fn execute(
        &self,
        request: LifecycleRequest,
    ) -> Result<LifecycleReceipt, AdministrativeError> {
        self.coordinator.execute(&self.grant, request).await
    }
    pub async fn receipt(
        &self,
        request: &LifecycleRequest,
    ) -> Result<Option<LifecycleReceipt>, AdministrativeError> {
        self.coordinator.receipt(&self.grant, request).await
    }
}

pub(crate) struct PreparedDaemonAdministration {
    profiles: TrustedProfileSet,
    startup: Vec<ProfileRef>,
    grant: AdministrativeGrant,
    visible: Vec<ProfileRef>,
    restorable: BTreeSet<AgentId>,
    max_jobs: NonZeroUsize,
}
impl DaemonAdministrationConfig {
    /// All configuration and cross-set aliases are checked before catalog open,
    /// Session creation, writer acquisition, Agent registration or server bind.
    pub(crate) async fn prepare(
        self,
        mut startup: Vec<HostStartupAgent>,
    ) -> Result<PreparedDaemonAdministration, DaemonError> {
        if self.max_jobs.get() > MAX_ADMINISTRATIVE_JOBS
            || startup.len().saturating_add(self.profiles.len()) > MAX_TRUSTED_PROFILES
        {
            return Err(DaemonError::Administration);
        }
        let grant =
            AdministrativeGrant::new(self.scope, self.allowed_agents.clone(), self.permissions)
                .map_err(|_| DaemonError::Administration)?;
        let startup_ids: Vec<_> = startup.iter().map(|agent| agent.spec.id).collect();
        startup.extend(self.profiles);
        let profiles = TrustedProfileSet::freeze(startup)
            .await
            .map_err(|_| DaemonError::Administration)?;
        let references: BTreeMap<_, _> = profiles
            .references()
            .map(|reference| (reference.agent_id, reference))
            .collect();
        if !self
            .allowed_agents
            .iter()
            .all(|id| references.contains_key(id))
        {
            return Err(DaemonError::Administration);
        }
        let visible = references
            .values()
            .filter(|reference| self.allowed_agents.contains(&reference.agent_id))
            .copied()
            .collect();
        let startup = startup_ids
            .into_iter()
            .map(|id| {
                references
                    .get(&id)
                    .copied()
                    .ok_or(DaemonError::Administration)
            })
            .collect::<Result<_, _>>()?;
        let restorable = if self.permissions.restore {
            self.allowed_agents
        } else {
            BTreeSet::new()
        };
        Ok(PreparedDaemonAdministration {
            profiles,
            startup,
            grant,
            visible,
            restorable,
            max_jobs: self.max_jobs,
        })
    }
}
impl PreparedDaemonAdministration {
    pub(crate) async fn start(&self, service: &HostService) -> Result<(), DaemonError> {
        super::daemon::start_frozen_agents(service, &self.profiles, &self.startup).await
    }
    pub(super) async fn start_recovery(
        &self,
        service: &HostService,
        bindings: &Bindings,
        mini_count: usize,
    ) -> Result<Vec<AgentId>, DaemonError> {
        super::daemon::start_recovery(
            service,
            &self.profiles,
            &self.startup,
            &self.restorable,
            bindings,
            mini_count,
        )
        .await
    }
    pub(crate) fn assemble(
        self,
        service: HostService,
    ) -> Result<DaemonAdministration, DaemonError> {
        Ok(DaemonAdministration {
            coordinator: AdministrativeCoordinator::new(service, self.profiles, self.max_jobs)
                .map_err(|_| DaemonError::Administration)?,
            grant: Arc::new(self.grant),
            references: self.visible.into(),
        })
    }
}

#[cfg(test)]
#[path = "daemon_administration_tests.rs"]
mod tests;
