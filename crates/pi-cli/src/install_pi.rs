//! Pi package installer (`rpi install-pi`).
//!
//! Pi packages are ordinary npm/git/local directories. The installer keeps
//! the package under the rpi-owned `.rpi/packages` (or global agent store),
//! installs production dependencies, and enables the resolved directory in
//! settings so static resources and JS/TS extensions are loaded on startup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
struct Options {
    spec: String,
    global: bool,
    force: bool,
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
    let destination = match destination(&cwd, &options) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&destination) {
        eprintln!("error: could not create {}: {error}", destination.display());
        return 1;
    }
    let package_root = match install_spec(&cwd, &destination, &options) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let package_root =
        normalize_config_path(std::fs::canonicalize(&package_root).unwrap_or(package_root));
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    let packages = settings.packages.get_or_insert_with(Vec::new);
    let enabled = format!("file:{}", package_root.display());
    if !packages.iter().any(|value| value == &enabled) {
        packages.push(enabled);
        if let Err(error) = crate::settings::save_settings(&settings) {
            eprintln!("error: installed package but could not save settings: {error}");
            return 1;
        }
    }
    println!("installed Pi package {}", package_root.display());
    println!("JS/TS extensions and static resources will load on the next start.");
    println!("warning: Pi extensions execute JavaScript with the current user's permissions.");
    0
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

fn destination(cwd: &Path, options: &Options) -> Result<PathBuf, String> {
    if options.global {
        return crate::config::agent_dir()
            .map(|path| path.join("packages"))
            .map_err(|error| error.to_string());
    }
    Ok(cwd.join(".rpi/packages"))
}

fn install_spec(cwd: &Path, destination: &Path, options: &Options) -> Result<PathBuf, String> {
    let spec = options.spec.as_str();
    if spec.starts_with("git:") || spec.starts_with("https://") || spec.starts_with("http://") {
        return install_git(cwd, destination, spec, options.force);
    }
    if spec.starts_with("npm:") || (!Path::new(spec).exists() && looks_like_npm(spec)) {
        return install_npm(destination, spec, options.force);
    }
    install_local(cwd, destination, spec, options.force)
}

fn package_name(spec: &str) -> String {
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

fn install_npm(destination: &Path, spec: &str, force: bool) -> Result<PathBuf, String> {
    let name = safe_name(spec);
    let stage = tempfile::tempdir()
        .map_err(|error| format!("could not create npm staging dir: {error}"))?;
    let mut command = Command::new(npm_program());
    command
        .args(["install", "--prefix"])
        .arg(stage.path())
        .args(["--omit=dev", "--no-package-lock"])
        .arg(spec.strip_prefix("npm:").unwrap_or(spec))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .map_err(|error| format!("could not execute npm (install Node.js/npm first): {error}"))?;
    if !status.success() {
        return Err(format!("npm install exited with {status}"));
    }
    let installed = stage
        .path()
        .join("node_modules")
        .join(npm_module_name(spec));
    if !installed.is_dir() {
        return Err(format!(
            "npm installed `{spec}` but no package directory was found"
        ));
    }
    let target = destination.join(&name);
    replace_dir(&installed, &target, force)?;
    install_production_dependencies(&target)?;
    Ok(target)
}

/// Refresh an installed npm package in-place. The package root is expected to
/// be the safe-name directory created by `install-pi`; replacing that directory
/// preserves the settings entry while updating its contents.
pub fn update_npm_package(root: &Path, name: &str) -> Result<PathBuf, String> {
    let destination = root
        .parent()
        .ok_or_else(|| format!("package root has no parent: {}", root.display()))?;
    install_npm(destination, &format!("npm:{name}@latest"), true)
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
    let root = find_installed_root(&cwd, &options);
    let removed_settings = match remove_settings_entries(&cwd, &options.spec, root.as_deref()) {
        Ok(count) => count,
        Err(error) => {
            eprintln!("error: could not update package settings: {error}");
            return 1;
        }
    };
    let mut removed_files = false;
    if let Some(root) = root {
        if root.exists() && is_managed_package_root(&cwd, &root) {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
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
    if !removed_files && removed_settings == 0 {
        println!("Pi package is not installed: {}", options.spec);
    } else if removed_settings > 0 && !removed_files {
        println!("disabled Pi package {}", options.spec);
    }
    0
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
    let raw = options.spec.strip_prefix("file:").unwrap_or(&options.spec);
    let package_key = package_dir_name(&options.spec);
    let mut candidates = Vec::new();
    let direct = PathBuf::from(raw);
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
        add_store(cwd.join(".rpi/packages"), &mut candidates);
        add_store(cwd.join(".pi/packages"), &mut candidates);
    }
    if let Ok(agent) = crate::config::agent_dir() {
        add_store(agent.join("packages"), &mut candidates);
    }
    if let Some(home) = dirs::home_dir() {
        add_store(home.join(".pi/agent/packages"), &mut candidates);
    }
    let wanted_name = package_name(&options.spec);
    for package in crate::packages::discover_from_settings(cwd).packages {
        if package.name == wanted_name
            || safe_name(&package.name) == package_key
            || package
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == package_key)
        {
            candidates.push(package.root);
        }
    }
    candidates
        .into_iter()
        .find_map(|path| std::fs::canonicalize(path).ok())
}

fn package_dir_name(spec: &str) -> String {
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

fn is_managed_package_root(cwd: &Path, root: &Path) -> bool {
    let mut stores = vec![cwd.join(".rpi/packages"), cwd.join(".pi/packages")];
    if let Ok(agent) = crate::config::agent_dir() {
        stores.push(agent.join("packages"));
    }
    if let Some(home) = dirs::home_dir() {
        stores.push(home.join(".pi/agent/packages"));
    }
    stores.iter().any(|store| {
        let store = std::fs::canonicalize(store).unwrap_or_else(|_| store.clone());
        root.starts_with(store)
    })
}

fn remove_settings_entries(cwd: &Path, spec: &str, root: Option<&Path>) -> Result<usize, String> {
    let mut settings = crate::settings::load_settings().unwrap_or_default();
    let Some(packages) = settings.packages.as_mut() else {
        return Ok(0);
    };
    let wanted_name = package_name(spec);
    let before = packages.len();
    packages.retain(|entry| {
        if entry == spec || package_name(entry) == wanted_name && !entry.starts_with('.') {
            return false;
        }
        let matches_root = root.is_some_and(|root| {
            crate::packages::resolve_package(cwd, entry)
                .ok()
                .and_then(|package| std::fs::canonicalize(package.root).ok())
                .is_some_and(|candidate| candidate == root)
        });
        !matches_root
    });
    let removed = before - packages.len();
    if removed == 0 {
        return Ok(0);
    }
    if packages.is_empty() {
        settings.packages = None;
    }
    crate::settings::save_settings(&settings).map_err(|error| error.to_string())?;
    Ok(removed)
}

fn npm_module_name(spec: &str) -> String {
    let raw = spec.strip_prefix("npm:").unwrap_or(spec);
    if raw.starts_with('@') {
        raw.rfind('@')
            .filter(|index| *index > 0)
            .map(|index| raw[..index].to_string())
            .unwrap_or_else(|| raw.to_string())
    } else {
        raw.split('@').next().unwrap_or(raw).to_string()
    }
}

fn install_git(cwd: &Path, destination: &Path, spec: &str, force: bool) -> Result<PathBuf, String> {
    let url = spec.strip_prefix("git:").unwrap_or(spec);
    let (url, revision) = url
        .split_once('@')
        .map_or((url, None), |(u, r)| (u, Some(r)));
    let name = safe_name(
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("package"),
    );
    let target = destination.join(name);
    if target.exists() {
        if !force {
            return Err(format!(
                "{} already exists; use --force to replace it",
                target.display()
            ));
        }
        std::fs::remove_dir_all(&target)
            .map_err(|error| format!("could not replace {}: {error}", target.display()))?;
    }
    let status = Command::new("git")
        .args(["clone", "--depth", "1"])
        .arg(url)
        .arg(&target)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute git: {error}"))?;
    if !status.success() {
        return Err(format!("git clone exited with {status}"));
    }
    if let Some(revision) = revision {
        let status = Command::new("git")
            .args(["fetch", "--depth", "1", "origin", revision])
            .current_dir(&target)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("could not fetch git revision: {error}"))?;
        if !status.success() {
            return Err(format!("git fetch exited with {status}"));
        }
        let status = Command::new("git")
            .args(["checkout", revision])
            .current_dir(&target)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("could not checkout git revision: {error}"))?;
        if !status.success() {
            return Err(format!("git checkout exited with {status}"));
        }
    }
    install_production_dependencies(&target)?;
    Ok(target)
}

fn install_local(
    cwd: &Path,
    destination: &Path,
    spec: &str,
    force: bool,
) -> Result<PathBuf, String> {
    let source = PathBuf::from(spec);
    let source = if source.is_absolute() {
        source
    } else {
        cwd.join(source)
    };
    if !source.is_dir() {
        return Err(format!(
            "local package directory not found: {}",
            source.display()
        ));
    }
    let name = std::fs::read_to_string(source.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("name")
                .and_then(|name| name.as_str())
                .map(safe_name)
        })
        .unwrap_or_else(|| safe_name(spec));
    let target = destination.join(name);
    if std::fs::canonicalize(&source).ok() == std::fs::canonicalize(&target).ok() {
        return Ok(target);
    }
    replace_dir(&source, &target, force)?;
    install_production_dependencies(&target)?;
    Ok(target)
}

fn replace_dir(source: &Path, target: &Path, force: bool) -> Result<(), String> {
    if target.exists() {
        if !force {
            return Err(format!(
                "{} already exists; use --force to replace it",
                target.display()
            ));
        }
        std::fs::remove_dir_all(target)
            .map_err(|error| format!("could not replace {}: {error}", target.display()))?;
    }
    std::fs::create_dir_all(target.parent().unwrap_or(target))
        .map_err(|error| error.to_string())?;
    copy_dir(source, target).map_err(|error| format!("could not copy package: {error}"))
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

fn install_production_dependencies(root: &Path) -> Result<(), String> {
    if !root.join("package.json").is_file() {
        return Ok(());
    }
    let status = Command::new(npm_program())
        .args(["install", "--omit=dev", "--no-package-lock"])
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute npm for package dependencies: {error}"))?;
    if !status.success() {
        return Err(format!("npm install exited with {status}"));
    }
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
    if !host_packages.is_empty() {
        let mut command = Command::new(npm_program());
        command
            .args(["install", "--omit=dev", "--no-package-lock", "--no-save"])
            .args(&host_packages)
            .current_dir(root)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command
            .status()
            .map_err(|error| format!("could not install Pi host dependencies: {error}"))?;
        if !status.success() {
            return Err(format!("npm host dependency install exited with {status}"));
        }
    }
    Ok(())
}

fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

fn normalize_config_path(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix("\\\\?\\") {
            return PathBuf::from(stripped);
        }
    }
    path
}

fn looks_like_npm(spec: &str) -> bool {
    !spec.contains(std::path::MAIN_SEPARATOR) && !spec.contains('/') && !spec.ends_with(".json")
}

pub fn print_help() {
    println!("Usage: rpi install-pi [options] <spec>\n\nInstall a Pi npm/git/local package and enable its static resources and JS/TS extensions.\n\nSpecs:\n  npm:@scope/package@1.0.0\n  git:github.com/user/repo@v1\n  ./local-package\n\nOptions:\n  --global, -g  Install into ~/.rpi/agent/packages\n  --force, -f   Replace an existing package\n  --help, -h    Show this help");
}

fn print_uninstall_help() {
    println!("Usage: rpi uninstall-pi [options] <spec>\n\nRemove an installed Pi npm/git/local package and disable it in settings.\n\nSpecs:\n  npm:@scope/package\n  git:github.com/user/repo\n  ./local-package\n\nOptions:\n  --global, -g  Remove from ~/.rpi/agent/packages\n  --help, -h    Show this help\n\nAliases:\n  rpi uninstall pi <spec>");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_install_specs_and_flags() {
        let options = parse_args(&["--global".into(), "npm:@scope/pkg@1.2.3".into()]).unwrap();
        assert!(options.global);
        assert_eq!(options.spec, "npm:@scope/pkg@1.2.3");
        assert_eq!(package_name(&options.spec), "scope__pkg");
        assert_eq!(npm_module_name(&options.spec), "@scope/pkg");
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
}
