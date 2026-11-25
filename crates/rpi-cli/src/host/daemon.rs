//! Working Host ownership for foreground and supervised execution.
pub use super::daemon_administration::{DaemonAdministration, DaemonAdministrationConfig};
use super::{
    ordinary::{OrdinaryBinding, OrdinaryLimits},
    profile::HostStartupAgent,
    recovery::Bindings,
    server::{ControlConfig, ControlServer, ServerError},
    service::{HostService, ServiceError},
    AgentId, AgentRecord, AgentSpec, CatalogError, HostCatalog, HostConfig, ProfileRef,
    TrustedProfileSet,
};
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    DefaultIdGenerator, SystemClock,
};
use rpi_tools::OsExecutionEnv;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("shared Host daemon timers are not implemented; leave timeout policies unset")]
    UnsupportedTimers,
    #[error("Host daemon catalog initialization failed")]
    Catalog(#[source] CatalogError),
    #[error(transparent)]
    Control(#[from] ServerError),
    #[error("Host daemon retains unsettled owners; reconcile before stopping")]
    Settlement(#[source] ServiceError),
    #[error("Host daemon startup Agent profile was rejected")]
    StartupAgent,
    #[error("Host daemon administrative configuration was rejected")]
    Administration,
    #[error("Host daemon startup failed and settlement is incomplete; reconcile before retrying")]
    StartupCleanup(#[source] ServiceError),
}

pub struct HostDaemon {
    service: HostService,
    control: ControlServer,
    administration: Option<DaemonAdministration>,
    #[cfg(target_os = "macos")]
    launch_lock: Option<super::registry::InstanceLock>,
}
impl HostDaemon {
    pub async fn open(
        directory: PathBuf,
        host: HostConfig,
        control: ControlConfig,
    ) -> Result<Self, DaemonError> {
        Self::open_with_agents(directory, host, control, Vec::new()).await
    }

    /// Open the catalog, start the explicitly configured Agents, then bind the
    /// control endpoint. A startup failure settles every started owner before
    /// returning; it never publishes readiness with a partially started Host.
    pub async fn open_with_agents(
        directory: PathBuf,
        host: HostConfig,
        control: ControlConfig,
        agents: Vec<HostStartupAgent>,
    ) -> Result<Self, DaemonError> {
        Self::open_configured(directory, host, control, agents, None).await
    }

    /// Explicit private policy and dynamically available profiles. Only `agents`
    /// are started. One union is frozen before catalog or Session creation.
    pub async fn open_configured(
        directory: PathBuf,
        host: HostConfig,
        control: ControlConfig,
        agents: Vec<HostStartupAgent>,
        administration: Option<DaemonAdministrationConfig>,
    ) -> Result<Self, DaemonError> {
        Self::open_with_mini(directory, host, control, agents, administration, None).await
    }

    /// Mini is opt-in via separate private authority. It never inherits the
    /// Persistent administrative grant or durable lifecycle journal.
    pub async fn open_with_mini(
        directory: PathBuf,
        host: HostConfig,
        control: ControlConfig,
        agents: Vec<HostStartupAgent>,
        administration: Option<DaemonAdministrationConfig>,
        mini_control: Option<super::mini_control::MiniControlConfig>,
    ) -> Result<Self, DaemonError> {
        Self::open_with_recovery(
            directory,
            host,
            control,
            agents,
            administration,
            mini_control,
            (None, None, None),
        )
        .await
    }

    pub(super) async fn open_with_recovery(
        directory: PathBuf,
        host: HostConfig,
        control: ControlConfig,
        agents: Vec<HostStartupAgent>,
        administration: Option<DaemonAdministrationConfig>,
        mini_control: Option<super::mini_control::MiniControlConfig>,
        startup: (
            Option<Bindings>,
            Option<std::os::unix::net::UnixListener>,
            Option<AgentId>,
        ),
    ) -> Result<Self, DaemonError> {
        let (recovery, listener, session_rpc_agent) = startup;
        // Reject invalid options before opening SQLite or creating directories.
        control.validate()?;
        if host.host_idle_timeout_ms.is_some()
            || host.default_timeouts != Default::default()
            || host.hard_timeout_limits != Default::default()
        {
            return Err(DaemonError::UnsupportedTimers);
        }
        host.validate()
            .map_err(CatalogError::from)
            .map_err(DaemonError::Catalog)?;
        match &mini_control {
            Some(policy) => policy.preflight(&agents).await?,
            None if agents
                .iter()
                .any(|agent| agent.spec.mode == super::AgentMode::Mini) =>
            {
                return Err(DaemonError::StartupAgent);
            }
            None => {}
        }
        let (minis, agents): (Vec<_>, Vec<_>) = agents
            .into_iter()
            .partition(|agent| agent.spec.mode == super::AgentMode::Mini);
        if administration.as_ref().is_some_and(|policy| {
            policy
                .profiles
                .iter()
                .any(|profile| minis.iter().any(|mini| mini.spec.id == profile.spec.id))
        }) {
            return Err(DaemonError::Administration);
        }
        let (agents, prepared, recovery_profiles) = match administration {
            Some(policy) => (Vec::new(), Some(policy.prepare(agents).await?), None),
            None if recovery.is_some() => {
                let profiles = TrustedProfileSet::freeze(agents)
                    .await
                    .map_err(|_| DaemonError::StartupAgent)?;
                (Vec::new(), None, Some(profiles))
            }
            None => (agents, None, None),
        };
        #[cfg(target_os = "macos")]
        let launch_lock = if listener.is_none() {
            Some(
                super::registry::own_directory_lock(&directory, "supervisor.lock")
                    .map_err(DaemonError::Catalog)?,
            )
        } else {
            None
        };
        let catalog =
            HostCatalog::open_with_recovery(directory, host, recovery.clone().unwrap_or_default())
                .await
                .map_err(DaemonError::Catalog)?;
        let service = HostService::new(catalog);
        let startup = async {
            if let Some(bindings) = &recovery {
                let loaded = match &prepared {
                    Some(policy) => {
                        policy
                            .start_recovery(&service, bindings, minis.len())
                            .await?
                    }
                    None => {
                        let profiles = recovery_profiles
                            .as_ref()
                            .ok_or(DaemonError::StartupAgent)?;
                        let startup: Vec<_> = profiles.references().collect();
                        start_recovery(
                            &service,
                            profiles,
                            &startup,
                            &BTreeSet::new(),
                            bindings,
                            minis.len(),
                        )
                        .await?
                    }
                };
                super::mini_control::start_minis(&service, minis).await?;
                return service
                    .catalog()
                    .remember_members(loaded)
                    .await
                    .map_err(DaemonError::Catalog);
            }
            super::mini_control::start_minis(&service, minis).await?;
            match &prepared {
                Some(prepared) => prepared.start(&service).await,
                None => start_agents(&service, agents).await,
            }
        }
        .await;
        if let Err(error) = startup {
            // A failed startup must not leave owners or catalog ownership behind.
            return match service.shutdown().await {
                Ok(()) => Err(error),
                Err(settlement) => Err(DaemonError::StartupCleanup(settlement)),
            };
        }
        let administration = match prepared
            .map(|prepared| prepared.assemble(service.clone()))
            .transpose()
        {
            Ok(administration) => administration,
            Err(error) => {
                return match service.shutdown().await {
                    Ok(()) => Err(error),
                    Err(settlement) => Err(DaemonError::StartupCleanup(settlement)),
                };
            }
        };
        let mini = mini_control
            .map(|policy| super::mini_control::MiniControl::new(service.clone(), policy));
        let server = match ControlServer::bind_with_controls(
            control,
            service.clone(),
            administration.clone(),
            mini,
            listener,
            session_rpc_agent,
        )
        .await
        {
            Ok(server) => server,
            Err(error) => {
                service.shutdown().await.map_err(DaemonError::Settlement)?;
                return Err(error.into());
            }
        };
        Ok(Self {
            service,
            control: server,
            administration,
            #[cfg(target_os = "macos")]
            launch_lock,
        })
    }
    /// Trusted native integrations can install explicit bindings. This handle
    /// is not an IPC grant and must not be exposed directly to model tools.
    pub fn service(&self) -> &HostService {
        &self.service
    }
    /// Trusted native handle. Its private policy fixes all authority for the
    /// lifetime; IPC only wraps explicitly implemented typed operations and
    /// never accepts a grant from a caller.
    pub fn administration(&self) -> Option<&DaemonAdministration> {
        self.administration.as_ref()
    }
    pub fn welcome(&self) -> &super::protocol::Welcome {
        self.control.welcome()
    }
    pub async fn wait_control(&mut self) -> Result<(), ServerError> {
        self.control.wait().await
    }
    pub(crate) fn control_stopped_by_administration(&self) -> bool {
        self.control.stopped_by_administration()
    }
    /// Failed native settlement deliberately retains owners, catalog ownership
    /// and the query listener for diagnosis/retry. Never force-abort backends.
    pub async fn shutdown(&mut self) -> Result<(), DaemonError> {
        self.service
            .shutdown()
            .await
            .map_err(DaemonError::Settlement)?;
        self.control.stop().await?;
        #[cfg(target_os = "macos")]
        drop(self.launch_lock.take());
        Ok(())
    }
}

async fn start_agents(
    service: &HostService,
    agents: Vec<HostStartupAgent>,
) -> Result<(), DaemonError> {
    for agent in agents {
        let binding = agent
            .profile
            .bind(agent.spec.clone())
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
        let existing = preflight_startup(service, &agent.spec, binding).await?;
        let binding = agent
            .profile
            .bind(agent.spec.clone())
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
        load_startup(service, agent.spec, agent.limits, binding, existing).await?;
    }
    Ok(())
}

pub(super) async fn start_frozen_agents(
    service: &HostService,
    profiles: &TrustedProfileSet,
    startup: &[ProfileRef],
) -> Result<(), DaemonError> {
    for &reference in startup {
        let prepared = profiles
            .prepare(reference)
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
        let existing = preflight_startup(service, &prepared.spec, prepared.binding).await?;
        let fresh = profiles
            .prepare(reference)
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
        load_startup(service, fresh.spec, fresh.limits, fresh.binding, existing).await?;
    }
    Ok(())
}

pub(super) async fn start_recovery(
    service: &HostService,
    profiles: &TrustedProfileSet,
    startup: &[ProfileRef],
    restorable: &BTreeSet<AgentId>,
    bindings: &Bindings,
    mini_count: usize,
) -> Result<Vec<AgentId>, DaemonError> {
    let catalog = service.catalog();
    let previous = catalog
        .recovery_members(startup.iter().map(|reference| reference.agent_id).collect())
        .await
        .map_err(DaemonError::Catalog)?;
    let mut selected: BTreeMap<_, _> = startup
        .iter()
        .map(|reference| (reference.agent_id, *reference))
        .collect();
    let configured: BTreeMap<_, _> = profiles
        .references()
        .map(|reference| (reference.agent_id, reference))
        .collect();
    for (&id, version) in &previous {
        match version {
            None => {
                selected.remove(&id);
            }
            Some(version) => {
                if bindings.get(&id) != Some(version)
                    || !(selected.contains_key(&id) || restorable.contains(&id))
                {
                    return Err(DaemonError::StartupAgent);
                }
                selected.insert(id, *configured.get(&id).ok_or(DaemonError::StartupAgent)?);
            }
        }
    }
    let capacity = catalog
        .status()
        .await
        .map_err(DaemonError::Catalog)?
        .max_agents;
    if capacity.is_some_and(|limit| selected.len().saturating_add(mini_count) > limit) {
        return Err(DaemonError::StartupAgent);
    }
    // Check every existing history before creating any new Session or taking a
    // writer. A recorded member missing its history is a permanent failure.
    for (&id, &reference) in &selected {
        let prepared = profiles
            .prepare(reference)
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
        let session = prepared
            .spec
            .session
            .as_ref()
            .ok_or(DaemonError::StartupAgent)?;
        match catalog.get(id).await {
            Ok(record) if record.spec != prepared.spec => return Err(DaemonError::StartupAgent),
            Ok(_) => {}
            Err(CatalogError::UnknownAgent) if !previous.contains_key(&id) => {
                match tokio::fs::symlink_metadata(&session.path).await {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => {}
                }
            }
            Err(CatalogError::UnknownAgent) => return Err(DaemonError::StartupAgent),
            Err(error) => return Err(DaemonError::Catalog(error)),
        }
        prepared
            .binding
            .preflight_session(&prepared.spec)
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
    }
    let references: Vec<_> = selected.values().copied().collect();
    start_frozen_agents(service, profiles, &references).await?;
    Ok(selected.into_keys().collect())
}

async fn preflight_startup(
    service: &HostService,
    spec: &AgentSpec,
    binding: OrdinaryBinding,
) -> Result<Option<AgentRecord>, DaemonError> {
    let existing = match service.catalog().get(spec.id).await {
        Ok(record) if record.spec == *spec => Some(record),
        Ok(_) => return Err(DaemonError::StartupAgent),
        Err(CatalogError::UnknownAgent) => None,
        Err(error) => return Err(DaemonError::Catalog(error)),
    };
    if existing.is_none() {
        ensure_session(spec).await?;
    }
    binding
        .preflight_session(spec)
        .await
        .map_err(|_| DaemonError::StartupAgent)?;
    Ok(existing)
}

async fn load_startup(
    service: &HostService,
    spec: AgentSpec,
    limits: OrdinaryLimits,
    binding: OrdinaryBinding,
    existing: Option<AgentRecord>,
) -> Result<(), DaemonError> {
    let catalog = service.catalog();
    let record = match existing {
        Some(record) => record,
        None => catalog.register(spec).await.map_err(DaemonError::Catalog)?,
    };
    service
        .restore_bound(record.spec.id, catalog.guard(&record), limits, binding)
        .await
        .map_err(|_| DaemonError::StartupAgent)?;
    Ok(())
}

/// Create only a *new* Session file for a profile that names an absent path.
/// Existing files are left for the ordinary strict restore to verify, so an
/// external or torn history is never overwritten here.
async fn ensure_session(spec: &AgentSpec) -> Result<(), DaemonError> {
    let session = spec.session.as_ref().ok_or(DaemonError::StartupAgent)?;
    match tokio::fs::symlink_metadata(&session.path).await {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(DaemonError::StartupAgent),
    }
    let cwd = super::profile::canonical_workspace(spec).map_err(|_| DaemonError::StartupAgent)?;
    super::profile::session_parent(&session.path).map_err(|_| DaemonError::StartupAgent)?;
    let path = session.path.to_str().ok_or(DaemonError::StartupAgent)?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let storage = JsonlSessionStorage::create_new(
        Arc::new(OsExecutionEnv::with_cwd(cwd.clone())),
        path,
        JsonlV4Header::new(
            session.id.clone(),
            created_at,
            cwd.to_string_lossy().into_owned(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .map_err(|_| DaemonError::StartupAgent)?;
    storage.drain().await;
    storage.release_writer_lease();
    Ok(())
}
