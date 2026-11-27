//! Early CLI entry for native Host areas.

const HELP: &str = "Usage: rpi --native-host [area] [--agent name] [--mode json|rpc]\n       rpi --native-host --restore-areas\n\nJoin the native Host for an area, starting it on first use. The default\narea is configured in $RPI_CODING_AGENT_DIR/host.json. macOS only.\nUse --mode json to report the selected Host and Session.\nUse --restore-areas to start the initialized daemons listed in restoreAreas.";

#[derive(Clone, Copy)]
enum Mode {
    Terminal,
    Json,
    Rpc,
}

pub(crate) async fn run(argv: &[String]) -> i32 {
    if matches!(argv, [flag, help] if flag == "--host" && matches!(help.as_str(), "-h" | "--help"))
    {
        println!("{HELP}");
        return 0;
    }
    if matches!(argv, [host, restore] if host == "--host" && restore == "--restore-areas") {
        #[cfg(target_os = "macos")]
        match super::area::restore_configured_areas().await {
            Ok(all_ready) => return i32::from(!all_ready),
            Err(error) => eprintln!("error: {error}"),
        }
        #[cfg(not(target_os = "macos"))]
        eprintln!("error: detached Host areas are currently supported on macOS");
        return 1;
    }
    let Some((name, agent, mode)) = parse(argv) else {
        eprintln!("{HELP}");
        return 2;
    };
    #[cfg(not(target_os = "macos"))]
    let result: Result<(), String> = {
        let _ = (name, agent, mode);
        Err("detached Host areas are currently supported on macOS".into())
    };
    #[cfg(target_os = "macos")]
    let result = enter(name, agent, mode).await;
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) async fn run_default_terminal() -> Option<i32> {
    let result = match super::area::join_default_terminal().await {
        Ok(Some(connection)) => super::area_terminal::run(connection, false).await,
        Ok(None) => return None,
        Err(error) => Err(error),
    };
    Some(match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    })
}

pub(super) fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && matches!(name.as_bytes()[0], b'a'..=b'z' | b'0'..=b'9')
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

fn parse(argv: &[String]) -> Option<(Option<String>, Option<String>, Mode)> {
    let (flag, mut rest) = argv.split_first()?;
    let area = if let Some(value) = flag.strip_prefix("--host=") {
        Some(value.to_owned())
    } else if let [name, tail @ ..] = rest {
        if name.starts_with('-') {
            None
        } else {
            rest = tail;
            Some(name.clone())
        }
    } else {
        None
    };
    let mut agent = None;
    let mut mode = Mode::Terminal;
    for option in rest.chunks(2) {
        match option {
            [flag, value] if flag == "--agent" && agent.is_none() && valid_name(value) => {
                agent = Some(value.clone());
            }
            [flag, value] if flag == "--mode" && matches!(mode, Mode::Terminal) => {
                mode = match value.as_str() {
                    "json" => Mode::Json,
                    "rpc" => Mode::Rpc,
                    _ => return None,
                };
            }
            _ => return None,
        }
    }
    Some((area, agent, mode))
}

#[cfg(target_os = "macos")]
async fn enter(name: Option<String>, agent: Option<String>, mode: Mode) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let connection = super::area::join(name, agent.as_deref(), cwd).await?;
    let host = connection.client.welcome();
    if matches!(mode, Mode::Json) {
        use std::io::Write;
        let state = serde_json::json!({
            "area":connection.name, "hostId":host.host_id, "epoch":host.epoch,
            "agentId":connection.agent_id, "sessionId":connection.session_id,
            "sessionPath":connection.session_path, "configPath":connection.config_path,
        });
        writeln!(std::io::stdout().lock(), "{state}").map_err(|e| e.to_string())?;
    } else {
        return super::area_terminal::run(connection, matches!(mode, Mode::Rpc)).await;
    }
    Ok(())
}
