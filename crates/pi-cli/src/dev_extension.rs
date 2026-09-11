//! Development workflow for Rust-native rpi extensions.
//!
//! `rpi dev` detects a Cargo `cdylib`, builds it, stages a uniquely named copy
//! under `.rpi/extensions/.dev`, and optionally watches its source tree. Unique
//! staging directories are required on Windows because a loaded DLL cannot be
//! overwritten until the old `Library` handle is released by `/reload`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::args::Args;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevOptions {
    pub package: Option<String>,
    pub release: bool,
    pub watch: bool,
    pub passthrough: Vec<String>,
    pub help: bool,
}

pub fn parse_args(args: &[String]) -> Result<DevOptions, String> {
    let mut options = DevOptions {
        watch: true,
        ..DevOptions::default()
    };
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let value = &args[index];
        if passthrough {
            options.passthrough.push(value.clone());
        } else {
            match value.as_str() {
                "--" => passthrough = true,
                "--help" | "-h" => options.help = true,
                "--release" => options.release = true,
                "--no-watch" => options.watch = false,
                "--package" | "-P" => {
                    index += 1;
                    let package = args
                        .get(index)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or("rpi dev --package requires a package name")?;
                    options.package = Some(package.clone());
                }
                _ => options.passthrough.push(value.clone()),
            }
        }
        index += 1;
    }
    Ok(options)
}

pub fn print_help() {
    println!(
        "Usage: rpi dev [dev options] [--] [rpi options/messages...]\n\n\
Build the current Rust rpi extension, load it, and watch for changes.\n\n\
Dev options:\n  \
--package, -P <name>  Select a cdylib package in a multi-package workspace\n  \
--release              Build with Cargo's release profile\n  \
--no-watch             Build once; /reload still rebuilds manually\n  \
--help, -h             Show this help\n\n\
Examples:\n  \
rpi dev\n  \
rpi dev --package my-extension\n  \
rpi dev --release -- --model gateway/model\n"
    );
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
    workspace_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct MetadataPackage {
    name: String,
    manifest_path: PathBuf,
    targets: Vec<MetadataTarget>,
}

#[derive(Debug, Deserialize)]
struct MetadataTarget {
    name: String,
    crate_types: Vec<String>,
}

#[derive(Debug, Clone)]
struct ExtensionProject {
    package: String,
    target: String,
    manifest: PathBuf,
    package_root: PathBuf,
    workspace_root: PathBuf,
}

pub struct DevExtension {
    project: ExtensionProject,
    release: bool,
    watch: bool,
    stage_root: PathBuf,
    current_stage: Mutex<Option<PathBuf>>,
    build_lock: Mutex<()>,
    generation: AtomicU64,
    stop: AtomicBool,
}

impl DevExtension {
    pub fn detect(cwd: &Path, options: &DevOptions) -> Result<Arc<Self>, String> {
        let project = detect_project(cwd, options.package.as_deref())?;
        let stage_root = project
            .workspace_root
            .join(".rpi/extensions/.dev")
            .join(format!(
                "{}-{}",
                safe_name(&project.package),
                std::process::id()
            ));
        std::fs::create_dir_all(&stage_root).map_err(|error| {
            format!(
                "could not create extension staging directory {}: {error}",
                stage_root.display()
            )
        })?;
        Ok(Arc::new(Self {
            project,
            release: options.release,
            watch: options.watch,
            stage_root,
            current_stage: Mutex::new(None),
            build_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }))
    }

    pub fn package_name(&self) -> &str {
        &self.project.package
    }

    pub fn watch_enabled(&self) -> bool {
        self.watch
    }

    /// Compile and stage a fresh copy. The current stage changes only after a
    /// successful Cargo build and copy, so failed reloads keep the live plugin.
    pub fn rebuild(&self) -> Result<PathBuf, String> {
        let _guard = self.build_lock.lock().unwrap();
        eprintln!("dev: building extension {}...", self.project.package);
        let artifact = build_cdylib(&self.project, self.release)?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let stage = self.stage_root.join(generation.to_string());
        std::fs::create_dir_all(&stage)
            .map_err(|error| format!("could not create {}: {error}", stage.display()))?;
        let file_name = artifact
            .file_name()
            .ok_or_else(|| format!("Cargo artifact has no file name: {}", artifact.display()))?;
        let staged = stage.join(file_name);
        std::fs::copy(&artifact, &staged).map_err(|error| {
            format!(
                "could not stage extension {} as {}: {error}",
                artifact.display(),
                staged.display()
            )
        })?;
        *self.current_stage.lock().unwrap() = Some(stage.clone());
        eprintln!("dev: built {}", staged.display());
        Ok(stage)
    }

    pub fn apply_to_args(&self, args: &mut Args) -> Result<(), String> {
        let current = self
            .current_stage
            .lock()
            .unwrap()
            .clone()
            .ok_or("development extension has not been built")?;
        args.extensions_dir
            .retain(|path| !path.starts_with(&self.stage_root));
        args.extensions_dir.push(current);
        Ok(())
    }

    /// Poll the extension inputs in a background thread. A change signals the
    /// existing reload mailbox; the TUI serializes compilation and the plugin
    /// swap on its async main loop, so watcher and manual reloads share one path.
    pub fn start_watcher(
        self: &Arc<Self>,
        mailbox: rpi_extensions::ReloadMailbox,
    ) -> Option<std::thread::JoinHandle<()>> {
        if !self.watch {
            return None;
        }
        let weak = Arc::downgrade(self);
        let mut fingerprint = source_fingerprint(&self.project);
        eprintln!(
            "dev: watching {} (use /reload to rebuild manually)",
            self.project.package_root.display()
        );
        Some(std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(650));
            let Some(dev) = weak.upgrade() else {
                break;
            };
            if dev.stop.load(Ordering::SeqCst) {
                break;
            }
            let next = source_fingerprint(&dev.project);
            if next == fingerprint {
                continue;
            }
            fingerprint = next;
            if mailbox.signal().is_err() {
                eprintln!("dev: source changed; run /reload to rebuild it");
            }
        }))
    }

    pub fn stop_watcher(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn cleanup(&self) {
        if std::fs::remove_dir_all(&self.stage_root).is_ok() {
            remove_empty_dev_dir(&self.stage_root);
            return;
        }
        // A host-side keepalive can retain the mapped DLL until process exit.
        // A detached copy of rpi retries after this process releases the file.
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe)
                .arg("__rpi_dev_cleanup")
                .arg(&self.stage_root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

impl Drop for DevExtension {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn detect_project(cwd: &Path, requested: Option<&str>) -> Result<ExtensionProject, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not execute cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid cargo metadata output: {error}"))?;
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut candidates = Vec::new();
    for package in metadata.packages {
        if requested.is_some_and(|name| package.name != name) {
            continue;
        }
        let package_root = package
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        for target in package
            .targets
            .iter()
            .filter(|target| target.crate_types.iter().any(|kind| kind == "cdylib"))
        {
            candidates.push(ExtensionProject {
                package: package.name.clone(),
                target: target.name.clone(),
                manifest: package.manifest_path.clone(),
                package_root: package_root.clone(),
                workspace_root: metadata.workspace_root.clone(),
            });
        }
    }
    if candidates.is_empty() {
        return Err(match requested {
            Some(name) => format!("Cargo package `{name}` is not a cdylib extension"),
            None => "current Cargo project does not contain a cdylib extension".to_string(),
        });
    }
    if requested.is_none() {
        let mut local: Vec<_> = candidates
            .iter()
            .filter(|project| cwd.starts_with(&project.package_root))
            .cloned()
            .collect();
        local.sort_by_key(|project| std::cmp::Reverse(project.package_root.components().count()));
        if let Some(project) = local.into_iter().next() {
            return Ok(project);
        }
    }
    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }
    Err(format!(
        "multiple cdylib extensions found: {}. Select one with `rpi dev --package <name>`",
        candidates
            .iter()
            .map(|project| project.package.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn build_cdylib(project: &ExtensionProject, release: bool) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .args(["--manifest-path"])
        .arg(&project.manifest)
        .args([
            "--package",
            &project.package,
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&project.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if release {
        command.arg("--release");
    }
    let output = command
        .output()
        .map_err(|error| format!("could not execute cargo build: {error}"))?;
    let mut artifact = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(rendered) = value
            .get("message")
            .and_then(|message| message.get("rendered"))
            .and_then(serde_json::Value::as_str)
        {
            eprint!("{rendered}");
        }
        if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact")
            || value
                .get("target")
                .and_then(|target| target.get("name"))
                .and_then(serde_json::Value::as_str)
                != Some(project.target.as_str())
        {
            continue;
        }
        artifact = value
            .get("filenames")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(PathBuf::from)
            .find(|path| is_cdylib(path));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        return Err(format!("cargo build exited with {}", output.status));
    }
    artifact.ok_or_else(|| {
        format!(
            "Cargo built `{}` but did not report a cdylib artifact for `{}`",
            project.package, project.target
        )
    })
}

fn is_cdylib(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("dll" | "so" | "dylib")
    )
}

fn safe_name(value: &str) -> String {
    value
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

fn source_fingerprint(project: &ExtensionProject) -> u64 {
    let mut hasher = DefaultHasher::new();
    fingerprint_path(&project.package_root.join("src"), &mut hasher);
    for path in [
        project.manifest.clone(),
        project.package_root.join("build.rs"),
        project.workspace_root.join("Cargo.toml"),
        project.workspace_root.join("Cargo.lock"),
    ] {
        fingerprint_file(&path, &mut hasher);
    }
    hasher.finish()
}

fn fingerprint_path(path: &Path, hasher: &mut DefaultHasher) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            fingerprint_path(&path, hasher);
        } else if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("rs" | "toml" | "json")
        ) {
            fingerprint_file(&path, hasher);
        }
    }
}

fn fingerprint_file(path: &Path, hasher: &mut DefaultHasher) {
    if let Ok(metadata) = std::fs::metadata(path) {
        path.hash(hasher);
        metadata.len().hash(hasher);
        metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .hash(hasher);
    }
}

pub fn run_cleanup_helper(args: &[String]) -> i32 {
    let Some(raw) = args.first() else {
        return 2;
    };
    let path = PathBuf::from(raw);
    if !is_dev_stage_root(&path) {
        return 2;
    }
    for _ in 0..100 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                remove_empty_dev_dir(&path);
                return 0;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    1
}

fn is_dev_stage_root(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|v| v.to_str())
        == Some(".dev")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|v| v.to_str())
            == Some("extensions")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|v| v.to_str())
            == Some(".rpi")
}

fn remove_empty_dev_dir(stage_root: &Path) {
    if let Some(parent) = stage_root.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dev_options_without_consuming_rpi_options() {
        let options = parse_args(&[
            "--package".into(),
            "demo".into(),
            "--release".into(),
            "--model".into(),
            "gateway/model".into(),
        ])
        .unwrap();
        assert_eq!(options.package.as_deref(), Some("demo"));
        assert!(options.release);
        assert!(options.watch);
        assert_eq!(options.passthrough, ["--model", "gateway/model"]);
    }

    #[test]
    fn no_watch_is_honored() {
        let options = parse_args(&["--no-watch".into()]).unwrap();
        assert!(!options.watch);
    }

    #[test]
    fn cleanup_helper_rejects_non_dev_paths() {
        assert!(!is_dev_stage_root(Path::new("target/debug")));
        assert!(is_dev_stage_root(Path::new(
            "workspace/.rpi/extensions/.dev/example-123"
        )));
    }
}
