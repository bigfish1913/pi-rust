//! Bounded asynchronous access to one blocking SQLite/ownership worker.
//!
//! Successful mutation responses follow the SQLite commit. Dropping a waiting
//! client future does not undo a queued command: inspect by stable Agent ID and
//! use epoch/revision guards before retrying. This is not a prompt submission,
//! durable input acknowledgement, or exactly-once tool execution API.

use super::lifecycle::{LifecycleAdmission, LifecycleReceipt, LifecycleRequest, LifecycleState};
use super::recovery::Bindings;
#[cfg(unix)]
use super::recovery::Membership;
use super::{
    registry::Registry, AgentId, AgentMode, AgentSpec, AgentState, CatalogError, CatalogResult,
    ControlScopeId, EffectiveTimeouts, HostConfig, HostEpoch, RequestId, TimeoutOverrides,
};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

pub const MAX_PAGE_SIZE: usize = 128;
const COMMAND_CAPACITY: usize = 64;
pub(crate) const MAX_RECORD_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogGuard {
    pub epoch: HostEpoch,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    pub spec: AgentSpec,
    pub state: AgentState,
    pub revision: u64,
    /// Retained when a new instance classifies old state; PID is not identity.
    pub last_epoch: HostEpoch,
}

impl AgentRecord {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        self.spec.validate()?;
        self.state.validate_for(self.spec.mode)?;
        if self.revision == 0 || self.revision > i64::MAX as u64 {
            return Err(CatalogError::InvalidRecord("revision outside stored range"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentPage {
    pub agents: Vec<AgentRecord>,
    /// Keyset pagination, not a snapshot across concurrently changing pages.
    pub next_cursor: Option<AgentId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogStatus {
    pub epoch: HostEpoch,
    pub registered: usize,
    pub resident: usize,
    /// Records reporting Running/WaitingApproval, not a model/tool barrier or
    /// proof that already-closing external work has stopped.
    pub executing: usize,
    /// Includes startup and cleanup; the reaper must not treat these as idle.
    pub busy: usize,
    pub mini: usize,
    pub max_agents: Option<usize>,
}

type Reply<T> = oneshot::Sender<CatalogResult<T>>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionSource {
    Explicit,
    IdleUnload,
    AttachRestore,
}

enum Command {
    Register {
        spec: Box<AgentSpec>,
        reply: Reply<AgentRecord>,
    },
    Get {
        id: AgentId,
        reply: Reply<AgentRecord>,
    },
    List {
        after: Option<AgentId>,
        limit: usize,
        reply: Reply<AgentPage>,
    },
    Transition {
        id: AgentId,
        guard: CatalogGuard,
        next: AgentState,
        source: TransitionSource,
        reply: Reply<AgentRecord>,
    },
    Status {
        reply: Reply<CatalogStatus>,
    },
    Limit {
        limit: Option<NonZeroUsize>,
        reply: Reply<()>,
    },
    PrepareLifecycle {
        request: Box<LifecycleRequest>,
        reply: Reply<LifecycleAdmission>,
    },
    FinishLifecycle {
        result: Box<(LifecycleRequest, LifecycleState)>,
        reply: Reply<LifecycleReceipt>,
    },
    LifecycleReceipt {
        scope: ControlScopeId,
        request_id: RequestId,
        reply: Reply<Option<LifecycleReceipt>>,
    },
    #[cfg(unix)]
    RecoveryMembers {
        configured: Vec<AgentId>,
        reply: Reply<Membership>,
    },
    #[cfg(unix)]
    RememberMembers {
        loaded: Vec<AgentId>,
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

/// Administrative state only. Does not create Agent backends or stop external
/// tools. The eventual service owner must drain actual backends before shutdown.
#[derive(Clone)]
pub struct HostCatalog {
    sender: mpsc::Sender<Command>,
    epoch: HostEpoch,
    config: Arc<HostConfig>,
}

impl HostCatalog {
    pub async fn open(directory: PathBuf, config: HostConfig) -> CatalogResult<Self> {
        Self::open_with_recovery(directory, config, Bindings::new()).await
    }

    pub(super) async fn open_with_recovery(
        directory: PathBuf,
        config: HostConfig,
        bindings: Bindings,
    ) -> CatalogResult<Self> {
        config.validate()?;
        if !directory.is_absolute() {
            return Err(
                super::HostError::InvalidInput("registry directory must be absolute").into(),
            );
        }
        let policy = Arc::new(config.clone());
        let (sender, mut receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new()
            .name("rpi-host-registry".into())
            .spawn(move || {
                let mut registry = match Registry::open(&directory, config, bindings) {
                    Ok(registry) => registry,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(registry.epoch())).is_err() {
                    return;
                }
                let mut shutdown = None;
                while let Some(command) = receiver.blocking_recv() {
                    match command {
                        Command::Register { spec, reply } => {
                            let _ = reply.send(registry.register(*spec));
                        }
                        Command::Get { id, reply } => {
                            let _ = reply.send(registry.get(id));
                        }
                        Command::List {
                            after,
                            limit,
                            reply,
                        } => {
                            let _ = reply.send(registry.list(after, limit));
                        }
                        Command::Transition {
                            id,
                            guard,
                            next,
                            source,
                            reply,
                        } => {
                            let _ = reply.send(registry.transition(id, guard, next, source));
                        }
                        Command::Status { reply } => {
                            let _ = reply.send(registry.status());
                        }
                        Command::Limit { limit, reply } => {
                            let _ = reply.send(registry.set_limit(limit));
                        }
                        Command::PrepareLifecycle { request, reply } => {
                            let _ = reply.send(registry.prepare_lifecycle(*request));
                        }
                        Command::FinishLifecycle { result, reply } => {
                            let (request, status) = *result;
                            let _ = reply.send(registry.finish_lifecycle(request, status));
                        }
                        Command::LifecycleReceipt {
                            scope,
                            request_id,
                            reply,
                        } => {
                            let _ = reply.send(registry.lifecycle_receipt(scope, request_id));
                        }
                        #[cfg(unix)]
                        Command::RecoveryMembers { configured, reply } => {
                            let _ = reply.send(registry.recovery_members(&configured));
                        }
                        #[cfg(unix)]
                        Command::RememberMembers { loaded, reply } => {
                            let _ = reply.send(registry.remember_members(&loaded));
                        }
                        Command::Shutdown { reply } => {
                            shutdown = Some(reply);
                            break;
                        }
                    }
                }
                // Release SQLite, reservations and inherited lock references
                // before acknowledging; unlock errors cannot become a success ACK.
                let cleanup = registry.shutdown();
                if let Some(reply) = shutdown {
                    let _ = reply.send(cleanup);
                }
            })?;
        let epoch = started.await.map_err(|_| CatalogError::WorkerStopped)??;
        Ok(Self {
            sender,
            epoch,
            config: policy,
        })
    }

    pub(super) fn resolve_timeouts(
        &self,
        overrides: TimeoutOverrides,
    ) -> CatalogResult<EffectiveTimeouts> {
        Ok(self.config.resolve_timeouts(overrides)?)
    }

    pub(super) fn idle_timeout(&self) -> Option<std::time::Duration> {
        self.config
            .host_idle_timeout_ms
            .map(|ms| std::time::Duration::from_millis(ms.get()))
    }

    pub fn epoch(&self) -> HostEpoch {
        self.epoch
    }

    pub fn guard(&self, record: &AgentRecord) -> CatalogGuard {
        CatalogGuard {
            epoch: self.epoch,
            revision: record.revision,
        }
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> CatalogResult<T> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(make(reply))
            .await
            .map_err(|_| CatalogError::WorkerStopped)?;
        result.await.map_err(|_| CatalogError::WorkerStopped)?
    }

    pub async fn register(&self, spec: AgentSpec) -> CatalogResult<AgentRecord> {
        spec.validate()?;
        // Bound queued metadata before moving it to the worker; leave record header room.
        if serde_json::to_vec(&spec)?.len() > MAX_RECORD_BYTES - 512 {
            return Err(
                super::HostError::InvalidInput("Agent metadata exceeds byte budget").into(),
            );
        }
        self.call(|reply| Command::Register {
            spec: Box::new(spec),
            reply,
        })
        .await
    }

    pub async fn get(&self, id: AgentId) -> CatalogResult<AgentRecord> {
        self.call(|reply| Command::Get { id, reply }).await
    }

    pub async fn list(&self, after: Option<AgentId>, limit: usize) -> CatalogResult<AgentPage> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(super::HostError::InvalidInput("invalid catalog page size").into());
        }
        self.call(|reply| Command::List {
            after,
            limit,
            reply,
        })
        .await
    }

    pub async fn transition(
        &self,
        id: AgentId,
        guard: CatalogGuard,
        next: AgentState,
    ) -> CatalogResult<AgentRecord> {
        self.call(|reply| Command::Transition {
            id,
            guard,
            next,
            source: TransitionSource::Explicit,
            reply,
        })
        .await
    }

    pub(super) async fn begin_idle_unload(
        &self,
        id: AgentId,
        guard: CatalogGuard,
    ) -> CatalogResult<AgentRecord> {
        self.call(|reply| Command::Transition {
            id,
            guard,
            next: AgentState::Closing,
            source: TransitionSource::IdleUnload,
            reply,
        })
        .await
    }

    pub(super) async fn begin_attach_restore(
        &self,
        id: AgentId,
        guard: CatalogGuard,
    ) -> CatalogResult<AgentRecord> {
        self.call(|reply| Command::Transition {
            id,
            guard,
            next: AgentState::Starting,
            source: TransitionSource::AttachRestore,
            reply,
        })
        .await
    }

    pub async fn status(&self) -> CatalogResult<CatalogStatus> {
        self.call(|reply| Command::Status { reply }).await
    }

    /// Runtime policy only; the caller remains responsible for saving Host config.
    pub async fn set_max_agents(&self, limit: Option<NonZeroUsize>) -> CatalogResult<()> {
        self.call(|reply| Command::Limit { limit, reply }).await
    }

    /// Commit administrative custody only. The trusted coordinator must own
    /// prepare+action+finish independently of a client waiter, and may act ONLY
    /// when `fresh` is true. This API executes nothing and grants no permission.
    pub async fn prepare_lifecycle(
        &self,
        request: LifecycleRequest,
    ) -> CatalogResult<LifecycleAdmission> {
        self.call(|reply| Command::PrepareLifecycle {
            request: Box::new(request),
            reply,
        })
        .await
    }

    /// Trusted coordinator publication, never a client-supplied result. A final
    /// state is immutable; errors require reconciliation rather than replay.
    pub async fn finish_lifecycle(
        &self,
        request: LifecycleRequest,
        status: LifecycleState,
    ) -> CatalogResult<LifecycleReceipt> {
        self.call(|reply| Command::FinishLifecycle {
            result: Box::new((request, status)),
            reply,
        })
        .await
    }

    /// No Agent load, action replay, or ownership transfer. Found records pass
    /// a strict SQLite checkpoint before they are returned as journal evidence.
    pub async fn lifecycle_receipt(
        &self,
        scope: ControlScopeId,
        request_id: RequestId,
    ) -> CatalogResult<Option<LifecycleReceipt>> {
        self.call(|reply| Command::LifecycleReceipt {
            scope,
            request_id,
            reply,
        })
        .await
    }

    #[cfg(unix)]
    pub(super) async fn recovery_members(
        &self,
        configured: Vec<AgentId>,
    ) -> CatalogResult<Membership> {
        self.call(|reply| Command::RecoveryMembers { configured, reply })
            .await
    }

    #[cfg(unix)]
    pub(super) async fn remember_members(&self, loaded: Vec<AgentId>) -> CatalogResult<()> {
        self.call(|reply| Command::RememberMembers { loaded, reply })
            .await
    }

    /// Waits for storage/lock cleanup, not for Agent execution cleanup.
    pub async fn shutdown(&self) -> CatalogResult<()> {
        self.call(|reply| Command::Shutdown { reply }).await
    }
}

pub(crate) fn is_mini(record: &AgentRecord) -> bool {
    record.spec.mode == AgentMode::Mini
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandoned_lifecycle_reply_does_not_transfer_a_committed_fresh_grant() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = HostCatalog::open(directory.path().join("host"), HostConfig::default())
            .await
            .unwrap();
        let request = LifecycleRequest {
            scope: ControlScopeId::new(),
            request_id: RequestId::new(),
            epoch: catalog.epoch(),
            action: crate::host::lifecycle::LifecycleAction::Create {
                agent_id: AgentId::new(),
            },
        };
        let (reply, result) = oneshot::channel();
        drop(result);
        catalog
            .sender
            .send(Command::PrepareLifecycle {
                request: Box::new(request.clone()),
                reply,
            })
            .await
            .unwrap();
        // FIFO observation after the abandoned queued command, not a timing race.
        let receipt = catalog
            .lifecycle_receipt(request.scope, request.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.status, LifecycleState::Prepared {});
        let duplicate = catalog.prepare_lifecycle(request).await.unwrap();
        assert!(!duplicate.fresh);
        assert_eq!(duplicate.receipt, receipt);
        assert_eq!(catalog.status().await.unwrap().registered, 0);
        catalog.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn abandoned_client_reply_does_not_undo_an_accepted_catalog_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = HostCatalog::open(directory.path().join("host"), HostConfig::default())
            .await
            .unwrap();
        let spec = crate::host::registry_tests::make_spec(AgentMode::Mini);
        let id = spec.id;
        let (reply, result) = oneshot::channel();
        drop(result);
        catalog
            .sender
            .send(Command::Register {
                spec: Box::new(spec),
                reply,
            })
            .await
            .unwrap();
        // FIFO fence after the abandoned command; no timing-dependent abort race.
        let record = catalog.get(id).await.unwrap();
        assert_eq!(record.state, AgentState::Registered);
        assert_eq!(record.revision, 1);
        assert!(matches!(
            catalog.register(record.spec).await,
            Err(CatalogError::AlreadyRegistered)
        ));
        catalog.shutdown().await.unwrap();
    }
}
