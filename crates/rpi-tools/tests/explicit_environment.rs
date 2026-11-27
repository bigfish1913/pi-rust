#![cfg(unix)]
use rpi_tools::{FileSystem, OsExecutionEnv, Shell, ShellExecOptions};
use std::collections::HashMap;

#[tokio::test]
async fn closed_baseline_and_home_do_not_fall_back_to_ambient_process_state() {
    if std::env::var_os("RPI_TEST_CLOSED_ENV_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "closed_baseline_and_home_do_not_fall_back_to_ambient_process_state",
            ])
            .env("RPI_TEST_CLOSED_ENV_CHILD", "1")
            .env("RPI_CLOSED_ENV_AMBIENT", "must-not-inherit")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated closed env regression failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let snapshot = HashMap::from([
        ("HOME".into(), cwd.to_string_lossy().into_owned()),
        ("SCOPE".into(), "agent-one".into()),
        (
            rpi_plugin_sdk::SESSION_ID_ENV.into(),
            "host-private-must-not-inherit".into(),
        ),
    ]);
    let env = OsExecutionEnv::with_cwd(cwd.clone())
        .with_shell_path("/bin/sh".into())
        .with_environment(snapshot);
    assert!(env.has_explicit_environment());
    assert_eq!(
        env.absolute_path("~/file", None).await.unwrap(),
        cwd.join("file")
    );
    let out = env
        .exec(
            r#"printf '%s|%s|%s|%s' "$SCOPE" "$RPI_CLOSED_ENV_AMBIENT" "$RPI_SESSION_ID" "$HOME""#,
            ShellExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, format!("agent-one|||{}", cwd.display()));
    // Existing callers retain the legacy process baseline.
    let legacy = OsExecutionEnv::with_cwd(cwd).with_shell_path("/bin/sh".into());
    assert!(!legacy.has_explicit_environment());
    assert_eq!(
        legacy
            .exec(
                r#"printf '%s' "$RPI_CLOSED_ENV_AMBIENT""#,
                ShellExecOptions::default()
            )
            .await
            .unwrap()
            .stdout,
        "must-not-inherit"
    );
}

#[tokio::test]
async fn explicit_baseline_preserves_inherit_false_and_overlay_precedence() {
    let root = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(root.path().to_path_buf())
        .with_shell_path("/bin/sh".into())
        .with_environment(HashMap::from([("SCOPE".into(), "baseline".into())]))
        .with_shell_env(HashMap::from([("OVERLAY".into(), "shell".into())]));
    let out = env
        .exec(
            r#"printf '%s|%s|%s' "$SCOPE" "$OVERLAY" "$GIT_TERMINAL_PROMPT""#,
            ShellExecOptions {
                inherit_env: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "|shell|0");
    let out = env
        .exec(
            r#"printf '%s|%s' "$SCOPE" "$OVERLAY""#,
            ShellExecOptions {
                env: Some(HashMap::from([("OVERLAY".into(), "call".into())])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, "baseline|call");
}

#[tokio::test]
async fn missing_explicit_home_stays_literal_instead_of_escaping_to_filesystem_root() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let env = OsExecutionEnv::with_cwd(cwd.clone()).with_environment(HashMap::new());
    assert_eq!(
        env.absolute_path("~/file", None).await.unwrap(),
        cwd.join("~/file")
    );
    assert_eq!(env.absolute_path("~", None).await.unwrap(), cwd.join("~"));
}
