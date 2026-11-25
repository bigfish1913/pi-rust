//! One local process per area serves a new native AgentHarness on each TUI connection.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::{sleep, timeout, Instant};

use crate::args::{Args, Mode};
use crate::config::AgentHome;
use crate::remote::protocol::RemoteCommand;
use crate::session_driver::{DriverCommand, DriverEvent, RemoteEventMapper, SessionDriver};

const DAEMON_COMMAND: &str = "__rpi_local_host";
const DEFAULT_AREA: &str = "default";
const VERSION: u32 = 1;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const DEFAULT_MAX_AGENTS: usize = 32;
// Each active session keeps one Tokio blocking worker for its RPC writer.
// Leave at least half of the default 512-worker pool for other work.
const MAX_CONFIGURED_AGENTS: usize = 256;
const MAX_CAPACITY_REJECTIONS: usize = 8;
const HELP: &str = "Usage: rpi --host [area]\n\nOpen a new TUI session in the area's local rpi process, starting it if needed.\nSet defaultArea in the agent home's host.json to override 'default'.";

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct HostConfig {
    default_area: Option<String>,
    areas: BTreeMap<String, AreaConfig>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct AreaConfig {
    provider: Option<String>,
    model: Option<String>,
    tools: Option<Vec<String>>,
    max_agents: Option<usize>,
    idle_timeout_seconds: u32,
}

impl HostConfig {
    fn load(home: &AgentHome) -> Result<Self, String> {
        let path = home
            .root()
            .map_err(|error| error.to_string())?
            .join("host.json");
        let invalid =
            |message: &str| format!("invalid Host configuration {}: {message}", path.display());
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(invalid(&error.to_string())),
        };
        if !file
            .metadata()
            .map_err(|error| invalid(&error.to_string()))?
            .is_file()
        {
            return Err(invalid("expected a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| invalid(&error.to_string()))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid("file exceeds 64 KiB"));
        }
        let config: Self =
            serde_json::from_slice(&bytes).map_err(|error| invalid(&error.to_string()))?;
        for area in config.default_area.iter().chain(config.areas.keys()) {
            if !valid_area(area) {
                return Err(invalid(&format!("invalid area name {area:?}")));
            }
        }
        for (area, profile) in &config.areas {
            if profile
                .max_agents
                .is_some_and(|count| !(1..=MAX_CONFIGURED_AGENTS).contains(&count))
            {
                return Err(invalid(&format!(
                    "area {area:?} maxAgents must be between 1 and {MAX_CONFIGURED_AGENTS}"
                )));
            }
            let mut values = profile
                .provider
                .iter()
                .chain(profile.model.iter())
                .chain(profile.tools.iter().flatten());
            if values.any(|value| value.trim().is_empty()) {
                return Err(invalid(&format!(
                    "area {area:?} contains an empty provider, model, or tool name"
                )));
            }
        }
        Ok(config)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenSession {
    version: u32,
    workspace: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenReply {
    Opened {
        host_pid: u32,
        agent_id: String,
        session_id: String,
    },
    Error {
        message: String,
    },
}

pub(crate) async fn run(args: &[String]) -> i32 {
    let result = match args {
        [command, flag] if command == "--host" && matches!(flag.as_str(), "--help" | "-h") => {
            println!("{HELP}");
            return 0;
        }
        [command, rest @ ..]
            if matches!(command.as_str(), "--host" | DAEMON_COMMAND) && rest.len() <= 1 =>
        {
            let requested = rest.first().map(String::as_str);
            if requested.is_some_and(|area| !valid_area(area)) {
                eprintln!("error: Host area must start with a lowercase ASCII letter or digit and contain 1–32 lowercase ASCII letters, digits, hyphens or underscores");
                return crate::app::EXIT_USAGE;
            }
            enter(command == DAEMON_COMMAND, requested).await
        }
        _ => {
            eprintln!("{HELP}");
            return crate::app::EXIT_USAGE;
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            crate::app::EXIT_RUNTIME
        }
    }
}

async fn enter(daemon: bool, requested: Option<&str>) -> Result<(), String> {
    let home = AgentHome::process();
    let config = HostConfig::load(&home)?;
    let area = requested
        .or(config.default_area.as_deref())
        .unwrap_or(DEFAULT_AREA);
    if daemon {
        run_daemon(&home, area, config.areas.get(area)).await
    } else {
        run_client(&home, area).await
    }
}

fn valid_area(area: &str) -> bool {
    let is_alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    area.len() <= 32
        && area.bytes().next().is_some_and(is_alphanumeric)
        && area
            .bytes()
            .all(|byte| is_alphanumeric(byte) || matches!(byte, b'-' | b'_'))
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no parameters and does not modify process state.
    unsafe { nix::libc::geteuid() }
}

fn host_directory(home: &AgentHome, area: &str) -> Result<PathBuf, String> {
    let root = home.root().map_err(|error| error.to_string())?;
    let base = root.join("local-host-v1");
    ensure_private_directory(&base)?;
    let directory = if area == DEFAULT_AREA {
        base
    } else {
        let directory = base.join(format!("area-{area}"));
        ensure_private_directory(&directory)?;
        directory
    };
    // Resolve aliases once, including macOS's /var and /tmp aliases.
    directory.canonicalize().map_err(|error| error.to_string())
}

fn ensure_private_directory(directory: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|error| format!("could not create local Host directory: {error}"))?;
    let metadata = fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "local Host directory must be private: {}",
            directory.display()
        ));
    }
    Ok(())
}

fn check_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local Host peer belongs to another user",
        ));
    }
    Ok(())
}

async fn connect(area: &str, directory: &Path) -> Result<UnixStream, String> {
    let socket = directory.join("rpc.sock");
    let mut child: Option<Child> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                check_peer(&stream).map_err(|error| error.to_string())?;
                if let Some(mut daemon) = child.take() {
                    // The daemon outlives the TUI. Reap a losing contender even
                    // when it exits after this client has connected.
                    tokio::spawn(async move {
                        let _ = daemon.wait().await;
                    });
                }
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "local Host did not listen at {}: {error}",
                        socket.display()
                    ));
                }
                if let Some(daemon) = child.as_mut() {
                    // Reap a startup contender if another process acquired the lock.
                    if let Some(status) = daemon.try_wait().map_err(|error| error.to_string())? {
                        if !status.success() {
                            return Err(format!("local Host exited at startup: {status}"));
                        }
                    }
                } else {
                    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
                    child = Some(
                        Command::new(executable)
                            .args([DAEMON_COMMAND, area])
                            .process_group(0)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|error| format!("could not start local Host: {error}"))?,
                    );
                }
                sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(format!("could not connect to local Host: {error}")),
        }
    }
}

async fn run_daemon(
    home: &AgentHome,
    area: &str,
    profile: Option<&AreaConfig>,
) -> Result<(), String> {
    let directory = host_directory(home, area)?;
    let args = Args {
        mode: Mode::Rpc,
        no_extensions: true,
        session_dir: (area != DEFAULT_AREA).then(|| directory.join("sessions")),
        provider: profile.and_then(|config| config.provider.clone()),
        model: profile.and_then(|config| config.model.clone()),
        tools: profile.and_then(|config| config.tools.clone()),
        ..Args::default()
    };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(directory.join("owner.lock"))
        .map_err(|error| format!("could not open local Host lock: {error}"))?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => return Err(format!("could not acquire local Host lock: {error}")),
    }
    let socket = directory.join("rpc.sock");
    match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == current_uid() => {
            fs::remove_file(&socket).map_err(|error| error.to_string())?;
        }
        Ok(_) => return Err("local Host socket path is occupied".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let settings = home.settings().unwrap_or_default();
    crate::session::apply_process_proxy(settings.http_proxy.as_deref());
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("could not bind local Host socket: {error}"))?;
    let max_agents = profile
        .and_then(|config| config.max_agents)
        .unwrap_or(DEFAULT_MAX_AGENTS);
    let idle_timeout = profile
        .filter(|config| config.idle_timeout_seconds != 0)
        .map(|config| Duration::from_secs(u64::from(config.idle_timeout_seconds)));
    let capacity = Arc::new(Semaphore::new(max_agents));
    let rejections = Arc::new(Semaphore::new(MAX_CAPACITY_REJECTIONS));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(nix::libc::EMFILE | nix::libc::ENFILE)
                ) =>
            {
                // Existing sessions can finish and free descriptors while accepts back off.
                sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if check_peer(&stream).is_err() {
            continue;
        }
        if let Ok(permit) = Arc::clone(&capacity).try_acquire_owned() {
            let home = home.clone();
            let args = args.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = serve_session(stream, home, args, idle_timeout).await {
                    tracing::debug!(%error, "local Host connection ended");
                }
            });
        } else if let Ok(permit) = Arc::clone(&rejections).try_acquire_owned() {
            let message = format!("Host area {area:?} is full (maxAgents: {max_agents})");
            tokio::spawn(async move {
                let _permit = permit;
                let (input, mut output) = stream.into_split();
                let mut input = BufReader::new(input);
                if matches!(
                    timeout(WRITE_TIMEOUT, read_json::<OpenSession>(&mut input)).await,
                    Ok(Ok(Some(_)))
                ) {
                    let _ = write_json(&mut output, &OpenReply::Error { message }).await;
                }
            });
        }
    }
}

async fn serve_session(
    stream: UnixStream,
    home: AgentHome,
    args: Args,
    idle_timeout: Option<Duration>,
) -> Result<(), String> {
    let (input, mut output) = stream.into_split();
    let mut input = BufReader::new(input);
    // A startup probe is allowed to disconnect without allocating a session.
    let Some(open) = timeout(WRITE_TIMEOUT, read_json::<OpenSession>(&mut input))
        .await
        .map_err(|_| "local Host opening request timed out")??
    else {
        return Ok(());
    };
    if open.version != VERSION {
        return write_json(
            &mut output,
            &OpenReply::Error {
                message: "unsupported local Host protocol version".into(),
            },
        )
        .await;
    }
    let built = async {
        let cwd = open
            .workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !open.workspace.is_absolute() || !cwd.is_dir() {
            return Err("local Host workspace must be an absolute directory".into());
        }
        let trusted = crate::session::resolve_project_trust_for(&home, &args, &cwd);
        let resolved = crate::provider::resolve_for_cwd(
            &home,
            args.provider.as_deref(),
            args.model.as_deref(),
            None,
            None,
            None,
            &cwd,
            trusted,
        )
        .map_err(|error| error.to_string())?;
        let models = crate::provider::authenticated_catalog(&resolved);
        let (harness, events, context) =
            crate::session::build_host_session(&home, &resolved, &args, &cwd, trusted)
                .await
                .map_err(|error| error.to_string())?;
        Ok::<_, String>((harness, events, context, models))
    }
    .await;
    let (harness, events, context, models) = match built {
        Ok(session) => session,
        Err(message) => return write_json(&mut output, &OpenReply::Error { message }).await,
    };
    let opened = OpenReply::Opened {
        host_pid: std::process::id(),
        agent_id: uuid::Uuid::new_v4().to_string(),
        session_id: harness.session_id(),
    };
    write_json(&mut output, &opened).await?;
    // Keep the session's owned resources alive through RPC run settlement.
    crate::modes::rpc_host_session(
        &harness,
        events,
        models,
        context.ui_dialog.clone(),
        home,
        idle_timeout,
        (input, SocketWriter(output)),
    )
    .await;
    drop(context);
    Ok(())
}

// The existing RPC writer calls this only on its blocking worker. The timeout
// lets it fail the transport if a disconnected or stalled client cannot drain it.
struct SocketWriter(OwnedWriteHalf);

impl Write for SocketWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        tokio::runtime::Handle::current()
            .block_on(async { timeout(WRITE_TIMEOUT, self.0.write(bytes)).await? })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    input: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<T>, String> {
    crate::rpc::framing::read_frame(input)
        .await
        .map_err(|error| error.to_string())?
        .map(|frame| serde_json::from_slice(&frame).map_err(|error| error.to_string()))
        .transpose()
}

async fn write_json(
    output: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() >= crate::rpc::protocol::MAX_FRAME_BYTES {
        return Err("local Host request exceeds the RPC frame limit".into());
    }
    bytes.push(b'\n');
    timeout(WRITE_TIMEOUT, output.write_all(&bytes))
        .await
        .map_err(|_| "local Host write timed out".to_string())?
        .map_err(|error| error.to_string())
}

async fn run_client(home: &AgentHome, area: &str) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("rpi --host requires a terminal".into());
    }
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let directory = host_directory(home, area)?;
    let (input, mut output) = connect(area, &directory).await?.into_split();
    let mut input = BufReader::new(input);
    write_json(
        &mut output,
        &OpenSession {
            version: VERSION,
            workspace,
        },
    )
    .await?;
    let opened = timeout(Duration::from_secs(90), read_json::<OpenReply>(&mut input))
        .await
        .map_err(|_| "local Host session startup timed out")??;
    let (host_pid, agent_id, session_id) = match opened {
        Some(OpenReply::Opened {
            host_pid,
            agent_id,
            session_id,
        }) => (host_pid, agent_id, session_id),
        Some(OpenReply::Error { message }) => return Err(message),
        None => return Err("local Host closed before opening a session".into()),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let (expired_tx, mut expired_rx) = oneshot::channel();
    let reader = tokio::spawn(async move {
        let mut mapper = RemoteEventMapper::default();
        loop {
            match read_json::<serde_json::Value>(&mut input).await {
                Ok(Some(line)) => {
                    if let Some(seconds) = line
                        .get("idleTimeoutSeconds")
                        .and_then(serde_json::Value::as_u64)
                        .filter(|_| line["type"] == "host_idle_timeout")
                    {
                        let _ = expired_tx.send(seconds);
                        break;
                    }
                    if !mapper.forward(line, &tx) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = tx.send(DriverEvent::Notice(format!("local Host stream: {error}")));
                    break;
                }
            }
        }
        let _ = tx.send(DriverEvent::Ended);
    });
    let driver = HostDriver {
        output: tokio::sync::Mutex::new(output),
        next_id: AtomicU64::new(1),
        events: Mutex::new(Some(rx)),
    };
    let result = async {
        driver.send(DriverCommand::RefreshState).await?;
        crate::tui_shell::run_driver_tui(
            &driver,
            &format!("Host {host_pid} · Area {area} · Session {session_id} · Agent {agent_id}"),
        )
        .await
    }
    .await;
    if let Ok(seconds) = expired_rx.try_recv() {
        return Err(format!(
            "Host session closed after {seconds} seconds of inactivity"
        ));
    }
    let _ = driver.send_rpc(RemoteCommand::Stop).await;
    reader.abort();
    result
}

struct HostDriver {
    output: tokio::sync::Mutex<OwnedWriteHalf>,
    next_id: AtomicU64,
    events: Mutex<Option<mpsc::UnboundedReceiver<DriverEvent>>>,
}

impl HostDriver {
    async fn send_rpc(&self, command: RemoteCommand) -> Result<(), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        write_json(&mut *self.output.lock().await, &command.with_id(id)).await
    }
}

#[async_trait]
impl SessionDriver for HostDriver {
    fn events(&self) -> Option<mpsc::UnboundedReceiver<DriverEvent>> {
        self.events.lock().unwrap().take()
    }

    async fn send(&self, command: DriverCommand) -> Result<(), String> {
        self.send_rpc(command.into()).await
    }
}

#[cfg(test)]
mod tests {
    use super::valid_area;

    #[test]
    fn area_names_follow_the_documented_grammar() {
        for valid in ["default", "work", "a", "0", "team-1_x", &"a".repeat(32)] {
            assert!(valid_area(valid), "{valid}");
        }
        for invalid in [
            "",
            "Work",
            "work.1",
            "-work",
            "_work",
            "a/b",
            "工作",
            &"a".repeat(33),
        ] {
            assert!(!valid_area(invalid), "{invalid}");
        }
    }
}
