//! `--agent-dir` isolation contract (PR 1).
//!
//! One process, two explicit agent homes: settings / models / skills / SYSTEM.md
//! / trust decisions must come from the selected home, never from the process
//! default (`RPI_CODING_AGENT_DIR` / `~/.rpi/agent`) or the other home.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rpi_cli::{config, provider, resource_dirs, settings};

fn write_home(root: &Path, provider_id: &str, model_id: &str, prompt: &str) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("settings.json"),
        format!(r#"{{"defaultProvider":"{provider_id}","defaultModel":"{model_id}"}}"#),
    )
    .unwrap();
    std::fs::write(
        root.join("models.json"),
        format!(
            r#"{{"providers":{{"{provider_id}":{{"api":"anthropic-messages","apiKey":"key-{provider_id}","models":[{{"id":"{model_id}"}}]}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(root.join("SYSTEM.md"), prompt).unwrap();
    let skill = root.join("skills").join(format!("skill-{provider_id}"));
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: skill-{provider_id}\ndescription: {provider_id} skill\n---\nbody\n"),
    )
    .unwrap();
    root.to_path_buf()
}

fn set_trust(home: &Path, cwd: &Path, trusted: bool) {
    let key = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut store = BTreeMap::new();
    store.insert(key, Some(trusted));
    std::fs::write(home.join("trust.json"), serde_json::to_vec(&store).unwrap()).unwrap();
}

#[test]
fn explicit_agent_dir_isolates_config_and_resources() {
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    write_home(alpha.path(), "alpha", "alpha-model", "SYSTEM-ALPHA");
    write_home(beta.path(), "beta", "beta-model", "SYSTEM-BETA");
    set_trust(alpha.path(), cwd.path(), true);
    set_trust(beta.path(), cwd.path(), false);

    // settings.json
    assert_eq!(
        settings::load_settings_at(Some(alpha.path()))
            .unwrap()
            .default_model
            .as_deref(),
        Some("alpha-model")
    );
    assert_eq!(
        settings::load_settings_at(Some(beta.path()))
            .unwrap()
            .default_model
            .as_deref(),
        Some("beta-model")
    );

    // models.json + provider resolution
    let resolved_alpha =
        provider::resolve_with_home(Some(alpha.path()), None, None, None, None, None).unwrap();
    assert_eq!(resolved_alpha.provider.id(), "alpha");
    assert_eq!(resolved_alpha.model.id, "alpha-model");
    let resolved_beta =
        provider::resolve_with_home(Some(beta.path()), None, None, None, None, None).unwrap();
    assert_eq!(resolved_beta.provider.id(), "beta");
    assert_eq!(resolved_beta.model.id, "beta-model");

    // skills: only the selected home's global skills dir is listed
    let alpha_skills = alpha.path().join("skills");
    let beta_skills = beta.path().join("skills");
    let alpha_dirs = resource_dirs::skill_dirs_at(cwd.path(), Some(alpha.path()));
    assert!(alpha_dirs.iter().any(|d| d == &alpha_skills));
    assert!(!alpha_dirs.iter().any(|d| d == &beta_skills));
    let beta_dirs = resource_dirs::skill_dirs_at(cwd.path(), Some(beta.path()));
    assert!(beta_dirs.iter().any(|d| d == &beta_skills));
    assert!(!beta_dirs.iter().any(|d| d == &alpha_skills));

    // SYSTEM.md
    let discovered_alpha =
        resource_dirs::discover_system_prompt_file_at(Some(alpha.path()), cwd.path(), false)
            .unwrap();
    assert!(discovered_alpha.starts_with(alpha.path()));
    let discovered_beta =
        resource_dirs::discover_system_prompt_file_at(Some(beta.path()), cwd.path(), false)
            .unwrap();
    assert!(discovered_beta.starts_with(beta.path()));

    // trust.json
    assert_eq!(
        config::project_trust_decision_at(Some(alpha.path()), cwd.path()).unwrap(),
        Some(true)
    );
    assert_eq!(
        config::project_trust_decision_at(Some(beta.path()), cwd.path()).unwrap(),
        Some(false)
    );
}

#[test]
fn explicit_agent_dirs_are_safe_to_resolve_concurrently() {
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();
    write_home(alpha.path(), "alpha", "alpha-model", "SYSTEM-ALPHA");
    write_home(beta.path(), "beta", "beta-model", "SYSTEM-BETA");

    let alpha_dir = alpha.path().to_path_buf();
    let beta_dir = beta.path().to_path_buf();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            for _ in 0..50 {
                let resolved =
                    provider::resolve_with_home(Some(&alpha_dir), None, None, None, None, None)
                        .unwrap();
                assert_eq!(resolved.model.id, "alpha-model");
            }
        });
        let b = scope.spawn(|| {
            for _ in 0..50 {
                let resolved =
                    provider::resolve_with_home(Some(&beta_dir), None, None, None, None, None)
                        .unwrap();
                assert_eq!(resolved.model.id, "beta-model");
            }
        });
        a.join().unwrap();
        b.join().unwrap();
    });
}

fn run_rpi(env_home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
        .env(config::CONFIG_DIR_ENV, env_home)
        .args(args)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("spawn the rpi binary")
}

/// The flag, not the inherited env var, selects the home the real binary reads.
#[test]
fn cli_agent_dir_beats_the_process_env_home() {
    let env_home = tempfile::tempdir().unwrap();
    let flag_home = tempfile::tempdir().unwrap();
    write_home(env_home.path(), "alpha", "alpha-model", "SYSTEM-ALPHA");
    write_home(flag_home.path(), "beta", "beta-model", "SYSTEM-BETA");

    let output = run_rpi(
        env_home.path(),
        &[
            "--agent-dir",
            flag_home.path().to_str().unwrap(),
            "--list-models",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("beta-model"), "stdout: {stdout}");
    assert!(!stdout.contains("alpha-model"), "stdout: {stdout}");
}

/// A typo in the flag must stop startup instead of partially applying a home.
#[test]
fn cli_agent_dir_rejects_relative_missing_and_non_directory_paths() {
    let env_home = tempfile::tempdir().unwrap();

    let relative = run_rpi(
        env_home.path(),
        &["--agent-dir", "relative-home", "--list-models"],
    );
    assert_eq!(relative.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&relative.stderr).contains("--agent-dir must be an absolute path"),
        "stderr: {}",
        String::from_utf8_lossy(&relative.stderr)
    );

    let missing = env_home.path().join("missing-home");
    let missing = run_rpi(
        env_home.path(),
        &["--agent-dir", missing.to_str().unwrap(), "--list-models"],
    );
    assert_eq!(missing.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("is not accessible"),
        "stderr: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let file = env_home.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let file = run_rpi(
        env_home.path(),
        &["--agent-dir", file.to_str().unwrap(), "--list-models"],
    );
    assert_eq!(file.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&file.stderr).contains("is not a directory"),
        "stderr: {}",
        String::from_utf8_lossy(&file.stderr)
    );
}
