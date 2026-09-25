//! Lightweight update discovery and update commands.
//!
//! Startup checks are deliberately best-effort: callers decide when to run
//! them, each registry request uses a short timeout, and cached values are
//! retained only as a fallback for temporary registry failures.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};

const CACHE_FILE: &str = "update-check.json";
const CACHE_FALLBACK_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
const REQUEST_TIMEOUT_MS: u64 = 1800;
const GIT_CHECK_TIMEOUT_MS: u64 = 5000;
const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024;
const UPDATE_CHECK_CONCURRENCY: usize = 4;
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates/rpi-cli";
const STAGED_RPI_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_STAGED_RPI_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_SELF_UPDATE_STATUS_BYTES: u64 = 64 * 1024;
const MAX_SELF_UPDATE_STATUS_MESSAGE_CHARS: usize = 2 * 1024;
static GIT_COMMAND_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
static UPDATE_CACHE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SELF_UPDATE_STATUS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateWarning {
    pub message: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub notices: Vec<UpdateNotice>,
    pub warnings: Vec<UpdateWarning>,
}

impl UpdateReport {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.warnings.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCache {
    /// Legacy aggregate timestamp retained for backward-compatible cache reads.
    checked_at: i64,
    rpi_latest: Option<String>,
    packages: BTreeMap<String, String>,
    #[serde(default)]
    native_packages: BTreeMap<String, String>,
    #[serde(default)]
    git_packages: BTreeMap<String, GitUpdateCache>,
    #[serde(default)]
    freshness: UpdateCacheFreshness,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCacheFreshness {
    rpi: i64,
    packages: BTreeMap<String, i64>,
    native_packages: BTreeMap<String, i64>,
    git_packages: BTreeMap<String, i64>,
}

type RegistryCacheUpdate = (BTreeMap<String, String>, BTreeMap<String, i64>);
type GitCacheUpdate = (BTreeMap<String, GitUpdateCache>, BTreeMap<String, i64>);
type PackageCacheUpdate = (RegistryCacheUpdate, RegistryCacheUpdate, GitCacheUpdate);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GitUpdateCache {
    current: String,
    latest: String,
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
struct SelfUpdateStatus {
    state: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryLookup {
    Found(String),
    Missing,
    TransientFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitLookup {
    Update(GitUpdateCache),
    Current,
    /// The checkout or its configured origin no longer matches the package.
    Invalid,
    /// A cached result is reusable only when the checkout still has this HEAD.
    TransientFailure(Option<String>),
}

enum PackageCheck {
    Npm(String),
    Git { source: String, root: PathBuf },
    Native(String),
}

enum PackageCheckResult {
    Npm(String, RegistryLookup),
    Git(String, GitLookup),
    Native(String, RegistryLookup),
}

#[derive(Debug, Clone, Copy)]
struct StartupCheckScope {
    rpi: bool,
    packages: bool,
}

impl StartupCheckScope {
    const ALL: Self = Self {
        rpi: true,
        packages: true,
    };
    const RPI: Self = Self {
        rpi: true,
        packages: false,
    };
    const PACKAGES: Self = Self {
        rpi: false,
        packages: true,
    };
}

/// Perform a best-effort check with Pi package discovery disabled.
///
/// Keep this compatibility entry point conservative: callers that have not
/// explicitly opted into Pi packages must never parse package settings.
pub async fn check_startup(cwd: &Path) -> UpdateReport {
    let _ = cwd;
    check_startup_with_package_resources(None).await
}

/// Perform a best-effort startup check, using cached values only when a fresh
/// request fails.
///
/// `package_resources` must be the same trust-gated resource set that the
/// session will load. `None` disables Pi package checks entirely.
pub async fn check_startup_with_package_resources(
    package_resources: Option<&crate::packages::PackageResources>,
) -> UpdateReport {
    let npm_command = crate::npm::NpmCommand::from_argv(None)
        .expect("the built-in npm command must always be valid");
    check_startup_with_package_resources_and_npm_command(package_resources, &npm_command).await
}

/// Perform startup discovery using the effective trusted `npmCommand` argv.
/// Keeping the command explicit prevents a project wrapper from being used
/// before its trust decision has been applied.
pub async fn check_startup_with_package_resources_and_npm_command(
    package_resources: Option<&crate::packages::PackageResources>,
    npm_command: &crate::npm::NpmCommand,
) -> UpdateReport {
    check_startup_with_package_resources_and_npm_command_in_cwd(
        package_resources,
        npm_command,
        None,
    )
    .await
}

/// Perform startup discovery with an explicitly trust-gated npm working
/// directory. `None` runs every npm lookup in a fresh isolated directory;
/// callers may supply the project cwd only after that project is trusted.
pub async fn check_startup_with_package_resources_and_npm_command_in_cwd(
    package_resources: Option<&crate::packages::PackageResources>,
    npm_command: &crate::npm::NpmCommand,
    trusted_project_cwd: Option<&Path>,
) -> UpdateReport {
    if update_checks_disabled() {
        return report_without_remote_checks(StartupCheckScope::ALL);
    }
    let client = startup_http_client();
    check_startup_with_client(package_resources, npm_command, trusted_project_cwd, client).await
}

/// Check only the rpi release. Callers can render this report independently so
/// slow package-manager subprocesses never delay the self-update notice.
pub async fn check_rpi_startup() -> UpdateReport {
    if update_checks_disabled() {
        return report_without_remote_checks(StartupCheckScope::RPI);
    }
    let npm_command = crate::npm::NpmCommand::from_argv(None)
        .expect("the built-in npm command must always be valid");
    check_startup_with_client_scope(
        None,
        &npm_command,
        None,
        startup_http_client(),
        StartupCheckScope::RPI,
    )
    .await
}

/// Check only npm, Git, and native Rust packages. This is the package half of
/// [`check_rpi_startup`] and is safe to run concurrently with it.
pub async fn check_package_startup_with_resources_and_npm_command_in_cwd(
    package_resources: Option<&crate::packages::PackageResources>,
    npm_command: &crate::npm::NpmCommand,
    trusted_project_cwd: Option<&Path>,
) -> UpdateReport {
    if update_checks_disabled() {
        return report_without_remote_checks(StartupCheckScope::PACKAGES);
    }
    check_startup_with_client_scope(
        package_resources,
        npm_command,
        trusted_project_cwd,
        startup_http_client(),
        StartupCheckScope::PACKAGES,
    )
    .await
}

/// Package-only compatibility wrapper using the built-in npm executable and
/// an isolated working directory.
pub async fn check_package_startup_with_resources(
    package_resources: Option<&crate::packages::PackageResources>,
) -> UpdateReport {
    let npm_command = crate::npm::NpmCommand::from_argv(None)
        .expect("the built-in npm command must always be valid");
    check_package_startup_with_resources_and_npm_command_in_cwd(
        package_resources,
        &npm_command,
        None,
    )
    .await
}

fn update_checks_disabled() -> bool {
    crate::args::offline_env_enabled() || std::env::var_os("RPI_DISABLE_UPDATE_CHECK").is_some()
}

fn report_without_remote_checks(scope: StartupCheckScope) -> UpdateReport {
    let warnings = if scope.rpi {
        crate::config::agent_dir()
            .ok()
            .map(|agent_dir| consume_self_update_statuses(&agent_dir))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    UpdateReport {
        notices: Vec::new(),
        warnings,
    }
}

fn startup_http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .ok()
}

async fn check_startup_with_client(
    package_resources: Option<&crate::packages::PackageResources>,
    npm_command: &crate::npm::NpmCommand,
    trusted_project_cwd: Option<&Path>,
    client: Option<reqwest::Client>,
) -> UpdateReport {
    check_startup_with_client_scope(
        package_resources,
        npm_command,
        trusted_project_cwd,
        client,
        StartupCheckScope::ALL,
    )
    .await
}

async fn check_startup_with_client_scope(
    package_resources: Option<&crate::packages::PackageResources>,
    npm_command: &crate::npm::NpmCommand,
    trusted_project_cwd: Option<&Path>,
    client: Option<reqwest::Client>,
    scope: StartupCheckScope,
) -> UpdateReport {
    let agent_dir = match crate::config::agent_dir() {
        Ok(dir) => dir,
        Err(_) => return UpdateReport::default(),
    };
    let warnings = if scope.rpi {
        consume_self_update_statuses(&agent_dir)
    } else {
        Vec::new()
    };
    let cache_path = agent_dir.join(CACHE_FILE);
    let previous_cache = read_cache(&cache_path);

    let package_specs = if scope.packages {
        package_resources
            .into_iter()
            .flat_map(|resources| resources.packages.iter())
            .filter(|package| package.version.is_some())
            .filter_map(|package| {
                package
                    .updateable_npm_source()
                    .map(|(_, spec)| spec.to_owned())
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut checks = package_specs
        .into_iter()
        .map(PackageCheck::Npm)
        .collect::<Vec<_>>();
    if scope.packages {
        checks.extend(
            package_resources
                .into_iter()
                .flat_map(|resources| resources.packages.iter())
                .filter_map(git_update_target)
                .map(|(source, root)| PackageCheck::Git { source, root }),
        );
        checks.extend(
            crate::install::installed_native_packages()
                .into_iter()
                .filter(|package| package.source.is_none())
                .map(|package| PackageCheck::Native(package.name)),
        );
    }
    let npm_process_cwd = crate::npm::NpmProcessCwd::startup(trusted_project_cwd);
    let package_checks = checks.into_iter().map(|check| {
        let npm_command = npm_command.clone();
        let npm_process_cwd = npm_process_cwd.clone();
        let client = client.clone();
        async move {
            match check {
                PackageCheck::Npm(spec) => {
                    let latest = fetch_npm_latest(&spec, &npm_command, npm_process_cwd).await;
                    PackageCheckResult::Npm(spec, latest)
                }
                PackageCheck::Git { source, root } => {
                    let update = fetch_git_update(&root, &source).await;
                    PackageCheckResult::Git(source, update)
                }
                PackageCheck::Native(name) => {
                    let latest = match client.as_ref() {
                        Some(client) => fetch_crates_latest(client, &name).await,
                        None => RegistryLookup::TransientFailure,
                    };
                    PackageCheckResult::Native(name, latest)
                }
            }
        }
    });
    let rpi_check = async {
        if !scope.rpi {
            return None;
        }
        Some(match client.as_ref() {
            Some(client) => fetch_rpi_latest(client).await,
            None => RegistryLookup::TransientFailure,
        })
    };
    let (rpi_result, check_results) = tokio::join!(rpi_check, collect_bounded(package_checks));
    let mut package_results = Vec::new();
    let mut git_results = Vec::new();
    let mut native_results = Vec::new();
    for result in check_results {
        match result {
            PackageCheckResult::Npm(spec, latest) => package_results.push((spec, latest)),
            PackageCheckResult::Git(source, update) => git_results.push((source, update)),
            PackageCheckResult::Native(name, latest) => native_results.push((name, latest)),
        }
    }
    let checked_at = now_ms();
    let mut rpi_update = None;
    if let Some(rpi_result) = rpi_result {
        let cached_at = previous_cache
            .as_ref()
            .map(rpi_cache_checked_at)
            .unwrap_or_default();
        let fallback_allowed = timestamp_is_fresh(cached_at, checked_at);
        let (latest, used_fallback) = lookup_with_cache_fallback(
            rpi_result,
            previous_cache
                .as_ref()
                .and_then(|cache| cache.rpi_latest.as_deref()),
            fallback_allowed,
        );
        let value_checked_at =
            resolved_checked_at(latest.as_deref(), used_fallback, cached_at, checked_at);
        rpi_update = Some((latest, value_checked_at));
    }

    let mut package_update = None;
    if scope.packages {
        let packages = results_with_individual_cache_fallback(
            package_results,
            previous_cache.as_ref().map(|cache| &cache.packages),
            previous_cache
                .as_ref()
                .map(|cache| &cache.freshness.packages),
            previous_cache
                .as_ref()
                .map(|cache| cache.checked_at)
                .unwrap_or_default(),
            checked_at,
        );
        let native_packages = results_with_individual_cache_fallback(
            native_results,
            previous_cache.as_ref().map(|cache| &cache.native_packages),
            previous_cache
                .as_ref()
                .map(|cache| &cache.freshness.native_packages),
            previous_cache
                .as_ref()
                .map(|cache| cache.checked_at)
                .unwrap_or_default(),
            checked_at,
        );
        let git_packages = if package_resources.is_some() {
            git_results_with_individual_cache_fallback(
                git_results,
                previous_cache.as_ref().map(|cache| &cache.git_packages),
                previous_cache
                    .as_ref()
                    .map(|cache| &cache.freshness.git_packages),
                previous_cache
                    .as_ref()
                    .map(|cache| cache.checked_at)
                    .unwrap_or_default(),
                checked_at,
            )
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        package_update = Some((packages, native_packages, git_packages));
    }

    let cache = merge_and_write_cache(&cache_path, checked_at, rpi_update, package_update);
    let mut report = report_from_cache_scope(&cache, package_resources, scope);
    report.warnings = warnings;
    report
}

async fn collect_bounded<I, F, T>(checks: I) -> Vec<T>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = T>,
{
    stream::iter(checks)
        .buffer_unordered(UPDATE_CHECK_CONCURRENCY)
        .collect()
        .await
}

fn rpi_cache_checked_at(cache: &UpdateCache) -> i64 {
    if cache.freshness.rpi > 0 {
        cache.freshness.rpi
    } else {
        cache.checked_at
    }
}

fn entry_cache_checked_at(
    freshness: Option<&BTreeMap<String, i64>>,
    key: &str,
    legacy_checked_at: i64,
) -> i64 {
    freshness
        .and_then(|values| values.get(key).copied())
        .filter(|checked_at| *checked_at > 0)
        .unwrap_or(legacy_checked_at)
}

fn timestamp_is_fresh(checked_at: i64, now: i64) -> bool {
    let age = now.saturating_sub(checked_at);
    checked_at > 0 && age >= 0 && age <= CACHE_FALLBACK_MAX_AGE_MS
}

fn resolved_checked_at(
    value: Option<&str>,
    used_fallback: bool,
    cached_at: i64,
    checked_at: i64,
) -> i64 {
    if value.is_none() {
        0
    } else if used_fallback {
        cached_at
    } else {
        checked_at
    }
}

fn results_with_individual_cache_fallback(
    results: Vec<(String, RegistryLookup)>,
    cached: Option<&BTreeMap<String, String>>,
    freshness: Option<&BTreeMap<String, i64>>,
    legacy_checked_at: i64,
    checked_at: i64,
) -> RegistryCacheUpdate {
    let mut values = BTreeMap::new();
    let mut checked_at_values = BTreeMap::new();
    for (name, result) in results {
        let cached_value = cached.and_then(|cached| cached.get(&name).map(String::as_str));
        let cached_at = entry_cache_checked_at(freshness, &name, legacy_checked_at);
        let (latest, used_fallback) = lookup_with_cache_fallback(
            result,
            cached_value,
            timestamp_is_fresh(cached_at, checked_at),
        );
        if let Some(latest) = latest {
            checked_at_values.insert(
                name.clone(),
                resolved_checked_at(Some(&latest), used_fallback, cached_at, checked_at),
            );
            values.insert(name, latest);
        }
    }
    (values, checked_at_values)
}

fn git_results_with_individual_cache_fallback(
    results: Vec<(String, GitLookup)>,
    cached: Option<&BTreeMap<String, GitUpdateCache>>,
    freshness: Option<&BTreeMap<String, i64>>,
    legacy_checked_at: i64,
    checked_at: i64,
) -> GitCacheUpdate {
    let mut values = BTreeMap::new();
    let mut checked_at_values = BTreeMap::new();
    for (source, result) in results {
        let cached_at = entry_cache_checked_at(freshness, &source, legacy_checked_at);
        match result {
            GitLookup::Update(update) => {
                checked_at_values.insert(source.clone(), checked_at);
                values.insert(source, update);
            }
            GitLookup::Current | GitLookup::Invalid => {}
            GitLookup::TransientFailure(Some(current))
                if timestamp_is_fresh(cached_at, checked_at) =>
            {
                if let Some(previous) = cached
                    .and_then(|cached| cached.get(&source))
                    .filter(|previous| previous.current == current)
                {
                    checked_at_values.insert(source.clone(), cached_at);
                    values.insert(source, previous.clone());
                }
            }
            GitLookup::TransientFailure(_) => {}
        }
    }
    (values, checked_at_values)
}

fn merge_and_write_cache(
    path: &Path,
    checked_at: i64,
    rpi_update: Option<(Option<String>, i64)>,
    package_update: Option<PackageCacheUpdate>,
) -> UpdateCache {
    let _guard = UPDATE_CACHE_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cache = read_cache(path).unwrap_or_default();
    if cache.checked_at <= 0 {
        cache.checked_at = checked_at;
    }
    if let Some((latest, latest_checked_at)) = rpi_update {
        cache.rpi_latest = latest;
        cache.freshness.rpi = latest_checked_at;
    }
    if let Some((packages, native_packages, git_packages)) = package_update {
        cache.packages = packages.0;
        cache.freshness.packages = packages.1;
        cache.native_packages = native_packages.0;
        cache.freshness.native_packages = native_packages.1;
        cache.git_packages = git_packages.0;
        cache.freshness.git_packages = git_packages.1;
    }
    let _ = write_cache(path, &cache);
    cache
}

#[cfg(test)]
fn git_results_with_cache_fallback(
    results: Vec<(String, GitLookup)>,
    cached: Option<&BTreeMap<String, GitUpdateCache>>,
    fallback_allowed: bool,
) -> (BTreeMap<String, GitUpdateCache>, bool) {
    let mut values = BTreeMap::new();
    let mut used_fallback = false;
    for (source, result) in results {
        match result {
            GitLookup::Update(update) => {
                values.insert(source, update);
            }
            GitLookup::Current | GitLookup::Invalid => {}
            GitLookup::TransientFailure(Some(current)) if fallback_allowed => {
                if let Some(previous) = cached
                    .and_then(|cached| cached.get(&source))
                    .filter(|previous| previous.current == current)
                {
                    values.insert(source, previous.clone());
                    used_fallback = true;
                }
            }
            GitLookup::TransientFailure(_) => {}
        }
    }
    (values, used_fallback)
}

#[cfg(test)]
fn results_with_cache_fallback(
    results: Vec<(String, RegistryLookup)>,
    cached: Option<&BTreeMap<String, String>>,
    fallback_allowed: bool,
) -> (BTreeMap<String, String>, bool) {
    let mut values = BTreeMap::new();
    let mut used_fallback = false;
    for (name, result) in results {
        let cached_value = cached.and_then(|cached| cached.get(&name).map(String::as_str));
        let (latest, fallback) = lookup_with_cache_fallback(result, cached_value, fallback_allowed);
        if let Some(latest) = latest {
            values.insert(name, latest);
        }
        used_fallback |= fallback;
    }
    (values, used_fallback)
}

fn lookup_with_cache_fallback(
    result: RegistryLookup,
    cached: Option<&str>,
    fallback_allowed: bool,
) -> (Option<String>, bool) {
    match result {
        RegistryLookup::Found(version) => (Some(version), false),
        RegistryLookup::Missing => (None, false),
        RegistryLookup::TransientFailure if fallback_allowed => {
            (cached.map(str::to_owned), cached.is_some())
        }
        RegistryLookup::TransientFailure => (None, false),
    }
}

#[cfg(test)]
fn cache_fallback_is_fresh(cache: &UpdateCache, now: i64) -> bool {
    timestamp_is_fresh(cache.checked_at, now)
}

#[cfg(test)]
fn report_from_cache(
    cache: &UpdateCache,
    package_resources: Option<&crate::packages::PackageResources>,
) -> UpdateReport {
    report_from_cache_scope(cache, package_resources, StartupCheckScope::ALL)
}

fn report_from_cache_scope(
    cache: &UpdateCache,
    package_resources: Option<&crate::packages::PackageResources>,
    scope: StartupCheckScope,
) -> UpdateReport {
    let mut notices = Vec::new();
    if scope.rpi {
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
    }
    if scope.packages {
        if let Some(resources) = package_resources {
            for package in &resources.packages {
                let Some((_, source_spec)) = package.updateable_npm_source() else {
                    continue;
                };
                let Some(current) = package.version.as_deref() else {
                    continue;
                };
                let Some(latest) = cache.packages.get(source_spec) else {
                    continue;
                };
                if is_newer(current, latest) {
                    notices.push(UpdateNotice {
                        name: package.name.clone(),
                        current: current.into(),
                        latest: latest.into(),
                        command: "rpi pi-package update".into(),
                    });
                }
            }
            for package in &resources.packages {
                let Some((source, _)) = git_update_target(package) else {
                    continue;
                };
                let Some(update) = cache.git_packages.get(&source) else {
                    continue;
                };
                let (Some(current), Some(latest)) = (
                    normalized_git_oid(&update.current),
                    normalized_git_oid(&update.latest),
                ) else {
                    continue;
                };
                if current == latest {
                    continue;
                }
                let Some(git) = crate::packages::parse_git_source(&source) else {
                    continue;
                };
                notices.push(UpdateNotice {
                    name: format!("{}/{}", git.host, git.path),
                    current: short_git_oid(&current),
                    latest: short_git_oid(&latest),
                    command: "rpi pi-package update".into(),
                });
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
    }
    UpdateReport {
        notices,
        warnings: Vec::new(),
    }
}

async fn fetch_rpi_latest(client: &reqwest::Client) -> RegistryLookup {
    fetch_crates_url(client, CRATES_IO_API).await
}

async fn fetch_npm_latest(
    source_spec: &str,
    npm_command: &crate::npm::NpmCommand,
    cwd: crate::npm::NpmProcessCwd,
) -> RegistryLookup {
    let Some(lookup_spec) = npm_registry_lookup_spec(source_spec) else {
        return RegistryLookup::Missing;
    };
    let args = match npm_command.view_args(&lookup_spec) {
        Ok(args) => args,
        Err(_) => return RegistryLookup::Missing,
    };
    let output = match npm_command.run_bounded(&args, cwd).await {
        Ok(output) => output,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return if stderr.contains("E404") || stderr.contains("404 Not Found") {
            RegistryLookup::Missing
        } else {
            RegistryLookup::TransientFailure
        };
    }
    parse_npm_view_version(&output.stdout)
        .map(RegistryLookup::Found)
        .unwrap_or(RegistryLookup::Missing)
}

fn npm_registry_lookup_spec(source_spec: &str) -> Option<String> {
    let parsed = crate::packages::parse_npm_package_spec(source_spec)?;
    if parsed.is_alias {
        parsed.requested
    } else {
        Some(source_spec.to_string())
    }
}

/// Select only explicit, unpinned Git sources whose package discovery already
/// established managed-store provenance. The filesystem shape is checked again
/// here so a checkout replaced after discovery cannot redirect a startup
/// subprocess through a symlink, junction, or worktree `.git` pointer.
fn git_update_target(package: &crate::packages::PackageRoot) -> Option<(String, PathBuf)> {
    if package.source != crate::packages::PackageSource::Git {
        return None;
    }
    let source = crate::packages::parse_git_source(&package.spec)?;
    if source.revision.is_some() || !is_validated_git_checkout(&package.root) {
        return None;
    }
    Some((package.spec.clone(), package.root.clone()))
}

fn is_validated_git_checkout(root: &Path) -> bool {
    if !root.is_absolute() {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(canonical_root) = std::fs::canonicalize(root).map(normalize_git_path) else {
        return false;
    };
    if !git_paths_equal(&canonical_root, root) {
        return false;
    }

    let git_dir = root.join(".git");
    let Ok(git_metadata) = std::fs::symlink_metadata(&git_dir) else {
        return false;
    };
    if !git_metadata.is_dir() || git_metadata.file_type().is_symlink() {
        return false;
    }
    std::fs::canonicalize(&git_dir)
        .map(normalize_git_path)
        .is_ok_and(|canonical| git_paths_equal(&canonical, &git_dir))
}

async fn fetch_git_update(root: &Path, source: &str) -> GitLookup {
    if !is_validated_git_checkout(root) {
        return GitLookup::Invalid;
    }
    let Some(current) = run_git_capture(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .and_then(|output| parse_git_oid_output(&output))
    else {
        return GitLookup::TransientFailure(None);
    };
    let Some(origin) = read_matching_git_origin(root, source).await else {
        return GitLookup::Invalid;
    };
    fetch_git_update_from_origin(root, &origin, current).await
}

async fn fetch_git_update_from_origin(root: &Path, origin: &str, current: String) -> GitLookup {
    let Some(latest) = fetch_git_remote_head(root, origin).await else {
        return GitLookup::TransientFailure(Some(current));
    };
    if current == latest {
        GitLookup::Current
    } else {
        GitLookup::Update(GitUpdateCache { current, latest })
    }
}

async fn read_matching_git_origin(root: &Path, source: &str) -> Option<String> {
    let output = run_git_capture(
        root,
        &[
            "config",
            "--file",
            ".git/config",
            "--get-all",
            "remote.origin.url",
        ],
    )
    .await?;
    parse_matching_git_origin(&output, source)
}

fn parse_matching_git_origin(output: &[u8], source: &str) -> Option<String> {
    let expected = crate::packages::parse_git_source(source)?;
    if expected.revision.is_some() {
        return None;
    }
    let output = std::str::from_utf8(output).ok()?;
    let mut origins = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let origin = origins.next()?;
    if origins.next().is_some() {
        return None;
    }
    let actual = crate::packages::parse_git_source(origin)?;
    (actual.revision.is_none() && actual.host == expected.host && actual.path == expected.path)
        .then(|| origin.to_string())
}

async fn fetch_git_remote_head(root: &Path, origin: &str) -> Option<String> {
    if let Some(branch) = run_git_capture(root, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .await
        .and_then(|output| parse_origin_upstream(&output))
    {
        let upstream_ref = format!("refs/heads/{branch}");
        if let Some(head) = run_git_remote_capture(origin, &upstream_ref)
            .await
            .and_then(|output| parse_ls_remote_oid(&output, &upstream_ref))
        {
            return Some(head);
        }
    }

    run_git_remote_capture(origin, "HEAD")
        .await
        .and_then(|output| parse_ls_remote_oid(&output, "HEAD"))
}

async fn run_git_capture(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    // Repeat the path check immediately before every spawn to narrow the race
    // between package discovery and the asynchronous startup task.
    if !is_validated_git_checkout(root) {
        return None;
    }
    let mut command_args = hardened_git_config_args();
    command_args.extend(args.iter().map(|arg| (*arg).to_string()));
    let output = crate::npm::run_bounded_command(
        "git",
        &command_args,
        root,
        &hardened_git_environment(),
        std::time::Duration::from_millis(GIT_CHECK_TIMEOUT_MS),
        MAX_GIT_OUTPUT_BYTES,
    )
    .await
    .ok()?;
    output.status.success().then_some(output.stdout)
}

async fn run_git_remote_capture(origin: &str, reference: &str) -> Option<Vec<u8>> {
    let command_dir = create_isolated_git_command_dir()?;
    let mut command_args = hardened_git_config_args();
    command_args.extend(
        ["ls-remote", "--exit-code", "--", origin, reference]
            .into_iter()
            .map(str::to_string),
    );
    let mut environment = hardened_git_environment();
    // Prevent repository discovery above the empty command directory, so
    // package-local Git config cannot select helpers, rewrites, or hooks.
    environment.push((
        std::ffi::OsString::from("GIT_CEILING_DIRECTORIES"),
        Some(command_dir.as_os_str().to_owned()),
    ));
    let output = crate::npm::run_bounded_command(
        "git",
        &command_args,
        &command_dir,
        &environment,
        std::time::Duration::from_millis(GIT_CHECK_TIMEOUT_MS),
        MAX_GIT_OUTPUT_BYTES,
    )
    .await
    .ok();
    let _ = std::fs::remove_dir(&command_dir);
    output
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn hardened_git_config_args() -> Vec<String> {
    crate::install_pi::hardened_git_network_config_args()
}

fn hardened_git_environment() -> Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> {
    let mut environment = crate::install_pi::HARDENED_GIT_ENV_REMOVE
        .iter()
        .map(|name| (std::ffi::OsString::from(name), None))
        .collect::<Vec<_>>();
    for (name, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        (
            "GIT_CONFIG_SYSTEM",
            crate::install_pi::hardened_git_null_config(),
        ),
        (
            "GIT_CONFIG_GLOBAL",
            crate::install_pi::hardened_git_null_config(),
        ),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_PROTOCOL_FROM_USER", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "Never"),
        ("SSH_ASKPASS_REQUIRE", "never"),
    ] {
        environment.push((
            std::ffi::OsString::from(name),
            Some(std::ffi::OsString::from(value)),
        ));
    }
    environment
}

fn create_isolated_git_command_dir() -> Option<PathBuf> {
    let temp = std::env::temp_dir();
    for _ in 0..8 {
        let sequence = GIT_COMMAND_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = temp.join(format!("rpi-git-check-{}-{sequence}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => return Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn parse_origin_upstream(output: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(output).ok()?.trim();
    let branch = value.strip_prefix("origin/")?;
    is_safe_git_branch(branch).then(|| branch.to_string())
}

fn is_safe_git_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '-' | '/' | '.'))
        && !branch
            .chars()
            .next_back()
            .is_some_and(|ch| matches!(ch, '/' | '.'))
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.contains("//")
        && !branch.contains('\\')
        && !branch.chars().any(|ch| {
            ch.is_control() || ch.is_whitespace() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[')
        })
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with('.'))
}

fn parse_git_oid_output(output: &[u8]) -> Option<String> {
    normalized_git_oid(std::str::from_utf8(output).ok()?)
}

fn parse_ls_remote_oid(output: &[u8], expected_ref: &str) -> Option<String> {
    let output = std::str::from_utf8(output).ok()?;
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let oid = fields.next()?;
        let reference = fields.next()?;
        if fields.next().is_some() || reference != expected_ref {
            return None;
        }
        normalized_git_oid(oid)
    })
}

fn normalized_git_oid(value: &str) -> Option<String> {
    let value = value.trim();
    ((value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn short_git_oid(value: &str) -> String {
    value.chars().take(12).collect()
}

fn normalize_git_path(path: PathBuf) -> PathBuf {
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

fn git_paths_equal(left: &Path, right: &Path) -> bool {
    let left = normalize_git_path(left.to_path_buf());
    let right = normalize_git_path(right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    } else {
        left == right
    }
}

fn parse_npm_view_version(output: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(output).ok()? {
        serde_json::Value::String(version) => parse_npm_semver(version).map(|(_, raw)| raw),
        serde_json::Value::Array(versions) => versions
            .into_iter()
            .filter_map(|version| match version {
                serde_json::Value::String(version) => parse_npm_semver(version),
                _ => None,
            })
            .max_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, raw)| raw),
        _ => None,
    }
}

fn parse_npm_semver(version: String) -> Option<(semver::Version, String)> {
    let parsed = semver::Version::parse(version.trim().trim_start_matches('v')).ok()?;
    Some((parsed, version))
}

async fn fetch_crates_latest(client: &reqwest::Client, name: &str) -> RegistryLookup {
    fetch_crates_url(client, &format!("https://crates.io/api/v1/crates/{name}")).await
}

async fn fetch_crates_url(client: &reqwest::Client, url: &str) -> RegistryLookup {
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(_) => return RegistryLookup::TransientFailure,
    };
    if matches!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return RegistryLookup::Missing;
    }
    if !response.status().is_success() {
        return RegistryLookup::TransientFailure;
    }
    match response.json::<CratesResponse>().await {
        Ok(response) => RegistryLookup::Found(response.crate_info.max_version),
        Err(_) => RegistryLookup::TransientFailure,
    }
}

/// Compare complete semantic versions, including prerelease precedence.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |value: &str| semver::Version::parse(value.trim().trim_start_matches('v')).ok();
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
    crate::config::atomic_write(path, &data).map_err(|error| error.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn print_startup_notices(report: &UpdateReport) {
    for warning in &report.warnings {
        eprintln!("warning: {} Run `{}`.", warning.message, warning.command);
    }
    for notice in &report.notices {
        eprintln!(
            "Update available: {} {} -> {}. Run `{}`.",
            notice.name, notice.current, notice.latest, notice.command
        );
    }
}

/// Update the installed rpi CLI through its documented crates.io install path.
/// Windows stages first because a running executable cannot replace itself;
/// other platforms retain Cargo's direct replacement behavior.
pub fn run_self_update(args: &[String]) -> i32 {
    let offline = crate::args::normalize_offline_mode(args);
    let args = crate::args::without_offline_flag(args);
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: rpi update [--offline]\n\nUpdate the rpi CLI from crates.io.");
        return 0;
    }
    if !args.is_empty() {
        eprintln!("error: `rpi update` does not accept arguments");
        return 2;
    }
    if offline {
        println!("rpi update skipped: offline mode is enabled");
        return 0;
    }

    #[cfg(windows)]
    {
        run_windows_self_update()
    }
    #[cfg(not(windows))]
    {
        run_direct_self_update()
    }
}

fn cargo_install_command(
    staging_root: Option<&Path>,
    isolated_cwd: &Path,
) -> Result<std::process::Command, String> {
    let isolated_cwd = validated_self_update_command_dir(isolated_cwd)?;
    let mut command = std::process::Command::new("cargo");
    command.arg("install").current_dir(isolated_cwd);
    for variable in [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(variable);
    }
    if let Some(staging_root) = staging_root {
        command.arg("--root").arg(staging_root);
    }
    command.args(["rpi-cli", "--locked", "--force"]);
    Ok(command)
}

/// Cargo discovers `.cargo/config.toml` from its working directory upward.
/// Put update commands directly below the user's home directory so neither an
/// untrusted project, a project-local `CARGO_HOME`, nor a world-writable system
/// temp directory can join that discovery chain. Cargo still reads its
/// explicitly configured user-level home through its normal environment.
fn create_self_update_command_dir() -> Result<tempfile::TempDir, String> {
    let configured_root =
        dirs::home_dir().ok_or_else(|| "could not resolve the user home directory".to_string())?;
    if !configured_root.is_absolute() {
        return Err("user home must be absolute for self-update".to_string());
    }
    let canonical_root = std::fs::canonicalize(&configured_root)
        .map(normalize_git_path)
        .map_err(|error| format!("could not canonicalize user home: {error}"))?;
    let metadata = std::fs::symlink_metadata(&canonical_root)
        .map_err(|error| format!("could not inspect user home: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("user home does not resolve to a real directory".to_string());
    }
    let current = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map(normalize_git_path)
        .map_err(|error| format!("could not validate the caller directory: {error}"))?;
    let project_root = current
        .ancestors()
        .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
        .map(Path::to_path_buf);
    validate_configured_cargo_home(project_root.as_deref())?;
    if project_root.as_deref().is_some_and(|root| {
        !git_paths_equal(&canonical_root, root) && path_is_within(&canonical_root, root)
    }) {
        return Err(
            "refusing to create a self-update command directory inside the caller project"
                .to_string(),
        );
    }
    let directory = tempfile::Builder::new()
        .prefix("rpi-self-update-command-")
        .tempdir_in(&canonical_root)
        .map_err(|error| format!("could not allocate Cargo command directory: {error}"))?;
    validated_self_update_command_dir(directory.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory_metadata = std::fs::symlink_metadata(directory.path())
            .map_err(|error| format!("could not inspect Cargo command directory: {error}"))?;
        if metadata.uid() != directory_metadata.uid() || metadata.permissions().mode() & 0o022 != 0
        {
            return Err("user home is not private to the current account".to_string());
        }
    }
    Ok(directory)
}

fn path_is_within(candidate: &Path, ancestor: &Path) -> bool {
    candidate
        .ancestors()
        .any(|component| git_paths_equal(component, ancestor))
}

fn validate_configured_cargo_home(project_root: Option<&Path>) -> Result<(), String> {
    let Some(configured) = std::env::var_os("CARGO_HOME") else {
        return Ok(());
    };
    let configured = PathBuf::from(configured);
    if !configured.is_absolute() {
        return Err("CARGO_HOME must be absolute for self-update".to_string());
    }
    let project_local = match project_root {
        Some(root) => cargo_home_is_project_local(&configured, root)?,
        None => {
            canonicalize_allow_missing(&configured, "CARGO_HOME")?;
            false
        }
    };
    if project_local {
        return Err("refusing project-local CARGO_HOME during self-update".to_string());
    }
    Ok(())
}

fn cargo_home_is_project_local(configured: &Path, project_root: &Path) -> Result<bool, String> {
    if !configured.is_absolute() {
        return Err("CARGO_HOME must be absolute for self-update".to_string());
    }
    let configured = canonicalize_allow_missing(configured, "CARGO_HOME")?;
    Ok(path_is_within(&configured, project_root))
}

fn canonicalize_allow_missing(path: &Path, label: &str) -> Result<PathBuf, String> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(format!("{label} contains relative path components"));
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| format!("could not resolve {label}"))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| format!("could not resolve {label}"))?;
            }
            Err(error) => return Err(format!("could not inspect {label}: {error}")),
        }
    }
    let mut canonical = std::fs::canonicalize(existing)
        .map(normalize_git_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn validated_self_update_command_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("self-update command directory must be absolute".to_string());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect self-update command directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("self-update command directory is not a real directory".to_string());
    }
    let canonical = std::fs::canonicalize(path)
        .map(normalize_git_path)
        .map_err(|error| {
            format!("could not canonicalize self-update command directory: {error}")
        })?;
    if !git_paths_equal(&canonical, path) {
        return Err(
            "self-update command directory resolves through a link or junction".to_string(),
        );
    }
    Ok(canonical)
}

#[cfg(not(windows))]
#[derive(Debug, Clone)]
struct DirectUpdatePlan {
    staging_dir: PathBuf,
    staged_exe: PathBuf,
    target_exe: PathBuf,
    replacement_temp: PathBuf,
    backup_file: PathBuf,
}

#[cfg(not(windows))]
fn run_direct_self_update() -> i32 {
    let staging = match create_self_update_staging_directory() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("error: could not create self-update staging directory: {error}");
            return 1;
        }
    };
    let command_dir = match create_self_update_command_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("error: could not create an isolated self-update directory: {error}");
            return 1;
        }
    };
    let mut command = match cargo_install_command(Some(staging.path()), command_dir.path()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: invalid isolated self-update directory: {error}");
            return 1;
        }
    };
    let status = command.status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("error: cargo install exited with {status}");
            return 1;
        }
        Err(error) => {
            eprintln!("error: could not run cargo (install Rust/Cargo first): {error}");
            return 1;
        }
    }

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: could not resolve the current rpi executable: {error}");
            return 1;
        }
    };
    let plan = match build_direct_update_plan(staging.path(), &current_exe) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    match activate_direct_update(&plan) {
        Ok(version) => {
            println!("rpi updated successfully to {version}");
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn create_self_update_staging_directory() -> Result<tempfile::TempDir, String> {
    let agent = crate::config::agent_dir().map_err(|error| error.to_string())?;
    if !agent.is_absolute() {
        return Err("agent directory must be absolute".to_string());
    }
    std::fs::create_dir_all(&agent)
        .map_err(|error| format!("could not create {}: {error}", agent.display()))?;
    let agent = validated_real_directory(&agent, "agent directory")?;
    let update_root = agent.join("self-update");
    std::fs::create_dir_all(&update_root)
        .map_err(|error| format!("could not create {}: {error}", update_root.display()))?;
    let update_root = validated_real_directory(&update_root, "self-update root")?;
    let staging = tempfile::Builder::new()
        .prefix("pending-")
        .tempdir_in(&update_root)
        .map_err(|error| format!("could not allocate staging directory: {error}"))?;
    validated_real_directory(staging.path(), "self-update staging directory")?;
    Ok(staging)
}

#[cfg(not(windows))]
fn build_direct_update_plan(
    staging_dir: &Path,
    target_exe: &Path,
) -> Result<DirectUpdatePlan, String> {
    let staging_dir = validated_real_directory(staging_dir, "self-update staging directory")?;
    let staged_exe = validated_regular_file(
        &staging_dir.join("bin").join(crate::APP_NAME),
        "staged rpi executable",
    )?;
    let target_exe = validated_regular_file(target_exe, "current rpi executable")?;
    let target_dir = validated_real_directory(
        target_exe
            .parent()
            .ok_or_else(|| "current executable has no parent directory".to_string())?,
        "current executable directory",
    )?;
    if target_exe.parent() != Some(target_dir.as_path()) {
        return Err("current executable is not a direct child of its validated directory".into());
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let replacement_temp = target_dir.join(format!(".rpi-update-{token}.new"));
    let backup_file = target_dir.join(format!(".rpi-update-{token}.old"));
    if replacement_temp.exists() || backup_file.exists() {
        return Err("self-update replacement path collision".to_string());
    }
    Ok(DirectUpdatePlan {
        staging_dir,
        staged_exe,
        target_exe,
        replacement_temp,
        backup_file,
    })
}

#[cfg(not(windows))]
fn activate_direct_update(plan: &DirectUpdatePlan) -> Result<semver::Version, String> {
    let staged_version = validate_rpi_executable_version(
        &plan.staged_exe,
        &plan.staging_dir,
        crate::VERSION,
        "staged rpi",
    )?;
    preflight_direct_replacement(plan)?;

    let result = (|| {
        std::fs::copy(&plan.staged_exe, &plan.replacement_temp).map_err(|error| {
            format!("could not copy staged rpi beside the current executable: {error}")
        })?;
        validated_regular_file(&plan.replacement_temp, "replacement rpi executable")?;
        let copied_version = validate_rpi_executable_version(
            &plan.replacement_temp,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "replacement rpi",
        )?;
        if copied_version != staged_version
            || !files_are_identical(&plan.staged_exe, &plan.replacement_temp)?
        {
            return Err(
                "replacement rpi does not match the validated staged executable".to_string(),
            );
        }

        std::fs::hard_link(&plan.target_exe, &plan.backup_file)
            .map_err(|error| format!("could not preserve the current rpi executable: {error}"))?;
        let installed_version = validate_rpi_executable_version(
            &plan.backup_file,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "current rpi",
        )?;
        if staged_version < installed_version {
            return Err(format!(
                "refusing to replace rpi {installed_version} with older version {staged_version}"
            ));
        }
        if !files_are_identical(&plan.target_exe, &plan.backup_file)? {
            return Err("current rpi executable changed while preparing the update".to_string());
        }

        std::fs::rename(&plan.replacement_temp, &plan.target_exe).map_err(|error| {
            format!("could not atomically replace the current rpi executable: {error}")
        })?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = remove_file_if_exists(&plan.replacement_temp);
        let _ = remove_file_if_exists(&plan.backup_file);
        return Err(error);
    }

    let validation = (|| {
        if !files_are_identical(&plan.staged_exe, &plan.target_exe)? {
            return Err("updated rpi executable does not match the staged executable".to_string());
        }
        let updated_version = validate_rpi_executable_version(
            &plan.target_exe,
            plan.target_exe
                .parent()
                .ok_or_else(|| "current executable has no parent directory".to_string())?,
            crate::VERSION,
            "updated rpi",
        )?;
        if updated_version != staged_version {
            return Err(format!(
                "updated rpi reported version {updated_version}, expected {staged_version}"
            ));
        }
        Ok(())
    })();

    if let Err(error) = validation {
        return Err(rollback_direct_update(plan, &error));
    }
    let _ = remove_file_if_exists(&plan.backup_file);
    Ok(staged_version)
}

#[cfg(not(windows))]
fn preflight_direct_replacement(plan: &DirectUpdatePlan) -> Result<(), String> {
    for path in [&plan.replacement_temp, &plan.backup_file] {
        if path.exists() {
            return Err(format!(
                "self-update path already exists: {}",
                path.display()
            ));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("cannot write beside the current executable: {error}"))?;
        drop(file);
        std::fs::remove_file(path)
            .map_err(|error| format!("could not remove self-update probe: {error}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn rollback_direct_update(plan: &DirectUpdatePlan, failure: &str) -> String {
    let _ = remove_file_if_exists(&plan.replacement_temp);
    match std::fs::rename(&plan.backup_file, &plan.target_exe) {
        Ok(()) => format!("{failure}; restored the previous rpi executable"),
        Err(rollback) => format!(
            "{failure}; automatic rollback failed ({rollback}); the previous executable remains at {}",
            plan.backup_file.display()
        ),
    }
}

#[cfg(not(windows))]
fn remove_file_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(windows))]
fn files_are_identical(left: &Path, right: &Path) -> Result<bool, String> {
    use std::io::{BufReader, Read};

    let left_file = std::fs::File::open(left)
        .map_err(|error| format!("could not open {}: {error}", left.display()))?;
    let right_file = std::fs::File::open(right)
        .map_err(|error| format!("could not open {}: {error}", right.display()))?;
    if left_file
        .metadata()
        .map_err(|error| error.to_string())?
        .len()
        != right_file
            .metadata()
            .map_err(|error| error.to_string())?
            .len()
    {
        return Ok(false);
    }
    let mut left = BufReader::new(left_file);
    let mut right = BufReader::new(right_file);
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_read = left
            .read(&mut left_buffer)
            .map_err(|error| error.to_string())?;
        let right_read = right
            .read(&mut right_buffer)
            .map_err(|error| error.to_string())?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct WindowsUpdatePlan {
    staging_dir: PathBuf,
    staged_exe: PathBuf,
    target_exe: PathBuf,
    helper_script: PathBuf,
    status_file: PathBuf,
    replacement_temp: PathBuf,
    backup_file: PathBuf,
    rollback_displaced_file: PathBuf,
}

#[cfg(windows)]
struct WindowsUpdateStaging {
    directory: tempfile::TempDir,
    status_file: PathBuf,
}

#[cfg(windows)]
const WINDOWS_UPDATE_HELPER: &str = r#"param(
    [Parameter(Mandatory = $true)][long] $ParentPid,
    [Parameter(Mandatory = $true)][string] $StagingPath,
    [Parameter(Mandatory = $true)][string] $StagedPath,
    [Parameter(Mandatory = $true)][string] $StagedVersion,
    [Parameter(Mandatory = $true)][string] $TargetPath,
    [Parameter(Mandatory = $true)][string] $TempPath,
    [Parameter(Mandatory = $true)][string] $BackupPath,
    [Parameter(Mandatory = $true)][string] $RollbackDisplacedPath,
    [Parameter(Mandatory = $true)][string] $StatusPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-UpdateStatus {
    param([string] $State, [string] $Message)
    $record = [ordered]@{
        state = $State
        message = $Message
        parentPid = $ParentPid
        staging = $StagingPath
        target = $TargetPath
        staged = $StagedPath
        stagedVersion = $StagedVersion
        backup = $BackupPath
        rollbackDisplaced = $RollbackDisplacedPath
        updatedAt = [DateTime]::UtcNow.ToString('o')
    }
    $json = $record | ConvertTo-Json -Compress
    $statusTemp = $StatusPath + '.tmp'
    [IO.File]::WriteAllText($statusTemp, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $statusTemp -Destination $StatusPath -Force
}

function Assert-RegularFile {
    param([string] $Path, [string] $Label)
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or $item.Length -le 0 -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label is not a non-empty regular file"
    }
}

function Assert-WindowsExecutable {
    param([string] $Path, [string] $Label)
    Assert-RegularFile -Path $Path -Label $Label
    $stream = [IO.File]::OpenRead($Path)
    try {
        if ($stream.ReadByte() -ne 77 -or $stream.ReadByte() -ne 90) {
            throw "$Label is not a Windows executable"
        }
    } finally {
        $stream.Dispose()
    }
}

function Assert-RealDirectory {
    param([string] $Path, [string] $Label)
    $item = Get-Item -LiteralPath $Path -Force
    if (-not $item.PSIsContainer -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "$Label is not a real directory"
    }
}

function Get-FileSha256 {
    param([string] $Path)
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            return ([BitConverter]::ToString($sha256.ComputeHash($stream))).Replace('-', '')
        } finally {
            $sha256.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Remove-StagingPayload {
    try {
        $root = [IO.Path]::GetFullPath($StagingPath)
        Assert-RealDirectory -Path $root -Label 'self-update staging directory'
        $bin = [IO.Path]::Combine($root, 'bin')
        $helper = [IO.Path]::Combine($root, 'apply-update.ps1')
        foreach ($file in @(
            $StagedPath,
            [IO.Path]::Combine($root, '.crates.toml'),
            [IO.Path]::Combine($root, '.crates2.json'),
            $helper
        )) {
            if ([IO.File]::Exists($file)) {
                [IO.File]::Delete($file)
            }
        }
        if ([IO.Directory]::Exists($bin) -and
            [IO.Directory]::GetFileSystemEntries($bin).Length -eq 0) {
            [IO.Directory]::Delete($bin, $false)
        }
        if ([IO.Directory]::Exists($root) -and
            [IO.Directory]::GetFileSystemEntries($root).Length -eq 0) {
            [IO.Directory]::Delete($root, $false)
        }
    } catch {
        # Cleanup is best-effort; update status and recovery files stay intact.
    }
}

$replaced = $false
try {
    $stagingFull = [IO.Path]::GetFullPath($StagingPath)
    $stagedFull = [IO.Path]::GetFullPath($StagedPath)
    $targetFull = [IO.Path]::GetFullPath($TargetPath)
    $tempFull = [IO.Path]::GetFullPath($TempPath)
    $backupFull = [IO.Path]::GetFullPath($BackupPath)
    $rollbackDisplacedFull = [IO.Path]::GetFullPath($RollbackDisplacedPath)
    $helperFull = [IO.Path]::Combine($stagingFull, 'apply-update.ps1')
    $statusFull = [IO.Path]::GetFullPath($StatusPath)
    $targetDirectory = [IO.Path]::GetDirectoryName($targetFull)
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName($stagedFull)), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($helperFull), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetFullPath($PSScriptRoot), $stagingFull) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($statusFull), [IO.Path]::GetDirectoryName($stagingFull)) -or
        [string]::IsNullOrWhiteSpace($targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($tempFull), $targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($backupFull), $targetDirectory) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetDirectoryName($rollbackDisplacedFull), $targetDirectory)) {
        throw 'self-update paths are outside their validated staging or target directories'
    }

    Assert-RealDirectory -Path $stagingFull -Label 'self-update staging directory'
    Assert-RealDirectory -Path $targetDirectory -Label 'target directory'
    Assert-WindowsExecutable -Path $stagedFull -Label 'staged executable'
    Assert-WindowsExecutable -Path $targetFull -Label 'current executable'
    $expectedHash = Get-FileSha256 -Path $stagedFull
    $originalTargetHash = Get-FileSha256 -Path $targetFull
    if ([IO.File]::Exists($tempFull) -or [IO.File]::Exists($backupFull) -or
        [IO.File]::Exists($rollbackDisplacedFull)) {
        throw 'replacement, backup, or rollback path already exists'
    }

    Write-UpdateStatus -State 'waiting' -Message 'waiting for the current rpi process to exit'
    if ($ParentPid -lt 1 -or $ParentPid -gt [int]::MaxValue) {
        throw 'parent process id is outside the supported Windows range'
    }
    $parentProcess = $null
    try {
        $candidate = [Diagnostics.Process]::GetProcessById([int]$ParentPid)
        try {
            $candidatePath = [IO.Path]::GetFullPath($candidate.MainModule.FileName)
            if ([StringComparer]::OrdinalIgnoreCase.Equals($candidatePath, $targetFull)) {
                $parentProcess = $candidate
            } else {
                $candidate.Dispose()
            }
        } catch {
            if ($candidate.HasExited) {
                $candidate.Dispose()
            } else {
                throw
            }
        }
    } catch [ArgumentException] {
        # The invoking process exited before the helper acquired its handle.
    }
    if ($null -ne $parentProcess) {
        try {
            if (-not $parentProcess.WaitForExit(600000)) {
                throw 'timed out waiting for the current rpi process to exit'
            }
        } finally {
            $parentProcess.Dispose()
        }
    }

    Assert-RealDirectory -Path $targetDirectory -Label 'target directory'
    Assert-WindowsExecutable -Path $stagedFull -Label 'staged executable'
    Assert-WindowsExecutable -Path $targetFull -Label 'current executable'
    if ((Get-FileSha256 -Path $stagedFull) -ne $expectedHash) {
        throw 'staged executable changed while waiting for rpi to exit'
    }
    if ((Get-FileSha256 -Path $targetFull) -ne $originalTargetHash) {
        throw 'current executable changed while waiting for rpi to exit'
    }
    if ([IO.File]::Exists($tempFull) -or [IO.File]::Exists($backupFull) -or
        [IO.File]::Exists($rollbackDisplacedFull)) {
        throw 'replacement, backup, or rollback path appeared while waiting for rpi to exit'
    }

    [IO.File]::Copy($stagedFull, $tempFull, $false)
    Assert-WindowsExecutable -Path $tempFull -Label 'replacement executable'
    if ((Get-FileSha256 -Path $tempFull) -ne $expectedHash) {
        throw 'replacement executable hash mismatch'
    }

    [IO.File]::Replace($tempFull, $targetFull, $backupFull, $true)
    $replaced = $true
    Assert-WindowsExecutable -Path $targetFull -Label 'updated executable'
    if ((Get-FileSha256 -Path $targetFull) -ne $expectedHash) {
        throw 'updated executable hash mismatch'
    }
} catch {
    $failureMessage = $_.Exception.Message
    $recoveryMessage = ''
    if ($replaced -and [IO.File]::Exists($BackupPath)) {
        try {
            [IO.File]::Copy($BackupPath, $TempPath, $false)
            Assert-WindowsExecutable -Path $TempPath -Label 'rollback executable'
            if ((Get-FileSha256 -Path $TempPath) -ne $originalTargetHash) {
                throw 'rollback executable hash mismatch'
            }
            [IO.File]::Replace($TempPath, $TargetPath, $RollbackDisplacedPath, $true)
            Assert-WindowsExecutable -Path $TargetPath -Label 'restored executable'
            if ((Get-FileSha256 -Path $TargetPath) -ne $originalTargetHash) {
                throw 'restored executable hash mismatch'
            }
            $replaced = $false
            try { [IO.File]::Delete($BackupPath) } catch {}
            try { [IO.File]::Delete($RollbackDisplacedPath) } catch {}
        } catch {
            $rollbackFailure = $_.Exception.Message
            if ([IO.File]::Exists($BackupPath)) {
                $recoveryMessage = "; automatic rollback failed ($rollbackFailure); the original executable remains at $BackupPath"
            } else {
                $recoveryMessage = "; automatic rollback failed ($rollbackFailure) and no verified recovery backup remains"
            }
        }
    }
    if ([IO.File]::Exists($TempPath)) {
        try { Remove-Item -LiteralPath $TempPath -Force } catch {}
    }
    try { Write-UpdateStatus -State 'failed' -Message ($failureMessage + $recoveryMessage) } catch {}
    Remove-StagingPayload
    exit 1
}

try { Write-UpdateStatus -State 'succeeded' -Message 'rpi executable replaced successfully' } catch {}
if ([IO.File]::Exists($BackupPath)) {
    try { Remove-Item -LiteralPath $BackupPath -Force } catch {}
}
Remove-StagingPayload
exit 0
"#;

#[cfg(windows)]
fn run_windows_self_update() -> i32 {
    let staging = match create_windows_update_staging() {
        Ok(staging) => staging,
        Err(error) => {
            eprintln!("error: could not create self-update staging directory: {error}");
            return 1;
        }
    };
    let staging_dir = staging.directory.path().to_path_buf();
    let status_file = staging.status_file.clone();
    let _ = write_windows_update_status(
        &status_file,
        "preparing",
        "installing the update into staging",
    );

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let message = format!("could not resolve the current rpi executable: {error}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
    };
    let powershell = match system_powershell_path() {
        Ok(path) => path,
        Err(error) => {
            let _ = write_windows_update_status(&status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };

    let command_dir = match create_self_update_command_dir() {
        Ok(directory) => directory,
        Err(error) => {
            let message = format!("could not create an isolated Cargo directory: {error}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
    };
    let mut cargo = match cargo_install_command(Some(&staging_dir), command_dir.path()) {
        Ok(command) => command,
        Err(error) => {
            let message = format!("invalid isolated Cargo directory: {error}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
    };
    let status = cargo.status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            let message = format!("cargo install exited with {status}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
        Err(error) => {
            let message = format!("could not run cargo (install Rust/Cargo first): {error}");
            let _ = write_windows_update_status(&status_file, "failed", &message);
            eprintln!("error: {message}");
            return 1;
        }
    }

    let plan = match build_windows_update_plan(&staging_dir, &current_exe, &status_file) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = write_windows_update_status(&status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };
    let staged_version = match validate_staged_rpi(&plan) {
        Ok(version) => version,
        Err(error) => {
            let _ = write_windows_update_status(&plan.status_file, "failed", &error);
            eprintln!("error: {error}");
            return 1;
        }
    };
    if let Err(error) = preflight_windows_replacement(&plan) {
        let _ = write_windows_update_status(&plan.status_file, "failed", &error);
        eprintln!("error: {error}");
        return 1;
    }
    if let Err(error) = std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER.as_bytes()) {
        let message = format!("could not write the self-update helper: {error}");
        let _ = write_windows_update_status(&plan.status_file, "failed", &message);
        eprintln!("error: {message}");
        return 1;
    }
    if validated_regular_file(&plan.helper_script, "self-update helper").is_err() {
        let message = "self-update helper did not pass regular-file validation";
        let _ = write_windows_update_status(&plan.status_file, "failed", message);
        eprintln!("error: {message}");
        return 1;
    }
    let _ = write_windows_update_status(
        &plan.status_file,
        "scheduled",
        &format!("validated rpi {staged_version}; waiting to replace rpi after this process exits"),
    );
    let mut command = windows_update_helper_command(
        &powershell,
        &plan,
        std::process::id(),
        &staged_version.to_string(),
    );
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match command.spawn() {
        Ok(_) => {
            let persisted_staging = staging.directory.keep();
            println!(
                "rpi update staged; it will be applied after this process exits\nStatus: {}",
                plan.status_file.display()
            );
            debug_assert!(git_paths_equal(&persisted_staging, &plan.staging_dir));
            0
        }
        Err(error) => {
            let message = format!("could not start the self-update helper: {error}");
            let _ = write_windows_update_status(&plan.status_file, "failed", &message);
            eprintln!("error: {message}");
            1
        }
    }
}

#[cfg(windows)]
fn create_windows_update_staging() -> Result<WindowsUpdateStaging, String> {
    let staging = create_self_update_staging_directory()?;
    let update_root = staging
        .path()
        .parent()
        .ok_or_else(|| "self-update staging directory has no parent".to_string())?;
    let status_file = update_root.join(format!("status-{}.json", uuid::Uuid::new_v4().simple()));
    if status_file.exists() {
        return Err("self-update status path collision".to_string());
    }
    Ok(WindowsUpdateStaging {
        directory: staging,
        status_file,
    })
}

#[cfg(windows)]
fn build_windows_update_plan(
    staging_dir: &Path,
    target_exe: &Path,
    status_file: &Path,
) -> Result<WindowsUpdatePlan, String> {
    let staging_dir = validated_real_directory(staging_dir, "self-update staging directory")?;
    if !status_file.is_absolute() {
        return Err("self-update status path must be absolute".to_string());
    }
    let status_parent = validated_real_directory(
        status_file
            .parent()
            .ok_or_else(|| "self-update status path has no parent".to_string())?,
        "self-update status directory",
    )?;
    if staging_dir.parent() != Some(status_parent.as_path()) {
        return Err("self-update status file is outside the staging parent".to_string());
    }
    let status_name = status_file
        .file_name()
        .ok_or_else(|| "self-update status file has no name".to_string())?;
    if !is_self_update_status_name(status_name) {
        return Err("self-update status file has an invalid name".to_string());
    }
    if status_file.exists() {
        validated_regular_file(status_file, "self-update status file")?;
    }
    let staged_exe = validated_windows_executable(
        &staging_dir.join("bin").join("rpi.exe"),
        "staged rpi executable",
    )?;
    let target_exe = validated_windows_executable(target_exe, "current rpi executable")?;
    let target_dir = validated_real_directory(
        target_exe
            .parent()
            .ok_or_else(|| "current executable has no parent directory".to_string())?,
        "current executable directory",
    )?;
    if target_exe.parent() != Some(target_dir.as_path()) {
        return Err("current executable is not a direct child of its validated directory".into());
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let replacement_temp = target_dir.join(format!(".rpi-update-{token}.new.exe"));
    let backup_file = target_dir.join(format!(".rpi-update-{token}.old.exe"));
    let rollback_displaced_file = target_dir.join(format!(".rpi-update-{token}.rollback-new.exe"));
    if replacement_temp.exists() || backup_file.exists() || rollback_displaced_file.exists() {
        return Err("self-update replacement path collision".to_string());
    }

    Ok(WindowsUpdatePlan {
        staging_dir: staging_dir.clone(),
        staged_exe,
        target_exe,
        helper_script: staging_dir.join("apply-update.ps1"),
        status_file: status_file.to_path_buf(),
        replacement_temp,
        backup_file,
        rollback_displaced_file,
    })
}

#[cfg(windows)]
fn preflight_windows_replacement(plan: &WindowsUpdatePlan) -> Result<(), String> {
    for path in [
        &plan.replacement_temp,
        &plan.backup_file,
        &plan.rollback_displaced_file,
    ] {
        if path.exists() {
            return Err(format!(
                "self-update path already exists: {}",
                path.display()
            ));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("cannot write beside the current executable: {error}"))?;
        drop(file);
        std::fs::remove_file(path)
            .map_err(|error| format!("could not remove self-update probe: {error}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_staged_rpi(plan: &WindowsUpdatePlan) -> Result<semver::Version, String> {
    let cwd = plan
        .helper_script
        .parent()
        .ok_or_else(|| "self-update staging directory is missing".to_string())?;
    validate_rpi_executable_version(&plan.staged_exe, cwd, crate::VERSION, "staged rpi")
}

fn validate_rpi_executable_version(
    executable: &Path,
    cwd: &Path,
    current: &str,
    label: &str,
) -> Result<semver::Version, String> {
    let program = executable
        .to_str()
        .ok_or_else(|| format!("{label} path cannot be represented as Unicode"))?;
    let args = vec!["--version".to_string()];
    let output = crate::npm::run_bounded_command_blocking(
        program,
        &args,
        cwd,
        &[],
        STAGED_RPI_CHECK_TIMEOUT,
        MAX_STAGED_RPI_OUTPUT_BYTES,
    )
    .map_err(|error| format!("{label} version check failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("{label} --version exited with {}", output.status));
    }
    validate_staged_rpi_version_output(&output.stdout, &output.stderr, current)
}

fn validate_staged_rpi_version_output(
    stdout: &[u8],
    stderr: &[u8],
    current: &str,
) -> Result<semver::Version, String> {
    if !stderr.is_empty() {
        return Err("staged rpi --version wrote unexpected stderr output".to_string());
    }
    let staged = parse_rpi_version_output(stdout)
        .ok_or_else(|| "staged rpi returned an invalid --version response".to_string())?;
    let current = semver::Version::parse(current)
        .map_err(|_| "the running rpi version is not valid semantic versioning".to_string())?;
    if staged < current {
        return Err(format!(
            "refusing to replace rpi {current} with older version {staged}"
        ));
    }
    Ok(staged)
}

fn parse_rpi_version_output(output: &[u8]) -> Option<semver::Version> {
    let output = std::str::from_utf8(output).ok()?;
    let line = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if line.contains(['\r', '\n']) {
        return None;
    }
    let version = line.strip_prefix(crate::APP_NAME)?.strip_prefix(' ')?;
    if version.is_empty() || version.chars().any(char::is_whitespace) {
        return None;
    }
    semver::Version::parse(version).ok()
}

fn validated_real_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {label}: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{label} is not a real directory"));
    }
    let canonical = std::fs::canonicalize(path)
        .map(normalize_git_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))?;
    if !git_paths_equal(&canonical, path) {
        return Err(format!("{label} resolves through a link or junction"));
    }
    Ok(canonical)
}

fn validated_regular_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {label}: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
        return Err(format!("{label} is not a non-empty regular file"));
    }
    let canonical = std::fs::canonicalize(path)
        .map(normalize_git_path)
        .map_err(|error| format!("could not canonicalize {label}: {error}"))?;
    if !git_paths_equal(&canonical, path) {
        return Err(format!("{label} resolves through a link or junction"));
    }
    Ok(canonical)
}

fn is_self_update_status_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(token) = name
        .strip_prefix("status-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validated_self_update_status_file(update_root: &Path, path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.parent() != Some(update_root) {
        return Err("self-update status file is outside the update root".to_string());
    }
    if !path.file_name().is_some_and(is_self_update_status_name) {
        return Err("self-update status file has an invalid name".to_string());
    }
    let path = validated_regular_file(path, "self-update status file")?;
    let length = std::fs::metadata(&path)
        .map_err(|error| format!("could not inspect self-update status file: {error}"))?
        .len();
    if length > MAX_SELF_UPDATE_STATUS_BYTES {
        return Err("self-update status file is too large".to_string());
    }
    Ok(path)
}

fn read_self_update_status(path: &Path) -> Option<SelfUpdateStatus> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::with_capacity(
        usize::try_from(MAX_SELF_UPDATE_STATUS_BYTES)
            .unwrap_or_default()
            .min(8 * 1024),
    );
    file.take(MAX_SELF_UPDATE_STATUS_BYTES.saturating_add(1))
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_SELF_UPDATE_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&data).ok()
}

fn sanitized_self_update_status_message(message: &str) -> String {
    let mut sanitized = String::new();
    let mut pending_space = false;
    let mut truncated = false;
    for character in message.trim().chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }
        if sanitized.chars().count() >= MAX_SELF_UPDATE_STATUS_MESSAGE_CHARS {
            truncated = true;
            break;
        }
        if pending_space {
            sanitized.push(' ');
            pending_space = false;
        }
        sanitized.push(character);
    }
    if sanitized.is_empty() {
        return "no diagnostic message was recorded".to_string();
    }
    if truncated {
        sanitized.push_str("...");
    }
    sanitized
}

fn consume_self_update_statuses(agent_dir: &Path) -> Vec<UpdateWarning> {
    let _guard = SELF_UPDATE_STATUS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let agent_dir = match validated_real_directory(agent_dir, "agent directory") {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let update_root = match validated_real_directory(
        &agent_dir.join("self-update"),
        "self-update status directory",
    ) {
        Ok(path) if path.parent() == Some(agent_dir.as_path()) => path,
        _ => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&update_root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        if !is_self_update_status_name(&entry.file_name()) {
            continue;
        }
        let path = match validated_self_update_status_file(&update_root, &entry.path()) {
            Ok(path) => path,
            Err(_) => continue,
        };
        let Some(status) = read_self_update_status(&path) else {
            continue;
        };
        if !matches!(status.state.as_str(), "failed" | "succeeded") {
            continue;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // A failure is still actionable even when cleanup is denied. Keep
            // the file so a later startup can retry consumption.
            Err(_) if status.state == "failed" => {}
            Err(_) => continue,
        }
        if status.state == "failed" {
            warnings.push(UpdateWarning {
                message: format!(
                    "The previously scheduled rpi update failed: {}",
                    sanitized_self_update_status_message(&status.message)
                ),
                command: "rpi update".to_string(),
            });
        }
    }
    warnings
}

#[cfg(windows)]
fn validated_windows_executable(path: &Path, label: &str) -> Result<PathBuf, String> {
    use std::io::Read;

    let path = validated_regular_file(path, label)?;
    let mut file =
        std::fs::File::open(&path).map_err(|error| format!("could not open {label}: {error}"))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)
        .map_err(|error| format!("could not read {label}: {error}"))?;
    if magic != *b"MZ" {
        return Err(format!("{label} is not a Windows executable"));
    }
    Ok(path)
}

#[cfg(windows)]
fn system_powershell_path() -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut buffer = vec![0u16; 260];
    let system_root = loop {
        let length = unsafe {
            GetWindowsDirectoryW(
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            )
        };
        if length == 0 {
            return Err(format!(
                "could not locate the Windows system directory: {}",
                std::io::Error::last_os_error()
            ));
        }
        let length = usize::try_from(length)
            .map_err(|error| format!("invalid Windows system directory length: {error}"))?;
        if length < buffer.len() {
            buffer.truncate(length);
            break PathBuf::from(OsString::from_wide(&buffer));
        }
        buffer.resize(length.saturating_add(1), 0);
    };
    let system_root = validated_real_directory(&system_root, "Windows system root")?;
    let powershell = validated_windows_executable(
        &system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        "Windows PowerShell",
    )?;
    powershell
        .strip_prefix(&system_root)
        .map_err(|_| "Windows PowerShell is outside SystemRoot".to_string())?;
    Ok(powershell)
}

#[cfg(windows)]
fn windows_update_helper_command(
    powershell: &Path,
    plan: &WindowsUpdatePlan,
    parent_pid: u32,
    staged_version: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new(powershell);
    let powershell_modules = powershell
        .parent()
        .expect("validated PowerShell path has a parent")
        .join("Modules");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&plan.helper_script)
        .arg("-ParentPid")
        .arg(parent_pid.to_string())
        .arg("-StagingPath")
        .arg(&plan.staging_dir)
        .arg("-StagedPath")
        .arg(&plan.staged_exe)
        .arg("-StagedVersion")
        .arg(staged_version)
        .arg("-TargetPath")
        .arg(&plan.target_exe)
        .arg("-TempPath")
        .arg(&plan.replacement_temp)
        .arg("-BackupPath")
        .arg(&plan.backup_file)
        .arg("-RollbackDisplacedPath")
        .arg(&plan.rollback_displaced_file)
        .arg("-StatusPath")
        .arg(&plan.status_file)
        .current_dir(
            plan.status_file
                .parent()
                .expect("validated status path has an update-root parent"),
        )
        .env("PSModulePath", powershell_modules)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(windows)]
fn write_windows_update_status(path: &Path, state: &str, message: &str) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(&serde_json::json!({
        "state": state,
        "message": message,
        "parentPid": std::process::id(),
        "currentVersion": crate::VERSION,
        "updatedAtMs": now_ms(),
    }))
    .map_err(|error| error.to_string())?;
    std::fs::write(path, data)
        .map_err(|error| format!("could not write update status {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[cfg(unix)]
    use super::{activate_direct_update, build_direct_update_plan};
    #[cfg(windows)]
    use super::{
        build_windows_update_plan, preflight_windows_replacement, system_powershell_path,
        windows_update_helper_command, write_windows_update_status, WINDOWS_UPDATE_HELPER,
    };
    use super::{
        cache_fallback_is_fresh, cargo_home_is_project_local, cargo_install_command,
        check_startup_with_client, collect_bounded, create_self_update_command_dir,
        fetch_crates_url, fetch_git_update, git_results_with_cache_fallback, git_update_target,
        hardened_git_config_args, hardened_git_environment, is_newer, is_validated_git_checkout,
        npm_registry_lookup_spec, parse_ls_remote_oid, parse_matching_git_origin,
        parse_npm_view_version, parse_origin_upstream, parse_rpi_version_output, path_is_within,
        report_from_cache, results_with_cache_fallback, results_with_individual_cache_fallback,
        validate_staged_rpi_version_output, write_cache, GitLookup, GitUpdateCache, RegistryLookup,
        UpdateCache, CACHE_FALLBACK_MAX_AGE_MS, UPDATE_CHECK_CONCURRENCY,
    };

    struct RestoreConfigDir(Option<OsString>);

    impl Drop for RestoreConfigDir {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    struct RestoreEnv {
        name: &'static str,
        value: Option<OsString>,
    }

    impl RestoreEnv {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                value: std::env::var_os(name),
            }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.value.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
        assert!(status.success(), "git {args:?} exited with {status}");
    }

    #[cfg(windows)]
    fn fake_windows_executable(path: &Path, marker: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = b"MZ".to_vec();
        bytes.extend_from_slice(marker);
        std::fs::write(path, bytes).unwrap();
    }

    #[cfg(unix)]
    fn fake_unix_rpi(path: &Path, version: &str, fail_after_rename_to: Option<&str>) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let failure = fail_after_rename_to.map_or_else(String::new, |name| {
            format!(
                "case \"$0\" in\n  */{name}) echo 'post-replace failure' >&2; exit 1 ;;\nesac\n"
            )
        });
        std::fs::write(
            path,
            format!("#!/bin/sh\n{failure}printf 'rpi {version}\\n'\n"),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_test_update_status(path: &Path, state: &str, message: &str) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "state": state,
                "message": message,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn cargo_self_update_staging_is_passed_as_a_structured_argument() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let command_dir = temp.path().join("command");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&command_dir).unwrap();
        let command = cargo_install_command(Some(&staging), &command_dir).unwrap();
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();

        assert_eq!(command.get_program(), "cargo");
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], "install");
        assert_eq!(args[1], "--root");
        assert_eq!(args[2], staging.as_os_str());
        assert_eq!(args[3], "rpi-cli");
        assert_eq!(args[4], "--locked");
        assert_eq!(args[5], "--force");
        assert_eq!(command.get_current_dir(), Some(command_dir.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn direct_update_replaces_the_selected_target_and_rejects_downgrades() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi");
        let target = temp.path().join("target/rpi-current");
        fake_unix_rpi(&staged, "9.9.9", None);
        fake_unix_rpi(&target, crate::VERSION, None);
        let expected = std::fs::read(&staged).unwrap();

        let plan = build_direct_update_plan(&staging, &target).unwrap();
        assert_eq!(activate_direct_update(&plan).unwrap().to_string(), "9.9.9");
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());

        fake_unix_rpi(&staged, "0.0.1", None);
        let plan = build_direct_update_plan(&staging, &target).unwrap();
        let error = activate_direct_update(&plan).unwrap_err();
        assert!(error.contains("older version"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn direct_update_rolls_back_when_post_replace_validation_fails() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi");
        let target = temp.path().join("target/rpi-current");
        fake_unix_rpi(&staged, "9.9.9", Some("rpi-current"));
        fake_unix_rpi(&target, crate::VERSION, None);
        let original = std::fs::read(&target).unwrap();

        let plan = build_direct_update_plan(&staging, &target).unwrap();
        let error = activate_direct_update(&plan).unwrap_err();

        assert!(error.contains("restored the previous"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
    }

    #[test]
    fn self_update_honors_env_and_early_dispatched_offline_flag() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore = RestoreEnv::capture(crate::args::PI_OFFLINE_ENV);

        std::env::set_var(crate::args::PI_OFFLINE_ENV, "tRuE");
        assert_eq!(super::run_self_update(&[]), 0);

        std::env::set_var(crate::args::PI_OFFLINE_ENV, "0");
        assert_eq!(super::run_self_update(&["--offline".into()]), 0);
        assert_eq!(
            std::env::var(crate::args::PI_OFFLINE_ENV).as_deref(),
            Ok("1")
        );
    }

    #[tokio::test]
    async fn pi_offline_skips_package_and_rpi_startup_checks_before_cache_io() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore_config = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let _restore_offline = RestoreEnv::capture(crate::args::PI_OFFLINE_ENV);
        let _restore_disabled = RestoreEnv::capture("RPI_DISABLE_UPDATE_CHECK");
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("offline-agent");
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
        std::env::set_var(crate::args::PI_OFFLINE_ENV, "YES");
        std::env::remove_var("RPI_DISABLE_UPDATE_CHECK");

        let report = super::check_startup_with_package_resources(None).await;

        assert_eq!(report, super::UpdateReport::default());
        assert!(
            !agent.exists(),
            "offline checks must not create the update cache directory"
        );
    }

    #[test]
    fn completed_self_update_statuses_are_consumed_once() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let update_root = agent.join("self-update");
        std::fs::create_dir_all(&update_root).unwrap();
        let failed = update_root.join("status-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json");
        let succeeded = update_root.join("status-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.json");
        let waiting = update_root.join("status-cccccccccccccccccccccccccccccccc.json");
        let temporary = update_root.join("status-dddddddddddddddddddddddddddddddd.json.tmp");
        let malformed = update_root.join("status-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json");
        let invalid_name = update_root.join("status-not-a-uuid.json");
        write_test_update_status(&failed, "failed", "permission\u{1b}[31m\r\n denied\u{7}");
        write_test_update_status(&succeeded, "succeeded", "done");
        write_test_update_status(&waiting, "waiting", "still running");
        write_test_update_status(&temporary, "failed", "not published");
        std::fs::write(&malformed, b"{not-json").unwrap();
        write_test_update_status(&invalid_name, "failed", "invalid name");

        let warnings = super::consume_self_update_statuses(&agent);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0]
            .message
            .contains("rpi update failed: permission [31m denied"));
        assert!(!warnings[0].message.chars().any(char::is_control));
        assert_eq!(warnings[0].command, "rpi update");
        assert!(!failed.exists());
        assert!(!succeeded.exists());
        assert!(waiting.exists());
        assert!(temporary.exists());
        assert!(malformed.exists());
        assert!(invalid_name.exists());
        assert!(super::consume_self_update_statuses(&agent).is_empty());
    }

    #[test]
    fn self_update_status_validation_rejects_escape_and_links() {
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let update_root = agent.join("self-update");
        let outside_root = temp.path().join("outside");
        std::fs::create_dir_all(&update_root).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let name = "status-ffffffffffffffffffffffffffffffff.json";
        let outside = outside_root.join(name);
        write_test_update_status(&outside, "failed", "outside");

        assert!(super::validated_self_update_status_file(&update_root, &outside).is_err());

        let linked = update_root.join(name);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        #[cfg(windows)]
        let link_created = std::os::windows::fs::symlink_file(&outside, &linked).is_ok();
        #[cfg(unix)]
        let link_created = true;
        if link_created {
            assert!(super::validated_self_update_status_file(&update_root, &linked).is_err());
            assert!(super::consume_self_update_statuses(&agent).is_empty());
            assert!(outside.exists());
        }
    }

    #[test]
    fn cargo_self_update_without_staging_never_inherits_the_callers_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let command_dir = temp.path().join("isolated");
        std::fs::create_dir_all(&command_dir).unwrap();

        let command = cargo_install_command(None, &command_dir).unwrap();
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();

        assert_eq!(
            args,
            ["install", "rpi-cli", "--locked", "--force"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(command.get_current_dir(), Some(command_dir.as_path()));
        assert_ne!(
            command.get_current_dir(),
            std::env::current_dir().ok().as_deref()
        );
        for variable in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        ] {
            assert!(command
                .get_envs()
                .any(|(name, value)| name == variable && value.is_none()));
        }
        assert!(cargo_install_command(None, Path::new("relative-command-dir")).is_err());

        let generated = create_self_update_command_dir().unwrap();
        let caller = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(generated.path().is_absolute());
        assert!(!path_is_within(generated.path(), &caller));
    }

    #[test]
    fn project_local_cargo_home_is_rejected_even_when_its_leaf_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let global = temp.path().join("global-cargo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        let project = std::fs::canonicalize(project).unwrap();

        assert!(cargo_home_is_project_local(&project.join(".cargo-home"), &project).unwrap());
        assert!(cargo_home_is_project_local(&project, &project).unwrap());
        assert!(!cargo_home_is_project_local(&global, &project).unwrap());
        assert!(cargo_home_is_project_local(Path::new("relative"), &project).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_atomically_replaces_a_valid_target() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging 'semi;& with spaces");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target 'semi;& with spaces/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&staged).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();

        let powershell = system_powershell_path().unwrap();
        let mut command =
            windows_update_helper_command(&powershell, &plan, i32::MAX as u32, "9.9.9");
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        for (flag, expected) in [
            ("-StagingPath", &plan.staging_dir),
            ("-StagedPath", &plan.staged_exe),
            ("-TargetPath", &plan.target_exe),
            ("-TempPath", &plan.replacement_temp),
            ("-BackupPath", &plan.backup_file),
            ("-RollbackDisplacedPath", &plan.rollback_displaced_file),
            ("-StatusPath", &plan.status_file),
        ] {
            let index = args.iter().position(|arg| arg == flag).unwrap();
            assert_eq!(args[index + 1], expected.as_os_str());
        }
        let version_index = args.iter().position(|arg| arg == "-StagedVersion").unwrap();
        assert_eq!(args[version_index + 1], "9.9.9");
        assert_eq!(command.get_current_dir(), plan.status_file.parent());
        assert!(command.get_envs().any(|(name, value)| {
            name == "PSModulePath"
                && value == Some(powershell.parent().unwrap().join("Modules").as_os_str())
        }));
        let status = command.status().unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(
            status.success(),
            "helper exited with {status}: {status_text}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "succeeded");
        assert_eq!(status["stagedVersion"], "9.9.9");
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_failure_preserves_the_old_executable() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging with spaces");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target with spaces/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        std::fs::write(&plan.helper_script, WINDOWS_UPDATE_HELPER).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();
        std::fs::remove_file(&plan.staged_exe).unwrap();

        let status = windows_update_helper_command(
            &system_powershell_path().unwrap(),
            &plan,
            i32::MAX as u32,
            "9.9.9",
        )
        .status()
        .unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(!status.success());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            expected,
            "status={status_text}; backup_exists={}",
            plan.backup_file.exists()
        );
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "failed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_rolls_back_after_post_replace_validation_failure() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target/rpi-copy.exe");
        fake_windows_executable(&staged, b"new-version");
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();
        let plan = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap();
        preflight_windows_replacement(&plan).unwrap();
        let forced_failure = WINDOWS_UPDATE_HELPER.replace(
            "(Get-FileSha256 -Path $targetFull) -ne $expectedHash",
            "$true",
        );
        assert_ne!(forced_failure, WINDOWS_UPDATE_HELPER);
        std::fs::write(&plan.helper_script, forced_failure).unwrap();
        write_windows_update_status(&plan.status_file, "scheduled", "test").unwrap();

        let status = windows_update_helper_command(
            &system_powershell_path().unwrap(),
            &plan,
            i32::MAX as u32,
            "9.9.9",
        )
        .status()
        .unwrap();

        let status_text = std::fs::read_to_string(&plan.status_file).unwrap();
        assert!(!status.success());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            expected,
            "status={status_text}; backup_exists={}",
            plan.backup_file.exists()
        );
        assert!(!staging.exists());
        assert!(!plan.replacement_temp.exists());
        assert!(!plan.backup_file.exists());
        assert!(!plan.rollback_displaced_file.exists());
        let status: serde_json::Value = serde_json::from_str(&status_text).unwrap();
        assert_eq!(status["state"], "failed");
        assert!(status["message"]
            .as_str()
            .unwrap()
            .contains("updated executable hash mismatch"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_plan_rejects_a_non_executable_staged_file() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let staged = staging.join("bin/rpi.exe");
        let target = temp.path().join("target/rpi.exe");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"not-a-pe").unwrap();
        fake_windows_executable(&target, b"old-version");
        let expected = std::fs::read(&target).unwrap();

        let error = build_windows_update_plan(
            &staging,
            &target,
            &temp
                .path()
                .join("status-00000000000000000000000000000000.json"),
        )
        .unwrap_err();

        assert!(error.contains("not a Windows executable"));
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    }

    #[test]
    fn staged_rpi_version_output_is_exact_and_cannot_downgrade() {
        assert_eq!(
            parse_rpi_version_output(b"rpi 1.2.3\r\n").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert_eq!(
            validate_staged_rpi_version_output(b"rpi 1.2.3\n", b"", "1.2.3").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert!(
            validate_staged_rpi_version_output(b"rpi 1.2.2\n", b"", "1.2.3")
                .unwrap_err()
                .contains("older version")
        );

        for output in [
            &b"pi 1.2.3\n"[..],
            &b"rpi v1.2.3\n"[..],
            &b"rpi 1.2.3 extra\n"[..],
            &b"rpi 1.2.3\nsecond line\n"[..],
            &b" rpi 1.2.3\n"[..],
        ] {
            assert!(
                parse_rpi_version_output(output).is_none(),
                "output={output:?}"
            );
        }
        assert!(
            validate_staged_rpi_version_output(b"rpi 1.2.3\n", b"warning", "1.2.3")
                .unwrap_err()
                .contains("stderr")
        );
    }

    #[test]
    fn compares_release_versions_conservatively() {
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(is_newer("v1.2.3", "1.3.0"));
        assert!(is_newer("1.2.3-beta.1", "1.2.3-beta.2"));
        assert!(is_newer("1.2.3-beta.2", "1.2.3"));
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
            native_packages: BTreeMap::new(),
            git_packages: BTreeMap::new(),
            freshness: Default::default(),
        };

        let report = report_from_cache(&cache, None);
        assert!(report.notices.is_empty());
    }

    #[test]
    fn package_notices_use_only_the_supplied_resource_set() {
        let temp = tempfile::tempdir().unwrap();
        let package_dir = temp.path().join(".rpi/packages/example-package");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"example-package","version":"1.0.0"}"#,
        )
        .unwrap();
        crate::packages::write_npm_source_marker(&package_dir, "npm:example-package").unwrap();
        let resources =
            crate::packages::discover(temp.path(), &["npm:example-package".to_string()]);
        assert_eq!(resources.packages.len(), 1);

        let mut packages = BTreeMap::new();
        packages.insert("npm:example-package".to_string(), "2.0.0".to_string());
        let cache = UpdateCache {
            checked_at: 0,
            rpi_latest: None,
            packages,
            native_packages: BTreeMap::new(),
            git_packages: BTreeMap::new(),
            freshness: Default::default(),
        };

        assert!(report_from_cache(&cache, None).notices.is_empty());
        let report = report_from_cache(&cache, Some(&resources));
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.notices[0].name, "example-package");
    }

    #[test]
    fn npm_view_version_accepts_tag_and_range_output_shapes() {
        assert_eq!(
            parse_npm_view_version(br#""2.0.0-beta.3""#).as_deref(),
            Some("2.0.0-beta.3")
        );
        assert_eq!(
            parse_npm_view_version(br#"["1.9.2","2.1.0","1.4.0"]"#).as_deref(),
            Some("2.1.0")
        );
        assert_eq!(
            parse_npm_view_version(br#"["v2.0.0-beta.10","2.0.0-beta.2","invalid"]"#).as_deref(),
            Some("v2.0.0-beta.10")
        );
        assert_eq!(parse_npm_view_version(br#"{"version":"1.0.0"}"#), None);
        assert_eq!(parse_npm_view_version(br#"[]"#), None);
        assert_eq!(parse_npm_view_version(br#"["latest","1.2"]"#), None);
        assert_eq!(parse_npm_view_version(br#""latest""#), None);
    }

    #[test]
    fn every_npm_source_uses_npm_view_command_arguments() {
        let command = crate::npm::NpmCommand::from_argv(Some(&[
            "wrapper".to_string(),
            "--fixed".to_string(),
        ]))
        .unwrap();
        for (source, expected_spec) in [
            ("npm:demo", "demo"),
            ("npm:demo@latest", "demo@latest"),
            ("npm:demo@^1", "demo@^1"),
            ("npm:@scope/demo@beta", "@scope/demo@beta"),
            ("npm:alias@npm:real@^1", "alias@npm:real@^1"),
            (
                "npm:@scope/alias@npm:@target/real@beta",
                "@scope/alias@npm:@target/real@beta",
            ),
        ] {
            let args = command.view_args(source).unwrap();
            assert_eq!(
                args.iter().map(String::as_str).collect::<Vec<_>>(),
                vec!["--fixed", "view", expected_spec, "version", "--json"],
                "source={source}"
            );
        }
        assert!(command.view_args("npm:   ").is_err());
    }

    #[test]
    fn npm_alias_update_lookup_queries_the_registry_target() {
        for (source, expected) in [
            ("npm:demo@^1", "npm:demo@^1"),
            ("npm:alias@npm:real", "npm:real"),
            (
                "npm:@scope/alias@npm:@target/real@beta",
                "npm:@target/real@beta",
            ),
        ] {
            assert_eq!(
                npm_registry_lookup_spec(source).as_deref(),
                Some(expected),
                "source={source}"
            );
        }
        assert!(npm_registry_lookup_spec("npm:alias@npm:real@npm:other").is_none());
        assert!(npm_registry_lookup_spec("npm:alias@file:../real").is_none());
    }

    #[test]
    fn package_cache_isolated_by_full_source_spec() {
        let temp = tempfile::tempdir().unwrap();
        let beta_dir = temp.path().join("project-a/.rpi/packages/demo");
        let range_dir = temp.path().join("project-b/.rpi/packages/demo");
        for (root, source) in [(&beta_dir, "npm:demo@beta"), (&range_dir, "npm:demo@^1")] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(
                root.join("package.json"),
                r#"{"name":"demo","version":"1.0.0"}"#,
            )
            .unwrap();
            crate::packages::write_npm_source_marker(root, source).unwrap();
        }
        let specs = vec![
            format!("file:{}", beta_dir.display()),
            format!("file:{}", range_dir.display()),
        ];
        let resources = crate::packages::discover(temp.path(), &specs);
        assert_eq!(resources.packages.len(), 2);
        let cache = UpdateCache {
            checked_at: 0,
            rpi_latest: None,
            packages: BTreeMap::from([
                ("demo".to_string(), "9.9.9".to_string()),
                ("npm:demo@beta".to_string(), "2.0.0-beta.1".to_string()),
                ("npm:demo@^1".to_string(), "1.0.0".to_string()),
            ]),
            native_packages: BTreeMap::new(),
            git_packages: BTreeMap::new(),
            freshness: Default::default(),
        };

        let report = report_from_cache(&cache, Some(&resources));
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.notices[0].latest, "2.0.0-beta.1");
    }

    #[test]
    fn failed_item_checks_keep_only_corresponding_cached_values() {
        let cached = BTreeMap::from([
            ("fresh".to_string(), "1.0.0".to_string()),
            ("failed".to_string(), "2.0.0".to_string()),
            ("removed".to_string(), "3.0.0".to_string()),
        ]);
        let results = vec![
            (
                "fresh".to_string(),
                RegistryLookup::Found("1.1.0".to_string()),
            ),
            ("failed".to_string(), RegistryLookup::TransientFailure),
            ("uncached".to_string(), RegistryLookup::TransientFailure),
            ("missing".to_string(), RegistryLookup::Missing),
        ];

        let (merged, used_fallback) = results_with_cache_fallback(results, Some(&cached), true);
        assert!(used_fallback);
        assert_eq!(merged.get("fresh").map(String::as_str), Some("1.1.0"));
        assert_eq!(merged.get("failed").map(String::as_str), Some("2.0.0"));
        assert!(!merged.contains_key("removed"));
        assert!(!merged.contains_key("uncached"));
        assert!(!merged.contains_key("missing"));
    }

    #[test]
    fn stale_cache_is_not_eligible_for_fallback() {
        let now = 10 * CACHE_FALLBACK_MAX_AGE_MS;
        let cache = UpdateCache {
            checked_at: now - CACHE_FALLBACK_MAX_AGE_MS - 1,
            rpi_latest: Some("9.9.9".into()),
            packages: BTreeMap::new(),
            native_packages: BTreeMap::new(),
            git_packages: BTreeMap::new(),
            freshness: Default::default(),
        };
        assert!(!cache_fallback_is_fresh(&cache, now));
    }

    #[test]
    fn old_cache_without_git_packages_remains_readable() {
        let cache: UpdateCache = serde_json::from_str(
            r#"{"checkedAt":1,"rpiLatest":"1.0.0","packages":{},"nativePackages":{}}"#,
        )
        .unwrap();

        assert!(cache.git_packages.is_empty());
    }

    #[tokio::test]
    async fn package_update_checks_have_a_shared_concurrency_limit() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let checks = (0..(UPDATE_CHECK_CONCURRENCY * 3)).map(|index| {
            let in_flight = Arc::clone(&in_flight);
            let maximum = Arc::clone(&maximum);
            async move {
                let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                index
            }
        });

        let mut results = collect_bounded(checks).await;
        results.sort_unstable();

        assert_eq!(
            results,
            (0..(UPDATE_CHECK_CONCURRENCY * 3)).collect::<Vec<_>>()
        );
        assert_eq!(maximum.load(Ordering::SeqCst), UPDATE_CHECK_CONCURRENCY);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bounded_checks_do_not_head_of_line_block_new_work() {
        let release_first = Arc::new(tokio::sync::Notify::new());
        let checks = (0..(UPDATE_CHECK_CONCURRENCY + 1)).map(|index| {
            let release_first = Arc::clone(&release_first);
            async move {
                if index == 0 {
                    release_first.notified().await;
                } else if index == UPDATE_CHECK_CONCURRENCY {
                    release_first.notify_one();
                }
                index
            }
        });

        let mut results =
            tokio::time::timeout(std::time::Duration::from_secs(1), collect_bounded(checks))
                .await
                .expect("a completed later check must free a concurrency slot");
        results.sort_unstable();
        assert_eq!(results, (0..=UPDATE_CHECK_CONCURRENCY).collect::<Vec<_>>());
    }

    #[test]
    fn fresh_results_keep_individual_timestamps_when_a_peer_uses_fallback() {
        let now = 10 * CACHE_FALLBACK_MAX_AGE_MS;
        let nearly_stale = now - CACHE_FALLBACK_MAX_AGE_MS + 60_000;
        let cached = BTreeMap::from([
            ("fresh".to_string(), "1.0.0".to_string()),
            ("failed".to_string(), "2.0.0".to_string()),
        ]);
        let freshness = BTreeMap::from([
            ("fresh".to_string(), nearly_stale),
            ("failed".to_string(), nearly_stale),
        ]);
        let (values, timestamps) = results_with_individual_cache_fallback(
            vec![
                (
                    "fresh".to_string(),
                    RegistryLookup::Found("1.1.0".to_string()),
                ),
                ("failed".to_string(), RegistryLookup::TransientFailure),
            ],
            Some(&cached),
            Some(&freshness),
            nearly_stale,
            now,
        );

        assert_eq!(timestamps.get("fresh"), Some(&now));
        assert_eq!(timestamps.get("failed"), Some(&nearly_stale));
        let after_old_entry_expires = now + 120_000;
        let (fallback, _) = results_with_individual_cache_fallback(
            vec![
                ("fresh".to_string(), RegistryLookup::TransientFailure),
                ("failed".to_string(), RegistryLookup::TransientFailure),
            ],
            Some(&values),
            Some(&timestamps),
            nearly_stale,
            after_old_entry_expires,
        );
        assert_eq!(fallback.get("fresh").map(String::as_str), Some("1.1.0"));
        assert!(!fallback.contains_key("failed"));
    }

    #[tokio::test]
    async fn crates_rate_limits_and_request_timeouts_are_transient() {
        async fn lookup(status: &str, body: &str) -> RegistryLookup {
            use std::io::{BufRead, BufReader, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let status = status.to_string();
            let body = body.to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                {
                    let mut reader = BufReader::new(&mut stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let client = reqwest::Client::builder().build().unwrap();
            let result = fetch_crates_url(&client, &format!("http://{address}/crate")).await;
            server.join().unwrap();
            result
        }

        assert_eq!(
            lookup("408 Request Timeout", "").await,
            RegistryLookup::TransientFailure
        );
        assert_eq!(
            lookup("429 Too Many Requests", "").await,
            RegistryLookup::TransientFailure
        );
        let malformed = lookup("200 OK", "{malformed-json").await;
        assert_eq!(malformed, RegistryLookup::TransientFailure);
        assert_eq!(
            super::lookup_with_cache_fallback(malformed, Some("9.9.9"), true),
            (Some("9.9.9".to_string()), true)
        );
        assert_eq!(lookup("404 Not Found", "").await, RegistryLookup::Missing);
    }

    #[test]
    fn git_cache_fallback_requires_the_same_checkout_head() {
        let old_head = "1".repeat(40);
        let cached = BTreeMap::from([(
            "git:github.com/example/repo".to_string(),
            GitUpdateCache {
                current: old_head.clone(),
                latest: "2".repeat(40),
            },
        )]);

        let (matching, used_fallback) = git_results_with_cache_fallback(
            vec![(
                "git:github.com/example/repo".to_string(),
                GitLookup::TransientFailure(Some(old_head)),
            )],
            Some(&cached),
            true,
        );
        assert!(used_fallback);
        assert_eq!(matching, cached);

        let (changed, used_fallback) = git_results_with_cache_fallback(
            vec![(
                "git:github.com/example/repo".to_string(),
                GitLookup::TransientFailure(Some("3".repeat(40))),
            )],
            Some(&cached),
            true,
        );
        assert!(!used_fallback);
        assert!(changed.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unavailable_http_client_still_revalidates_git_cache() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let root = agent.join("git/github.com/example/repo");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);
        git(&root, &["init", "--initial-branch", "main"]);
        git(&root, &["config", "user.email", "rpi-test@example.invalid"]);
        git(&root, &["config", "user.name", "rpi-test"]);
        std::fs::write(root.join("version.txt"), "one\n").unwrap();
        git(&root, &["add", "version.txt"]);
        git(&root, &["commit", "-m", "one"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://evil.example/example/repo.git",
            ],
        );

        let spec = "git:github.com/example/repo".to_string();
        let resources = crate::packages::discover(temp.path(), std::slice::from_ref(&spec));
        assert_eq!(resources.packages.len(), 1);
        let cache = UpdateCache {
            checked_at: super::now_ms(),
            git_packages: BTreeMap::from([(
                spec,
                GitUpdateCache {
                    current: "1".repeat(40),
                    latest: "2".repeat(40),
                },
            )]),
            ..UpdateCache::default()
        };
        assert_eq!(report_from_cache(&cache, Some(&resources)).notices.len(), 1);
        write_cache(&agent.join(super::CACHE_FILE), &cache).unwrap();

        let npm_command = crate::npm::NpmCommand::from_argv(None).unwrap();
        let report = check_startup_with_client(Some(&resources), &npm_command, None, None).await;

        assert!(report.notices.is_empty());
        assert!(super::read_cache(&agent.join(super::CACHE_FILE))
            .unwrap()
            .git_packages
            .is_empty());
    }

    #[test]
    fn startup_git_commands_remove_command_capable_environment() {
        let environment = hardened_git_environment();
        for name in ["GIT_CONFIG_COUNT", "GIT_SSH_COMMAND", "GIT_TEMPLATE_DIR"] {
            assert!(environment.iter().any(|(actual, value)| {
                actual == std::ffi::OsStr::new(name) && value.is_none()
            }));
        }

        let config = hardened_git_config_args();
        assert!(config
            .windows(2)
            .any(|pair| { pair == ["-c".to_string(), "protocol.ext.allow=never".to_string()] }));
        assert!(config
            .windows(2)
            .any(|pair| { pair == ["-c".to_string(), "credential.helper=".to_string()] }));
    }

    #[test]
    fn git_remote_parsers_accept_only_safe_exact_values() {
        assert_eq!(
            parse_origin_upstream(b"origin/feature/update\n").as_deref(),
            Some("feature/update")
        );
        for value in [
            "upstream/main\n",
            "origin/-upload-pack=evil\n",
            "origin/.hidden\n",
            "origin/feature..escape\n",
            "origin/feature@{1}\n",
            "origin/feature\\escape\n",
            "origin/main.lock\n",
        ] {
            assert!(
                parse_origin_upstream(value.as_bytes()).is_none(),
                "value={value:?}"
            );
        }

        let sha1 = "a".repeat(40);
        let sha256 = "B".repeat(64);
        assert_eq!(
            parse_ls_remote_oid(
                format!("{sha1}\trefs/heads/main\n").as_bytes(),
                "refs/heads/main"
            )
            .as_deref(),
            Some(sha1.as_str())
        );
        assert_eq!(
            parse_ls_remote_oid(format!("{sha256}\tHEAD\n").as_bytes(), "HEAD").as_deref(),
            Some(sha256.to_ascii_lowercase().as_str())
        );
        assert!(parse_ls_remote_oid(
            format!("{sha1}\trefs/heads/other\n").as_bytes(),
            "refs/heads/main"
        )
        .is_none());
        assert!(parse_ls_remote_oid(b"not-an-oid\tHEAD\n", "HEAD").is_none());
        assert!(parse_ls_remote_oid(format!("{sha1}\tHEAD\textra\n").as_bytes(), "HEAD").is_none());

        let source = "git:github.com/example/repo";
        assert_eq!(
            parse_matching_git_origin(b"https://github.com/example/repo.git\n", source).as_deref(),
            Some("https://github.com/example/repo.git")
        );
        assert_eq!(
            parse_matching_git_origin(b"git@github.com:example/repo.git\n", source).as_deref(),
            Some("git@github.com:example/repo.git")
        );
        for origin in [
            "https://github.com/other/repo.git\n",
            "https://evil.example/example/repo.git\n",
            "ext::sh -c evil github.com/example/repo\n",
            "https://github.com/example/repo.git\nhttps://github.com/example/repo.git\n",
        ] {
            assert!(
                parse_matching_git_origin(origin.as_bytes(), source).is_none(),
                "origin={origin:?}"
            );
        }
        assert!(parse_matching_git_origin(
            b"https://github.com/example/repo.git\n",
            "git:github.com/example/repo@main"
        )
        .is_none());
    }

    #[test]
    fn git_notices_require_an_unpinned_managed_checkout() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let _restore = RestoreConfigDir(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let temp = tempfile::tempdir().unwrap();
        let agent = temp.path().join("agent");
        let root = agent.join("git/github.com/example/repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"repo"}"#).unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, &agent);

        let spec = "git:github.com/example/repo".to_string();
        let resources = crate::packages::discover(temp.path(), std::slice::from_ref(&spec));
        assert_eq!(resources.packages.len(), 1);
        assert!(git_update_target(&resources.packages[0]).is_some());

        let current = "1".repeat(40);
        let latest = "2".repeat(40);
        let cache = UpdateCache {
            git_packages: BTreeMap::from([(
                spec,
                GitUpdateCache {
                    current: current.clone(),
                    latest: latest.clone(),
                },
            )]),
            ..UpdateCache::default()
        };
        let report = report_from_cache(&cache, Some(&resources));
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.notices[0].name, "github.com/example/repo");
        assert_eq!(report.notices[0].current, &current[..12]);
        assert_eq!(report.notices[0].latest, &latest[..12]);

        let pinned = crate::packages::discover(
            temp.path(),
            &["git:github.com/example/repo@main".to_string()],
        );
        assert_eq!(pinned.packages.len(), 1);
        assert!(git_update_target(&pinned.packages[0]).is_none());
        assert!(report_from_cache(&cache, Some(&pinned)).notices.is_empty());
    }

    #[test]
    fn git_checkout_validation_rejects_worktree_pointer_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: ../outside/.git\n").unwrap();

        assert!(!is_validated_git_checkout(&root));
    }

    #[tokio::test]
    async fn git_lookup_rejects_an_origin_that_does_not_match_the_package_source() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
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
        git(
            &checkout,
            &[
                "remote",
                "add",
                "origin",
                "https://evil.example/example/repo.git",
            ],
        );

        assert_eq!(
            fetch_git_update(&checkout, "git:github.com/example/repo").await,
            GitLookup::Invalid
        );
    }
}
