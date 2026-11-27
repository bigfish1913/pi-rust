//! Stdio adapter for clients that speak the existing native JSONL RPC protocol.
use super::{
    connect, host_directory, read_json, write_json, AgentHome, Args, OpenReply, OpenSession,
};
use crate::args::RpcMessageUpdates;
use rpi_harness::session::types::ForkPosition;
use serde::{Deserialize, Serialize};
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::time::timeout;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    provider: Option<String>,
    model: Option<String>,
    session: Option<String>,
    session_id: Option<String>,
    session_dir: Option<PathBuf>,
    fork: Option<String>,
    fork_entry: Option<String>,
    fork_at: Option<bool>,
    resource_config: Option<PathBuf>,
    resource_transition: Option<PathBuf>,
    trust_override: Option<bool>,
    no_session: bool,
    native_durable: bool,
    snapshots: bool,
    permission_gate: bool,
    bypass: bool,
}

impl Request {
    fn from_args(mut args: Args) -> Self {
        Self {
            provider: args.provider,
            model: args.model,
            session: args.session,
            session_id: args.session_id,
            session_dir: args.session_dir,
            fork: args.fork,
            fork_entry: args.fork_entry,
            fork_at: args.fork_position.map(|pos| pos == ForkPosition::At),
            resource_config: args.resource_config,
            resource_transition: args.resource_transition,
            trust_override: args.trust_override,
            no_session: args.no_session,
            native_durable: args.native_durable,
            snapshots: args.rpc_message_updates == RpcMessageUpdates::Snapshot,
            permission_gate: args.unknown_flags.remove("permission-gate").is_some(),
            bypass: args.unknown_flags.remove("bypass").is_some(),
        }
    }

    pub(super) async fn apply(self, args: &mut Args, cwd: PathBuf) -> PathBuf {
        args.provider = self.provider.or(args.provider.take());
        args.model = self.model.or(args.model.take());
        args.session = self.session;
        args.session_id = self.session_id;
        args.session_dir = self
            .session_dir
            .map(|dir| cwd.join(dir))
            .or(args.session_dir.take());
        args.fork = self.fork;
        args.fork_entry = self.fork_entry;
        args.fork_position = self.fork_at.map(|at| {
            if at {
                ForkPosition::At
            } else {
                ForkPosition::Before
            }
        });
        args.resource_config = self.resource_config.map(|path| cwd.join(path));
        args.resource_transition = self.resource_transition.map(|path| cwd.join(path));
        args.trust_override = self.trust_override;
        args.no_session = self.no_session;
        args.native_durable = self.native_durable;
        if self.snapshots {
            args.rpc_message_updates = RpcMessageUpdates::Snapshot;
        }
        if self.permission_gate {
            args.unknown_flags
                .insert("permission-gate".into(), true.into());
        }
        if self.bypass {
            args.unknown_flags.insert("bypass".into(), true.into());
        }
        // Freeze the storage root before adopting the recorded workspace, as
        // ordinary RPC startup does when reopening or forking a transcript.
        let session_root = crate::session::configured_session_dir(args, &cwd);
        args.session_dir = Some(session_root.clone());
        let selection = crate::session::select_session_in_dir(args, session_root);
        if let Some(restored) = crate::session::peek_restored_session_cwd(&selection, &cwd).await {
            crate::session::pin_restored_selection(args, &restored);
            if let Some(adopted) = restored.adopted {
                return adopted;
            }
        }
        cwd
    }
}

pub(super) async fn run_client(home: &AgentHome, area: &str, args: Args) -> Result<i32, String> {
    if !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("send Host RPC prompts over stdin".into());
    }
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let directory = host_directory(home, area)?;
    let (input, mut output) = connect(area, &directory).await?.into_split();
    let mut input = BufReader::new(input);
    write_json(
        &mut output,
        &OpenSession {
            version: super::VERSION,
            workspace,
            mini: None,
            terminal: None,
            rpc: Some(Box::new(Request::from_args(args))),
        },
    )
    .await?;
    let mut stdout = tokio::io::stdout();
    // Adapters opt in so existing stdio RPC clients still see `ready` first.
    if std::env::var_os("RPI_RPC_LOCAL_HOST_STARTUP").as_deref() == Some(std::ffi::OsStr::new("1"))
    {
        write_json(
            &mut stdout,
            &serde_json::json!({"type":"starting","protocol":"rpi","transport":"local-host","version":1}),
        )
        .await?;
        stdout.flush().await.map_err(|error| error.to_string())?;
    }
    let reply = timeout(Duration::from_secs(90), read_json::<OpenReply>(&mut input))
        .await
        .map_err(|_| "local Host RPC session startup timed out")??;
    match reply {
        Some(OpenReply::Opened { .. }) => {}
        Some(OpenReply::Error { message }) => return Err(message),
        _ => return Err("local Host closed before opening an RPC session".into()),
    }
    // A blocked stdin read must not hold Tokio shutdown open after the daemon
    // disconnects. The process can exit while this dedicated reader is blocked.
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("host-rpc-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buffer = [0; 8192];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        if runtime
                            .block_on(output.write_all(&buffer[..length]))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = runtime.block_on(output.shutdown());
        })
        .map_err(|error| error.to_string())?;
    tokio::io::copy(&mut input, &mut stdout)
        .await
        .map_err(|error| error.to_string())?;
    stdout.flush().await.map_err(|error| error.to_string())?;
    Ok(0)
}
