//! Project-local Rust extension packages.
//!
//! A package is a built cdylib plus optional `skills`, `prompts`, and `themes`
//! directories under `.rpi/packages/<name>`. The registry is deliberately
//! separate from the global native package registry.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const REGISTRY: &str = "packages.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub cargo_package: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PackageRegistry {
    #[serde(default)]
    packages: Vec<ProjectPackage>,
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            0
        }
        Some("link") => link(&args[1..]),
        Some("list") => list(&args[1..]),
        Some("update") => update(&args[1..]),
        Some("unlink") => unlink(&args[1..]),
        Some(other) => {
            eprintln!("error: unknown package command `{other}`");
            print_help();
            2
        }
    }
}

fn link(args: &[String]) -> i32 {
    let mut source = None;
    let mut cargo_package = None;
    let mut name = None;
    let mut release = true;
    let mut force = false;
    let mut locked = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                i += 1;
                source = value(args, i, "--path");
                if source.is_none() {
                    return 2;
                }
            }
            "--package" | "-P" => {
                i += 1;
                cargo_package = value(args, i, "--package");
                if cargo_package.is_none() {
                    return 2;
                }
            }
            "--name" => {
                i += 1;
                name = value(args, i, "--name");
                if name.is_none() {
                    return 2;
                }
            }
            "--debug" => release = false,
            "--locked" => locked = true,
            "--force" | "-f" => force = true,
            "--help" | "-h" => {
                print_link_help();
                return 0;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown link option `{value}`");
                return 2;
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    eprintln!("error: link accepts one package name");
                    return 2;
                }
            }
        }
        i += 1;
    }
    let project_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let source = source
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.clone());
    let metadata = match cargo_metadata(&source) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let selected = match select_package(&metadata, cargo_package.as_deref(), &source) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let package_name = name.unwrap_or_else(|| selected.name.clone());
    if !valid_name(&package_name) {
        eprintln!("error: invalid package name `{package_name}`");
        return 2;
    }
    eprintln!("package: building {}...", selected.name);
    let artifact = match build(&selected, release, locked) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let root = project_root.join(".rpi/packages").join(&package_name);
    let ext = root.join("extensions");
    if let Err(e) = std::fs::create_dir_all(&ext) {
        eprintln!("error: could not create {}: {e}", ext.display());
        return 1;
    }
    let file = match artifact.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => {
            eprintln!("error: invalid artifact name");
            return 1;
        }
    };
    let target = ext.join(&file);
    if target.exists() && !force {
        eprintln!("error: package already exists; use --force");
        return 1;
    }
    let tmp = ext.join(format!(".{file}.tmp"));
    if force {
        if let Err(e) = std::fs::remove_file(&target) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("error: could not replace existing package artifact: {e}");
                return 1;
            }
        }
    }
    if let Err(e) = std::fs::copy(&artifact, &tmp).and_then(|_| std::fs::rename(&tmp, &target)) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("error: could not activate package: {e}");
        return 1;
    }
    let mut registry = match read_registry(&project_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    registry.packages.retain(|p| p.name != package_name);
    registry.packages.push(ProjectPackage {
        name: package_name.clone(),
        version: selected.version,
        source: source.display().to_string(),
        cargo_package: selected.name,
        artifacts: vec![file],
    });
    registry.packages.sort_by(|a, b| a.name.cmp(&b.name));
    if let Err(e) = write_registry(&project_root, &registry) {
        eprintln!("error: {e}");
        return 1;
    }
    println!("linked project package `{package_name}`; run `rpi` in this directory to load it");
    0
}

fn list(args: &[String]) -> i32 {
    if !args.is_empty() && args[0] != "--help" && args[0] != "-h" {
        eprintln!("error: unexpected list argument");
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    match read_registry(&cwd) {
        Ok(r) => {
            if r.packages.is_empty() {
                println!("no project packages");
            } else {
                for p in r.packages {
                    println!("{} {} [{}]", p.name, p.version, p.source);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn update(args: &[String]) -> i32 {
    let mut name = None;
    let mut link_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--debug" | "--locked" => link_args.push(args[i].clone()),
            "--help" | "-h" => {
                print_update_help();
                return 0;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown update option `{value}`");
                return 2;
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    eprintln!("error: update accepts at most one package name");
                    return 2;
                }
            }
        }
        i += 1;
    }

    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let registry = match read_registry(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let packages = registry
        .packages
        .iter()
        .filter(|package| {
            name.as_deref()
                .map_or(true, |wanted| wanted == package.name)
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut status = 0;
    for package in packages {
        let package_name = package.name.clone();
        eprintln!("package: updating project package `{package_name}`...");
        let mut args = vec![
            "--path".to_string(),
            package.source,
            "--package".to_string(),
            package.cargo_package,
            "--name".to_string(),
            package.name,
            "--force".to_string(),
        ];
        args.extend(link_args.iter().cloned());
        if link(&args) != 0 {
            status = 1;
        }
    }

    // Global extensions installed by `rpi install` are also packages. This is
    // the path used by crates.io extensions such as rpi-im-message; delegate to
    // the existing transactional installer so artifact and registry updates
    // remain atomic and version resolution stays in one place.
    let global_packages = match crate::install::installed_native_packages_strict() {
        Ok(packages) => packages
            .into_iter()
            .filter(|package| {
                package.source.is_none()
                    && name
                        .as_deref()
                        .map_or(true, |wanted| wanted == package.name)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Some(wanted) = name.as_deref() {
        let project_found = registry
            .packages
            .iter()
            .any(|package| package.name == wanted);
        let global_found = global_packages.iter().any(|package| package.name == wanted);
        if !project_found && !global_found {
            eprintln!("error: package `{wanted}` is not installed");
            return 1;
        }
    }
    for package in global_packages {
        eprintln!("package: updating global package `{}`...", package.name);
        if !update_global_package(&package.name, &link_args) {
            status = 1;
        }
    }

    if status == 0 && registry.packages.is_empty() && name.is_none() {
        let has_global = crate::install::installed_native_packages()
            .into_iter()
            .any(|package| package.source.is_none());
        if !has_global {
            println!("no project or global packages to update");
        }
    }
    status
}

fn update_global_package(name: &str, args: &[String]) -> bool {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve rpi executable: {error}");
            return false;
        }
    };
    let mut command = std::process::Command::new(executable);
    command.args(["install", name, "--force"]);
    if args.iter().any(|arg| arg == "--locked") {
        command.arg("--locked");
    }
    match command.status() {
        Ok(exit) if exit.success() => true,
        Ok(exit) => {
            eprintln!("error: updating global package `{name}` failed with {exit}");
            false
        }
        Err(error) => {
            eprintln!("error: could not run rpi install for `{name}`: {error}");
            false
        }
    }
}

fn unlink(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        eprintln!("error: missing package name");
        return 2;
    };
    if args.len() != 1 || !valid_name(name) {
        eprintln!("error: invalid package name or arguments");
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut registry = match read_registry(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if !registry.packages.iter().any(|p| p.name == *name) {
        eprintln!("error: project package `{name}` is not installed");
        return 1;
    }
    let dir = cwd.join(".rpi/packages").join(name);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("error: could not remove package: {e}");
        return 1;
    }
    registry.packages.retain(|p| p.name != *name);
    if let Err(e) = write_registry(&cwd, &registry) {
        eprintln!("error: {e}");
        return 1;
    }
    println!("unlinked project package `{name}`");
    0
}

#[derive(Debug, Clone)]
struct CargoPackage {
    name: String,
    version: String,
    manifest: PathBuf,
    target: String,
}
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
}
#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    version: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}
#[derive(Deserialize)]
struct Target {
    name: String,
    crate_types: Vec<String>,
}

fn cargo_metadata(cwd: &Path) -> Result<Metadata, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not execute cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("invalid cargo metadata output: {e}"))
}
fn select_package(
    m: &Metadata,
    requested: Option<&str>,
    cwd: &Path,
) -> Result<CargoPackage, String> {
    let mut found = Vec::new();
    for p in &m.packages {
        if requested.is_some_and(|n| n != p.name) {
            continue;
        }
        for t in &p.targets {
            if t.crate_types.iter().any(|k| k == "cdylib") {
                found.push(CargoPackage {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    manifest: p.manifest_path.clone(),
                    target: t.name.clone(),
                });
            }
        }
    }
    if requested.is_none() {
        let mut local = found
            .iter()
            .filter(|p| p.manifest.parent().is_some_and(|r| cwd.starts_with(r)))
            .cloned()
            .collect::<Vec<_>>();
        if local.len() == 1 {
            return Ok(local.remove(0));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err("no cdylib package found; use --package to select one".into()),
        _ => Err(format!(
            "multiple cdylib packages found: {}; use --package",
            found
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
fn build(p: &CargoPackage, release: bool, locked: bool) -> Result<PathBuf, String> {
    let mut c = Command::new("cargo");
    c.args(["build", "--manifest-path"]).arg(&p.manifest).args([
        "--package",
        &p.name,
        "--message-format=json-render-diagnostics",
    ]);
    if release {
        c.arg("--release");
    }
    if locked {
        c.arg("--locked");
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = c
        .output()
        .map_err(|e| format!("could not execute cargo build: {e}"))?;
    let mut artifact = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("reason").and_then(|v| v.as_str()) == Some("compiler-artifact")
                && v.get("target")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    == Some(&p.target)
            {
                artifact = v
                    .get("filenames")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .find(|p| is_dynamic(p));
            }
        }
    }
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(format!("cargo build exited with {}", out.status));
    }
    artifact.ok_or_else(|| "Cargo did not report a cdylib artifact".into())
}
fn is_dynamic(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .as_deref(),
        Some("dll" | "so" | "dylib")
    )
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn value(args: &[String], i: usize, flag: &str) -> Option<String> {
    match args.get(i) {
        Some(v) if !v.starts_with('-') => Some(v.clone()),
        _ => {
            eprintln!("error: {flag} requires a value");
            None
        }
    }
}
fn read_registry(cwd: &Path) -> Result<PackageRegistry, String> {
    let path = cwd.join(".rpi").join(REGISTRY);
    match std::fs::read_to_string(path) {
        Ok(s) => {
            serde_json::from_str(&s).map_err(|e| format!("invalid project package registry: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PackageRegistry::default()),
        Err(e) => Err(e.to_string()),
    }
}
fn write_registry(cwd: &Path, r: &PackageRegistry) -> Result<(), String> {
    let dir = cwd.join(".rpi");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(REGISTRY);
    let tmp = dir.join(".packages.json.tmp");
    let bytes = serde_json::to_vec_pretty(r).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}
pub fn project_package_dirs(cwd: &Path, kind: &str) -> Vec<PathBuf> {
    match read_registry(cwd) {
        Ok(registry) => registry
            .packages
            .into_iter()
            .map(|p| cwd.join(".rpi/packages").join(p.name).join(kind))
            .collect(),
        Err(error) => {
            eprintln!("warning: could not load project package registry: {error}");
            Vec::new()
        }
    }
}
pub fn print_help() {
    println!("Usage: rpi package <command>\n\nCommands:\n  link [name] [--path <dir>] [--package <name>] [--force]  Build and link a project-local Rust extension\n  list                                                   List project packages\n  update [name] [--debug] [--locked]                    Update project and global installed packages\n  unlink <name>                                          Remove a project package");
}
fn print_link_help() {
    println!("Usage: rpi package link [name] [options]\n\nOptions:\n  --path <dir>       Extension source directory (default: current directory)\n  --package, -P <n>  Select a Cargo cdylib package\n  --debug            Build debug artifacts instead of release\n  --locked           Pass --locked to Cargo\n  --force, -f        Replace an existing package");
}
fn print_update_help() {
    println!("Usage: rpi package update [name] [options]\n\nOptions:\n  --debug            Build debug artifacts instead of release\n  --locked           Pass --locked to Cargo\n  --help, -h         Show this help\n\nWithout a name, all project and global installed packages are updated.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_reject_path_traversal() {
        assert!(valid_name("review-tools"));
        assert!(!valid_name("../review-tools"));
        assert!(!valid_name("review/tools"));
        assert!(!valid_name(""));
    }

    #[test]
    fn update_help_is_available_without_a_registry() {
        print_update_help();
    }

    #[test]
    fn dynamic_library_extensions_are_platform_neutral() {
        assert!(is_dynamic(Path::new("plugin.dll")));
        assert!(is_dynamic(Path::new("libplugin.so")));
        assert!(is_dynamic(Path::new("libplugin.dylib")));
        assert!(!is_dynamic(Path::new("plugin.exe")));
    }
}
