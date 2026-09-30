//! `rpi install` — install a Rust `cdylib` extension from crates.io.
//!
//! Cargo's `cargo install` command is intended for binaries and does not copy
//! dynamic-library targets. rpi extensions are cdylibs loaded by
//! `rpi-extensions`, so this command creates a tiny temporary Cargo workspace,
//! resolves the requested crate through Cargo, builds the dependency in
//! release mode, and copies its cdylib into the same global directory scanned
//! during normal startup.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const INSTALLER_MANIFEST: &str = "rpi-extension-installer";
const NATIVE_PACKAGES_FILE: &str = "native-packages.json";
const NATIVE_PACKAGES_LOCK_FILE: &str = "native-packages.lock";
const CDYLIB_EXTENSION_HINT: &str = "hint: the crate must declare `crate-type = [\"cdylib\"]` and export `rpi_plugin_register` (the unified ABI; legacy `rpi_plugin_register_v2` and `rpi_plugin_register_v3` remain supported for migration)";

/// A Rust-native extension installed through `rpi install`.
///
/// `source` is absent for crates.io packages and contains the local source
/// path for `--path` installs. Local development crates are intentionally not
/// eligible for automatic registry updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledNativePackage {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Dynamic-library file names copied into the extension store. Older
    /// metadata files omit this field; uninstall falls back to crate-name
    /// matching for those records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

/// The on-disk state of the Rust-native extension registry.
///
/// A malformed or unreadable registry is returned as an error, rather than
/// being conflated with a missing file. Mutation paths use this distinction to
/// avoid replacing damaged metadata with a newly generated registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePackageRegistry {
    /// No registry has been created yet.
    Missing,
    /// A registry was present and passed JSON/schema validation.
    Valid(Vec<InstalledNativePackage>),
}

impl NativePackageRegistry {
    fn into_records(self) -> Vec<InstalledNativePackage> {
        match self {
            Self::Missing => Vec::new(),
            Self::Valid(records) => records,
        }
    }
}

struct NativePackageMutationLock {
    file: Option<std::fs::File>,
}

impl NativePackageMutationLock {
    fn acquire(metadata_path: &Path) -> Result<Self, String> {
        let parent = metadata_path
            .parent()
            .ok_or_else(|| "native package metadata has no parent".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let lock_path = parent.join(NATIVE_PACKAGES_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                format!(
                    "could not open native package lock {}: {error}",
                    lock_path.display()
                )
            })?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            format!("could not lock native package state (another install may be running): {error}")
        })?;
        Ok(Self { file: Some(file) })
    }
}

impl Drop for NativePackageMutationLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs2::FileExt::unlock(&file);
        }
    }
}

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
    version: String,
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

    // Validate before Cargo work or extension-store mutations. A damaged file
    // may contain the only record of installed artifacts, so treating it as
    // an empty registry would make a later write destructive.
    if let Err(error) = read_native_package_registry() {
        eprintln!("error: cannot safely install while native package metadata is invalid: {error}");
        return 1;
    }

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
        eprintln!("{CDYLIB_EXTENSION_HINT}");
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

    let agent_dir = match crate::config::agent_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("error: could not resolve the rpi config directory: {error}");
            return 1;
        }
    };
    let destination = agent_dir.join("extensions");
    if let Err(error) = std::fs::create_dir_all(&destination) {
        eprintln!(
            "error: could not create extension directory {}: {error}",
            destination.display()
        );
        return 1;
    }

    let version = metadata
        .packages
        .iter()
        .find(|candidate| candidate.name == options.package)
        .map(|candidate| candidate.version.clone())
        .unwrap_or_else(|| "0.0.0".to_string());
    let record = InstalledNativePackage {
        name: options.package.clone(),
        version,
        source: options
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        artifacts: Vec::new(),
    };
    let installed = match install_native_package_at(
        &destination,
        &agent_dir.join(NATIVE_PACKAGES_FILE),
        record,
        &artifacts,
        options.force,
    ) {
        Ok(installed) => installed,
        Err(error) => {
            eprintln!("error: could not install `{}`: {error}", options.package);
            return 1;
        }
    };
    for target in installed {
        println!("installed {}", target.display());
    }
    println!("rpi will load this extension on the next start.");
    0
}

/// Remove a Rust-native extension installed by `rpi install`.
///
/// The install registry is authoritative for new installs. For metadata from
/// older rpi versions, dynamic libraries whose normalized file stem matches
/// the crate name are removed as a compatibility fallback.
pub fn uninstall(args: &[String]) -> i32 {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        print_uninstall_help();
        return 0;
    }
    let name = match parse_uninstall_name(args) {
        Ok(name) => name,
        Err(error) => {
            eprintln!("error: {error}");
            print_uninstall_help();
            return 2;
        }
    };
    let agent = match crate::config::agent_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve the rpi config directory: {error}");
            return 1;
        }
    };
    let extension_dir = agent.join("extensions");
    let metadata_path = agent.join(NATIVE_PACKAGES_FILE);
    let _lock = match NativePackageMutationLock::acquire(&metadata_path) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("error: cannot safely uninstall: {error}");
            return 1;
        }
    };
    let records = match read_native_package_registry_at(&metadata_path) {
        Ok(registry) => registry.into_records(),
        Err(error) => {
            eprintln!(
                "error: cannot safely uninstall while native package metadata is invalid: {error}"
            );
            return 1;
        }
    };
    let had_record = records.iter().any(|record| record.name == name);
    let wanted = normalize_name(&name);
    let artifact_names: std::collections::HashSet<String> = records
        .iter()
        .filter(|record| record.name == name)
        .flat_map(|record| record.artifacts.iter().cloned())
        .collect();
    let use_legacy_name_fallback = artifact_names.is_empty();
    let mut artifact_targets = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&extension_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_artifact = artifact_names.contains(
                &path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ) || (use_legacy_name_fallback
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| normalize_name(stem.trim_start_matches("lib")) == wanted)
                    .unwrap_or(false));
            if is_artifact && is_dynamic_library(&path) {
                artifact_targets.push(path);
            }
        }
    }
    let mut remaining: Vec<_> = records
        .into_iter()
        .filter(|record| record.name != name)
        .collect();
    if had_record {
        remaining.sort_by(|left, right| left.name.cmp(&right.name));
    }
    let mut replacements = Vec::new();
    let mut removals = artifact_targets.clone();
    if had_record {
        if remaining.is_empty() {
            removals.push(metadata_path.clone());
        } else {
            let registry = match serde_json::to_vec_pretty(&remaining) {
                Ok(registry) => registry,
                Err(error) => {
                    eprintln!("error: could not serialize native package metadata: {error}");
                    return 1;
                }
            };
            let staged = match stage_native_bytes(&registry, &metadata_path) {
                Ok(staged) => staged,
                Err(error) => {
                    eprintln!("error: could not stage native package metadata: {error}");
                    return 1;
                }
            };
            replacements.push(NativeReplacement {
                staged,
                target: metadata_path.clone(),
            });
        }
    }
    if let Err(error) = activate_native_transaction(&replacements, &removals, &mut |_, _| Ok(())) {
        eprintln!("error: could not uninstall Rust extension {name}: {error}");
        return 1;
    }
    for path in &artifact_targets {
        println!("removed {}", path.display());
    }
    let removed = artifact_targets.len();
    if removed == 0 && !had_record {
        println!("Rust extension is not installed: {name}");
        return 0;
    }
    println!("uninstalled Rust extension {name}");
    0
}

fn parse_uninstall_name(args: &[String]) -> Result<String, String> {
    let mut name = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Err("use `rpi uninstall --help` for usage".into()),
            value if value.starts_with('-') => {
                return Err(format!("unknown uninstall option `{value}`"))
            }
            value => {
                if name.replace(value.to_string()).is_some() {
                    return Err("uninstall accepts exactly one crate name".into());
                }
            }
        }
    }
    let name = name.ok_or_else(|| "missing crate name".to_string())?;
    if !valid_package_name(&name) {
        return Err(format!("invalid Cargo package name `{name}`"));
    }
    Ok(name)
}

#[cfg(test)]
fn write_native_packages_at(path: &Path, records: &[InstalledNativePackage]) -> Result<(), String> {
    if records.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| "native package metadata has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let data = serde_json::to_vec_pretty(records).map_err(|error| error.to_string())?;
    std::fs::write(path, data).map_err(|error| error.to_string())
}

/// Read and validate the Rust-native extension registry without hiding errors.
pub fn read_native_package_registry() -> Result<NativePackageRegistry, String> {
    let path = crate::config::agent_dir()
        .map_err(|error| error.to_string())?
        .join(NATIVE_PACKAGES_FILE);
    read_native_package_registry_at(&path)
}

fn read_native_package_registry_at(path: &Path) -> Result<NativePackageRegistry, String> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NativePackageRegistry::Missing)
        }
        Err(error) => {
            return Err(format!("could not read {}: {error}", path.display()));
        }
    };
    serde_json::from_slice(&data)
        .map(NativePackageRegistry::Valid)
        .map_err(|error| format!("invalid JSON or schema in {}: {error}", path.display()))
}

/// Read the registry for best-effort discovery and startup update notices.
///
/// Missing, unreadable, and malformed registries all produce an empty list so
/// startup remains safe. Install and uninstall operations instead use
/// [`read_native_package_registry`] and fail closed on an invalid file.
pub fn installed_native_packages() -> Vec<InstalledNativePackage> {
    installed_native_packages_strict().unwrap_or_default()
}

/// Read the installed package list for a command that may mutate package
/// state. Unlike startup discovery, callers must surface registry errors and
/// abort before touching any package store.
pub(crate) fn installed_native_packages_strict() -> Result<Vec<InstalledNativePackage>, String> {
    read_native_package_registry().map(NativePackageRegistry::into_records)
}

#[cfg(test)]
fn record_native_package_at(path: &Path, record: &InstalledNativePackage) -> Result<(), String> {
    let mut records = read_native_package_registry_at(path)?.into_records();
    if let Some(existing) = records.iter_mut().find(|item| item.name == record.name) {
        *existing = record.clone();
    } else {
        records.push(record.clone());
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    write_native_packages_at(path, &records)
}

struct StagedNativeFile {
    path: PathBuf,
}

impl Drop for StagedNativeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct NativeReplacement {
    staged: StagedNativeFile,
    target: PathBuf,
}

struct NativeBackup {
    target: PathBuf,
    backup: PathBuf,
}

fn install_native_package_at(
    destination: &Path,
    metadata_path: &Path,
    record: InstalledNativePackage,
    artifacts: &[PathBuf],
    force: bool,
) -> Result<Vec<PathBuf>, String> {
    install_native_package_at_with_hook(
        destination,
        metadata_path,
        record,
        artifacts,
        force,
        |_, _| Ok(()),
    )
}

fn install_native_package_at_with_hook(
    destination: &Path,
    metadata_path: &Path,
    mut record: InstalledNativePackage,
    artifacts: &[PathBuf],
    force: bool,
    mut before_activate: impl FnMut(usize, &Path) -> Result<(), String>,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(destination).map_err(|error| {
        format!(
            "could not create extension directory {}: {error}",
            destination.display()
        )
    })?;
    let metadata_parent = metadata_path
        .parent()
        .ok_or_else(|| "native package metadata has no parent".to_string())?;
    std::fs::create_dir_all(metadata_parent).map_err(|error| error.to_string())?;
    let _lock = NativePackageMutationLock::acquire(metadata_path)?;
    let mut records = read_native_package_registry_at(metadata_path)?.into_records();
    let previous = records
        .iter()
        .find(|candidate| candidate.name == record.name)
        .cloned();

    let mut new_artifacts = Vec::with_capacity(artifacts.len());
    let mut new_keys = HashSet::new();
    for artifact in artifacts {
        let metadata = std::fs::symlink_metadata(artifact).map_err(|error| {
            format!(
                "could not inspect built artifact {}: {error}",
                artifact.display()
            )
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "built artifact is not a regular file: {}",
                artifact.display()
            ));
        }
        let name = artifact_file_name(artifact)?;
        let key = native_artifact_identity(&name);
        if !new_keys.insert(key) {
            return Err(format!("duplicate built artifact name `{name}`"));
        }
        new_artifacts.push((name, artifact.clone()));
    }
    if new_artifacts.is_empty() {
        return Err("Cargo produced no installable cdylib artifacts".to_string());
    }

    for other in records
        .iter()
        .filter(|candidate| candidate.name != record.name)
    {
        for owned in &other.artifacts {
            checked_registered_artifact_path(destination, owned)?;
            if new_keys.contains(&native_artifact_identity(owned)) {
                return Err(format!(
                    "artifact `{owned}` is already owned by installed package `{}`",
                    other.name
                ));
            }
        }
    }

    let mut stale_targets = Vec::new();
    if let Some(previous) = &previous {
        if previous.artifacts.is_empty() {
            let wanted = normalize_name(&record.name);
            for entry in std::fs::read_dir(destination).map_err(|error| {
                format!(
                    "could not inspect extension directory {}: {error}",
                    destination.display()
                )
            })? {
                let entry = entry.map_err(|error| {
                    format!("could not inspect installed extension artifact: {error}")
                })?;
                let path = entry.path();
                let matches_legacy_name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| normalize_name(stem.trim_start_matches("lib")) == wanted)
                    .unwrap_or(false);
                if matches_legacy_name && is_dynamic_library(&path) {
                    stale_targets.push(path);
                }
            }
        } else {
            for old_name in &previous.artifacts {
                if !new_keys.contains(&native_artifact_identity(old_name)) {
                    stale_targets.push(checked_registered_artifact_path(destination, old_name)?);
                }
            }
        }
    }

    let installed_targets = new_artifacts
        .iter()
        .map(|(name, _)| destination.join(name))
        .collect::<Vec<_>>();
    if !force {
        for target in &installed_targets {
            match std::fs::symlink_metadata(target) {
                Ok(_) => {
                    return Err(format!(
                        "extension {} already exists; use --force to replace it",
                        target.display()
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "could not inspect extension target {}: {error}",
                        target.display()
                    ))
                }
            }
        }
    }

    record.artifacts = new_artifacts.iter().map(|(name, _)| name.clone()).collect();
    if let Some(existing) = records
        .iter_mut()
        .find(|candidate| candidate.name == record.name)
    {
        *existing = record;
    } else {
        records.push(record);
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));

    let mut replacements = Vec::with_capacity(new_artifacts.len() + 1);
    for ((_, source), target) in new_artifacts.iter().zip(&installed_targets) {
        replacements.push(NativeReplacement {
            staged: stage_native_artifact(source, target)?,
            target: target.clone(),
        });
    }
    let registry = serde_json::to_vec_pretty(&records).map_err(|error| error.to_string())?;
    replacements.push(NativeReplacement {
        staged: stage_native_bytes(&registry, metadata_path)?,
        target: metadata_path.to_path_buf(),
    });

    activate_native_transaction(&replacements, &stale_targets, &mut before_activate)?;
    Ok(installed_targets)
}

fn artifact_file_name(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("artifact has no UTF-8 file name: {}", path.display()))?;
    checked_registered_artifact_path(Path::new("."), name)?;
    Ok(name.to_string())
}

fn checked_registered_artifact_path(destination: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    let is_single_component = relative.components().count() == 1
        && relative.file_name().and_then(|value| value.to_str()) == Some(name);
    if !is_single_component || !is_dynamic_library(relative) {
        return Err(format!("unsafe native package artifact name `{name}`"));
    }
    Ok(destination.join(relative))
}

fn native_artifact_identity(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

fn transaction_path_identity(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn adjacent_transaction_path(target: &Path, kind: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("transaction target has no parent: {}", target.display()))?;
    let leaf = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    for _ in 0..8 {
        let candidate = parent.join(format!(
            ".{leaf}.rpi-{kind}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect transaction path {}: {error}",
                    candidate.display()
                ))
            }
        }
    }
    Err(format!(
        "could not allocate a transaction path beside {}",
        target.display()
    ))
}

fn stage_native_artifact(source: &Path, target: &Path) -> Result<StagedNativeFile, String> {
    let stage = adjacent_transaction_path(target, "stage")?;
    let result = (|| {
        let source_metadata = std::fs::symlink_metadata(source).map_err(|error| {
            format!(
                "could not inspect built artifact {}: {error}",
                source.display()
            )
        })?;
        let mut input = std::fs::File::open(source).map_err(|error| {
            format!(
                "could not open built artifact {}: {error}",
                source.display()
            )
        })?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)
            .map_err(|error| {
                format!(
                    "could not create staged artifact {}: {error}",
                    stage.display()
                )
            })?;
        std::io::copy(&mut input, &mut output)
            .and_then(|_| output.sync_all())
            .map_err(|error| {
                format!(
                    "could not write staged artifact {} beside {}: {error}",
                    stage.display(),
                    target.display()
                )
            })?;
        drop(output);
        std::fs::set_permissions(&stage, source_metadata.permissions()).map_err(|error| {
            format!(
                "could not preserve permissions on staged artifact {}: {error}",
                stage.display()
            )
        })
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&stage);
        return Err(error);
    }
    Ok(StagedNativeFile { path: stage })
}
fn stage_native_bytes(bytes: &[u8], target: &Path) -> Result<StagedNativeFile, String> {
    let stage = adjacent_transaction_path(target, "stage")?;
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)
            .map_err(|error| {
                format!(
                    "could not create staged registry {}: {error}",
                    stage.display()
                )
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                format!(
                    "could not flush staged registry {}: {error}",
                    stage.display()
                )
            })
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&stage);
        return Err(error);
    }
    Ok(StagedNativeFile { path: stage })
}

fn activate_native_transaction(
    replacements: &[NativeReplacement],
    removals: &[PathBuf],
    before_activate: &mut impl FnMut(usize, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let mut affected = Vec::new();
    let mut seen = HashSet::new();
    for target in replacements
        .iter()
        .map(|replacement| &replacement.target)
        .chain(removals.iter())
    {
        if seen.insert(transaction_path_identity(target)) {
            affected.push(target.clone());
        }
    }

    for target in &affected {
        match std::fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "refusing to replace non-regular native package file {}",
                    target.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect native package file {}: {error}",
                    target.display()
                ))
            }
        }
    }

    let mut backups = Vec::new();
    for target in &affected {
        if !target.exists() {
            continue;
        }
        let backup = match adjacent_transaction_path(target, "backup") {
            Ok(backup) => backup,
            Err(error) => {
                let rollback = rollback_native_transaction(&[], &backups);
                return Err(transaction_error(error, rollback));
            }
        };
        if let Err(error) = std::fs::rename(target, &backup) {
            let rollback = rollback_native_transaction(&[], &backups);
            return Err(transaction_error(
                format!("could not back up {}: {error}", target.display()),
                rollback,
            ));
        }
        backups.push(NativeBackup {
            target: target.clone(),
            backup,
        });
    }

    let mut activated = Vec::new();
    for (index, replacement) in replacements.iter().enumerate() {
        let activation = before_activate(index, &replacement.target).and_then(|()| {
            std::fs::rename(&replacement.staged.path, &replacement.target).map_err(|error| {
                format!(
                    "could not activate replacement {}: {error}",
                    replacement.target.display()
                )
            })
        });
        if let Err(error) = activation {
            let rollback = rollback_native_transaction(&activated, &backups);
            return Err(transaction_error(error, rollback));
        }
        activated.push(replacement.target.clone());
    }

    for backup in backups {
        if let Err(error) = std::fs::remove_file(&backup.backup) {
            eprintln!(
                "warning: native package updated but backup {} could not be removed: {error}",
                backup.backup.display()
            );
        }
    }
    Ok(())
}

fn rollback_native_transaction(
    activated: &[PathBuf],
    backups: &[NativeBackup],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for target in activated.iter().rev() {
        match std::fs::remove_file(target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => errors.push(format!("could not remove {}: {error}", target.display())),
        }
    }
    for backup in backups.iter().rev() {
        if let Err(error) = std::fs::rename(&backup.backup, &backup.target) {
            errors.push(format!(
                "could not restore {} from {}: {error}",
                backup.target.display(),
                backup.backup.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn transaction_error(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => error,
        Err(rollback) => format!("{error}; rollback also failed: {rollback}"),
    }
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

fn print_uninstall_help() {
    println!(
        "Usage: rpi uninstall <crate>\n\nRemove a Rust cdylib extension installed by `rpi install`.\n\nOptions:\n  --help, -h           Show this help\n\nExample:\n  rpi uninstall rpi-extension-example"
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

    fn native_record(name: &str) -> InstalledNativePackage {
        InstalledNativePackage {
            name: name.to_string(),
            version: "1.2.3".to_string(),
            source: None,
            artifacts: vec![format!("{name}.dll")],
        }
    }

    struct TempAgent {
        _guard: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TempAgent {
        fn new() -> Self {
            let guard = crate::config::test_support::env_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let temp = tempfile::tempdir().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, temp.path());
            Self {
                _guard: guard,
                temp,
                previous,
            }
        }

        fn metadata_path(&self) -> PathBuf {
            self.temp.path().join(NATIVE_PACKAGES_FILE)
        }
    }

    impl Drop for TempAgent {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
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

    #[test]
    fn cdylib_hint_recommends_unified_and_documents_legacy_compatibility() {
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register"));
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register_v2"));
        assert!(CDYLIB_EXTENSION_HINT.contains("rpi_plugin_register_v3"));
    }

    #[test]
    fn registry_read_distinguishes_missing_and_valid_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);

        assert_eq!(
            read_native_package_registry_at(&path).unwrap(),
            NativePackageRegistry::Missing
        );

        std::fs::write(
            &path,
            br#"[{"name":"demo","version":"1.2.3","source":null}]"#,
        )
        .unwrap();
        assert_eq!(
            read_native_package_registry_at(&path).unwrap(),
            NativePackageRegistry::Valid(vec![InstalledNativePackage {
                name: "demo".to_string(),
                version: "1.2.3".to_string(),
                source: None,
                artifacts: Vec::new(),
            }])
        );
    }

    #[test]
    fn registry_read_rejects_corrupt_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        std::fs::write(&path, b"[{").unwrap();

        let error = read_native_package_registry_at(&path).unwrap_err();
        assert!(error.contains("invalid JSON or schema"));
    }

    #[test]
    fn registry_read_rejects_wrong_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        std::fs::write(&path, br#"{"name":"demo","version":"1.2.3"}"#).unwrap();

        let error = read_native_package_registry_at(&path).unwrap_err();
        assert!(error.contains("invalid JSON or schema"));
    }

    #[test]
    fn record_does_not_replace_a_corrupt_registry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(NATIVE_PACKAGES_FILE);
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();

        assert!(record_native_package_at(&path, &native_record("demo")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn native_install_transaction_replaces_artifacts_and_removes_stale_ones() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata_path = temp.path().join(NATIVE_PACKAGES_FILE);
        let build = temp.path().join("build");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(destination.join("old.dll"), b"old-only").unwrap();
        std::fs::write(destination.join("same.dll"), b"old-shared").unwrap();
        let old = InstalledNativePackage {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: None,
            artifacts: vec!["old.dll".into(), "same.dll".into()],
        };
        write_native_packages_at(&metadata_path, &[old]).unwrap();
        std::fs::write(build.join("new.dll"), b"new-only").unwrap();
        std::fs::write(build.join("same.dll"), b"new-shared").unwrap();

        let installed = install_native_package_at(
            &destination,
            &metadata_path,
            InstalledNativePackage {
                name: "demo".into(),
                version: "2.0.0".into(),
                source: None,
                artifacts: Vec::new(),
            },
            &[build.join("new.dll"), build.join("same.dll")],
            true,
        )
        .unwrap();

        assert_eq!(
            installed,
            vec![destination.join("new.dll"), destination.join("same.dll")]
        );
        assert!(!destination.join("old.dll").exists());
        assert_eq!(
            std::fs::read(destination.join("new.dll")).unwrap(),
            b"new-only"
        );
        assert_eq!(
            std::fs::read(destination.join("same.dll")).unwrap(),
            b"new-shared"
        );
        let records = read_native_package_registry_at(&metadata_path)
            .unwrap()
            .into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].version, "2.0.0");
        assert_eq!(records[0].artifacts, vec!["new.dll", "same.dll"]);
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    #[test]
    fn native_install_transaction_rolls_back_artifacts_and_registry() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("extensions");
        let metadata_path = temp.path().join(NATIVE_PACKAGES_FILE);
        let build = temp.path().join("build");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(destination.join("old.dll"), b"old-only").unwrap();
        std::fs::write(destination.join("same.dll"), b"old-shared").unwrap();
        let old = InstalledNativePackage {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: None,
            artifacts: vec!["old.dll".into(), "same.dll".into()],
        };
        write_native_packages_at(&metadata_path, &[old]).unwrap();
        let original_registry = std::fs::read(&metadata_path).unwrap();
        std::fs::write(build.join("new.dll"), b"new-only").unwrap();
        std::fs::write(build.join("same.dll"), b"new-shared").unwrap();

        let error = install_native_package_at_with_hook(
            &destination,
            &metadata_path,
            InstalledNativePackage {
                name: "demo".into(),
                version: "2.0.0".into(),
                source: None,
                artifacts: Vec::new(),
            },
            &[build.join("new.dll"), build.join("same.dll")],
            true,
            |_, target| {
                if target == metadata_path {
                    Err("injected registry activation failure".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.contains("injected registry activation failure"));
        assert_eq!(std::fs::read(&metadata_path).unwrap(), original_registry);
        assert_eq!(
            std::fs::read(destination.join("old.dll")).unwrap(),
            b"old-only"
        );
        assert_eq!(
            std::fs::read(destination.join("same.dll")).unwrap(),
            b"old-shared"
        );
        assert!(!destination.join("new.dll").exists());
        assert_no_native_transaction_files(temp.path());
        assert_no_native_transaction_files(&destination);
    }

    fn assert_no_native_transaction_files(directory: &Path) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".rpi-stage-"), "left staged file: {name}");
            assert!(!name.contains(".rpi-backup-"), "left backup file: {name}");
        }
    }

    #[test]
    fn best_effort_registry_read_omits_corrupt_metadata() {
        let agent = TempAgent::new();
        std::fs::write(agent.metadata_path(), b"[{broken metadata").unwrap();

        assert!(installed_native_packages().is_empty());
    }

    #[test]
    fn install_fails_closed_before_work_when_registry_is_corrupt() {
        let agent = TempAgent::new();
        let path = agent.metadata_path();
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();

        assert_eq!(run(&args(&["demo"])), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!agent.temp.path().join("extensions").exists());
    }

    #[test]
    fn uninstall_fails_closed_without_removing_artifacts() {
        let agent = TempAgent::new();
        let path = agent.metadata_path();
        let original = b"[{broken metadata";
        std::fs::write(&path, original).unwrap();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        let artifact = extension_dir.join("demo.dll");
        std::fs::write(&artifact, b"extension").unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(artifact).unwrap(), b"extension");
    }

    #[test]
    fn uninstall_commits_artifact_and_registry_changes_together() {
        let agent = TempAgent::new();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(extension_dir.join("demo.dll"), b"demo").unwrap();
        std::fs::write(extension_dir.join("other.dll"), b"other").unwrap();
        write_native_packages_at(
            &agent.metadata_path(),
            &[native_record("demo"), native_record("other")],
        )
        .unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!extension_dir.join("demo.dll").exists());
        assert_eq!(
            std::fs::read(extension_dir.join("other.dll")).unwrap(),
            b"other"
        );
        assert_eq!(
            read_native_package_registry_at(&agent.metadata_path())
                .unwrap()
                .into_records(),
            vec![native_record("other")]
        );
        assert_no_native_transaction_files(agent.temp.path());
        assert_no_native_transaction_files(&extension_dir);
    }

    #[test]
    fn uninstall_uses_recorded_artifacts_without_name_fallback() {
        let agent = TempAgent::new();
        let extension_dir = agent.temp.path().join("extensions");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(extension_dir.join("recorded.dll"), b"managed").unwrap();
        std::fs::write(extension_dir.join("demo.dll"), b"unmanaged").unwrap();
        write_native_packages_at(
            &agent.metadata_path(),
            &[InstalledNativePackage {
                name: "demo".into(),
                version: "1.2.3".into(),
                source: None,
                artifacts: vec!["recorded.dll".into()],
            }],
        )
        .unwrap();

        assert_eq!(uninstall(&args(&["demo"])), 0);
        assert!(!extension_dir.join("recorded.dll").exists());
        assert_eq!(
            std::fs::read(extension_dir.join("demo.dll")).unwrap(),
            b"unmanaged"
        );
        assert!(!agent.metadata_path().exists());
        assert_no_native_transaction_files(agent.temp.path());
        assert_no_native_transaction_files(&extension_dir);
    }
}
