//! Closed/idle binding retirement is separate from resource settlement.
use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Weak,
};

struct DropProbe {
    harness: Mutex<Option<AgentHarness>>,
    dropped: Arc<AtomicBool>,
}
impl Drop for DropProbe {
    fn drop(&mut self) {
        let harness = self.harness.get_mut().unwrap().take().unwrap();
        assert!(
            harness.inner.try_lock().is_ok(),
            "binding dropped under inner lock"
        );
        assert!(
            harness.message_writes.try_lock().is_ok(),
            "binding dropped under write gate"
        );
        self.dropped.store(true, Ordering::SeqCst);
    }
}
async fn fixture() -> (AgentHarness, Weak<DropProbe>, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = Arc::new(DropProbe {
        harness: Mutex::new(None),
        dropped: dropped.clone(),
    });
    let retained = probe.clone();
    let after: AfterToolCall = Arc::new(move |_, _| {
        let retained = retained.clone();
        Box::pin(async move {
            drop(retained);
            None
        })
    });
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        after_tool_call: Some(after),
        ..Default::default()
    })
    .await
    .unwrap();
    // Deliberately reentrant destruction: a final binding destructor must be
    // able to inspect the harness without either host lock being held.
    *probe.harness.lock().unwrap() = Some(harness.clone());
    let weak = Arc::downgrade(&probe);
    drop(probe);
    (harness, weak, dropped)
}

#[tokio::test]
async fn requires_close_and_retires_exact_bindings_outside_locks_without_losing_recovery_data() {
    let (harness, probe, dropped) = fixture().await;
    let session_id = harness.session_id();
    harness.seal_resources(Vec::new()).unwrap();
    harness.keep_unsaved_message_for_test(
        "retirement fixture",
        rpi_ai::UserMessage::new("must survive", 1).into(),
    );
    harness
        .write_failures
        .lock()
        .unwrap()
        .push(HarnessWriteFailure {
            what: "fixture".into(),
            message: "must survive".into(),
            at: 1,
        });
    assert!(harness.retire_runtime_bindings().await.is_err());
    assert!(probe.upgrade().is_some());
    harness.close().await.unwrap();
    assert!(
        probe.upgrade().is_some(),
        "close alone must retain bindings"
    );
    harness.retire_runtime_bindings().await.unwrap();
    harness.retire_runtime_bindings().await.unwrap();
    assert!(probe.upgrade().is_none());
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(harness.session_id(), session_id);
    assert!(harness.has_exact_resources());
    assert_eq!(harness.unsaved_messages().len(), 1);
    assert_eq!(harness.take_write_failures().len(), 1);
    assert!(harness.prompt_text("closed", vec![]).await.is_err());
}

#[tokio::test]
async fn closed_but_active_admission_cannot_dispose_live_bindings() {
    let (harness, probe, dropped) = fixture().await;
    let admission = harness.acquire_run(OperationKind::Run).await.unwrap();
    harness.close().await.unwrap();
    assert!(matches!(
        harness.retire_runtime_bindings().await,
        Err(HarnessError::LaneBusy { .. })
    ));
    assert!(probe.upgrade().is_some());
    assert!(!dropped.load(Ordering::SeqCst));
    harness.release_run(&admission.0);
    harness.retire_runtime_bindings().await.unwrap();
    assert!(probe.upgrade().is_none());
}

#[tokio::test]
async fn cancelling_retirement_waiting_for_writes_does_not_discard_bindings() {
    let (harness, probe, dropped) = fixture().await;
    harness.close().await.unwrap();
    let writes = harness.message_writes.lock().await;
    let started = CancellationToken::new();
    let signal = started.clone();
    let clone = harness.clone();
    let waiter = tokio::spawn(async move {
        signal.cancel();
        clone.retire_runtime_bindings().await
    });
    started.cancelled().await;
    waiter.abort();
    let _ = waiter.await;
    assert!(probe.upgrade().is_some());
    assert!(!dropped.load(Ordering::SeqCst));
    drop(writes);
    harness.retire_runtime_bindings().await.unwrap();
    assert!(probe.upgrade().is_none());
}
