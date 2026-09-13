//! Lightweight update discovery and update commands.
//!
//! Startup checks are deliberately best-effort: they run only for an
//! interactive terminal, use a short timeout, and cache the result so a
//! temporary registry outage never blocks the agent.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::join_all;
use serde::{Deserialize, Serialize};

const CACHE_FILE: &str = "update-check.json";
const CACHE_TTL_MS: i64 = 6 * 60 * 60 * 1000;
const REQUEST_TIMEOUT_MS: u64 = 1800;
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates/rpi-cli";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub notices: Vec<UpdateNotice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCache {
    checked_at: i64,
    rpi_latest: Option<String>,
    packages: BTreeMap<String, String>,
    /// Timestamp for the last opt-in Pi package check. This is separate from
    /// `checked_at` because the default startup path deliberately skips npm
    /// package discovery and must not make stale package data look fresh.
    #[serde(default)]
    packages_checked_at: i64,
    #[serde(default)]
    native_packages: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_version: String,
}

#[derive(Debug, Deserialize)]
struct NpmResponse {
    dist_tags: Option<DistTags>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

/// Perform a cached, best-effort check with Pi package discovery disabled.
///
/// Keep this compatibility entry point conservative: callers that have not
/// explicitly opted into Pi packages must never parse package settings.
pub async fn check_startup(cwd: &Path) -> UpdateReport {
    check_startup_with_packages(cwd, false).await
}

/// Perform a cached, best-effort startup check.
///
/// enable_pi_packages is the already-resolved runtime gate
/// (--enable-pi-packages plus the --no-extensions kill switch). Package
/// settings are only discovered and checked when this is true.
pub async fn check_startup_with_packages(cwd: &Path, enable_pi_packages: bool) -> UpdateReport {
    if std::env::var_os("RPI_DISABLE_UPDATE_CHECK").is_some() {
        return UpdateReport::default();
    }
    let cache_path = match crate::config::agent_dir() {
        Ok(dir) => dir.join(CACHE_FILE),
        Err(_) => return UpdateReport::default(),
    };
    let previous_cache = read_cache(&cache_path);
    if let Some(cache) = previous_cache.as_ref() {
        if cache_is_fresh(cache, enable_pi_packages) {
            return report_from_cache(&cache, cwd, enable_pi_packages);
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
    {
        Ok(client) => client,
        Err(_) => return UpdateReport::default(),
    };
    let rpi_latest = fetch_rpi_latest(&client).await;
    let package_names = if enable_pi_packages {
        crate::packages::discover_from_settings(cwd)
            .packages
            .into_iter()
            .filter(|package| package.version.is_some() && is_registry_package_path(&package.root))
            .map(|package| (package.name, package.version.unwrap_or_default()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let package_checks = package_names.into_iter().map(|(name, _)| {
        let client = client.clone();
        async move { (name.clone(), fetch_npm_latest(&client, &name).await) }
    });
    let mut packages = BTreeMap::new();
    for (name, latest) in join_all(package_checks).await {
        if let Some(latest) = latest {
            packages.insert(name, latest);
        }
    }
    let native_checks = crate::install::installed_native_packages()
        .into_iter()
        .filter(|package| package.source.is_none())
        .map(|package| {
            let client = client.clone();
            let name = package.name;
            async move {
                let latest = fetch_crates_latest(&client, &name).await;
                (name, latest)
            }
        });
    let mut native_packages = BTreeMap::new();
    for (name, latest) in join_all(native_checks).await {
        if let Some(latest) = latest {
            native_packages.insert(name, latest);
        }
    }
    let checked_at = now_ms();
    let (packages, packages_checked_at) = if enable_pi_packages {
        (packages, checked_at)
    } else {
        (
            previous_cache
                .as_ref()
                .map(|cache| cache.packages.clone())
                .unwrap_or_default(),
            previous_cache
                .as_ref()
                .map(|cache| cache.packages_checked_at)
                .unwrap_or_default(),
        )
    };
    let cache = UpdateCache {
        checked_at,
        rpi_latest,
        packages,
        packages_checked_at,
        native_packages,
    };
    if cache.rpi_latest.is_some() || !cache.packages.is_empty() || !cache.native_packages.is_empty()
    {
        let _ = write_cache(&cache_path, &cache);
    }
    report_from_cache(&cache, cwd, enable_pi_packages)
}

fn cache_is_fresh(cache: &UpdateCache, enable_pi_packages: bool) -> bool {
    let now = now_ms();
    now.saturating_sub(cache.checked_at) < CACHE_TTL_MS
        && (!enable_pi_packages || now.saturating_sub(cache.packages_checked_at) < CACHE_TTL_MS)
}

fn report_from_cache(cache: &UpdateCache, cwd: &Path, enable_pi_packages: bool) -> UpdateReport {
    let mut notices = Vec::new();
    if let Some(latest) = cache.rpi_latest.as_deref() {
        if is_newer(crate::VERSION, latest) {
            notices.push(UpdateNotice {
                name: "rpi".into(),
                current: crate::VERSION.into(),
                latest: latest.into(),
                command: "rpi update".into(),
            });
        }
    }
    if enable_pi_packages {
        let resources = crate::packages::discover_from_settings(cwd);
        for package in resources.packages {
            let Some(current) = package.version.as_deref() else {
                continue;
            };
            let Some(latest) = cache.packages.get(&package.name) else {
                continue;
            };
            if is_newer(current, latest) {
                notices.push(UpdateNotice {
                    name: package.name,
                    current: current.into(),
                    latest: latest.into(),
                    command: "rpi package update".into(),
                });
            }
        }
    }
    for package in crate::install::installed_native_packages() {
        if package.source.is_some() {
            continue;
        }
        let Some(latest) = cache.native_packages.get(&package.name) else {
            continue;
        };
        if is_newer(&package.version, latest) {
            notices.push(UpdateNotice {
                name: package.name,
                current: package.version,
                latest: latest.clone(),
                command: "rpi package update".into(),
            });
        }
    }
    UpdateReport { notices }
}

async fn fetch_rpi_latest(client: &reqwest::Client) -> Option<String> {
    client
        .get(CRATES_IO_API)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<CratesResponse>()
        .await
        .ok()
        .map(|response| response.crate_info.max_version)
}

async fn fetch_npm_latest(client: &reqwest::Client, name: &str) -> Option<String> {
    let encoded = name.replace('/', "%2F");
    client
        .get(format!("https://registry.npmjs.org/{encoded}"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<NpmResponse>()
        .await
        .ok()?
        .dist_tags?
        .latest
}

async fn fetch_crates_latest(client: &reqwest::Client, name: &str) -> Option<String> {
    client
        .get(format!("https://crates.io/api/v1/crates/{name}"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<CratesResponse>()
        .await
        .ok()
        .map(|response| response.crate_info.max_version)
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

/// Compare the numeric semver components. Pre-release/build metadata are
/// intentionally ignored because update notifications should be conservative.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |value: &str| -> Option<[u64; 3]> {
        let value = value.trim().trim_start_matches('v');
        let mut parts = value.split(['.', '-', '+']);
        Some([
            parts.next()?.parse().ok()?,
            parts.next().unwrap_or("0").parse().ok()?,
            parts.next().unwrap_or("0").parse().ok()?,
        ])
    };
    match (parse(current), parse(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &UpdateCache) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let data = serde_json::to_vec_pretty(cache).map_err(|error| error.to_string())?;
    std::fs::write(path, data).map_err(|error| error.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn print_startup_notices(report: &UpdateReport) {
    for notice in &report.notices {
        eprintln!(
            "Update available: {} {} -> {}. Run `{}`.",
            notice.name, notice.current, notice.latest, notice.command
        );
    }
}

/// Update the installed rpi CLI through its documented crates.io install
/// path. Cargo remains responsible for selecting the target and replacing the
/// executable atomically.
pub fn run_self_update(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: rpi update\n\nUpdate the rpi CLI from crates.io.");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("error: `rpi update` does not accept arguments");
        return 2;
    }
    let status = std::process::Command::new("cargo")
        .args(["install", "rpi-cli", "--locked", "--force"])
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("rpi updated successfully");
            0
        }
        Ok(status) => {
            eprintln!("error: cargo install exited with {status}");
            1
        }
        Err(error) => {
            eprintln!("error: could not run cargo (install Rust/Cargo first): {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{cache_is_fresh, is_newer, report_from_cache, UpdateCache};

    #[test]
    fn compares_release_versions_conservatively() {
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("v1.2.3", "1.3.0"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("nightly", "1.0.0"));
    }

    #[test]
    fn disabled_package_gate_skips_cached_package_notices() {
        let mut packages = BTreeMap::new();
        packages.insert("@scope/example".to_string(), "9.9.9".to_string());
        let cache = UpdateCache {
            checked_at: 0,
            rpi_latest: None,
            packages,
            packages_checked_at: 0,
            native_packages: BTreeMap::new(),
        };

        let report = report_from_cache(&cache, Path::new("."), false);
        assert!(report.notices.is_empty());
    }

    #[test]
    fn opt_in_cache_requires_a_package_check_timestamp() {
        let cache = UpdateCache {
            checked_at: super::now_ms(),
            rpi_latest: None,
            packages: BTreeMap::new(),
            packages_checked_at: 0,
            native_packages: BTreeMap::new(),
        };

        assert!(cache_is_fresh(&cache, false));
        assert!(!cache_is_fresh(&cache, true));
    }
}
