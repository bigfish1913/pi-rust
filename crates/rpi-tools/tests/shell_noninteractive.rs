//! A tool call has no terminal the user can answer. `git` with no credential
//! helper opens `/dev/tty` directly for `Username for 'https://…':`, bypassing
//! the piped stdio (and the deliberate stdin-close in `exec`): it paints text
//! over the TUI frame that the editor never owned, and blocks the child until
//! the timeout. This is the third way a shell command can hang, next to the two
//! pipe cases in `bash_stdout_and_stdin_hold.rs`.
//!
//! The defaults must be *overridable*: an explicit host export,
//! `with_shell_env`, or per-call `env` is how a caller opts back into an
//! interactive prompt. These checks run in one test because they mutate the
//! process environment (`set_var`) and cargo runs tests in a file in parallel.
//!
//! Uses `OsExecutionEnv` — the in-memory fake spawns nothing and cannot
//! observe a real child's environment. Skips (not fails) without a shell or
//! git.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rpi_tools::{OsExecutionEnv, Shell, ShellExecOptions};

fn scratch() -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(format!("pi-tools-noninteractive-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// Restore a process variable even if an assertion panics.
struct EnvGuard(&'static str);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

const PRINT_ENV: &str = r#"printf 'git=[%s] gcm=[%s] ssh=[%s]\n' "$GIT_TERMINAL_PROMPT" "$GCM_INTERACTIVE" "$SSH_ASKPASS_REQUIRE""#;

async fn shell_or_skip() -> Option<OsExecutionEnv> {
    let env = OsExecutionEnv::with_cwd(scratch());
    match env.exec("true", ShellExecOptions::default()).await {
        Ok(_) => Some(env),
        Err(error) => {
            eprintln!("skipping: no usable shell ({error})");
            None
        }
    }
}

#[tokio::test]
async fn shell_children_are_non_interactive_by_default_and_can_opt_back_in() {
    // A clean baseline: if the host exported one of these, the "default"
    // assertions below would be measuring the caller's opt-in instead.
    for key in [
        "GIT_TERMINAL_PROMPT",
        "GCM_INTERACTIVE",
        "SSH_ASKPASS_REQUIRE",
    ] {
        std::env::remove_var(key);
    }
    let Some(_probe) = shell_or_skip().await else {
        return;
    };

    // 1. Defaults are present without any caller configuration.
    let defaults = OsExecutionEnv::with_cwd(scratch())
        .exec(PRINT_ENV, ShellExecOptions::default())
        .await
        .expect("exec");
    assert_eq!(
        defaults.stdout.trim(),
        "git=[0] gcm=[Never] ssh=[never]",
        "a spawned shell must not be able to ask the terminal for credentials"
    );

    // 2. `with_shell_env` (host-level base) wins over the defaults.
    let mut base = HashMap::new();
    base.insert("GIT_TERMINAL_PROMPT".to_string(), "1".to_string());
    let base_env = OsExecutionEnv::with_cwd(scratch()).with_shell_env(base);
    let out = base_env
        .exec(PRINT_ENV, ShellExecOptions::default())
        .await
        .expect("exec");
    assert!(
        out.stdout.contains("git=[1]"),
        "shell_env must override the default: {:?}",
        out.stdout
    );

    // 3. Per-call env wins too (and over `shell_env`).
    let mut per_call = HashMap::new();
    per_call.insert("GIT_TERMINAL_PROMPT".to_string(), "2".to_string());
    let out = base_env
        .exec(
            PRINT_ENV,
            ShellExecOptions {
                env: Some(per_call),
                ..Default::default()
            },
        )
        .await
        .expect("exec");
    assert!(
        out.stdout.contains("git=[2]"),
        "per-call env must override shell_env: {:?}",
        out.stdout
    );

    // 4. `inherit_env: false` asks for a clean environment, not for prompts.
    let out = OsExecutionEnv::with_cwd(scratch())
        .exec(
            PRINT_ENV,
            ShellExecOptions {
                inherit_env: false,
                ..Default::default()
            },
        )
        .await
        .expect("exec");
    assert_eq!(
        out.stdout.trim(),
        "git=[0] gcm=[Never] ssh=[never]",
        "the defaults are not part of the inherited environment"
    );

    // 5. An explicit host export is still an opt-in.
    let _guard = EnvGuard("GIT_TERMINAL_PROMPT");
    std::env::set_var("GIT_TERMINAL_PROMPT", "3");
    let out = OsExecutionEnv::with_cwd(scratch())
        .exec(PRINT_ENV, ShellExecOptions::default())
        .await
        .expect("exec");
    assert!(
        out.stdout.contains("git=[3]"),
        "an explicit host export must win: {:?}",
        out.stdout
    );
    drop(_guard);

    // 6. The actual failure mode: a credential request fails fast through
    // stderr instead of opening /dev/tty and hanging the call. `-c
    // credential.helper=` clears any configured helper so the prompt path is
    // reached deterministically. The outer timeout is a guard for the RED
    // state (without the defaults git would wait on the terminal).
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !git_available {
        eprintln!("skipping git credential check: git not on PATH");
        return;
    }
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        OsExecutionEnv::with_cwd(scratch()).exec(
            "printf 'protocol=https\\nhost=example.invalid\\n\\n' | git -c credential.helper= credential fill 2>&1; echo \"rc=$?\"",
            ShellExecOptions {
                timeout: Some(10.0),
                ..Default::default()
            },
        ),
    )
    .await;
    let elapsed = started.elapsed();
    let output = match result {
        Ok(Ok(out)) => out,
        Ok(Err(error)) => panic!("git credential fill failed as a tool error: {error:?}"),
        Err(_) => panic!("git credential fill hung: interactive prompts were not disabled"),
    };
    assert!(
        output.stdout.contains("terminal prompts disabled"),
        "the credential request must fail fast in captured stderr, not paint over the TUI: {:?}",
        output.stdout
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "git waited on a terminal for {elapsed:?}"
    );
}
