//! Detached Darwin supervisor. Only workers assemble profiles or open a Host.
use super::{
    client::{encode_frame, read_frame, write_frame},
    protocol::Welcome,
    registry,
    server::{bind_endpoint, SocketLease},
    tool_processes::drain_session,
};
use nix::{
    libc,
    sys::signal::{kill, Signal},
    unistd::{getpid, getsid, setsid, Pid},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream as StdStream},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
    signal::unix::{signal, SignalKind},
};

pub(super) const SUPERVISOR: &str = "__supervisor";
pub(super) const WORKER: &str = "__worker";
const MAX_FRAME: usize = 8192;
const STARTUP: Duration = Duration::from_secs(300);
const SETTLEMENT: Duration = Duration::from_secs(10);
const RESTARTS: usize = 3;

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Ready {
    pub pid: u32,
    pub control: Welcome,
}
impl Ready {
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"event":"host_ready", "pid":self.pid, "control":self.control})
    }
}

async fn send<T: Serialize>(stream: &mut UnixStream, message: &T) -> io::Result<()> {
    let frame = encode_frame(message, MAX_FRAME)
        .map_err(|_| io::Error::other("invalid detached Host message"))?;
    write_frame(stream, &frame)
        .await
        .map_err(|_| io::Error::other("detached Host channel write failed"))
}
async fn receive<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    read_frame(stream, MAX_FRAME)
        .await
        .map_err(|_| io::Error::other("detached Host channel closed or invalid response"))
}

fn stream(socket: StdStream) -> io::Result<UnixStream> {
    socket.set_nonblocking(true)?;
    UnixStream::from_std(socket)
}
fn channel(command: &mut Command) -> io::Result<UnixStream> {
    let (local, child) = StdStream::pair()?;
    command.stdin(Stdio::from(OwnedFd::from(child)));
    stream(local)
}

/// Adopt the private launch socket, then give the CLI and plugins ordinary EOF
/// on stdin. The duplicate is close-on-exec before any Host code can spawn.
pub(super) fn adopt_channel() -> io::Result<UnixStream> {
    let null = File::open("/dev/null")?;
    // SAFETY: fcntl duplicates the inherited descriptor. dup2 atomically
    // replaces stdin and neither call creates a Rust owner of descriptor zero.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 4) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly owned, distinct from stdin and null.
    let socket = unsafe { StdStream::from_raw_fd(fd) };
    socket.peer_addr()?;
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    stream(socket)
}

pub(super) async fn launch(path: &Path) -> io::Result<Ready> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["host", SUPERVISOR, "--config"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe. The new session provides a stable
    // kernel-owned boundary for every generation and its tool process groups.
    unsafe { command.pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from)) };
    let mut socket = channel(&mut command)?;
    let mut child = command.spawn()?;
    let result =
        tokio::time::timeout(STARTUP, receive::<Result<Ready, String>>(&mut socket)).await?;
    let outcome = result.and_then(|value| value.map_err(io::Error::other));
    drop(socket);
    if outcome.is_err() {
        // Let a settled failure finish unlinking before returning its result.
        let _ = tokio::time::timeout(SETTLEMENT, child.wait()).await;
    }
    outcome
}

pub(super) struct Worker {
    pub listener: UnixListener,
    pub monitor: WorkerMonitor,
}

pub(super) struct WorkerMonitor {
    pub socket: UnixStream,
    pub session: i32,
    pub lease: SocketLease,
    // Keeps custody across supervisor SIGKILL until the worker drains the session.
    _supervisor_lock: File,
}

impl Worker {
    pub fn adopt(
        path: PathBuf,
        identity: (u64, u64),
        socket: UnixStream,
        listener_fd: i32,
        lock_fd: i32,
    ) -> io::Result<Self> {
        let session = getsid(None)?.as_raw();
        if session != nix::unistd::getppid().as_raw() {
            return Err(io::Error::other("detached Host supervisor is unavailable"));
        }
        if listener_fd < 3 || lock_fd < 3 || listener_fd == lock_fd {
            return Err(io::Error::other("invalid detached Host descriptors"));
        }
        let listener = UnixListener::from(adopt_fd(listener_fd)?);
        if listener.local_addr()?.as_pathname() != Some(path.as_path()) {
            return Err(io::Error::other(
                "detached Host endpoint does not match config",
            ));
        }
        Ok(Self {
            listener,
            monitor: WorkerMonitor {
                socket,
                session,
                lease: SocketLease::inherited(path, identity),
                _supervisor_lock: adopt_fd(lock_fd)?.into(),
            },
        })
    }
}

fn adopt_fd(fd: i32) -> io::Result<OwnedFd> {
    // SAFETY: the worker owns the inherited descriptor. Set close-on-exec
    // before profile assembly can spawn programs that would inherit it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags != 0 {
        return Err(io::Error::other("invalid inherited Host descriptor"));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(super) async fn read_config(
    socket: &mut UnixStream,
    max_len: usize,
) -> io::Result<((u64, u64), Vec<u8>)> {
    let identity = (socket.read_u64().await?, socket.read_u64().await?);
    let len = socket.read_u32().await? as usize;
    if len > max_len {
        return Err(io::Error::other("detached Host config exceeds byte budget"));
    }
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes).await?;
    Ok((identity, bytes))
}

pub(super) async fn publish(socket: &mut UnixStream, ready: &Ready) -> io::Result<()> {
    send(socket, ready).await
}

pub(super) async fn detached(
    mut launcher: UnixStream,
    directory: PathBuf,
    socket_path: PathBuf,
    config: Vec<u8>,
    restart: bool,
) -> io::Result<()> {
    let plan = (|| {
        if getsid(None)? != getpid() {
            return Err(io::Error::other(
                "detached Host session was not established",
            ));
        }
        let lock = registry::own_directory_lock(&directory, "supervisor.lock")
            .map_err(io::Error::other)?;
        let log =
            registry::private_file(&directory.join("supervisor.log")).map_err(io::Error::other)?;
        let (listener, lease) = bind_endpoint(socket_path).map_err(io::Error::other)?;
        Ok((lock, log, listener, lease))
    })();
    let (lock, mut log, listener, lease) = match plan {
        Ok(value) => value,
        Err(error) => {
            let _ = send(&mut launcher, &Err::<Ready, _>(error.to_string())).await;
            return Err(error);
        }
    };
    // A single open-file description with append enabled keeps the worker's
    // copies and the supervisor from overwriting each other's diagnostics.
    enable_append(&log)?;
    let outcome = supervise(
        launcher,
        &directory,
        &listener,
        (&lease, &lock),
        &mut log,
        &config,
        restart,
    )
    .await;
    if let Err(error) = &outcome {
        let _ = writeln!(log, "error: {error}");
    }
    outcome
}

fn enable_append(log: &File) -> io::Result<()> {
    // SAFETY: descriptor belongs to a checked private regular file.
    let flags = unsafe { libc::fcntl(log.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(log.as_raw_fd(), libc::F_SETFL, flags | libc::O_APPEND) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn spawn_worker(
    listener: &UnixListener,
    lock: &registry::InstanceLock,
    log: &File,
) -> io::Result<(Child, UnixStream)> {
    let mut command = Command::new(std::env::current_exe()?);
    let listener = listener.as_raw_fd();
    let lock = lock.file().as_raw_fd();
    command
        .args(["host", WORKER, &listener.to_string(), &lock.to_string()])
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .process_group(0);
    let socket = channel(&mut command)?;
    // SAFETY: fcntl changes the inherited descriptors only in the child.
    // Their numbers are reserved by the open files and cannot collide with
    // stdio redirection or the spawn error channel.
    unsafe {
        command.pre_exec(move || {
            for fd in [listener, lock] {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok((command.spawn()?, socket))
}

async fn supervise(
    launcher: UnixStream,
    directory: &Path,
    listener: &UnixListener,
    (lease, lock): (&SocketLease, &registry::InstanceLock),
    log: &mut File,
    config: &[u8],
    restart: bool,
) -> io::Result<()> {
    let (socket_dev, socket_ino) = lease.identity();
    let mut launcher = Some(launcher);
    let (mut interrupt, mut terminate) = (
        signal(SignalKind::interrupt())?,
        signal(SignalKind::terminate())?,
    );
    let session = getpid().as_raw();
    for attempt in 0..=RESTARTS {
        let (mut child, mut socket) = spawn_worker(listener, lock, log)?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("detached Host worker PID is absent"))?;
        let mut stop_byte = [0];
        let stopping = async {
            tokio::select! {
                _ = interrupt.recv() => (),
                _ = terminate.recv() => (),
                _ = async {
                    if let Some(launcher) = launcher.as_mut() {
                        let _ = launcher.read(&mut stop_byte).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => (),
            }
        };
        let startup = async {
            socket.write_u64(socket_dev).await?;
            socket.write_u64(socket_ino).await?;
            socket.write_u32(config.len() as u32).await?;
            socket.write_all(config).await?;
            let ready: Ready = receive(&mut socket).await?;
            if ready.pid != pid {
                return Err(io::Error::other(
                    "detached Host worker readiness identity changed",
                ));
            }
            Ok(ready)
        };
        let ready = tokio::select! {
            biased;
            _ = stopping => Err(io::Error::other("detached Host was stopped during startup")),
            result = tokio::time::timeout(STARTUP, startup) => result.map_err(io::Error::other).and_then(|value| value),
        };
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                // Closing the private channel also requests shutdown if the
                // worker is still building state and has not sent readiness.
                drop(socket);
                terminate_worker(&child)?;
                child.wait().await?;
                settle_owned(session, log).await;
                if let Some(mut launcher) = launcher.take() {
                    let _ = send(
                        &mut launcher,
                        &Err::<Ready, _>("detached Host startup failed; inspect supervisor.log"),
                    )
                    .await;
                }
                return Err(error);
            }
        };
        let epoch = ready.control.epoch.to_string();
        let mut intentional = false;
        if let Some(mut launcher) = launcher.take() {
            intentional = send(&mut launcher, &Ok::<_, String>(ready)).await.is_err();
        }
        if intentional {
            terminate_worker(&child)?;
        }
        let mut socket_open = true;
        let status = loop {
            tokio::select! {
                biased;
                _ = interrupt.recv() => {
                    intentional = true;
                    terminate_worker(&child)?;
                }
                _ = terminate.recv() => {
                    intentional = true;
                    terminate_worker(&child)?;
                }
                result = socket.read(&mut stop_byte), if socket_open => {
                    socket_open = matches!(result, Ok(1));
                    intentional |= socket_open && stop_byte[0] == 1;
                }
                status = child.wait() => break status?,
            }
        };
        settle_owned(session, log).await;
        if intentional || status.success() || !restart || stop_admitted(directory, &epoch)? {
            return Ok(());
        }
        if attempt == RESTARTS {
            return Err(io::Error::other("detached Host restart budget exhausted"));
        }
        // The old worker cannot accept any more connections. Discard sockets
        // queued during exit, before admitting a new epoch on this listener.
        while listener.accept().is_ok() {}
        let _ = writeln!(
            log,
            "Host worker exited; restarting after cleanup ({}/{RESTARTS})",
            attempt + 1
        );
        tokio::select! {
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))) => (),
        }
    }
    unreachable!("bounded worker loop always returns after the final attempt")
}

fn terminate_worker(child: &Child) -> io::Result<()> {
    if let Some(pid) = child.id() {
        match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) async fn settle(session: i32) -> io::Result<()> {
    tokio::task::spawn_blocking(move || drain_session(session, SETTLEMENT)).await?
}

async fn settle_owned(session: i32, log: &mut File) {
    loop {
        match settle(session).await {
            Ok(()) => return,
            Err(error) => {
                let _ = writeln!(log, "Host tool settlement is still uncertain: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Observe the exact epoch's committed Stop after worker death. A worker can
/// die immediately after admitting it, before any in-memory notification.
fn stop_admitted(directory: &Path, epoch: &str) -> io::Result<bool> {
    use rusqlite::{Connection, OpenFlags};
    let database = directory.canonicalize()?.join("agents.sqlite3");
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(io::Error::other)?;
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_requests WHERE epoch=?1 AND agent_id IS NULL)",
            [epoch],
            |row| row.get(0),
        )
        .map_err(io::Error::other)
}
