//! A spawned shell must not receive the host's private plumbing.
//!
//! The host writes `RPI_SESSION_ID` into its own environment so plugins can name
//! the session they observe. A bash/powershell child inherits the environment by
//! default, so without an explicit exclusion the variable reaches *everything*
//! the shell runs — including a nested `rpi`, which would then see the variable
//! already present and refuse to publish its own session id (the host only sets
//! it when absent). The result is a descendant reporting an ancestor's session,
//! silently.
//!
//! These run against a real shell, because the property under test is about the
//! process environment of a real child — an in-memory fake spawns nothing and
//! could not observe it.

use rpi_tools::{is_host_private_env, OsExecutionEnv, Shell, ShellExecOptions};

/// Skip when the platform has no usable shell (the env resolves git-bash on
/// Windows, `/bin/bash` on POSIX). Mirrors the guard other shell tests use.
async fn shell_or_skip() -> Option<OsExecutionEnv> {
    let env = OsExecutionEnv::new();
    match env.exec("true", ShellExecOptions::default()).await {
        Ok(_) => Some(env),
        Err(error) => {
            eprintln!("skipping: no usable shell ({error})");
            None
        }
    }
}

#[tokio::test]
async fn a_host_private_variable_does_not_reach_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    std::env::set_var("RPI_SESSION_ID", "01a0f60b-parent-session");

    // A variable the host does NOT mark private still inherits, so this is a
    // targeted exclusion rather than "the shell sees no environment".
    std::env::set_var("RPI_TOOLS_INHERIT_PROBE", "visible");

    let out = env
        .exec(
            r#"printf 'session=[%s] probe=[%s]\n' "$RPI_SESSION_ID" "$RPI_TOOLS_INHERIT_PROBE""#,
            ShellExecOptions::default(),
        )
        .await
        .expect("exec");

    assert_eq!(
        out.stdout.trim(),
        "session=[] probe=[visible]",
        "the host-private variable must not inherit while ordinary ones do"
    );

    std::env::remove_var("RPI_TOOLS_INHERIT_PROBE");
    std::env::remove_var("RPI_SESSION_ID");
}

#[tokio::test]
async fn an_explicit_private_variable_still_reaches_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    // The exclusion is about *inheriting*, not about forbidding the name: a
    // caller that deliberately passes a value (a test fixture, a wrapper script)
    // must still be able to. Per-call env is applied after the inherit loop.
    std::env::set_var("RPI_SESSION_ID", "01a0f60b-parent-session");

    let mut per_call = std::collections::HashMap::new();
    per_call.insert(
        "RPI_SESSION_ID".to_string(),
        "01a0f60b-explicit".to_string(),
    );
    let out = env
        .exec(
            r#"printf 'session=[%s]\n' "$RPI_SESSION_ID""#,
            ShellExecOptions {
                env: Some(per_call),
                ..ShellExecOptions::default()
            },
        )
        .await
        .expect("exec");

    assert_eq!(out.stdout.trim(), "session=[01a0f60b-explicit]");

    std::env::remove_var("RPI_SESSION_ID");
}

#[test]
fn the_private_list_names_the_plugin_session_variable() {
    // The exclusion list and the plugin contract have to agree; rpi-cli asserts
    // the same thing from the other side (see its own test), because rpi-tools
    // must not depend on rpi-plugin-sdk just to name one string.
    assert!(is_host_private_env("RPI_SESSION_ID"));
    assert!(!is_host_private_env("RPI_OFFLINE"));
    assert!(!is_host_private_env("PATH"));
}
