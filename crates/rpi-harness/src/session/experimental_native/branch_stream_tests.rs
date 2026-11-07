use super::*;
use crate::session::{experimental_native::ExperimentalNativeRepo, state::SessionState};

async fn fixture() -> (tempfile::TempDir, ExperimentalNativeStorage) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("synthetic-branch-stream".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    (dir, storage)
}

fn user(n: usize) -> ProvisionedEntry {
    ProvisionedEntry {
        id: format!("u{n}"),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(format!("synthetic {n}"), 42).into(),
            terminate: None,
        },
    }
}

async fn users(s: &ExperimentalNativeStorage, count: usize) {
    s.run(move |s, clock| {
        commit(
            s,
            (0..count)
                .map(|n| Write::Entry(user(n), "main".into()))
                .collect(),
            None,
            clock,
        )?;
        Ok(())
    })
    .await
    .unwrap();
}

fn bounded(limit: usize) -> EntryQuery {
    EntryQuery {
        limit: Some(limit),
        ..Default::default()
    }
}

// Deliberately test-only: retain a valid metadata digest while changing a parent.
// Navigation/body are untouched; keys must validate the metadata path itself.
fn parent(s: &Store, seq: u64, target: &str) -> Result<()> {
    let mut meta = engine::read_metadata(&s.connection, &s.identity, seq)?.0;
    let Effect::Entry { parent, .. } = &mut meta.effect else {
        panic!("entry fixture")
    };
    *parent = Some(target.into());
    let json = serde_json::to_string(&meta)?;
    s.connection.execute(
        "UPDATE facts SET metadata=?1,metadata_digest=?2 WHERE seq=?3",
        params![
            json,
            engine::digest(&s.identity, seq, "metadata", json.as_bytes()),
            seq
        ],
    )?;
    Ok(())
}

#[tokio::test]
async fn bounded_branch_matrix_matches_memory_and_survives_reopen_and_history_audit() {
    let (dir, s) = fixture().await;
    let mutations = s
        .run(|s, clock| {
            let custom = ProvisionedEntry {
                id: "custom".into(),
                kind: ProvisionedKind::Custom {
                    custom_type: "marker".into(),
                    data: Some(serde_json::json!({"value":7})),
                },
            };
            let tool = ProvisionedEntry {
                id: "tool".into(),
                kind: ProvisionedKind::Message {
                    message: serde_json::from_value(
                        serde_json::json!({"kind":"tool_result","role":"toolResult",
                "toolCallId":"call","toolName":"synthetic","content":[],
                "isError":false,"timestamp":42}),
                    )?,
                    terminate: None,
                },
            };
            let outbox = ProvisionedEntry {
                id: "outbox".into(),
                kind: ProvisionedKind::Message {
                    message: rpi_agent::message::AgentMessage::Custom(
                        rpi_agent::message::CustomMessage::new(
                            "synthetic",
                            Vec::new(),
                            serde_json::json!({"__rpiOutbox":{"key":"wanted"}}),
                            42,
                        ),
                    ),
                    terminate: None,
                },
            };
            commit(
                s,
                vec![
                    Write::Entry(user(0), "main".into()),
                    Write::Entry(custom, "main".into()),
                    Write::Entry(tool, "main".into()),
                    Write::Entry(outbox, "main".into()),
                    Write::Entry(user(1), "main".into()),
                    Write::Lane("sibling".into(), Some("custom".into()), true),
                    Write::Entry(user(2), "sibling".into()),
                ],
                None,
                clock,
            )
        })
        .await
        .unwrap();
    // Replay identical stamped mutations rather than comparing wall-clock timestamps.
    let mut oracle = SessionState::new();
    for mutation in mutations.clone() {
        oracle.apply_mutation(mutation).unwrap();
    }
    let mut cases = vec![
        bounded(1),
        bounded(2),
        bounded(20),
        EntryQuery {
            entry_type: Some("message"),
            ..bounded(2)
        },
        EntryQuery {
            entry_type: Some("custom"),
            ..bounded(2)
        },
        EntryQuery {
            custom_type: Some("marker".into()),
            ..bounded(2)
        },
        EntryQuery {
            custom_type: Some("absent".into()),
            ..bounded(2)
        },
        EntryQuery {
            entry_id: Some("custom".into()),
            ..bounded(2)
        },
        EntryQuery {
            entry_id: Some("missing".into()),
            ..bounded(2)
        },
        EntryQuery {
            message_boundary_only: true,
            ..bounded(2)
        },
        EntryQuery {
            tool_call_id: Some("call".into()),
            ..bounded(2)
        },
        EntryQuery {
            tool_call_id: Some("missing".into()),
            ..bounded(2)
        },
        EntryQuery {
            outbox_key: Some("wanted".into()),
            ..bounded(2)
        },
        EntryQuery {
            outbox_key: Some("missing".into()),
            ..bounded(2)
        },
        EntryQuery {
            custom_type: Some("marker".into()),
            message_boundary_only: true,
            ..bounded(2)
        },
    ];
    for cursor in [0, 1, 3, 5, u64::MAX] {
        cases.push(EntryQuery {
            cursor: Some(EntryCursor { after_seq: cursor }),
            ..bounded(2)
        });
    }
    let bounds = vec![
        BranchBounds::default(),
        BranchBounds {
            stop_at_id: Some("custom".into()),
            ..Default::default()
        },
        BranchBounds {
            stop_at_type: Some("custom"),
            ..Default::default()
        },
        BranchBounds {
            stop_at_id: Some("u1".into()),
            ..Default::default()
        },
        BranchBounds {
            stop_at_id: Some("missing".into()),
            ..Default::default()
        },
    ];
    let metadata = s.metadata();
    let history = mutations
        .iter()
        .map(crate::session::jsonl::encode_mutation)
        .collect::<Vec<_>>();
    let mut live = s;
    for pass in 0..2 {
        for start in ["u1", "u2", "tool", "custom"] {
            for bound in &bounds {
                for query in &cases {
                    for order in [
                        None,
                        Some(EntryOrder::NewestFirst),
                        Some(EntryOrder::OldestFirst),
                    ] {
                        for cap in [query.limit, None] {
                            let q = EntryQuery {
                                order,
                                limit: cap,
                                ..query.clone()
                            };
                            let expected = oracle.find_entries_on_branch(&q, bound, start).unwrap();
                            let actual =
                                live.find_entries_on_branch(&q, bound, start).await.unwrap();
                            assert_eq!(
                                serde_json::to_value(actual).unwrap(),
                                serde_json::to_value(expected).unwrap(),
                                "pass={pass} start={start} query={q:?} bounds={bound:?}"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(
            live.history_revision().await.unwrap(),
            mutations.len() as u64
        );
        let audited = live
            .run(|s, _| {
                let (projection, _) = engine::restore(&s.connection, &s.identity)?;
                (1..=projection.seq)
                    .map(|seq| s.mutation(seq))
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .unwrap();
        assert_eq!(
            audited
                .iter()
                .map(crate::session::jsonl::encode_mutation)
                .collect::<Vec<_>>(),
            history
        );
        if pass == 0 {
            drop(live);
            let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
                .await
                .unwrap();
            live = repo.open(&metadata).await.unwrap();
        }
    }
}

#[tokio::test]
async fn deep_branch_keys_retain_only_limit_and_need_no_body_authorization() {
    // Structural read/retention probe, not a constant RSS, traversal or I/O claim.
    for depth in [32, 512] {
        let (_dir, s) = fixture().await;
        users(&s, depth).await;
        s.run(move |s, _| {
            s.forbid_bodies(true);
            let start = format!("u{}", depth - 1);
            for cap in [1, 2, 7] {
                let selected = keys(
                    &s.connection,
                    &s.identity,
                    &bounded(cap),
                    &start,
                    &BranchBounds::default(),
                )?;
                assert!(selected.len() <= cap);
                assert_eq!(
                    selected,
                    (1..=depth as u64).rev().take(cap).collect::<Vec<_>>()
                );
            }
            // The private helper must not accept the fallback routing cases.
            for invalid in [
                EntryQuery::default(),
                bounded(0),
                EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..bounded(1)
                },
            ] {
                assert!(keys(
                    &s.connection,
                    &s.identity,
                    &invalid,
                    &start,
                    &BranchBounds::default()
                )
                .is_err());
            }
            // A no-match also walks the full ancestry without touching bodies.
            assert!(keys(
                &s.connection,
                &s.identity,
                &EntryQuery {
                    custom_type: Some("absent".into()),
                    ..bounded(2)
                },
                &start,
                &BranchBounds::default()
            )?
            .is_empty());
            assert!(entries(
                &s.connection,
                &s.identity,
                &bounded(1),
                &start,
                &BranchBounds::default()
            )
            .is_err());
            // Damage the oldest metadata after filling the page: proves full traversal.
            s.connection
                .execute("UPDATE facts SET metadata='damaged' WHERE seq=1", [])?;
            assert!(keys(
                &s.connection,
                &s.identity,
                &bounded(1),
                &start,
                &BranchBounds::default()
            )
            .is_err());
            s.forbid_bodies(false);
            Ok(())
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn entries_materialize_only_selected_bodies_after_metadata_scan() {
    let (_dir, s) = fixture().await;
    users(&s, 32).await;
    s.run(|s, _| {
        s.connection.execute(
            "UPDATE facts SET body='damaged unselected body' WHERE seq<32",
            [],
        )?;
        let latest = entries(
            &s.connection,
            &s.identity,
            &bounded(1),
            "u31",
            &BranchBounds::default(),
        )?;
        assert_eq!(latest[0].id(), "u31");
        assert!(entries(
            &s.connection,
            &s.identity,
            &bounded(2),
            "u31",
            &BranchBounds::default()
        )
        .is_err());
        s.connection.execute(
            "UPDATE facts SET body='damaged selected body' WHERE seq=32",
            [],
        )?;
        assert!(keys(
            &s.connection,
            &s.identity,
            &bounded(1),
            "u31",
            &BranchBounds::default()
        )
        .is_ok());
        assert!(entries(
            &s.connection,
            &s.identity,
            &bounded(1),
            "u31",
            &BranchBounds::default()
        )
        .is_err());
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn older_metadata_and_selection_damage_reject_full_page_unless_bounds_stop_before_it() {
    for selection in [false, true] {
        let (_dir, s) = fixture().await;
        users(&s, 32).await;
        s.run(move |s, _| {
            let before: Vec<u8> = s.connection.query_row(
                "SELECT CAST(body AS BLOB) FROM facts WHERE seq=1",
                [],
                |r| r.get(0),
            )?;
            if selection {
                s.connection
                    .execute("UPDATE selection SET keys='{}' WHERE seq=1", [])?;
            } else {
                s.connection.execute(
                    "UPDATE facts SET metadata='damaged metadata' WHERE seq=1",
                    [],
                )?;
            }
            // Direct calls avoid restore/tail recovery masking the helper's validation.
            assert!(keys(
                &s.connection,
                &s.identity,
                &bounded(1),
                "u31",
                &BranchBounds::default()
            )
            .is_err());
            assert!(entries(
                &s.connection,
                &s.identity,
                &bounded(1),
                "u31",
                &BranchBounds::default()
            )
            .is_err());
            for bound in [
                BranchBounds {
                    stop_at_id: Some("u1".into()),
                    ..Default::default()
                },
                BranchBounds {
                    stop_at_type: Some("message"),
                    ..Default::default()
                },
            ] {
                assert_eq!(
                    keys(&s.connection, &s.identity, &bounded(1), "u31", &bound)?,
                    vec![32]
                );
                assert_eq!(
                    entries(&s.connection, &s.identity, &bounded(1), "u31", &bound)?[0].id(),
                    "u31"
                );
            }
            let after: Vec<u8> = s.connection.query_row(
                "SELECT CAST(body AS BLOB) FROM facts WHERE seq=1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(before, after);
            Ok(())
        })
        .await
        .unwrap();
        // Public routing must retain the helper's full-path rejection, even
        // though the newest matching entry alone would have filled the page.
        assert!(s
            .find_entries_on_branch(&bounded(1), &BranchBounds::default(), "u31")
            .await
            .is_err());
        let bound = BranchBounds {
            stop_at_id: Some("u1".into()),
            ..Default::default()
        };
        let page = s
            .find_entries_on_branch(&bounded(1), &bound, "u31")
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id(), "u31");
    }
}

#[tokio::test]
async fn digest_valid_self_forward_cycle_missing_and_nonentry_parents_fail_closed() {
    for attack in ["self", "forward", "cycle", "missing", "nonentry"] {
        let (_dir, s) = fixture().await;
        users(&s, 4).await;
        // Give the nonentry attack a real record id, not just an absent id.
        s.run(|s, clock| {
            let record: LaneRecord = serde_json::from_value(serde_json::json!({
                "id":"record-only","lane":"main","seq":0,"timestamp":42,
                "type":"abort_requested","runId":"synthetic-run"
            }))?;
            commit(s, vec![Write::Record(record)], None, clock)?;
            Ok(())
        })
        .await
        .unwrap();
        s.run(move |s, _| {
            match attack {
                "self" => parent(s, 2, "u1")?,
                "forward" => parent(s, 2, "u3")?,
                "cycle" => {
                    parent(s, 2, "u0")?;
                    parent(s, 1, "u2")?;
                }
                "missing" => parent(s, 2, "absent")?,
                "nonentry" => parent(s, 2, "record-only")?,
                _ => unreachable!(),
            }
            // The rewritten row itself passes digest/index validation.
            engine::read_metadata(&s.connection, &s.identity, 2)?;
            s.forbid_bodies(true);
            assert!(
                keys(
                    &s.connection,
                    &s.identity,
                    &bounded(1),
                    "u3",
                    &BranchBounds::default()
                )
                .is_err(),
                "{attack}"
            );
            let bounded_path = BranchBounds {
                stop_at_id: Some("u2".into()),
                ..Default::default()
            };
            assert_eq!(
                keys(&s.connection, &s.identity, &bounded(1), "u3", &bounded_path)?,
                vec![4]
            );
            s.forbid_bodies(false);
            Ok(())
        })
        .await
        .unwrap();
    }
}
