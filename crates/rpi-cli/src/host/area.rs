//! Resolve and join the native Host selected by a local area name.

use super::{
    cli::unix::{
        parse_config, read_area_settings, read_private_bytes, FileConfig, MAX_CONFIG_BYTES,
    },
    client::{ConnectionConfig, HostClient},
    protocol::AgentId,
    registry, supervisor, CatalogError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
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
    areas: BTreeMap<String, AreaSettings>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AreaSettings {
    provider: Option<String>,
    model: Option<String>,
    tools: Option<Vec<String>>,
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

fn valid_area(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && matches!(name.as_bytes()[0], b'a'..=b'z' | b'0'..=b'9')
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
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
    let text = read_area_settings(&path)?;
    let settings: Settings = serde_json::from_slice(&text).map_err(|_| "invalid Host settings")?;
    if !valid_area(&settings.default_area) || !settings.areas.keys().all(|key| valid_area(key)) {
        return Err("invalid area name in Host settings".into());
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

fn profile(cwd: &Path, opts: AreaSettings, directory: &Path) -> Result<Value> {
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
    let tools = opts.tools.unwrap_or_else(|| vec!["read".into()]);
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

pub(super) async fn join(requested: Option<String>, cwd: PathBuf) -> Result<Connection> {
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
    if !valid_area(&name) {
        return Err("area name must be 1–32 lowercase ASCII letters, digits, hyphens or underscores and start with a letter or digit".into());
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("could not resolve workspace: {e}"))?;
    let directory = hosts.join(&name);
    let _entry_lock = lock(&directory).await?;
    let profile_path = directory.join("agent.json");
    let config_path = directory.join("host.json");
    let configured = exists(&config_path).map_err(|e| e.to_string())?;
    if !configured && !exists(&profile_path).map_err(|e| e.to_string())? {
        let opts = settings.areas.remove(&name).unwrap_or_default();
        let profile = profile(&cwd, opts, &directory)?;
        if serde_json::to_vec_pretty(&profile)
            .map_err(|_| "could not encode Agent profile")?
            .len() as u64
            >= super::MAX_PROFILE_BYTES
        {
            return Err("generated Host Agent profile exceeds its byte budget".into());
        }
        serde_json::from_value::<super::AgentProfileFile>(profile.clone())
            .map_err(|_| "invalid generated Host Agent profile")?
            .into_startup()
            .map_err(|e| e.to_string())?;
        publish(&profile_path, &profile)
            .map_err(|e| format!("could not save area Agent profile: {e}"))?;
    }
    let Member { agent } = serde_json::from_slice(&read_private_bytes(
        &profile_path,
        super::MAX_PROFILE_BYTES,
    )?)
    .map_err(|_| "invalid area Agent profile")?;
    if agent.cwd != cwd {
        return Err(format!(
            "area {name} belongs to workspace {}; open it there or choose another area",
            agent.cwd.display()
        ));
    }
    if !configured {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let config = json!({
            "directory":directory, "socket_path":directory.join("control.sock"),
            "host_id":super::protocol::HostId::new(), "token":token, "mode":"daemon",
            "agents":[profile_path], "max_connections":32, "session_rpc_agent":agent.id,
            "recovery":{"restore_on_start":true,"restart_on_crash":true},
            "administration":{"scope":super::ControlScopeId::new(),"allowed_agents":[agent.id],
                "permissions":["cancel","stop"]},
        });
        parse_config(
            &serde_json::to_vec(&config).map_err(|_| "could not encode area Host configuration")?,
        )?;
        publish(&config_path, &config)
            .map_err(|e| format!("could not save area Host configuration: {e}"))?;
    }
    let config = parse_config(&read_private_bytes(&config_path, MAX_CONFIG_BYTES)?)?;
    let client = connect(&directory, &config_path, &config).await?;
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
