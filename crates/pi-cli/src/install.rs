//! `rpi install` — install a Rust `cdylib` extension from crates.io.
//!
//! Cargo's `cargo install` command is intended for binaries and does not copy
//! dynamic-library targets. rpi extensions are cdylibs loaded by
//! `rpi-extensions`, so this command creates a tiny temporary Cargo workspace,
//! resolves the requested crate through Cargo, builds the dependency in
//! release mode, and copies its cdylib into the same global directory scanned
//! during normal startup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

const INSTALLER_MANIFEST: &str = "rpi-extension-installer";

#[derive(Debug, Clone)]
struct InstallOptions {
    package: String,
    version: Option<String>,
    path: Option<PathBuf>,
    locked: bool,
    force: bool,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: String,
    crate_types: Vec<String>,
}

/// Run `rpi install ...`. This is intentionally synchronous: it is a short
/// lived package operation and keeping Cargo's build output attached to the
/// user's terminal makes failures actionable.
pub fn run(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_help();
        return 0;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            print_help();
            return 2;
        }
    };

    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => {
            eprintln!("error: could not create a temporary Cargo workspace: {error}");
            return 1;
        }
    };
    let manifest = temp.path().join("Cargo.toml");
    if let Err(error) = write_manifest(&manifest, &options) {
        eprintln!("error: could not prepare Cargo workspace: {error}");
        return 1;
    }

    if let Err(error) = cargo_command("fetch", &manifest, &options, false) {
        eprintln!("error: could not resolve `{}`: {error}", options.package);
        return 1;
    }

    let metadata = match cargo_metadata(&manifest, &options) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("error: could not inspect `{}`: {error}", options.package);
            return 1;
        }
    };
    let package = match metadata
        .packages
        .iter()
        .find(|package| package.name == options.package)
    {
        Some(package) => package,
        None => {
            eprintln!(
                "error: Cargo did not resolve a package named `{}`",
                options.package
            );
            return 1;
        }
    };
    let cdylib_targets: Vec<&CargoTarget> = package
        .targets
        .iter()
        .filter(|target| target.crate_types.iter().any(|kind| kind == "cdylib"))
        .collect();
    if cdylib_targets.is_empty() {
        eprintln!(
            "error: `{}` is not an rpi extension crate; it has no `cdylib` target",
            options.package
        );
        eprintln!(
            "hint: the crate must declare `crate-type = [\"cdylib\"]` and export `rpi_plugin_register`"
        );
        return 1;
    }

    if let Err(error) = cargo_command("build", &manifest, &options, true) {
        eprintln!("error: failed to build `{}`: {error}", options.package);
        return 1;
    }

    let artifact_dir = temp.path().join("target").join("release");
    let artifacts = match find_artifacts(&artifact_dir, &cdylib_targets) {
        Ok(artifacts) => artifacts,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let destination = match crate::config::agent_dir() {
        Ok(dir) => dir.join("extensions"),
        Err(error) => {
            eprintln!("error: could not resolve the rpi config directory: {error}");
            return 1;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&destination) {
        eprintln!(
            "error: could not create extension directory {}: {error}",
            destination.display()
        );
        return 1;
    }

    for artifact in &artifacts {
        let target = destination.join(artifact.file_name().unwrap_or_default());
        if target.exists() && !options.force {
            eprintln!(
                "error: extension {} already exists; use --force to replace it",
                target.display()
            );
            return 1;
        }
        if let Err(error) = std::fs::copy(artifact, &target) {
            eprintln!(
                "error: could not install {} to {}: {error}",
                artifact.display(),
                target.display()
            );
            return 1;
        }
        println!("installed {}", target.display());
    }
    println!("rpi will load this extension on the next start.");
    0
}

fn parse_args(args: &[String]) -> Result<InstallOptions, String> {
    let mut package = None;
    let mut version = None;
    let mut path = None;
    let mut locked = false;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Err(help_requested().to_string()),
            "--locked" => locked = true,
            "--force" | "-f" => force = true,
            "--version" | "-V" => {
                i += 1;
                version = Some(value(args, i, "--version")?);
            }
            "--path" => {
                i += 1;
                path = Some(PathBuf::from(value(args, i, "--path")?));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown install option `{value}`"));
            }
            value => {
                if package.replace(value.to_string()).is_some() {
                    return Err("install accepts exactly one crate name".to_string());
                }
            }
        }
        i += 1;
    }
    let package = package.ok_or_else(|| "missing crate name".to_string())?;
    if path.is_some() && version.is_some() {
        return Err("--path and --version cannot be used together".to_string());
    }
    if !valid_package_name(&package) {
        return Err(format!("invalid Cargo package name `{package}`"));
    }
    Ok(InstallOptions {
        package,
        version,
        path,
        locked,
        force,
    })
}

fn value(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn help_requested() -> &'static str {
    "use `rpi install --help` for usage"
}

pub fn print_help() {
    println!(
        "Usage: rpi install <crate> [options]\n\nInstall an rpi Rust cdylib extension from crates.io.\n\nOptions:\n  --version <version>  Install a specific crates.io version\n  --path <directory>   Build a local extension crate\n  --locked             Require Cargo.lock to remain unchanged\n  --force, -f          Replace an existing installed extension\n  --help, -h           Show this help\n\nExamples:\n  rpi install rpi-extension-example\n  rpi install rpi-extension-example --version 0.1.0\n  rpi install my-extension --path ../my-rpi-extension --force"
    );
}

fn write_manifest(path: &Path, options: &InstallOptions) -> Result<(), String> {
    let source_dir = path
        .parent()
        .ok_or_else(|| "temporary workspace has no parent directory".to_string())?
        .join("src");
    std::fs::create_dir_all(&source_dir).map_err(|error| error.to_string())?;
    // Cargo requires the temporary root package to have a target even though
    // rpi never builds it; the requested extension is built as a dependency.
    std::fs::write(source_dir.join("lib.rs"), "pub fn installer_marker() {}\n")
        .map_err(|error| error.to_string())?;
    let dependency = if let Some(local_path) = &options.path {
        let absolute = if local_path.is_absolute() {
            local_path.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| error.to_string())?
                .join(local_path)
        };
        format!(
            "rpi_extension_dep = {{ package = {:?}, path = {:?} }}",
            options.package,
            absolute.display().to_string()
        )
    } else {
        let version = options.version.as_deref().unwrap_or("*");
        format!(
            "rpi_extension_dep = {{ package = {:?}, version = {:?} }}",
            options.package, version
        )
    };
    let contents = format!(
        "[package]\nname = \"{INSTALLER_MANIFEST}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\n{dependency}\n"
    );
    std::fs::write(path, contents).map_err(|error| error.to_string())
}

fn cargo_command(
    subcommand: &str,
    manifest: &Path,
    options: &InstallOptions,
    build: bool,
) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.arg(subcommand).arg("--manifest-path").arg(manifest);
    if build {
        command
            .arg("--package")
            .arg(&options.package)
            .arg("--release")
            .arg("--target-dir")
            .arg(manifest.parent().unwrap().join("target"));
    }
    if options.locked {
        command.arg("--locked");
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("could not execute cargo: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {subcommand} exited with {status}"))
    }
}

fn cargo_metadata(manifest: &Path, options: &InstallOptions) -> Result<CargoMetadata, String> {
    let mut command = Command::new("cargo");
    command
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest);
    if options.locked {
        command.arg("--locked");
    }
    let output = command
        .output()
        .map_err(|error| format!("could not execute cargo: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid cargo metadata: {error}"))
}

fn find_artifacts(release_dir: &Path, targets: &[&CargoTarget]) -> Result<Vec<PathBuf>, String> {
    let mut artifacts = Vec::new();
    for target in targets {
        let wanted = normalize_name(&target.name);
        let mut matches = Vec::new();
        for dir in [release_dir.to_path_buf(), release_dir.join("deps")] {
            let entries = std::fs::read_dir(&dir).map_err(|error| {
                format!("could not inspect build output {}: {error}", dir.display())
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_dynamic_library(&path) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let normalized = normalize_name(stem.trim_start_matches("lib"));
                if normalized == wanted {
                    matches.push(path);
                }
            }
        }
        matches.sort_by_key(|path| path.components().count());
        let artifact = matches.into_iter().next().ok_or_else(|| {
            format!(
                "Cargo built `{}` but no cdylib artifact was found in {}",
                target.name,
                release_dir.display()
            )
        })?;
        artifacts.push(artifact);
    }
    Ok(artifacts)
}

fn normalize_name(name: &str) -> String {
    name.replace('-', "_").to_ascii_lowercase()
}

fn is_dynamic_library(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("dll" | "so" | "dylib" | "pyd")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_registry_package_and_options() {
        let parsed = parse_args(&args(&["my-extension", "--version", "1.2.3", "--force"])).unwrap();
        assert_eq!(parsed.package, "my-extension");
        assert_eq!(parsed.version.as_deref(), Some("1.2.3"));
        assert!(parsed.force);
    }

    #[test]
    fn parses_local_package() {
        let parsed = parse_args(&args(&["--path", "../extension", "my-extension"])).unwrap();
        assert_eq!(parsed.path, Some(PathBuf::from("../extension")));
    }

    #[test]
    fn rejects_non_extension_options_and_invalid_names() {
        assert!(parse_args(&args(&["my.extension"])).is_err());
        assert!(parse_args(&args(&["my-extension", "--unknown"])).is_err());
        assert!(parse_args(&args(&["my-extension", "--path", ".", "--version", "1"])).is_err());
    }
}
