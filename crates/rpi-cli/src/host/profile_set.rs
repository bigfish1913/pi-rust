//! Frozen trusted native profiles. Public refs contain no configuration or
//! secret digest. This is not an IPC parser, grant issued by a client, filesystem
//! sandbox, catalog mutation or execution coordinator.
use super::{
    ordinary::{BackendError, BackendResult, OrdinaryBinding, OrdinaryLimits},
    AgentId, AgentMode, AgentSpec, ExecutionMode, HostStartupAgent, ProfileRef, ProfileVersion,
};
use std::collections::{BTreeMap, BTreeSet};

/// Trusted inputs are still bounded, independently of the IPC frame budget.
pub const MAX_TRUSTED_PROFILES: usize = 1024;

struct FrozenProfile {
    reference: ProfileRef,
    agent: HostStartupAgent,
}

/// Owns private inputs without Debug/Serialize, replacement or insertion APIs.
/// Construct only from Host-owned inputs, never a peer's profile JSON. Versions
/// are random nonces, not hashes that could become a credential guessing oracle.
pub struct TrustedProfileSet {
    agents: BTreeMap<AgentId, FrozenProfile>,
}

/// A fresh, owned binding snapshot. Preparing creates no Session, acquires no
/// Session writer and registers no Agent. Saved resource facts still have to be
/// validated by strict restore before writer acquisition/repair.
pub struct PreparedProfile {
    pub reference: ProfileRef,
    pub spec: AgentSpec,
    pub limits: OrdinaryLimits,
    pub binding: OrdinaryBinding,
}

impl TrustedProfileSet {
    /// Validate all trusted inputs before publishing any refs. Reads path
    /// metadata and resolves closed Provider configuration, but never runs an
    /// Agent, probes its HTTP endpoint, writes files or touches a catalog.
    /// Path checks reject aliases visible now, not external filesystem changes.
    pub async fn freeze(agents: Vec<HostStartupAgent>) -> BackendResult<Self> {
        if agents.len() > MAX_TRUSTED_PROFILES {
            return Err(BackendError::Binding("too many trusted profiles"));
        }
        let mut frozen = BTreeMap::new();
        let mut sessions = BTreeSet::new();
        let mut paths = BTreeSet::new();
        #[cfg(unix)]
        let mut inodes = BTreeSet::new();
        for agent in agents {
            agent
                .spec
                .validate()
                .map_err(|_| BackendError::Binding("invalid trusted Agent metadata"))?;
            if agent.limits.max_input_bytes == 0 {
                return Err(BackendError::Binding("input byte budget must be nonzero"));
            }
            if agent.spec.mode != AgentMode::Persistent
                || agent.spec.execution != ExecutionMode::InProcess
            {
                return Err(BackendError::Binding(
                    "trusted profile is not native ordinary",
                ));
            }
            if frozen.contains_key(&agent.spec.id) {
                return Err(BackendError::Binding("duplicate trusted Agent id"));
            }
            let session = agent
                .spec
                .session
                .as_ref()
                .ok_or(BackendError::Binding("trusted profile requires a Session"))?;
            if !sessions.insert(session.id.clone()) {
                return Err(BackendError::Binding("duplicate trusted Session id"));
            }
            let parent = session.path.parent().ok_or(BackendError::Binding(
                "trusted Session parent is unavailable",
            ))?;
            let leaf = session
                .path
                .file_name()
                .ok_or(BackendError::Binding("trusted Session filename is invalid"))?;
            let parent = tokio::fs::canonicalize(parent)
                .await
                .map_err(|_| BackendError::Binding("trusted Session parent is unavailable"))?;
            if !tokio::fs::metadata(&parent)
                .await
                .map_err(|_| BackendError::Binding("trusted Session parent is unavailable"))?
                .is_dir()
            {
                return Err(BackendError::Binding(
                    "trusted Session parent is not a directory",
                ));
            }
            let path = parent.join(leaf);
            if !paths.insert(path.clone()) {
                return Err(BackendError::Binding("duplicate trusted Session path"));
            }
            match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) => {
                    // In particular reject final symlinks, including broken
                    // links; only the existing strict restore may accept facts.
                    if !metadata.is_file() {
                        return Err(BackendError::Binding(
                            "trusted Session is not a regular file",
                        ));
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if !inodes.insert((metadata.dev(), metadata.ino())) {
                            return Err(BackendError::Binding("aliased trusted Session file"));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(BackendError::Binding("trusted Session is unavailable")),
            }
            // No binding/environment is cached: cleanup ownership must always
            // belong to the actual load, never this immutable configuration set.
            drop(agent.profile.bind(agent.spec.clone()).await?);
            let reference = ProfileRef {
                agent_id: agent.spec.id,
                version: ProfileVersion::new(),
            };
            frozen.insert(agent.spec.id, FrozenProfile { reference, agent });
        }
        Ok(Self { agents: frozen })
    }

    pub fn references(&self) -> impl ExactSizeIterator<Item = ProfileRef> + '_ {
        self.agents.values().map(|entry| entry.reference)
    }

    /// Look up the entire opaque ref before binding. Never reinterpret an old
    /// version as the current inputs or resolve an arbitrary peer-supplied spec.
    pub async fn prepare(&self, reference: ProfileRef) -> BackendResult<PreparedProfile> {
        let entry = self
            .agents
            .get(&reference.agent_id)
            .filter(|entry| entry.reference == reference)
            .ok_or(BackendError::Binding("profile reference is not permitted"))?;
        let binding = entry.agent.profile.bind(entry.agent.spec.clone()).await?;
        Ok(PreparedProfile {
            reference,
            spec: entry.agent.spec.clone(),
            limits: entry.agent.limits,
            binding,
        })
    }
}

#[cfg(all(test, unix))]
#[path = "profile_set_tests.rs"]
mod tests;
