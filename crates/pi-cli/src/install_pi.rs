//! Pi package installer (`rpi install-pi`).
//!
//! Pi packages are ordinary npm/git/local directories. npm and Git sources use
//! Pi-compatible managed stores, while local directories are enabled in place.
//! The resulting source is recorded in settings so static resources and JS/TS
//! extensions can be loaded on startup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::npm::{NpmCommand, NpmManagerKind};

#[derive(Debug, Clone)]
struct Options {
    spec: String,
    global: bool,
    force: bool,
}

#[derive(Debug)]
struct InstallOutcome {
    root: PathBuf,
    settings_source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSpecKind {
    Npm,
    Git,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NpmExecutionMode {
    Interactive,
    StartupRemediation,
}

pub fn run(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) if message == "help" => {
            print_help();
            return 0;
        }
        Err(message) => {
            eprintln!("error: {message}");
            print_help();
            return 2;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not determine current directory: {error}");
            return 1;
        }
    };
    let project_trusted = match project_trust_for_package_operation(&cwd, options.global) {
        Ok(trusted) => trusted,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let spec_kind = classify_install_spec(&cwd, &options.spec);
    let npm_command = match npm_command_for_install(&cwd, project_trusted, spec_kind) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let mut settings = match load_settings_for_install(&cwd, options.global) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("error: refusing to install with unreadable settings: {error}");
            return 1;
        }
    };
    let outcome = match install_spec(&cwd, &options, spec_kind, npm_command.as_ref()) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let package_root = normalize_config_path(
        std::fs::canonicalize(&outcome.root).unwrap_or_else(|_| outcome.root.clone()),
    );
    let packages = settings.packages.get_or_insert_with(Vec::new);
    let enabled = match outcome
        .settings_source
        .map(Ok)
        .unwrap_or_else(|| settings_entry(&package_root, &cwd, options.global))
    {
        Ok(enabled) => enabled,
        Err(error) => {
            eprintln!("error: installed package but could not record its settings entry: {error}");
            return 1;
        }
    };
    if enable_settings_entry(&cwd, packages, enabled) {
        if let Err(error) = save_settings_for_install(&cwd, options.global, &settings) {
            eprintln!("error: installed package but could not save settings: {error}");
            return 1;
        }
    }
    println!("installed Pi package {}", package_root.display());
    println!("JS/TS extensions and static resources will load on the next start.");
    println!("warning: Pi extensions execute JavaScript with the current user's permissions.");
    0
}

fn project_trust_for_package_operation(cwd: &Path, global: bool) -> Result<bool, String> {
    if global {
        return Ok(false);
    }
    crate::config::project_trust_decision(cwd)
        .map(|decision| decision.unwrap_or(true))
        .map_err(|error| format!("could not read project trust decision: {error}"))
}

fn classify_install_spec(cwd: &Path, spec: &str) -> InstallSpecKind {
    if is_git_install_spec(spec) {
        return InstallSpecKind::Git;
    }
    let local_candidate = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        cwd.join(spec)
    };
    if spec.starts_with("npm:") || (!local_candidate.exists() && looks_like_npm(spec)) {
        InstallSpecKind::Npm
    } else {
        InstallSpecKind::Local
    }
}

fn npm_command_for_install(
    cwd: &Path,
    project_trusted: bool,
    kind: InstallSpecKind,
) -> Result<Option<NpmCommand>, String> {
    if kind == InstallSpecKind::Local {
        Ok(None)
    } else {
        NpmCommand::resolve(cwd, project_trusted).map(Some)
    }
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut spec = None;
    let mut global = false;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Err("help".into()),
            "--global" | "-g" => global = true,
            "--force" | "-f" => force = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown install-pi option `{value}`"))
            }
            value => {
                if spec.replace(value.to_string()).is_some() {
                    return Err("install-pi accepts exactly one package spec".into());
                }
            }
        }
        i += 1;
    }
    Ok(Options {
        spec: spec.ok_or_else(|| "missing npm, git, or local package spec".to_string())?,
        global,
        force,
    })
}

/// Create a managed store one directory at a time and reject every symlink,
/// junction, or other canonical redirection in its ancestry. This check runs
/// before any package manager, clone, copy, or deletion touches the store.
fn prepare_managed_destination(destination: &Path) -> Result<(), String> {
    if !destination.is_absolute() {
        return Err(format!(
            "refusing non-absolute package destination {}",
            destination.display()
        ));
    }
    ensure_real_directory_tree(destination)
}

fn ensure_real_directory_tree(path: &Path) -> Result<(), String> {
    if path.parent().is_some_and(|parent| parent != path) {
        if !path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| format!("directory has no parent: {}", path.display()))?;
            ensure_real_directory_tree(parent)?;
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "could not create managed package directory {}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect managed package directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing redirected package destination {}",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map(normalize_config_path)
        .map_err(|error| {
            format!(
                "could not resolve managed package directory {}: {error}",
                path.display()
            )
        })?;
    if !paths_equal(&canonical, path) {
        return Err(format!(
            "refusing redirected package destination {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_install_target(destination: &Path, target: &Path) -> Result<(), String> {
    ensure_real_directory_tree(destination)?;
    if target.parent() != Some(destination) {
        return Err(format!(
            "refusing package target outside managed destination: {}",
            target.display()
        ));
    }
    let Some(leaf) = target.file_name().and_then(|name| name.to_str()) else {
        return Err("package target must have a valid file name".to_string());
    };
    if matches!(leaf, "" | "." | "..") || leaf.starts_with('.') {
        return Err(format!("refusing invalid package target name `{leaf}`"));
    }
    match std::fs::symlink_metadata(target) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "refusing redirected package target {}",
                    target.display()
                ));
            }
            let canonical = std::fs::canonicalize(target)
                .map(normalize_config_path)
                .map_err(|error| format!("could not resolve {}: {error}", target.display()))?;
            if !paths_equal(&canonical, target) {
                return Err(format!(
                    "refusing redirected package target {}",
                    target.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect package target {}: {error}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn install_spec(
    cwd: &Path,
    options: &Options,
    kind: InstallSpecKind,
    npm_command: Option<&NpmCommand>,
) -> Result<InstallOutcome, String> {
    let spec = options.spec.as_str();
    if kind == InstallSpecKind::Git {
        let npm_command = npm_command.ok_or("missing package manager for git installation")?;
        let root = install_git(cwd, options.global, spec, options.force, npm_command)?;
        return Ok(InstallOutcome {
            root,
            settings_source: Some(spec.trim().to_string()),
        });
    }
    if kind == InstallSpecKind::Npm {
        let npm_command = npm_command.ok_or("missing package manager for npm installation")?;
        let npm_spec = npm_settings_spec(spec);
        let root = install_managed_npm(cwd, options.global, &npm_spec, options.force, npm_command)?;
        return Ok(InstallOutcome {
            root,
            settings_source: Some(npm_spec),
        });
    }
    let root = install_local(cwd, spec)?;
    Ok(InstallOutcome {
        settings_source: Some(settings_entry(&root, cwd, options.global)?),
        root,
    })
}

fn is_git_install_spec(spec: &str) -> bool {
    let spec = spec.trim().to_ascii_lowercase();
    spec.starts_with("git:")
        || spec.starts_with("https://")
        || spec.starts_with("http://")
        || spec.starts_with("ssh://")
        || spec.starts_with("git://")
}

fn npm_settings_spec(spec: &str) -> String {
    format!("npm:{}", spec.strip_prefix("npm:").unwrap_or(spec).trim())
}

fn load_settings_for_install(
    cwd: &Path,
    global: bool,
) -> Result<crate::settings::Settings, crate::config::ConfigError> {
    if global {
        crate::settings::load_settings()
    } else {
        crate::settings::load_project_settings_for_write(cwd)
    }
}

fn save_settings_for_install(
    cwd: &Path,
    global: bool,
    settings: &crate::settings::Settings,
) -> Result<(), String> {
    if global {
        crate::settings::save_settings(settings)
    } else {
        crate::settings::save_project_settings(cwd, settings)
    }
}

fn settings_entry(root: &Path, cwd: &Path, global: bool) -> Result<String, String> {
    let base = if global {
        crate::config::agent_dir().map_err(|error| error.to_string())?
    } else {
        cwd.join(".rpi")
    };
    let relative = relative_path_from(root, &base).ok_or_else(|| {
        format!(
            "package root {} cannot be represented from its settings directory {}",
            root.display(),
            base.display()
        )
    })?;
    Ok(format!(
        "file:{}",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn relative_path_from(path: &Path, base: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let path = normalize_config_path(path.to_path_buf());
    let base = normalize_config_path(base.to_path_buf());
    if !path.is_absolute() || !base.is_absolute() {
        return None;
    }
    let path_components = path.components().collect::<Vec<_>>();
    let base_components = base.components().collect::<Vec<_>>();
    let mut common = 0;
    while common < path_components.len()
        && common < base_components.len()
        && path_components_equal(path_components[common], base_components[common])
    {
        common += 1;
    }
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in &base_components[common..] {
        match component {
            Component::Normal(_) => relative.push(".."),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    for component in &path_components[common..] {
        match component {
            Component::Prefix(_) | Component::RootDir => return None,
            _ => relative.push(component.as_os_str()),
        }
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

fn path_components_equal(left: std::path::Component<'_>, right: std::path::Component<'_>) -> bool {
    if cfg!(windows) {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    } else {
        left == right
    }
}

fn enable_settings_entry(
    cwd: &Path,
    packages: &mut Vec<crate::settings::PackageSetting>,
    enabled: String,
) -> bool {
    if let Some(existing) = packages
        .iter_mut()
        .find(|value| package_settings_sources_match(cwd, value.source(), &enabled))
    {
        if existing.source() == enabled {
            return false;
        }
        match existing {
            crate::settings::PackageSetting::Source(source) => *source = enabled,
            crate::settings::PackageSetting::Filtered(filter) => filter.source = enabled,
        }
        return true;
    }
    packages.push(enabled.into());
    true
}

fn package_name(spec: &str) -> String {
    if let Some(parsed) = crate::packages::parse_npm_package_spec(spec) {
        return parsed
            .install_name
            .trim_start_matches('@')
            .replace('/', "__")
            .replace('\\', "__");
    }
    let raw = spec.strip_prefix("npm:").unwrap_or(spec);
    let raw = if raw.starts_with('@') {
        raw.rfind('@')
            .filter(|index| *index > 0)
            .map(|index| &raw[..index])
            .unwrap_or(raw)
    } else {
        raw.split('@').next().unwrap_or(raw)
    };
    raw.trim_start_matches('@')
        .replace('/', "__")
        .replace('\\', "__")
}

fn safe_name(spec: &str) -> String {
    package_name(spec)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn install_managed_npm(
    cwd: &Path,
    global: bool,
    spec: &str,
    force: bool,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    let parsed = npm_package_spec_checked(spec)?;
    let name = PathBuf::from(&parsed.install_name);
    let root = managed_npm_root(cwd, global)?;
    validate_recognized_npm_store_root(&root, cwd, true)?;
    let target = root.join("node_modules").join(&name);
    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        if !force {
            return Err(format!(
                "{} already exists; use --force to reinstall it",
                target.display()
            ));
        }
        if !metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to replace non-directory npm target {}",
                target.display()
            ));
        }
    }
    let source = spec.strip_prefix("npm:").unwrap_or(spec).trim();
    update_npm_store_root(
        &root,
        &[(
            name.to_string_lossy().replace('\\', "/"),
            format!("npm:{source}"),
        )],
        npm_command,
        cwd,
        true,
    )?;
    validate_managed_npm_package(&root, &target, &name, &parsed.manifest_name)?;
    Ok(target)
}

fn managed_npm_root(cwd: &Path, global: bool) -> Result<PathBuf, String> {
    if global {
        crate::config::agent_dir()
            .map(|agent| agent.join("npm"))
            .map_err(|error| error.to_string())
    } else {
        Ok(cwd.join(".pi/npm"))
    }
}

fn validate_managed_npm_package(
    install_root: &Path,
    target: &Path,
    relative_name: &Path,
    manifest_name: &str,
) -> Result<(), String> {
    let canonical_root = std::fs::canonicalize(install_root)
        .map(normalize_config_path)
        .map_err(|error| {
            format!(
                "could not resolve npm root {}: {error}",
                install_root.display()
            )
        })?;
    let expected = install_root.join("node_modules").join(relative_name);
    if !paths_equal(&expected, target) {
        return Err(format!(
            "refusing unexpected npm package target {}",
            target.display()
        ));
    }
    let canonical_target = std::fs::canonicalize(target)
        .map(normalize_config_path)
        .map_err(|error| format!("npm did not install {}: {error}", target.display()))?;
    let manifest = target.join("package.json");
    let canonical_manifest = std::fs::canonicalize(&manifest)
        .map(normalize_config_path)
        .map_err(|error| format!("npm did not install {}: {error}", manifest.display()))?;
    if !path_is_within(&canonical_target, &canonical_root)
        || !path_is_within(&canonical_manifest, &canonical_target)
        || !manifest.is_file()
    {
        return Err(format!(
            "npm installed an unsafe package target {}",
            target.display()
        ));
    }
    validate_npm_manifest_name(&manifest, manifest_name)?;
    Ok(())
}

fn validate_npm_manifest_name(manifest: &Path, expected: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(manifest).map_err(|error| {
        format!(
            "could not read npm manifest {}: {error}",
            manifest.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse npm manifest {}: {error}",
            manifest.display()
        )
    })?;
    let actual = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("npm manifest {} has no package name", manifest.display()))?;
    if actual != expected {
        return Err(format!(
            "npm installed manifest name `{actual}` does not match expected package `{expected}`"
        ));
    }
    Ok(())
}

fn install_npm_at(
    target: &Path,
    install_spec: &str,
    source_spec: &str,
    force: bool,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    validate_standalone_npm_manager(npm_command)?;
    install_npm_at_with(
        target,
        install_spec,
        source_spec,
        force,
        npm_command,
        |stage, npm_spec| {
            let install_args = npm_command.install_args(&[npm_spec.to_string()], stage);
            let mut command = Command::new(npm_command.program());
            command
                .args(npm_command.combined_args(&install_args))
                .current_dir(stage)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let status = command.status().map_err(|error| {
                format!(
                    "could not execute {} (install the configured package manager first): {error}",
                    npm_command.program()
                )
            })?;
            if !status.success() {
                return Err(format!(
                    "{} install exited with {status}",
                    npm_command.program()
                ));
            }
            Ok(())
        },
    )
}

fn install_npm_at_for_startup(
    target: &Path,
    install_spec: &str,
    source_spec: &str,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    validate_standalone_npm_manager(npm_command)?;
    install_npm_at_with_mode(
        target,
        install_spec,
        source_spec,
        true,
        npm_command,
        NpmExecutionMode::StartupRemediation,
        |stage, npm_spec| {
            let install_args = npm_command.install_args(&[npm_spec.to_string()], stage);
            npm_command
                .run_startup_remediation(&install_args, stage)
                .map_err(|error| {
                    format!(
                        "could not restore npm package with {} during startup: {error}",
                        npm_command.program()
                    )
                })
        },
    )
}

fn validate_standalone_npm_manager(npm_command: &NpmCommand) -> Result<(), String> {
    if matches!(
        npm_command.manager_kind(),
        NpmManagerKind::Pnpm | NpmManagerKind::Bun
    ) {
        return Err(format!(
            "{} cannot safely install an npm package into the standalone rpi package store; use a native Pi npm store entry instead",
            match npm_command.manager_kind() {
                NpmManagerKind::Pnpm => "pnpm",
                NpmManagerKind::Bun => "bun",
                _ => unreachable!(),
            }
        ));
    }
    Ok(())
}

fn install_npm_at_with(
    target: &Path,
    install_spec: &str,
    source_spec: &str,
    force: bool,
    npm_command: &NpmCommand,
    install: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<PathBuf, String> {
    install_npm_at_with_mode(
        target,
        install_spec,
        source_spec,
        force,
        npm_command,
        NpmExecutionMode::Interactive,
        install,
    )
}

fn install_npm_at_with_mode(
    target: &Path,
    install_spec: &str,
    source_spec: &str,
    force: bool,
    npm_command: &NpmCommand,
    npm_mode: NpmExecutionMode,
    install: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let stage = tempfile::tempdir()
        .map_err(|error| format!("could not create npm staging dir: {error}"))?;
    let npm_spec = install_spec.strip_prefix("npm:").unwrap_or(install_spec);
    install(stage.path(), npm_spec)?;
    let parsed = npm_package_spec_checked(install_spec)?;
    let package_name = PathBuf::from(&parsed.install_name);
    let node_modules = stage.path().join("node_modules");
    let installed = node_modules.join(&package_name);
    let installed_metadata = std::fs::symlink_metadata(&installed).ok();
    let installed_canonical = std::fs::canonicalize(&installed)
        .ok()
        .map(normalize_config_path);
    let node_modules_canonical = std::fs::canonicalize(&node_modules)
        .ok()
        .map(normalize_config_path);
    if !installed_metadata
        .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        || !installed_canonical.as_ref().is_some_and(|path| {
            node_modules_canonical
                .as_ref()
                .is_some_and(|root| path.starts_with(root))
        })
    {
        return Err(format!(
            "npm installed `{install_spec}` but no safe package directory was found"
        ));
    }
    validate_npm_manifest_name(&installed.join("package.json"), &parsed.manifest_name)?;
    replace_dir_prepared(&installed, target, force, |prepared| {
        install_production_dependencies_with_mode(prepared, npm_command, npm_mode)?;
        validate_npm_manifest_name(&prepared.join("package.json"), &parsed.manifest_name)?;
        crate::packages::write_npm_source_marker(prepared, source_spec)
    })?;
    Ok(target.to_path_buf())
}

/// Refresh an installed npm package in-place. The package root is expected to
/// be the safe-name directory created by `install-pi`; replacing that directory
/// preserves the settings entry while updating its contents.
pub(crate) fn update_npm_package(
    root: &Path,
    name: &str,
    source_spec: &str,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    if !is_known_package_target(root) {
        return Err(format!(
            "refusing npm update outside a managed package store: {}",
            root.display()
        ));
    }
    let install_spec = npm_update_install_spec(name, source_spec);
    install_npm_at(root, &install_spec, source_spec, true, npm_command)
}

/// Startup-only npm recovery. Unlike the interactive update command, every
/// package-manager subprocess has a finite install budget and bounded output.
pub(crate) fn update_npm_package_for_startup(
    root: &Path,
    name: &str,
    source_spec: &str,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    if !is_known_package_target(root) {
        return Err(format!(
            "refusing npm update outside a managed package store: {}",
            root.display()
        ));
    }
    let install_spec = npm_update_install_spec(name, source_spec);
    install_npm_at_for_startup(root, &install_spec, source_spec, npm_command)
}

/// Batch-update packages installed in one native Pi npm root. The caller must
/// pass a root already recognized by package discovery; this function adds a
/// root-level advisory lock, refuses direct symlinks, preserves an existing
/// package manifest until the package manager runs, and never swaps an
/// individual package leaf. The package manager owns its normal in-place
/// `package.json`, lockfile, and `node_modules` changes, matching native Pi.
pub(crate) fn update_npm_store_root(
    root: &Path,
    packages: &[(String, String)],
    npm_command: &NpmCommand,
    cwd: &Path,
    project_trusted: bool,
) -> Result<(), String> {
    validate_recognized_npm_store_root(root, cwd, project_trusted)?;
    update_npm_store_root_with(root, packages, npm_command, |root, command, args| {
        let status = Command::new(command.program())
            .args(command.combined_args(args))
            .current_dir(root)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| {
                format!(
                    "could not execute {} for npm store update: {error}",
                    command.program()
                )
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} npm store update exited with {status}",
                command.program()
            ))
        }
    })?;
    validate_updated_npm_store_packages(root, packages)?;
    Ok(())
}

/// Startup-only native npm-store recovery with a finite subprocess budget.
pub(crate) fn update_npm_store_root_for_startup(
    root: &Path,
    packages: &[(String, String)],
    npm_command: &NpmCommand,
    cwd: &Path,
    project_trusted: bool,
) -> Result<(), String> {
    validate_recognized_npm_store_root(root, cwd, project_trusted)?;
    update_npm_store_root_with(root, packages, npm_command, |root, command, args| {
        command
            .run_startup_remediation(args, root)
            .map_err(|error| {
                format!(
                    "could not restore npm store with {} during startup: {error}",
                    command.program()
                )
            })
    })?;
    validate_updated_npm_store_packages(root, packages)?;
    Ok(())
}

fn validate_updated_npm_store_packages(
    root: &Path,
    packages: &[(String, String)],
) -> Result<(), String> {
    for (name, source_spec) in packages {
        let parsed = npm_package_spec_checked(source_spec)?;
        if parsed.install_name != *name {
            return Err(format!(
                "npm update source `{source_spec}` does not match package `{name}`"
            ));
        }
        let relative_name = PathBuf::from(name);
        let target = root.join("node_modules").join(&relative_name);
        validate_managed_npm_package(root, &target, &relative_name, &parsed.manifest_name)?;
    }
    Ok(())
}

/// Update a native Pi git checkout through a same-filesystem staging directory.
/// The caller supplies the exact managed store root and source discovered from
/// the trusted settings entry; the live checkout is not replaced until clone,
/// dependency installation, and post-build validation have all succeeded.
pub(crate) fn update_git_package(
    root: &Path,
    store_root: &Path,
    source_spec: &str,
    npm_command: &NpmCommand,
) -> Result<(), String> {
    update_git_package_with(
        root,
        store_root,
        source_spec,
        npm_command,
        |root, origin, revision, npm_command| {
            update_git_checkout_from_origin(root, store_root, origin, revision, npm_command)
        },
    )
}

fn update_git_package_with(
    root: &Path,
    store_root: &Path,
    source_spec: &str,
    npm_command: &NpmCommand,
    update: impl FnOnce(&Path, &str, Option<&str>, &NpmCommand) -> Result<(), String>,
) -> Result<(), String> {
    if !root.is_absolute() || !store_root.is_absolute() {
        return Err("refusing git update for a non-absolute managed path".to_string());
    }
    let source = crate::packages::parse_git_source(source_spec)
        .ok_or_else(|| "refusing git update without a valid configured source".to_string())?;
    validate_managed_git_root(root, store_root)?;

    let _lock = PackageSwapLock::acquire(root)?;
    // Re-resolve both paths after taking the advisory lock. A package entry
    // can be replaced by a symlink/junction between discovery and update; in
    // that case fail closed before invoking git.
    validate_managed_git_root(root, store_root)?;
    let origin = validated_git_origin(root, &source)?;
    update(root, &origin, source.revision.as_deref(), npm_command)
}

fn update_git_checkout_from_origin(
    root: &Path,
    store_root: &Path,
    origin: &str,
    revision: Option<&str>,
    npm_command: &NpmCommand,
) -> Result<(), String> {
    stage_git_checkout_update_with(
        root,
        store_root,
        origin,
        revision,
        npm_command,
        |prepared, origin, selected_revision| {
            prepare_git_checkout_for_update(prepared, origin, revision, selected_revision)
        },
        |prepared, npm_command| {
            install_git_dependencies(prepared, npm_command, NpmExecutionMode::Interactive)
        },
    )
}

fn prepare_git_checkout_for_update(
    prepared: &Path,
    origin: &str,
    explicit_revision: Option<&str>,
    selected_revision: Option<&str>,
) -> Result<(), String> {
    prepare_git_checkout_for_update_with(
        prepared,
        origin,
        explicit_revision,
        selected_revision,
        clone_git_checkout,
        fetch_git_revision,
        checkout_git_revision,
        validate_git_head,
    )
}

fn prepare_git_checkout_for_update_with(
    prepared: &Path,
    origin: &str,
    explicit_revision: Option<&str>,
    selected_revision: Option<&str>,
    clone: impl FnOnce(&str, Option<&str>, &Path) -> Result<(), String>,
    fetch: impl FnOnce(&Path, &str, &str) -> Result<(), String>,
    checkout: impl FnOnce(&Path, &str) -> Result<(), String>,
    validate: impl FnOnce(&Path, Option<&str>) -> Result<(), String>,
) -> Result<(), String> {
    let Some(revision) = explicit_revision else {
        return clone(origin, selected_revision, prepared);
    };

    // A full clone does not necessarily contain a commit that is only
    // reachable through an unadvertised ref. Native Pi explicitly fetches a
    // configured revision, so preserve that behavior in the staging checkout.
    clone(origin, None, prepared)?;
    fetch(prepared, origin, revision)?;
    checkout(prepared, "FETCH_HEAD")?;
    validate(prepared, Some("FETCH_HEAD"))
}

fn stage_git_checkout_update_with(
    root: &Path,
    store_root: &Path,
    origin: &str,
    revision: Option<&str>,
    npm_command: &NpmCommand,
    fetch: impl FnOnce(&Path, &str, Option<&str>) -> Result<(), String>,
    build: impl FnOnce(&Path, &NpmCommand) -> Result<(), String>,
) -> Result<(), String> {
    let selected_revision = revision
        .map(str::to_string)
        .or_else(|| git_upstream_branch(root))
        .or_else(|| git_origin_head_branch(root));
    let expected_origin = crate::packages::parse_git_source(origin)
        .filter(|source| source.revision.is_none())
        .ok_or_else(|| "refusing to stage an invalid git origin".to_string())?;
    let original_head = validated_git_head(root)?;
    let update_marker = git_update_marker_path(root)?;
    let had_update_marker = validate_git_update_marker(&update_marker)?;
    let parent = root
        .parent()
        .ok_or_else(|| format!("git package has no parent: {}", root.display()))?;
    let stage = tempfile::Builder::new()
        .prefix(".rpi-git-update-")
        .tempdir_in(parent)
        .map_err(|error| format!("could not create git update staging directory: {error}"))?;
    let prepared = stage.path().join("package");

    fetch(&prepared, origin, selected_revision.as_deref())?;
    validate_cloned_git_origin(&prepared, origin, &expected_origin)?;
    let prepared_head = validated_git_head(&prepared)?;

    build(&prepared, npm_command)?;
    crate::packages::remove_package_source_marker(&prepared)?;
    validate_cloned_git_origin(&prepared, origin, &expected_origin)?;
    if validated_git_head(&prepared)? != prepared_head {
        return Err("refusing git update because the staged HEAD changed during build".to_string());
    }
    if !package_target_is_valid(&prepared) {
        return Err("staged git checkout does not contain a valid Pi package".to_string());
    }

    // The network/build phase can take minutes. Repeat every live-target check
    // immediately before activation so an out-of-band replacement cannot be
    // overwritten merely because it passed the initial preflight.
    validate_managed_git_root(root, store_root)?;
    if validated_git_head(root)? != original_head {
        return Err(
            "refusing git update because the installed checkout changed during staging".to_string(),
        );
    }
    let current_origin = validated_git_origin(root, &expected_origin)?;
    if current_origin != origin {
        return Err("refusing git update because origin changed during staging".to_string());
    }

    swap_prepared_dir(&prepared, root, true)?;
    if had_update_marker {
        if let Err(error) = std::fs::remove_file(&update_marker) {
            eprintln!(
                "warning: updated git package but could not remove stale marker {}: {error}",
                update_marker.display()
            );
        }
    }
    Ok(())
}

fn validate_managed_git_root(root: &Path, store_root: &Path) -> Result<(), String> {
    let canonical_root = normalize_update_path(std::fs::canonicalize(root).map_err(|error| {
        format!(
            "could not resolve git package root {}: {error}",
            root.display()
        )
    })?);
    let canonical_store =
        normalize_update_path(std::fs::canonicalize(store_root).map_err(|error| {
            format!(
                "could not resolve git store root {}: {error}",
                store_root.display()
            )
        })?);
    if !paths_equal(&canonical_root, root)
        || !paths_equal(&canonical_store, store_root)
        || !path_is_within(&canonical_root, &canonical_store)
        || !is_real_directory(&canonical_root)
        || !is_real_git_metadata(&canonical_root.join(".git"))
    {
        return Err(format!(
            "refusing git update outside the managed store: {}",
            root.display()
        ));
    }
    let store_components = canonical_store.components().count();
    let mut relative = canonical_root.components().skip(store_components);
    if canonical_root.components().count() <= store_components
        || relative.any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "refusing invalid managed git package path: {}",
            root.display()
        ));
    }
    Ok(())
}

const MAX_LOCAL_GIT_CONFIG_BYTES: usize = 64 * 1024;

/// Return the one explicit fetch URL recorded in this checkout only when its
/// normalized repository identity matches the trusted settings source. Local
/// includes and command-capable configuration are rejected because `git
/// fetch` would otherwise evaluate them after this check.
fn validated_git_origin(
    root: &Path,
    expected: &crate::packages::GitSpec,
) -> Result<String, String> {
    let keys = local_git_config_output(root, &["--name-only", "--list"])?;
    if !keys.status.success() || keys.stdout.len() > MAX_LOCAL_GIT_CONFIG_BYTES {
        return Err("refusing git update with unreadable local configuration".to_string());
    }
    for key in parse_nul_config_values(&keys.stdout)? {
        if dangerous_git_fetch_config_key(key) {
            return Err("refusing git update with unsafe local configuration".to_string());
        }
    }

    let origins = local_git_config_output(root, &["--get-all", "remote.origin.url"])?;
    if !origins.status.success() || origins.stdout.len() > MAX_LOCAL_GIT_CONFIG_BYTES {
        return Err("refusing git update without exactly one configured origin".to_string());
    }
    let origins = parse_nul_config_values(&origins.stdout)?;
    let [origin] = origins.as_slice() else {
        return Err("refusing git update without exactly one configured origin".to_string());
    };
    if !is_explicit_git_remote_url(origin) {
        return Err("refusing git update with a non-network or ambiguous origin".to_string());
    }
    let actual = crate::packages::parse_git_source(origin)
        .filter(|source| source.revision.is_none())
        .ok_or_else(|| "refusing git update with an invalid or dangerous origin".to_string())?;
    let actual_port = actual
        .port
        .unwrap_or_else(|| actual.transport.default_port());
    let expected_port = expected
        .port
        .unwrap_or_else(|| expected.transport.default_port());
    if actual.host != expected.host
        || actual.path != expected.path
        || actual.transport != expected.transport
        || actual_port != expected_port
        || actual.user_info != expected.user_info
    {
        return Err(
            "refusing git update because origin does not match package settings".to_string(),
        );
    }
    Ok((*origin).to_string())
}

fn local_git_config_output(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let config = root.join(".git/config");
    let metadata = std::fs::symlink_metadata(&config).map_err(|error| {
        format!(
            "could not inspect git package configuration {}: {error}",
            config.display()
        )
    })?;
    let canonical_config =
        normalize_update_path(std::fs::canonicalize(&config).map_err(|error| {
            format!(
                "could not resolve git package configuration {}: {error}",
                config.display()
            )
        })?);
    let canonical_git = normalize_update_path(
        std::fs::canonicalize(root.join(".git"))
            .map_err(|error| format!("could not resolve git package metadata: {error}"))?,
    );
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_LOCAL_GIT_CONFIG_BYTES as u64
        || !paths_equal(&canonical_config, &config)
        || !path_is_within(&canonical_config, &canonical_git)
    {
        return Err("refusing git update with redirected package configuration".to_string());
    }

    let mut command = Command::new("git");
    apply_hardened_git_environment(&mut command);
    command
        .args(["config", "--no-includes", "--null", "--file"])
        .arg(&config)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("could not inspect git package configuration: {error}"))
}

fn parse_nul_config_values(output: &[u8]) -> Result<Vec<&str>, String> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let Some(values) = output.strip_suffix(&[0]) else {
        return Err("refusing malformed git configuration output".to_string());
    };
    values
        .split(|byte| *byte == 0)
        .map(|value| {
            let value = std::str::from_utf8(value)
                .map_err(|_| "refusing non-UTF-8 git configuration".to_string())?;
            if value.is_empty() {
                Err("refusing empty git configuration value".to_string())
            } else {
                Ok(value)
            }
        })
        .collect()
}

fn dangerous_git_fetch_config_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "core.askpass"
            | "core.attributesfile"
            | "core.fsmonitor"
            | "core.gitproxy"
            | "core.hookspath"
            | "core.sshcommand"
            | "core.worktree"
            | "extensions.worktreeconfig"
            | "fetch.bundleuri"
            | "http.curloptresolve"
            | "http.extraheader"
            | "http.proxy"
            | "http.sslcainfo"
            | "http.sslcapath"
            | "http.sslverify"
            | "include.path"
    ) || (key.starts_with("includeif.") && key.ends_with(".path"))
        || (key.starts_with("http.")
            && (key.ends_with(".curloptresolve")
                || key.ends_with(".extraheader")
                || key.ends_with(".proxy")
                || key.ends_with(".sslcainfo")
                || key.ends_with(".sslcapath")
                || key.ends_with(".sslverify")))
        || (key.starts_with("url.")
            && (key.ends_with(".insteadof") || key.ends_with(".pushinsteadof")))
        || (key.starts_with("credential.") && key.ends_with(".helper"))
        || (key.starts_with("remote.")
            && (key.ends_with(".proxy") || key.ends_with(".uploadpack") || key.ends_with(".vcs")))
        || (key.starts_with("filter.")
            && (key.ends_with(".clean") || key.ends_with(".process") || key.ends_with(".smudge")))
}

fn is_explicit_git_remote_url(origin: &str) -> bool {
    if origin.is_empty()
        || origin != origin.trim()
        || origin
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    if origin.starts_with("git@") {
        return origin.contains(':');
    }
    let Some((scheme, _)) = origin.split_once("://") else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ssh" | "git"
    )
}

fn git_upstream_branch(root: &Path) -> Option<String> {
    let upstream = run_git_capture(
        root,
        ["rev-parse", "--abbrev-ref", "@{upstream}"].as_slice(),
    )
    .ok()?;
    let branch = upstream.trim().strip_prefix("origin/")?;
    is_safe_git_branch(branch).then(|| branch.to_string())
}

fn git_origin_head_branch(root: &Path) -> Option<String> {
    let symbolic = run_git_capture(
        root,
        ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"].as_slice(),
    )
    .ok()?;
    let branch = symbolic.trim().strip_prefix("refs/remotes/origin/")?;
    is_safe_git_branch(branch).then(|| branch.to_string())
}

fn git_update_marker_path(root: &Path) -> Result<PathBuf, String> {
    let parent = root
        .parent()
        .ok_or_else(|| format!("git package has no parent: {}", root.display()))?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("git package has no valid name: {}", root.display()))?;
    Ok(parent.join(format!(".{name}.pi-update-incomplete")))
}

fn validate_git_update_marker(marker: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(marker) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "git update marker must be a regular file: {}",
            marker.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "could not inspect git update marker {}: {error}",
            marker.display()
        )),
    }
}

fn is_safe_git_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.contains('\\')
        && !branch.contains('\0')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.chars().any(|ch| {
            ch.is_control() || ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[')
        })
        && !branch
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn run_git_capture(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = hardened_git_command();
    let output = command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not execute hardened git: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return if detail.is_empty() {
            Err(format!(
                "git {} exited with {}",
                args.join(" "),
                output.status
            ))
        } else {
            Err(format!(
                "git {} exited with {}: {detail}",
                args.join(" "),
                output.status
            ))
        };
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) const HARDENED_GIT_ENV_REMOVE: &[&str] = &[
    "GIT_ALLOW_PROTOCOL",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_ASKPASS",
    "GIT_ATTR_NOSYSTEM",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_EXEC_PATH",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PROXY_COMMAND",
    "GIT_QUARANTINE_PATH",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "GIT_SSL_NO_VERIFY",
    "GIT_TEMPLATE_DIR",
    "GIT_WORK_TREE",
    "SSH_ASKPASS",
];

pub(crate) fn hardened_git_null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Build command-scope Git configuration without consulting command-capable
/// system, global, or environment configuration. A short allowlist preserves
/// the standard credential helpers shipped by common Git installations; SSH
/// agent and `~/.ssh/config` authentication continue to work independently.
pub(crate) fn hardened_git_network_config_args() -> Vec<String> {
    let mut args = Vec::new();
    for value in [
        "protocol.allow=never",
        "protocol.http.allow=always",
        "protocol.https.allow=always",
        "protocol.ssh.allow=always",
        "protocol.git.allow=always",
        "protocol.ext.allow=never",
        "protocol.file.allow=never",
        "credential.helper=",
        "credential.interactive=false",
        "gc.auto=0",
        "maintenance.auto=false",
        if cfg!(windows) {
            "core.hooksPath=NUL"
        } else {
            "core.hooksPath=/dev/null"
        },
    ] {
        args.push("-c".to_string());
        args.push(value.to_string());
    }
    for helper in safe_configured_credential_helpers() {
        args.push("-c".to_string());
        args.push(format!("credential.helper={helper}"));
    }
    args
}

fn hardened_git_command() -> Command {
    let mut command = Command::new("git");
    command.args(hardened_git_network_config_args());
    apply_hardened_git_environment(&mut command);
    command
}

fn apply_hardened_git_environment(command: &mut Command) {
    for name in HARDENED_GIT_ENV_REMOVE {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", hardened_git_null_config())
        .env("GIT_CONFIG_GLOBAL", hardened_git_null_config())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("SSH_ASKPASS_REQUIRE", "never");
}

fn safe_configured_credential_helpers() -> &'static [String] {
    static HELPERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    HELPERS.get_or_init(|| {
        let mut helpers = Vec::new();
        for scope in ["--system", "--global"] {
            let mut command = Command::new("git");
            command.args([
                "config",
                scope,
                "--no-includes",
                "--null",
                "--get-all",
                "credential.helper",
            ]);
            for name in HARDENED_GIT_ENV_REMOVE {
                command.env_remove(name);
            }
            let Ok(output) = command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
            else {
                continue;
            };
            if !output.status.success() || output.stdout.len() > MAX_LOCAL_GIT_CONFIG_BYTES {
                continue;
            }
            for value in output.stdout.split(|byte| *byte == 0) {
                let Ok(value) = std::str::from_utf8(value) else {
                    continue;
                };
                if is_safe_standard_credential_helper(value)
                    && !helpers.iter().any(|helper| helper == value)
                {
                    helpers.push(value.to_string());
                }
            }
        }
        helpers
    })
}

fn is_safe_standard_credential_helper(value: &str) -> bool {
    matches!(
        value,
        "cache" | "libsecret" | "manager" | "manager-core" | "osxkeychain" | "store" | "wincred"
    )
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_real_git_metadata(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        // A worktree's `.git` file points at another repository. Automatic
        // package updates only operate on self-contained checkouts, so reject
        // files (including `gitdir:` pointers) and symlinks outright.
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn normalize_update_path(path: PathBuf) -> PathBuf {
    normalize_config_path(path)
}

fn update_npm_store_root_with(
    root: &Path,
    packages: &[(String, String)],
    npm_command: &NpmCommand,
    run: impl FnOnce(&Path, &NpmCommand, &[String]) -> Result<(), String>,
) -> Result<(), String> {
    if packages.is_empty() {
        return Ok(());
    }
    if !root.is_absolute() || root.file_name() != Some(std::ffi::OsStr::new("npm")) {
        return Err(format!(
            "refusing npm store update outside a recognized absolute npm root: {}",
            root.display()
        ));
    }
    let specs = packages
        .iter()
        .map(|(name, source_spec)| npm_root_update_spec(name, source_spec))
        .collect::<Result<Vec<_>, _>>()?;
    let parent = root
        .parent()
        .ok_or_else(|| format!("npm store root has no parent: {}", root.display()))?;
    ensure_real_directory_tree(parent)?;
    let _lock = PackageSwapLock::acquire(root)?;
    ensure_npm_store_root(root)?;
    for (name, _) in packages {
        let relative_name = npm_module_name_checked(name)?;
        validate_npm_target_before_mutation(root, &relative_name, true)?;
    }
    let args = npm_command.install_args(&specs, root);
    run(root, npm_command, &args)
}

fn npm_root_update_spec(name: &str, source_spec: &str) -> Result<String, String> {
    let name = name.trim();
    let configured = source_spec
        .strip_prefix("npm:")
        .unwrap_or(source_spec)
        .trim();
    if name.is_empty() || configured.is_empty() {
        return Err("npm update source and package name must not be empty".to_string());
    }
    if npm_module_name(configured) != name {
        return Err(format!(
            "npm update source `{source_spec}` does not match package `{name}`"
        ));
    }
    Ok(if configured == name {
        format!("{name}@latest")
    } else {
        configured.to_string()
    })
}

fn ensure_npm_store_root(root: &Path) -> Result<(), String> {
    ensure_real_directory_tree(root)?;

    let manifest = root.join("package.json");
    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "npm store package.json must be a regular file: {}",
            manifest.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::write(
            &manifest,
            "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}\n",
        )
        .map_err(|error| format!("could not create npm store package.json: {error}")),
        Err(error) => return Err(format!("could not inspect npm store package.json: {error}")),
    }?;

    let ignore = root.join(".gitignore");
    match std::fs::symlink_metadata(&ignore) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "npm store .gitignore must be a regular file: {}",
            ignore.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&ignore, "*\n!.gitignore\n")
                .map_err(|error| format!("could not create npm store .gitignore: {error}"))
        }
        Err(error) => Err(format!("could not inspect npm store .gitignore: {error}")),
    }
}

fn validate_npm_target_before_mutation(
    root: &Path,
    relative_name: &Path,
    create_parents: bool,
) -> Result<(), String> {
    let canonical_root = std::fs::canonicalize(root)
        .map(normalize_config_path)
        .map_err(|error| format!("could not resolve npm root {}: {error}", root.display()))?;
    if !paths_equal(&canonical_root, root) {
        return Err(format!("refusing redirected npm root {}", root.display()));
    }

    let node_modules = root.join("node_modules");
    match std::fs::symlink_metadata(&node_modules) {
        Ok(_) => ensure_real_directory_tree(&node_modules)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parents => {
            ensure_real_directory_tree(&node_modules)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect npm node_modules {}: {error}",
                node_modules.display()
            ));
        }
    }

    if let Some(parent) = relative_name
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let scope = node_modules.join(parent);
        match std::fs::symlink_metadata(&scope) {
            Ok(_) => ensure_real_directory_tree(&scope)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parents => {
                ensure_real_directory_tree(&scope)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "could not inspect npm scope {}: {error}",
                    scope.display()
                ));
            }
        }
    }

    let target = node_modules.join(relative_name);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) => {
            let canonical_target = std::fs::canonicalize(&target)
                .map(normalize_config_path)
                .map_err(|error| {
                    format!("could not resolve npm target {}: {error}", target.display())
                })?;
            if !metadata.is_dir() && !metadata.file_type().is_symlink() {
                return Err(format!(
                    "npm package target is not a directory: {}",
                    target.display()
                ));
            }
            if !path_is_within(&canonical_target, &canonical_root) {
                return Err(format!(
                    "refusing npm package target outside its managed root: {}",
                    target.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect npm target {}: {error}",
                target.display()
            ));
        }
    }
    Ok(())
}

fn npm_update_install_spec(name: &str, source_spec: &str) -> String {
    let configured = source_spec
        .strip_prefix("npm:")
        .unwrap_or(source_spec)
        .trim();
    let install_spec = if configured == name {
        format!("npm:{name}@latest")
    } else {
        format!("npm:{configured}")
    };
    install_spec
}

#[derive(Debug, Clone)]
struct UninstallOptions {
    spec: String,
    global: bool,
}

/// Remove a Pi package installed by `rpi install-pi` and disable its settings
/// entry. Source directories outside rpi/pi package stores are never deleted.
pub fn uninstall(args: &[String]) -> i32 {
    let options = match parse_uninstall_args(args) {
        Ok(options) => options,
        Err(message) if message == "help" => {
            print_uninstall_help();
            return 0;
        }
        Err(message) => {
            eprintln!("error: {message}");
            print_uninstall_help();
            return 2;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not determine current directory: {error}");
            return 1;
        }
    };
    let project_trusted = match project_trust_for_package_operation(&cwd, options.global) {
        Ok(trusted) => trusted,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let is_npm = is_npm_uninstall_spec(&cwd, &options.spec);
    let is_git = is_git_install_spec(&options.spec);
    let root = find_installed_root(&cwd, &options);
    let (settings_after_removal, removed_settings) =
        match plan_settings_removal(&cwd, &options.spec, root.as_deref(), options.global) {
            Ok(plan) => plan,
            Err(error) => {
                eprintln!("error: refusing to uninstall with unreadable settings: {error}");
                return 1;
            }
        };
    let mut removed_files = false;
    if is_npm {
        let npm_command = match NpmCommand::resolve(&cwd, project_trusted) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        };
        match uninstall_managed_npm(&cwd, options.global, &options.spec, &npm_command) {
            Ok(Some(removed)) => {
                removed_files |= removed;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        }
    }
    if is_git {
        if let Some(root) = root.as_deref() {
            let managed_legacy = is_managed_package_root(&cwd, root);
            let managed_git = is_managed_git_package_root(&cwd, root, options.global);
            if root.exists() && (managed_legacy || managed_git) {
                let git_marker = if managed_git {
                    let marker = match git_update_marker_path(root) {
                        Ok(marker) => marker,
                        Err(error) => {
                            eprintln!("error: {error}");
                            return 1;
                        }
                    };
                    match validate_git_update_marker(&marker) {
                        Ok(exists) => Some((marker, exists)),
                        Err(error) => {
                            eprintln!("error: {error}");
                            return 1;
                        }
                    }
                } else {
                    None
                };
                match std::fs::remove_dir_all(root) {
                    Ok(()) => {
                        if let Some((marker, true)) = git_marker {
                            if let Err(error) = std::fs::remove_file(&marker) {
                                eprintln!(
                                    "error: removed git package but could not remove marker {}: {error}",
                                    marker.display()
                                );
                                return 1;
                            }
                        }
                        if managed_git {
                            prune_empty_git_parents(&cwd, root, options.global);
                        }
                        println!("removed Pi package {}", root.display());
                        removed_files = true;
                    }
                    Err(error) => {
                        eprintln!("error: could not remove {}: {error}", root.display());
                        return 1;
                    }
                }
            } else if root.exists() {
                println!(
                    "disabled Pi package at {}; source directory was left intact",
                    root.display()
                );
            }
        }
    } else if let Some(root) = root.as_deref().filter(|root| root.exists()) {
        println!(
            "disabled Pi package at {}; source directory was left intact",
            root.display()
        );
    }
    // Persist the settings change only after every filesystem/package-manager
    // operation succeeds. A failed uninstall therefore remains retryable.
    if removed_settings > 0 {
        if let Err(error) = save_settings_for_install(&cwd, options.global, &settings_after_removal)
        {
            eprintln!(
                "error: package files were removed but settings could not be updated: {error}"
            );
            return 1;
        }
    }
    if !removed_files && removed_settings == 0 {
        println!("Pi package is not installed: {}", options.spec);
    } else if removed_settings > 0 && !removed_files {
        println!("disabled Pi package {}", options.spec);
    }
    0
}

fn is_npm_uninstall_spec(cwd: &Path, spec: &str) -> bool {
    if spec.starts_with("npm:") {
        return true;
    }
    if spec.starts_with("file:") || is_git_install_spec(spec) {
        return false;
    }
    let local = PathBuf::from(spec);
    let local = if local.is_absolute() {
        local
    } else {
        cwd.join(local)
    };
    !local.exists() && looks_like_npm(spec)
}

fn uninstall_managed_npm(
    cwd: &Path,
    global: bool,
    spec: &str,
    npm_command: &NpmCommand,
) -> Result<Option<bool>, String> {
    let relative_name = npm_module_name_checked(spec)?;
    let package_name = relative_name.to_string_lossy().replace('\\', "/");
    let root = managed_npm_root(cwd, global)?;
    validate_recognized_npm_store_root(&root, cwd, true)?;
    if std::fs::symlink_metadata(&root)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(None);
    }
    validate_existing_npm_store_root(&root)?;
    let target = root.join("node_modules").join(&relative_name);
    let existed = match std::fs::symlink_metadata(&target) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "could not inspect npm package target {}: {error}",
                target.display()
            ));
        }
    };
    if !existed && !npm_store_manifest_tracks_package(&root, &package_name)? {
        return Ok(None);
    }
    let _lock = PackageSwapLock::acquire(&root)?;
    validate_existing_npm_store_root(&root)?;
    validate_npm_target_before_mutation(&root, &relative_name, false)?;
    let args = npm_command.uninstall_args(&package_name, &root);
    let status = Command::new(npm_command.program())
        .args(npm_command.combined_args(&args))
        .current_dir(&root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            format!(
                "could not execute {} for npm package removal: {error}",
                npm_command.program()
            )
        })?;
    if !status.success() {
        return Err(format!(
            "{} npm package removal exited with {status}",
            npm_command.program()
        ));
    }
    Ok(Some(existed))
}

fn npm_store_manifest_tracks_package(root: &Path, package_name: &str) -> Result<bool, String> {
    let manifest = root.join("package.json");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "could not read npm store manifest {}: {error}",
                manifest.display()
            ));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse npm store manifest {}: {error}",
            manifest.display()
        )
    })?;
    Ok(["dependencies", "devDependencies", "optionalDependencies"]
        .into_iter()
        .any(|key| {
            value
                .get(key)
                .and_then(serde_json::Value::as_object)
                .is_some_and(|entries| entries.contains_key(package_name))
        }))
}

fn validate_existing_npm_store_root(root: &Path) -> Result<(), String> {
    ensure_real_directory_tree(root)?;
    for path in [root.join("package.json"), root.join("node_modules")] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || (path.file_name() == Some(std::ffi::OsStr::new("package.json"))
                        && !metadata.is_file())
                    || (path.file_name() == Some(std::ffi::OsStr::new("node_modules"))
                        && !metadata.is_dir()) =>
            {
                return Err(format!(
                    "refusing redirected npm store entry {}",
                    path.display()
                ));
            }
            Ok(_) if path.file_name() == Some(std::ffi::OsStr::new("node_modules")) => {
                let canonical = std::fs::canonicalize(&path)
                    .map(normalize_config_path)
                    .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
                if !paths_equal(&canonical, &path) {
                    return Err(format!(
                        "refusing redirected npm store entry {}",
                        path.display()
                    ));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", path.display()));
            }
        }
    }
    Ok(())
}

fn parse_uninstall_args(args: &[String]) -> Result<UninstallOptions, String> {
    let mut spec = None;
    let mut global = false;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Err("help".into()),
            "--global" | "-g" => global = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown uninstall-pi option `{value}`"));
            }
            value => {
                if spec.replace(value.to_string()).is_some() {
                    return Err("uninstall-pi accepts exactly one package spec".into());
                }
            }
        }
    }
    Ok(UninstallOptions {
        spec: spec.ok_or_else(|| "missing npm, git, or local package spec".to_string())?,
        global,
    })
}

fn find_installed_root(cwd: &Path, options: &UninstallOptions) -> Option<PathBuf> {
    if is_git_install_spec(&options.spec) {
        return find_installed_git_root(cwd, options);
    }
    let raw = options.spec.strip_prefix("file:").unwrap_or(&options.spec);
    let package_key = package_dir_name(&options.spec);
    let mut candidates = Vec::new();
    let direct = PathBuf::from(raw);
    let is_local =
        !is_npm_uninstall_spec(cwd, &options.spec) && !is_git_install_spec(&options.spec);
    if is_local && !direct.is_absolute() {
        if options.global {
            if let Ok(agent) = crate::config::agent_dir() {
                candidates.push(agent.join(&direct));
            }
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".pi/agent").join(&direct));
            }
        } else {
            candidates.push(cwd.join(".rpi").join(&direct));
            candidates.push(cwd.join(".pi").join(&direct));
        }
    }
    if direct.is_absolute() || raw.starts_with('.') {
        candidates.push(if direct.is_absolute() {
            direct
        } else {
            cwd.join(direct)
        });
    }
    let add_store = |store: PathBuf, candidates: &mut Vec<PathBuf>| {
        candidates.push(store.join(&package_key));
    };
    if !options.global {
        if let Ok(name) = npm_module_name_checked(&options.spec) {
            if options.spec.starts_with("npm:") {
                candidates.push(cwd.join(".pi/npm/node_modules").join(name));
            }
        }
        add_store(cwd.join(".rpi/packages"), &mut candidates);
        add_store(cwd.join(".pi/packages"), &mut candidates);
    } else {
        if let Ok(agent) = crate::config::agent_dir() {
            if let Ok(name) = npm_module_name_checked(&options.spec) {
                if options.spec.starts_with("npm:") {
                    candidates.push(agent.join("npm/node_modules").join(name));
                }
            }
            add_store(agent.join("packages"), &mut candidates);
        }
        if let Some(home) = dirs::home_dir() {
            if let Ok(name) = npm_module_name_checked(&options.spec) {
                if options.spec.starts_with("npm:") {
                    candidates.push(home.join(".pi/agent/npm/node_modules").join(name));
                }
            }
            add_store(home.join(".pi/agent/packages"), &mut candidates);
        }
    }
    candidates
        .into_iter()
        .find_map(|path| std::fs::canonicalize(path).ok())
}

fn find_installed_git_root(cwd: &Path, options: &UninstallOptions) -> Option<PathBuf> {
    let source = crate::packages::parse_git_source(&options.spec)?;
    let relative = Path::new(&source.host).join(&source.path);
    let mut native_targets = Vec::new();
    let mut legacy_targets = Vec::new();
    if options.global {
        if let Ok(agent) = crate::config::agent_dir() {
            native_targets.push(agent.join("git").join(&relative));
            legacy_targets.push(agent.join("packages").join(package_dir_name(&options.spec)));
        }
        if let Some(home) = dirs::home_dir() {
            native_targets.push(home.join(".pi/agent/git").join(&relative));
            legacy_targets.push(
                home.join(".pi/agent/packages")
                    .join(package_dir_name(&options.spec)),
            );
        }
    } else {
        native_targets.push(cwd.join(".pi/git").join(&relative));
        native_targets.push(cwd.join(".rpi/git").join(&relative));
        legacy_targets.push(
            cwd.join(".rpi/packages")
                .join(package_dir_name(&options.spec)),
        );
        legacy_targets.push(
            cwd.join(".pi/packages")
                .join(package_dir_name(&options.spec)),
        );
    }

    for target in native_targets {
        let Ok(root) = std::fs::canonicalize(target).map(normalize_config_path) else {
            continue;
        };
        if is_managed_git_package_root(cwd, &root, options.global) {
            return Some(root);
        }
    }
    legacy_targets.into_iter().find_map(|target| {
        let root = normalize_config_path(std::fs::canonicalize(target).ok()?);
        legacy_git_root_matches_source(cwd, &root, options.global, &source).then_some(root)
    })
}

fn legacy_git_root_matches_source(
    cwd: &Path,
    root: &Path,
    global: bool,
    expected: &crate::packages::GitSpec,
) -> bool {
    if !is_managed_package_root(cwd, root)
        || !is_real_git_metadata(&root.join(".git"))
        || (!global
            && !is_direct_package_below_store(root, cwd, Path::new(".rpi/packages"))
            && !is_direct_package_below_store(root, cwd, Path::new(".pi/packages")))
    {
        return false;
    }
    let Ok(origin) = run_git_capture(root, &["config", "--get", "remote.origin.url"]) else {
        return false;
    };
    crate::packages::parse_git_source(&origin)
        .is_some_and(|actual| actual.host == expected.host && actual.path == expected.path)
}

fn package_dir_name(spec: &str) -> String {
    if let Some(git) = crate::packages::parse_git_source(spec) {
        return safe_name(
            git.path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("package"),
        );
    }
    let raw = spec.strip_prefix("git:").unwrap_or(spec);
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return safe_name(
            raw.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("package"),
        );
    }
    safe_name(raw)
}

fn is_managed_git_package_root(cwd: &Path, root: &Path, global: bool) -> bool {
    let mut stores = Vec::new();
    if global {
        if let Ok(agent) = crate::config::agent_dir() {
            stores.push(agent.join("git"));
        }
        if let Some(home) = dirs::home_dir() {
            stores.push(home.join(".pi/agent/git"));
        }
    } else {
        stores.push(cwd.join(".pi/git"));
        stores.push(cwd.join(".rpi/git"));
    }
    let root = normalize_config_path(match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(_) => return false,
    });
    if !is_real_directory(&root) {
        return false;
    }
    stores.into_iter().any(|store| {
        let canonical_store = std::fs::canonicalize(&store)
            .ok()
            .map(normalize_config_path);
        canonical_store.as_ref().is_some_and(|canonical_store| {
            paths_equal(canonical_store, &store)
                && path_is_within(&root, canonical_store)
                && root.components().count() >= canonical_store.components().count() + 3
        })
    })
}

fn prune_empty_git_parents(cwd: &Path, target: &Path, global: bool) {
    let mut stores = Vec::new();
    if global {
        if let Ok(agent) = crate::config::agent_dir() {
            stores.push(agent.join("git"));
        }
        if let Some(home) = dirs::home_dir() {
            stores.push(home.join(".pi/agent/git"));
        }
    } else {
        stores.push(cwd.join(".pi/git"));
        stores.push(cwd.join(".rpi/git"));
    }
    let Some(store) = stores.into_iter().find(|store| {
        std::fs::canonicalize(store)
            .map(normalize_config_path)
            .is_ok_and(|canonical| paths_equal(&canonical, store) && path_is_within(target, store))
    }) else {
        return;
    };
    let Some(mut current) = target.parent().map(Path::to_path_buf) else {
        return;
    };
    while !paths_equal(&current, &store) && path_is_within(&current, &store) {
        let is_empty = std::fs::read_dir(&current)
            .ok()
            .is_some_and(|mut entries| entries.next().is_none());
        if !is_empty || std::fs::remove_dir(&current).is_err() {
            break;
        }
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        current = parent;
    }
}

fn is_managed_package_root(cwd: &Path, root: &Path) -> bool {
    if is_direct_package_below_store(root, cwd, Path::new(".rpi/packages"))
        || is_direct_package_below_store(root, cwd, Path::new(".pi/packages"))
    {
        return true;
    }
    if let Ok(agent) = crate::config::agent_dir() {
        if is_direct_package_below_store(root, &agent, Path::new("packages")) {
            return true;
        }
    }
    if let Some(home) = dirs::home_dir() {
        if is_direct_package_below_store(root, &home, Path::new(".pi/agent/packages")) {
            return true;
        }
    }
    false
}

fn is_direct_package_below_store(root: &Path, base: &Path, relative_store: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let Some(store) = canonical_safe_store(base, relative_store) else {
        return false;
    };
    let Ok(relative) = root.strip_prefix(store) else {
        return false;
    };
    let mut components = relative.components();
    components.next().is_some() && components.next().is_none()
}

fn canonical_safe_store(base: &Path, relative_store: &Path) -> Option<PathBuf> {
    let base = std::fs::canonicalize(base).ok()?;
    let store = std::fs::canonicalize(base.join(relative_store)).ok()?;
    (store == base.join(relative_store) && store.starts_with(&base)).then_some(store)
}

fn canonical_managed_store_for_target(target: &Path) -> Option<PathBuf> {
    let parent = std::fs::canonicalize(target.parent()?).ok()?;
    if let Ok(agent) = crate::config::agent_dir() {
        if canonical_safe_store(&agent, Path::new("packages")).as_ref() == Some(&parent) {
            return Some(parent);
        }
    }
    if let Some(home) = dirs::home_dir() {
        if canonical_safe_store(&home, Path::new(".pi/agent/packages")).as_ref() == Some(&parent) {
            return Some(parent);
        }
    }

    let project_config = parent.parent()?;
    let config_name = project_config.file_name()?;
    if config_name != ".rpi" && config_name != ".pi" {
        return None;
    }
    let project = std::fs::canonicalize(project_config.parent()?).ok()?;
    let expected = project.join(config_name).join("packages");
    (parent == expected).then_some(parent)
}

fn target_is_missing_without_symlink(target: &Path) -> bool {
    std::fs::symlink_metadata(target)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn target_is_package_at_store(target: &Path, store: &Path) -> bool {
    let Some(target_parent) = target.parent() else {
        return false;
    };
    let Ok(parent) = std::fs::canonicalize(target_parent) else {
        return false;
    };
    let Some(leaf) = target.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let valid_parent = (parent == store && !leaf.starts_with('@'))
        || (!leaf.starts_with('@')
            && parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('@') && name.len() > 1)
            && parent.parent() == Some(store));
    if !valid_parent {
        return false;
    }
    match std::fs::canonicalize(target) {
        Ok(root) => root.parent() == Some(parent.as_path()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            target_is_missing_without_symlink(target)
        }
        Err(_) => false,
    }
}

fn is_known_npm_package_target(target: &Path) -> bool {
    if let Ok(agent) = crate::config::agent_dir() {
        if canonical_safe_store(&agent, Path::new("npm/node_modules"))
            .is_some_and(|store| target_is_package_at_store(target, &store))
        {
            return true;
        }
    }
    if let Some(home) = dirs::home_dir() {
        if canonical_safe_store(&home, Path::new(".pi/agent/npm/node_modules"))
            .is_some_and(|store| target_is_package_at_store(target, &store))
        {
            return true;
        }
    }

    let Some(target_parent) = target.parent() else {
        return false;
    };
    let Ok(parent) = std::fs::canonicalize(target_parent) else {
        return false;
    };
    let store = if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('@') && name.len() > 1)
    {
        let Some(store) = parent.parent() else {
            return false;
        };
        store
    } else {
        parent.as_path()
    };
    if store.file_name() != Some(std::ffi::OsStr::new("node_modules"))
        || store.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("npm"))
        || store
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            != Some(std::ffi::OsStr::new(".pi"))
    {
        return false;
    }
    let Some(project) = store.parent().and_then(Path::parent).and_then(Path::parent) else {
        return false;
    };
    let Ok(project) = std::fs::canonicalize(project) else {
        return false;
    };
    store == project.join(".pi/npm/node_modules") && target_is_package_at_store(target, store)
}

fn is_known_git_package_target(target: &Path) -> bool {
    if !target.is_absolute() {
        return false;
    }
    if let Ok(agent) = crate::config::agent_dir() {
        if target_is_git_package_at_store(target, &agent.join("git")) {
            return true;
        }
    }
    if let Some(home) = dirs::home_dir() {
        if target_is_git_package_at_store(target, &home.join(".pi/agent/git")) {
            return true;
        }
    }
    target.ancestors().any(|store| {
        store.file_name() == Some(std::ffi::OsStr::new("git"))
            && store
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".pi" || name == ".rpi")
            && target_is_git_package_at_store(target, store)
    })
}

fn target_is_git_package_at_store(target: &Path, store: &Path) -> bool {
    let Ok(canonical_store) = std::fs::canonicalize(store).map(normalize_config_path) else {
        return false;
    };
    if !paths_equal(&canonical_store, store) {
        return false;
    }
    let Some(parent) = target.parent() else {
        return false;
    };
    let Ok(canonical_parent) = std::fs::canonicalize(parent).map(normalize_config_path) else {
        return false;
    };
    if !paths_equal(&canonical_parent, parent)
        || !path_is_within(&canonical_parent, &canonical_store)
    {
        return false;
    }
    let store_depth = store.components().count();
    let relative = target.components().skip(store_depth).collect::<Vec<_>>();
    if relative.len() < 3
        || relative
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::canonicalize(target)
                .map(normalize_config_path)
                .is_ok_and(|canonical| paths_equal(&canonical, target))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        _ => false,
    }
}

fn is_known_package_target(target: &Path) -> bool {
    if let Some(store) = canonical_managed_store_for_target(target) {
        return match std::fs::canonicalize(target) {
            Ok(root) => root.parent() == Some(store.as_path()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                target_is_missing_without_symlink(target)
            }
            Err(_) => false,
        };
    }
    is_known_npm_package_target(target) || is_known_git_package_target(target)
}

fn plan_settings_removal(
    cwd: &Path,
    spec: &str,
    root: Option<&Path>,
    global: bool,
) -> Result<(crate::settings::Settings, usize), String> {
    let mut settings = load_settings_for_install(cwd, global).map_err(|error| error.to_string())?;
    let Some(packages) = settings.packages.as_mut() else {
        return Ok((settings, 0));
    };
    let removed = remove_matching_settings_entries(cwd, packages, spec, root, global);
    if packages.is_empty() {
        settings.packages = None;
    }
    Ok((settings, removed))
}

fn remove_matching_settings_entries(
    cwd: &Path,
    packages: &mut Vec<crate::settings::PackageSetting>,
    spec: &str,
    root: Option<&Path>,
    global: bool,
) -> usize {
    let before = packages.len();
    packages.retain(|entry| {
        let source = entry.source();
        if package_settings_sources_match(cwd, source, spec) {
            return false;
        }
        let matches_root = root.is_some_and(|root| {
            resolve_settings_source_root(cwd, source, global)
                .is_some_and(|candidate| paths_equal(&candidate, root))
        });
        !matches_root
    });
    before - packages.len()
}

fn package_settings_sources_match(cwd: &Path, existing: &str, input: &str) -> bool {
    let existing = existing.trim();
    let input = input.trim();
    if existing == input {
        return true;
    }
    if let Some(existing_npm) = existing.strip_prefix("npm:") {
        if is_npm_uninstall_spec(cwd, input) {
            let existing_name = npm_module_name(existing_npm);
            let input_name = npm_module_name(input);
            return !existing_name.is_empty()
                && !input_name.is_empty()
                && existing_name.eq_ignore_ascii_case(&input_name);
        }
        return false;
    }
    match (
        crate::packages::parse_git_source(existing),
        crate::packages::parse_git_source(input),
    ) {
        (Some(existing), Some(input)) => {
            return existing.host == input.host && existing.path == input.path;
        }
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    false
}

fn resolve_settings_source_root(cwd: &Path, source: &str, global: bool) -> Option<PathBuf> {
    if source.starts_with("npm:") || is_git_install_spec(source) {
        return find_installed_root(
            cwd,
            &UninstallOptions {
                spec: source.to_string(),
                global,
            },
        );
    }
    let raw = source.strip_prefix("file:").unwrap_or(source).trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let mut candidates = Vec::new();
    if path.is_absolute() {
        candidates.push(path);
    } else if global {
        if let Ok(agent) = crate::config::agent_dir() {
            candidates.push(agent.join(&path));
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".pi/agent").join(&path));
        }
    } else {
        candidates.push(cwd.join(".rpi").join(&path));
        candidates.push(cwd.join(".pi").join(&path));
        candidates.push(cwd.join(&path));
    }
    candidates.into_iter().find_map(|candidate| {
        let candidate = if candidate.is_file()
            && candidate.file_name().and_then(|name| name.to_str()) == Some("package.json")
        {
            candidate.parent()?.to_path_buf()
        } else {
            candidate
        };
        std::fs::canonicalize(candidate)
            .ok()
            .map(normalize_config_path)
    })
}

fn npm_module_name(spec: &str) -> String {
    crate::packages::parse_npm_package_spec(spec)
        .map(|parsed| parsed.install_name)
        .unwrap_or_default()
}

fn install_git(
    cwd: &Path,
    global: bool,
    spec: &str,
    force: bool,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    install_git_with_npm_mode(
        cwd,
        global,
        spec,
        force,
        npm_command,
        NpmExecutionMode::Interactive,
    )
}

fn install_git_with_npm_mode(
    cwd: &Path,
    global: bool,
    spec: &str,
    force: bool,
    npm_command: &NpmCommand,
    npm_mode: NpmExecutionMode,
) -> Result<PathBuf, String> {
    let raw = match spec.strip_prefix("git:") {
        Some(rest) if !rest.starts_with("//") => rest.trim(),
        _ => spec.trim(),
    };
    let (repo, revision) = split_git_install_ref(raw);
    if revision
        .as_deref()
        .is_some_and(|value| !is_safe_git_revision(value))
    {
        return Err("refusing invalid git revision from package settings".to_string());
    }
    let url = normalize_git_clone_url(&repo)?;
    let parsed = crate::packages::parse_git_source(spec)
        .ok_or_else(|| format!("invalid git package source `{spec}`"))?;
    let install_root = if global {
        crate::config::agent_dir()
            .map(|agent| agent.join("git"))
            .map_err(|error| error.to_string())?
    } else {
        cwd.join(".pi/git")
    };
    let target = install_root.join(&parsed.host).join(&parsed.path);
    let destination = target
        .parent()
        .ok_or_else(|| format!("git package target has no parent: {}", target.display()))?;
    prepare_managed_destination(destination)?;
    ensure_managed_store_ignore(&install_root)?;
    validate_install_target(destination, &target)?;
    let update_marker = git_update_marker_path(&target)?;
    let had_update_marker = validate_git_update_marker(&update_marker)?;
    if target.exists() {
        if !force {
            return Err(format!(
                "{} already exists; use --force to replace it",
                target.display()
            ));
        }
    }
    let stage = tempfile::Builder::new()
        .prefix(".rpi-git-stage-")
        .tempdir_in(destination)
        .map_err(|error| format!("could not create git staging directory: {error}"))?;
    let prepared = stage.path().join("package");
    clone_git_checkout(&url, revision.as_deref(), &prepared)?;
    install_git_dependencies(&prepared, npm_command, npm_mode)?;
    crate::packages::remove_package_source_marker(&prepared)?;
    let _lock = PackageSwapLock::acquire(&target)?;
    validate_install_target(destination, &target)?;
    recover_interrupted_swap(&target)?;
    validate_install_target(destination, &target)?;
    swap_prepared_dir(&prepared, &target, force)?;
    if had_update_marker {
        std::fs::remove_file(&update_marker).map_err(|error| {
            format!(
                "installed git package but could not remove stale update marker {}: {error}",
                update_marker.display()
            )
        })?;
    }
    Ok(target)
}

fn clone_git_checkout(url: &str, revision: Option<&str>, target: &Path) -> Result<(), String> {
    let expected = crate::packages::parse_git_source(url)
        .filter(|source| source.revision.is_none())
        .ok_or_else(|| "refusing to clone an invalid git source".to_string())?;
    let clone_cwd = target
        .parent()
        .ok_or_else(|| format!("git staging target has no parent: {}", target.display()))?;
    let mut command = hardened_git_command();
    let status = command
        // Native Pi performs a full clone. Besides matching that behavior, it
        // keeps non-default branches available for the checkout below.
        .args(["clone", "--no-recurse-submodules", "--"])
        .arg(url)
        .arg(target)
        // Never let an invoking project's repository config participate in a
        // managed clone. The staging parent is newly allocated by rpi.
        .current_dir(clone_cwd)
        .env("GIT_CEILING_DIRECTORIES", clone_cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute hardened git clone: {error}"))?;
    if !status.success() {
        return Err(format!("git clone exited with {status}"));
    }
    validate_cloned_git_origin(target, url, &expected)?;
    let Some(revision) = revision else {
        validate_git_head(target, None)?;
        return Ok(());
    };
    checkout_git_revision(target, revision)?;
    validate_cloned_git_origin(target, url, &expected)?;
    validate_git_head(target, Some(revision))
}

fn fetch_git_revision(root: &Path, origin: &str, revision: &str) -> Result<(), String> {
    if !is_safe_git_revision(revision) {
        return Err("refusing invalid git revision from package settings".to_string());
    }
    let mut command = hardened_git_command();
    let status = command
        .args(["fetch", "--depth", "1", "--no-recurse-submodules", "--"])
        .arg(origin)
        .arg(revision)
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute hardened git fetch: {error}"))?;
    if !status.success() {
        return Err(format!("git fetch exited with {status}"));
    }
    Ok(())
}

fn checkout_git_revision(root: &Path, revision: &str) -> Result<(), String> {
    let mut command = hardened_git_command();
    let status = command
        .args(["checkout", revision])
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute hardened git checkout: {error}"))?;
    if !status.success() {
        return Err(format!("git checkout exited with {status}"));
    }
    Ok(())
}

fn validate_cloned_git_origin(
    root: &Path,
    requested_url: &str,
    expected: &crate::packages::GitSpec,
) -> Result<(), String> {
    let canonical_root = std::fs::canonicalize(root)
        .map(normalize_config_path)
        .map_err(|error| format!("could not resolve cloned git package: {error}"))?;
    if !paths_equal(&canonical_root, root)
        || !is_real_directory(root)
        || !is_real_git_metadata(&root.join(".git"))
    {
        return Err("refusing a redirected or incomplete cloned git checkout".to_string());
    }
    let origin = validated_git_origin(root, expected)?;
    if origin != requested_url {
        return Err("refusing cloned git checkout whose origin changed during clone".to_string());
    }
    Ok(())
}

fn validate_git_head(root: &Path, revision: Option<&str>) -> Result<(), String> {
    let head = validated_git_head(root)?;
    if let Some(revision) = revision {
        let commit_ref = format!("{revision}^{{commit}}");
        let selected = run_git_capture(root, &["rev-parse", "--verify", &commit_ref])?;
        let selected = normalized_git_commit(&selected)
            .ok_or_else(|| "selected git revision is not a commit".to_string())?;
        if selected != head {
            return Err("git checkout did not select the requested revision".to_string());
        }
    }
    Ok(())
}

fn validated_git_head(root: &Path) -> Result<String, String> {
    let head = run_git_capture(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    normalized_git_commit(&head).ok_or_else(|| "git checkout has an invalid HEAD".to_string())
}

fn normalized_git_commit(value: &str) -> Option<String> {
    let value = value.trim();
    ((value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

pub(crate) fn install_missing_git_package(
    cwd: &Path,
    global: bool,
    spec: &str,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    install_git(cwd, global, spec, false, npm_command)
}

pub(crate) fn install_missing_git_package_for_startup(
    cwd: &Path,
    global: bool,
    spec: &str,
    npm_command: &NpmCommand,
) -> Result<PathBuf, String> {
    install_git_with_npm_mode(
        cwd,
        global,
        spec,
        false,
        npm_command,
        NpmExecutionMode::StartupRemediation,
    )
}

fn install_git_dependencies(
    root: &Path,
    npm_command: &NpmCommand,
    npm_mode: NpmExecutionMode,
) -> Result<(), String> {
    install_production_dependencies_with_mode(root, npm_command, npm_mode)
}

fn ensure_managed_store_ignore(root: &Path) -> Result<(), String> {
    ensure_real_directory_tree(root)?;
    let ignore = root.join(".gitignore");
    match std::fs::symlink_metadata(&ignore) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "managed store .gitignore must be a regular file: {}",
            ignore.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&ignore, "*\n!.gitignore\n")
                .map_err(|error| format!("could not create managed store .gitignore: {error}"))
        }
        Err(error) => Err(format!(
            "could not inspect managed store .gitignore: {error}"
        )),
    }
}

fn split_git_install_ref(raw: &str) -> (String, Option<String>) {
    let path_start = if raw.starts_with("git@") {
        raw.find(':').map(|index| index + 1)
    } else if let Some(scheme_end) = raw.find("://") {
        let authority_start = scheme_end + 3;
        raw[authority_start..]
            .find('/')
            .map(|index| authority_start + index + 1)
    } else {
        raw.find('/').map(|index| index + 1)
    };
    let Some(path_start) = path_start else {
        return (raw.to_string(), None);
    };
    let Some(offset) = raw[path_start..].find('@') else {
        return (raw.to_string(), None);
    };
    let separator = path_start + offset;
    let repo = &raw[..separator];
    let revision = &raw[separator + 1..];
    if repo.is_empty() || revision.is_empty() {
        return (raw.to_string(), None);
    }
    (repo.to_string(), Some(revision.to_string()))
}

fn normalize_git_clone_url(repo: &str) -> Result<String, String> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err("git repository URL must not be empty".to_string());
    }
    if repo.contains('\0')
        || repo.contains('\\')
        || repo.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(format!("invalid git repository source `{repo}`"));
    }
    if let Some(index) = repo.find("://") {
        let scheme = repo[..index].to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "ssh" | "git") {
            return Err(format!("unsupported git transport `{scheme}`"));
        }
        let authority_and_path = &repo[index + 3..];
        let Some((_, path)) = authority_and_path.split_once('/') else {
            return Err(format!("invalid git repository source `{repo}`"));
        };
        if path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!("invalid git repository source `{repo}`"));
        }
        return Ok(repo.to_string());
    }
    if repo.starts_with("git@") {
        let Some((_, path)) = repo.split_once(':') else {
            return Err(format!("invalid git repository source `{repo}`"));
        };
        if path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!("invalid git repository source `{repo}`"));
        }
        return Ok(repo.to_string());
    }
    // Pi accepts hosted shorthand only with the `git:` source prefix. The
    // clone command itself needs an explicit transport for that form.
    if repo.split('/').count() >= 3
        && !repo
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Ok(format!("https://{repo}"));
    }
    Err(format!("invalid git repository source `{repo}`"))
}

#[cfg(test)]
fn git_package_name(repo: &str) -> String {
    let leaf = repo
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("package");
    let leaf = leaf.strip_suffix(".git").unwrap_or(leaf);
    safe_name(leaf)
}

fn is_safe_git_revision(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && !value.contains('\0')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.chars().any(|ch| {
            ch.is_control() || ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[')
        })
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn install_local(cwd: &Path, spec: &str) -> Result<PathBuf, String> {
    let source = PathBuf::from(spec.strip_prefix("file:").unwrap_or(spec));
    let source = if source.is_absolute() {
        source
    } else {
        cwd.join(source)
    };
    let source = std::fs::canonicalize(&source)
        .map(normalize_config_path)
        .map_err(|error| {
            format!(
                "local package path was not found at {}: {error}",
                source.display()
            )
        })?;
    let root = if source.is_dir() {
        source
    } else if source.is_file() && source.file_name() == Some(std::ffi::OsStr::new("package.json")) {
        source
            .parent()
            .ok_or_else(|| format!("local package manifest has no parent: {}", source.display()))?
            .to_path_buf()
    } else {
        return Err(format!(
            "local package must be a directory or package.json: {}",
            source.display()
        ));
    };
    Ok(root)
}

fn replace_dir_prepared(
    source: &Path,
    target: &Path,
    force: bool,
    prepare: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if target.exists() {
        if !force {
            return Err(format!(
                "{} already exists; use --force to replace it",
                target.display()
            ));
        }
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("package target has no parent: {}", target.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let prepared = tempfile::Builder::new()
        .prefix(".rpi-package-stage-")
        .tempdir_in(parent)
        .map_err(|error| format!("could not create package staging directory: {error}"))?;
    copy_dir(source, prepared.path())
        .map_err(|error| format!("could not copy package: {error}"))?;
    prepare(prepared.path())?;
    let _lock = PackageSwapLock::acquire(target)?;
    recover_interrupted_swap(target)?;
    swap_prepared_dir(prepared.path(), target, force)
}

struct PackageSwapLock {
    file: Option<std::fs::File>,
}

impl PackageSwapLock {
    fn acquire(target: &Path) -> Result<Self, String> {
        let parent = target
            .parent()
            .ok_or_else(|| format!("package target has no parent: {}", target.display()))?;
        let leaf = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("package");
        let path = parent.join(format!(".{leaf}.rpi-update.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "could not open package lock for {}: {error}",
                    target.display()
                )
            })?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            format!(
                "could not lock package {} (another update may be running): {error}",
                target.display()
            )
        })?;
        Ok(Self { file: Some(file) })
    }
}

impl Drop for PackageSwapLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs2::FileExt::unlock(&file);
        }
    }
}

/// Recover or clean up an interrupted update at a known managed/native npm
/// package target. A lock file is opened only when a matching backup exists.
pub(crate) fn recover_configured_package_root(target: &Path) -> Result<(), String> {
    if !is_known_package_target(target) || package_backup_dirs(target)?.is_empty() {
        return Ok(());
    }
    let _lock = PackageSwapLock::acquire(target)?;
    recover_interrupted_swap(target)
}

fn recover_interrupted_swap(target: &Path) -> Result<(), String> {
    if target.exists() {
        if package_target_is_valid(target) {
            remove_stale_package_backups(target)?;
        }
        return Ok(());
    }
    let backups = package_backup_dirs(target)?;
    match backups.as_slice() {
        [] => Ok(()),
        [backup] => std::fs::rename(backup, target).map_err(|error| {
            format!(
                "could not restore interrupted package update from {}: {error}",
                backup.display()
            )
        }),
        _ => Err(format!(
            "could not recover {}: multiple interrupted update backups found",
            target.display()
        )),
    }
}

fn package_backup_dirs(target: &Path) -> Result<Vec<PathBuf>, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("package target has no parent: {}", target.display()))?;
    let leaf = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("package");
    let prefix = format!(".{leaf}.rpi-backup-");
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(parent)
        .map_err(|error| format!("could not inspect package backups: {error}"))?
    {
        let entry = entry.map_err(|error| format!("could not inspect package backup: {error}"))?;
        let matches_name = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        if matches_name
            && entry
                .file_type()
                .map_err(|error| format!("could not inspect package backup: {error}"))?
                .is_dir()
        {
            backups.push(entry.path());
        }
    }
    backups.sort();
    Ok(backups)
}

fn package_target_is_valid(target: &Path) -> bool {
    if !target.is_dir() {
        return false;
    }
    let manifest_valid = std::fs::read_to_string(target.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|value| value.is_object());
    manifest_valid
        || [
            "skills",
            "prompts",
            "themes",
            "extensions",
            "SYSTEM.md",
            "APPEND_SYSTEM.md",
        ]
        .iter()
        .any(|resource| target.join(resource).exists())
}

fn remove_stale_package_backups(target: &Path) -> Result<(), String> {
    for backup in package_backup_dirs(target)? {
        std::fs::remove_dir_all(&backup).map_err(|error| {
            format!(
                "could not remove stale package backup {}: {error}",
                backup.display()
            )
        })?;
    }
    Ok(())
}

fn swap_prepared_dir(prepared: &Path, target: &Path, force: bool) -> Result<(), String> {
    if !target.exists() {
        return std::fs::rename(prepared, target)
            .map_err(|error| format!("could not activate {}: {error}", target.display()));
    }
    if !force {
        return Err(format!(
            "{} already exists; use --force to replace it",
            target.display()
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("package target has no parent: {}", target.display()))?;
    let leaf = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("package");
    let backup = parent.join(format!(
        ".{leaf}.rpi-backup-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::rename(target, &backup)
        .map_err(|error| format!("could not back up {}: {error}", target.display()))?;
    if let Err(error) = std::fs::rename(prepared, target) {
        return match std::fs::rename(&backup, target) {
            Ok(()) => Err(format!(
                "could not activate replacement for {}: {error}",
                target.display()
            )),
            Err(rollback) => Err(format!(
                "could not activate replacement for {}: {error}; rollback at {} also failed: {rollback}",
                target.display(),
                backup.display()
            )),
        };
    }
    if let Err(error) = std::fs::remove_dir_all(&backup) {
        eprintln!(
            "warning: updated package but could not remove backup {}: {error}",
            backup.display()
        );
    }
    Ok(())
}

fn copy_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if matches!(entry.file_name().to_str(), Some("node_modules" | ".git")) {
            continue;
        }
        let from = entry.path();
        let to = target.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(from, to)?;
        }
    }
    Ok(())
}

fn install_production_dependencies_with_mode(
    root: &Path,
    npm_command: &NpmCommand,
    npm_mode: NpmExecutionMode,
) -> Result<(), String> {
    if !root.join("package.json").is_file() {
        return Ok(());
    }
    let install_args = if npm_command.is_configured() {
        vec!["install".to_string()]
    } else {
        ["install", "--omit=dev"].map(String::from).to_vec()
    };
    run_dependency_npm_command(root, npm_command, &install_args, npm_mode, false)?;
    let mut host_packages = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(peers) = manifest
                .get("peerDependencies")
                .and_then(|value| value.as_object())
            {
                for (name, version) in peers {
                    let version = version.as_str().unwrap_or("*");
                    host_packages.push(if version == "*" {
                        name.to_string()
                    } else {
                        format!("{name}@{version}")
                    });
                }
                if peers.contains_key("@earendil-works/pi-coding-agent") {
                    // Pi's coding-agent package imports this host runtime from
                    // its UI barrel, although it is not declared as a peer.
                    host_packages.push("@earendil-works/pi-server".to_string());
                }
            }
        }
    }
    if !host_packages.is_empty() && npm_command.manager_kind() == NpmManagerKind::Npm {
        let mut host_args = ["install", "--omit=dev", "--no-package-lock", "--no-save"]
            .map(String::from)
            .to_vec();
        host_args.extend(host_packages);
        run_dependency_npm_command(root, npm_command, &host_args, npm_mode, true)?;
    }
    Ok(())
}

fn run_dependency_npm_command(
    root: &Path,
    npm_command: &NpmCommand,
    args: &[String],
    npm_mode: NpmExecutionMode,
    host_dependencies: bool,
) -> Result<(), String> {
    if npm_mode == NpmExecutionMode::StartupRemediation {
        return npm_command
            .run_startup_remediation(args, root)
            .map_err(|error| {
                format!(
                    "could not install {} during startup remediation: {error}",
                    if host_dependencies {
                        "Pi host dependencies"
                    } else {
                        "package dependencies"
                    }
                )
            });
    }

    let status = Command::new(npm_command.program())
        .args(npm_command.combined_args(args))
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            if host_dependencies {
                format!(
                    "could not install Pi host dependencies with {}: {error}",
                    npm_command.program()
                )
            } else {
                format!(
                    "could not execute {} for package dependencies: {error}",
                    npm_command.program()
                )
            }
        })?;
    if status.success() {
        Ok(())
    } else if host_dependencies {
        Err(format!(
            "{} host dependency install exited with {status}",
            npm_command.program()
        ))
    } else {
        Err(format!(
            "{} install exited with {status}",
            npm_command.program()
        ))
    }
}

fn normalize_config_path(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(local);
        }
    }
    path
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = normalize_config_path(left.to_path_buf());
    let right = normalize_config_path(right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    } else {
        left == right
    }
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = normalize_config_path(path.to_path_buf());
    let root = normalize_config_path(root.to_path_buf());
    let mut path_components = path.components();
    for root_component in root.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        if !path_components_equal(path_component, root_component) {
            return false;
        }
    }
    true
}

fn validate_recognized_npm_store_root(
    root: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Result<(), String> {
    if !root.is_absolute() || root.file_name() != Some(std::ffi::OsStr::new("npm")) {
        return Err(format!(
            "refusing npm store update outside a recognized absolute npm root: {}",
            root.display()
        ));
    }
    let mut allowed = Vec::new();
    if let Ok(agent) = crate::config::agent_dir() {
        allowed.push(agent.join("npm"));
    }
    if let Some(home) = dirs::home_dir() {
        allowed.push(home.join(".pi/agent/npm"));
    }
    if project_trusted {
        allowed.push(cwd.join(".pi/npm"));
    }
    if !allowed.iter().any(|candidate| paths_equal(candidate, root)) {
        return Err(format!(
            "refusing npm store update outside a configured package store: {}",
            root.display()
        ));
    }
    Ok(())
}

fn npm_module_name_checked(spec: &str) -> Result<PathBuf, String> {
    npm_package_spec_checked(spec).map(|parsed| PathBuf::from(parsed.install_name))
}

fn npm_package_spec_checked(spec: &str) -> Result<crate::packages::ParsedNpmPackageSpec, String> {
    crate::packages::parse_npm_package_spec(spec)
        .ok_or_else(|| format!("refusing invalid npm package spec `{spec}`"))
}

fn looks_like_npm(spec: &str) -> bool {
    !spec.contains(std::path::MAIN_SEPARATOR) && !spec.contains('/') && !spec.ends_with(".json")
}

pub fn print_help() {
    println!("Usage: rpi install-pi [options] <spec>\n\nInstall a Pi npm/git/local package.\n\nSpecs:\n  npm:@scope/package@1.0.0\n  npm:alias@npm:@scope/package@^1\n  git:github.com/user/repo@v1\n  ./local-package\n\nOptions:\n  --global, -g  Use global agent settings and managed stores\n  --force, -f   Replace an existing package\n  --help, -h    Show this help");
}

fn print_uninstall_help() {
    println!("Usage: rpi uninstall-pi [options] <spec>\n\nRemove an installed Pi npm/git/local package and disable it in settings.\n\nSpecs:\n  npm:@scope/package\n  git:github.com/user/repo\n  ./local-package\n\nOptions:\n  --global, -g  Use global agent settings and managed stores\n  --help, -h    Show this help\n\nAliases:\n  rpi uninstall pi <spec>");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failing_npm_command(script: &Path) -> NpmCommand {
        #[cfg(windows)]
        {
            std::fs::write(
                script,
                "[Console]::Error.Write('startup-bounded'); exit 7\n",
            )
            .unwrap();
            let program = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/WindowsPowerShell/v1.0/powershell.exe")
                .to_string_lossy()
                .into_owned();
            return NpmCommand::from_argv(Some(&[
                program,
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-File".to_string(),
                script.to_string_lossy().into_owned(),
            ]))
            .unwrap();
        }
        #[cfg(unix)]
        {
            std::fs::write(script, "printf startup-bounded >&2\nexit 7\n").unwrap();
            NpmCommand::from_argv(Some(&[
                "/bin/sh".to_string(),
                script.to_string_lossy().into_owned(),
            ]))
            .unwrap()
        }
    }

    fn run_test_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
        assert!(
            output.status.success(),
            "git {:?} exited with {}: {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn initialize_test_git_package(root: &Path, origin: &str, state: &str) -> String {
        std::fs::create_dir_all(root).unwrap();
        run_test_git(root, &["init", "--initial-branch", "main"]);
        run_test_git(root, &["config", "user.email", "rpi-test@example.invalid"]);
        run_test_git(root, &["config", "user.name", "rpi-test"]);
        std::fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        std::fs::write(root.join("state.txt"), format!("{state}\n")).unwrap();
        run_test_git(root, &["add", "package.json", "state.txt"]);
        run_test_git(root, &["commit", "-m", state]);
        run_test_git(root, &["remote", "add", "origin", origin]);
        run_test_git(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn parses_install_specs_and_flags() {
        let options = parse_args(&["--global".into(), "npm:@scope/pkg@1.2.3".into()]).unwrap();
        assert!(options.global);
        assert_eq!(options.spec, "npm:@scope/pkg@1.2.3");
        assert_eq!(package_name(&options.spec), "scope__pkg");
        assert_eq!(npm_module_name(&options.spec), "@scope/pkg");
    }

    #[test]
    fn startup_git_dependency_mode_returns_bounded_process_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("package");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        let script = tmp.path().join(if cfg!(windows) {
            "failing-npm.ps1"
        } else {
            "failing-npm.sh"
        });
        let command = failing_npm_command(&script);

        let error = install_git_dependencies(&root, &command, NpmExecutionMode::StartupRemediation)
            .unwrap_err();

        assert!(error.contains("startup remediation"), "{error}");
        assert!(error.contains("startup-bounded"), "{error}");
    }

    #[test]
    fn install_git_sources_split_slash_refs_and_normalize_shorthand() {
        for spec in [
            "git:github.com/example/repo@feature/branch",
            "https://github.com/example/repo.git@feature/branch",
            "ssh://git@github.com/example/repo@feature/branch",
            "git://github.com/example/repo@feature/branch",
            "git:git@github.com:example/repo@feature/branch",
        ] {
            assert!(is_git_install_spec(spec), "spec={spec}");
            let raw = match spec.strip_prefix("git:") {
                Some(rest) if !rest.starts_with("//") => rest,
                _ => spec,
            };
            let (repo, revision) = split_git_install_ref(raw);
            assert_eq!(revision.as_deref(), Some("feature/branch"), "spec={spec}");
            let clone_url = normalize_git_clone_url(&repo).unwrap();
            assert!(clone_url.contains("github.com"), "spec={spec}");
            assert_eq!(git_package_name(&repo), "repo", "spec={spec}");
        }
    }

    #[test]
    fn install_git_source_rejects_unsafe_revision() {
        let (repo, revision) = split_git_install_ref("github.com/example/repo@../escape");
        assert_eq!(repo, "github.com/example/repo");
        assert!(!is_safe_git_revision(revision.as_deref().unwrap()));
        assert!(normalize_git_clone_url(&repo).is_ok());
    }

    #[test]
    fn settings_entries_are_relative_to_their_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".rpi/packages/package");
        assert_eq!(npm_settings_spec("demo"), "npm:demo");
        assert_eq!(
            npm_settings_spec("npm:@scope/demo@^1"),
            "npm:@scope/demo@^1"
        );
        assert_eq!(
            settings_entry(&root, tmp.path(), false).unwrap(),
            format!("file:{}", Path::new("packages/package").display())
        );
    }

    #[test]
    fn local_install_references_the_source_without_copying_it() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let source = tmp.path().join("local-package");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("package.json"), r#"{"name":"local-package"}"#).unwrap();

        let installed = install_local(&project, "../local-package").unwrap();
        assert!(paths_equal(
            &installed,
            &std::fs::canonicalize(&source).unwrap()
        ));
        assert!(!project.join(".rpi/packages/local-package").exists());

        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        let entry = settings_entry(&installed, &project, false).unwrap();
        let relative = Path::new(entry.strip_prefix("file:").unwrap());
        assert!(paths_equal(
            &std::fs::canonicalize(project.join(".rpi").join(relative)).unwrap(),
            &installed
        ));
    }

    #[test]
    fn npm_names_cannot_be_interpreted_as_command_options() {
        for spec in ["npm:-demo", "npm:@-scope/demo", "npm:@scope/-demo"] {
            assert!(npm_module_name_checked(spec).is_err(), "spec={spec}");
        }
    }

    #[test]
    fn interrupted_native_git_swap_is_recovered() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".pi/git/github.com/example/repo");
        let backup = target
            .parent()
            .unwrap()
            .join(".repo.rpi-backup-00000000000000000000000000000001");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("package.json"), r#"{"name":"repo"}"#).unwrap();

        recover_configured_package_root(&target).unwrap();
        assert!(target.join("package.json").is_file());
        assert!(!backup.exists());
    }

    #[test]
    fn package_source_matching_uses_npm_and_git_identity_without_cross_kind_collisions() {
        let cwd = Path::new(".");
        assert!(package_settings_sources_match(
            cwd,
            "npm:@scope/demo@^1",
            "npm:@scope/demo@beta"
        ));
        assert!(package_settings_sources_match(
            cwd,
            "npm:alias@npm:real@^1",
            "npm:alias@npm:other@beta"
        ));
        assert!(!package_settings_sources_match(
            cwd,
            "npm:alias@npm:real@^1",
            "npm:real@^1"
        ));
        assert!(package_settings_sources_match(
            cwd,
            "git:https://github.com/example/repo.git@main",
            "git:git@github.com:example/repo@release/v2"
        ));
        assert!(!package_settings_sources_match(
            cwd,
            "file:packages/demo",
            "npm:demo"
        ));
    }

    #[test]
    fn npm_package_names_cannot_be_option_arguments() {
        assert!(npm_module_name_checked("npm:-rf").is_err());
        assert!(npm_module_name_checked("npm:--workspace").is_err());
        assert_eq!(
            npm_module_name_checked("npm:@scope/valid-name@^1").unwrap(),
            PathBuf::from("@scope/valid-name")
        );
        assert_eq!(
            npm_module_name_checked("npm:@scope/alias@npm:@target/real@^1").unwrap(),
            PathBuf::from("@scope/alias")
        );
        for spec in [
            "npm:alias@npm:real@npm:other",
            "npm:alias@file:../real",
            "npm:alias@npm:real@file:../other",
        ] {
            assert!(npm_module_name_checked(spec).is_err(), "spec={spec}");
        }
    }

    #[test]
    fn global_relative_settings_entries_resolve_from_the_agent_directory() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _lock = crate::config::test_support::env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let package = agent.join("packages/demo");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        let _restore = RestoreEnv(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let resolved = resolve_settings_source_root(temp.path(), "file:packages/demo", true)
            .expect("global package should resolve from its settings owner");
        assert!(paths_equal(
            &resolved,
            &std::fs::canonicalize(package).unwrap()
        ));
    }

    #[test]
    fn git_uninstall_lookup_uses_native_host_and_repository_path() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".pi/git/github.com/example/repo");
        std::fs::create_dir_all(&target).unwrap();
        let options = UninstallOptions {
            spec: "git:github.com/example/repo@feature/branch".to_string(),
            global: false,
        };
        assert!(paths_equal(
            &find_installed_root(temp.path(), &options).unwrap(),
            &std::fs::canonicalize(target).unwrap()
        ));
    }

    #[test]
    fn git_uninstall_never_falls_back_to_an_unverified_legacy_leaf() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".rpi/packages/repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"repo"}"#).unwrap();
        let options = UninstallOptions {
            spec: "git:github.com/example/repo@main".to_string(),
            global: false,
        };
        assert_eq!(find_installed_root(temp.path(), &options), None);

        git(&root, &["init"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/other/repo.git",
            ],
        );
        assert_eq!(find_installed_root(temp.path(), &options), None);

        git(
            &root,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/example/repo.git",
            ],
        );
        assert!(paths_equal(
            &find_installed_root(temp.path(), &options).unwrap(),
            &std::fs::canonicalize(root).unwrap(),
        ));
    }

    #[test]
    fn npm_store_manifest_provenance_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"name":"pi-extensions","private":true,"dependencies":{"demo":"^1"}}"#,
        )
        .unwrap();
        assert!(npm_store_manifest_tracks_package(temp.path(), "demo").unwrap());
        assert!(!npm_store_manifest_tracks_package(temp.path(), "other").unwrap());
    }

    #[test]
    fn npm_updates_preserve_ranges_and_tags() {
        assert_eq!(
            npm_update_install_spec("demo", "npm:demo"),
            "npm:demo@latest"
        );
        assert_eq!(
            npm_update_install_spec("demo", "npm:demo@^1"),
            "npm:demo@^1"
        );
        assert_eq!(
            npm_update_install_spec("demo", "npm:demo@beta"),
            "npm:demo@beta"
        );
        assert_eq!(
            npm_update_install_spec("alias", "npm:alias@npm:@scope/real@^1"),
            "npm:alias@npm:@scope/real@^1"
        );
        assert_eq!(
            npm_root_update_spec("demo", "npm:demo").unwrap(),
            "demo@latest"
        );
        assert_eq!(
            npm_root_update_spec("demo", "npm:demo@^1").unwrap(),
            "demo@^1"
        );
        assert_eq!(
            npm_root_update_spec("alias", "npm:alias@npm:real@beta").unwrap(),
            "alias@npm:real@beta"
        );
        assert!(npm_root_update_spec("demo", "npm:other@^1").is_err());
    }

    #[test]
    fn managed_npm_alias_validates_the_target_manifest_name() {
        let tmp = tempfile::tempdir().unwrap();
        let install_root = tmp.path().join("npm");
        let relative = PathBuf::from("@scope/alias");
        let target = install_root.join("node_modules").join(&relative);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(
            target.join("package.json"),
            r#"{"name":"@target/real","version":"1.0.0"}"#,
        )
        .unwrap();

        validate_managed_npm_package(&install_root, &target, &relative, "@target/real").unwrap();
        assert!(
            validate_managed_npm_package(&install_root, &target, &relative, "@target/wrong",)
                .is_err()
        );
    }

    #[test]
    fn native_store_update_does_not_overwrite_manifest_before_manager_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("npm");
        std::fs::create_dir_all(&root).unwrap();
        let manifest = "{\"name\":\"custom-root\",\"private\":true}\n";
        std::fs::write(root.join("package.json"), manifest).unwrap();
        let argv = ["mise", "exec", "--", "pnpm"].map(String::from);
        let command = NpmCommand::from_argv(Some(&argv)).unwrap();
        let packages = vec![
            ("one".to_string(), "npm:one".to_string()),
            ("@scope/two".to_string(), "npm:@scope/two@^2".to_string()),
        ];
        let root_arg = root.to_string_lossy().into_owned();

        update_npm_store_root_with(&root, &packages, &command, |actual_root, command, args| {
            assert_eq!(actual_root, root);
            assert_eq!(command.program(), "mise");
            assert_eq!(
                args,
                [
                    "install",
                    "one@latest",
                    "@scope/two@^2",
                    "--prefix",
                    &root_arg,
                    "--config.auto-install-peers=false",
                    "--config.strict-peer-dependencies=false",
                    "--config.strict-dep-builds=false",
                ]
                .map(String::from)
                .as_slice()
            );
            assert_eq!(
                std::fs::read_to_string(root.join("package.json")).unwrap(),
                manifest
            );
            Ok(())
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("package.json")).unwrap(),
            manifest
        );
        assert!(tmp.path().join(".npm.rpi-update.lock").is_file());
    }

    #[test]
    fn native_store_update_creates_only_the_missing_root_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("npm");
        let command = NpmCommand::from_argv(Some(&["bun".to_string()])).unwrap();
        let root_arg = root.to_string_lossy().into_owned();
        update_npm_store_root_with(
            &root,
            &[("demo".to_string(), "npm:demo@beta".to_string())],
            &command,
            |actual_root, _, args| {
                assert!(actual_root.join("package.json").is_file());
                assert_eq!(
                    args,
                    ["install", "demo@beta", "--cwd", &root_arg, "--omit=peer",]
                        .map(String::from)
                        .as_slice()
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(root.join("package.json")).unwrap()
            )
            .unwrap()["name"],
            "pi-extensions"
        );
    }

    #[test]
    fn pnpm_and_bun_refuse_standalone_leaf_copy_install() {
        let tmp = tempfile::tempdir().unwrap();
        for manager in ["pnpm", "bun"] {
            let command = NpmCommand::from_argv(Some(&[manager.to_string()])).unwrap();
            let error = install_npm_at(
                &tmp.path().join(manager),
                "npm:demo@latest",
                "npm:demo",
                true,
                &command,
            )
            .unwrap_err();
            assert!(error.contains("cannot safely install"), "{error}");
        }
    }

    #[test]
    fn filtered_settings_entries_are_preserved_or_removed_by_source() {
        let filtered = crate::settings::PackageSetting::Filtered(crate::settings::PackageFilter {
            source: "npm:filtered".to_string(),
            autoload: Some(false),
            extensions: None,
            skills: None,
            prompts: None,
            themes: None,
            unknown: serde_json::Map::from_iter([(
                "futureFilter".to_string(),
                serde_json::json!({"enabled": true}),
            )]),
        });
        let preserved = filtered.clone();
        let mut packages = vec![filtered];

        assert!(!enable_settings_entry(
            Path::new("."),
            &mut packages,
            "npm:filtered".to_string()
        ));
        assert_eq!(packages, vec![preserved.clone()]);
        assert!(enable_settings_entry(
            Path::new("."),
            &mut packages,
            "npm:filtered@beta".to_string()
        ));
        assert_eq!(packages.len(), 1);
        let crate::settings::PackageSetting::Filtered(updated) = &packages[0] else {
            panic!("filtered package entry was replaced instead of updated");
        };
        assert_eq!(updated.source, "npm:filtered@beta");
        assert_eq!(updated.autoload, Some(false));
        assert_eq!(
            updated.unknown.get("futureFilter"),
            Some(&serde_json::json!({"enabled": true}))
        );

        assert!(enable_settings_entry(
            Path::new("."),
            &mut packages,
            "npm:second".to_string()
        ));

        assert_eq!(
            remove_matching_settings_entries(
                Path::new("."),
                &mut packages,
                "npm:second",
                None,
                false,
            ),
            1
        );
        assert_eq!(packages.len(), 1);
        assert_eq!(
            remove_matching_settings_entries(
                Path::new("."),
                &mut packages,
                "npm:filtered",
                None,
                false,
            ),
            1
        );
        assert!(packages.is_empty());
    }

    #[test]
    fn git_settings_ref_is_replaced_by_repository_identity() {
        let mut packages = vec![crate::settings::PackageSetting::Filtered(
            crate::settings::PackageFilter {
                source: "git:github.com/example/repo@main".to_string(),
                autoload: Some(false),
                extensions: Some(vec!["extensions/index.ts".to_string()]),
                skills: None,
                prompts: None,
                themes: None,
                unknown: serde_json::Map::new(),
            },
        )];

        assert!(enable_settings_entry(
            Path::new("."),
            &mut packages,
            "git:github.com/example/repo@feature/branch".to_string(),
        ));
        assert_eq!(packages.len(), 1);
        let crate::settings::PackageSetting::Filtered(updated) = &packages[0] else {
            panic!("filtered package entry was replaced instead of updated");
        };
        assert_eq!(updated.source, "git:github.com/example/repo@feature/branch");
        assert_eq!(
            updated.extensions.as_deref(),
            Some(["extensions/index.ts".to_string()].as_slice())
        );
    }

    #[test]
    fn project_package_operations_are_enabled_by_default() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _lock = crate::config::test_support::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let _restore = RestoreEnv(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        assert!(project_trust_for_package_operation(&project, false).unwrap());
        assert!(!project_trust_for_package_operation(&project, true).unwrap());
        crate::config::set_project_trust(&project, Some(false)).unwrap();
        assert!(!project_trust_for_package_operation(&project, false).unwrap());
        crate::config::set_project_trust(&project, Some(true)).unwrap();
        assert!(project_trust_for_package_operation(&project, false).unwrap());
    }

    #[test]
    fn local_install_does_not_resolve_npm_command() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _lock = crate::config::test_support::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("settings.json"), r#"{"npmCommand":[""]}"#).unwrap();
        let _restore = RestoreEnv(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        assert!(
            npm_command_for_install(tmp.path(), true, InstallSpecKind::Local)
                .unwrap()
                .is_none()
        );
        assert!(npm_command_for_install(tmp.path(), true, InstallSpecKind::Npm).is_err());
    }

    #[test]
    fn uninstall_lookup_does_not_read_untrusted_project_settings() {
        struct RestoreEnv {
            previous: Option<std::ffi::OsString>,
        }
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let _lock = crate::config::test_support::env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let project = tmp.path().join("project");
        let external = tmp.path().join("external-package");
        std::fs::create_dir_all(project.join(".rpi")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("package.json"), r#"{"name":"external-name"}"#).unwrap();
        std::fs::write(
            project.join(".rpi/settings.json"),
            serde_json::to_vec(&serde_json::json!({
                "packages": [{
                    "source": format!("file:{}", external.display()),
                    "autoload": false
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let _restore = RestoreEnv {
            previous: std::env::var_os(crate::config::CONFIG_DIR_ENV),
        };
        std::env::set_var(crate::config::CONFIG_DIR_ENV, agent);

        let options = UninstallOptions {
            spec: "external-name".to_string(),
            global: false,
        };
        assert_eq!(find_installed_root(&project, &options), None);
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_windows_verbatim_drive_path() {
        assert_eq!(
            normalize_config_path(PathBuf::from(r"\\?\C:\packages\demo")),
            PathBuf::from(r"C:\packages\demo")
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_windows_verbatim_unc_path() {
        assert_eq!(
            normalize_config_path(PathBuf::from(r"\\?\UNC\server\share\demo")),
            PathBuf::from(r"\\server\share\demo")
        );
    }

    #[test]
    fn rejects_missing_spec() {
        assert!(parse_args(&[]).is_err());
    }

    #[test]
    fn parses_uninstall_specs_and_flags() {
        let options = parse_uninstall_args(&["--global".into(), "npm:@scope/pkg".into()]).unwrap();
        assert!(options.global);
        assert_eq!(options.spec, "npm:@scope/pkg");
    }

    #[test]
    fn rejects_multiple_uninstall_specs() {
        assert!(parse_uninstall_args(&["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn scoped_npm_update_replaces_the_supplied_root_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".pi/npm/node_modules/@scope/pkg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("old.txt"), "old").unwrap();
        assert!(is_known_npm_package_target(&root));

        let result = install_npm_at_with(
            &root,
            "npm:@scope/pkg@latest",
            "npm:@scope/pkg",
            true,
            &NpmCommand::from_argv(None).unwrap(),
            |stage, spec| {
                assert_eq!(spec, "@scope/pkg@latest");
                let staged = stage.join("node_modules/@scope/pkg");
                std::fs::create_dir_all(&staged).unwrap();
                std::fs::write(
                    staged.join("package.json"),
                    r#"{"name":"@scope/pkg","version":"1.0.0"}"#,
                )
                .unwrap();
                std::fs::write(staged.join("new.txt"), "new").unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(result, root);
        assert!(root.join("new.txt").is_file());
        assert!(!root.join("old.txt").exists());
        assert!(!tmp
            .path()
            .join(".pi/npm/node_modules/@scope/scope__pkg")
            .exists());
        let marker: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".rpi-package-source.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["kind"], "npm");
        assert_eq!(marker["spec"], "npm:@scope/pkg");
    }

    #[test]
    fn preparation_failure_leaves_existing_package_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join(".rpi/packages/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("new.txt"), "new").unwrap();
        std::fs::write(target.join("old.txt"), "old").unwrap();

        let error = replace_dir_prepared(&source, &target, true, |_prepared| {
            Err("dependency install failed".to_string())
        })
        .unwrap_err();

        assert_eq!(error, "dependency install failed");
        assert_eq!(
            std::fs::read_to_string(target.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!target.join("new.txt").exists());
    }

    #[test]
    fn stale_lock_file_does_not_block_a_new_advisory_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("packages/demo");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let lock_path = target.parent().unwrap().join(".demo.rpi-update.lock");
        std::fs::write(&lock_path, "left by interrupted process").unwrap();

        let lock = PackageSwapLock::acquire(&target).unwrap();
        drop(lock);
        assert!(lock_path.is_file());
        drop(PackageSwapLock::acquire(&target).unwrap());
    }

    #[test]
    fn interrupted_swap_recovers_missing_target_and_cleans_stale_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("packages/demo");
        let backup = tmp
            .path()
            .join("packages/.demo.rpi-backup-00000000000000000000000000000001");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("old.txt"), "old").unwrap();
        std::fs::write(backup.join("package.json"), r#"{"name":"demo"}"#).unwrap();

        recover_interrupted_swap(&target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("old.txt")).unwrap(),
            "old"
        );
        assert!(!backup.exists());

        let stale_backup = tmp
            .path()
            .join("packages/.demo.rpi-backup-00000000000000000000000000000002");
        std::fs::create_dir_all(&stale_backup).unwrap();
        std::fs::write(stale_backup.join("stale.txt"), "stale").unwrap();
        recover_interrupted_swap(&target).unwrap();
        assert!(target.join("old.txt").is_file());
        assert!(!stale_backup.exists());

        for suffix in ["manual", "1234", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
            let unrelated = tmp
                .path()
                .join(format!("packages/.demo.rpi-backup-{suffix}"));
            std::fs::create_dir_all(&unrelated).unwrap();
            recover_interrupted_swap(&target).unwrap();
            assert!(unrelated.is_dir(), "suffix={suffix}");
        }

        let absent = tmp.path().join("packages/absent");
        let manual = tmp.path().join("packages/.absent.rpi-backup-manual");
        std::fs::create_dir_all(&manual).unwrap();
        recover_interrupted_swap(&absent).unwrap();
        assert!(!absent.exists());
        assert!(manual.is_dir());
    }

    #[test]
    fn managed_uninstall_root_requires_a_canonical_direct_store_child() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".rpi/packages/demo");
        let nested = root.join("nested");
        let external = tmp.path().join("node_modules/demo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        assert!(is_direct_package_below_store(
            &root,
            tmp.path(),
            Path::new(".rpi/packages")
        ));
        assert!(!is_direct_package_below_store(
            &nested,
            tmp.path(),
            Path::new(".rpi/packages")
        ));
        assert!(!is_direct_package_below_store(
            &external,
            tmp.path(),
            Path::new(".rpi/packages")
        ));
    }

    #[test]
    fn git_metadata_rejects_gitdir_pointer_files() {
        let tmp = tempfile::tempdir().unwrap();
        let git_file = tmp.path().join(".git");
        std::fs::write(&git_file, "gitdir: ../outside/.git\n").unwrap();
        assert!(!is_real_git_metadata(&git_file));
        std::fs::remove_file(&git_file).unwrap();
        std::fs::create_dir(&git_file).unwrap();
        assert!(is_real_git_metadata(&git_file));
    }

    #[test]
    fn explicit_git_update_revision_uses_exact_fetch_and_fetch_head() {
        let tmp = tempfile::tempdir().unwrap();
        let prepared = tmp.path().join("prepared");
        let origin = "https://github.com/example/repo.git";
        let revision = "refs/pull/42/head";
        let calls = std::cell::RefCell::new(Vec::new());

        prepare_git_checkout_for_update_with(
            &prepared,
            origin,
            Some(revision),
            Some(revision),
            |actual_origin, selected, actual_prepared| {
                assert_eq!(actual_origin, origin);
                assert_eq!(selected, None);
                assert_eq!(actual_prepared, prepared);
                calls.borrow_mut().push("clone".to_string());
                Ok(())
            },
            |actual_prepared, actual_origin, selected| {
                assert_eq!(actual_prepared, prepared);
                assert_eq!(actual_origin, origin);
                assert_eq!(selected, revision);
                calls.borrow_mut().push("fetch-exact".to_string());
                Ok(())
            },
            |actual_prepared, selected| {
                assert_eq!(actual_prepared, prepared);
                assert_eq!(selected, "FETCH_HEAD");
                calls.borrow_mut().push("checkout-fetch-head".to_string());
                Ok(())
            },
            |actual_prepared, selected| {
                assert_eq!(actual_prepared, prepared);
                assert_eq!(selected, Some("FETCH_HEAD"));
                calls.borrow_mut().push("validate-fetch-head".to_string());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            calls.into_inner(),
            [
                "clone",
                "fetch-exact",
                "checkout-fetch-head",
                "validate-fetch-head"
            ]
        );
    }

    #[test]
    fn unpinned_git_update_keeps_the_selected_branch_in_the_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let prepared = tmp.path().join("prepared");
        let origin = "https://github.com/example/repo.git";

        prepare_git_checkout_for_update_with(
            &prepared,
            origin,
            None,
            Some("release/v2"),
            |actual_origin, selected, actual_prepared| {
                assert_eq!(actual_origin, origin);
                assert_eq!(selected, Some("release/v2"));
                assert_eq!(actual_prepared, prepared);
                Ok(())
            },
            |_, _, _| panic!("unpinned update must not perform an exact fetch"),
            |_, _| panic!("unpinned update must not checkout FETCH_HEAD"),
            |_, _| panic!("clone validates its own selected revision"),
        )
        .unwrap();
    }

    #[test]
    fn staged_git_update_build_failure_preserves_the_installed_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let target = store.join("github.com/example/repo");
        let origin = "https://github.com/example/repo.git";
        let old_head = initialize_test_git_package(&target, origin, "old");
        std::fs::write(target.join("old-only.txt"), "keep\n").unwrap();
        let command = NpmCommand::from_argv(Some(&["unused".to_string()])).unwrap();

        let error = stage_git_checkout_update_with(
            &target,
            &store,
            origin,
            None,
            &command,
            |prepared, expected_origin, revision| {
                assert_eq!(expected_origin, origin);
                assert_eq!(revision, None);
                initialize_test_git_package(prepared, origin, "new");
                Ok(())
            },
            |_prepared, _| Err("dependency build failed".to_string()),
        )
        .unwrap_err();

        assert_eq!(error, "dependency build failed");
        assert_eq!(run_test_git(&target, &["rev-parse", "HEAD"]), old_head);
        assert_eq!(
            std::fs::read_to_string(target.join("state.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "old\n"
        );
        assert!(target.join("old-only.txt").is_file());
        assert!(package_backup_dirs(&target).unwrap().is_empty());
        assert!(!target.parent().unwrap().read_dir().unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(str::to_string))
                .is_some_and(|name| name.starts_with(".rpi-git-update-"))
        }));
    }

    #[test]
    fn staged_git_update_swaps_only_after_validation_and_cleans_legacy_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let target = store.join("github.com/example/repo");
        let origin = "https://github.com/example/repo.git";
        let old_head = initialize_test_git_package(&target, origin, "old");
        std::fs::write(target.join("old-only.txt"), "remove\n").unwrap();
        let marker = git_update_marker_path(&target).unwrap();
        std::fs::write(&marker, "legacy in-place update\n").unwrap();
        let command = NpmCommand::from_argv(Some(&["unused".to_string()])).unwrap();
        let mut staged_head = None;

        stage_git_checkout_update_with(
            &target,
            &store,
            origin,
            None,
            &command,
            |prepared, _, revision| {
                assert_eq!(revision, None);
                staged_head = Some(initialize_test_git_package(prepared, origin, "new"));
                Ok(())
            },
            |prepared, _| {
                std::fs::create_dir_all(prepared.join("node_modules/demo")).unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_ne!(staged_head.as_deref(), Some(old_head.as_str()));
        assert_eq!(
            run_test_git(&target, &["rev-parse", "HEAD"]),
            staged_head.unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(target.join("state.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "new\n"
        );
        assert!(!target.join("old-only.txt").exists());
        assert!(target.join("node_modules/demo").is_dir());
        assert!(!marker.exists());
        assert!(package_backup_dirs(&target).unwrap().is_empty());
    }

    #[test]
    fn git_swap_activation_failure_restores_the_previous_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("store/github.com/example/repo");
        let origin = "https://github.com/example/repo.git";
        let old_head = initialize_test_git_package(&target, origin, "old");
        let missing_prepared = target.parent().unwrap().join("missing-prepared");

        let error = swap_prepared_dir(&missing_prepared, &target, true).unwrap_err();

        assert!(error.contains("could not activate replacement"), "{error}");
        assert_eq!(run_test_git(&target, &["rev-parse", "HEAD"]), old_head);
        assert_eq!(
            std::fs::read_to_string(target.join("state.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "old\n"
        );
        assert!(package_backup_dirs(&target).unwrap().is_empty());
    }

    #[test]
    fn git_update_origin_gate_preserves_fixed_ref_for_a_matching_checkout() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let target = store.join("github.com/example/repo");
        std::fs::create_dir_all(&target).unwrap();
        git(&target, &["init", "--initial-branch", "main"]);
        git(
            &target,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/repo.git",
            ],
        );

        let command = NpmCommand::from_argv(Some(&["unused".to_string()])).unwrap();
        let called = std::cell::Cell::new(false);
        update_git_package_with(
            &target,
            &store,
            "git:github.com/example/repo@release/v2",
            &command,
            |actual_root, origin, revision, _| {
                called.set(true);
                assert_eq!(actual_root, target);
                assert_eq!(origin, "https://github.com/example/repo.git");
                assert_eq!(revision, Some("release/v2"));
                Ok(())
            },
        )
        .unwrap();
        assert!(called.get());
    }

    #[test]
    fn git_update_origin_mismatch_stops_before_network_or_checkout_changes() {
        fn git(cwd: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(
                output.status.success(),
                "git {:?} exited with {}",
                args,
                output.status
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let target = store.join("github.com/example/repo");
        std::fs::create_dir_all(&target).unwrap();
        git(&target, &["init", "--initial-branch", "main"]);
        git(
            &target,
            &["config", "user.email", "rpi-test@example.invalid"],
        );
        git(&target, &["config", "user.name", "rpi-test"]);
        std::fs::write(target.join("version.txt"), "unchanged\n").unwrap();
        git(&target, &["add", "version.txt"]);
        git(&target, &["commit", "-m", "initial"]);
        let head = git(&target, &["rev-parse", "HEAD"]);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!(
            "http://127.0.0.1:{}/attacker/repo.git",
            listener.local_addr().unwrap().port()
        );
        git(&target, &["remote", "add", "origin", &origin]);

        let command = NpmCommand::from_argv(Some(&["unused".to_string()])).unwrap();
        let called = std::cell::Cell::new(false);
        let error = update_git_package_with(
            &target,
            &store,
            "git:github.com/example/repo@main",
            &command,
            |_, _, _, _| {
                called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.contains("origin does not match"));
        assert!(!called.get());
        assert_eq!(git(&target, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(target.join("version.txt")).unwrap(),
            "unchanged\n"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn git_update_origin_gate_rejects_missing_duplicate_and_dangerous_urls() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "--initial-branch", "main"]);
        let expected = crate::packages::parse_git_source("git:github.com/example/repo").unwrap();

        assert!(validated_git_origin(&checkout, &expected).is_err());

        git(
            &checkout,
            &[
                "config",
                "--add",
                "remote.origin.url",
                "https://github.com/example/repo.git",
            ],
        );
        git(
            &checkout,
            &[
                "config",
                "--add",
                "remote.origin.url",
                "git@github.com:example/repo.git",
            ],
        );
        assert!(validated_git_origin(&checkout, &expected).is_err());

        git(&checkout, &["config", "--unset-all", "remote.origin.url"]);
        git(
            &checkout,
            &["config", "remote.origin.url", "ext::sh -c 'echo unsafe'"],
        );
        assert!(validated_git_origin(&checkout, &expected).is_err());

        git(
            &checkout,
            &[
                "config",
                "remote.origin.url",
                "https://github.com/example/repo.git ",
            ],
        );
        assert!(validated_git_origin(&checkout, &expected).is_err());

        for mismatched in [
            "http://github.com/example/repo.git",
            "https://github.com:444/example/repo.git",
            "https://other-user@github.com/example/repo.git",
        ] {
            git(&checkout, &["config", "remote.origin.url", mismatched]);
            let error = validated_git_origin(&checkout, &expected).unwrap_err();
            assert!(
                error.contains("origin does not match"),
                "origin={mismatched}"
            );
        }

        git(
            &checkout,
            &[
                "config",
                "remote.origin.url",
                "https://github.com:443/example/repo.git",
            ],
        );
        assert_eq!(
            validated_git_origin(&checkout, &expected).unwrap(),
            "https://github.com:443/example/repo.git"
        );
    }

    #[test]
    fn git_update_rejects_external_core_worktree_before_fetch_reset_or_npm() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let target = store.join("github.com/example/repo");
        let outside = tmp.path().join("outside-worktree");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("must-survive.txt");
        std::fs::write(&sentinel, "unchanged\n").unwrap();
        git(&target, &["init", "--initial-branch", "main"]);
        git(
            &target,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/repo.git",
            ],
        );
        git(
            &target,
            &["config", "core.worktree", outside.to_str().unwrap()],
        );

        let command = NpmCommand::from_argv(Some(&["must-not-run".to_string()])).unwrap();
        let called = std::cell::Cell::new(false);
        let error = update_git_package_with(
            &target,
            &store,
            "git:github.com/example/repo",
            &command,
            |_, _, _, _| {
                called.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.contains("unsafe local configuration"), "{error}");
        assert!(!called.get());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged\n");
    }

    #[test]
    fn git_update_origin_gate_rejects_command_capable_local_config() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let expected = crate::packages::parse_git_source("git:github.com/example/repo").unwrap();
        for (key, value) in [
            ("url.https://evil.invalid/.insteadOf", "https://github.com/"),
            ("core.sshCommand", "malicious-ssh"),
            ("credential.helper", "!malicious-helper"),
            ("http.https://github.com.proxy", "http://evil.invalid"),
            ("http.curloptResolve", "github.com:443:127.0.0.1"),
            (
                "http.https://github.com.curloptResolve",
                "github.com:443:127.0.0.1",
            ),
            ("fetch.bundleURI", "file:///outside/repository.bundle"),
            ("include.path", "../untrusted-config"),
            ("filter.package.process", "malicious-filter"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let checkout = tmp.path().join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            git(&checkout, &["init", "--initial-branch", "main"]);
            git(
                &checkout,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/example/repo.git",
                ],
            );
            git(&checkout, &["config", key, value]);

            let error = validated_git_origin(&checkout, &expected).unwrap_err();
            assert!(error.contains("unsafe local configuration"), "key={key}");
        }
    }

    #[test]
    fn hardened_git_command_drops_environment_config_and_command_injection() {
        let tmp = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch", "main"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(status.success());

        let injected_key = "url.https://attacker.invalid/.insteadOf";
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", injected_key)
            .env("GIT_CONFIG_VALUE_0", "https://github.com/")
            .env("GIT_SSH_COMMAND", "malicious-ssh");
        apply_hardened_git_environment(&mut command);
        let ssh_command = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new("GIT_SSH_COMMAND"));
        assert!(ssh_command.is_some_and(|(_, value)| value.is_none()));

        let output = command
            .args(hardened_git_network_config_args())
            .args(["config", "--get-all", injected_key])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn hardened_git_config_allows_only_known_credential_helpers() {
        for helper in [
            "cache",
            "libsecret",
            "manager",
            "manager-core",
            "osxkeychain",
            "store",
            "wincred",
        ] {
            assert!(is_safe_standard_credential_helper(helper));
        }
        for helper in [
            "!malicious-helper",
            "manager --arg",
            "/tmp/helper",
            "custom",
            "",
        ] {
            assert!(!is_safe_standard_credential_helper(helper));
        }

        let args = hardened_git_network_config_args();
        let values = args
            .chunks_exact(2)
            .map(|pair| pair[1].as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"protocol.allow=never"));
        assert!(values.contains(&"protocol.ext.allow=never"));
        assert!(values.contains(&"protocol.file.allow=never"));
        assert!(values.contains(&"credential.helper="));
        assert!(values.iter().all(|value| {
            value
                .strip_prefix("credential.helper=")
                .map_or(true, |helper| {
                    helper.is_empty() || is_safe_standard_credential_helper(helper)
                })
        }));
    }

    #[test]
    fn cloned_checkout_validation_requires_exact_origin_and_valid_head() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(
            &checkout,
            &["config", "user.email", "rpi-test@example.invalid"],
        );
        git(&checkout, &["config", "user.name", "rpi-test"]);
        std::fs::write(checkout.join("version.txt"), "one\n").unwrap();
        git(&checkout, &["add", "version.txt"]);
        git(&checkout, &["commit", "-m", "one"]);
        let requested = "https://github.com/example/repo.git";
        git(&checkout, &["remote", "add", "origin", requested]);
        let expected = crate::packages::parse_git_source(requested).unwrap();

        validate_cloned_git_origin(&checkout, requested, &expected).unwrap();
        validate_git_head(&checkout, None).unwrap();

        git(
            &checkout,
            &[
                "remote",
                "set-url",
                "origin",
                "ssh://git@github.com/example/repo.git",
            ],
        );
        let error = validate_cloned_git_origin(&checkout, requested, &expected).unwrap_err();
        assert!(error.contains("origin does not match"), "{error}");
    }

    #[test]
    fn full_clone_can_checkout_a_non_default_branch() {
        fn git(cwd: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap_or_else(|error| panic!("git {:?}: {error}", args));
            assert!(status.success(), "git {:?} exited with {status}", args);
        }

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "--initial-branch", "main"]);
        git(
            &source,
            &["config", "user.email", "rpi-test@example.invalid"],
        );
        git(&source, &["config", "user.name", "rpi-test"]);
        std::fs::write(source.join("version.txt"), "main\n").unwrap();
        git(&source, &["add", "version.txt"]);
        git(&source, &["commit", "-m", "main"]);
        git(&source, &["checkout", "-b", "feature/branch"]);
        std::fs::write(source.join("version.txt"), "feature\n").unwrap();
        git(&source, &["add", "version.txt"]);
        git(&source, &["commit", "-m", "feature"]);
        git(&source, &["checkout", "main"]);

        git(
            temp.path(),
            &[
                "-c",
                "core.autocrlf=false",
                "clone",
                source.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        checkout_git_revision(&checkout, "feature/branch").unwrap();
        validate_git_head(&checkout, Some("feature/branch")).unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("version.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "feature\n"
        );
        assert_eq!(
            run_git_capture(&checkout, &["branch", "--show-current"]).unwrap(),
            "feature/branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_uninstall_root_rejects_symlinked_store_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let external_store = tmp.path().join("node_modules");
        let external_package = external_store.join("demo");
        std::fs::create_dir_all(&external_package).unwrap();
        std::fs::create_dir_all(tmp.path().join(".rpi")).unwrap();
        std::os::unix::fs::symlink(&external_store, tmp.path().join(".rpi/packages")).unwrap();

        assert!(!is_managed_package_root(tmp.path(), &external_package));
    }

    #[cfg(unix)]
    #[test]
    fn npm_mutation_rejects_symlinked_target_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("npm");
        let external = tmp.path().join("external/demo");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, root.join("node_modules/demo")).unwrap();

        let error =
            validate_npm_target_before_mutation(&root, Path::new("demo"), false).unwrap_err();
        assert!(error.contains("outside its managed root"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn managed_uninstall_root_rejects_symlinked_store_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let external_store = tmp.path().join("node_modules");
        let external_package = external_store.join("demo");
        std::fs::create_dir_all(&external_package).unwrap();
        std::fs::create_dir_all(tmp.path().join(".rpi")).unwrap();
        if std::os::windows::fs::symlink_dir(&external_store, tmp.path().join(".rpi/packages"))
            .is_err()
        {
            return;
        }

        assert!(!is_managed_package_root(tmp.path(), &external_package));
    }

    #[cfg(windows)]
    #[test]
    fn npm_mutation_rejects_symlinked_target_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("npm");
        let external = tmp.path().join("external/demo");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        if std::os::windows::fs::symlink_dir(&external, root.join("node_modules/demo")).is_err() {
            return;
        }

        let error =
            validate_npm_target_before_mutation(&root, Path::new("demo"), false).unwrap_err();
        assert!(error.contains("outside its managed root"), "{error}");
    }
}
