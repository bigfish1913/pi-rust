//! Actual experimental-native persistence contracts; enabled by the root module's feature gate.
//! Faults below are outer Probe ACK boundaries after a successful inner commit, not power loss.
use super::*;
use rpi_harness::session::experimental_native::ExperimentalNativeRepo;

#[tokio::test]
async fn native_control_receipts_claim_identity_and_reopen() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let store = repo
            .create(&SessionCreateOptions {
                id: Some("durable-control-native-receipts".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let metadata = store.metadata();
        let h = harness(Arc::new(store)).await;
        let s = service(&h);
        let first_request = request(&s, "start", start()).await;
        let first = s.mutate(first_request.clone()).await.unwrap();
        let queued = apply(
            &s,
            "enqueue",
            json!({"action":"enqueue", "text":"native work"}),
        )
        .await;
        let item = queued.snapshot.state.queue[0].id.clone();
        let claimed = apply(
            &s,
            "claim",
            json!({"action":"claim", "item":item, "claimedBy":"spoofed"}),
        )
        .await;
        assert_eq!(
            claimed.snapshot.state.queue[0].claimed_by.as_deref(),
            Some("owner")
        );
        let before_denial = entries(&h).await;
        let revision_before_denial = h.session().storage().history_revision().await.unwrap();
        let child = DurableService::bind(&h, DurableActor::child("wrong-child").unwrap());
        let denied = request(
            &child,
            "wrong-finish",
            json!({"action":"finish", "item":item, "status":"completed"}),
        )
        .await;
        assert!(matches!(
            child.mutate(denied).await,
            Err(DurableError::PermissionDenied)
        ));
        assert!(child
            .request_outcome("wrong-finish")
            .await
            .unwrap()
            .is_none());
        assert_eq!(entries(&h).await, before_denial);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision_before_denial
        );
        drop(child);
        let finished = apply(
            &s,
            "finish",
            json!({"action":"finish", "item":item, "status":"completed"}),
        )
        .await;
        assert_eq!(
            finished.snapshot.state.queue[0].status,
            QueueStatus::Completed
        );
        apply(&s, "pause", json!({"action":"pause"})).await;
        apply(&s, "resume", json!({"action":"resume"})).await;
        apply(&s, "reset", json!({"action":"reset"})).await;
        let latest_request = request(&s, "noop-reset", json!({"action":"reset"})).await;
        let latest = s.mutate(latest_request.clone()).await.unwrap();
        assert!(!latest.receipt.changed);
        assert!(!latest.replayed);
        assert!(latest.snapshot.state.workflow.is_none());
        assert_eq!(latest.snapshot.state.queue, finished.snapshot.state.queue);
        assert_eq!(latest.receipt.request_id, latest_request.request_id);
        assert_eq!(latest.receipt.actor, "owner");
        assert_eq!(latest.receipt.action, latest_request.action);
        assert_eq!(latest.receipt.expected, latest_request.expected);
        assert_eq!(
            s.request_outcome("start").await.unwrap(),
            Some(first.receipt.clone())
        );
        assert_eq!(
            s.request_outcome("noop-reset").await.unwrap(),
            Some(latest.receipt.clone())
        );
        let saved_entries = entries(&h).await;
        let revision = h.session().storage().history_revision().await.unwrap();
        assert_eq!(saved_entries.len(), 8);
        assert_eq!(s.snapshot().await.unwrap(), latest.snapshot);
        assert!(s.mutate(latest_request.clone()).await.unwrap().replayed);
        assert!(matches!(
            s.mutate(first_request.clone()).await,
            Err(DurableError::Conflict)
        ));
        assert_eq!(entries(&h).await, saved_entries);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
        s.drain().await.unwrap();
        drop(s);
        drop(h);
        drop(repo);

        // Compare logical history, not SQLite/WAL file bytes (drain may checkpoint them).
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
        let s = service(&h);
        let reopened = s.snapshot().await.unwrap();
        assert_ne!(
            reopened.scope.binding_token,
            latest.snapshot.scope.binding_token
        );
        assert_eq!(reopened.state, latest.snapshot.state);
        assert_eq!(
            reopened.scope.history_revision,
            latest.snapshot.scope.history_revision
        );
        assert_eq!(
            reopened.scope.control_revision,
            latest.snapshot.scope.control_revision
        );
        assert_eq!(
            s.request_outcome("start").await.unwrap(),
            Some(first.receipt)
        );
        assert_eq!(
            s.request_outcome("noop-reset").await.unwrap(),
            Some(latest.receipt.clone())
        );
        // Latest exact request replays its recorded expected scope, even across a new binding.
        let replay = s.mutate(latest_request).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.receipt, latest.receipt);
        assert_eq!(replay.snapshot, reopened);
        assert!(matches!(
            s.mutate(first_request).await,
            Err(DurableError::Conflict)
        ));
        assert_eq!(entries(&h).await, saved_entries);
        assert_eq!(
            h.session().storage().history_revision().await.unwrap(),
            revision
        );
        s.drain().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn native_committed_ack_faults_and_cancelled_waiter_drain_reopen() {
    bounded(async {
        for mode in [2, 4] {
            let dir = tempfile::tempdir().unwrap();
            let repo =
                ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
                    .await
                    .unwrap();
            let store = repo
                .create(&SessionCreateOptions {
                    id: Some(format!("durable-control-native-ack-{mode}")),
                    ..Default::default()
                })
                .await
                .unwrap();
            let metadata = store.metadata();
            let p = Probe::new(Arc::new(store));
            let h = harness(p.clone()).await;
            let s = service(&h);
            let mutation = request(&s, "committed-start", start()).await;
            p.mode.store(mode, Ordering::SeqCst);
            if mode == 2 {
                assert!(matches!(
                    s.mutate(mutation.clone()).await,
                    Err(DurableError::CommitUncertain(_))
                ));
            } else {
                let worker = {
                    let s = s.clone();
                    let mutation = mutation.clone();
                    tokio::spawn(async move { s.mutate(mutation).await })
                };
                p.committed.notified().await;
                assert_eq!(p.commits.load(Ordering::SeqCst), 1);
                worker.abort();
                assert!(worker.await.unwrap_err().is_cancelled());
                let before = p.drain_calls.load(Ordering::SeqCst);
                let drain = s.drain();
                tokio::pin!(drain);
                // Poll the real same-core drain: cancellation cannot release the owner gate
                // while the accepted inner commit is still waiting to acknowledge.
                assert!(futures::poll!(tokio::task::unconstrained(drain.as_mut())).is_pending());
                assert_eq!(p.drain_calls.load(Ordering::SeqCst), before);
                p.release.notify_one();
                drain.await.unwrap();
                assert_eq!(p.drain_calls.load(Ordering::SeqCst), before + 1);
            }
            let receipt = s.request_outcome("committed-start").await.unwrap().unwrap();
            assert_eq!(receipt.request_id, mutation.request_id);
            assert_eq!(receipt.actor, "owner");
            assert_eq!(receipt.action, mutation.action);
            assert_eq!(receipt.expected, mutation.expected);
            let snapshot = s.snapshot().await.unwrap();
            assert!(snapshot.state.workflow.is_some());
            let saved_entries = entries(&h).await;
            let revision = p.inner.history_revision().await.unwrap();
            assert_eq!(saved_entries.len(), 1);
            let replay = s.mutate(mutation.clone()).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.receipt, receipt);
            assert_eq!(replay.snapshot, snapshot);
            assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(p.commits.load(Ordering::SeqCst), 1);
            assert_eq!(entries(&h).await, saved_entries);
            assert_eq!(p.inner.history_revision().await.unwrap(), revision);
            s.drain().await.unwrap();
            drop(s);
            drop(h);
            drop(p);
            drop(repo);

            let repo =
                ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
                    .await
                    .unwrap();
            let h = harness(Arc::new(repo.open(&metadata).await.unwrap())).await;
            let s = service(&h);
            let reopened = s.snapshot().await.unwrap();
            assert_ne!(reopened.scope.binding_token, snapshot.scope.binding_token);
            assert_eq!(reopened.state, snapshot.state);
            assert_eq!(
                s.request_outcome("committed-start").await.unwrap(),
                Some(receipt.clone())
            );
            let replay = s.mutate(mutation).await.unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.receipt, receipt);
            assert_eq!(replay.snapshot, reopened);
            assert_eq!(entries(&h).await, saved_entries);
            assert_eq!(
                h.session().storage().history_revision().await.unwrap(),
                revision
            );
            s.drain().await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn native_conditional_control_append_rejects_external_history_change() {
    bounded(async {
        let dir = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(SystemClock))
            .await
            .unwrap();
        let store = repo
            .create(&SessionCreateOptions {
                id: Some("durable-control-native-cas".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let p = Probe::new(Arc::new(store));
        let h = harness(p.clone()).await;
        let s = service(&h);
        let mutation = request(&s, "native-cas-rejected", start()).await;
        let original_revision = p.inner.history_revision().await.unwrap();
        p.mode.store(3, Ordering::SeqCst);
        let worker = {
            let s = s.clone();
            tokio::spawn(async move { s.mutate(mutation).await })
        };
        p.entered.notified().await;
        let external = p
            .inner
            .append_entry(
                ProvisionedEntry {
                    id: "native-cas-external".into(),
                    kind: ProvisionedKind::Message {
                        message: UserMessage::new("external native body", 0).into(),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        let external_revision = p.inner.history_revision().await.unwrap();
        assert!(external_revision > original_revision);
        p.release.notify_one();
        // The shared core deliberately classifies a failed append acknowledgement as uncertain;
        // this test separately verifies the actual conditional writer committed no control entry.
        assert!(matches!(
            worker.await.unwrap(),
            Err(DurableError::CommitUncertain(_))
        ));
        assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(p.commits.load(Ordering::SeqCst), 0);
        assert!(s
            .request_outcome("native-cas-rejected")
            .await
            .unwrap()
            .is_none());
        assert!(entries(&h).await.is_empty());
        assert!(s.snapshot().await.unwrap().state.workflow.is_none());
        assert_eq!(p.inner.history_revision().await.unwrap(), external_revision);
        assert_eq!(
            p.inner.get_entry("native-cas-external").await.unwrap(),
            Some(external)
        );
        s.drain().await.unwrap();
    })
    .await;
}
