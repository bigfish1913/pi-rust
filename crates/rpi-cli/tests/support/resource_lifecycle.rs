// Native lifecycle probes reuse the parent test module's bounded RPC driver.
use super::*;

fn session_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        if path.is_file() {
            // Windows denies reads of the live writer's advisory lock range.
            // Record its presence; native lock files contain no session data.
            let bytes = if path.extension().is_some_and(|e| e == "lock") {
                Vec::new()
            } else {
                std::fs::read(path).unwrap()
            };
            files.insert(path.to_path_buf(), bytes);
        } else if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                walk(&entry.unwrap().path(), files);
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    walk(&root.join("sessions"), &mut files);
    files
}

async fn rejected(root: &Path, args: &[&str], expected: &str) {
    let before = session_files(root);
    let output = tokio::time::timeout(
        Duration::from_secs(25),
        command(root).args(args).stdin(Stdio::null()).output(),
    )
    .await
    .expect("rejected startup hung")
    .unwrap();
    assert_eq!(
        session_files(root),
        before,
        "rejected preflight must not repair/create/claim session files"
    );
    assert!(
        !output.status.success(),
        "rejected startup reached ready: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"type\":\"ready\""));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn fixture_model_config(root: &Path) {
    put(root.join("agent/models.json"),json!({"providers":{"fixture":{"api":"openai-completions","apiKey":"fixture-only","baseUrl":"http://127.0.0.1:1","models":[{"id":"test"}]}}}).to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_reopen_never_registers_native_code_and_public_apis_cannot_erase_binding() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture_model_config(root);
    let suffix = std::env::consts::DLL_EXTENSION;
    let unwanted = root.join(format!(".rpi/extensions/unwanted.{suffix}"));
    std::fs::create_dir_all(unwanted.parent().unwrap()).unwrap();
    std::fs::copy(&fixture.library, &unwanted).unwrap();
    put(
        root.join("none.json"),
        r#"{"version":1,"tools":{"mode":"none"}}"#,
    );
    put(
        root.join("different.json"),
        json!({"version":1,"extensions":[fixture.library],"tools":{"mode":"none"}}).to_string(),
    );
    let mut host = Host::start(root, "none.json", "bound").await;
    for key in [
        "rpi.resource-selection.v1",
        "rpi.resource-inventory.v1",
        "rpi.resource-selection.v2",
    ] {
        for value in [Value::Null, json!({"forged":true})] {
            let response = host
                .request(
                    "append_entry",
                    json!({"customType":"rpi.value","data":{"key":key,"value":value}}),
                )
                .await;
            assert_eq!(response["success"], false, "{response}");
            assert!(response["error"].as_str().unwrap().contains("host-owned"));
        }
    }
    assert_eq!(
        host.request(
            "append_entry",
            json!({"customType":"rpi.value","data":{"key":"user.preference","value":1}})
        )
        .await["success"],
        true
    );
    assert_eq!(
        host.request(
            "append_entry",
            json!({"customType":"ordinary","data":{"n":1}})
        )
        .await["success"],
        true
    );
    rejected(root, &["--session-id", "bound"], "selection differs").await;
    host.stop().await;
    let transcript = session_files(root)
        .into_keys()
        .find(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && !p.to_string_lossy().ends_with(".progress.jsonl")
        })
        .unwrap();
    let original = std::fs::read_to_string(&transcript).unwrap();
    put(&transcript, format!("{original}{{unacknowledged tail"));
    let log = root.join("registrations.log");
    assert!(!log.exists());
    // Both automatic discovery and a differently selected explicit DLL would
    // register real code without the preflight. Neither may run on rejection.
    rejected(
        root,
        &["--session-id", "bound", "--verbose"],
        "selection differs",
    )
    .await;
    assert!(
        !log.exists(),
        "automatic extension registered before rejecting missing config"
    );
    rejected(
        root,
        &[
            "--session-id",
            "bound",
            "--resource-config",
            "different.json",
            "--verbose",
        ],
        "selection differs",
    )
    .await;
    assert!(
        !log.exists(),
        "different explicit extension registered before rejection"
    );
    let mut reopened = Host::start(root, "none.json", "bound").await;
    let history = reopened
        .request("get_session_branch", json!({"customTypes":["rpi.value"]}))
        .await;
    assert_eq!(history["success"], true, "{history}");
    assert!(history.to_string().contains("user.preference"));
    reopened.stop().await;
    assert!(!std::fs::read_to_string(&transcript)
        .unwrap()
        .contains("{unacknowledged tail"));
    // Exact duplicate sequences are recovered logically during preflight, not
    // republished until the ordinary accepted writer opens the native storage.
    let valid = std::fs::read_to_string(&transcript).unwrap();
    let duplicate = valid.lines().last().unwrap().to_string();
    put(&transcript, format!("{valid}{duplicate}\n"));
    rejected(root, &["--session-id", "bound"], "selection differs").await;
    Host::start(root, "none.json", "bound").await.stop().await;
    assert_eq!(
        std::fs::read_to_string(&transcript)
            .unwrap()
            .lines()
            .filter(|line| *line == duplicate)
            .count(),
        1
    );
    // Same declared path but changed executable bytes is rejected before dlopen,
    // even if the platform loader would accept a harmless trailing PE/ELF byte.
    let selected = root.join(format!("selected.{suffix}"));
    std::fs::copy(&fixture.library, &selected).unwrap();
    put(
        root.join("selected.json"),
        json!({"version":1,"extensions":[selected],"tools":{"mode":"none"}}).to_string(),
    );
    Host::start(root, "selected.json", "selected")
        .await
        .stop()
        .await;
    fixture.assert_registrations(&log);
    put(&log, "");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&selected)
        .unwrap()
        .write_all(&[0])
        .unwrap();
    rejected(
        root,
        &[
            "--session-id",
            "selected",
            "--resource-config",
            "selected.json",
        ],
        "artifact inventory changed",
    )
    .await;
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
    // Validate exact mode before even looking for a Cargo project/dev build.
    let output = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(root)
            .env("RPI_CODING_AGENT_DIR", root.join("agent"))
            .args(["dev", "--mode", "rpc", "--resource-config", "none.json"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be combined with dev"));
}

#[tokio::test]
async fn legacy_torn_tail_still_recovers_on_accepted_restore() {
    async fn start(root: &Path) -> Host {
        let error = root.join("legacy.stderr");
        let mut child = command(root)
            .args(["--session-id", "legacy-tail"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&error).unwrap())
            .spawn()
            .unwrap();
        let mut host = Host {
            error,
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
        };
        assert_eq!(
            host.until(|v| v["type"] == "ready").await["resourceState"]["enabled"],
            false
        );
        host
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture_model_config(root);
    let mut host = start(root).await;
    assert_eq!(
        host.request(
            "append_entry",
            json!({"customType":"ordinary","data":{"marker":"legacy-preserved"}})
        )
        .await["success"],
        true
    );
    host.stop().await;
    let path = session_files(root)
        .into_keys()
        .find(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && !p.to_string_lossy().ends_with(".progress.jsonl")
        })
        .unwrap();
    let valid = std::fs::read_to_string(&path).unwrap();
    put(&path, format!("{valid}{{legacy torn"));
    start(root).await.stop().await;
    let repaired = std::fs::read_to_string(path).unwrap();
    assert!(repaired.contains("legacy-preserved"));
    assert!(!repaired.contains("{legacy torn"));
    assert!(repaired.ends_with('\n'));
}

async fn directory_alias(link: &Path, target: &Path) {
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(windows)]
    {
        // Directory junctions do not require Windows symlink privileges.
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link.to_string_lossy().replace('/', "\\"))
                .arg(target.to_string_lossy().replace('/', "\\"))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            out.status.success(),
            "junction: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test]
async fn package_identities_and_canonical_resources_cannot_escape() {
    for name in [".", ".."] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture_model_config(root);
        put(root.join(".rpi/packages.json"),json!({"packages":[{"name":name,"version":"1","source":"fixture","cargo_package":"fixture","artifacts":[]}]}).to_string());
        put(
            root.join("invalid.json"),
            json!({"version":1,"main_package":{"name":name},"tools":{"mode":"none"}}).to_string(),
        );
        rejected(
            root,
            &["--no-session", "--resource-config", "invalid.json"],
            "invalid main package name",
        )
        .await;
    }
    for kind in ["root", "artifact", "instruction", "skill"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture_model_config(root);
        let package = root.join(".rpi/packages/safe");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let filename = format!("escape.{}", std::env::consts::DLL_EXTENSION);
        put(outside.join(&filename), "must never dlopen");
        put(outside.join("text.md"), "outside instructions");
        skill(outside.join("escape/SKILL.md"), "escape", "outside skill");
        let artifacts = if kind == "artifact" {
            json!([filename])
        } else {
            json!([])
        };
        let instructions = if kind == "instruction" {
            json!(["linked/text.md"])
        } else {
            json!([])
        };
        put(root.join(".rpi/packages.json"),json!({"packages":[{"name":"safe","version":"recorded-version","source":"fixture-identity","cargo_package":"fixture-cargo","artifacts":artifacts}]}).to_string());
        match kind {
            "root" => directory_alias(&package, &outside).await,
            "artifact" => directory_alias(&package.join("extensions"), &outside).await,
            "instruction" => directory_alias(&package.join("linked"), &outside).await,
            "skill" => directory_alias(&package.join("skills/nested"), &outside).await,
            _ => unreachable!(),
        }
        put(root.join("invalid.json"),json!({"version":1,"main_package":{"name":"safe","instructions":instructions},"tools":{"mode":"none"}}).to_string());
        rejected(
            root,
            &["--no-session", "--resource-config", "invalid.json"],
            "escapes",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_fixture_builds_are_isolated_across_processes() {
    const CHILD: &str = "RPI_RESOURCE_FIXTURE_CHILD_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let fixture = Fixture::build().await;
        fixture_model_config(&root);
        put(
            root.join("fixture.json"),
            json!({"version":1,"extensions":[fixture.library],"tools":{"mode":"none"}}).to_string(),
        );
        let mut host = Host::start(&root, "fixture.json", "fresh-source").await;
        assert_eq!(
            host.state().await["loadedExtensions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        host.stop().await;
        fixture.assert_registrations(&root.join("registrations.log"));
        put(
            root.join("proof.json"),
            json!({"library":fixture.library,"sourceDigest":fixture.source_digest}).to_string(),
        );
        return;
    }
    async fn child(root: PathBuf) -> Value {
        std::fs::create_dir_all(&root).unwrap();
        let log_path = root.join("child.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut process = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resource_lifecycle::source_fixture_builds_are_isolated_across_processes",
                "--nocapture",
            ])
            .env(CHILD, &root)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = match tokio::time::timeout(Duration::from_secs(150), process.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                process.kill().await.unwrap();
                process.wait().await.unwrap();
                panic!("fixture child timed out");
            }
        };
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(log_path).unwrap()
        );
        let proof: Value =
            serde_json::from_slice(&std::fs::read(root.join("proof.json")).unwrap()).unwrap();
        assert!(
            !Path::new(proof["library"].as_str().unwrap()).exists(),
            "isolated fixture target was not cleaned up"
        );
        proof
    }
    let root = tempfile::tempdir().unwrap();
    let (a, b) = tokio::join!(child(root.path().join("a")), child(root.path().join("b")));
    assert_ne!(a["library"], b["library"]);
    assert_eq!(a["sourceDigest"], b["sourceDigest"]);
}
