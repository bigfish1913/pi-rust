//! Fork selection must agree between metadata-only JSONL and eager backends.
use rpi_harness::error::SessionErrorCode;
use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionRepo, SqliteSessionRepo, SystemClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::Arc;

fn options(id: &str) -> SessionCreateOptions {
    SessionCreateOptions {
        id: Some(id.into()),
        ..Default::default()
    }
}
fn model(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::ModelChange {
            provider: "test".into(),
            model_id: "model".into(),
        },
    }
}
fn message(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(id, 0).into(),
            terminate: None,
        },
    }
}
async fn check<R: SessionRepo>(repo: R) {
    let source = repo.create(&options("source")).await.unwrap();
    for entry in [
        model("root"),
        message("m1"),
        model("between"),
        message("m2"),
        model("switch"),
        ProvisionedEntry {
            id: "thinking".into(),
            kind: ProvisionedKind::ThinkingLevel {
                thinking_level: "high".into(),
            },
        },
    ] {
        source.append_entry(entry, "main").await.unwrap();
    }
    source.set_name(Some("named source")).await.unwrap();
    source
        .set_label("m2", Some("nearest message"))
        .await
        .unwrap();
    source
        .set_label("thinking", Some("excluded suffix"))
        .await
        .unwrap();
    source.create_lane("side", Some("m1")).await.unwrap();
    source
        .append_entry(message("side-message"), "side")
        .await
        .unwrap();
    let metadata = source.metadata();
    // JSONL fork reopens a cold source: exercise the metadata path, not only
    // freshly appended resident entries. Also release the writer claim.
    drop(source);
    for (index, entry_id, position, expected) in [
        (0, None, None, vec!["root", "m1", "between", "m2"]),
        (
            1,
            None,
            Some(ForkPosition::At),
            vec!["root", "m1", "between", "m2"],
        ),
        (
            2,
            None,
            Some(ForkPosition::Before),
            vec!["root", "m1", "between"],
        ),
        (3, Some("m2"), None, vec!["root", "m1", "between"]),
        (4, Some("m1"), None, vec!["root"]),
    ] {
        let child = repo
            .fork(
                &metadata,
                &options(&format!("child-{index}")),
                &ForkOptions::Branch {
                    entry_id: entry_id.map(str::to_owned),
                    position,
                },
            )
            .await
            .unwrap();
        let entries = child
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.iter().map(Entry::id).collect::<Vec<_>>(), expected);
        assert_eq!(
            child.get_lanes().await.unwrap(),
            vec![LanePointer {
                lane: "main".into(),
                leaf_id: expected.last().map(|id| (*id).into())
            }]
        );
        assert_eq!(
            child.get_name().await.unwrap().as_deref(),
            Some("named source")
        );
        assert_eq!(child.get_label("thinking").await.unwrap(), None);
        if expected.contains(&"m2") {
            assert_eq!(
                child.get_label("m2").await.unwrap().as_deref(),
                Some("nearest message")
            );
        }
    }
    for (index, position) in [None, Some(ForkPosition::At), Some(ForkPosition::Before)]
        .into_iter()
        .enumerate()
    {
        let result = repo
            .fork(
                &metadata,
                &options(&format!("rejected-{index}")),
                &ForkOptions::Branch {
                    entry_id: Some("thinking".into()),
                    position,
                },
            )
            .await;
        assert_eq!(
            result
                .err()
                .expect("explicit state target must be rejected")
                .code,
            SessionErrorCode::InvalidForkTarget
        );
    }
    let tree = repo
        .fork(&metadata, &options("tree"), &ForkOptions::Tree)
        .await
        .unwrap();
    assert_eq!(
        tree.find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        7
    );
    assert_eq!(tree.get_lanes().await.unwrap().len(), 2);
    drop(tree);
    let no_message = repo.create(&options("no-message")).await.unwrap();
    no_message
        .append_entry(model("only-state"), "main")
        .await
        .unwrap();
    let metadata = no_message.metadata();
    drop(no_message);
    let result = repo
        .fork(
            &metadata,
            &options("no-ancestor"),
            &ForkOptions::Branch {
                entry_id: None,
                position: None,
            },
        )
        .await;
    assert_eq!(
        result
            .err()
            .expect("implicit state tip needs a message ancestor")
            .code,
        SessionErrorCode::InvalidForkTarget
    );
    let empty = repo.create(&options("empty")).await.unwrap();
    let metadata = empty.metadata();
    drop(empty);
    let child = repo
        .fork(
            &metadata,
            &options("empty-child"),
            &ForkOptions::Branch {
                entry_id: None,
                position: None,
            },
        )
        .await
        .unwrap();
    assert!(child
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn memory_fork_tip_contract() {
    check(InMemorySessionRepo::new(
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ))
    .await;
}
#[tokio::test]
async fn sqlite_fork_tip_contract() {
    let dir = tempfile::tempdir().unwrap();
    check(SqliteSessionRepo::open(dir.path().join("sessions.db")).unwrap()).await;
}
#[tokio::test]
async fn jsonl_cold_fork_tip_contract() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    check(JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.path().to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    }))
    .await;
}
