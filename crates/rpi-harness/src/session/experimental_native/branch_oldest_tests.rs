//! Oldest-first retention probes: bounded keys, full metadata validation, selected bodies only.
use super::*;
use crate::session::{experimental_native::ExperimentalNativeRepo, state::SessionState};

async fn fixture(count: usize) -> (tempfile::TempDir, ExperimentalNativeStorage) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("synthetic-oldest-branch".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    if count > 0 {
        storage
            .run(move |s, clock| {
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

fn oldest(limit: usize) -> EntryQuery {
    EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        limit: Some(limit),
        ..Default::default()
    }
}

fn at(id: &str) -> BranchBounds {
    BranchBounds {
        stop_at_id: Some(id.into()),
        ..Default::default()
    }
}

fn seqs(keys: std::collections::VecDeque<u64>) -> Vec<u64> {
    keys.into_iter().collect()
}

// Change only authenticated metadata, leaving navigation and body untouched.
fn reparent(s: &Store, seq: u64, target: &str) -> Result<()> {
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
async fn oldest_matrix_matches_replayed_memory_after_reopen_and_history_audit() {
    let (dir, s) = fixture(0).await;
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
                    message: serde_json::from_value(serde_json::json!({
                        "kind":"tool_result","role":"toolResult","toolCallId":"call",
                        "toolName":"synthetic","content":[],"isError":false,"timestamp":42
                    }))?,
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
    let mut oracle = SessionState::new();
    for mutation in mutations.clone() {
        oracle.apply_mutation(mutation).unwrap();
    }
    let mut cases = vec![
        oldest(2),
        EntryQuery {
            entry_type: Some("message"),
            ..oldest(2)
        },
        EntryQuery {
            entry_type: Some("custom"),
            ..oldest(2)
        },
        EntryQuery {
            custom_type: Some("marker".into()),
            ..oldest(2)
        },
        EntryQuery {
            custom_type: Some("absent".into()),
            ..oldest(2)
        },
        EntryQuery {
            entry_id: Some("custom".into()),
            ..oldest(2)
        },
        EntryQuery {
            entry_id: Some("missing".into()),
            ..oldest(2)
        },
        EntryQuery {
            message_boundary_only: true,
            ..oldest(2)
        },
        EntryQuery {
            tool_call_id: Some("call".into()),
            ..oldest(2)
        },
        EntryQuery {
            tool_call_id: Some("missing".into()),
            ..oldest(2)
        },
        EntryQuery {
            outbox_key: Some("wanted".into()),
            ..oldest(2)
        },
        EntryQuery {
            outbox_key: Some("missing".into()),
            ..oldest(2)
        },
        EntryQuery {
            custom_type: Some("marker".into()),
            message_boundary_only: true,
            ..oldest(2)
        },
    ];
    for cursor in [0, 1, 3, 5, u64::MAX] {
        cases.push(EntryQuery {
            cursor: Some(EntryCursor { after_seq: cursor }),
            ..oldest(2)
        });
    }
    // Matching oldest-first bounds retain only the stop candidate, then apply
    // every query predicate. Include those cases in the Memory differential.
    let bounds = [
        BranchBounds::default(),
        at("missing"),
        at("custom"),
        at("u0"),
        at("u1"),
        BranchBounds {
            stop_at_type: Some("custom"),
            ..Default::default()
        },
        BranchBounds {
            stop_at_type: Some("message"),
            ..Default::default()
        },
        BranchBounds {
            stop_at_type: Some("absent"),
            ..Default::default()
        },
    ];
    let history = mutations
        .iter()
        .map(crate::session::jsonl::encode_mutation)
        .collect::<Vec<_>>();
    let metadata = s.metadata();
    let mut live = s;
    for pass in 0..2 {
        for start in ["u1", "u2", "tool", "custom"] {
            for bound in &bounds {
                for query in &cases {
                    for limit in [Some(1), Some(2), Some(20), None] {
                        let q = EntryQuery {
                            limit,
                            ..query.clone()
                        };
                        let expected = oracle.find_entries_on_branch(&q, bound, start).unwrap();
                        let actual = live.find_entries_on_branch(&q, bound, start).await.unwrap();
                        assert_eq!(
                            serde_json::to_value(actual).unwrap(),
                            serde_json::to_value(expected).unwrap(),
                            "pass={pass} start={start} query={q:?} bounds={bound:?}"
                        );
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
async fn deep_oldest_ring_needs_no_bodies_and_materializes_only_selected_bodies() {
    // Structural retention/read probe, not an RSS or traversal/I/O complexity claim.
    for depth in [32, 512] {
        let (_dir, s) = fixture(depth).await;
        s.run(move |s, _| {
            let start = format!("u{}", depth - 1);
            let bounds = BranchBounds::default();
            s.forbid_bodies(true);
            for cap in [1, 2, 7] {
                for cursor in [0, 3, depth as u64 - 2, u64::MAX] {
                    let q = EntryQuery {
                        cursor: Some(EntryCursor { after_seq: cursor }),
                        ..oldest(cap)
                    };
                    let selected = oldest_keys(&s.connection, &s.identity, &q, &start, &bounds)?;
                    assert!(selected.len() <= cap);
                    assert_eq!(
                        seqs(selected),
                        (1..=depth as u64)
                            .filter(|seq| *seq > cursor)
                            .take(cap)
                            .collect::<Vec<_>>()
                    );
                }
            }
            for q in [
                EntryQuery {
                    entry_id: Some("u3".into()),
                    ..oldest(2)
                },
                EntryQuery {
                    entry_type: Some("message"),
                    ..oldest(2)
                },
                EntryQuery {
                    custom_type: Some("absent".into()),
                    ..oldest(2)
                },
            ] {
                let expected = if q.entry_id.is_some() {
                    vec![4]
                } else if q.entry_type.is_some() {
                    vec![1, 2]
                } else {
                    vec![]
                };
                assert_eq!(
                    seqs(oldest_keys(
                        &s.connection,
                        &s.identity,
                        &q,
                        &start,
                        &bounds
                    )?),
                    expected
                );
            }
            assert!(entries(&s.connection, &s.identity, &oldest(2), &start, &bounds).is_err());
            s.forbid_bodies(false);
            s.connection.execute(
                "UPDATE facts SET body='damaged unselected body' WHERE seq>2",
                [],
            )?;
            let page = entries(&s.connection, &s.identity, &oldest(2), &start, &bounds)?;
            assert_eq!(
                page.iter().map(|e| e.id()).collect::<Vec<_>>(),
                vec!["u0", "u1"]
            );
            assert!(entries(&s.connection, &s.identity, &oldest(3), &start, &bounds).is_err());
            s.connection.execute(
                "UPDATE facts SET body='damaged selected body' WHERE seq=1",
                [],
            )?;
            assert_eq!(
                seqs(oldest_keys(
                    &s.connection,
                    &s.identity,
                    &oldest(2),
                    &start,
                    &bounds
                )?),
                vec![1, 2]
            );
            assert!(entries(&s.connection, &s.identity, &oldest(2), &start, &bounds).is_err());
            Ok(())
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn older_damage_rejects_oldest_page_and_stop_keeps_selected_parent_validation() {
    for selection in [false, true] {
        let (_dir, s) = fixture(32).await;
        s.run(move |s, _| {
            if selection {
                s.connection
                    .execute("UPDATE selection SET keys='{}' WHERE seq=1", [])?;
            } else {
                s.connection.execute(
                    "UPDATE facts SET metadata='damaged metadata' WHERE seq=1",
                    [],
                )?;
            }
            s.forbid_bodies(true);
            assert!(oldest_keys(
                &s.connection,
                &s.identity,
                &oldest(1),
                "u31",
                &BranchBounds::default()
            )
            .is_err());
            for bound in [
                at("u1"),
                BranchBounds {
                    stop_at_type: Some("message"),
                    ..Default::default()
                },
            ] {
                let expected = if bound.stop_at_id.is_some() {
                    vec![2]
                } else {
                    vec![32]
                };
                assert_eq!(
                    seqs(oldest_keys(
                        &s.connection,
                        &s.identity,
                        &oldest(1),
                        "u31",
                        &bound
                    )?),
                    expected
                );
            }
            s.forbid_bodies(false);
            assert!(entries(
                &s.connection,
                &s.identity,
                &oldest(1),
                "u31",
                &BranchBounds::default()
            )
            .is_err());
            // Selected-body verification also validates its immediate parent.
            // Stop at u1 omits corrupt u0 from the walk, but materializing u1
            // must still reject that damaged parent metadata/index.
            assert!(entries(&s.connection, &s.identity, &oldest(1), "u31", &at("u1")).is_err());
            assert_eq!(
                entries(&s.connection, &s.identity, &oldest(1), "u31", &at("u2"))?[0].id(),
                "u2"
            );
            Ok(())
        })
        .await
        .unwrap();
        assert!(s
            .find_entries_on_branch(&oldest(1), &BranchBounds::default(), "u31")
            .await
            .is_err());
        assert!(s
            .find_entries_on_branch(&oldest(1), &at("u1"), "u31")
            .await
            .is_err());
        let page = s
            .find_entries_on_branch(&oldest(1), &at("u2"), "u31")
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id(), "u2");
    }
}

#[tokio::test]
async fn oldest_rejects_invalid_inputs_and_parent_order_and_filters_matching_stop() {
    let (_dir, s) = fixture(4).await;
    // Even a bound matching START remains subject to every query predicate.
    let mismatch = EntryQuery {
        entry_type: Some("custom"),
        custom_type: Some("absent".into()),
        entry_id: Some("missing".into()),
        message_boundary_only: true,
        tool_call_id: Some("missing".into()),
        outbox_key: Some("missing".into()),
        cursor: Some(EntryCursor {
            after_seq: u64::MAX,
        }),
        ..oldest(1)
    };
    for start in ["u0", "u3"] {
        for bound in [
            at(start),
            BranchBounds {
                stop_at_type: Some("message"),
                ..Default::default()
            },
        ] {
            for cap in [Some(1), Some(2), None] {
                let q = EntryQuery {
                    limit: cap,
                    ..mismatch.clone()
                };
                let page = s.find_entries_on_branch(&q, &bound, start).await.unwrap();
                assert!(page.is_empty());
                let matching = EntryQuery {
                    limit: cap,
                    ..oldest(1)
                };
                let page = s
                    .find_entries_on_branch(&matching, &bound, start)
                    .await
                    .unwrap();
                assert_eq!(page.len(), 1);
                assert_eq!(page[0].id(), start);
            }
            assert!(s
                .find_entries_on_branch(&oldest(0), &bound, start)
                .await
                .is_err());
        }
    }
    for bound in [
        BranchBounds::default(),
        at("missing"),
        at("u1"),
        BranchBounds {
            stop_at_type: Some("custom"),
            ..Default::default()
        },
    ] {
        assert!(s
            .find_entries_on_branch(&mismatch, &bound, "u3")
            .await
            .unwrap()
            .is_empty());
    }
    // A multi-node oldest-first path with an explicit stop retains only that
    // stop node, not the newer suffix. A root-only path still applies filters.
    let page = s
        .find_entries_on_branch(&oldest(2), &at("u1"), "u3")
        .await
        .unwrap();
    assert_eq!(page.iter().map(|e| e.id()).collect::<Vec<_>>(), vec!["u1"]);
    assert!(s
        .find_entries_on_branch(&mismatch, &BranchBounds::default(), "u0")
        .await
        .unwrap()
        .is_empty());
    s.run(move |s, _| {
        let bounds = BranchBounds::default();
        assert_eq!(
            seqs(oldest_keys(
                &s.connection,
                &s.identity,
                &oldest(2),
                "u3",
                &at("u1")
            )?),
            vec![2]
        );
        for q in [
            EntryQuery {
                limit: None,
                ..oldest(1)
            },
            oldest(0),
            EntryQuery {
                order: None,
                ..oldest(1)
            },
            EntryQuery {
                order: Some(EntryOrder::NewestFirst),
                ..oldest(1)
            },
        ] {
            assert!(oldest_keys(&s.connection, &s.identity, &q, "u3", &bounds).is_err());
        }
        assert!(oldest_keys(&s.connection, &s.identity, &oldest(1), "missing", &bounds).is_err());
        for bound in [
            at("u3"),
            BranchBounds {
                stop_at_type: Some("message"),
                ..Default::default()
            },
        ] {
            assert_eq!(
                seqs(oldest_keys(
                    &s.connection,
                    &s.identity,
                    &mismatch,
                    "u3",
                    &bound
                )?),
                Vec::<u64>::new()
            );
            assert!(oldest_keys(&s.connection, &s.identity, &oldest(0), "u3", &bound).is_err());
        }
        s.forbid_bodies(true);
        for target in ["u1", "u3"] {
            reparent(s, 2, target)?;
            engine::read_metadata(&s.connection, &s.identity, 2)?;
            assert!(
                oldest_keys(&s.connection, &s.identity, &oldest(1), "u3", &bounds).is_err(),
                "{target}"
            );
            assert_eq!(
                seqs(oldest_keys(
                    &s.connection,
                    &s.identity,
                    &oldest(1),
                    "u3",
                    &at("u2")
                )?),
                vec![3]
            );
        }
        s.forbid_bodies(false);
        Ok(())
    })
    .await
    .unwrap();
    assert!(s
        .find_entries_on_branch(&oldest(1), &BranchBounds::default(), "missing")
        .await
        .is_err());
}
