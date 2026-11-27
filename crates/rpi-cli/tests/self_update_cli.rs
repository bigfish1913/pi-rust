#![cfg(unix)]

use std::{fs, os::unix::fs::symlink, process::Command};

#[test]
fn homebrew_update_leaves_the_keg_to_brew() {
    let root = tempfile::tempdir().unwrap();
    let keg = root.path().join("Cellar/rpi/1.2.3_1");
    fs::create_dir_all(keg.join("bin")).unwrap();
    let executable = keg.join("bin/rpi");
    fs::copy(env!("CARGO_BIN_EXE_rpi"), &executable).unwrap();

    let link = root.path().join("bin/rpi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink("../Cellar/rpi/1.2.3_1/bin/rpi", &link).unwrap();

    let agent = root.path().join("agent");
    let run_update = |args: &[&str]| {
        Command::new(&link)
            .env_clear()
            .env("RPI_CODING_AGENT_DIR", &agent)
            .current_dir(root.path())
            .arg("update")
            .args(args)
            .output()
            .unwrap()
    };

    let direct = run_update(&["--offline"]);
    assert!(direct.status.success(), "{direct:?}");
    assert!(String::from_utf8_lossy(&direct.stdout).contains("offline mode"));

    fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    for args in [
        &["--offline"][..],
        &["preview", "--force", "--build", "--offline"][..],
    ] {
        let managed = run_update(args);
        assert_eq!(managed.status.code(), Some(1), "{managed:?}");
        assert!(String::from_utf8_lossy(&managed.stderr).contains("brew upgrade rpi"));
    }
    assert!(!agent.join("self-update").exists());
}
