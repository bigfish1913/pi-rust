use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

fn file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    path
}
#[test]
fn freeze_is_readonly_and_stages_frozen_bytes_into_distinct_private_inodes() {
    let source = tempfile::tempdir().unwrap();
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let path = file(source.path(), "fixture.dylib", b"frozen {{contents}}");
    let snapshot = NativeExtensionArtifacts::freeze(std::slice::from_ref(&path)).unwrap();
    assert_eq!(fs::read_dir(source.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    // Neither replacement nor disappearance of the selected source changes code
    // subsequently staged. No mutable path is reopened by owned assembly.
    fs::write(&path, b"replacement").unwrap();
    fs::remove_file(&path).unwrap();
    let id = AgentId::new();
    let first = snapshot.stage(root.path(), id).unwrap();
    let second = snapshot.stage(root.path(), id).unwrap();
    let other = snapshot.stage(root.path(), AgentId::new()).unwrap();
    let mut identities = BTreeSet::new();
    for staged in [&first, &second, &other] {
        let path = &staged.paths()[0];
        assert_eq!(fs::read(path).unwrap(), b"frozen {{contents}}");
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o400);
        assert!(identities.insert((metadata.dev(), metadata.ino())));
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        assert!(path.starts_with(fs::canonicalize(root.path()).unwrap()));
    }
    let path = first.paths()[0].clone();
    let guard = first.clone();
    drop(first);
    assert!(path.exists());
    drop(guard);
    assert!(!path.exists());
    drop(second);
    drop(other);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}
#[test]
fn entire_artifact_set_rejects_relative_missing_directory_alias_and_hardlink() {
    let root = tempfile::tempdir().unwrap();
    let path = file(root.path(), "source", b"bytes");
    let alias = root.path().join("alias");
    symlink(&path, &alias).unwrap();
    let link = root.path().join("hardlink");
    fs::hard_link(&path, &link).unwrap();
    for bad in [
        PathBuf::from("relative"),
        root.path().join("missing"),
        root.path().to_path_buf(),
        alias,
        link,
    ] {
        assert!(NativeExtensionArtifacts::freeze(&[path.clone(), bad]).is_err());
    }
    assert!(
        NativeExtensionArtifacts::freeze(&vec![path; MAX_NATIVE_EXTENSION_ARTIFACTS + 1]).is_err()
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    assert!(NativeExtensionArtifacts::freeze(&[]).is_ok());
}
#[test]
fn frozen_identity_includes_order_canonical_provenance_and_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let first = file(root.path(), "one", b"secret {{one}}");
    let second = file(root.path(), "two", b"two");
    let paths = [first.clone(), second.clone()];
    let frozen = NativeExtensionArtifacts::freeze(&paths).unwrap();
    assert_eq!(
        frozen.identity(),
        NativeExtensionArtifacts::freeze(&paths).unwrap().identity()
    );
    assert_ne!(
        frozen.identity(),
        NativeExtensionArtifacts::freeze(&[second, first.clone()])
            .unwrap()
            .identity()
    );
    let provenance = frozen.provenance();
    assert_eq!(provenance[0].0, fs::canonicalize(&first).unwrap());
    assert_eq!(
        provenance[0].1,
        format!("{:x}", Sha256::digest(b"secret {{one}}"))
    );
    fs::write(&first, b"new {{one}}").unwrap();
    assert_ne!(
        frozen.identity(),
        NativeExtensionArtifacts::freeze(&paths).unwrap().identity()
    );
    assert_eq!(format!("{frozen:?}"), "NativeExtensionArtifacts { .. }");
}
#[test]
fn artifact_budgets_reject_empty_oversized_and_total_bytes_without_large_allocations() {
    let root = tempfile::tempdir().unwrap();
    let empty = file(root.path(), "empty", b"");
    assert!(NativeExtensionArtifacts::freeze(&[empty]).is_err());
    let oversized = file(root.path(), "oversized", b"x");
    OpenOptions::new()
        .write(true)
        .open(&oversized)
        .unwrap()
        .set_len(MAX_NATIVE_EXTENSION_BYTES + 1)
        .unwrap();
    assert!(NativeExtensionArtifacts::freeze(&[oversized]).is_err());
    let first = file(root.path(), "one", b"123");
    let second = file(root.path(), "two", b"456");
    assert!(
        NativeExtensionArtifacts::freeze_bounded(&[first.clone(), second.clone()], 3, 6).is_ok()
    );
    assert!(NativeExtensionArtifacts::freeze_bounded(&[first.clone(), second], 3, 5).is_err());
    assert!(NativeExtensionArtifacts::freeze_bounded(&[first], 2, 6).is_err());
}
#[test]
fn staging_requires_explicit_private_real_directory_without_changing_root_permissions() {
    let root = tempfile::tempdir().unwrap();
    let source = file(root.path(), "source", b"code");
    let frozen = NativeExtensionArtifacts::freeze(std::slice::from_ref(&source)).unwrap();
    let id = AgentId::new();
    assert!(frozen.stage(Path::new("relative"), id).is_err());
    assert!(frozen.stage(&source, id).is_err());
    let alias = root.path().join("alias");
    let target = root.path().join("private");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &alias).unwrap();
    assert!(frozen.stage(&alias, id).is_err());
    assert!(frozen.stage(&alias.join(""), id).is_err());
    assert!(frozen.stage(&alias.join("."), id).is_err());
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(frozen.stage(&target, id).is_err());
    assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}
