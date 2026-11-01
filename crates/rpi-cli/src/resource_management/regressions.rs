use super::*;
fn put(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}
fn install(kind: Kind, name: &str, source: PathBuf) -> Options {
    Options {
        op: "install".into(),
        kind: Some(kind),
        name: Some(name.into()),
        source: Some(source),
        scope: Some(Scope::Project),
        ..Default::default()
    }
}
#[test]
fn unknown_package_metadata_is_never_discarded() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("skills/demo/SKILL.md"), b"body");
    fs::create_dir(&root).unwrap();
    for bytes in [
        r#"{ "unexpected-owner-data": {"keep":"exact bytes"} }"#,
        r#"{"packages":[{"name":"other","version":"1","source":"local","cargo_package":"other","artifacts":[],"owner-private":"keep"}]}"#,
    ] {
        fs::write(root.join("packages.json"), bytes).unwrap();
        assert!(mutate(&root, &install(Kind::Package, "demo", source.clone())).is_err());
        assert_eq!(
            fs::read(root.join("packages.json")).unwrap(),
            bytes.as_bytes()
        );
        assert!(!root.join("packages/demo").exists());
        assert!(!root.join(RECEIPTS).exists());
    }
}
#[test]
fn explicit_parent_source_is_normalized_but_owned_traversal_stays_forbidden() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    fs::create_dir(d.path().join("nested")).unwrap();
    put(&source.join("SKILL.md"), b"body");
    let addressed = d.path().join("nested/../source");
    let result = mutate(&root, &install(Kind::Skill, "demo", addressed)).unwrap();
    assert_eq!(result["outcome"], "installed");
    assert_eq!(
        fs::read(root.join("skills/demo/SKILL.md")).unwrap(),
        b"body"
    );
    assert!(relative(Path::new("skills/../other")).is_err());
    assert!(checked_path(&root.join("skills/../other")).is_err());
}
#[cfg(windows)]
#[test]
fn windows_extended_case_and_ancestor_source_overlap_rejected_before_staging() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("SKILL.md"), b"body");
    mutate(&root, &install(Kind::Skill, "demo", source)).unwrap();
    let before = fs::read(root.join(RECEIPTS)).unwrap();
    let installed = fs::canonicalize(root.join("skills/demo")).unwrap();
    for source in [
        installed.clone(),
        PathBuf::from(installed.to_string_lossy().to_uppercase()),
        fs::canonicalize(&root).unwrap(),
        fs::canonicalize(d.path()).unwrap(),
    ] {
        let e = mutate(&root, &install(Kind::Skill, "overlap", source)).unwrap_err();
        assert!(e.contains("overlap"), "{e}");
        assert_eq!(before, fs::read(root.join(RECEIPTS)).unwrap());
        assert!(!root.join("skills/overlap").exists());
        assert!(!fs::read_dir(&root).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".resource-txn-")));
    }
    let outside = d.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let alias = d.path().join("junction");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(status.status.success());
    // Normalizing '..' must not erase a reparse component before checking it.
    assert!(explicit_source(&alias.join("../source")).is_err());
    fs::remove_dir(alias).unwrap();
}
#[test]
fn oversized_success_is_rejected_before_commit_with_previous_state_intact() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("SKILL.md"), b"body");
    mutate(&root, &install(Kind::Skill, "demo", source.clone())).unwrap();
    let old = fs::read(root.join(RECEIPTS)).unwrap();
    let old_content = inventory(&root.join("skills/demo")).unwrap();
    for n in 0..1250 {
        put(&source.join(format!("{n:04}-{}", "x".repeat(120))), b"x");
    }
    let mut inv = inventory(&source).unwrap();
    let mut r = Receipt {
        kind: Kind::Skill,
        name: "demo".into(),
        source: fs::canonicalize(&source).unwrap(),
        version: None,
        target: Kind::Skill.target("demo"),
        inventory: inv.clone(),
    };
    // Choose a receipt that fits the metadata limit but whose full public
    // success envelope exceeds the precommitted result budget.
    loop {
        r.inventory = inv.clone();
        if json_bytes(&Receipts {
            resources: vec![r.clone()],
        })
        .is_ok()
        {
            break;
        }
        let index = inv
            .files
            .iter()
            .rposition(|f| f.path != Path::new("SKILL.md"))
            .unwrap();
        let file = inv.files.remove(index);
        fs::remove_file(source.join(file.path)).unwrap();
    }
    let mut o = install(Kind::Skill, "demo", source);
    o.op = "update".into();
    let e = mutate(&root, &o).unwrap_err();
    assert!(e.contains("success result"), "{e}");
    assert_eq!(old, fs::read(root.join(RECEIPTS)).unwrap());
    assert_eq!(old_content, inventory(&root.join("skills/demo")).unwrap());
    assert!(!root.join(JOURNAL).exists());
}
