use super::*;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn precommit_failure_and_lost_ack_reconcile_exact_request_without_extra_append() {
    for disk in [false, true] {
        for mode in [1, 2] {
            let (session, _dir) = fixture::fixture("writer-ack", disk).await;
            let p = probe::Probe::new(session.storage());
            let h = open(Session::new(p.clone(), None), Some(policy(EPOCH))).await;
            let s = owner(&h);
            let mutation = request(&s, "uncertain", start(), None).await;
            p.mode.store(mode, Ordering::SeqCst);
            assert!(matches!(
                s.mutate(mutation.clone()).await,
                Err(DurableError::CommitUncertain(_))
            ));
            assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(p.commits.load(Ordering::SeqCst), usize::from(mode == 2));
            assert_eq!(
                s.request_outcome("uncertain").await.unwrap().is_some(),
                mode == 2
            );
            let retried = s.mutate(mutation).await.unwrap();
            assert_eq!(retried.replayed, mode == 2);
            assert_eq!(
                p.attempts.load(Ordering::SeqCst),
                if mode == 2 { 1 } else { 2 }
            );
            assert_eq!(p.commits.load(Ordering::SeqCst), 1);
            assert_eq!(
                s.request_outcome("uncertain").await.unwrap(),
                Some(retried.receipt)
            );
            assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 1);
        }
    }
}

#[tokio::test]
async fn canceled_caller_keeps_owned_writer_and_drain_waits_original_gate() {
    for disk in [false, true] {
        let (session, _dir) = fixture::fixture("writer-cancel", disk).await;
        let p = probe::Probe::new(session.storage());
        let h = open(Session::new(p.clone(), None), Some(policy(EPOCH))).await;
        let s = owner(&h);
        let mutation = request(&s, "owned", start(), None).await;
        p.mode.store(3, Ordering::SeqCst);
        let job = tokio::spawn({
            let s = s.clone();
            async move { s.mutate(mutation).await }
        });
        bounded(p.entered.notified()).await;
        job.abort();
        let _ = job.await;
        let drain = tokio::spawn({
            let s = s.clone();
            async move { s.drain().await }
        });
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        p.release.notify_one();
        bounded(drain).await.unwrap().unwrap();
        assert_eq!(p.commits.load(Ordering::SeqCst), 1);
        assert_eq!(p.attempts.load(Ordering::SeqCst), 1);
        assert!(s.request_outcome("owned").await.unwrap().is_some());
        assert_eq!(custom_entries(&h, CUSTOM_TYPE).await.len(), 1);
    }
}

#[tokio::test]
async fn ack_rebind_or_navigation_never_reports_hidden_receipt_as_success() {
    for rebind in [false, true] {
        let original = memory_session("writer-original");
        let p = probe::Probe::new(original.storage());
        let h = open(Session::new(p.clone(), None), Some(policy(EPOCH))).await;
        let s = owner(&h);
        let mutation = request(&s, "hidden", start(), None).await;
        p.mode.store(4, Ordering::SeqCst);
        let job = tokio::spawn({
            let s = s.clone();
            async move { s.mutate(mutation).await }
        });
        bounded(p.committed.notified()).await;
        if rebind {
            // Same logical session ID deliberately does not imply same binding.
            h.session()
                .set_storage(memory_session("writer-original").storage());
        } else {
            h.session().move_lane("main", None).await.unwrap();
        }
        p.release.notify_one();
        assert!(matches!(
            bounded(job).await.unwrap(),
            Err(DurableError::CommitUncertain(_))
        ));
        assert!(s.request_outcome("hidden").await.unwrap().is_none());
        if !rebind {
            let persisted = custom_entries(&h, CUSTOM_TYPE).await;
            assert_eq!(persisted.len(), 1);
            h.session()
                .move_lane("main", Some(persisted[0].id()))
                .await
                .unwrap();
        }
        let reopened = open(Session::new(p.inner.clone(), None), Some(policy(EPOCH))).await;
        assert!(owner(&reopened)
            .request_outcome("hidden")
            .await
            .unwrap()
            .is_some());
        assert_eq!(p.commits.load(Ordering::SeqCst), 1);
    }
}
