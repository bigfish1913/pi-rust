use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn unobserved_events_are_not_serialized_and_stale_payloads_are_freed_once() {
    let registry = crate::ExtensionRegistry::new();
    let snapshot = Arc::new(registry.snapshot());
    let emitter = ExtensionEmitter::new(snapshot.clone(), crate::PluginKeepalive::empty());
    let event = AgentEvent::MessageEnd {
        message: rpi_ai::UserMessage::new("large payload".repeat(1000), 0).into(),
    };
    let before = TRANSLATED_EVENTS.with(|count| count.get());
    emitter.try_emit(event.clone());
    assert_eq!(TRANSLATED_EVENTS.with(|count| count.get()), before);
    let stable = translate(&event).unwrap();
    registry.invalidate();
    let before = FREED_EVENTS.with(|count| count.get());
    dispatch_to_handlers(&snapshot, &stable);
    assert_eq!(FREED_EVENTS.with(|count| count.get()), before + 1);
    emitter.try_emit(event);
    assert_eq!(TRANSLATED_EVENTS.with(|count| count.get()), 1);
}

#[derive(Default)]
struct Paused {
    entered: tokio::sync::Notify,
    release: AtomicBool,
}
extern "C" fn paused_handler(_: StablePluginEvent, data: *mut std::ffi::c_void) -> i32 {
    // SAFETY: one invocation consumes the Arc transferred by this test.
    let state = unsafe { Arc::from_raw(data.cast::<Paused>()) };
    state.entered.notify_one();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !state.release.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    0
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_lifecycle_dispatch_preserves_its_foreign_callback_and_child_capacity() {
    let state = Arc::new(Paused::default());
    let mut registry = crate::ExtensionRegistry::new();
    registry.register_event_handler(
        "paused".into(),
        EventTag::SessionStart,
        paused_handler,
        Arc::into_raw(state.clone()) as *mut std::ffi::c_void,
    );
    let snapshot = registry.snapshot();
    let keepalive = crate::PluginKeepalive::empty();
    let weak = Arc::downgrade(&keepalive);
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let task_scope = scope.clone();
    let task = tokio::spawn(async move {
        task_scope
            .run(dispatch_lifecycle_event(
                &snapshot,
                EventTag::SessionStart,
                keepalive,
            ))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let retained = weak.upgrade().is_some();
    let occupied = tokio::time::timeout(Duration::from_millis(25), scope.wait_idle())
        .await
        .is_err();
    state.release.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(2), async {
        scope.wait_idle().await;
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        retained,
        "cancelling lifecycle dispatch must not unload its executing handler"
    );
    assert!(
        occupied,
        "the handler must retain child capacity until it really returns"
    );
}
