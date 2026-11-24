//! Foreground daemon ownership. No background spawn, implicit discovery,
//! catalog-wide Agent restore, ambient secret hydration or management-extension loading.
pub use super::daemon_administration::{DaemonAdministration, DaemonAdministrationConfig};
use super::{
    ordinary::{OrdinaryBinding, OrdinaryLimits},
    profile::HostStartupAgent,
    server::{ControlConfig, ControlServer, ServerError},
    service::{HostService, ServiceError},
    AgentRecord, AgentSpec, CatalogError, HostCatalog, HostConfig, ProfileRef, TrustedProfileSet,
};
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    DefaultIdGenerator, SystemClock,
};
use rpi_tools::OsExecutionEnv;
use std::{
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
        // Raw Mini assembly is Session-free, but startup is not yet connected
        // to its owner/volatile execution engine. Fail closed before SQLite or
        // any earlier persistent profile can create a Session.
        if agents
            .iter()
            .any(|agent| agent.spec.mode != super::AgentMode::Persistent)
        {
            return Err(DaemonError::StartupAgent);
        }
        let (agents, prepared) = match administration {
            Some(policy) => (Vec::new(), Some(policy.prepare(agents).await?)),
            None => (agents, None),
        };
        let catalog = HostCatalog::open(directory, host)
            .await
            .map_err(DaemonError::Catalog)?;
        let service = HostService::new(catalog);
        let startup = match &prepared {
            Some(prepared) => prepared.start(&service).await,
            None => start_agents(&service, agents).await,
        };
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
        let server = match ControlServer::bind_with_administration(
            control,
            service.clone(),
            administration.clone(),
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
