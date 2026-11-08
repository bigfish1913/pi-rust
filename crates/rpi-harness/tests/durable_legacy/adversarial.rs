//! Actual inner branch capture, adversarial responses and observed fences.
use super::*;
use rpi_harness::{
    agent_harness::AgentHarness,
    durable::{DurableActor, DurableService},
};
use std::sync::atomic::Ordering;
mod probe {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/durable_control_probe.rs"
    ));
}
use probe::Probe;

fn probed() -> (Arc<Session>, Arc<Probe>) {
    let p = Probe::new(AgentHarnessOptions::default().session.storage());
    (Arc::new(Session::new(p.clone(), None)), p)
}
async fn seeded() -> (Arc<Session>, Arc<Probe>) {
    let (s, p) = probed();
    append(&s, "prefix", "durable-state", Some(snapshot(1))).await;
    append(&s, "selected", "durable-state", Some(snapshot(2))).await;
    (s, p)
}
fn no_writes(p: &Probe) {
    assert_eq!(p.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(p.commits.load(Ordering::SeqCst), 0);
}
async fn captured(
    s: Arc<Session>,
    p: &Probe,
) -> tokio::task::JoinHandle<Result<LegacyBranchSource, DurableError>> {
    p.pause_legacy_source.store(true, Ordering::SeqCst);
    let reader =
        tokio::spawn(async move { read_legacy_branch(&s, "main", Some("selected")).await });
    bounded(p.legacy_source_captured.notified()).await;
    reader
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_source_detects_real_append_navigation_rebind_and_final_read_faults() {
    for mode in 0..6 {
        let (s, p) = seeded().await;
        if mode == 5 {
            s.storage().create_lane("other", None).await.unwrap();
        }
        let reader = captured(s.clone(), &p).await;
        match mode {
            0 => {
                append(
                    &s,
                    "external",
                    "unrelated",
                    Some(json!({"private":"external"})),
                )
                .await;
            }
            1 => {
                s.move_lane("main", None).await.unwrap();
            }
            2 => {
                s.set_storage(p.clone());
            }
            3 => {
                p.fail_revision.store(true, Ordering::SeqCst);
            }
            4 => {
                p.fail_lanes.store(true, Ordering::SeqCst);
            }
            5 => {
                s.append_entry(
                    ProvisionedEntry {
                        id: "other-lane-entry".into(),
                        kind: ProvisionedKind::Custom {
                            custom_type: "unrelated".into(),
                            data: None,
                        },
                    },
                    "other",
                )
                .await
                .unwrap();
                assert_eq!(
                    s.view("main").get_leaf_id().await.unwrap().as_deref(),
                    Some("selected")
                );
            }
            _ => unreachable!(),
        }
        p.release_legacy_source.notify_one();
        let result = bounded(reader).await.unwrap();
        if mode < 3 || mode == 5 {
            assert!(
                matches!(result, Err(DurableError::Conflict)),
                "{mode}: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(DurableError::ReadFailure(_))),
                "{mode}: {result:?}"
            );
        }
        p.fail_revision.store(false, Ordering::SeqCst);
        p.fail_lanes.store(false, Ordering::SeqCst);
        let after_external = audit(&s).await;
        let fresh = bounded(read_legacy_branch(&s, "main", Some("selected")))
            .await
            .unwrap();
        assert_eq!(fresh.canonical_entries.len(), 2);
        assert_eq!(audit(&s).await, after_external);
        no_writes(&p);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_capture_and_storage_failures_never_publish_empty_or_write() {
    let (s, p) = seeded().await;
    let before = audit(&s).await;
    p.fail_reads.store(true, Ordering::SeqCst);
    let error = read_legacy_branch(&s, "main", Some("selected"))
        .await
        .unwrap_err();
    assert!(matches!(error, DurableError::ReadFailure(_)));
    assert!(!error.to_string().contains("injected"));
    p.fail_reads.store(false, Ordering::SeqCst);
    let reader = captured(s.clone(), &p).await;
    reader.abort();
    assert!(bounded(reader).await.unwrap_err().is_cancelled());
    // Read-only future was actually dropped. No worker or admission gate exists.
    let fresh = bounded(read_legacy_branch(&s, "main", Some("selected")))
        .await
        .unwrap();
    assert_eq!(fresh.canonical_entries.len(), 2);
    assert_eq!(audit(&s).await, before);
    no_writes(&p);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_query_bad_variant_identity_order_and_omission_never_return_partial_source() {
    let (s, p) = seeded().await;
    let original = p
        .inner
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(original.len(), 2);
    let mut cases = vec![vec![], vec![original[0].clone()]];
    let mut reverse = original.clone();
    reverse.reverse();
    cases.push(reverse);
    let mut wrong_type = original.clone();
    let mut base = original[0].base().clone();
    base.entry_type = "message".into();
    wrong_type[0] = Entry::Message(MessageEntry {
        base,
        message: rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new("synthetic", 0)),
        terminate: None,
    });
    cases.push(wrong_type);
    for mode in 0..5 {
        let mut entries = original.clone();
        match mode {
            0 => entries[0].base_mut().seq = 0,
            1 => entries[0].base_mut().seq = entries[1].base().seq,
            2 => entries[0].base_mut().seq = u64::MAX,
            3 => entries[0].base_mut().id = "selected".into(),
            4 => entries[0].base_mut().entry_type = "not-custom".into(),
            _ => unreachable!(),
        }
        cases.push(entries);
    }
    let before = audit(&s).await;
    for entries in cases {
        let reader = captured(s.clone(), &p).await;
        *p.legacy_source_override.lock().unwrap() = Some(entries);
        p.release_legacy_source.notify_one();
        assert!(matches!(
            bounded(reader).await.unwrap(),
            Err(DurableError::InvalidJournal(_))
        ));
        assert_eq!(audit(&s).await, before);
        assert_eq!(source(&s, "selected").await.canonical_entries.len(), 2);
        no_writes(&p);
    }
}

#[tokio::test]
async fn extracting_legacy_source_does_not_admit_it_as_native_control_state() {
    let h = bounded(AgentHarness::create(Default::default()))
        .await
        .unwrap();
    append(
        h.session(),
        "legacy-only",
        "durable-state",
        Some(snapshot(1)),
    )
    .await;
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: "ordinary-message-leaf".into(),
                kind: ProvisionedKind::Message {
                    message: rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
                        "private ordinary prompt",
                        0,
                    )),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    let before = audit(h.session()).await;
    let input = source(h.session(), "ordinary-message-leaf").await;
    assert_eq!(input.inspected_custom_entries, 1);
    assert!(!serde_json::to_string(&input)
        .unwrap()
        .contains("private ordinary prompt"));
    assert_eq!(
        legacy::DurableState::try_from_branch(&input.canonical_entries)
            .unwrap()
            .revision,
        1
    );
    let s = DurableService::bind(&h, DurableActor::owner("synthetic-owner").unwrap());
    assert!(matches!(
        s.snapshot().await,
        Err(DurableError::LegacyJournalPresent)
    ));
    assert_eq!(audit(h.session()).await, before);
    h.close().await.unwrap();
}
