//! Discovery of Pi-compatible package resources.
//!
//! This module resolves package manifests and static resource paths. Extension
//! paths are handed to the Node bridge by `js_extensions`; the Rust cdylib
//! loader remains a separate extension mechanism. A package is a directory containing a
//! `package.json` (or a conventional `skills/`, `prompts/`, `themes/` tree).
//! The optional `pi`/`rpi` manifest object may override those resource paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRoot {
    pub root: PathBuf,
    pub name: String,
    pub version: Option<String>,
    pub manifest: Option<PathBuf>,
    skills: Vec<PathBuf>,
    prompts: Vec<PathBuf>,
    themes: Vec<PathBuf>,
    system_prompts: Vec<PathBuf>,
    append_system_prompts: Vec<PathBuf>,
    /// JavaScript/TypeScript extension entry paths declared by `pi.extensions`
    /// or `rpi.extensions`. A directory is expanded by the JS host.
    pub extensions: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDiagnostic {
    pub spec: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct PackageResources {
    pub packages: Vec<PackageRoot>,
    pub diagnostics: Vec<PackageDiagnostic>,
}

impl PackageResources {
    pub fn extension_paths(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| p.extensions.iter().cloned())
            .collect()
    }
    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| p.skills.iter().cloned())
            .collect()
    }

    pub fn prompt_dirs(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| p.prompts.iter().cloned())
            .collect()
    }

    pub fn theme_files(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| {
                p.themes.iter().flat_map(|path| {
                    if path.is_dir() {
                        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
                            .ok()
                            .into_iter()
                            .flatten()
                            .filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .filter(|file| {
                                file.is_file()
                                    && file.extension().and_then(|ext| ext.to_str()) == Some("json")
                            })
                            .collect();
                        files.sort();
                        files
                    } else {
                        vec![path.clone()]
                    }
                })
            })
            .collect()
    }

    pub fn system_prompt_files(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| p.system_prompts.iter().cloned())
            .collect()
    }

    pub fn append_system_prompt_files(&self) -> Vec<PathBuf> {
        self.packages
            .iter()
            .flat_map(|p| p.append_system_prompts.iter().cloned())
            .collect()
    }

    pub fn find_theme(&self, name: &str) -> Option<PathBuf> {
        let wanted = Path::new(name);
        self.theme_files().into_iter().find(|path| {
            path == wanted
                || path.file_stem().and_then(|s| s.to_str()) == Some(name)
                || path.file_name().and_then(|s| s.to_str()) == Some(name)
        })
    }
}

/// Resolve package specs from the settings file and conventional local roots.
/// Empty or missing `packages` means no packages are enabled, matching Pi's
/// explicit package list instead of silently executing every directory found
/// under the user's home directory.
pub fn discover_from_settings(cwd: &Path) -> PackageResources {
    let mut specs = Vec::new();
    for settings in crate::settings::load_project_settings(cwd) {
        if let Some(packages) = settings.packages {
            specs.extend(packages);
        }
    }
    if let Ok(settings) = crate::settings::load_settings() {
        if let Some(packages) = settings.packages {
            specs.extend(packages);
        }
    }
    discover(cwd, &specs)
}

/// Discover packages in settings order. Package resources are intentionally
/// returned after project and global resources; callers append these paths last
/// so a package cannot shadow a project-local or user-local resource.
pub fn discover(cwd: &Path, specs: &[String]) -> PackageResources {
    let mut out = PackageResources::default();
    let mut seen = HashSet::new();
    for spec in specs
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        let Some(root) = resolve_spec(cwd, spec) else {
            out.diagnostics.push(PackageDiagnostic {
                spec: spec.to_string(),
                message: "package path/name could not be resolved".to_string(),
            });
            continue;
        };
        let key = normalize_key(&root);
        if !seen.insert(key) {
            continue;
        }
        match load_package(root, spec) {
            Ok(package) => out.packages.push(package),
            Err(message) => out.diagnostics.push(PackageDiagnostic {
                spec: spec.to_string(),
                message,
            }),
        }
    }
    out
}

/// Validate and load one package spec. Used by `rpi package add` before the
/// spec is persisted to settings.
pub fn resolve_package(cwd: &Path, spec: &str) -> Result<PackageRoot, String> {
    let root = resolve_spec(cwd, spec)
        .ok_or_else(|| "package path/name could not be resolved".to_string())?;
    load_package(root, spec)
}

/// Load a theme from an explicit JSON path without discovering configured Pi
/// packages. Startup code that has passed the package gate uses
/// [`load_theme_with_resources`] to resolve package theme names.
pub fn load_theme(cwd: &Path, name_or_path: &str) -> Result<rpi_tui::Theme, String> {
    load_theme_with_resources(cwd, name_or_path, &PackageResources::default())
}

/// Load a package theme from an already-resolved resource set. Startup callers
/// use this variant so a disabled package configuration cannot be re-discovered
/// indirectly from a TUI theme selector.
pub fn load_theme_with_resources(
    _cwd: &Path,
    name_or_path: &str,
    resources: &PackageResources,
) -> Result<rpi_tui::Theme, String> {
    let path = {
        let direct = PathBuf::from(name_or_path);
        if direct.is_file() {
            Some(direct)
        } else {
            resources.find_theme(name_or_path)
        }
    }
    .ok_or_else(|| format!("theme `{name_or_path}` was not found in enabled packages"))?;
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read theme {}: {error}", path.display()))?;
    let value = parse_json_with_comments(&text)
        .map_err(|error| format!("invalid theme {}: {error}", path.display()))?;
    let mut theme = rpi_tui::Theme::default();
    let colors = value.get("colors").unwrap_or(&value);
    let target = &mut theme.colors;
    macro_rules! color {
        ($field:ident, $($key:literal),+ $(,)?) => {
            if let Some(value) = first_value(colors, &[$($key),+]) {
                if let Some(parsed) = parse_color(value) {
                    target.$field = parsed;
                }
            }
        };
    }
    color!(text, "text");
    color!(muted, "muted");
    color!(dim, "dim");
    color!(accent, "accent");
    color!(error, "error");
    color!(success, "success");
    color!(warning, "warning");
    color!(info, "info");
    color!(background, "background", "bg");
    color!(surface, "surface", "userMessageBg");
    color!(border, "border");
    color!(border_accent, "borderAccent");
    color!(border_muted, "borderMuted");
    color!(selection, "selection", "selectedBg");
    color!(cursor, "cursor");
    color!(thinking_text, "thinkingText");
    color!(md_heading, "mdHeading");
    color!(md_link, "mdLink");
    color!(md_link_url, "mdLinkUrl");
    color!(md_code, "mdCode");
    color!(md_code_bg, "mdCodeBg");
    color!(md_code_block, "mdCodeBlock");
    color!(md_code_block_bg, "mdCodeBlockBg");
    color!(md_code_block_border, "mdCodeBlockBorder");
    color!(md_quote, "mdQuote");
    color!(md_quote_border, "mdQuoteBorder");
    color!(md_hr, "mdHr");
    color!(md_list_bullet, "mdListBullet");
    color!(tool_pending_bg, "toolPendingBg");
    color!(tool_success_bg, "toolSuccessBg");
    color!(tool_error_bg, "toolErrorBg");
    color!(tool_title, "toolTitle");
    color!(tool_output, "toolOutput");
    color!(bash_mode, "bashMode");
    color!(tool_diff_added, "toolDiffAdded");
    color!(tool_diff_removed, "toolDiffRemoved");
    color!(tool_diff_context, "toolDiffContext");
    if let Some(border) = value.get("borderStyle").and_then(Value::as_str) {
        theme.border_style = match border.to_ascii_lowercase().as_str() {
            "sharp" => rpi_tui::theme::BorderStyle::Sharp,
            "double" => rpi_tui::theme::BorderStyle::Double,
            "thick" => rpi_tui::theme::BorderStyle::Thick,
            "none" => rpi_tui::theme::BorderStyle::None,
            _ => rpi_tui::theme::BorderStyle::Rounded,
        };
    }
    if let Some(corner) = value.get("cornerStyle").and_then(Value::as_str) {
        theme.corner_style = if corner.eq_ignore_ascii_case("sharp") {
            rpi_tui::theme::CornerStyle::Sharp
        } else {
            rpi_tui::theme::CornerStyle::Rounded
        };
    }
    Ok(theme)
}

fn first_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}

fn parse_color(value: &Value) -> Option<rpi_tui::Color> {
    match value {
        Value::String(raw) => {
            let value = raw.trim();
            let hex = value.strip_prefix('#')?;
            if hex.len() == 6 {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(rpi_tui::Color::Rgb(r, g, b))
            } else if let Some(index) = value.strip_prefix("ansi256:") {
                Some(rpi_tui::Color::Ansi256(index.parse().ok()?))
            } else {
                None
            }
        }
        Value::Array(values) if values.len() == 3 => Some(rpi_tui::Color::Rgb(
            values[0].as_u64()?.try_into().ok()?,
            values[1].as_u64()?.try_into().ok()?,
            values[2].as_u64()?.try_into().ok()?,
        )),
        Value::Object(map) => {
            let r = map.get("r")?.as_u64()?.try_into().ok()?;
            let g = map.get("g")?.as_u64()?.try_into().ok()?;
            let b = map.get("b")?.as_u64()?.try_into().ok()?;
            Some(rpi_tui::Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

/// `rpi package ...` command for managing the enabled Pi package list. This is
/// a local package manager; use `install-pi` when the package must be fetched.
pub fn run_cli(args: &[String]) -> i32 {
    let command = args.first().map(String::as_str).unwrap_or("list");
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not determine current directory: {error}");
            return 1;
        }
    };
    match command {
        "list" => {
            let resources = discover_from_settings(&cwd);
            let native = crate::install::installed_native_packages();
            if args.iter().any(|arg| arg == "--json") {
                let mut values: Vec<_> = resources
                    .packages
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "version": p.version,
                            "type": "ts",
                            "root": p.root,
                            "manifest": p.manifest,
                            "skills": p.skill_dirs_for_display(),
                            "prompts": p.prompt_dirs_for_display(),
                            "themes": p.theme_files_for_display(),
                        })
                    })
                    .collect();
                values.extend(native.iter().map(|package| {
                    serde_json::json!({
                        "name": package.name,
                        "version": package.version,
                        "type": "rust",
                        "source": package.source,
                    })
                }));
                println!(
                    "{}",
                    serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".into())
                );
            } else if resources.packages.is_empty() && native.is_empty() {
                println!("no Pi packages enabled");
            } else {
                for package in &resources.packages {
                    let version = package.version.as_deref().unwrap_or("-");
                    println!("{}@{} {}", package.name, version, package.root.display());
                }
                for package in native {
                    let source = package.source.as_deref().unwrap_or("crates.io");
                    println!("{}@{} [rust] {}", package.name, package.version, source);
                }
            }
            for diagnostic in resources.diagnostics {
                eprintln!(
                    "warning: package {}: {}",
                    diagnostic.spec, diagnostic.message
                );
            }
            0
        }
        "add" => {
            let Some(spec) = args.get(1).filter(|s| !s.starts_with('-')) else {
                eprintln!("error: missing package path or name");
                print_help();
                return 2;
            };
            if let Err(error) = resolve_package(&cwd, spec) {
                eprintln!("error: {error}");
                return 1;
            }
            let mut settings = crate::settings::load_settings().unwrap_or_default();
            let packages = settings.packages.get_or_insert_with(Vec::new);
            if !packages.iter().any(|existing| existing == spec) {
                packages.push(spec.clone());
                if let Err(error) = crate::settings::save_settings(&settings) {
                    eprintln!("error: could not save package settings: {error}");
                    return 1;
                }
                println!("enabled Pi package {spec}");
            } else {
                println!("Pi package already enabled: {spec}");
            }
            0
        }
        "remove" | "rm" => {
            let Some(spec) = args.get(1).filter(|s| !s.starts_with('-')) else {
                eprintln!("error: missing package path or name");
                print_help();
                return 2;
            };
            let mut settings = crate::settings::load_settings().unwrap_or_default();
            let Some(packages) = settings.packages.as_mut() else {
                println!("Pi package is not enabled: {spec}");
                return 0;
            };
            let before = packages.len();
            packages.retain(|existing| existing != spec);
            if packages.len() == before {
                println!("Pi package is not enabled: {spec}");
                return 0;
            }
            if packages.is_empty() {
                settings.packages = None;
            }
            if let Err(error) = crate::settings::save_settings(&settings) {
                eprintln!("error: could not save package settings: {error}");
                return 1;
            }
            println!("disabled Pi package {spec}");
            0
        }
        "update" => update_packages(&cwd),
        "help" | "--help" | "-h" => {
            print_help();
            0
        }
        other => {
            eprintln!("error: unknown package command `{other}`");
            print_help();
            2
        }
    }
}

fn print_help() {
    println!(
        "Usage: rpi package <command>\n\nCommands:\n  list [--json]      List enabled TS packages and installed Rust extensions\n  add <path-or-name> Enable a local/package.json package\n  remove <path-or-name>\n                     Disable a Pi package\n  update             Update TS npm/git packages and Rust crates.io extensions\n\nTS package resources are loaded from skills/, prompts/, themes/, SYSTEM.md, APPEND_SYSTEM.md, and extensions. Rust-native extensions are installed with `rpi install`."
    );
}

fn update_packages(cwd: &Path) -> i32 {
    let native = crate::install::installed_native_packages();
    let resources = discover_from_settings(cwd);
    if resources.packages.is_empty() && native.is_empty() {
        println!("no Pi packages enabled");
        return 0;
    }
    let mut updated = 0;
    let mut skipped = 0;
    for package in native {
        if package.source.is_some() {
            println!(
                "skipped local Rust package {} (no registry source)",
                package.name
            );
            skipped += 1;
            continue;
        }
        let args = vec![package.name.clone(), "--force".to_string()];
        if crate::install::run(&args) == 0 {
            updated += 1;
        } else {
            eprintln!("warning: could not update Rust package {}", package.name);
        }
    }
    for package in resources.packages {
        let root = package.root;
        if root.join(".git").is_dir() {
            match std::process::Command::new("git")
                .args(["-C"])
                .arg(&root)
                .args(["pull", "--ff-only"])
                .status()
            {
                Ok(status) if status.success() => {
                    println!("updated git package {}", package.name);
                    updated += 1;
                }
                Ok(status) => eprintln!(
                    "warning: could not update {} (git exited with {status})",
                    package.name
                ),
                Err(error) => eprintln!("warning: could not update {}: {error}", package.name),
            }
            continue;
        }
        if is_registry_package_path(&root) {
            match crate::install_pi::update_npm_package(&root, &package.name) {
                Ok(_) => {
                    println!("updated npm package {}", package.name);
                    updated += 1;
                }
                Err(error) => eprintln!("warning: could not update {}: {error}", package.name),
            }
        } else {
            println!(
                "skipped local package {} (no registry source)",
                package.name
            );
            skipped += 1;
        }
    }
    println!("package update complete: {updated} updated, {skipped} skipped");
    0
}

fn is_registry_package_path(path: &Path) -> bool {
    let text = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    text.contains("/.rpi/packages/")
        || text.contains("/.pi/packages/")
        || text.contains("/agent/packages/")
}

impl PackageRoot {
    fn skill_dirs_for_display(&self) -> Vec<PathBuf> {
        self.skills.clone()
    }

    fn prompt_dirs_for_display(&self) -> Vec<PathBuf> {
        self.prompts.clone()
    }

    fn theme_files_for_display(&self) -> Vec<PathBuf> {
        if self.themes.len() == 1 && self.themes[0].is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&self.themes[0])
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|file| {
                    file.is_file() && file.extension().and_then(|ext| ext.to_str()) == Some("json")
                })
                .collect();
            files.sort();
            files
        } else {
            self.themes.clone()
        }
    }
}

fn resolve_spec(cwd: &Path, spec: &str) -> Option<PathBuf> {
    let file_spec = spec.strip_prefix("file:");
    let raw = file_spec.unwrap_or(spec);
    // `npm:` is a package-source prefix, not part of the on-disk package
    // name. Keeping it in the candidates makes installed npm packages look
    // like directories literally named `npm:...`.
    let npm_name = raw.strip_prefix("npm:").unwrap_or(raw);
    let package_name = package_name_without_version(npm_name);
    let package_key = package_name
        .strip_prefix('@')
        .unwrap_or(package_name)
        .replace('/', "__");
    let direct = PathBuf::from(npm_name);
    let mut candidates = Vec::new();
    if direct.is_absolute() {
        candidates.push(direct);
    } else {
        let explicit_relative_path =
            file_spec.is_some() || npm_name.starts_with('.') || npm_name.starts_with("./");
        if explicit_relative_path {
            candidates.push(cwd.join(&direct));
        }
        // Prefer rpi-owned package stores over native Pi stores and generic
        // node_modules when a bare package name resolves in more than one
        // place.
        candidates.push(cwd.join(".rpi/packages").join(package_name));
        if package_key != package_name {
            candidates.push(cwd.join(".rpi/packages").join(&package_key));
        }
        candidates.push(cwd.join(".pi/packages").join(package_name));
        if package_key != package_name {
            candidates.push(cwd.join(".pi/packages").join(&package_key));
        }
        for ancestor in cwd.ancestors() {
            candidates.push(ancestor.join("node_modules").join(package_name));
        }
        if let Ok(agent) = config::agent_dir() {
            candidates.push(agent.join("packages").join(package_name));
            if package_key != package_name {
                candidates.push(agent.join("packages").join(&package_key));
            }
            // Pi's native npm installer keeps packages under
            // ~/.pi/agent/npm/node_modules rather than ~/.pi/agent/packages.
            // Keep the same layout usable when rpi reads Pi's settings.json.
            candidates.push(agent.join("npm/node_modules").join(package_name));
            if package_key != package_name {
                candidates.push(agent.join("npm/node_modules").join(&package_key));
            }
        }
        if let Some(home) = dirs::home_dir() {
            // Keep native Pi's installed package store usable when the user
            // has not copied it into the rpi-owned config directory yet.
            candidates.push(home.join(".pi/agent/packages").join(package_name));
            if package_key != package_name {
                candidates.push(home.join(".pi/agent/packages").join(&package_key));
            }
            candidates.push(home.join(".pi/agent/npm/node_modules").join(package_name));
            if package_key != package_name {
                candidates.push(home.join(".pi/agent/npm/node_modules").join(&package_key));
            }
        }
        if !explicit_relative_path {
            candidates.push(cwd.join(package_name));
        }
    }
    for candidate in candidates {
        if candidate.is_file()
            && candidate.file_name().and_then(|s| s.to_str()) == Some("package.json")
        {
            return candidate.parent().map(Path::to_path_buf);
        }
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Strip an npm version suffix while preserving the `@scope/name` portion.
fn package_name_without_version(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix('@') {
        rest.find('@')
            .map(|index| &name[..index + 1])
            .unwrap_or(name)
    } else {
        name.split('@').next().unwrap_or(name)
    }
}

fn load_package(root: PathBuf, spec: &str) -> Result<PackageRoot, String> {
    let manifest_path = root.join("package.json");
    let raw =
        match std::fs::read_to_string(&manifest_path) {
            Ok(text) => Some(parse_json_with_comments(&text).map_err(|e| {
                format!("invalid package manifest {}: {e}", manifest_path.display())
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("could not read {}: {e}", manifest_path.display())),
        };
    let name = raw
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| root.file_name().and_then(|s| s.to_str()).map(str::to_owned))
        .unwrap_or_else(|| spec.to_string());
    let version = raw
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let manifest = raw.as_ref().map(|_| manifest_path);
    // rpi-specific manifest settings win per resource key; a missing rpi key
    // falls back to the original Pi key so partial migrations stay compatible.
    let rpi = raw
        .as_ref()
        .and_then(|v| v.get("rpi"))
        .unwrap_or(&Value::Null);
    let pi = raw
        .as_ref()
        .and_then(|v| v.get("pi"))
        .unwrap_or(&Value::Null);

    Ok(PackageRoot {
        skills: resource_paths(&root, raw.as_ref(), rpi, pi, "skills", "skills"),
        prompts: resource_paths(&root, raw.as_ref(), rpi, pi, "prompts", "prompts"),
        themes: resource_paths(&root, raw.as_ref(), rpi, pi, "themes", "themes"),
        system_prompts: file_paths(
            &root,
            raw.as_ref(),
            rpi,
            pi,
            &["systemPrompt", "system_prompt", "system"],
            "SYSTEM.md",
        ),
        append_system_prompts: file_paths(
            &root,
            raw.as_ref(),
            rpi,
            pi,
            &["appendSystemPrompt", "append_system_prompt", "appendSystem"],
            "APPEND_SYSTEM.md",
        ),
        extensions: resource_paths(&root, raw.as_ref(), rpi, pi, "extensions", "extensions"),
        root,
        name,
        version,
        manifest,
    })
}

fn parse_json_with_comments(text: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(first) => serde_json::from_str(&config::strip_line_comments(text)).map_err(|_| first),
    }
}

fn resource_paths(
    root: &Path,
    top: Option<&Value>,
    rpi: &Value,
    pi: &Value,
    key: &str,
    default_dir: &str,
) -> Vec<PathBuf> {
    let values = rpi
        .get(key)
        .or_else(|| pi.get(key))
        .or_else(|| top.and_then(|v| v.get(key)));
    let mut paths = values
        .map(|v| string_values(v).into_iter().map(|p| root.join(p)).collect())
        .unwrap_or_else(|| vec![root.join(default_dir)]);
    paths.retain(|p: &PathBuf| p.exists());
    paths
}

fn file_paths(
    root: &Path,
    top: Option<&Value>,
    rpi: &Value,
    pi: &Value,
    keys: &[&str],
    default_file: &str,
) -> Vec<PathBuf> {
    let value = keys.iter().find_map(|key| {
        rpi.get(*key)
            .or_else(|| pi.get(*key))
            .or_else(|| top.and_then(|v| v.get(*key)))
    });
    let mut paths = value
        .map(|v| string_values(v).into_iter().map(|p| root.join(p)).collect())
        .unwrap_or_else(|| vec![root.join(default_file)]);
    paths.retain(|p: &PathBuf| p.is_file());
    paths
}

fn string_values(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn normalize_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_conventional_and_manifest_resources() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pkg");
        std::fs::create_dir_all(root.join("custom-skills")).unwrap();
        std::fs::create_dir_all(root.join("rpi-skills")).unwrap();
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("legacy-prompts")).unwrap();
        std::fs::create_dir_all(root.join("themes")).unwrap();
        std::fs::write(root.join("custom-skills/a.md"), "---\nname: a\n---\nbody").unwrap();
        std::fs::write(root.join("prompts/explain.md"), "explain").unwrap();
        std::fs::write(root.join("themes/ocean.json"), "{}").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"demo","version":"1.0.0","pi":{"skills":["custom-skills"],"prompts":["legacy-prompts"]},"rpi":{"skills":["rpi-skills"]}}"#,
        )
        .unwrap();

        let resources = discover(tmp.path(), &[root.to_string_lossy().into_owned()]);
        assert_eq!(resources.packages.len(), 1);
        assert_eq!(resources.packages[0].name, "demo");
        assert_eq!(resources.skill_dirs(), vec![root.join("rpi-skills")]);
        assert_eq!(resources.prompt_dirs(), vec![root.join("legacy-prompts")]);
        assert_eq!(
            resources.theme_files(),
            vec![root.join("themes/ocean.json")]
        );
    }

    #[test]
    fn resolves_package_json_spec_and_deduplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pkg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        let manifest = root.join("package.json").to_string_lossy().into_owned();
        let resources = discover(
            tmp.path(),
            &[manifest.clone(), root.to_string_lossy().into_owned()],
        );
        assert_eq!(resources.packages.len(), 1);
        assert!(resources.diagnostics.is_empty());
    }

    #[test]
    fn bare_package_name_prefers_project_rpi_store_over_legacy_pi_store() {
        let tmp = tempfile::tempdir().unwrap();
        let rpi_root = tmp.path().join(".rpi/packages/demo");
        let pi_root = tmp.path().join(".pi/packages/demo");
        std::fs::create_dir_all(rpi_root.join("skills")).unwrap();
        std::fs::create_dir_all(pi_root.join("skills")).unwrap();
        std::fs::write(
            rpi_root.join("package.json"),
            r#"{"name":"rpi-demo","version":"rpi"}"#,
        )
        .unwrap();
        std::fs::write(
            pi_root.join("package.json"),
            r#"{"name":"pi-demo","version":"pi"}"#,
        )
        .unwrap();

        let resources = discover(tmp.path(), &["demo".to_string()]);
        assert_eq!(resources.packages.len(), 1);
        assert_eq!(resources.packages[0].root, rpi_root);
        assert_eq!(resources.packages[0].version.as_deref(), Some("rpi"));
    }

    #[test]
    fn npm_scoped_spec_resolves_installed_safe_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".rpi/packages/narumitw__pi-btw");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"@narumitw/pi-btw","version":"0.58.1"}"#,
        )
        .unwrap();

        let resources = discover(tmp.path(), &["npm:@narumitw/pi-btw".to_string()]);
        assert_eq!(resources.packages.len(), 1);
        assert!(resources.diagnostics.is_empty());
        assert_eq!(resources.packages[0].name, "@narumitw/pi-btw");
    }

    #[test]
    fn npm_scoped_spec_resolves_project_store_and_versioned_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".pi/packages/@scope/demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"@scope/demo","version":"1.2.3"}"#,
        )
        .unwrap();

        for spec in ["npm:@scope/demo", "npm:@scope/demo@1.2.3"] {
            let resources = discover(tmp.path(), &[spec.to_string()]);
            assert!(resources.diagnostics.is_empty(), "spec={spec}");
            assert_eq!(resources.packages[0].root, root, "spec={spec}");
        }
    }
}
