//! Resolve and join the native Host selected by a local area name.

use super::{
    area_entry::valid_name,
    cli::unix::{
        decode_startup_profile, parse_config, read_area_settings, read_private_bytes,
        validate_named_profiles, FileConfig, NamedAgentProfile, MAX_CONFIG_BYTES,
    },
    client::{ConnectionConfig, HostClient},
    launch_policy::RecoveryPolicy,
    protocol::{AgentId, Command},
    registry, supervisor, CatalogError, HostConfig, Timeouts,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    num::{NonZeroU64, NonZeroUsize},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Settings {
    default_area: String,
    #[serde(default)]
    default_terminal: bool,
    #[serde(default)]
    areas: BTreeMap<String, AreaSettings>,
    #[serde(default)]
    restore_areas: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AreaSettings {
    provider: Option<String>,
    model: Option<String>,
    tools: Option<Vec<String>>,
    max_agents: Option<NonZeroUsize>,
    idle_timeout_ms: Option<NonZeroU64>,
    run_timeout_ms: Option<NonZeroU64>,
    disconnected_timeout_ms: Option<NonZeroU64>,
    #[serde(default)]
    restore_on_attach: bool,
    shutdown_grace_ms: Option<NonZeroU64>,
    #[serde(default)]
    agent_profiles: Vec<NamedAgentProfile>,
    #[serde(default = "default_area_recovery")]
    recovery: RecoveryPolicy,
}

impl AreaSettings {
    fn host_config(&self) -> HostConfig {
        HostConfig {
            max_agents: self.max_agents,
            default_timeouts: Timeouts {
                idle_ms: self.idle_timeout_ms,
                run_ms: self.run_timeout_ms,
                disconnected_ms: self.disconnected_timeout_ms,
                shutdown_grace_ms: self.shutdown_grace_ms,
            },
            ..Default::default()
        }
    }
}

fn default_area_recovery() -> RecoveryPolicy {
    RecoveryPolicy {
        restore_on_start: true,
        restart_on_crash: true,
    }
}

impl Default for AreaSettings {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            tools: None,
            max_agents: None,
            idle_timeout_ms: None,
            run_timeout_ms: None,
            disconnected_timeout_ms: None,
            restore_on_attach: false,
            shutdown_grace_ms: None,
            agent_profiles: Vec::new(),
            recovery: default_area_recovery(),
        }
    }
}

#[derive(Deserialize)]
struct Member {
    agent: Agent,
}

#[derive(Deserialize)]
struct Agent {
    id: AgentId,
    cwd: PathBuf,
    session: Session,
}

#[derive(Deserialize)]
struct Session {
    id: String,
    path: PathBuf,
}

pub(super) struct Connection {
    pub name: String,
    pub client: HostClient,
    pub agent_id: AgentId,
    pub session_id: String,
    pub session_path: PathBuf,
    pub config_path: PathBuf,
    pub control: ConnectionConfig,
}

fn publish(path: &Path, value: &Value) -> io::Result<()> {
    let directory = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path)?;
    fs::File::open(directory)?.sync_all()
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn settings(root: &Path) -> Result<Settings> {
    let path = root.join("host.json");
    if !exists(&path).map_err(|e| e.to_string())? {
        let initial = json!({"defaultArea":"default", "areas":{"default":{}}});
        match publish(&path, &initial) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(format!("could not initialize Host settings: {error}")),
        }
    }
    parse_settings(&read_area_settings(&path)?)
}

fn parse_settings(bytes: &[u8]) -> Result<Settings> {
    let shape: Value = serde_json::from_slice(bytes).map_err(|_| "invalid Host settings")?;
    if !shape.is_object()
        || shape.get("areas").is_some_and(|areas| {
            areas
                .as_object()
                .is_none_or(|areas| areas.values().any(|area| !area.is_object()))
        })
    {
        return Err("Host settings and each area must be JSON objects".into());
    }
    let settings: Settings = serde_json::from_slice(bytes).map_err(|_| "invalid Host settings")?;
    if !valid_name(&settings.default_area) || !settings.areas.keys().all(|key| valid_name(key)) {
        return Err("invalid area name in Host settings".into());
    }
    let mut targets = BTreeSet::new();
    for name in &settings.restore_areas {
        if !valid_name(name) || !settings.areas.contains_key(name) {
            return Err("restoreAreas must name areas declared in Host settings".into());
        }
        if !targets.insert(name) {
            return Err(format!("duplicate area {name} in restoreAreas"));
        }
        if !settings.areas[name].recovery.restore_on_start {
            return Err(format!(
                "area {name} in restoreAreas requires restore_on_start"
            ));
        }
    }
    for (name, area) in &settings.areas {
        area.host_config()
            .validate()
            .map_err(|error| format!("area {name}: {error}"))?;
        if !validate_named_profiles(&area.agent_profiles, 1, area.max_agents) {
            return Err(format!("invalid agentProfiles or capacity in area {name}"));
        }
        if area
            .provider
            .iter()
            .chain(area.model.iter())
            .chain(area.tools.iter().flatten())
            .any(|value| value.trim().is_empty())
        {
            return Err(format!(
                "area {name} contains an empty provider, model, or tool name"
            ));
        }
    }
    Ok(settings)
}

// The owner can use the usual file and environment credentials. Secret
// commands and project-local configuration are outside the generated profile.
struct EnvironmentValues;
impl rpi_runtime::ValueSource for EnvironmentValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

fn freeze_value(values: &mut BTreeMap<String, String>, name: String, value: String) -> String {
    let reference = format!("${{{name}}}");
    values.insert(name, value);
    reference
}

fn profile(cwd: &Path, opts: &AreaSettings, directory: &Path) -> Result<Value> {
    let settings = crate::settings::load_settings().map_err(|_| "could not load model settings")?;
    let mut runtime = rpi_runtime::RuntimeConfig {
        models: crate::config::load_models_config().map_err(|_| "could not load models.json")?,
        credentials: crate::config::read_auth().map_err(|_| "could not load auth.json")?,
        settings: crate::provider::runtime_settings(settings),
    };
    let env = EnvironmentValues;
    let resolved = runtime
        .resolve(
            &rpi_runtime::ModelRequest {
                provider: opts.provider.as_deref(),
                model: opts.model.as_deref(),
                ..Default::default()
            },
            &Default::default(),
            &env,
        )
        .map_err(|e| e.to_string())?;
    let model = resolved.model;
    let provider = &model.provider;
    let mut config = runtime
        .models
        .providers
        .shift_remove(provider)
        .unwrap_or_else(|| rpi_runtime::config::ProviderConfig {
            name: None,
            base_url: None,
            api_key: None,
            api: None,
            headers: None,
            auth_header: None,
            models: Vec::new(),
        });
    if matches!(
        runtime.credentials.get(provider),
        Some(rpi_runtime::config::Credential::Oauth { .. })
    ) {
        return Err(
            "automatic Host profiles require a non-OAuth provider; choose one in host.json".into(),
        );
    }
    let credential = rpi_runtime::credentials::resolve(
        provider,
        Some(&config),
        &runtime.credentials,
        None,
        &env,
    );
    config.api = Some(model.api.as_str().into());
    config.base_url = Some(model.base_url.clone());
    let mut values: BTreeMap<_, _> = [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ]
    .into_iter()
    .filter_map(|name| Some((name.to_owned(), std::env::var(name).ok()?)))
    .collect();
    config.api_key = credential
        .key
        .map(|key| freeze_value(&mut values, "RPI_HOST_PROFILE_KEY".into(), key));
    config.auth_header = Some(credential.bearer);
    // Keep the selected model and its effective headers. Referencing frozen
    // literals avoids treating a dollar sign inside a credential as a template.
    config.headers = None;
    let headers = model.headers.clone().map(|headers| {
        headers
            .into_iter()
            .enumerate()
            .map(|(index, (name, value))| {
                (
                    name,
                    freeze_value(
                        &mut values,
                        format!("RPI_HOST_PROFILE_HEADER_{index}"),
                        value,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>()
    });
    config.models = vec![serde_json::from_value(json!({
        "id":model.id,"name":model.name,"baseUrl":model.base_url,
        "reasoning":model.reasoning,"thinkingLevelMap":model.thinking_level_map,
        "contextWindow":model.context_window,"maxTokens":model.max_tokens,
        "cost":model.has_pricing().then_some(model.cost),"input":model.input,
        "headers":headers,"compat":model.compat,
    }))
    .map_err(|_| "could not freeze selected Host model")?];
    runtime.models.providers.clear();
    runtime.models.providers.insert(provider.clone(), config);
    let home = dirs::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or("could not resolve HOME")?;
    let tools = opts.tools.clone().unwrap_or_else(|| vec!["read".into()]);
    let id = AgentId::new();
    let session_id = Uuid::new_v4().to_string();
    let path = directory.join(format!("{session_id}.jsonl"));
    Ok(json!({
        "agent":{"id": id, "name":"agent", "cwd":cwd,
            "session":{"id":session_id,"path":path},
            "model":{"provider":provider,"model":model.id}, "tools":tools},
        "runtime":runtime.models, "providerValues":values,
        "toolEnvironment":{"HOME":home,"PATH":"/usr/bin:/bin:/usr/sbin:/sbin"},
        "shell":"/bin/sh", "approvedTools":tools, "thinking":resolved.thinking_level,
    }))
}

async fn lock(directory: &Path) -> Result<registry::InstanceLock> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(310);
    loop {
        match registry::own_directory_lock(directory, "entry.lock") {
            Ok(lock) => return Ok(lock),
            Err(CatalogError::AlreadyRunning) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(format!("could not coordinate area startup: {error}")),
        }
    }
}

fn has_listener(directory: &Path, socket: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(socket) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not inspect area endpoint: {error}")),
    };
    if !metadata.file_type().is_socket()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("unsafe area endpoint".into());
    }
    let _supervisor = match registry::own_directory_lock(directory, "supervisor.lock") {
        Ok(lock) => lock,
        Err(CatalogError::AlreadyRunning) => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    let _worker = match registry::own_directory_lock(directory, "instance.lock") {
        Ok(lock) => lock,
        Err(CatalogError::AlreadyRunning) => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(socket)
                .map_err(|e| format!("could not remove stale area endpoint: {e}"))?;
            Ok(false)
        }
        Err(error) => Err(format!("could not assess area endpoint: {error}")),
    }
}

async fn connect(directory: &Path, config_path: &Path, config: &FileConfig) -> Result<HostClient> {
    if exists(&config.socket_path).map_err(|e| e.to_string())? {
        match HostClient::connect(config.connection()).await {
            Ok(client) => return Ok(client),
            Err(error) if has_listener(directory, &config.socket_path)? => {
                return Err(format!("could not join the running area: {error}"));
            }
            Err(_) => (),
        }
    }
    supervisor::launch(config_path)
        .await
        .map_err(|error| format!("could not start area Host: {error}"))?;
    HostClient::connect(config.connection())
        .await
        .map_err(|error| format!("could not authenticate to the new area Host: {error}"))
}

fn private_hosts(root: &Path) -> Result<PathBuf> {
    let hosts = root.join("hosts");
    let metadata = fs::symlink_metadata(&hosts)
        .map_err(|error| format!("could not inspect Host directory: {error}"))?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Host directory must be a private owned directory".into());
    }
    Ok(hosts)
}

fn restore_targets(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let settings = parse_settings(&read_area_settings(&root.join("host.json"))?)?;
    if settings.restore_areas.is_empty() {
        return Ok(Vec::new());
    }
    let hosts = private_hosts(root)?;
    let mut targets = Vec::with_capacity(settings.restore_areas.len());
    for name in settings.restore_areas {
        let directory = hosts.join(&name);
        read_restore_config(&name, &directory)?;
        targets.push((name, directory));
    }
    Ok(targets)
}

fn read_restore_config(name: &str, directory: &Path) -> Result<FileConfig> {
    let config = read_private_bytes(&directory.join("host.json"), MAX_CONFIG_BYTES)
        .and_then(|bytes| parse_config(&bytes))
        .map_err(|error| format!("area {name}: {error}"))?;
    config
        .validate_area_restore(directory)
        .map_err(|error| format!("area {name}: {error}"))?;
    Ok(config)
}

async fn restore_area(name: &str, directory: &Path) -> Result<Value> {
    let _entry_lock = lock(directory).await?;
    let config = read_restore_config(name, directory)?;
    let client = connect(directory, &directory.join("host.json"), &config).await?;
    let host = client.welcome();
    Ok(json!({"area":name, "hostId":host.host_id, "epoch":host.epoch}))
}

pub(super) async fn restore_configured_areas() -> Result<bool> {
    let root = crate::config::agent_dir().map_err(|error| error.to_string())?;
    let targets = restore_targets(&root)?;
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for (name, directory) in targets {
        match restore_area(&name, &directory).await {
            Ok(result) => restored.push(result),
            Err(error) => failed.push(json!({"area":name, "error":error})),
        }
    }
    let all_ready = failed.is_empty();
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &json!({"ready":restored, "failed":failed}))
        .map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    Ok(all_ready)
}

fn read_agent(path: &Path) -> Result<Agent> {
    let bytes = read_private_bytes(path, super::MAX_PROFILE_BYTES)?;
    let Member { agent } =
        serde_json::from_slice(&bytes).map_err(|_| "invalid area Agent profile")?;
    Ok(agent)
}

fn check_workspace(agent: &Agent, cwd: &Path, area: &str) -> Result<()> {
    if agent.cwd != cwd {
        return Err(format!(
            "the selected Agent in area {area} belongs to workspace {}; open it there",
            agent.cwd.display()
        ));
    }
    Ok(())
}

fn validate_area_members(
    default_profile: &Path,
    primary: &Agent,
    profiles: &[NamedAgentProfile],
) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::from([primary.id.to_string()]);
    let mut sessions = BTreeSet::from([primary.session.id.clone()]);
    let mut paths = BTreeSet::from([primary.session.path.clone()]);
    for profile in profiles {
        if profile.path == default_profile {
            return Err(format!(
                "Agent {} references the default profile",
                profile.name
            ));
        }
        let startup = read_private_bytes(&profile.path, super::MAX_PROFILE_BYTES)
            .and_then(|bytes| decode_startup_profile(&bytes))
            .map_err(|error| format!("Agent {}: {error}", profile.name))?;
        let session = startup.spec.session.ok_or("missing Persistent Session")?;
        if !ids.insert(startup.spec.id.to_string())
            || !sessions.insert(session.id)
            || !paths.insert(session.path)
        {
            return Err(format!(
                "Agent {} duplicates an Agent or Session identity",
                profile.name
            ));
        }
    }
    Ok(ids)
}

fn default_terminal_area(root: &Path, cwd: &Path) -> Option<(String, PathBuf)> {
    let settings = parse_settings(&read_area_settings(&root.join("host.json")).ok()?).ok()?;
    if !settings.default_terminal {
        return None;
    }
    let directory = private_hosts(root).ok()?.join(&settings.default_area);
    let agent = read_agent(&directory.join("agent.json")).ok()?;
    if agent.cwd != cwd || !exists(&directory.join("host.json")).ok()? {
        return None;
    }
    Some((settings.default_area, directory))
}

pub(super) async fn join_default_terminal() -> Result<Option<Connection>> {
    let Ok(root) = crate::config::agent_dir() else {
        return Ok(None);
    };
    let Ok(cwd) = std::env::current_dir().and_then(|path| path.canonicalize()) else {
        return Ok(None);
    };
    let Some((name, directory)) = default_terminal_area(&root, &cwd) else {
        return Ok(None);
    };
    let _entry_lock = lock(&directory).await?;
    let config_path = directory.join("host.json");
    let config = parse_config(&read_private_bytes(&config_path, MAX_CONFIG_BYTES)?)?;
    config.validate_area_daemon(&directory)?;
    let agent = read_agent(&directory.join("agent.json"))?;
    check_workspace(&agent, &cwd, &name)?;
    finish_join(name, None, config_path, config, agent)
        .await
        .map(Some)
}

pub(super) async fn join(
    requested: Option<String>,
    member: Option<&str>,
    cwd: PathBuf,
) -> Result<Connection> {
    let root = crate::config::agent_dir().map_err(|e| e.to_string())?;
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)
        .map_err(|e| format!("could not create the agent directory: {e}"))?;
    let metadata = fs::symlink_metadata(&root).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err("agent directory must be user-owned and writable only by its owner".into());
    }
    let hosts = root.join("hosts");
    let root_lock = lock(&hosts).await?;
    let mut settings = settings(&root)?;
    drop(root_lock);
    let name = requested.unwrap_or(settings.default_area);
    if !valid_name(&name) {
        return Err("area name must be 1–32 lowercase ASCII letters, digits, hyphens or underscores and start with a letter or digit".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("could not resolve workspace: {e}"))?;
    let directory = hosts.join(&name);
    let _entry_lock = lock(&directory).await?;
    let profile_path = directory.join("agent.json");
    let config_path = directory.join("host.json");
    let member = member.filter(|name| *name != "agent");
    if !exists(&config_path).map_err(|e| e.to_string())? {
        if member.is_some() {
            return Err(format!(
                "initialize area {name} from its default Agent workspace first"
            ));
        }
        let opts = settings.areas.remove(&name).unwrap_or_default();
        let generated = if exists(&profile_path).map_err(|e| e.to_string())? {
            None
        } else {
            let profile = profile(&cwd, &opts, &directory)?;
            let bytes = serde_json::to_vec_pretty(&profile)
                .map_err(|_| "could not encode Agent profile")?;
            if bytes.len() as u64 >= super::MAX_PROFILE_BYTES {
                return Err("generated Host Agent profile exceeds its byte budget".into());
            }
            decode_startup_profile(&bytes)?;
            Some(profile)
        };
        let primary = if let Some(profile) = &generated {
            let Member { agent } = serde_json::from_value(profile.clone())
                .map_err(|_| "invalid generated Host Agent profile")?;
            agent
        } else {
            read_agent(&profile_path)?
        };
        let members = validate_area_members(&profile_path, &primary, &opts.agent_profiles)?;
        check_workspace(&primary, &cwd, &name)?;
        let (allowed_agents, permissions) = if opts.idle_timeout_ms.is_some()
            || opts.disconnected_timeout_ms.is_some()
            || opts.restore_on_attach
        {
            (members, vec!["cancel", "stop", "restore"])
        } else {
            (
                BTreeSet::from([primary.id.to_string()]),
                vec!["cancel", "stop"],
            )
        };
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let config = json!({
            "directory":directory, "socket_path":directory.join("control.sock"),
            "host_id":super::protocol::HostId::new(), "token":token, "mode":"daemon",
            "agents":[profile_path], "max_connections":32, "session_rpc_agent":primary.id,
            "named_agent_profiles":opts.agent_profiles,
            "restore_on_attach":opts.restore_on_attach,
            "host":opts.host_config(),
            "recovery":{"restore_on_start":opts.recovery.restore_on_start,
                "restart_on_crash":opts.recovery.restart_on_crash},
            "administration":{"scope":super::ControlScopeId::new(),"allowed_agents":allowed_agents,
                "permissions":permissions},
        });
        let bytes = serde_json::to_vec_pretty(&config)
            .map_err(|_| "could not encode area Host configuration")?;
        if bytes.len() as u64 >= MAX_CONFIG_BYTES {
            return Err("generated area Host configuration exceeds its byte budget".into());
        }
        parse_config(&bytes)?;
        if let Some(profile) = generated {
            publish(&profile_path, &profile)
                .map_err(|e| format!("could not save area Agent profile: {e}"))?;
        }
        publish(&config_path, &config)
            .map_err(|e| format!("could not save area Host configuration: {e}"))?;
    }
    let config = parse_config(&read_private_bytes(&config_path, MAX_CONFIG_BYTES)?)?;
    let profile_path = match member {
        Some(alias) => config
            .named_profile_path(alias)
            .ok_or_else(|| format!("unknown Agent {alias} in area {name}"))?,
        None => &profile_path,
    };
    let agent = read_agent(profile_path)?;
    check_workspace(&agent, &cwd, &name)?;
    finish_join(name, member, config_path, config, agent).await
}

async fn finish_join(
    name: String,
    member: Option<&str>,
    config_path: PathBuf,
    config: FileConfig,
    agent: Agent,
) -> Result<Connection> {
    let directory = config_path.parent().ok_or("missing area directory")?;
    let mut client = connect(directory, &config_path, &config).await?;
    if let Some(alias) = member {
        let spec = client
            .call(Command::Inspect { agent_id: agent.id })
            .await
            .map_err(|error| format!("could not inspect Agent {alias} in area {name}: {error}"))?;
        if spec["spec"]["session"]["id"] != agent.session.id
            || spec["spec"]["session"]["path"].as_str() != agent.session.path.to_str()
            || spec["spec"]["launch"]["cwd"].as_str() != agent.cwd.to_str()
        {
            return Err(format!(
                "Agent {alias} does not match the Session loaded in area {name}"
            ));
        }
    }
    Ok(Connection {
        name,
        client,
        agent_id: agent.id,
        session_id: agent.session.id,
        session_path: agent.session.path,
        config_path,
        control: config.connection(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_area_capacity_and_recovery_preserve_existing_defaults() {
        let settings = parse_settings(
            br#"{"defaultArea":"default","areas":{
            "default":{},
            "work":{"maxAgents":3,"idleTimeoutMs":120,"runTimeoutMs":250,"disconnectedTimeoutMs":180,"restoreOnAttach":true,"shutdownGraceMs":350,"recovery":{"restore_on_start":true,"restart_on_crash":false}},
            "isolated":{"recovery":{}}
        }}"#,
        )
        .unwrap();
        assert_eq!(settings.areas["default"].recovery, default_area_recovery());
        assert_eq!(settings.areas["default"].max_agents, None);
        assert!(!settings.areas["default"].restore_on_attach);
        assert!(settings.areas["work"].restore_on_attach);
        assert_eq!(
            settings.areas["default"].host_config().default_timeouts,
            Timeouts::default()
        );
        assert_eq!(settings.areas["work"].max_agents, NonZeroUsize::new(3));
        let timeouts = settings.areas["work"].host_config().default_timeouts;
        assert_eq!(timeouts.idle_ms, NonZeroU64::new(120));
        assert_eq!(timeouts.disconnected_ms, NonZeroU64::new(180));
        assert_eq!(
            settings.areas["work"].host_config().default_timeouts.run_ms,
            NonZeroU64::new(250)
        );
        assert_eq!(
            settings.areas["work"]
                .host_config()
                .default_timeouts
                .shutdown_grace_ms,
            NonZeroU64::new(350)
        );
        assert!(settings.areas["work"].recovery.restore_on_start);
        assert!(!settings.areas["work"].recovery.restart_on_crash);
        assert_eq!(
            settings.areas["isolated"].recovery,
            RecoveryPolicy::default()
        );
    }

    #[test]
    fn default_terminal_requires_enabled_settings_and_initialized_workspace() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path();
        let cwd = root_path.join("workspace");
        let settings = root_path.join("host.json");
        assert!(default_terminal_area(root_path, &cwd).is_none());
        publish(
            &settings,
            &json!({"defaultArea":"work", "defaultTerminal":true}),
        )
        .unwrap();
        assert!(default_terminal_area(root_path, &cwd).is_none());
        assert!(!root_path.join("hosts").exists());
        let directory = root_path.join("hosts/work");
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&directory)
            .unwrap();
        publish(
            &directory.join("agent.json"),
            &json!({"agent":{
                "id":AgentId::new(), "cwd":cwd,
                "session":{"id":"fixture","path":directory.join("session.jsonl")}
            }}),
        )
        .unwrap();
        assert!(default_terminal_area(root_path, &cwd).is_none());
        publish(&directory.join("host.json"), &json!({})).unwrap();
        assert!(default_terminal_area(root_path, &root_path.join("other")).is_none());
        assert_eq!(
            default_terminal_area(root_path, &cwd),
            Some(("work".into(), directory.clone()))
        );
        assert!(!directory.join("entry.lock").exists());
        fs::write(settings, br#"{"defaultArea":"work"}"#).unwrap();
        assert!(default_terminal_area(root_path, &cwd).is_none());
    }

    #[test]
    fn named_profiles_require_distinct_names_and_paths_within_capacity() {
        let review = json!({"name":"review", "path":"/private/review.json"});
        let test = json!({"name":"test", "path":"/private/test.json"});
        let settings = |profiles: Value, max: usize| {
            let settings = json!({"defaultArea":"work","areas":{"work":{
                "maxAgents":max,"agentProfiles":profiles
            }}});
            parse_settings(&serde_json::to_vec(&settings).unwrap())
        };
        assert!(settings(json!([review, test]), 3).is_ok());
        assert!(settings(json!([review, test]), 2).is_err());
        assert!(settings(json!([review, review]), 3).is_err());
        assert!(settings(
            json!([review, {"name":"test","path":"/private/review.json"}]),
            3
        )
        .is_err());
        assert!(settings(json!([{"name":"agent","path":"/private/other.json"}]), 3).is_err());
        assert!(settings(json!([{"name":"review","path":"review.json"}]), 3).is_err());
    }

    #[test]
    fn restore_targets_are_explicit_unique_and_restore_enabled() {
        let ok = br#"{"defaultArea":"default","areas":{"default":{},"work":{}},
            "restoreAreas":["work","default"]}"#;
        assert_eq!(
            parse_settings(ok).unwrap().restore_areas,
            ["work", "default"]
        );
        for invalid in [
            br#"{"defaultArea":"default","areas":{"work":{}},"restoreAreas":["work","work"]}"#.as_slice(),
            br#"{"defaultArea":"default","restoreAreas":["work"]}"#,
            br#"{"defaultArea":"default","areas":{"work":{"recovery":{}}},"restoreAreas":["work"]}"#,
            br#"{"defaultArea":"default","restoreAreas":null}"#,
        ] {
            assert!(parse_settings(invalid).is_err());
        }
    }

    #[test]
    fn restore_preflights_every_area_without_creating_missing_state() {
        let root = tempfile::tempdir().unwrap();
        let hosts = root.path().join("hosts");
        fs::DirBuilder::new().mode(0o700).create(&hosts).unwrap();
        let existing = hosts.join("work");
        fs::DirBuilder::new().mode(0o700).create(&existing).unwrap();
        let config = json!({
            "directory": existing, "socket_path":existing.join("control.sock"),
            "host_id":super::super::protocol::HostId::new(),
            "token":"test-token-with-at-least-thirty-two-bytes",
            "mode":"daemon", "recovery":{"restore_on_start":true}
        });
        publish(&existing.join("host.json"), &config).unwrap();
        let settings_file = root.path().join("host.json");
        std::fs::write(
            &settings_file,
            br#"{"defaultArea":"work","areas":{"work":{}},"restoreAreas":["work"]}"#,
        )
        .unwrap();
        assert_eq!(
            restore_targets(root.path()).unwrap(),
            [("work".into(), existing.clone())]
        );
        std::fs::write(settings_file, br#"{"defaultArea":"work","areas":{"work":{},"missing":{}},"restoreAreas":["work","missing"]}"#).unwrap();
        assert!(restore_targets(root.path())
            .unwrap_err()
            .starts_with("area missing:"));
        assert!(!hosts.join("missing").exists());
        assert!(!existing.join("entry.lock").exists());
        assert!(!existing.join("control.sock").exists());
    }

    #[test]
    fn rejects_invalid_policy_even_for_an_unselected_area() {
        for policy in [
            r#"[null,null,null,null,{}]"#,
            r#"{"maxAgents":0}"#,
            r#"{"idleTimeoutMs":0}"#,
            r#"{"idleTimeoutMs":31536000001}"#,
            r#"{"disconnectedTimeoutMs":0}"#,
            r#"{"disconnectedTimeoutMs":31536000001}"#,
            r#"{"runTimeoutMs":0}"#,
            r#"{"runTimeoutMs":31536000001}"#,
            r#"{"shutdownGraceMs":0}"#,
            r#"{"shutdownGraceMs":31536000001}"#,
            r#"{"maxAgents":"2"}"#,
            r#"{"recovery":[]}"#,
            r#"{"recovery":null}"#,
            r#"{"recovery":{"unknown":true}}"#,
            r#"{"recovery":{"restart_on_crash":"true"}}"#,
            r#"{"tools":["read"," "]}"#,
        ] {
            let bytes = format!(r#"{{"defaultArea":"default","areas":{{"unselected":{policy}}}}}"#);
            assert!(parse_settings(bytes.as_bytes()).is_err(), "{policy}");
        }
    }
}
