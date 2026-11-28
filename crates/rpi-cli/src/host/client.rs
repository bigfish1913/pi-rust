//! Explicit, bounded Unix IPC client for shared Host control. No discovery,
//! process spawn, transcript access, retry, background reader or Agent ownership.
//! A dropped exchange closes its connection: it does not undo remote work.

use super::protocol::*;
#[cfg(unix)]
use serde::de::DeserializeOwned;
#[cfg(unix)]
use serde::Serialize;
use std::{fmt, path::PathBuf, time::Duration};
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone)]
pub struct ConnectionConfig {
    pub socket_path: PathBuf,
    pub host_id: HostId,
    pub token: AuthToken,
    /// Total connect + filesystem preflight + authentication budget; also the
    /// total budget of each subsequent exchange, never reset per frame byte.
    pub timeout: Duration,
}
impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("host_id", &self.host_id)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    UnsupportedPlatform,
    InvalidConfiguration,
    UnsafeEndpoint,
    Connection,
    Timeout,
    Closed,
    FrameTooLarge,
    MalformedFrame,
    IncompatiblePeer,
    UnsupportedCapability,
    InvalidCommand,
    Correlation,
    EpochChanged,
}

/// These errors intentionally omit raw peer data, paths, tokens and OS error
/// Display strings. A mutation's uncertain outcome is NOT a retry instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    NotSent(Failure),
    QueryFailed(Failure),
    OutcomeUnknown {
        request_id: RequestId,
        reason: Failure,
    },
    Rejected(ErrorCode),
}
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(reason) => write!(f, "shared Host request not sent: {reason:?}"),
            Self::QueryFailed(reason) => write!(f, "shared Host query failed: {reason:?}"),
            Self::OutcomeUnknown { request_id, reason } => write!(f,
                "shared Host outcome unknown for request {request_id}: {reason:?}; query its receipt before retrying"),
            Self::Rejected(code) => write!(f, "shared Host rejected request: {code:?}"),
        }
    }
}
impl std::error::Error for ClientError {}

/// One serialized command exchange at a time. Creating another client is an
/// explicit connection, not another Agent. Drop/detach never sends stop/close.
pub struct HostClient {
    #[cfg(unix)]
    stream: Option<tokio::net::UnixStream>,
    welcome: Welcome,
    #[cfg(unix)]
    timeout: Duration,
}
impl fmt::Debug for HostClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostClient")
            .field("host_id", &self.welcome.host_id)
            .field("epoch", &self.welcome.epoch)
            .finish_non_exhaustive()
    }
}
impl HostClient {
    #[cfg(unix)]
    pub async fn connect(config: ConnectionConfig) -> Result<Self, ClientError> {
        if config.timeout.is_zero()
            || config.timeout > Duration::from_secs(300)
            || !config.socket_path.is_absolute()
        {
            return Err(ClientError::NotSent(Failure::InvalidConfiguration));
        }
        let timeout = config.timeout;
        let connect = async {
            check_endpoint(&config.socket_path).await?;
            let mut stream = tokio::net::UnixStream::connect(&config.socket_path)
                .await
                .map_err(|_| Failure::Connection)?;
            // Recheck the actual connection, not just filesystem metadata. The
            // parent chain is trusted; this does not isolate hostile same-UID code.
            let peer = stream.peer_cred().map_err(|_| Failure::UnsafeEndpoint)?;
            if peer.uid() != nix::unistd::Uid::effective().as_raw() {
                return Err(Failure::UnsafeEndpoint);
            }
            let hello = Hello {
                protocol: PROTOCOL.into(),
                version: Version::default(),
                host_id: config.host_id,
                token: config.token,
            };
            let bytes = encode_frame(&hello, MAX_FRAME_BYTES)?;
            write_frame(&mut stream, &bytes).await?;
            let welcome: Welcome = read_frame(&mut stream, MAX_FRAME_BYTES).await?;
            if !welcome.compatible(config.host_id) {
                return Err(Failure::IncompatiblePeer);
            }
            Ok(Self {
                stream: Some(stream),
                welcome,
                timeout,
            })
        };
        tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| ClientError::NotSent(Failure::Timeout))?
            .map_err(ClientError::NotSent)
    }

    #[cfg(not(unix))]
    pub async fn connect(_config: ConnectionConfig) -> Result<Self, ClientError> {
        Err(ClientError::NotSent(Failure::UnsupportedPlatform))
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    #[cfg(unix)]
    pub async fn call(&mut self, command: Command) -> Result<serde_json::Value, ClientError> {
        if self.stream.is_none() {
            return Err(ClientError::NotSent(Failure::Closed));
        }
        if !self.welcome.supports(command.capability()) {
            return Err(ClientError::NotSent(Failure::UnsupportedCapability));
        }
        if !command.valid(self.welcome.limits) {
            return Err(ClientError::NotSent(Failure::InvalidCommand));
        }
        let request_id = command.request_id();
        let request = Request {
            call_id: CallId::new(),
            epoch: self.welcome.epoch,
            command,
        };
        let limit = self.welcome.limits.max_frame_bytes;
        let bytes = encode_frame(&request, limit).map_err(ClientError::NotSent)?;
        // Move ownership OUT before awaiting. Cancellation midway through either
        // framing direction drops this socket; no later call can read its reply
        // as a new request, or append to an incomplete frame.
        let mut stream = self.stream.take().expect("checked connection");
        let exchange = async {
            write_frame(&mut stream, &bytes).await?;
            let response: Response = read_frame(&mut stream, limit).await?;
            if response.call_id != request.call_id {
                return Err(Failure::Correlation);
            }
            if response.epoch != request.epoch {
                return Err(Failure::EpochChanged);
            }
            Ok(response.reply)
        };
        let result = tokio::time::timeout(self.timeout, exchange)
            .await
            .unwrap_or(Err(Failure::Timeout));
        match result {
            Ok(reply) => {
                self.stream = Some(stream);
                match reply {
                    Reply::Data { data } => Ok(data),
                    Reply::Error { code } => Err(ClientError::Rejected(code)),
                }
            }
            Err(reason) => Err(match request_id {
                Some(request_id) => ClientError::OutcomeUnknown { request_id, reason },
                None => ClientError::QueryFailed(reason),
            }),
        }
    }

    #[cfg(not(unix))]
    pub async fn call(&mut self, _command: Command) -> Result<serde_json::Value, ClientError> {
        Err(ClientError::NotSent(Failure::UnsupportedPlatform))
    }

    pub fn detach(&mut self) {
        #[cfg(unix)]
        {
            self.stream.take();
        }
    }
}

#[cfg(unix)]
async fn check_endpoint(path: &std::path::Path) -> Result<(), Failure> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let uid = nix::unistd::Uid::effective().as_raw();
    let parent = path.parent().ok_or(Failure::InvalidConfiguration)?;
    let directory = tokio::fs::symlink_metadata(parent)
        .await
        .map_err(|_| Failure::UnsafeEndpoint)?;
    let socket = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| Failure::UnsafeEndpoint)?;
    if !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o077 != 0
        || !socket.file_type().is_socket()
        || socket.uid() != uid
        || socket.mode() & 0o077 != 0
    {
        return Err(Failure::UnsafeEndpoint);
    }
    Ok(())
}

/// Encoding is bounded; callers must still bound application input before
/// constructing a large in-memory value. These helpers never log wire data.
#[cfg(unix)]
pub(crate) fn encode_frame(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, Failure> {
    let bytes = serde_json::to_vec(value).map_err(|_| Failure::MalformedFrame)?;
    if bytes.is_empty() || bytes.len() > limit.min(MAX_FRAME_BYTES) {
        return Err(Failure::FrameTooLarge);
    }
    Ok(bytes)
}
#[cfg(unix)]
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<(), Failure> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err(Failure::FrameTooLarge);
    }
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| Failure::Connection)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| Failure::Connection)?;
    writer.flush().await.map_err(|_| Failure::Connection)
}
#[cfg(unix)]
pub(crate) async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
    limit: usize,
) -> Result<T, Failure> {
    let length = reader.read_u32().await.map_err(|_| Failure::Connection)? as usize;
    // Check header BEFORE allocating/reading any announced body.
    if length == 0 || length > limit.min(MAX_FRAME_BYTES) {
        return Err(Failure::FrameTooLarge);
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| Failure::Connection)?;
    serde_json::from_slice(&bytes).map_err(|_| Failure::MalformedFrame)
}

#[cfg(all(test, unix))]
#[path = "client_tests.rs"]
mod tests;
