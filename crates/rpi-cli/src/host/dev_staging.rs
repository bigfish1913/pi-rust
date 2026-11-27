//! Private dev-instance staging names, not process supervision or cleanup proof.
//! A PID is only diagnostic: same-process Harnesses and later PID reuse must not
//! share a directory that a delayed cleanup helper is allowed to remove.
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ATTEMPTS: usize = 8;

pub(crate) fn allocate(workspace: &Path, package: &str) -> Result<PathBuf, String> {
    allocate_with(workspace, package, std::process::id(), Uuid::new_v4)
}

/// Discard only an exclusively owned initial namespace, before DLL loading.
/// The caller must prove it is unshared/unbuilt; never use this for retirement.
/// No detached cleanup helper is launched on an error.
pub(crate) fn discard_unloaded(root: &Path) -> std::io::Result<()> {
    // The owner path was created as a real directory. A symlink or file at the
    // same name is a later replacement, not ours: refuse instead of removing a
    // caller-chosen target or silently unlinking somebody else's data.
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to remove a replaced staging path that is not the owned directory",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn allocate_with(
    workspace: &Path,
    package: &str,
    pid: u32,
    mut next: impl FnMut() -> Uuid,
) -> Result<PathBuf, String> {
    let parent = workspace.join(".rpi/extensions/.dev");
    std::fs::create_dir_all(&parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let package: String = package
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    for _ in 0..ATTEMPTS {
        let candidate = parent.join(format!("{package}-{pid}-{}", next()));
        // Never create_dir_all the owner root: a collision is somebody else's
        // namespace, not permission to adopt it or overwrite its generations.
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create {}: {error}", candidate.display())),
        }
    }
    Err("could not allocate an exclusive dev staging namespace".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_package_and_pid_get_independent_owner_directories() {
        let root = tempfile::tempdir().unwrap();
        let first = allocate(root.path(), "same-package").unwrap();
        let second = allocate(root.path(), "same-package").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), second.parent());
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn collision_retries_without_adopting_or_overwriting_an_existing_owner() {
        let root = tempfile::tempdir().unwrap();
        let collision = Uuid::new_v4();
        let first = allocate_with(root.path(), "demo", 123, || collision).unwrap();
        let sentinel = first.join("keep-existing-data");
        std::fs::write(&sentinel, b"existing owner").unwrap();
        let fresh = Uuid::new_v4();
        let mut candidates = [collision, fresh].into_iter();
        let second =
            allocate_with(root.path(), "demo", 123, || candidates.next().unwrap()).unwrap();
        assert_ne!(first, second);
        assert!(second.is_dir());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing owner");
    }

    #[test]
    fn exhausting_collisions_fails_without_touching_the_existing_owner() {
        let root = tempfile::tempdir().unwrap();
        let collision = Uuid::new_v4();
        let existing = allocate_with(root.path(), "demo", 123, || collision).unwrap();
        let sentinel = existing.join("keep-existing-data");
        std::fs::write(&sentinel, b"no fallback to PID-only namespace").unwrap();
        let mut attempts = 0;
        assert!(allocate_with(root.path(), "demo", 123, || {
            attempts += 1;
            collision
        })
        .is_err());
        assert_eq!(attempts, ATTEMPTS);
        assert_eq!(
            std::fs::read(sentinel).unwrap(),
            b"no fallback to PID-only namespace"
        );
    }

    #[test]
    fn discard_refuses_a_replaced_file_without_unlinking_it() {
        let root = tempfile::tempdir().unwrap();
        let own = root.path().join("owner");
        std::fs::write(&own, b"replacement, not our directory").unwrap();
        assert!(discard_unloaded(&own).is_err());
        assert_eq!(
            std::fs::read(&own).unwrap(),
            b"replacement, not our directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discard_refuses_a_replaced_symlink_and_keeps_its_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("not-owned");
        std::fs::write(&sentinel, b"outside the dev namespace").unwrap();
        let own = root.path().join("owner");
        std::os::unix::fs::symlink(outside.path(), &own).unwrap();
        assert!(discard_unloaded(&own).is_err());
        assert!(std::fs::symlink_metadata(&own)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"outside the dev namespace"
        );
    }

    #[test]
    fn discard_is_idempotent_for_an_absent_owner_path() {
        let root = tempfile::tempdir().unwrap();
        discard_unloaded(&root.path().join("already-gone")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_colliding_owner_symlink_is_not_followed_or_adopted() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("not-owned-by-the-dev-instance");
        std::fs::write(&sentinel, b"leave this directory alone").unwrap();
        let identity = Uuid::new_v4();
        let parent = root.path().join(".rpi/extensions/.dev");
        std::fs::create_dir_all(&parent).unwrap();
        let collision = parent.join(format!("demo-123-{identity}"));
        std::os::unix::fs::symlink(outside.path(), &collision).unwrap();
        assert!(allocate_with(root.path(), "demo", 123, || identity).is_err());
        assert!(std::fs::symlink_metadata(&collision)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read(sentinel).unwrap(),
            b"leave this directory alone"
        );
    }
}
