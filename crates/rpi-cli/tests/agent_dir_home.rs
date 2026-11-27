//! Explicit agent-home isolation contract (PR 1).
//!
//! One process, two explicit agent homes: settings / models / skills / SYSTEM.md
//! / trust decisions must come from the selected home, never from the process
//! default (`RPI_CODING_AGENT_DIR` / `~/.rpi/agent`) or the other home.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rpi_cli::{config, provider};

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
fn from_option_rejects_relative_homes_eagerly() {
    let err = config::AgentHome::from_option(Some(PathBuf::from("relative/home")))
        .expect_err("relative homes must fail at the API boundary");
    assert!(matches!(err, config::ConfigError::InvalidAgentHome { .. }));
    assert!(config::AgentHome::from_option(None).is_ok());
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

    let alpha_home = config::AgentHome::from_option(Some(alpha.path().to_path_buf())).unwrap();
    let beta_home = config::AgentHome::from_option(Some(beta.path().to_path_buf())).unwrap();

    // settings.json
    assert_eq!(
        alpha_home.settings().unwrap().default_model.as_deref(),
        Some("alpha-model")
    );
    assert_eq!(
        beta_home.settings().unwrap().default_model.as_deref(),
        Some("beta-model")
    );

    // models.json + provider resolution
    let resolved_alpha = provider::resolve(&alpha_home, None, None, None, None, None).unwrap();
    assert_eq!(resolved_alpha.provider.id(), "alpha");
    assert_eq!(resolved_alpha.model.id, "alpha-model");
    let resolved_beta = provider::resolve(&beta_home, None, None, None, None, None).unwrap();
    assert_eq!(resolved_beta.provider.id(), "beta");
    assert_eq!(resolved_beta.model.id, "beta-model");

    // skills: only the selected home's global skills dir is listed
    let alpha_skills = alpha.path().join("skills");
    let beta_skills = beta.path().join("skills");
    let alpha_dirs = alpha_home.skill_dirs(cwd.path());
    assert!(alpha_dirs.iter().any(|d| d == &alpha_skills));
    assert!(!alpha_dirs.iter().any(|d| d == &beta_skills));
    let beta_dirs = beta_home.skill_dirs(cwd.path());
    assert!(beta_dirs.iter().any(|d| d == &beta_skills));
    assert!(!beta_dirs.iter().any(|d| d == &alpha_skills));

    // SYSTEM.md
    let discovered_alpha = alpha_home
        .discover_system_prompt_file(cwd.path(), false)
        .unwrap();
    assert!(discovered_alpha.starts_with(alpha.path()));
    let discovered_beta = beta_home
        .discover_system_prompt_file(cwd.path(), false)
        .unwrap();
    assert!(discovered_beta.starts_with(beta.path()));

    // trust.json
    assert_eq!(
        alpha_home.project_trust_decision(cwd.path()).unwrap(),
        Some(true)
    );
    assert_eq!(
        beta_home.project_trust_decision(cwd.path()).unwrap(),
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
            let home = config::AgentHome::from_option(Some(alpha_dir)).unwrap();
            for _ in 0..50 {
                let resolved = provider::resolve(&home, None, None, None, None, None).unwrap();
                assert_eq!(resolved.model.id, "alpha-model");
            }
        });
        let b = scope.spawn(|| {
            let home = config::AgentHome::from_option(Some(beta_dir)).unwrap();
            for _ in 0..50 {
                let resolved = provider::resolve(&home, None, None, None, None, None).unwrap();
                assert_eq!(resolved.model.id, "beta-model");
            }
        });
        a.join().unwrap();
        b.join().unwrap();
    });
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The real session entrypoint (`build`) reads the injected home, not the
/// process `RPI_CODING_AGENT_DIR`.
#[tokio::test(flavor = "current_thread")]
async fn build_uses_the_injected_home_over_the_process_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(config::CONFIG_DIR_ENV),
            }
        }
    }

    let env_home = tempfile::tempdir().unwrap();
    let selected_home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_home(env_home.path(), "alpha", "alpha-model", "SYSTEM-ALPHA");
    write_home(selected_home.path(), "beta", "beta-model", "SYSTEM-BETA");
    let _restore = RestoreEnv(std::env::var_os(config::CONFIG_DIR_ENV));
    std::env::set_var(config::CONFIG_DIR_ENV, env_home.path());

    let home = config::AgentHome::from_option(Some(selected_home.path().to_path_buf())).unwrap();
    let resolved = provider::resolve(&home, None, None, None, None, None).unwrap();
    let args = rpi_cli::args::Args {
        no_session: true,
        no_extensions: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("stable".into()),
        ..Default::default()
    };
    let (harness, _events, context) =
        rpi_cli::session::build(&home, &resolved, &args, cwd.path(), false)
            .await
            .unwrap();

    assert_eq!(context.home.root().unwrap(), selected_home.path());
    let skills = harness
        .get_resources()
        .await
        .unwrap()
        .skills
        .unwrap_or_default();
    assert!(skills.iter().any(|skill| skill.name == "skill-beta"));
    assert!(!skills.iter().any(|skill| skill.name == "skill-alpha"));
}

/// The headline claim is "two sessions in one process". Build home A and home
/// B in the same process (with a third process home in the env) and assert
/// each ReloadContext and each harness's resources belong to its own home.
#[tokio::test(flavor = "current_thread")]
async fn builds_two_explicit_homes_in_one_process() {
    let _guard = ENV_LOCK.lock().unwrap();
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(config::CONFIG_DIR_ENV),
            }
        }
    }

    let process_home = tempfile::tempdir().unwrap();
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_home(
        process_home.path(),
        "process",
        "process-model",
        "SYSTEM-PROCESS",
    );
    write_home(alpha.path(), "alpha", "alpha-model", "SYSTEM-ALPHA");
    write_home(beta.path(), "beta", "beta-model", "SYSTEM-BETA");
    let _restore = RestoreEnv(std::env::var_os(config::CONFIG_DIR_ENV));
    std::env::set_var(config::CONFIG_DIR_ENV, process_home.path());

    let alpha_home = config::AgentHome::from_option(Some(alpha.path().to_path_buf())).unwrap();
    let beta_home = config::AgentHome::from_option(Some(beta.path().to_path_buf())).unwrap();
    let args = rpi_cli::args::Args {
        no_session: true,
        no_extensions: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("stable".into()),
        ..Default::default()
    };

    let alpha_model = provider::resolve(&alpha_home, None, None, None, None, None).unwrap();
    let (alpha_harness, _alpha_events, alpha_context) =
        rpi_cli::session::build(&alpha_home, &alpha_model, &args, cwd.path(), false)
            .await
            .unwrap();
    let beta_model = provider::resolve(&beta_home, None, None, None, None, None).unwrap();
    let (beta_harness, _beta_events, beta_context) =
        rpi_cli::session::build(&beta_home, &beta_model, &args, cwd.path(), false)
            .await
            .unwrap();

    assert_eq!(alpha_context.home.root().unwrap(), alpha.path());
    assert_eq!(beta_context.home.root().unwrap(), beta.path());

    let alpha_skills = alpha_harness
        .get_resources()
        .await
        .unwrap()
        .skills
        .unwrap_or_default();
    let beta_skills = beta_harness
        .get_resources()
        .await
        .unwrap()
        .skills
        .unwrap_or_default();
    assert!(alpha_skills.iter().any(|skill| skill.name == "skill-alpha"));
    assert!(!alpha_skills.iter().any(|skill| skill.name == "skill-beta"));
    assert!(!alpha_skills
        .iter()
        .any(|skill| skill.name == "skill-process"));
    assert!(beta_skills.iter().any(|skill| skill.name == "skill-beta"));
    assert!(!beta_skills.iter().any(|skill| skill.name == "skill-alpha"));
    assert!(!beta_skills
        .iter()
        .any(|skill| skill.name == "skill-process"));
}
