use super::*;
use crate::host::AgentProfileFile;
use serde_json::json;
use std::{os::unix::fs::symlink, path::Path};

fn agent(root: &Path, name: &str) -> HostStartupAgent {
    let root = root.canonicalize().unwrap();
    serde_json::from_value::<AgentProfileFile>(json!({
        "agent": {
            "id": AgentId::new(), "name": name, "cwd": root,
            "session": {"id": name, "path": root.join(format!("{name}.jsonl"))},
            "model": {"provider": "fixture", "model": "host-fixture"}, "tools": []
        },
        "runtime": {"providers": {"fixture": {
            "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
            "apiKey": "private-profile-key",
            "models": [{"id": "host-fixture", "contextWindow": 1024, "maxTokens": 128}]
        }}},
        "toolEnvironment": {"HOME": root, "PATH": "/usr/bin:/bin"},
        "shell": "/bin/sh", "thinking": "off"
    }))
    .unwrap()
    .into_startup()
    .unwrap()
}

#[tokio::test]
async fn frozen_profiles_prepare_owned_snapshots_without_sessions_or_catalogs() {
    let root = tempfile::tempdir().unwrap();
    let input = agent(root.path(), "one");
    let expected = input.spec.clone();
    let set = TrustedProfileSet::freeze(vec![input]).await.unwrap();
    assert_eq!(set.references().len(), 1);
    let reference = set.references().next().unwrap();
    assert_eq!(reference.agent_id, expected.id);
    let (first, second) = tokio::join!(set.prepare(reference), set.prepare(reference));
    let mut first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.reference, reference);
    assert_eq!(first.spec, expected);
    assert_eq!(second.spec, expected);
    first.spec.name = "changed-owned-copy".into();
    assert_eq!(set.prepare(reference).await.unwrap().spec, expected);
    drop(set);
    // Prepared values outlive the private source and each other. The lower
    // binding tests also assert distinct environment Arcs/cleanup ownership.
    drop(first);
    assert_eq!(second.spec, expected);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn foreign_or_previous_freeze_versions_never_resolve_to_current_inputs() {
    let root = tempfile::tempdir().unwrap();
    let first = agent(root.path(), "one");
    let spec = first.spec.clone();
    let first = TrustedProfileSet::freeze(vec![first]).await.unwrap();
    let old = first.references().next().unwrap();
    let mut replacement = agent(root.path(), "one");
    replacement.spec = spec;
    let current = TrustedProfileSet::freeze(vec![replacement]).await.unwrap();
    let reference = current.references().next().unwrap();
    assert_ne!(old.version, reference.version);
    for invalid in [
        old,
        ProfileRef {
            agent_id: AgentId::new(),
            ..reference
        },
        ProfileRef {
            version: ProfileVersion::new(),
            ..reference
        },
    ] {
        assert!(matches!(
            current.prepare(invalid).await,
            Err(BackendError::Binding("profile reference is not permitted"))
        ));
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn freeze_rejects_duplicate_ids_sessions_canonical_paths_and_hardlinks() {
    for case in ["agent", "session", "path", "parent-alias", "hardlink"] {
        let root = tempfile::tempdir().unwrap();
        let first = agent(root.path(), "one");
        let mut second = agent(root.path(), "two");
        let first_path = first.spec.session.as_ref().unwrap().path.clone();
        let second_session = second.spec.session.as_mut().unwrap();
        match case {
            "agent" => second.spec.id = first.spec.id,
            "session" => second_session.id = "one".into(),
            "path" => second_session.path = first_path.clone(),
            "parent-alias" => {
                let alias = root.path().join("alias");
                symlink(root.path(), &alias).unwrap();
                second_session.path = alias.join("one.jsonl");
            }
            "hardlink" => {
                std::fs::write(&first_path, b"preserved-existing-history\n").unwrap();
                std::fs::hard_link(&first_path, &second_session.path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                TrustedProfileSet::freeze(vec![first, second]).await,
                Err(BackendError::Binding(_))
            ),
            "{case}"
        );
        if case == "hardlink" {
            assert_eq!(
                std::fs::read(&first_path).unwrap(),
                b"preserved-existing-history\n"
            );
        } else {
            assert!(!first_path.exists());
        }
    }
}

#[tokio::test]
async fn freeze_rejects_nonregular_final_paths_and_unavailable_parents_without_repair() {
    for case in ["symlink", "broken-link", "directory", "missing-parent"] {
        let root = tempfile::tempdir().unwrap();
        let mut input = agent(root.path(), "one");
        let session = input.spec.session.as_mut().unwrap();
        match case {
            "symlink" => {
                let target = root.path().join("existing");
                std::fs::write(&target, b"preserved-target").unwrap();
                symlink(target, &session.path).unwrap();
            }
            "broken-link" => symlink(root.path().join("absent"), &session.path).unwrap(),
            "directory" => std::fs::create_dir(&session.path).unwrap(),
            "missing-parent" => session.path = root.path().join("absent/one.jsonl"),
            _ => unreachable!(),
        }
        let before: BTreeSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(matches!(
            TrustedProfileSet::freeze(vec![input]).await,
            Err(BackendError::Binding(_))
        ));
        let after: BTreeSet<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "{case}");
        if case == "symlink" {
            assert_eq!(
                std::fs::read(root.path().join("existing")).unwrap(),
                b"preserved-target"
            );
        }
    }
}

#[tokio::test]
async fn freeze_preflights_all_profiles_before_publishing_refs_or_mutating_any_session() {
    for case in [
        "model",
        "environment",
        "grant",
        "metadata",
        "mode",
        "limits",
    ] {
        let root = tempfile::tempdir().unwrap();
        let first = agent(root.path(), "one");
        let mut invalid = agent(root.path(), "two");
        match case {
            "model" => invalid.spec.launch.model.as_mut().unwrap().model = "host".into(),
            "environment" => {
                invalid.profile.tool_environment.remove("HOME");
            }
            "grant" => invalid.spec.launch.tools = vec!["bash".into()],
            "metadata" => invalid.spec.parent_id = Some(invalid.spec.id),
            "limits" => invalid.limits.max_input_bytes = 0,
            "mode" => {
                invalid.spec.mode = AgentMode::Mini;
                invalid.spec.session = None;
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                TrustedProfileSet::freeze(vec![first, invalid]).await,
                Err(BackendError::Binding(_))
            ),
            "{case}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0, "{case}");
    }
}

#[tokio::test]
async fn profile_count_is_bounded_and_empty_sets_grant_nothing() {
    let root = tempfile::tempdir().unwrap();
    let set = TrustedProfileSet::freeze(Vec::new()).await.unwrap();
    assert_eq!(set.references().len(), 0);
    let too_many = (0..=MAX_TRUSTED_PROFILES)
        .map(|_| agent(root.path(), "same"))
        .collect();
    assert!(matches!(
        TrustedProfileSet::freeze(too_many).await,
        Err(BackendError::Binding("too many trusted profiles"))
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn profile_refs_serialize_only_opaque_identity_and_reject_extra_authority() {
    let reference = ProfileRef {
        agent_id: AgentId::new(),
        version: ProfileVersion::new(),
    };
    let value = serde_json::to_value(reference).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(
        serde_json::from_value::<ProfileRef>(value.clone()).unwrap(),
        reference
    );
    let mut extra = value.clone();
    extra["provider_values"] = json!({"key": "must-not-be-accepted"});
    assert!(serde_json::from_value::<ProfileRef>(extra).is_err());
    for version in ["not-a-version", "00000000-0000-0000-0000-000000000000"] {
        let mut invalid = value.clone();
        invalid["version"] = json!(version);
        assert!(serde_json::from_value::<ProfileRef>(invalid).is_err());
    }
}
