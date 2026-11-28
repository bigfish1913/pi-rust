use super::*;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::atomic::AtomicUsize,
    task::Poll,
    time::Duration,
};

struct DropProbe(Arc<AtomicUsize>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn construction_needs_no_catalog_environment_or_async_runtime() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<HostRuntime<String>>();
    let runtime = HostRuntime::<()>::new();
    assert_eq!(runtime.owner_count(), 0);
    assert_eq!(runtime.phase(AgentId::new()), None);
    assert!(!runtime.is_stopping());
    assert!(!runtime.is_closed());
    runtime.begin_stopping();
    assert!(runtime.is_stopping());
    // A stopping signal alone is never reported as settled/closed.
    assert!(!runtime.is_closed());
    runtime.mark_closed();
    assert!(runtime.is_closed());
}

#[test]
fn shared_table_retains_independent_owners_across_observer_drop_and_close_failure() {
    let runtime = Arc::new(HostRuntime::<DropProbe>::new());
    let observer = runtime.clone();
    let first = AgentId::new();
    let second = AgentId::new();
    let first_drops = Arc::new(AtomicUsize::new(0));
    let second_drops = Arc::new(AtomicUsize::new(0));
    for (id, drops) in [(first, &first_drops), (second, &second_drops)] {
        runtime.entries().insert(
            id,
            OwnerEntry {
                phase: OwnerPhase::Live,
                owner: Some(Arc::new(DropProbe(drops.clone()))),
                failed_start: None,
            },
        );
    }
    observer.entries().get_mut(&first).unwrap().phase = OwnerPhase::CloseFailed;
    assert_eq!(runtime.phase(first), Some(OwnerPhase::CloseFailed));
    assert_eq!(runtime.phase(second), Some(OwnerPhase::Live));
    assert_eq!(runtime.owner_count(), 2);
    drop(observer);
    runtime.begin_stopping();
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);
    assert!(!runtime.is_closed());
    drop(runtime.entries().remove(&first));
    assert_eq!(first_drops.load(Ordering::SeqCst), 1);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.owner_count(), 1);
    assert_eq!(runtime.phase(second), Some(OwnerPhase::Live));
    drop(runtime.entries().remove(&second));
    assert_eq!(second_drops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.owner_count(), 0);
}

#[test]
fn failed_start_retains_cleanup_resources_without_a_live_owner() {
    let runtime = HostRuntime::<(), DropProbe>::new();
    let id = AgentId::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let residue = Arc::new(DropProbe(drops.clone()));
    let weak = Arc::downgrade(&residue);
    runtime.entries().insert(
        id,
        OwnerEntry {
            phase: OwnerPhase::StartFailed,
            owner: None,
            failed_start: Some(residue),
        },
    );
    runtime.begin_stopping();
    assert_eq!(runtime.owner_count(), 1);
    assert_eq!(runtime.phase(id), Some(OwnerPhase::StartFailed));
    assert!(runtime.entries().get(&id).unwrap().owner.is_none());
    assert!(Arc::ptr_eq(
        runtime
            .entries()
            .get(&id)
            .unwrap()
            .failed_start
            .as_ref()
            .unwrap(),
        &weak.upgrade().unwrap(),
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(!runtime.is_closed());
    drop(runtime.entries().remove(&id));
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn poisoned_entry_lock_does_not_discard_native_ownership() {
    let runtime = HostRuntime::<DropProbe>::new();
    let id = AgentId::new();
    let drops = Arc::new(AtomicUsize::new(0));
    runtime.entries().insert(
        id,
        OwnerEntry {
            phase: OwnerPhase::Live,
            owner: Some(Arc::new(DropProbe(drops.clone()))),
            failed_start: None,
        },
    );
    assert!(catch_unwind(AssertUnwindSafe(|| {
        let _guard = runtime.entries();
        panic!("injected registry panic");
    }))
    .is_err());
    assert_eq!(runtime.owner_count(), 1);
    assert_eq!(runtime.phase(id), Some(OwnerPhase::Live));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(runtime.entries().remove(&id));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lifecycle_readers_coexist_and_queued_stop_fences_later_readers() {
    let runtime = HostRuntime::<()>::new();
    let first = runtime.lifecycle.read().await;
    let second = runtime.lifecycle.read().await;
    runtime.begin_stopping();
    let mut stop = Box::pin(runtime.lifecycle.write());
    assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
    let mut later = Box::pin(runtime.lifecycle.read());
    assert!(matches!(futures::poll!(later.as_mut()), Poll::Pending));
    drop(first);
    assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
    drop(second);
    let exclusive = tokio::time::timeout(Duration::from_secs(1), stop)
        .await
        .expect("stop must acquire after both existing transactions finish");
    assert!(matches!(futures::poll!(later.as_mut()), Poll::Pending));
    // The adapter, not a reader drop or a stop signal, establishes closure.
    assert!(!runtime.is_closed());
    runtime.mark_closed();
    drop(exclusive);
    let _later = tokio::time::timeout(Duration::from_secs(1), later)
        .await
        .expect("queued reader must be released after stop");
    assert!(runtime.is_stopping());
    assert!(runtime.is_closed());
}
