use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const RELEASE_CRATES: &[(&str, &str)] = &[
    ("rpi-telemetry", "crates/rpi-telemetry/Cargo.toml"),
    ("rpi-ai", "crates/rpi-ai/Cargo.toml"),
    ("rpi-agent", "crates/rpi-agent/Cargo.toml"),
    ("rpi-plugin-sdk", "crates/rpi-plugin-sdk/Cargo.toml"),
    ("rpi-extensions", "crates/rpi-extensions/Cargo.toml"),
    ("rpi-tools", "crates/rpi-tools/Cargo.toml"),
    ("rpi-harness", "crates/rpi-harness/Cargo.toml"),
    ("rpi-tui", "crates/rpi-tui/Cargo.toml"),
    ("rpi-cli", "crates/rpi-cli/Cargo.toml"),
];

const INTERNAL_DEPENDENCIES: &[&str] = &[
    "rpi-telemetry",
    "rpi-ai",
    "rpi-agent",
    "rpi-plugin-sdk",
    "rpi-extensions",
    "rpi-tools",
    "rpi-harness",
    "rpi-tui",
];

const USER_AGENT: &str = "pi-rust Taskfile release workflow";

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [command, version] if command == "validate" => validate(version, false),
        [command, version, flag] if command == "validate" && flag == "--allow-dirty" => {
            validate(version, true)
        }
        [command] if command == "dry-run" => dry_run(),
        [command, version] if command == "publish" => publish(version),
        [command, crate_name, version] if command == "probe" => probe(crate_name, version),
        _ => Err(
            "usage: release-helper <validate VERSION [--allow-dirty]|dry-run|publish VERSION|probe CRATE VERSION>"
                .to_string(),
        ),
    }
}

fn validate(expected: &str, allow_dirty: bool) -> Result<(), String> {
    if !allow_dirty {
        let status = command_output(
            "git",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if !status.stdout.is_empty() {
            return Err("release requires a clean git worktree (including untracked files)".into());
        }
    }

    let metadata = command_output(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1", "--locked"],
    )
    .map_err(|error| format!("cargo metadata --locked failed: {error}"))?;
    let metadata = String::from_utf8(metadata.stdout)
        .map_err(|_| "cargo metadata returned non-UTF-8 JSON".to_string())?;
    let packages = metadata
        .split_once("\"packages\":[")
        .and_then(|(_, rest)| {
            rest.split_once("],\"workspace_members\"")
                .map(|(value, _)| value)
        })
        .ok_or_else(|| "cargo metadata returned an unexpected format".to_string())?;

    let root = read("Cargo.toml")?;
    let workspace_package = toml_section(&root, "workspace.package")
        .ok_or_else(|| "Cargo.toml has no [workspace.package] section".to_string())?;
    let workspace_version = toml_string(workspace_package, "version")
        .ok_or_else(|| "[workspace.package] has no string version".to_string())?;
    if workspace_version != expected {
        return Err(format!(
            "workspace release version {workspace_version} does not match requested RELEASE_VERSION {expected}"
        ));
    }

    let workspace_dependencies = toml_section(&root, "workspace.dependencies")
        .ok_or_else(|| "Cargo.toml has no [workspace.dependencies] section".to_string())?;
    for dependency in INTERNAL_DEPENDENCIES {
        let line = toml_assignment(workspace_dependencies, dependency)
            .ok_or_else(|| format!("missing workspace dependency {dependency}"))?;
        let version = inline_table_string(line, "version")
            .ok_or_else(|| format!("workspace dependency {dependency} has no version"))?;
        if version != expected {
            return Err(format!(
                "workspace dependency {dependency} uses {version}, expected {expected}"
            ));
        }
    }

    for (crate_name, manifest_path) in RELEASE_CRATES {
        let marker = format!("{{\"name\":\"{crate_name}\",\"version\":\"{expected}\"");
        if !packages.contains(&marker) {
            return Err(format!(
                "cargo metadata does not list {crate_name} {expected} as a workspace package"
            ));
        }
        let manifest = read(manifest_path)?;
        let package = toml_section(&manifest, "package")
            .ok_or_else(|| format!("{manifest_path} has no [package] section"))?;
        let name = toml_string(package, "name")
            .ok_or_else(|| format!("{manifest_path} has no package name"))?;
        if name != *crate_name {
            return Err(format!(
                "{manifest_path} declares package {name}, expected {crate_name}"
            ));
        }
        validate_internal_dependencies(crate_name, &manifest, expected)?;

        let inherits_version = toml_assignment(package, "version.workspace")
            .is_some_and(|line| assignment_value(line) == Some("true"));
        let explicit_version = toml_string(package, "version");
        if !inherits_version && explicit_version != Some(expected) {
            return Err(format!(
                "{crate_name} must inherit the workspace version or explicitly use {expected}"
            ));
        }
        validate_publish_field(crate_name, package)?;
    }

    println!("Validated clean release state for rpi-* {expected}.");
    Ok(())
}

fn validate_internal_dependencies(
    crate_name: &str,
    manifest: &str,
    expected: &str,
) -> Result<(), String> {
    for line in manifest.lines() {
        let Some((key, _)) = line.trim().split_once('=') else {
            continue;
        };
        let dependency = key.trim();
        if !INTERNAL_DEPENDENCIES.contains(&dependency) && dependency != "rpi-cli" {
            continue;
        }
        if line.contains("workspace = true")
            || inline_table_string(line, "version") == Some(expected)
        {
            continue;
        }
        return Err(format!(
            "{crate_name} dependency {dependency} must inherit or use version {expected}"
        ));
    }
    Ok(())
}

fn validate_publish_field(crate_name: &str, package: &str) -> Result<(), String> {
    let Some(line) = toml_assignment(package, "publish") else {
        return Ok(());
    };
    let value = assignment_value(line).unwrap_or_default().replace(' ', "");
    if value == "false" || value == "[]" {
        return Err(format!("{crate_name} is marked publish = false"));
    }
    if value.starts_with('[') && !value.contains("\"crates-io\"") {
        return Err(format!("{crate_name} cannot be published to crates-io"));
    }
    Ok(())
}

fn dry_run() -> Result<(), String> {
    let workspace_root = env::current_dir()
        .map_err(|error| format!("could not determine the workspace root: {error}"))?;
    for (crate_name, manifest_path) in RELEASE_CRATES {
        let manifest = read(manifest_path)?;
        let args = dry_run_args(crate_name, &manifest, &workspace_root)?;
        let patch_count = args.iter().filter(|arg| *arg == "--config").count();
        println!(
            "Dry-running {crate_name} with full package verification and {} local release dependency patch(es).",
            patch_count
        );
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        command_status("cargo", &args)
            .map_err(|error| format!("cargo publish --dry-run failed for {crate_name}: {error}"))?;
    }
    Ok(())
}

fn dry_run_args(
    crate_name: &str,
    manifest: &str,
    workspace_root: &Path,
) -> Result<Vec<String>, String> {
    let mut args = vec![
        "publish".to_string(),
        "--dry-run".to_string(),
        "--locked".to_string(),
        "--registry".to_string(),
        "crates-io".to_string(),
        "-p".to_string(),
        crate_name.to_string(),
    ];
    for patch in local_dependency_patches(manifest, workspace_root)? {
        args.push("--config".to_string());
        args.push(patch);
    }
    Ok(args)
}

fn local_dependency_patches(manifest: &str, workspace_root: &Path) -> Result<Vec<String>, String> {
    let mut patches = Vec::new();
    for (dependency, manifest_path) in RELEASE_CRATES {
        if !manifest_declares_dependency(manifest, dependency) {
            continue;
        }
        let relative_dir = Path::new(manifest_path)
            .parent()
            .ok_or_else(|| format!("release manifest {manifest_path} has no parent directory"))?;
        let dependency_dir = workspace_root.join(relative_dir);
        let path = dependency_dir.to_string_lossy();
        #[cfg(windows)]
        let path = path.replace('\\', "/");
        let path = path
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        patches.push(format!("patch.crates-io.{dependency}.path=\"{path}\""));
    }
    Ok(patches)
}

fn manifest_declares_dependency(manifest: &str, dependency: &str) -> bool {
    manifest.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim() == dependency)
    })
}

fn publish(version: &str) -> Result<(), String> {
    // Re-check immediately before upload so a long test/check run cannot leave
    // publish operating on a different tree than the one initially validated.
    validate(version, false)?;
    command_output("curl", &["--version"])
        .map_err(|error| format!("curl is required before publishing any crate: {error}"))?;

    for (crate_name, _) in RELEASE_CRATES {
        match published_version(crate_name, version)? {
            PublishedVersion::Available => {
                println!(
                    "Skipping {crate_name} {version}: exact version already exists on crates.io."
                );
            }
            PublishedVersion::Yanked => {
                return Err(format!(
                    "{crate_name} {version} exists on crates.io but is yanked; refusing to treat it as a usable dependency"
                ));
            }
            PublishedVersion::Missing => {
                println!("Publishing {crate_name} {version}...");
                command_status(
                    "cargo",
                    &[
                        "publish",
                        "--locked",
                        "--registry",
                        "crates-io",
                        "-p",
                        crate_name,
                    ],
                )
                .map_err(|error| {
                    format!(
                        "cargo publish failed for {crate_name} {version}: {error}. Re-run task publish after resolving the error; published versions will be skipped"
                    )
                })?;
            }
        }
        wait_for_sparse_index(crate_name, version, Duration::from_secs(600))?;
    }
    Ok(())
}

fn probe(crate_name: &str, version: &str) -> Result<(), String> {
    command_output("curl", &["--version"])?;
    let state = published_version(crate_name, version)?;
    println!("crates.io API: {state:?}");
    wait_for_sparse_index(crate_name, version, Duration::ZERO)
}

#[derive(Debug, PartialEq, Eq)]
enum PublishedVersion {
    Missing,
    Available,
    Yanked,
}

fn published_version(crate_name: &str, version: &str) -> Result<PublishedVersion, String> {
    let url = format!(
        "https://crates.io/api/v1/crates/{}/{}",
        encode_url_component(crate_name),
        encode_url_component(version)
    );
    let response = http_get(&url)?;
    match response.status {
        200 => match json_bool_field(&response.body, "yanked") {
            Some(true) => Ok(PublishedVersion::Yanked),
            Some(false) => Ok(PublishedVersion::Available),
            None => Err(format!(
                "crates.io returned no yanked field for {crate_name} {version}"
            )),
        },
        404 => Ok(PublishedVersion::Missing),
        status => Err(format!(
            "crates.io returned HTTP {status} while checking {crate_name} {version}"
        )),
    }
}

fn wait_for_sparse_index(
    crate_name: &str,
    version: &str,
    max_wait: Duration,
) -> Result<(), String> {
    let url = format!("https://index.crates.io/{}", sparse_index_path(crate_name));
    let started = Instant::now();
    let mut attempt = 0usize;
    let last_result = loop {
        attempt += 1;
        let result = match http_get(&url) {
            Ok(response)
                if response.status == 200
                    && response.body.lines().any(|line| {
                        json_string_field(line, "vers").as_deref() == Some(version)
                            && json_bool_field(line, "yanked") != Some(true)
                    }) =>
            {
                println!("{crate_name} {version} is resolvable from the crates.io sparse index.");
                return Ok(());
            }
            Ok(response) => format!("sparse index returned HTTP {}", response.status),
            Err(error) => error,
        };
        if started.elapsed() >= max_wait {
            break result;
        }
        println!(
            "Waiting for crates.io index propagation ({crate_name} {version}, attempt {attempt})..."
        );
        let remaining = max_wait.saturating_sub(started.elapsed());
        thread::sleep(Duration::from_secs(10).min(remaining));
    };
    Err(format!(
        "{crate_name} {version} did not become resolvable from the crates.io index within 10 minutes (last result: {last_result}). Re-run task publish to resume safely"
    ))
}

struct HttpResponse {
    status: u16,
    body: String,
}

fn http_get(url: &str) -> Result<HttpResponse, String> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--header",
            &format!("User-Agent: {USER_AGENT}"),
            "--header",
            "Cache-Control: no-cache",
            "--write-out",
            "\n%{http_code}",
            url,
        ])
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("curl returned non-UTF-8 data for {url}"))?;
    let split = stdout
        .rfind('\n')
        .ok_or_else(|| format!("curl returned no HTTP status for {url}"))?;
    let status = stdout[split + 1..]
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("curl returned an invalid HTTP status for {url}"))?;
    Ok(HttpResponse {
        status,
        body: stdout[..split].to_string(),
    })
}

fn sparse_index_path(crate_name: &str) -> String {
    let name = crate_name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

fn encode_url_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn json_bool_field(json: &str, field: &str) -> Option<bool> {
    let value = json_value_start(json, field)?;
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_string_field(json: &str, field: &str) -> Option<String> {
    let value = json_value_start(json, field)?;
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn json_value_start<'a>(json: &'a str, field: &str) -> Option<&'a str> {
    let marker = format!("\"{field}\"");
    let rest = json.get(json.find(&marker)? + marker.len()..)?;
    let rest = rest.trim_start();
    rest.strip_prefix(':').map(str::trim_start)
}

fn read(path: impl AsRef<Path>) -> Result<String, String> {
    let path = path.as_ref();
    fs::read_to_string(path).map_err(|error| format!("could not read {}: {error}", path.display()))
}

fn toml_section<'a>(toml: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("[{name}]");
    let start = toml.find(&marker)? + marker.len();
    let rest = &toml[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

fn toml_assignment<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    section.lines().find(|line| {
        let line = line.trim();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim() == key)
    })
}

fn assignment_value(line: &str) -> Option<&str> {
    line.split_once('=').map(|(_, value)| value.trim())
}

fn toml_string<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    let value = assignment_value(toml_assignment(section, key)?)?;
    quoted_value(value)
}

fn inline_table_string<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let table = assignment_value(line)?;
    let marker = format!("{key} =");
    let rest = table
        .get(table.find(&marker)? + marker.len()..)?
        .trim_start();
    quoted_value(rest)
}

fn quoted_value(value: &str) -> Option<&str> {
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn command_output(program: &str, args: &[&str]) -> Result<Output, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn command_status(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} exited with {status}", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_sparse_index_paths() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("ab"), "2/ab");
        assert_eq!(sparse_index_path("abc"), "3/a/abc");
        assert_eq!(sparse_index_path("rpi-cli"), "rp/i-/rpi-cli");
    }

    #[test]
    fn parses_json_fields_with_or_without_spaces() {
        assert_eq!(
            json_string_field(r#"{"vers":"0.1.12"}"#, "vers").as_deref(),
            Some("0.1.12")
        );
        assert_eq!(
            json_bool_field(r#"{"yanked": false}"#, "yanked"),
            Some(false)
        );
    }

    #[test]
    fn parses_release_toml_shapes() {
        let toml = "[workspace.package]\nversion = \"0.1.12\"\n\n[workspace.dependencies]\nrpi-ai = { path = \"crates/rpi-ai\", version = \"0.1.12\" }\n";
        let package = toml_section(toml, "workspace.package").unwrap();
        assert_eq!(toml_string(package, "version"), Some("0.1.12"));
        let dependencies = toml_section(toml, "workspace.dependencies").unwrap();
        let line = toml_assignment(dependencies, "rpi-ai").unwrap();
        assert_eq!(inline_table_string(line, "version"), Some("0.1.12"));
    }

    #[test]
    fn dry_run_uses_full_verification_with_only_declared_local_dependencies() {
        let manifest = r#"
[dependencies]
rpi-telemetry = { workspace = true }
rpi-plugin-sdk = { version = "0.1.12" }
serde = { version = "1" }
"#;
        let args = dry_run_args("rpi-ai", manifest, Path::new("workspace")).unwrap();
        assert!(!args.iter().any(|arg| arg == "--no-verify"));
        assert_eq!(args.iter().filter(|arg| *arg == "--config").count(), 2);
        assert_eq!(
            args,
            [
                "publish",
                "--dry-run",
                "--locked",
                "--registry",
                "crates-io",
                "-p",
                "rpi-ai",
                "--config",
                "patch.crates-io.rpi-telemetry.path=\"workspace/crates/rpi-telemetry\"",
                "--config",
                "patch.crates-io.rpi-plugin-sdk.path=\"workspace/crates/rpi-plugin-sdk\"",
            ]
        );
    }
}
