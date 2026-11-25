//! Early CLI entry for native Host areas.

const HELP: &str = "Usage: rpi --host [area] [--mode json|rpc]\n\nJoin the native Host for an area, starting it on first use. The default\narea is configured in $RPI_CODING_AGENT_DIR/host.json. macOS only.\nUse --mode json to report the selected Host and Session.";

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
    let Some((name, mode)) = parse(argv) else {
        eprintln!("{HELP}");
        return 2;
    };
    #[cfg(not(target_os = "macos"))]
    let result: Result<(), String> = {
        let _ = (name, mode);
        Err("detached Host areas are currently supported on macOS".into())
    };
    #[cfg(target_os = "macos")]
    let result = enter(name, mode).await;
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn parse(argv: &[String]) -> Option<(Option<String>, Mode)> {
    let (flag, mut rest) = argv.split_first()?;
    let name = if let Some(value) = flag.strip_prefix("--host=") {
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
    match rest {
        [] => Some((name, Mode::Terminal)),
        [flag, mode] if flag == "--mode" && mode == "json" => Some((name, Mode::Json)),
        [flag, mode] if flag == "--mode" && mode == "rpc" => Some((name, Mode::Rpc)),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
async fn enter(name: Option<String>, mode: Mode) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let connection = super::area::join(name, cwd).await?;
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
