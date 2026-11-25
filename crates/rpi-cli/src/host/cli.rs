//! Early CLI dispatch: no Args/settings, trust, extension, credential or Session
//! setup. Secrets are read only from an explicitly named private config file.

const HELP: &str = "Usage:
  rpi host serve --config <absolute-private-json>
  rpi host status --config <absolute-private-json>
  rpi host list --config <absolute-private-json> [--after <agent-uuid>] [--limit <1..128>]
  rpi host inspect --config <absolute-private-json> --agent <agent-uuid>
  rpi host profiles --config <absolute-private-json> [--after <agent-uuid>] [--limit <1..128>]
  rpi host administrative-receipt --config <absolute-private-json> --request <absolute-private-json>
  rpi host administrative-create --config <absolute-private-json> --request <absolute-private-json>
  rpi host administrative-restore --config <absolute-private-json> --request <absolute-private-json>
  rpi host administrative-close --config <absolute-private-json> --request <absolute-private-json>
  rpi host administrative-cancel --config <absolute-private-json> --request <absolute-private-json>
  rpi host administrative-stop --config <absolute-private-json> --request <absolute-private-json>
  rpi host submit --config <absolute-private-json> --request <absolute-private-submission-json>
  rpi host receipt --config <absolute-private-json> --request <absolute-private-submission-json>

Submit/receipt files contain original agent_id/request_id/prompt, <=1MiB file,
<=256KiB UTF-8 prompt and <=512KiB serialized native messages. No prompt CLI flag.
Submit exit 0 proves durable input custody only, NOT execution completion/success.
Receipt exit 0 proves a matching custody observation (inspect execution separately);
finished=true is only a native finish marker, NOT a successful tool/run result.
No automatic wait or replay.

Administrative request files (private owned regular JSON, <=4096 bytes) contain
original scope/request_id/epoch/action. No ID, epoch or profile version is generated
or replaced; keep the original file for reconciliation. Execution commands accept
only their matching CreateProfile/RestoreProfile/guarded Close/CancelSubmitted/Stop intent.
NativeSettled records native settlement, NOT current listener/process exit or peer delivery.
Cancellation targets original submitted_request_id, never an observer ticket or a new
input. CancelSignalled (including false) records only the signal decision, NOT settlement.
No automatic retry. Administrative exit codes: 0 recorded success, 1 definite failure/not sent, 2 usage,
3 unresolved (missing/prepared/interrupted/uncertain, lost reply or mutation output).
A historical success is not proof of current residency or process exit.

Unix only. serve defaults to the foreground. SIGINT/SIGTERM request settlement.
IPC: status/list/inspect, Agent-scoped receipt queries and text submit to loaded
Agents. Optional administration adds allowed-profile discovery, full-intent
receipts, profile create/restore, guarded close, stable-input cancellation and Stop.
Legacy ticket Cancel and bare Stop remain unavailable.
Agent assembly uses only explicit private profiles and settings.
Config (0600 file, private owned parent; no token CLI flag or env fallback):
  directory, socket_path, host_id (UUID), token (32..128 URL-safe ASCII),
  host (optional HostConfig; max_agents supported, timeout policies rejected),
  mode (foreground|daemon, default foreground),
  recovery (optional object; restore_on_start/restart_on_crash, both default false).
    On macOS, daemon starts a detached supervisor and waits for the Host to be ready.
    restart_on_crash requires daemon mode. Accepted Stop suppresses restart.
    restore_on_start loads the saved Persistent member set and explicit Agents.
  agents (optional list of absolute private Agent profile paths, serve only),
  administration (optional private policy; fixed native authority, journaled IPC):
    scope (fixed UUID namespace), allowed_agents (UUID list, default empty),
    permissions (create/restore/cancel/close/stop list, default empty),
    profiles (dynamic-only private profile paths, default empty), max_jobs (1..64, default 8).
  Startup and dynamic profiles share one immutable set, with no duplicate identities
  or Session aliases. Every allowed Agent must be in that set. Recovery also
  requires a matching private profile and restore authority for dynamic members.
  max_connections (default 8),
  handshake_timeout_ms (default 5000), request_timeout_ms (default 30000),
  client_timeout_ms (default 5000). All paths must be absolute.
Agent profile (0600 absolute JSON, deny_unknown_fields):
  agent{id,name,cwd,session{id,path},model{provider,model},tools},
  runtime{providers...}, providerValues, toolEnvironment (absolute HOME),
  shell (absolute), approvedTools, thinking, timeoutMs, maxInputBytes.
  The daemon creates a new Session only when the path is absent, registers the
  Agent if needed, then performs the ordinary strict restore.
The token can query the catalog and submit text to loaded Agents (their
preapproved tools may run): keep it out of model context and tool arguments.";

pub(crate) async fn run(argv: &[String]) -> i32 {
    if (argv.len() == 1 && matches!(argv[0].as_str(), "--help" | "-h"))
        || (argv.len() == 2 && matches!(argv[1].as_str(), "--help" | "-h"))
    {
        println!("{HELP}");
        return 0;
    }
    #[cfg(unix)]
    {
        unix::run(argv).await
    }
    #[cfg(not(unix))]
    {
        let _ = argv;
        eprintln!("error: shared Host control is not supported on this platform");
        1
    }
}

#[cfg(unix)]
#[path = "cli_administration.rs"]
mod administration_cli;

#[cfg(unix)]
#[path = "cli_submission.rs"]
mod submission_cli;

#[cfg(unix)]
mod unix {
    use super::{administration_cli, submission_cli, HELP};
    #[cfg(target_os = "macos")]
    use crate::host::supervisor;
    use crate::host::{
        administration::AdministrativePermissions,
        client::{ConnectionConfig, HostClient},
        daemon::{DaemonAdministrationConfig, HostDaemon},
        launch_policy::{HostRunMode, LaunchPolicy, RecoveryPolicy},
        protocol::{AgentId, AuthToken, Command, HostId, MAX_PAGE_SIZE},
        recovery::{self, Bindings},
        server::ControlConfig,
        AgentId as NativeAgentId, ControlScopeId, HostConfig,
    };
    use serde::Deserialize;
    use std::{
        collections::{BTreeMap, BTreeSet},
        num::NonZeroUsize,
        path::PathBuf,
        time::Duration,
    };

    const MAX_CONFIG_BYTES: u64 = 64 * 1024;
    fn connections() -> usize {
        8
    }
    fn handshake() -> u64 {
        5000
    }
    fn request_timeout() -> u64 {
        30_000
    }
    fn administrative_jobs() -> usize {
        8
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum FileAdministrativePermission {
        Create,
        Restore,
        Cancel,
        Close,
        Stop,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FileAdministration {
        scope: ControlScopeId,
        #[serde(default)]
        allowed_agents: Vec<NativeAgentId>,
        #[serde(default)]
        permissions: Vec<FileAdministrativePermission>,
        #[serde(default)]
        profiles: Vec<PathBuf>,
        #[serde(default = "administrative_jobs")]
        max_jobs: usize,
    }
    impl FileAdministration {
        fn valid(&self, startup_count: usize) -> bool {
            (1..=crate::host::administration::MAX_ADMINISTRATIVE_JOBS).contains(&self.max_jobs)
                && startup_count.saturating_add(self.profiles.len())
                    <= crate::host::MAX_TRUSTED_PROFILES
                && self.allowed_agents.len() <= crate::host::MAX_TRUSTED_PROFILES
                && self
                    .allowed_agents
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    == self.allowed_agents.len()
                && self.permissions.len() <= 5
                && self
                    .permissions
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    == self.permissions.len()
                && self.profiles.iter().all(|path| path.is_absolute())
        }
        fn native(
            &self,
            profiles: Vec<crate::host::HostStartupAgent>,
        ) -> Result<DaemonAdministrationConfig, &'static str> {
            let mut permissions = AdministrativePermissions::default();
            for permission in &self.permissions {
                match permission {
                    FileAdministrativePermission::Create => permissions.create = true,
                    FileAdministrativePermission::Restore => permissions.restore = true,
                    FileAdministrativePermission::Cancel => permissions.cancel = true,
                    FileAdministrativePermission::Close => permissions.close = true,
                    FileAdministrativePermission::Stop => permissions.stop = true,
                }
            }
            Ok(DaemonAdministrationConfig {
                scope: self.scope,
                allowed_agents: self.allowed_agents.iter().copied().collect(),
                permissions,
                max_jobs: NonZeroUsize::new(self.max_jobs)
                    .ok_or("invalid Host administrative config")?,
                profiles,
            })
        }
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FileConfig {
        directory: PathBuf,
        socket_path: PathBuf,
        host_id: HostId,
        token: AuthToken,
        #[serde(default)]
        host: HostConfig,
        #[serde(default)]
        mode: HostRunMode,
        #[serde(default)]
        recovery: RecoveryPolicy,
        #[serde(default = "connections")]
        max_connections: usize,
        #[serde(default = "handshake")]
        handshake_timeout_ms: u64,
        #[serde(default = "request_timeout")]
        request_timeout_ms: u64,
        #[serde(default = "handshake")]
        client_timeout_ms: u64,
        /// Startup Agent profiles for `serve`, absolute private files.
        #[serde(default)]
        agents: Vec<PathBuf>,
        /// Owner policy only; never populated from a control request.
        #[serde(default)]
        administration: Option<FileAdministration>,
        /// Separate volatile Mini grant; absent means no Mini engine/capability.
        #[serde(default)]
        mini_control: Option<crate::host::mini_control::MiniControlConfig>,
    }
    impl FileConfig {
        fn launch_policy(&self) -> LaunchPolicy {
            LaunchPolicy {
                mode: self.mode,
                recovery: self.recovery,
            }
        }
        fn control(&self) -> ControlConfig {
            ControlConfig {
                socket_path: self.socket_path.clone(),
                host_id: self.host_id,
                token: self.token.clone(),
                max_connections: self.max_connections,
                handshake_timeout: Duration::from_millis(self.handshake_timeout_ms),
                request_timeout: Duration::from_millis(self.request_timeout_ms),
            }
        }
        fn valid(&self) -> bool {
            self.directory.is_absolute()
                && self.control().validate().is_ok()
                && self.host.validate().is_ok()
                && self.launch_policy().validate().is_ok()
                && (1..=300_000).contains(&self.client_timeout_ms)
                && self
                    .administration
                    .as_ref()
                    .is_none_or(|policy| policy.valid(self.agents.len()))
                && self
                    .mini_control
                    .as_ref()
                    .is_none_or(|policy| policy.valid())
        }
        fn connection(&self) -> ConnectionConfig {
            ConnectionConfig {
                socket_path: self.socket_path.clone(),
                host_id: self.host_id,
                token: self.token.clone(),
                timeout: Duration::from_millis(self.client_timeout_ms),
            }
        }
    }
    /// Read one explicitly named private Host input (daemon config or Agent
    /// profile). No fallback path, environment lookup or symlink following.
    pub(super) fn read_private_bytes(
        path: &std::path::Path,
        max: u64,
    ) -> Result<Vec<u8>, &'static str> {
        use std::{
            io::Read,
            os::unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let unsafe_file =
            "Host input must be a private owned regular file with a private owned parent";
        if !path.is_absolute() {
            return Err("Host input path must be absolute");
        }
        let parent = path.parent().ok_or(unsafe_file)?;
        let directory = std::fs::symlink_metadata(parent).map_err(|_| unsafe_file)?;
        let uid = nix::unistd::Uid::effective().as_raw();
        if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
            return Err(unsafe_file);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| unsafe_file)?;
        let metadata = file.metadata().map_err(|_| unsafe_file)?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() > max
        {
            return Err(unsafe_file);
        }
        let mut bytes = Vec::new();
        file.take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Host input read failed")?;
        if bytes.len() as u64 > max {
            return Err("Host input exceeds byte budget");
        }
        Ok(bytes)
    }
    fn parse_config(bytes: &[u8]) -> Result<FileConfig, &'static str> {
        let config: FileConfig =
            serde_json::from_slice(bytes).map_err(|_| "invalid Host config")?;
        if !config.valid() {
            return Err("invalid Host config");
        }
        Ok(config)
    }
    fn read_profiles(
        paths: &[PathBuf],
        config: &FileConfig,
        recovery: &mut Option<Bindings>,
    ) -> Result<Vec<crate::host::HostStartupAgent>, &'static str> {
        let mut agents = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = read_private_bytes(path, crate::host::MAX_PROFILE_BYTES)?;
            let profile: crate::host::AgentProfileFile =
                serde_json::from_slice(&bytes).map_err(|_| "invalid Agent profile")?;
            let agent = profile
                .into_startup()
                .map_err(|_| "invalid Agent profile")?;
            if let Some(bindings) = recovery.as_mut() {
                if agent.spec.mode == crate::host::AgentMode::Persistent {
                    let version = recovery::profile_version(
                        config.host_id,
                        &config.token,
                        &bytes,
                        &agent.profile,
                    );
                    if bindings.insert(agent.spec.id, version).is_some() {
                        return Err("duplicate Agent identity in recovery configuration");
                    }
                }
            }
            agents.push(agent);
        }
        Ok(agents)
    }
    enum Input {
        Command(Command),
        Request(administration_cli::Operation, PathBuf),
        Submission(submission_cli::Operation, PathBuf),
    }
    enum Expectation {
        Administrative(administration_cli::Expectation),
        Submission(submission_cli::Expectation),
    }
    impl Expectation {
        fn is_mutation(&self) -> bool {
            match self {
                Self::Administrative(value) => value.is_mutation(),
                Self::Submission(value) => value.is_mutation(),
            }
        }
        fn exit_code(&self, value: &serde_json::Value) -> Result<i32, ()> {
            match self {
                Self::Administrative(expected) => expected.exit_code(value),
                Self::Submission(expected) => expected.exit_code(value),
            }
        }
    }
    impl Input {
        fn load(self, host_id: HostId) -> Result<(Command, Option<Expectation>), &'static str> {
            match self {
                Self::Command(command) => Ok((command, None)),
                Self::Request(operation, path) => {
                    let command = administration_cli::read_request(operation, &path)?;
                    let expected = administration_cli::Expectation::for_command(&command)
                        .map(Expectation::Administrative);
                    Ok((command, expected))
                }
                Self::Submission(operation, path) => {
                    let (command, expected) =
                        submission_cli::read_request(operation, &path, host_id)?;
                    Ok((command, Some(Expectation::Submission(expected))))
                }
            }
        }
    }
    struct Invocation {
        config: PathBuf,
        query: Option<Input>,
    }
    fn parse(argv: &[String]) -> Result<Invocation, ()> {
        let Some(operation) = argv.first().map(String::as_str) else {
            return Err(());
        };
        let allowed: &[&str] = match operation {
            "serve" | "status" => &["--config"],
            "list" | "profiles" => &["--config", "--after", "--limit"],
            "inspect" => &["--config", "--agent"],
            value
                if administration_cli::Operation::parse(value).is_some()
                    || submission_cli::Operation::parse(value).is_some() =>
            {
                &["--config", "--request"]
            }
            _ => return Err(()),
        };
        let mut flags = BTreeMap::new();
        let mut arguments = argv[1..].iter();
        while let Some(argument) = arguments.next() {
            let (key, value) = match argument.split_once('=') {
                Some(pair) => pair,
                None => (argument.as_str(), arguments.next().ok_or(())?.as_str()),
            };
            if !allowed.contains(&key) || value.is_empty() || flags.insert(key, value).is_some() {
                return Err(());
            }
        }
        let config = PathBuf::from(*flags.get("--config").ok_or(())?);
        if !config.is_absolute() {
            return Err(());
        }
        if let Some(operation) = administration_cli::Operation::parse(operation) {
            let path = PathBuf::from(*flags.get("--request").ok_or(())?);
            if !path.is_absolute() {
                return Err(());
            }
            return Ok(Invocation {
                config,
                query: Some(Input::Request(operation, path)),
            });
        }
        if let Some(operation) = submission_cli::Operation::parse(operation) {
            let path = PathBuf::from(*flags.get("--request").ok_or(())?);
            if !path.is_absolute() {
                return Err(());
            }
            return Ok(Invocation {
                config,
                query: Some(Input::Submission(operation, path)),
            });
        }
        let query = match operation {
            "serve" => None,
            "status" => Some(Command::Status {}),
            "inspect" => Some(Command::Inspect {
                agent_id: AgentId::try_from(flags.get("--agent").ok_or(())?.to_string())
                    .map_err(|_| ())?,
            }),
            "list" | "profiles" => {
                let after = flags
                    .get("--after")
                    .map(|raw| AgentId::try_from(raw.to_string()))
                    .transpose()
                    .map_err(|_| ())?;
                let limit = flags
                    .get("--limit")
                    .map(|raw| raw.parse::<usize>())
                    .transpose()
                    .map_err(|_| ())?
                    .unwrap_or(MAX_PAGE_SIZE);
                if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                    return Err(());
                }
                Some(if operation == "profiles" {
                    Command::Profiles { after, limit }
                } else {
                    Command::List { after, limit }
                })
            }
            _ => return Err(()),
        };
        Ok(Invocation {
            config,
            query: query.map(Input::Command),
        })
    }
    fn write_json(value: &serde_json::Value) -> std::io::Result<()> {
        use std::io::Write;
        writeln!(std::io::stdout().lock(), "{value}")
    }

    pub(super) async fn run(argv: &[String]) -> i32 {
        #[cfg(target_os = "macos")]
        match argv.first().map(String::as_str) {
            Some(supervisor::SUPERVISOR) => {
                return internal_supervisor(&argv[1..])
                    .await
                    .unwrap_or_else(report_io);
            }
            Some(supervisor::WORKER) => {
                return internal_worker(&argv[1..]).await.unwrap_or_else(report_io);
            }
            _ => (),
        }
        let invocation = match parse(argv) {
            Ok(value) => value,
            Err(_) => {
                eprintln!("error: invalid Host command\n{HELP}");
                return 2;
            }
        };
        let path = invocation.config.clone();
        let config = match tokio::task::spawn_blocking(move || {
            parse_config(&read_private_bytes(&path, MAX_CONFIG_BYTES)?)
        })
        .await
        {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => {
                eprintln!("error: {error}");
                return 1;
            }
            Err(_) => {
                eprintln!("error: Host config task stopped");
                return 1;
            }
        };
        if let Some(input) = invocation.query {
            let host_id = config.host_id;
            let (command, expectation) =
                match tokio::task::spawn_blocking(move || input.load(host_id)).await {
                    Ok(Ok(value)) => value,
                    _ => {
                        eprintln!("error: invalid or unsafe Host control request");
                        return 2;
                    }
                };
            let mut client = match HostClient::connect(config.connection()).await {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("error: {error}");
                    return 1;
                }
            };
            return match client.call(command).await {
                Ok(value) => {
                    let code = match expectation
                        .as_ref()
                        .map(|expected| expected.exit_code(&value))
                        .transpose()
                    {
                        Ok(code) => code.unwrap_or(0),
                        Err(_) => {
                            eprintln!("error: invalid Host control response; retain the original request and query its receipt");
                            return 3;
                        }
                    };
                    if write_json(&value).is_err() {
                        eprintln!("error: Host output write failed; retain the original request before any retry");
                        if expectation
                            .as_ref()
                            .is_some_and(|expected| expected.is_mutation())
                        {
                            3
                        } else {
                            1
                        }
                    } else {
                        if code == 3 {
                            eprintln!("error: outcome unresolved; retain the original request and query its receipt, never replay with a new ID");
                        }
                        code
                    }
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    let code = if expectation.is_some() {
                        administration_cli::error_exit(error)
                    } else {
                        1
                    };
                    if code == 3 {
                        eprintln!("error: retain the complete original request and query its receipt; never replay with a new ID");
                    }
                    code
                }
            };
        }
        // Do not silently run foreground or partially recover when a valid
        // config asks for capabilities that have not been wired up yet. This
        // comes after the query branch, but before even reading Agent profiles.
        if let Err(error) = config.launch_policy().require_implemented() {
            eprintln!("error: {error}");
            return 1;
        }
        #[cfg(target_os = "macos")]
        if config.mode == HostRunMode::Daemon {
            return match supervisor::launch(&invocation.config).await {
                Ok(ready) => write_json(&ready.json())
                    .map(|()| 0)
                    .unwrap_or_else(report_io),
                Err(error) => report_io(error),
            };
        }
        start(
            config,
            #[cfg(target_os = "macos")]
            None,
        )
        .await
    }

    #[cfg(target_os = "macos")]
    fn report_io(error: std::io::Error) -> i32 {
        eprintln!("error: {error}");
        1
    }

    #[cfg(target_os = "macos")]
    async fn internal_supervisor(argv: &[String]) -> std::io::Result<i32> {
        let [flag, path] = argv else {
            return Err(std::io::Error::other("invalid detached Host invocation"));
        };
        if flag != "--config" {
            return Err(std::io::Error::other("invalid detached Host invocation"));
        }
        let channel = supervisor::adopt_channel()?;
        let bytes = read_private_bytes(std::path::Path::new(path), MAX_CONFIG_BYTES)
            .map_err(std::io::Error::other)?;
        let config = parse_config(&bytes).map_err(std::io::Error::other)?;
        if config.mode != HostRunMode::Daemon {
            return Err(std::io::Error::other("detached Host configuration changed"));
        }
        supervisor::detached(
            channel,
            config.directory,
            config.socket_path,
            bytes,
            config.recovery.restart_on_crash,
        )
        .await?;
        Ok(0)
    }

    #[cfg(target_os = "macos")]
    async fn internal_worker(argv: &[String]) -> std::io::Result<i32> {
        let [listener, lock] = argv else {
            return Err(std::io::Error::other(
                "invalid detached Host worker invocation",
            ));
        };
        let listener = listener.parse::<i32>().map_err(std::io::Error::other)?;
        let lock = lock.parse::<i32>().map_err(std::io::Error::other)?;
        let mut channel = supervisor::adopt_channel()?;
        let (identity, bytes) =
            supervisor::read_config(&mut channel, MAX_CONFIG_BYTES as usize).await?;
        let config = parse_config(&bytes).map_err(std::io::Error::other)?;
        if config.mode != HostRunMode::Daemon {
            return Err(std::io::Error::other(
                "invalid detached Host worker configuration",
            ));
        }
        let worker = supervisor::Worker::adopt(
            config.socket_path.clone(),
            identity,
            channel,
            listener,
            lock,
        )?;
        Ok(start(config, Some(worker)).await)
    }

    async fn start(
        config: FileConfig,
        #[cfg(target_os = "macos")] worker: Option<supervisor::Worker>,
    ) -> i32 {
        let mut recovery = config.recovery.restore_on_start.then(Bindings::new);
        let agents = match read_profiles(&config.agents, &config, &mut recovery) {
            Ok(agents) => agents,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
        let administration = match config
            .administration
            .as_ref()
            .map(|policy| {
                read_profiles(&policy.profiles, &config, &mut recovery)
                    .and_then(|profiles| policy.native(profiles))
            })
            .transpose()
        {
            Ok(administration) => administration,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
        serve(
            config,
            agents,
            administration,
            recovery,
            #[cfg(target_os = "macos")]
            worker,
        )
        .await
    }
    async fn serve(
        config: FileConfig,
        agents: Vec<crate::host::HostStartupAgent>,
        administration: Option<DaemonAdministrationConfig>,
        recovery: Option<Bindings>,
        #[cfg(target_os = "macos")] worker: Option<supervisor::Worker>,
    ) -> i32 {
        use tokio::signal::unix::{signal, SignalKind};
        #[cfg(target_os = "macos")]
        let (listener, mut supervisor) = match worker {
            Some(worker) => (Some(worker.listener), Some(worker.monitor)),
            None => (None, None),
        };
        #[cfg(not(target_os = "macos"))]
        let listener = None;
        // Diagnostics must not panic away a native owner when stderr is closed.
        fn diagnostic(message: impl std::fmt::Display) {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "error: {message}");
        }
        // Install before opening catalog/listener, not after publishing ready.
        let (Ok(mut interrupt), Ok(mut terminate)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            eprintln!("error: Host signal registration failed");
            return 1;
        };
        let control = config.control();
        let mut daemon = match HostDaemon::open_with_recovery(
            config.directory,
            config.host,
            control,
            agents,
            administration,
            config.mini_control,
            (recovery, listener),
        )
        .await
        {
            Ok(daemon) => daemon,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
        let ready = serde_json::json!({"event":"host_ready", "pid":std::process::id(), "control":daemon.welcome()});
        #[cfg(target_os = "macos")]
        let mut parent_lost = false;
        #[cfg(target_os = "macos")]
        let output_failed = match supervisor.as_mut() {
            Some(monitor) => {
                parent_lost = supervisor::publish(
                    &mut monitor.socket,
                    &supervisor::Ready {
                        pid: std::process::id(),
                        control: daemon.welcome().clone(),
                    },
                )
                .await
                .is_err();
                parent_lost
            }
            None => write_json(&ready).is_err(),
        };
        #[cfg(not(target_os = "macos"))]
        let output_failed = write_json(&ready).is_err();
        if output_failed {
            diagnostic("Host readiness output failed");
        }
        let mut listener_failed = false;
        let mut shutdown_now = output_failed;
        loop {
            if !shutdown_now {
                let parent_exit = async {
                    #[cfg(target_os = "macos")]
                    if let Some(monitor) = supervisor.as_mut() {
                        use tokio::io::AsyncReadExt;
                        if !parent_lost {
                            let _ = monitor.socket.read(&mut [0]).await;
                            return;
                        }
                    }
                    std::future::pending::<()>().await;
                };
                let stopping = tokio::select! {
                    _ = interrupt.recv() => true,
                    _ = terminate.recv() => true,
                    _ = parent_exit => {
                        #[cfg(target_os = "macos")]
                        { parent_lost = true; }
                        false
                    },
                    result = daemon.wait_control(), if !listener_failed => {
                        listener_failed = result.is_err() || !daemon.control_stopped_by_administration();
                        if listener_failed { diagnostic("Host control listener stopped"); }
                        false
                    },
                };
                #[cfg(target_os = "macos")]
                if let Some(monitor) = supervisor.as_mut() {
                    if stopping && !parent_lost {
                        use tokio::io::AsyncWriteExt;
                        parent_lost = monitor.socket.write_all(&[1]).await.is_err();
                    }
                }
                #[cfg(not(target_os = "macos"))]
                let _ = stopping;
            }
            shutdown_now = false;
            match daemon.shutdown().await {
                Ok(()) => {
                    #[cfg(target_os = "macos")]
                    if let Some(monitor) = supervisor.take() {
                        if nix::unistd::getppid().as_raw() != monitor.session {
                            loop {
                                match supervisor::settle(monitor.session).await {
                                    Ok(()) => break,
                                    Err(error) => {
                                        diagnostic(error);
                                        tokio::time::sleep(Duration::from_secs(1)).await;
                                    }
                                }
                            }
                            monitor.lease.remove();
                        }
                    }
                    return if listener_failed || output_failed {
                        1
                    } else {
                        0
                    };
                }
                Err(error) => {
                    diagnostic(error);
                    if daemon.service().is_closed() {
                        return 1;
                    }
                    // No exit/force-success while native owners are unsettled.
                    // The next signal retries; do not spin on a failed listener.
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn strict_commands_do_not_accept_mutations_secret_flags_or_ambient_paths() {
            let args = |values: &[&str]| {
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            };
            for values in [
                vec!["restore", "--config", "/private/c"],
                vec!["serve", "--config", "relative.json"],
                vec!["status", "--config", "/c", "--token", "not-allowed"],
                vec!["list", "--config", "/c", "--limit", "0"],
                vec!["list", "--config", "/c", "--limit", "129"],
                vec!["serve", "--config", "/c", "--config", "/other"],
                vec!["inspect", "--config", "/c"],
            ] {
                assert!(parse(&args(&values)).is_err());
            }
            assert!(parse(&args(&["list", "--config=/c", "--limit=2"])).is_ok());
        }
        #[test]
        fn administrative_commands_require_explicit_request_paths_and_never_accept_rewrites() {
            let args = |values: &[&str]| {
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
            };
            for operation in [
                "submit",
                "receipt",
                "administrative-receipt",
                "administrative-create",
                "administrative-restore",
                "administrative-close",
                "administrative-cancel",
                "administrative-stop",
            ] {
                assert!(parse(&args(&[operation, "--config", "/c", "--request", "/r"])).is_ok());
                for flags in [
                    vec!["--config", "/c"],
                    vec!["--config", "/c", "--request", "relative"],
                    vec!["--config", "/c", "--request", "/r", "--request-id", "new"],
                    vec!["--config", "/c", "--request", "/r", "--epoch", "new"],
                    vec![
                        "--config",
                        "/c",
                        "--request",
                        "/r",
                        "--submitted-request-id",
                        "new",
                    ],
                ] {
                    let mut values = vec![operation];
                    values.extend(flags);
                    assert!(parse(&args(&values)).is_err());
                }
            }
            assert!(parse(&args(&["profiles", "--config", "/c", "--limit", "1"])).is_ok());
            for limit in ["0", "129"] {
                assert!(parse(&args(&["profiles", "--config", "/c", "--limit", limit])).is_err());
            }
            for operation in ["create", "restore", "close", "cancel", "stop"] {
                assert!(parse(&args(&[operation, "--config", "/c", "--request", "/r"])).is_err());
            }
        }
        #[test]
        fn file_launch_policy_defaults_and_all_combinations_are_validated() {
            let base = serde_json::json!({
                "directory": "/private/host/state", "socket_path": "/private/host/s",
                "host_id": HostId::new(), "token": "a".repeat(32)
            });
            let old: FileConfig = serde_json::from_value(base.clone()).unwrap();
            assert!(old.valid());
            assert_eq!(old.launch_policy(), LaunchPolicy::default());
            assert!(old.launch_policy().require_implemented().is_ok());
            for mode in ["foreground", "daemon"] {
                for restore in [false, true] {
                    for restart in [false, true] {
                        let mut value = base.clone();
                        value["mode"] = serde_json::json!(mode);
                        value["recovery"] = serde_json::json!({
                            "restore_on_start": restore, "restart_on_crash": restart
                        });
                        let config: FileConfig = serde_json::from_value(value).unwrap();
                        assert_eq!(config.valid(), !restart || mode == "daemon");
                        assert_eq!(
                            config.launch_policy().require_implemented().is_ok(),
                            (!restart || mode == "daemon")
                                && (mode == "foreground" || cfg!(target_os = "macos"))
                        );
                    }
                }
            }
        }
        #[test]
        fn private_file_launch_fields_are_strict_not_flattened_or_coerced() {
            for extra in [
                serde_json::json!({"mode": "background"}),
                serde_json::json!({"mode": null}),
                serde_json::json!({"recovery": null}),
                serde_json::json!({"recovery": {"restartOnCrash": true}}),
                serde_json::json!({"recovery": {"restore_on_start": "false"}}),
                serde_json::json!({"daemon": true}),
            ] {
                let mut value = serde_json::json!({
                    "directory": "/private/host/state", "socket_path": "/private/host/s",
                    "host_id": HostId::new(), "token": "a".repeat(32)
                });
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                assert!(serde_json::from_value::<FileConfig>(value).is_err());
            }
        }
        #[test]
        fn private_administrative_policy_defaults_deny_without_minting_peer_grants() {
            let policy: FileAdministration =
                serde_json::from_value(serde_json::json!({"scope": ControlScopeId::new()}))
                    .unwrap();
            assert!(policy.valid(0));
            let native = policy.native(vec![]).unwrap();
            assert!(native.allowed_agents.is_empty());
            assert_eq!(native.max_jobs.get(), 8);
            let rights = native.permissions;
            assert!(
                !rights.create
                    && !rights.restore
                    && !rights.cancel
                    && !rights.close
                    && !rights.stop
            );
            assert!(serde_json::from_value::<FileAdministration>(
                serde_json::json!({"scope": ControlScopeId::new(), "grant": {"stop": true}})
            )
            .is_err());
        }
    }
}
