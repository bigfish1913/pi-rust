//! Bounded asynchronous access to one blocking SQLite/ownership worker.
//!
//! Successful mutation responses follow the SQLite commit. Dropping a waiting
//! client future does not undo a queued command: inspect by stable Agent ID and
//! use epoch/revision guards before retrying. This is not a prompt submission,
//! durable input acknowledgement, or exactly-once tool execution API.

use super::{
    registry::Registry, AgentId, AgentMode, AgentSpec, AgentState, CatalogError, CatalogResult,
    HostConfig, HostEpoch,
};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroUsize, path::PathBuf};
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

pub(crate) enum Command {
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
        reply: Reply<AgentRecord>,
    },
    Status {
        reply: Reply<CatalogStatus>,
    },
    Limit {
        limit: Option<NonZeroUsize>,
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
}

impl HostCatalog {
    pub async fn open(directory: PathBuf, config: HostConfig) -> CatalogResult<Self> {
        config.validate()?;
        if !directory.is_absolute() {
            return Err(
                super::HostError::InvalidInput("registry directory must be absolute").into(),
            );
        }
        let (sender, mut receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new()
            .name("rpi-host-registry".into())
            .spawn(move || {
                let mut registry = match Registry::open(&directory, config) {
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
                            reply,
                        } => {
                            let _ = reply.send(registry.transition(id, guard, next));
                        }
                        Command::Status { reply } => {
                            let _ = reply.send(registry.status());
                        }
                        Command::Limit { limit, reply } => {
                            let _ = reply.send(registry.set_limit(limit));
                        }
                        Command::Shutdown { reply } => {
                            shutdown = Some(reply);
                            break;
                        }
                    }
                }
                // Release SQLite and the instance lock before acknowledging shutdown.
                drop(registry);
                if let Some(reply) = shutdown {
                    let _ = reply.send(Ok(()));
                }
            })?;
        let epoch = started.await.map_err(|_| CatalogError::WorkerStopped)??;
        Ok(Self { sender, epoch })
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
