//! Authenticated, bounded Unix control endpoint. Capabilities are implementation
//! promises, not an enumeration of the eventual protocol. Native prompt
//! admission/receipts are supported. Administrative execution only accepts
//! complete profile, guarded Close/CancelSubmitted and Stop intents through the
//! daemon's fixed native authority and durable coordinator; legacy ops stay off.

use super::{
    client::{encode_frame, read_frame, write_frame},
    daemon::DaemonAdministration,
    protocol::*,
    service::HostService,
    CatalogError,
};
use std::{path::PathBuf, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ControlConfig {
    pub socket_path: PathBuf,
    pub host_id: HostId,
    pub token: AuthToken,
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    /// Total idle/read + dispatch + write budget per query, not per byte.
    pub request_timeout: Duration,
}
impl ControlConfig {
    pub fn validate(&self) -> Result<(), ServerError> {
        if !self.socket_path.is_absolute()
            || !(1..=64).contains(&self.max_connections)
            || [self.handshake_timeout, self.request_timeout]
                .iter()
                .any(|duration| duration.is_zero() || *duration > Duration::from_secs(300))
        {
            return Err(ServerError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Fixed diagnostics never contain wire input, tokens, paths or OS errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServerError {
    #[error("invalid shared Host control configuration")]
    InvalidConfiguration,
    #[error("shared Host endpoint must be absent, with a private owned parent")]
    UnsafeEndpoint,
    #[error("shared Host endpoint could not be bound")]
    Bind,
    #[error("shared Host control listener stopped unexpectedly")]
    ListenerStopped,
    #[error("shared Host control task stopped unexpectedly")]
    TaskStopped,
}

/// Remove only the socket inode this listener created. Never remove a newer
/// listener, replacement file or stale socket belonging to somebody else.
struct SocketLease {
    path: PathBuf,
    dev: u64,
    ino: u64,
}
impl Drop for SocketLease {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.dev
                && metadata.ino() == self.ino
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
fn bind_endpoint(
    path: PathBuf,
) -> Result<(std::os::unix::net::UnixListener, SocketLease), ServerError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let parent = path.parent().ok_or(ServerError::InvalidConfiguration)?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|_| ServerError::UnsafeEndpoint)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(ServerError::UnsafeEndpoint);
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err(ServerError::UnsafeEndpoint),
    }
    let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|_| ServerError::Bind)?;
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| ServerError::Bind)?;
    let lease = SocketLease {
        path: path.clone(),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    if !metadata.file_type().is_socket()
        || metadata.nlink() != 1
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
    {
        return Err(ServerError::UnsafeEndpoint);
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ServerError::Bind)?;
    listener
        .set_nonblocking(true)
        .map_err(|_| ServerError::Bind)?;
    Ok((listener, lease))
}

/// Explicit stop waits for all connection tasks and inode cleanup. Drop merely
/// requests that cleanup; it never closes Agents or shuts down the catalog.
pub struct ControlServer {
    welcome: Welcome,
    stop: CancellationToken,
    // true only when this listener observed the current owner's completed Stop.
    task: Option<JoinHandle<Result<bool, ServerError>>>,
    terminal: Option<Result<bool, ServerError>>,
}
impl ControlServer {
    pub async fn bind(config: ControlConfig, service: HostService) -> Result<Self, ServerError> {
        Self::bind_with_administration(config, service, None).await
    }
    pub(crate) async fn bind_with_administration(
        config: ControlConfig,
        service: HostService,
        administration: Option<DaemonAdministration>,
    ) -> Result<Self, ServerError> {
        config.validate()?;
        let path = config.socket_path.clone();
        // An abandoned spawn_blocking result still drops its socket lease.
        let (listener, lease) = tokio::task::spawn_blocking(move || bind_endpoint(path))
            .await
            .map_err(|_| ServerError::TaskStopped)??;
        let listener = UnixListener::from_std(listener).map_err(|_| ServerError::Bind)?;
        let mut welcome = Welcome {
            protocol: PROTOCOL.into(),
            version: Version::default(),
            host_id: config.host_id,
            epoch: String::from(service.catalog().epoch())
                .try_into()
                .map_err(|_| ServerError::InvalidConfiguration)?,
            capabilities: vec![
                Capability::Status,
                Capability::List,
                Capability::Inspect,
                Capability::Receipts,
                Capability::Results,
                Capability::Submit,
            ],
            limits: Limits {
                max_in_flight: 1,
                ..Limits::default()
            },
        };
        if administration.is_some() {
            welcome.capabilities.extend([
                Capability::Profiles,
                Capability::AdministrativeReceipts,
                Capability::AdministrativeCreate,
                Capability::AdministrativeRestore,
                Capability::AdministrativeClose,
                Capability::AdministrativeCancel,
                Capability::AdministrativeStop,
            ]);
        }
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(
            listener,
            lease,
            config,
            service,
            administration,
            welcome.clone(),
            stop.clone(),
        ));
        Ok(Self {
            welcome,
            stop,
            task: Some(task),
            terminal: None,
        })
    }
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }
    pub fn stopped_by_administration(&self) -> bool {
        matches!(self.terminal, Some(Ok(true)))
    }
    /// Cancellation-safe: abandoning this wait does not discard the task handle.
    pub async fn wait(&mut self) -> Result<(), ServerError> {
        if let Some(result) = self.terminal {
            return result.map(|_| ());
        }
        let result = match self.task.as_mut() {
            Some(task) => task.await.unwrap_or(Err(ServerError::TaskStopped)),
            None => Ok(false),
        };
        self.task.take();
        self.terminal = Some(result);
        result.map(|_| ())
    }
    pub async fn stop(&mut self) -> Result<(), ServerError> {
        self.stop.cancel();
        self.wait().await
    }
}
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn serve(
    listener: UnixListener,
    lease: SocketLease,
    config: ControlConfig,
    service: HostService,
    administration: Option<DaemonAdministration>,
    welcome: Welcome,
    stop: CancellationToken,
) -> Result<bool, ServerError> {
    let native_stop = administration
        .as_ref()
        .map(DaemonAdministration::stop_completed)
        .unwrap_or_default();
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = native_stop.cancelled() => break Ok(true),
            _ = stop.cancelled() => break Ok(false),
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(pair) => pair, Err(_) => break Err(ServerError::ListenerStopped) };
                // No task, frame allocation, or auth work for excess connections.
                if connections.len() >= config.max_connections { drop(stream); continue; }
                connections.spawn(connection(stream, config.clone(), service.clone(), administration.clone(), welcome.clone(), native_stop.clone()));
            }
        }
    };
    // A completed native Stop closes admission, not the response waiters.
    // Idle readers/handshakes observe native_stop; already-dispatched exchanges
    // may flush within their ORIGINAL total deadline. No new grace per byte,
    // waiter or connection, and no dependency on a peer reading the reply.
    if result != Ok(true) {
        // External stop/listener failure may abort waits, never owned jobs.
        connections.abort_all();
    }
    while connections.join_next().await.is_some() {}
    // Keep the listener FD alive through the inode check/unlink, so its inode
    // cannot be recycled for a replacement listener before our cleanup check.
    drop(lease);
    drop(listener);
    result
}

async fn connection(
    mut stream: UnixStream,
    config: ControlConfig,
    service: HostService,
    administration: Option<DaemonAdministration>,
    welcome: Welcome,
    native_stop: CancellationToken,
) {
    if stream.peer_cred().map(|peer| peer.uid()).ok()
        != Some(nix::unistd::Uid::effective().as_raw())
    {
        return;
    }
    let handshake = async {
        let hello: Hello = read_frame(&mut stream, MAX_FRAME_BYTES).await?;
        if !config.token.matches(&hello.token)
            || hello.host_id != config.host_id
            || hello.protocol != PROTOCOL
            || hello.version.major != MAJOR
        {
            return Err(super::client::Failure::IncompatiblePeer);
        }
        write_frame(&mut stream, &encode_frame(&welcome, MAX_FRAME_BYTES)?).await
    };
    let handshake_result = tokio::select! {
        biased;
        _ = native_stop.cancelled() => return,
        result = tokio::time::timeout(config.handshake_timeout, handshake) => result,
    };
    if !matches!(handshake_result, Ok(Ok(()))) {
        return;
    }
    loop {
        let exchange = async {
            let request: Request = tokio::select! {
                biased;
                _ = native_stop.cancelled() => return Err(super::client::Failure::Connection),
                result = read_frame(&mut stream, welcome.limits.max_frame_bytes) => result?,
            };
            // Once dispatch starts it retains its bounded reply opportunity,
            // including when the owned Stop completes before observe wakes.
            let reply = if request.epoch != welcome.epoch {
                Reply::Error {
                    code: ErrorCode::StaleEpoch,
                }
            } else if !request.command.valid(welcome.limits) {
                Reply::Error {
                    code: ErrorCode::InvalidRequest,
                }
            } else if !welcome.supports(request.command.capability()) {
                Reply::Error {
                    code: ErrorCode::Unsupported,
                }
            } else {
                dispatch(
                    &service,
                    administration.as_ref(),
                    config.host_id,
                    request.command,
                )
                .await
            };
            let response = Response {
                call_id: request.call_id,
                epoch: welcome.epoch,
                reply,
            };
            let bytes = encode_frame(&response, welcome.limits.max_frame_bytes)?;
            write_frame(&mut stream, &bytes).await
        };
        if !matches!(
            tokio::time::timeout(config.request_timeout, exchange).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}
fn catalog_code(error: &CatalogError) -> ErrorCode {
    match error {
        CatalogError::UnknownAgent => ErrorCode::NotFound,
        CatalogError::WorkerStopped => ErrorCode::Busy,
        _ => ErrorCode::Internal,
    }
}
fn catalog_error(error: CatalogError) -> Reply {
    Reply::Error {
        code: catalog_code(&error),
    }
}
fn service_error(error: super::service::ServiceError) -> Reply {
    use super::ordinary::BackendError;
    use super::service::ServiceError;
    Reply::Error {
        code: match error {
            ServiceError::Stopping | ServiceError::Busy => ErrorCode::Busy,
            ServiceError::NotLoaded => ErrorCode::NotFound,
            ServiceError::Backend(BackendError::AdmissionUncertain) => {
                ErrorCode::PersistenceUncertain
            }
            ServiceError::AlreadyOwned => ErrorCode::Conflict,
            ServiceError::Backend(_) | ServiceError::TaskStopped => ErrorCode::Internal,
            ServiceError::Catalog(error) => catalog_code(&error),
            ServiceError::ShutdownIncomplete { .. } => ErrorCode::Internal,
        },
    }
}
fn backend_code(error: &super::ordinary::BackendError) -> ErrorCode {
    use super::ordinary::BackendError;
    match error {
        BackendError::AdmissionUncertain | BackendError::Unsaved => ErrorCode::PersistenceUncertain,
        BackendError::Admission(_) | BackendError::InputTooLarge => ErrorCode::InvalidRequest,
        BackendError::Busy => ErrorCode::Busy,
        BackendError::Closed => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    }
}
async fn dispatch(
    service: &HostService,
    administration: Option<&DaemonAdministration>,
    host_id: HostId,
    command: Command,
) -> Reply {
    if matches!(
        command,
        Command::Profiles { .. }
            | Command::AdministrativeReceipt { .. }
            | Command::AdministrativeCreate { .. }
            | Command::AdministrativeRestore { .. }
            | Command::AdministrativeClose { .. }
            | Command::AdministrativeCancel { .. }
            | Command::AdministrativeStop { .. }
    ) {
        return administrative::dispatch(administration, command).await;
    }
    let result = match command {
        Command::Status {} => service.catalog().status().await.map(|status| serde_json::json!({
            "epoch": status.epoch, "registered": status.registered, "resident": status.resident,
            "executing": status.executing, "busy": status.busy, "mini": status.mini,
            "max_agents": status.max_agents, "owners": service.owner_count(), "stopping": service.is_stopping(),
        })),
        Command::Inspect { agent_id } => {
            let Ok(id) = String::from(agent_id).try_into() else { return Reply::Error { code: ErrorCode::InvalidRequest }; };
            service.catalog().get(id).await.and_then(|record| serde_json::to_value(record).map_err(CatalogError::from))
        }
        Command::Result { agent_id, request_id, run_id, offset, max_bytes } => {
            return submission_result::query(service, host_id, agent_id, request_id, run_id, offset, max_bytes).await;
        }
        Command::Receipt {
            agent_id,
            request_id,
        } => {
            let Ok(id) = String::from(agent_id).try_into() else {
                return Reply::Error {
                    code: ErrorCode::InvalidRequest,
                };
            };
            // A receipt query is a read of one loaded Agent's native journal.
            // It never loads, replays or resumes anything.
            return match service.admission(id, &String::from(request_id)).await {
                Ok(Some(status)) => match serde_json::to_value(serde_json::json!({
                    "found": true,
                    "agent_id": agent_id,
                    "request_id": request_id,
                    "receipt": status.receipt,
                    "execution": status.execution,
                })) {
                    Ok(data) => Reply::Data { data },
                    Err(_) => Reply::Error {
                        code: ErrorCode::Internal,
                    },
                },
                Ok(None) => Reply::Data {
                    data: serde_json::json!({
                        "found": false,
                        "agent_id": agent_id,
                        "request_id": request_id,
                    }),
                },
                Err(error) => service_error(error),
            };
        }
        Command::Submit {
            agent_id,
            request_id,
            prompt,
        } => {
            let Ok(id) = String::from(agent_id).try_into() else {
                return Reply::Error {
                    code: ErrorCode::InvalidRequest,
                };
            };
            let client = match service.ordinary_client(id) {
                Ok(client) => client,
                Err(error) => return service_error(error),
            };
            // Stable message identity: the wire submit is plain text, so a
            // retry of the same request id must serialize identically. Ordering
            // comes from entry sequences, not a wall-clock message stamp.
            let messages = vec![rpi_ai::UserMessage::new(prompt, 0).into()];
            let ticket = match client
                .submit_admitted(
                    String::from(host_id),
                    String::from(request_id),
                    messages,
                )
                .await
            {
                Ok(ticket) => ticket,
                Err(error) => {
                    return Reply::Error {
                        code: backend_code(&error),
                    }
                }
            };
            // Only the durable admission is awaited here. Execution continues in
            // the supervised backend after this reply, so the reply is not a run
            // result and not an exactly-once tool side-effect proof.
            return match ticket.wait_admitted().await {
                Ok(receipt) => Reply::Data {
                    data: serde_json::json!({
                        "durable": true,
                        "agent_id": agent_id,
                        "request_id": request_id,
                        "receipt": receipt,
                    }),
                },
                Err(error) => Reply::Error {
                    code: backend_code(&error),
                },
            };
        }
        Command::List { after, limit } => {
            let after = match after.map(|id| String::from(id).try_into()).transpose() { Ok(value) => value, Err(_) => return Reply::Error { code: ErrorCode::InvalidRequest } };
            service.catalog().list(after, limit).await.map(|page| {
                // A list is a bounded summary, never 128 full launch records.
                let agents: Vec<_> = page.agents.into_iter().map(|record| serde_json::json!({
                    "id": record.spec.id, "name": record.spec.name, "parent_id": record.spec.parent_id,
                    "mode": record.spec.mode, "state": record.state, "revision": record.revision,
                })).collect();
                serde_json::json!({"agents": agents, "next_cursor": page.next_cursor})
            })
        }
        _ => return Reply::Error { code: ErrorCode::Unsupported },
    };
    match result {
        Ok(data) => Reply::Data { data },
        Err(error) => catalog_error(error),
    }
}

#[path = "submission_result.rs"]
mod submission_result;

#[path = "server_administration.rs"]
mod administrative;

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
