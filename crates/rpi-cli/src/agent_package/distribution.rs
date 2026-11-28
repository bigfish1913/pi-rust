use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{identifier, Commands, Manifest};
use crate::prebuilt_extensions::{best_tag_version, body_limited, host_target, repository_tags};

const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    schema_version: u32,
    name: String,
    version: String,
    asset: Option<String>,
    bytes: Option<u64>,
    sha256: Option<String>,
    targets: Option<BTreeMap<String, Archive>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    asset: String,
    bytes: u64,
    sha256: String,
}

const REPOSITORY: &str = "https://github.com/pi-rust/rpi-package";
pub(crate) async fn install(args: &[String]) -> i32 {
    apply_release("install", args).await
}

pub(crate) async fn update(args: &[String]) -> i32 {
    apply_release("update", args).await
}

async fn apply_release(operation: &str, args: &[String]) -> i32 {
    let args: Vec<_> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--offline")
        .collect();
    let usage = format!(
        "rpi package {operation} NAME [--version VERSION | --manifest URL] [--scope global]"
    );
    match stage(&args, &usage).await {
        Ok(release) => crate::resource_management::publish_release(
            &[
                operation.into(),
                release.name,
                "--path".into(),
                release.root.to_string_lossy().into_owned(),
                "--scope".into(),
                "global".into(),
                "--version".into(),
                release.version,
            ],
            &release.origin,
        ),
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

struct StagedRelease {
    _directory: tempfile::TempDir,
    root: PathBuf,
    name: String,
    version: String,
    origin: String,
}

async fn stage(args: &[&str], usage: &str) -> Result<StagedRelease, String> {
    let [name, options @ ..] = args else {
        return Err(usage.into());
    };
    let name = *name;
    if !identifier(name) {
        return Err("invalid agent package name".into());
    }
    let mut version = None;
    let mut manifest = None;
    let mut scoped = false;
    for option in options.chunks(2) {
        match option {
            ["--scope", "global"] if !scoped => scoped = true,
            ["--scope", scope] if *scope != "global" => {
                return Err("published agent packages require --scope global".into());
            }
            ["--version", value] if version.is_none() && manifest.is_none() => {
                version = Some(semver::Version::parse(value).map_err(|e| e.to_string())?);
            }
            ["--manifest", value] if version.is_none() && manifest.is_none() => {
                manifest = Some(*value);
            }
            _ => return Err(usage.into()),
        }
    }
    if crate::args::offline_env_enabled() {
        return Err("package download is disabled in offline mode".into());
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("rpi/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || check_url(attempt.url()).is_err() {
                attempt.error("invalid package download redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| e.to_string())?;
    let (url, version) = match manifest {
        Some(url) => (Url::parse(url).map_err(|e| e.to_string())?, None),
        None => {
            let version = match version {
                Some(version) => version,
                None => {
                    let refs = repository_tags(&client, "https://api.github.com", name, REPOSITORY)
                        .await?;
                    best_tag_version(&refs, name)
                        .ok_or_else(|| format!("no stable release tag found for {name}"))?
                }
            };
            let url = Url::parse(&format!(
                "{REPOSITORY}/releases/download/{name}-v{version}/release.json"
            ))
            .map_err(|e| e.to_string())?;
            (url, Some(version))
        }
    };
    check_url(&url)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err("package manifest URL must not contain a query or fragment".into());
    }
    let manifest = download(&client, url.clone(), 64 * 1024).await?;
    let release: Release = serde_json::from_slice(&manifest).map_err(|e| e.to_string())?;
    let release_version = semver::Version::parse(&release.version).map_err(|e| e.to_string())?;
    if release.schema_version != 1
        || release.name != name
        || version.is_some_and(|version| version != release_version)
    {
        return Err("invalid package release identity".into());
    }
    let (archive, suffix) = match (
        release.asset,
        release.bytes,
        release.sha256,
        release.targets,
    ) {
        (Some(asset), Some(bytes), Some(sha256), None) => (
            Archive {
                asset,
                bytes,
                sha256,
            },
            String::new(),
        ),
        (None, None, None, Some(mut targets)) => {
            let target = host_target()?;
            let archive = targets
                .remove(target)
                .ok_or_else(|| format!("package {name} has no archive for {target}"))?;
            (archive, format!("-{target}"))
        }
        _ => return Err("invalid package archive declaration".into()),
    };
    if archive.asset != format!("{name}-{release_version}{suffix}.zip")
        || archive.bytes == 0
        || archive.bytes > MAX_ARCHIVE_BYTES
        || archive.sha256.len() != 64
        || !archive
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("invalid package archive metadata".into());
    }
    let asset = url.join(&archive.asset).map_err(|e| e.to_string())?;
    let bytes = download(&client, asset, archive.bytes).await?;
    if bytes.len() as u64 != archive.bytes
        || format!("{:x}", Sha256::digest(&bytes)) != archive.sha256
    {
        return Err("package archive size or SHA-256 mismatch".into());
    }
    let bundle = tempfile::tempdir().map_err(|e| e.to_string())?;
    extract(&bytes, bundle.path())?;
    let root = bundle.path().canonicalize().map_err(|e| e.to_string())?;
    Manifest::read(&root, name, Some(&release.version))?;
    Commands::load(&root, name, Some(&release.version))?;
    Ok(StagedRelease {
        _directory: bundle,
        root,
        name: name.into(),
        version: release.version,
        origin: url.to_string(),
    })
}

fn check_url(url: &Url) -> Result<(), String> {
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("package downloads require HTTPS or loopback HTTP without credentials".into());
    }
    Ok(())
}

async fn download(client: &Client, url: Url, limit: u64) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    body_limited(response, limit).await
}

fn extract(bytes: &[u8], root: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    if archive.len() > 2048 {
        return Err("too many package files".into());
    }
    let mut names = HashSet::new();
    let mut total = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let name = entry.name();
        if !entry.is_file()
            || entry.is_symlink()
            || name.len() > 1024
            || name.split('/').count() > 32
            || name.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.ends_with(['.', ' '])
                    || part
                        .bytes()
                        .any(|b| b.is_ascii_control() || b"\\:<>\"|?*".contains(&b))
            })
            || entry.enclosed_name().is_none()
            || !names.insert(name.to_lowercase())
            || entry.size() > MAX_ARCHIVE_BYTES
            || total + entry.size() > MAX_EXTRACTED_BYTES
        {
            return Err(format!("invalid package archive entry: {name:?}"));
        }
        let dest = root.join(name);
        fs::create_dir_all(dest.parent().ok_or("missing archive file parent")?)
            .map_err(|e| e.to_string())?;
        let mut target = fs::File::create_new(&dest).map_err(|e| e.to_string())?;
        let expected = entry.size();
        let copied = io::copy(&mut entry.by_ref().take(expected + 1), &mut target)
            .map_err(|e| e.to_string())?;
        if copied != expected {
            return Err("package archive entry size mismatch".into());
        }
        total += copied;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            target
                .set_permissions(fs::Permissions::from_mode(mode & 0o755))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
