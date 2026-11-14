//! The `rpi` binary entry point.
//!
//! The entry point is deliberately synchronous: the filesystem sandbox
//! (Linux Landlock in M1) must be applied **before** Tokio creates worker
//! threads, because Landlock domains are per-thread and inherited by children.
//! [`rpi_cli::sandbox::prepare_from_argv`] does the synchronous preflight
//! (args, trust, policy, backend) and then we build the multi-thread runtime the
//! harness needs (`block_in_place` in the stream bridge requires it).

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    match rpi_cli::sandbox::prepare_from_argv(&argv) {
        Ok(_prepared) => {}
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    ExitCode::from(runtime.block_on(rpi_cli::app::run()) as u8)
}
