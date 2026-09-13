use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    agent_dir: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create test directory");
        let cwd = temp.path().join("project");
        let agent_dir = temp.path().join("agent");
        let package_dir = temp.path().join("gate-package");
        let marker = temp.path().join("extension-load-count");
        fs::create_dir_all(&cwd).expect("create project directory");
        fs::create_dir_all(&agent_dir).expect("create agent directory");
        fs::create_dir_all(&package_dir).expect("create package directory");

        let package_json = serde_json::json!({
            "name": "rpi-package-gate-test",
            "version": "1.0.0",
            "rpi": { "extensions": ["extension.js"] }
        });
        fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&package_json).expect("serialize package manifest"),
        )
        .expect("write package manifest");

        let marker_literal = serde_json::to_string(&marker.to_string_lossy())
            .expect("serialize marker path for JavaScript");
        let extension = format!(
            "import fs from 'node:fs';\n\
             export default () => {{\n\
               const marker = {marker_literal};\n\
               const count = fs.existsSync(marker) ? Number(fs.readFileSync(marker, 'utf8')) : 0;\n\
               fs.writeFileSync(marker, String(count + 1));\n\
             }};\n"
        );
        fs::write(package_dir.join("extension.js"), extension).expect("write extension");

        let settings = serde_json::json!({
            "packages": [package_dir.to_string_lossy()]
        });
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec(&settings).expect("serialize settings"),
        )
        .expect("write settings");

        Self {
            _temp: temp,
            cwd,
            agent_dir,
            marker,
        }
    }

    fn run(&self, extra_args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rpi"))
            .current_dir(&self.cwd)
            .env("RPI_CODING_AGENT_DIR", &self.agent_dir)
            .env("RPI_DISABLE_UPDATE_CHECK", "1")
            .args([
                "--print",
                "--no-session",
                "--provider",
                "anthropic",
                "--model",
                "claude-sonnet-5",
                "--api-key",
                "package-gate-test-key",
            ])
            .args(extra_args)
            .output()
            .expect("run rpi")
    }

    fn clear_marker(&self) {
        match fs::remove_file(&self.marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove marker: {error}"),
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "rpi failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn node_is_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn marker_count(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

#[test]
fn configured_pi_packages_require_the_explicit_startup_gate() {
    let fixture = Fixture::new();

    let output = fixture.run(&[]);
    assert_success(&output);
    assert_eq!(marker_count(&fixture.marker), None);

    let output = fixture.run(&["--enable-pi-packages", "--no-extensions"]);
    assert_success(&output);
    assert_eq!(marker_count(&fixture.marker), None);

    if !node_is_available() {
        return;
    }

    fixture.clear_marker();
    let output = fixture.run(&["--enable-pi-packages"]);
    assert_success(&output);
    assert_eq!(marker_count(&fixture.marker).as_deref(), Some("1"));
}
