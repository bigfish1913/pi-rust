use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, Response, StatusCode, Url};
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
    let usage = format!("rpi {operation} package NAME [--version VERSION | --manifest URL]");
    if args == ["--help"] || args == ["-h"] {
        println!("{usage}");
        return 0;
    }
    match stage(&args, &usage).await {
        Ok(release) => crate::resource_management::publish_release(
            &[
                operation.into(),
                "package".into(),
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
    let (name, version, manifest) = match args {
        [name] => (*name, None, None),
        [name, "--version", version] => (
            *name,
            Some(semver::Version::parse(version).map_err(|e| e.to_string())?),
            None,
        ),
        [name, "--manifest", manifest] => (*name, None, Some(*manifest)),
        _ => return Err(usage.into()),
    };
    if !identifier(name) {
        return Err("invalid agent package name".into());
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
    let response = download(&client, url.clone())
        .await
        .map_err(|error| error.to_string())?;
    let archive_base = archive_base(&url, response.url()).clone();
    let manifest = body_limited(response, 64 * 1024).await?;
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
    let asset = archive_base
        .join(&archive.asset)
        .map_err(|e| e.to_string())?;
    let release_asset = url
        .join(&archive.asset)
        .map_err(|error| error.to_string())?;
    let bytes = fetch_archive(&client, asset, release_asset, &archive).await?;
    let bundle = tempfile::tempdir().map_err(|e| e.to_string())?;
    extract(&bytes, bundle.path())?;
    let root = bundle.path().canonicalize().map_err(|e| e.to_string())?;
    let (manifest, _) = Manifest::read(&root, name, Some(&release.version))?;
    manifest.validate_release_instructions(&root)?;
    Commands::load(&root, name, Some(&release.version))?;
    Ok(StagedRelease {
        _directory: bundle,
        root,
        name: name.into(),
        version: release.version,
        origin: url.to_string(),
    })
}

async fn fetch_archive(
    client: &Client,
    asset: Url,
    release_asset: Url,
    archive: &Archive,
) -> Result<Vec<u8>, String> {
    let first = match download(client, asset.clone()).await {
        Ok(response) => verified_archive_body(response, archive).await,
        Err(error)
            if matches!(
                error.status(),
                Some(
                    StatusCode::BAD_REQUEST
                        | StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::NOT_FOUND
                        | StatusCode::GONE
                )
            ) =>
        {
            Err(error.to_string())
        }
        Err(error) => return Err(error.to_string()),
    };
    match first {
        Err(error) if asset != release_asset => {
            // The manifest can redirect to an opaque object endpoint. Its
            // apparent sibling may be absent or even serve an HTML page.
            match download(client, release_asset).await {
                Ok(response) => verified_archive_body(response, archive)
                    .await
                    .map_err(|_| error),
                Err(_) => Err(error),
            }
        }
        result => result,
    }
}

async fn verified_archive_body(response: Response, archive: &Archive) -> Result<Vec<u8>, String> {
    let bytes = body_limited(response, archive.bytes).await?;
    if bytes.len() as u64 != archive.bytes
        || format!("{:x}", Sha256::digest(&bytes)) != archive.sha256
    {
        return Err("package archive size or SHA-256 mismatch".into());
    }
    Ok(bytes)
}

fn archive_base<'a>(requested: &'a Url, effective: &'a Url) -> &'a Url {
    // GitHub signs each asset separately and redirects it to a temporary
    // download URL with no sibling assets. Use the public release directory.
    if requested.host_str() == Some("github.com")
        && (requested.path().contains("/releases/download/")
            || requested.path().contains("/releases/latest/download/"))
    {
        requested
    } else {
        effective
    }
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

async fn download(client: &Client, url: Url) -> Result<Response, reqwest::Error> {
    client.get(url).send().await?.error_for_status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn sibling_archive_uses_the_release_directory_after_redirect() {
        let parse = |s: &str| Url::parse(s).unwrap();
        let requested = parse("https://example.test/latest/release.json");
        let effective = parse("https://example.test/1.0.0/release.json");
        assert_eq!(archive_base(&requested, &effective), &effective);

        let effective = parse("https://release-assets.githubusercontent.com/id?signature=fixture");
        for path in ["download/pkg-v1.0.0", "latest/download"] {
            let requested = parse(&format!(
                "https://github.com/org/repo/releases/{path}/release.json"
            ));
            assert_eq!(archive_base(&requested, &effective), &requested);
        }
    }

    #[tokio::test]
    async fn archive_fallback_checks_the_digest_of_both_candidates() {
        let valid = b"valid bytes";
        let wrong = b"wrong bytes";
        let archive = Archive {
            asset: "bundle.zip".into(),
            bytes: valid.len() as u64,
            sha256: format!("{:x}", Sha256::digest(valid)),
        };
        let client = Client::builder().no_proxy().build().unwrap();
        for public in [valid, wrong] {
            let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Url::parse(&format!("http://{}/", server.local_addr().unwrap())).unwrap();
            let serve = async {
                for (path, body) in [("objects", wrong), ("release", public)] {
                    let (mut stream, _) = server.accept().await.unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    assert!(line.starts_with(&format!("GET /{path}/bundle.zip ")));
                    loop {
                        line.clear();
                        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(body).await.unwrap();
                }
            };
            let fetch = fetch_archive(
                &client,
                base.join("objects/bundle.zip").unwrap(),
                base.join("release/bundle.zip").unwrap(),
                &archive,
            );
            let (result, ()) =
                tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(fetch, serve) })
                    .await
                    .unwrap();
            if public == valid {
                assert_eq!(result.unwrap(), valid);
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    "package archive size or SHA-256 mismatch"
                );
            }
        }
    }
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
