//! Isolated create/recovery probe, NOT full CLI/resource-preflight startup.
//! cargo run -p rpi-harness --example harness_create_probe --release --locked -- FROZEN.jsonl
//! Always copies input into its own temporary directory. Recovery writes are
//! permitted only there. No tools, providers, extensions, or agent run are used.
use std::{path::Path, sync::Arc, time::Instant};

use rpi_harness::{
    agent_harness::AgentHarness,
    session::{jsonl::JsonlSessionStorage, DefaultIdGenerator, Session, SystemClock},
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};

fn stamp(path: &Path) -> std::io::Result<(u64, std::time::SystemTime)> {
    let metadata = path.metadata()?;
    Ok((metadata.len(), metadata.modified()?))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = std::env::args_os()
        .nth(1)
        .ok_or("expected frozen input JSONL")?;
    let source = Path::new(&source);
    let mut companion = source.as_os_str().to_os_string();
    companion.push(".progress.jsonl");
    let companion = Path::new(&companion);
    let before = stamp(source)?;
    let progress_before = match stamp(companion) {
        Ok(stamp) => Some(stamp),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let root = tempfile::tempdir()?;
    let path = root.path().join("history.jsonl");
    std::fs::copy(source, &path)?;
    if progress_before.is_some() {
        std::fs::copy(companion, root.path().join("history.jsonl.progress.jsonl"))?;
    }
    if stamp(source)? != before
        || match stamp(companion) {
            Ok(stamp) => Some(stamp),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        } != progress_before
    {
        return Err("input changed while copying; use an immutable fixture".into());
    }
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(root.path().into()));
    let started = Instant::now();
    let prepared = JsonlSessionStorage::prepare_restore(
        fs,
        &path.to_string_lossy(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await?;
    let storage = Arc::new(prepared.commit().await?);
    let session = Session::new(storage.clone(), None);
    let load_ns = started.elapsed().as_nanos();
    let open_before = session.find_open_operations("main", None).await?.len();
    let started = Instant::now();
    let harness = AgentHarness::create(AgentHarnessOptions {
        session,
        allow_existing_session: true,
        // Empty defaults make recorded safe calls non-replayable in this probe.
        // Never populate tools/models or continue the restored run here.
        ..Default::default()
    })
    .await?;
    let create_ns = started.elapsed().as_nanos();
    println!("load_ns={load_ns} create_ns={create_ns} input_bytes={} open_before={open_before} pending_resume={}", before.0, harness.pending_resume().is_some());
    harness.close().await?;
    harness.drain_session().await;
    drop(harness);
    storage.drain().await;
    storage.release_writer_lease();
    drop(storage);
    // tempfile removes transcript/progress/lease artifacts, including on errors.
    root.close()?;
    Ok(())
}
