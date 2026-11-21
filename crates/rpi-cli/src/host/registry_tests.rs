use super::*;
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

pub(super) fn make_spec(mode: AgentMode) -> AgentSpec {
    let id = AgentId::new();
    let cwd = std::env::temp_dir();
    AgentSpec {
        id,
        name: "catalog-test".into(),
        parent_id: None,
        execution: ExecutionMode::InProcess,
        mode,
        session: (mode == AgentMode::Persistent).then(|| SessionRef {
            id: format!("session-{id}"),
            path: cwd.join(format!("session-{id}.jsonl")),
        }),
        launch: LaunchSpec {
            cwd,
            model: None,
            tools: vec!["read".into()],
            resource_profile: None,
        },
        timeouts: TimeoutOverrides::default(),
    }
}

fn directory() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host");
    (root, path)
}

async fn transition(catalog: &HostCatalog, record: &AgentRecord, next: AgentState) -> AgentRecord {
    catalog
        .transition(record.spec.id, catalog.guard(record), next)
        .await
        .unwrap()
}

#[tokio::test]
async fn catalog_preserves_identity_spec_and_relationships_across_restart() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let parent = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let mut child = make_spec(AgentMode::Persistent);
    child.parent_id = Some(parent.spec.id);
    let child = catalog.register(child).await.unwrap();
    let child = transition(&catalog, &child, AgentState::Starting).await;
    let child = transition(&catalog, &child, AgentState::Idle).await;
    let parent = transition(&catalog, &parent, AgentState::Starting).await;
    let parent = transition(&catalog, &parent, AgentState::Idle).await;
    let parent = transition(&catalog, &parent, AgentState::Running).await;
    let old_epoch = catalog.epoch();
    catalog.shutdown().await.unwrap();
    let restored = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    assert_ne!(restored.epoch(), old_epoch);
    let parent2 = restored.get(parent.spec.id).await.unwrap();
    let child2 = restored.get(child.spec.id).await.unwrap();
    assert_eq!(parent2.state, AgentState::Interrupted);
    assert_eq!(child2.state, AgentState::Unloaded);
    assert_eq!(parent2.spec, parent.spec);
    assert_eq!(child2.spec, child.spec);
    assert_eq!(parent2.last_epoch, old_epoch);
    assert_eq!(parent2.revision, parent.revision + 1);
    assert_eq!(child2.revision, child.revision + 1);
    let status = restored.status().await.unwrap();
    assert_eq!(
        (
            status.registered,
            status.resident,
            status.executing,
            status.busy
        ),
        (2, 0, 0, 0)
    );
    let parent3 = transition(&restored, &parent2, AgentState::Starting).await;
    assert_eq!(parent3.last_epoch, restored.epoch());
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn epoch_guards_reject_old_commands_even_when_revision_did_not_change() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let record = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let old_guard = catalog.guard(&record);
    catalog.shutdown().await.unwrap();
    let next = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    assert_eq!(
        next.get(record.spec.id).await.unwrap().revision,
        record.revision
    );
    assert!(matches!(
        next.transition(record.spec.id, old_guard, AgentState::Starting)
            .await,
        Err(CatalogError::EpochConflict)
    ));
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_agent_and_session_registrations_never_overwrite() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    let record = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let mut duplicate = record.spec.clone();
    duplicate.name = "would-overwrite".into();
    assert!(matches!(
        catalog.register(duplicate).await,
        Err(CatalogError::AlreadyRegistered)
    ));
    for same_id in [true, false] {
        let mut duplicate = make_spec(AgentMode::Persistent);
        if same_id {
            duplicate.session.as_mut().unwrap().id =
                record.spec.session.as_ref().unwrap().id.clone();
        } else {
            duplicate.session.as_mut().unwrap().path =
                record.spec.session.as_ref().unwrap().path.clone();
        }
        assert!(matches!(
            catalog.register(duplicate).await,
            Err(CatalogError::SessionAlreadyRegistered)
        ));
    }
    assert_eq!(catalog.get(record.spec.id).await.unwrap(), record);
    assert_eq!(catalog.status().await.unwrap().registered, 1);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_commits_have_separate_registered_resident_and_executing_counts() {
    let (_root, path) = directory();
    let config = HostConfig {
        max_agents: NonZeroUsize::new(1),
        ..Default::default()
    };
    let catalog = HostCatalog::open(path, config).await.unwrap();
    let first = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let second = catalog.register(make_spec(AgentMode::Mini)).await.unwrap();
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    let stale = catalog.guard(&first);
    let first = transition(&catalog, &first, AgentState::Starting).await;
    assert!(matches!(
        catalog
            .transition(first.spec.id, stale, AgentState::Idle)
            .await,
        Err(CatalogError::RevisionConflict {
            expected: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        catalog
            .transition(second.spec.id, catalog.guard(&second), AgentState::Starting)
            .await,
        Err(CatalogError::Policy(HostError::AtCapacity { limit: 1 }))
    ));
    let first = transition(&catalog, &first, AgentState::Idle).await;
    let first = transition(&catalog, &first, AgentState::Running).await;
    let first = transition(&catalog, &first, AgentState::WaitingApproval).await;
    let status = catalog.status().await.unwrap();
    assert_eq!(
        (
            status.registered,
            status.resident,
            status.executing,
            status.busy,
            status.mini
        ),
        (2, 1, 1, 1, 1)
    );
    let first = transition(&catalog, &first, AgentState::Closing).await;
    assert_eq!(catalog.status().await.unwrap().resident, 1);
    let _first = transition(&catalog, &first, AgentState::Closed).await;
    let second = transition(&catalog, &second, AgentState::Starting).await;
    let _second = transition(&catalog, &second, AgentState::Lost).await;
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_commit_rolls_back_both_new_and_closing_permits() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let record = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    let reject = || {
        database
            .execute_batch(
                "CREATE TRIGGER reject_update BEFORE UPDATE ON agents
        BEGIN SELECT RAISE(ABORT,'test-injected-write-failure'); END;",
            )
            .unwrap()
    };
    reject();
    assert!(matches!(
        catalog
            .transition(record.spec.id, catalog.guard(&record), AgentState::Starting)
            .await,
        Err(CatalogError::Database(_))
    ));
    assert_eq!(catalog.get(record.spec.id).await.unwrap(), record);
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    database
        .execute_batch("DROP TRIGGER reject_update")
        .unwrap();
    let record = transition(&catalog, &record, AgentState::Starting).await;
    let record = transition(&catalog, &record, AgentState::Idle).await;
    let record = transition(&catalog, &record, AgentState::Closing).await;
    reject();
    assert!(catalog
        .transition(record.spec.id, catalog.guard(&record), AgentState::Closed)
        .await
        .is_err());
    assert_eq!(catalog.get(record.spec.id).await.unwrap(), record);
    assert_eq!(catalog.status().await.unwrap().resident, 1);
    database
        .execute_batch("DROP TRIGGER reject_update")
        .unwrap();
    transition(&catalog, &record, AgentState::Closed).await;
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    drop(database);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_private_registry_has_one_owner_and_shutdown_releases_before_ack() {
    const CHILD_DIR: &str = "RPI_TEST_HOST_CATALOG_LOCK_DIR";
    const CHILD_EXPECTED: &str = "RPI_TEST_HOST_CATALOG_LOCK_EXPECTED";
    if let Some(path) = std::env::var_os(CHILD_DIR) {
        let result = HostCatalog::open(path.into(), HostConfig::default()).await;
        if std::env::var(CHILD_EXPECTED).unwrap() == "locked" {
            assert!(matches!(result, Err(CatalogError::AlreadyRunning)));
        } else {
            result.unwrap().shutdown().await.unwrap();
        }
        return;
    }
    async fn subprocess(path: &std::path::Path, expected: &str) {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host::registry_tests::one_private_registry_has_one_owner_and_shutdown_releases_before_ack"])
            .env(CHILD_DIR, path).env(CHILD_EXPECTED, expected)
            .output().await.unwrap();
        assert!(
            output.status.success(),
            "subprocess lock check failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let (_root, path) = directory();
    let first = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let second = HostCatalog::open(path.clone(), HostConfig::default()).await;
    assert!(matches!(second, Err(CatalogError::AlreadyRunning)));
    subprocess(&path, "locked").await;
    let clone = first.clone();
    drop(first);
    assert_eq!(clone.status().await.unwrap().registered, 0);
    clone.shutdown().await.unwrap();
    assert!(matches!(
        clone.status().await,
        Err(CatalogError::WorkerStopped)
    ));
    assert!(path.join("instance.lock").is_file());
    subprocess(&path, "opened").await;
    let next = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_last_handle_eventually_releases_the_worker_and_lock() {
    let (_root, path) = directory();
    let first = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    drop(first);
    let mut recovered = None;
    for _ in 0..100 {
        match HostCatalog::open(path.clone(), HostConfig::default()).await {
            Ok(catalog) => {
                recovered = Some(catalog);
                break;
            }
            Err(CatalogError::AlreadyRunning) => tokio::time::sleep(Duration::from_millis(5)).await,
            Err(error) => panic!("unexpected open error: {error}"),
        }
    }
    recovered
        .expect("worker leaked ownership after last handle dropped")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn inventory_pagination_is_bounded_and_unknown_agents_are_explicit() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    let mut expected = Vec::new();
    for _ in 0..9 {
        expected.push(
            catalog
                .register(make_spec(AgentMode::Mini))
                .await
                .unwrap()
                .spec
                .id,
        );
    }
    expected.sort();
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let page = catalog.list(after, 3).await.unwrap();
        assert!(page.agents.len() <= 3);
        found.extend(page.agents.into_iter().map(|r| r.spec.id));
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(found, expected);
    assert!(catalog.list(None, 0).await.is_err());
    assert!(catalog.list(None, MAX_PAGE_SIZE + 1).await.is_err());
    assert!(matches!(
        catalog.get(AgentId::new()).await,
        Err(CatalogError::UnknownAgent)
    ));
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_sweeps_more_than_one_page_and_mini_cannot_revive() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..MAX_PAGE_SIZE + 3 {
        ids.push(
            catalog
                .register(make_spec(AgentMode::Mini))
                .await
                .unwrap()
                .spec
                .id,
        );
    }
    catalog.shutdown().await.unwrap();
    let catalog = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    for id in ids {
        let record = catalog.get(id).await.unwrap();
        assert_eq!((record.state, record.revision), (AgentState::Lost, 2));
        assert!(matches!(
            catalog
                .transition(id, catalog.guard(&record), AgentState::Starting)
                .await,
            Err(CatalogError::Policy(HostError::MiniStateLost))
        ));
    }
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    catalog.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_catalog_admission_commits_exactly_the_available_slots() {
    let (_root, path) = directory();
    let config = HostConfig {
        max_agents: NonZeroUsize::new(5),
        ..Default::default()
    };
    let catalog = HostCatalog::open(path, config).await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..40 {
        let catalog = catalog.clone();
        tasks.push(tokio::spawn(async move {
            let record = catalog.register(make_spec(AgentMode::Mini)).await.unwrap();
            catalog
                .transition(record.spec.id, catalog.guard(&record), AgentState::Starting)
                .await
        }));
    }
    let mut accepted = 0;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(CatalogError::Policy(HostError::AtCapacity { limit: 5 })) => rejected += 1,
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    assert_eq!((accepted, rejected), (5, 35));
    catalog.set_max_agents(NonZeroUsize::new(1)).await.unwrap();
    let status = catalog.status().await.unwrap();
    assert_eq!(
        (
            status.registered,
            status.resident,
            status.executing,
            status.busy
        ),
        (40, 5, 0, 5)
    );
    assert_eq!(status.max_agents, Some(1));
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn blocking_database_wait_does_not_block_current_thread_runtime() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    let caller = catalog.clone();
    let pending = tokio::spawn(async move { caller.register(make_spec(AgentMode::Mini)).await });
    tokio::task::yield_now().await;
    tokio::time::timeout(
        Duration::from_millis(200),
        tokio::time::sleep(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    database.execute_batch("COMMIT").unwrap();
    tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(database);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_metadata_is_visible_even_if_new_hard_limits_prevent_loading() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let mut spec = make_spec(AgentMode::Persistent);
    spec.timeouts.run = TimeoutOverride::After(NonZeroU64::new(200).unwrap());
    let record = catalog.register(spec).await.unwrap();
    catalog.shutdown().await.unwrap();
    let timeouts = Timeouts {
        run_ms: NonZeroU64::new(100),
        ..Default::default()
    };
    let catalog = HostCatalog::open(
        path,
        HostConfig {
            default_timeouts: timeouts,
            hard_timeout_limits: timeouts,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let observed = catalog.get(record.spec.id).await.unwrap();
    assert_eq!(observed.spec, record.spec);
    assert!(matches!(
        catalog
            .transition(
                record.spec.id,
                catalog.guard(&observed),
                AgentState::Starting
            )
            .await,
        Err(CatalogError::Policy(_))
    ));
    assert_eq!(catalog.status().await.unwrap().resident, 0);
    catalog.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_startup_classification_rolls_back_all_rows_and_epoch() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let old_epoch = catalog.epoch();
    let mut records = Vec::new();
    for _ in 0..2 {
        let record = catalog
            .register(make_spec(AgentMode::Persistent))
            .await
            .unwrap();
        let record = transition(&catalog, &record, AgentState::Starting).await;
        let record = transition(&catalog, &record, AgentState::Idle).await;
        records.push(transition(&catalog, &record, AgentState::Running).await);
    }
    records.sort_by_key(|r| r.spec.id);
    catalog.shutdown().await.unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE agents SET data='invalid-json' WHERE id=?1",
            [records[1].spec.id.to_string()],
        )
        .unwrap();
    assert!(HostCatalog::open(path, HostConfig::default())
        .await
        .is_err());
    let data: String = database
        .query_row(
            "SELECT data FROM agents WHERE id=?1",
            [records[0].spec.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<AgentRecord>(&data).unwrap(),
        records[0]
    );
    let epoch: String = database
        .query_row("SELECT epoch FROM host_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(epoch, old_epoch.to_string());
}

#[tokio::test]
async fn unknown_schema_fails_without_retaining_instance_ownership() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    catalog.shutdown().await.unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database.execute_batch("PRAGMA user_version=99").unwrap();
    assert!(matches!(
        HostCatalog::open(path.clone(), HostConfig::default()).await,
        Err(CatalogError::UnsupportedVersion(99))
    ));
    database.execute_batch("PRAGMA user_version=1").unwrap();
    drop(database);
    HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn paths_are_private_and_existing_permissions_are_never_repaired_silently() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    for entry in [
        path.clone(),
        path.join("agents.sqlite3"),
        path.join("instance.lock"),
    ] {
        assert_eq!(
            std::fs::metadata(entry).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    catalog.shutdown().await.unwrap();
    let alias = root.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(HostCatalog::open(alias, HostConfig::default())
        .await
        .is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        HostCatalog::open(path.clone(), HostConfig::default()).await,
        Err(CatalogError::UnsafePath)
    ));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(
        HostCatalog::open("relative-path".into(), HostConfig::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn symlinks_hardlinks_and_public_database_files_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for case in 0..4 {
        let (root, path) = directory();
        let catalog = HostCatalog::open(path.clone(), HostConfig::default())
            .await
            .unwrap();
        catalog.shutdown().await.unwrap();
        match case {
            0 => symlink(
                root.path().join("missing-target"),
                path.join("agents.sqlite3-wal"),
            )
            .unwrap(),
            1 => {
                let target = root.path().join("database-alias");
                std::fs::rename(path.join("agents.sqlite3"), &target).unwrap();
                symlink(target, path.join("agents.sqlite3")).unwrap();
            }
            2 => std::fs::hard_link(path.join("instance.lock"), root.path().join("lock-alias"))
                .unwrap(),
            3 => std::fs::set_permissions(
                path.join("agents.sqlite3"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(
            HostCatalog::open(path, HostConfig::default())
                .await
                .is_err(),
            "unsafe case {case}"
        );
    }
}

#[tokio::test]
async fn unversioned_foreign_schema_is_not_taken_over() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    catalog.shutdown().await.unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database
        .execute_batch(
            "DROP TABLE agents; DROP TABLE host_meta;
        CREATE TABLE sqlitex_foreign (value TEXT); INSERT INTO sqlitex_foreign VALUES ('keep');
        PRAGMA user_version=0;",
        )
        .unwrap();
    assert!(matches!(
        HostCatalog::open(path, HostConfig::default()).await,
        Err(CatalogError::InvalidRecord(_))
    ));
    let value: String = database
        .query_row("SELECT value FROM sqlitex_foreign", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "keep");
}

#[tokio::test]
async fn indexed_revision_mismatch_is_not_silently_repaired_on_restart() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let record = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    catalog.shutdown().await.unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE agents SET revision=revision+1 WHERE id=?1",
            [record.spec.id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        HostCatalog::open(path, HostConfig::default()).await,
        Err(CatalogError::InvalidRecord(_))
    ));
    let revision: u64 = database
        .query_row("SELECT revision FROM agents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(revision, record.revision + 1);
}

#[tokio::test]
async fn oversized_external_row_is_rejected_by_sqlite_before_json_decoding() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path.clone(), HostConfig::default())
        .await
        .unwrap();
    let record = catalog
        .register(make_spec(AgentMode::Persistent))
        .await
        .unwrap();
    catalog.shutdown().await.unwrap();
    let database = rusqlite::Connection::open(path.join("agents.sqlite3")).unwrap();
    database
        .execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    database
        .execute(
            "UPDATE agents SET data=?1 WHERE id=?2",
            rusqlite::params![
                "x".repeat(super::catalog::MAX_RECORD_BYTES * 4),
                record.spec.id.to_string()
            ],
        )
        .unwrap();
    assert!(matches!(
        HostCatalog::open(path, HostConfig::default()).await,
        Err(CatalogError::Database(_))
    ));
}

#[tokio::test]
async fn oversized_queued_metadata_is_rejected_before_persistence() {
    let (_root, path) = directory();
    let catalog = HostCatalog::open(path, HostConfig::default())
        .await
        .unwrap();
    let mut spec = make_spec(AgentMode::Mini);
    spec.launch.tools = vec!["\u{0001}".repeat(128); 256];
    assert!(matches!(
        catalog.register(spec).await,
        Err(CatalogError::Policy(HostError::InvalidInput(_)))
    ));
    assert_eq!(catalog.status().await.unwrap().registered, 0);
    catalog.shutdown().await.unwrap();
}
