use crate::app::{EXIT_RUNTIME, EXIT_USAGE};
use rpi_harness::session::jsonl::JsonlSessionStorage;
use serde_json::json;
use std::{io::Write, path::Path};

pub(crate) fn run(args: &[String]) -> i32 {
    if !cfg!(unix) {
        eprintln!("error: --session-preview requires Unix");
        return EXIT_USAGE;
    }
    let (id, dir) = match args {
        [flag, id, dir_flag, dir] | [dir_flag, dir, flag, id]
            if flag == "--session-preview" && dir_flag == "--session-dir" =>
        {
            (id, dir)
        }
        _ => {
            eprintln!("usage: rpi --session-preview <session UUID> --session-dir <existing dir>");
            return EXIT_USAGE;
        }
    };
    if uuid::Uuid::parse_str(id).is_err() || dir.is_empty() {
        eprintln!("error: session preview requires a session UUID and an existing directory");
        return EXIT_USAGE;
    }
    match write_preview(id, Path::new(dir)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: session preview unavailable: {error}");
            EXIT_RUNTIME
        }
    }
}

fn write_preview(id: &str, dir: &Path) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let (entries, has_more) =
        JsonlSessionStorage::read_preview(dir, &cwd, id).map_err(|error| error.to_string())?;
    let messages = crate::modes::rpc_project_transcript(entries)?
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    let output = json!({"sessionId": id, "messages": messages, "hasMore": has_more});
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &output).map_err(|error| error.to_string())?;
    stdout.write_all(b"\n").map_err(|error| error.to_string())
}
