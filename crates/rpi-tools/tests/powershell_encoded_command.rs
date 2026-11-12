//! Review #270: real `-EncodedCommand` regressions for scripts whose first
//! statement must stay first (`using namespace`, `param(...)`). Prepending the
//! progress-preference assignment used to turn both into parser/command errors,
//! sometimes with exit code 0 — so these probes assert stdout **and** stderr,
//! not just the process status or the base64 round trip.
//!
//! They need a real Windows PowerShell, so they are compiled and run only on
//! Windows CI (`cargo check --workspace --all-targets` covers compilation
//! there, `windows-latest` hosts ship both `pwsh` and `powershell.exe`).

#![cfg(windows)]

use std::sync::Arc;

use rpi_agent::types::{TextContentOrImage, ToolResultPartial};
use rpi_tools::{ExecutionToolContext, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

async fn run(command: &str) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let context = ExecutionToolContext::new(env, None);
    let tool = rpi_tools::create_powershell_tool(&context);
    let result = tool
        .execute(
            "review270",
            serde_json::json!({ "command": command, "timeout": 30.0 }),
            CancellationToken::new(),
            Arc::new(|_update: ToolResultPartial| {}),
        )
        .await
        .expect("PowerShell tool must not report a parser/command error");
    match &result.content[0] {
        TextContentOrImage::Text(text) => text.text.clone(),
        other => panic!("expected text output, got {other:?}"),
    }
}

async fn assert_real_stdout_and_empty_stderr(command: &str, expected_stdout: &str) -> String {
    let output = run(command).await;
    assert!(
        output.contains(expected_stdout),
        "script must produce its expected stdout: {output}"
    );
    // The tool combines stdout+stderr; an error record from `param` being
    // parsed as a command, or a ParserError for `using`, would surface here
    // even when the child exits 0.
    for noise in [
        "ParserError",
        "A 'using' statement must appear before any other statements",
        "The term 'param' is not recognized",
        "NativeCommandError",
        "CLIXML",
    ] {
        assert!(
            !output.contains(noise),
            "encoded-command wrapper must not corrupt {command:?}: found {noise:?} in {output}"
        );
    }
    output
}

#[tokio::test]
async fn using_namespace_stays_the_first_statement() {
    let output = assert_real_stdout_and_empty_stderr(
        "using namespace System.Text\n[StringBuilder]::new().GetType().Name\n",
        "StringBuilder",
    )
    .await;
    assert!(
        output.trim() == "StringBuilder" || output.lines().any(|l| l.trim() == "StringBuilder"),
        "expected a clean StringBuilder line: {output}"
    );
}

#[tokio::test]
async fn param_block_stays_the_first_statement() {
    assert_real_stdout_and_empty_stderr("param([string]$Name='ok')\nWrite-Output $Name\n", "ok")
        .await;
}
