//! One local process per area serves TUI sessions and one-shot Mini runs.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout, Instant};
use tokio_util::sync::CancellationToken;

use crate::app::{EXIT_RUNTIME, EXIT_USAGE};
use crate::args::{Args, Mode};
use crate::config::AgentHome;
mod terminal;
mod terminal_event;
pub(crate) use terminal::{HostTerminal, NativeInput};

pub(crate) const DAEMON_COMMAND: &str = "__rpi_local_host";
const DEFAULT_AREA: &str = "default";
const VERSION: u32 = 1;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const DEFAULT_MAX_AGENTS: usize = 32;
// Each active session keeps one Tokio blocking worker for its RPC writer.
// Leave at least half of the default 512-worker pool for other work.
const MAX_CONFIGURED_AGENTS: usize = 256;
const MAX_CAPACITY_REJECTIONS: usize = 8;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mini: Option<Box<MiniRequest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal: Option<terminal::TerminalRequest>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MiniRequest {
    input: crate::mini::Input,
    provider: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    thinking: Option<rpi_ai::ThinkingLevel>,
    timeout: Option<Duration>,
    stream_idle_timeout: Option<Duration>,
    trust_override: Option<bool>,
    tools: Option<Vec<String>>,
    exclude_tools: Option<Vec<String>>,
    no_tools: bool,
    no_builtin_tools: bool,
    system_prompt: Option<String>,
    append_system_prompt: Vec<String>,
    debug_system_prompt: bool,
}

impl MiniRequest {
    fn new(args: Args, input: crate::mini::Input) -> Self {
        Self {
            input,
            provider: args.provider,
            model: args.model,
            api_key: args.api_key,
            base_url: args.base_url,
            thinking: args.thinking,
            timeout: args.timeout,
            stream_idle_timeout: args.stream_idle_timeout,
            trust_override: args.trust_override,
            tools: args.tools,
            exclude_tools: args.exclude_tools,
            no_tools: args.no_tools,
            no_builtin_tools: args.no_builtin_tools,
            system_prompt: args.system_prompt,
            append_system_prompt: args.append_system_prompt,
            debug_system_prompt: args.debug_system_prompt,
        }
    }

    fn into_run(self, area: Args) -> (Args, crate::mini::Input) {
        let args = Args {
            mini: true,
            no_session: true,
            no_extensions: true,
            provider: self.provider.or(area.provider),
            model: self.model.or(area.model),
            api_key: self.api_key,
            base_url: self.base_url,
            thinking: self.thinking,
            timeout: self.timeout,
            stream_idle_timeout: self.stream_idle_timeout,
            trust_override: self.trust_override,
            tools: self.tools,
            exclude_tools: self.exclude_tools,
            no_tools: self.no_tools,
            no_builtin_tools: self.no_builtin_tools,
            system_prompt: self.system_prompt,
            append_system_prompt: self.append_system_prompt,
            debug_system_prompt: self.debug_system_prompt,
            ..Args::default()
        };
        (args, self.input)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenReply {
    Opened {
        host_pid: u32,
        agent_id: String,
        session_id: String,
    },
    TerminalOpened,
    MiniFinished {
        exit_code: i32,
        text: Option<String>,
        diagnostics: Vec<String>,
    },
    Error {
        message: String,
    },
}

pub(crate) async fn run(mut args: Args) -> i32 {
    let requested = args.host.take().filter(|area| !area.is_empty());
    if !check_area(requested.as_deref()) {
        return EXIT_USAGE;
    }
    let home = AgentHome::process();
    let config = match HostConfig::load(&home) {
        Ok(config) => config,
        Err(message) => return exit_code(Err(message)),
    };
    let area = resolve_area(requested.as_deref(), &config);
    if args.mini {
        exit_code(run_mini_client(&home, area, args).await)
    } else {
        exit_code(run_client(&home, area, &args).await)
    }
}

pub(crate) async fn run_daemon_command(args: &[String]) -> i32 {
    if args.len() > 1 {
        return EXIT_USAGE;
    }
    let requested = args.first().map(String::as_str);
    if !check_area(requested) {
        return EXIT_USAGE;
    }
    let home = AgentHome::process();
    let config = match HostConfig::load(&home) {
        Ok(config) => config,
        Err(message) => return exit_code(Err(message)),
    };
    let area = resolve_area(requested, &config);
    exit_code(
        run_daemon(&home, area, config.areas.get(area))
            .await
            .map(|()| 0),
    )
}

fn exit_code(result: Result<i32, String>) -> i32 {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_RUNTIME
        }
    }
}

fn check_area(area: Option<&str>) -> bool {
    if area.is_some_and(|area| !valid_area(area)) {
        eprintln!("error: Host area must start with a lowercase ASCII letter or digit and contain 1–32 lowercase ASCII letters, digits, hyphens or underscores");
        return false;
    }
    true
}

fn resolve_area<'a>(requested: Option<&'a str>, config: &'a HostConfig) -> &'a str {
    requested
        .or(config.default_area.as_deref())
        .unwrap_or(DEFAULT_AREA)
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
    mut args: Args,
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
    let workspace = validate_workspace(&open.workspace);
    let cwd = match workspace {
        Ok(cwd) => cwd,
        Err(message) => return write_json(&mut output, &OpenReply::Error { message }).await,
    };
    if let Some(request) = open.mini {
        let (args, request) = request.into_run(args);
        let signal = CancellationToken::new();
        let mut diagnostics = Vec::new();
        let result = {
            let run = crate::mini::execute(&home, args, &cwd, request, signal.clone(), |message| {
                diagnostics.push(message);
            });
            tokio::pin!(run);
            tokio::select! {
                _ = input.fill_buf() => {
                    signal.cancel();
                    let _ = run.await;
                    return Ok(());
                }
                result = &mut run => result,
            }
        };
        let (exit_code, text) = match result {
            Ok(text) => (0, text),
            Err(exit_code) => (exit_code, None),
        };
        return write_json(
            &mut output,
            &OpenReply::MiniFinished {
                exit_code,
                text,
                diagnostics,
            },
        )
        .await;
    }
    if let Some(terminal) = &open.terminal {
        args.mode = Mode::Text;
        args.tui_mode = if terminal.regular {
            crate::args::TuiMode::Regular
        } else {
            crate::args::TuiMode::Fullscreen
        };
        args.theme = terminal.theme.clone();
        args.no_themes = terminal.no_themes;
    }
    let built = async {
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
        Ok::<_, String>((harness, events, context, models, resolved.theme))
    }
    .await;
    let (harness, events, context, models, theme) = match built {
        Ok(session) => session,
        Err(message) => return write_json(&mut output, &OpenReply::Error { message }).await,
    };
    if let Some(terminal) = open.terminal {
        write_json(&mut output, &OpenReply::TerminalOpened).await?;
        return terminal::serve(
            &harness,
            terminal,
            idle_timeout,
            (input, output),
            async |terminal| {
                Box::pin(crate::modes::interactive_with_terminal(
                    &harness,
                    Some(events),
                    &args,
                    models,
                    None,
                    &[],
                    Vec::new(),
                    args.theme.as_deref().or(theme.as_deref()),
                    args.no_themes,
                    &context,
                    crate::interactive_tui::InteractiveTerminal::Host(terminal),
                ))
                .await
            },
        )
        .await;
    }
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

fn validate_workspace(workspace: &Path) -> Result<PathBuf, String> {
    let cwd = workspace
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !workspace.is_absolute() || !cwd.is_dir() {
        return Err("local Host workspace must be an absolute directory".into());
    }
    Ok(cwd)
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

async fn run_mini_client(home: &AgentHome, area: &str, mut args: Args) -> Result<i32, String> {
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    crate::app::emit_startup_warnings(&args);
    let (file_text, images) = match crate::app::process_file_args(&args.file_args, &workspace) {
        Ok(files) => files,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(EXIT_USAGE);
        }
    };
    let input = crate::app::mini_input(&mut args, file_text, images);
    if input.prompts.is_empty() {
        return Ok(0);
    }
    let directory = host_directory(home, area)?;
    let (input_stream, mut output) = connect(area, &directory).await?.into_split();
    let mut input_stream = BufReader::new(input_stream);
    write_json(
        &mut output,
        &OpenSession {
            version: VERSION,
            workspace,
            mini: Some(Box::new(MiniRequest::new(args, input))),
            terminal: None,
        },
    )
    .await?;
    match read_json::<OpenReply>(&mut input_stream).await? {
        Some(OpenReply::MiniFinished { exit_code, text, diagnostics }) => {
            for message in diagnostics {
                eprintln!("{message}");
            }
            if let Some(text) = text {
                crate::mini::print_text(text);
            }
            Ok(exit_code)
        }
        Some(OpenReply::Error { message }) => Err(message),
        Some(OpenReply::TerminalOpened | OpenReply::Opened { .. }) => Err("local Host returned a session for a Mini run".into()),
        None => Err("local Host closed before returning a Mini result. Check that the area's running Host supports --mini".into()),
    }
}

async fn run_client(home: &AgentHome, area: &str, args: &Args) -> Result<i32, String> {
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
            mini: None,
            terminal: Some(terminal::TerminalRequest::new(args)),
        },
    )
    .await?;
    let opened = timeout(Duration::from_secs(90), read_json::<OpenReply>(&mut input))
        .await
        .map_err(|_| "local Host session startup timed out")??;
    match opened {
        Some(OpenReply::TerminalOpened) => terminal::attach(input, output).await,
        Some(OpenReply::Error { message }) => Err(message),
        _ => Err(format!("Host area {area:?} did not open a native terminal session. Restart the area if it is running an earlier rpi version.")),
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
