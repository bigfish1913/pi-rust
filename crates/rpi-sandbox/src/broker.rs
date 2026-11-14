//! Out-of-process write broker.
//!
//! Kernel sandboxes (Landlock, Seatbelt, Windows restricted tokens) can only
//! narrow permissions, never widen them at runtime. Dynamic approval therefore
//! needs one unconfined helper process: the supervisor started before the
//! sandbox is applied. The sandboxed agent asks the broker to perform an
//! approved write; the broker re-validates the request against its own approval
//! store and the audit log and is the only component that ever touches a path
//! outside the sandbox.
//!
//! Protocol: one JSON object per line over a socketpair. The protocol is
//! intentionally boring so a future GUI can speak it too.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalScope, WriteOp};
use crate::policy::normalize_and_resolve;

/// Where the broker appends one JSON line per handled request.
#[derive(Clone, Debug, Default)]
pub struct BrokerConfig {
    /// Audit log destination. `None` disables logging (not recommended).
    pub audit: Option<PathBuf>,
}

/// One request from the sandboxed agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BrokerRequest {
    /// Register an approval for `path`.
    Approve {
        /// Request id, echoed in the response.
        id: u64,
        /// Absolute path to allow.
        path: String,
        /// Operations covered by the approval.
        operations: Vec<WriteOp>,
        /// Lifetime of the approval.
        scope: ApprovalScope,
    },
    /// Write (or append to) a file.
    Write {
        /// Request id.
        id: u64,
        /// Absolute target path.
        path: String,
        /// Base64-encoded bytes.
        data: String,
        /// Append instead of replace.
        append: bool,
    },
    /// Delete a file.
    Delete {
        /// Request id.
        id: u64,
        /// Absolute target path.
        path: String,
    },
    /// Rename or move a file.
    Rename {
        /// Request id.
        id: u64,
        /// Source path.
        path: String,
        /// Destination path.
        target: String,
    },
    /// Create a directory (and its parents).
    Mkdir {
        /// Request id.
        id: u64,
        /// Absolute directory path.
        path: String,
    },
}

impl BrokerRequest {
    /// Request id used to correlate the response.
    pub fn id(&self) -> u64 {
        match self {
            Self::Approve { id, .. }
            | Self::Write { id, .. }
            | Self::Delete { id, .. }
            | Self::Rename { id, .. }
            | Self::Mkdir { id, .. } => *id,
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Approve { path, .. }
            | Self::Write { path, .. }
            | Self::Delete { path, .. }
            | Self::Rename { path, .. }
            | Self::Mkdir { path, .. } => path,
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Approve { .. } => "approve",
            Self::Write { .. } => "write",
            Self::Delete { .. } => "delete",
            Self::Rename { .. } => "rename",
            Self::Mkdir { .. } => "mkdir",
        }
    }
}

/// One response line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrokerResponse {
    /// Request id.
    pub id: u64,
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Failure detail when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BrokerResponse {
    fn ok(id: u64) -> Self {
        Self {
            id,
            ok: true,
            error: None,
        }
    }

    fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    operations: BTreeSet<WriteOp>,
    scope: ApprovalScope,
}

/// Session-scoped approval store owned by the broker (not the sandboxed agent).
#[derive(Default, Debug)]
pub struct ApprovalStore {
    entries: BTreeMap<PathBuf, Entry>,
}

impl ApprovalStore {
    /// Record an approval. `ops` must be non-empty.
    pub fn approve(&mut self, path: &Path, ops: &[WriteOp], scope: ApprovalScope) {
        let operations: BTreeSet<WriteOp> = ops.iter().copied().collect();
        if operations.is_empty() {
            return;
        }
        self.entries
            .entry(normalize_and_resolve(path))
            .and_modify(|entry| {
                entry.operations.extend(operations.iter().copied());
                entry.scope = scope;
            })
            .or_insert(Entry { operations, scope });
    }

    /// Revoke every approval for `path`.
    pub fn revoke(&mut self, path: &Path) {
        self.entries.remove(&normalize_and_resolve(path));
    }

    /// Check and consume one use of an approval.
    pub fn allow(&mut self, path: &Path, operation: WriteOp) -> bool {
        let key = normalize_and_resolve(path);
        let Some(entry) = self.entries.get(&key) else {
            return false;
        };
        if !entry.operations.contains(&operation) {
            return false;
        }
        let scope = entry.scope;
        if scope == ApprovalScope::Once {
            self.entries.remove(&key);
        }
        true
    }
}

/// Serve the broker until the peer closes the connection.
pub fn serve<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    config: BrokerConfig,
) -> std::io::Result<()> {
    let mut store = ApprovalStore::default();
    let mut audit = open_audit(config.audit.as_deref());
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                log_audit(&mut audit, "protocol", "-", false, Some(&error.to_string()));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<BrokerRequest>(&line) {
            Ok(request) => handle(&mut store, request, &mut audit),
            Err(error) => BrokerResponse::err(0, format!("invalid request: {error}")),
        };
        let encoded = serde_json::to_string(&response)?;
        writeln!(writer, "{encoded}")?;
        writer.flush()?;
    }
    Ok(())
}

fn handle(
    store: &mut ApprovalStore,
    request: BrokerRequest,
    audit: &mut Option<File>,
) -> BrokerResponse {
    let id = request.id();
    let operation = request.operation();
    let path = request.path().to_string();
    let result = match request {
        BrokerRequest::Approve {
            path,
            operations,
            scope,
            ..
        } => match absolute_path(&path) {
            Some(path) => {
                store.approve(&path, &operations, scope);
                Ok(())
            }
            None => Err(format!("approval path must be absolute: {path}")),
        },
        BrokerRequest::Write {
            path, data, append, ..
        } => with_absolute(&path, |path| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|error| format!("invalid base64 payload: {error}"))?;
            let existed = path.exists();
            let op = if append {
                WriteOp::Append
            } else if existed {
                WriteOp::Overwrite
            } else {
                WriteOp::Create
            };
            if !store.allow(path, op) {
                return Err(format!("write to {} is not approved", path.display()));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            if append {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
                file.write_all(&bytes)
                    .map_err(|error| format!("cannot append {}: {error}", path.display()))
            } else {
                std::fs::write(path, &bytes)
                    .map_err(|error| format!("cannot write {}: {error}", path.display()))
            }
        }),
        BrokerRequest::Delete { path, .. } => with_absolute(&path, |path| {
            if !store.allow(path, WriteOp::Delete) {
                return Err(format!("delete of {} is not approved", path.display()));
            }
            std::fs::remove_file(path)
                .map_err(|error| format!("cannot delete {}: {error}", path.display()))
        }),
        BrokerRequest::Rename { path, target, .. } => {
            let source = absolute_path(&path);
            let destination = absolute_path(&target);
            match (source, destination) {
                (Some(source), Some(destination)) => {
                    if !store.allow(&source, WriteOp::Rename) {
                        return BrokerResponse::err(
                            id,
                            format!("rename of {} is not approved", source.display()),
                        );
                    }
                    if !store.allow(&destination, WriteOp::Create) {
                        return BrokerResponse::err(
                            id,
                            format!("rename target {} is not approved", destination.display()),
                        );
                    }
                    std::fs::rename(&source, &destination)
                        .map_err(|error| format!("cannot rename {}: {error}", source.display()))
                }
                _ => Err("rename requires absolute source and target".to_string()),
            }
        }
        BrokerRequest::Mkdir { path, .. } => with_absolute(&path, |path| {
            if !store.allow(path, WriteOp::Mkdir) {
                return Err(format!("mkdir of {} is not approved", path.display()));
            }
            std::fs::create_dir_all(path)
                .map_err(|error| format!("cannot create {}: {error}", path.display()))
        }),
    };

    match result {
        Ok(()) => {
            log_audit(audit, operation, &path, true, None);
            BrokerResponse::ok(id)
        }
        Err(error) => {
            log_audit(audit, operation, &path, false, Some(&error));
            BrokerResponse::err(id, error)
        }
    }
}

fn with_absolute(path: &str, run: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let path = absolute_path(path).ok_or_else(|| format!("path must be absolute: {path}"))?;
    run(&path)
}

fn absolute_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    Some(normalize_and_resolve(path))
}

fn open_audit(path: Option<&Path>) -> Option<File> {
    let path = path?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn log_audit(audit: &mut Option<File>, operation: &str, path: &str, ok: bool, error: Option<&str>) {
    let Some(file) = audit else { return };
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let entry = serde_json::json!({
        "time": seconds,
        "op": operation,
        "path": path,
        "ok": ok,
        "error": error,
    });
    let _ = writeln!(file, "{entry}");
    let _ = file.flush();
}

/// Client used by the sandboxed agent to ask the broker for approved writes.
#[cfg(unix)]
pub struct BrokerClient {
    state: std::sync::Mutex<ClientState>,
    next_id: std::sync::atomic::AtomicU64,
}

#[cfg(unix)]
struct ClientState {
    writer: std::os::unix::net::UnixStream,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
impl BrokerClient {
    /// Wrap an inherited socket fd (the child end of the broker socketpair).
    ///
    /// # Safety
    ///
    /// `fd` must be a connected Unix stream socket owned by the caller; this
    /// takes ownership of it.
    #[allow(unsafe_code)]
    pub unsafe fn from_raw_fd(fd: std::os::raw::c_int) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let writer = std::os::unix::net::UnixStream::from_raw_fd(fd);
        let reader = writer.try_clone()?;
        Ok(Self {
            state: std::sync::Mutex::new(ClientState {
                writer,
                reader: std::io::BufReader::new(reader),
            }),
            next_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// Request an approval. The broker records it for later operations.
    pub fn approve(
        &self,
        path: &Path,
        operations: &[WriteOp],
        scope: ApprovalScope,
    ) -> Result<(), String> {
        self.send(BrokerRequest::Approve {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
            operations: operations.to_vec(),
            scope,
        })
    }

    /// Write bytes through the broker.
    pub fn write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        self.send(BrokerRequest::Write {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(data),
            append: false,
        })
    }

    /// Append bytes through the broker.
    pub fn append(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        self.send(BrokerRequest::Write {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(data),
            append: true,
        })
    }

    /// Delete a file through the broker.
    pub fn delete(&self, path: &Path) -> Result<(), String> {
        self.send(BrokerRequest::Delete {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Rename through the broker.
    pub fn rename(&self, path: &Path, target: &Path) -> Result<(), String> {
        self.send(BrokerRequest::Rename {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
            target: target.to_string_lossy().into_owned(),
        })
    }

    /// Create a directory through the broker.
    pub fn mkdir(&self, path: &Path) -> Result<(), String> {
        self.send(BrokerRequest::Mkdir {
            id: self.id(),
            path: path.to_string_lossy().into_owned(),
        })
    }

    fn id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn send(&self, request: BrokerRequest) -> Result<(), String> {
        let id = request.id();
        let mut state = self.state.lock().map_err(|_| "broker client poisoned")?;
        let encoded = serde_json::to_string(&request).map_err(|error| error.to_string())?;
        writeln!(state.writer, "{encoded}").map_err(|error| error.to_string())?;
        state.writer.flush().map_err(|error| error.to_string())?;
        let mut line = String::new();
        state
            .reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if line.is_empty() {
            return Err("broker closed the connection".to_string());
        }
        let response: BrokerResponse = serde_json::from_str(&line)
            .map_err(|error| format!("invalid broker reply: {error}"))?;
        if response.id != id {
            return Err(format!(
                "broker reply id mismatch: expected {id}, got {}",
                response.id
            ));
        }
        if response.ok {
            Ok(())
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "broker refused the request".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn serve_bytes(requests: &[BrokerRequest], audit: Option<PathBuf>) -> Vec<BrokerResponse> {
        let mut input = String::new();
        for request in requests {
            input.push_str(&serde_json::to_string(request).unwrap());
            input.push('\n');
        }
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, BrokerConfig { audit }).unwrap();
        output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn unapproved_write_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        let responses = serve_bytes(
            &[BrokerRequest::Write {
                id: 1,
                path: target.to_string_lossy().into_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(b"x"),
                append: false,
            }],
            None,
        );
        assert_eq!(responses.len(), 1);
        assert!(!responses[0].ok);
        assert!(!target.exists());
    }

    #[test]
    fn approved_write_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/a.txt");
        let responses = serve_bytes(
            &[
                BrokerRequest::Approve {
                    id: 1,
                    path: target.to_string_lossy().into_owned(),
                    operations: vec![WriteOp::Create, WriteOp::Overwrite],
                    scope: ApprovalScope::Session,
                },
                BrokerRequest::Write {
                    id: 2,
                    path: target.to_string_lossy().into_owned(),
                    data: base64::engine::general_purpose::STANDARD.encode(b"approved"),
                    append: false,
                },
            ],
            None,
        );
        assert!(responses[0].ok);
        assert!(responses[1].ok, "{:?}", responses[1]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved");
    }

    #[test]
    fn once_scope_is_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("once.txt");
        let path = target.to_string_lossy().into_owned();
        let responses = serve_bytes(
            &[
                BrokerRequest::Approve {
                    id: 1,
                    path: path.clone(),
                    operations: vec![WriteOp::Create, WriteOp::Overwrite],
                    scope: ApprovalScope::Once,
                },
                BrokerRequest::Write {
                    id: 2,
                    path: path.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(b"1"),
                    append: false,
                },
                BrokerRequest::Write {
                    id: 3,
                    path,
                    data: base64::engine::general_purpose::STANDARD.encode(b"2"),
                    append: false,
                },
            ],
            None,
        );
        assert!(responses[1].ok);
        assert!(!responses[2].ok);
    }

    #[test]
    fn relative_paths_are_rejected() {
        let responses = serve_bytes(
            &[BrokerRequest::Write {
                id: 1,
                path: "relative.txt".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(b"x"),
                append: false,
            }],
            None,
        );
        assert!(!responses[0].ok);
        assert!(responses[0].error.as_deref().unwrap().contains("absolute"));
    }

    #[test]
    fn audit_log_records_allowed_and_denied_operations() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.jsonl");
        let target = dir.path().join("audited.txt");
        let path = target.to_string_lossy().into_owned();
        serve_bytes(
            &[
                BrokerRequest::Approve {
                    id: 1,
                    path: path.clone(),
                    operations: vec![WriteOp::Create],
                    scope: ApprovalScope::Session,
                },
                BrokerRequest::Write {
                    id: 2,
                    path: path.clone(),
                    data: base64::engine::general_purpose::STANDARD.encode(b"x"),
                    append: false,
                },
                BrokerRequest::Delete {
                    id: 3,
                    path: path.clone(),
                },
            ],
            Some(audit.clone()),
        );
        let text = std::fs::read_to_string(audit).unwrap();
        assert!(text.contains("\"op\":\"write\""));
        assert!(text.contains("\"ok\":true"));
        assert!(text.contains("\"op\":\"delete\""));
        assert!(text.contains("\"ok\":false"));
    }

    #[cfg(unix)]
    #[test]
    fn client_round_trip_over_socketpair() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (client, server) = UnixStream::pair().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("client.txt");

        let server_handle = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(server.try_clone().unwrap());
            serve(reader, server, BrokerConfig::default()).unwrap();
        });

        // SAFETY: `client` owns this fd; `from_raw_fd` takes ownership.
        let client = unsafe { BrokerClient::from_raw_fd(client.into_raw_fd()) }.unwrap();
        assert!(client
            .write(&target, b"no")
            .unwrap_err()
            .contains("not approved"));
        client
            .approve(
                &target,
                &[WriteOp::Create, WriteOp::Overwrite],
                ApprovalScope::Session,
            )
            .unwrap();
        client.write(&target, b"yes").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "yes");
        drop(client);
        server_handle.join().unwrap();
    }
}
