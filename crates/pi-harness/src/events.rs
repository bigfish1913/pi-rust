//! Mirrors `packages/agent/src/harness/events.ts` — the harness event bus,
//! `RunStart`/`RunEnd` events, typed `on` listeners, and the `WatchHandle`
//! (snapshot + buffered-until-start + unsubscribe) used by lane/session watches.
//!
//! The TS bus is synchronous (`emit` calls listeners inline, async results
//! fire-and-forget). This port keeps the same shape: [`HarnessEventBus::emit`]
//! invokes callbacks inline. To stay reentrancy-safe under Rust's locks (a TS
//! listener can `emit` reentrantly with no lock in the way), callbacks are
//! clone-Arc'd out of the mutex and invoked with the lock dropped — so a
//! reentrant `emit` from inside a listener never deadlocks.
//!
//! The watch `start()` drains the buffer in a reentrancy-safe loop (mirrors
//! the TS `while (buffered.length > 0)` with reassignment): `started` stays
//! `false` during the flush so reentrant emissions re-buffer and are picked up
//! by the next loop iteration; only after the buffer drains does the watch go
//! live.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// `run_start` event. Mirrors TS `RunStartEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStartEvent {
    pub lane: String,
    pub run_id: String,
}

/// Terminal state of a run. Mirrors TS `"completed" | "aborted" | "failed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Aborted => "aborted",
            RunOutcome::Failed => "failed",
        }
    }
}

/// `run_end` event payload. Mirrors TS `RunEndEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEndEvent {
    pub lane: String,
    pub run_id: String,
    pub outcome: RunOutcome,
    pub leaf_id: String,
}

/// The harness event union. Mirrors TS `HarnessEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessEvent {
    RunStart(RunStartEvent),
    RunEnd(RunEndEvent),
}

impl HarnessEvent {
    /// Stable type tag. Mirrors TS `HarnessEvent["type"]`.
    pub fn type_tag(&self) -> &'static str {
        match self {
            HarnessEvent::RunStart(_) => "run_start",
            HarnessEvent::RunEnd(_) => "run_end",
        }
    }
}

impl From<RunStartEvent> for HarnessEvent {
    fn from(e: RunStartEvent) -> Self {
        HarnessEvent::RunStart(e)
    }
}
impl From<RunEndEvent> for HarnessEvent {
    fn from(e: RunEndEvent) -> Self {
        HarnessEvent::RunEnd(e)
    }
}

/// Trait linking an event type to its bus tag + downcast. Mirrors TS
/// `HarnessEventOfType<TType>` (Extract by type tag).
pub trait HarnessEventOf: Sized {
    const TYPE_TAG: &'static str;
    fn downcast(event: &HarnessEvent) -> Option<&Self>;
}

impl HarnessEventOf for RunStartEvent {
    const TYPE_TAG: &'static str = "run_start";
    fn downcast(event: &HarnessEvent) -> Option<&Self> {
        match event {
            HarnessEvent::RunStart(e) => Some(e),
            _ => None,
        }
    }
}

impl HarnessEventOf for RunEndEvent {
    const TYPE_TAG: &'static str = "run_end";
    fn downcast(event: &HarnessEvent) -> Option<&Self> {
        match event {
            HarnessEvent::RunEnd(e) => Some(e),
            _ => None,
        }
    }
}

/// Erased direct listener stored per type tag. `Arc` so `emit` can clone it
/// out of the mutex and invoke with the lock dropped (reentrancy-safe).
type DirectListener = Arc<dyn Fn(&HarnessEvent) + Send + Sync>;

/// A watch-side live listener (installed by `WatchHandle::start`). `Arc` for
/// the same clone-out-and-invoke-unlocked reason.
pub type WatchListener = Arc<dyn Fn(&HarnessEvent) + Send + Sync>;

struct ListenerEntry {
    id: u64,
    f: DirectListener,
}

/// Per-watch delivery state, shared between the bus's emit path and the
/// [`WatchHandle`].
struct WatchDelivery {
    buffer: Vec<HarnessEvent>,
    started: bool,
    listener: Option<WatchListener>,
}

struct BusInner {
    /// Per-type-tag direct listeners.
    listeners: HashMap<&'static str, Vec<ListenerEntry>>,
    /// Watch slots — every event is delivered to each. Strong refs so a watch
    /// stays alive until `WatchHandle::unsubscribe` (mirrors TS: dropping the
    /// handle without unsubscribing leaves the watch active).
    watch_listeners: Vec<Arc<Mutex<WatchDelivery>>>,
}

/// The harness event bus. Mirrors TS `HarnessEventBus`.
pub struct HarnessEventBus {
    inner: Arc<Mutex<BusInner>>,
}

impl HarnessEventBus {
    /// Construct an empty bus.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner {
                listeners: HashMap::new(),
                watch_listeners: Vec::new(),
            })),
        }
    }

    /// Register a typed listener for one event type and return an unsubscribe
    /// guard. Earlier events are not replayed. Mirrors TS `Events.on`.
    ///
    /// The returned guard is `'static` (holds a `Weak` to the bus), so it can
    /// be stored anywhere; dropping it unregisters the listener.
    pub fn on<T, F>(&self, listener: F) -> OnUnsubscribe
    where
        T: HarnessEventOf,
        F: Fn(&T) + Send + Sync + 'static,
    {
        let type_tag = T::TYPE_TAG;
        let id = next_listener_id();
        let wrapper: DirectListener = Arc::new(move |event: &HarnessEvent| {
            if let Some(typed) = T::downcast(event) {
                listener(typed);
            }
        });
        {
            let mut inner = self.inner.lock().unwrap();
            inner.listeners.entry(type_tag).or_default().push(ListenerEntry { id, f: wrapper });
        }
        OnUnsubscribe {
            inner: Arc::downgrade(&self.inner),
            type_tag,
            id,
            taken: false,
        }
    }

    /// Publish an event to direct listeners for its type AND to all watch
    /// listeners. Mirrors TS `emit`. Synchronous; callbacks run inline.
    pub fn emit(&self, event: &HarnessEvent) {
        // Clone callbacks out of the mutex, then invoke with the lock dropped
        // so a reentrant `emit` from inside a listener cannot deadlock.
        let (direct, watches): (Vec<DirectListener>, Vec<Arc<Mutex<WatchDelivery>>>) = {
            let inner = self.inner.lock().unwrap();
            let direct = inner
                .listeners
                .get(event.type_tag())
                .map(|v| v.iter().map(|e| e.f.clone()).collect())
                .unwrap_or_default();
            let watches = inner.watch_listeners.clone();
            (direct, watches)
        };
        for f in &direct {
            f(event);
        }
        for slot in &watches {
            let live = {
                let mut del = slot.lock().unwrap();
                if del.started {
                    del.listener.clone()
                } else {
                    del.buffer.push(event.clone());
                    None
                }
            };
            if let Some(l) = live {
                l(event);
            }
        }
    }

    /// Register a watch that captures a snapshot NOW and buffers events until
    /// [`WatchHandle::start`] is called. Mirrors TS `HarnessEventBus.watch`.
    ///
    /// The watch is registered (in buffering mode) BEFORE `capture_snapshot`
    /// runs, so any `emit` inside the snapshot closure is buffered — matching
    /// the TS test "captures a snapshot without an event gap".
    pub fn watch<T, F>(&self, capture_snapshot: F) -> WatchHandle<T>
    where
        F: FnOnce() -> T,
    {
        let slot = Arc::new(Mutex::new(WatchDelivery {
            buffer: Vec::new(),
            started: false,
            listener: None,
        }));
        {
            let mut inner = self.inner.lock().unwrap();
            inner.watch_listeners.push(slot.clone());
        }
        let snapshot = capture_snapshot();
        WatchHandle {
            snapshot: Some(snapshot),
            slot,
            bus: Arc::downgrade(&self.inner),
            unsubscribed: false,
        }
    }
}

impl Default for HarnessEventBus {
    fn default() -> Self {
        Self::new()
    }
}

fn next_listener_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Guard returned by [`HarnessEventBus::on`]; dropping it unregisters the
/// listener. Mirrors the TS `() => void` unsubscribe function. `'static` so it
/// can be stored independently of the bus borrow.
pub struct OnUnsubscribe {
    inner: Weak<Mutex<BusInner>>,
    type_tag: &'static str,
    id: u64,
    taken: bool,
}

impl OnUnsubscribe {
    /// Manually unsubscribe (equivalent to dropping the guard).
    pub fn off(&mut self) {
        if self.taken {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            let mut inner = inner.lock().unwrap();
            if let Some(vec) = inner.listeners.get_mut(self.type_tag) {
                vec.retain(|e| e.id != self.id);
                if vec.is_empty() {
                    inner.listeners.remove(self.type_tag);
                }
            }
        }
        self.taken = true;
    }
}

impl Drop for OnUnsubscribe {
    fn drop(&mut self) {
        self.off();
    }
}

/// Watch handle. Mirrors TS `WatchHandle<TSnapshot>`. Carries the snapshot
/// captured at registration; [`Self::start`] installs the live listener
/// (flushing the buffered events first); [`Self::unsubscribe`] removes the
/// watch from the bus.
pub struct WatchHandle<T> {
    snapshot: Option<T>,
    slot: Arc<Mutex<WatchDelivery>>,
    bus: Weak<Mutex<BusInner>>,
    unsubscribed: bool,
}

impl<T> WatchHandle<T> {
    /// The snapshot captured at registration. Panics if taken twice.
    pub fn snapshot(&self) -> &T {
        self.snapshot.as_ref().expect("watch snapshot already taken")
    }

    /// Move the snapshot out (the TS `WatchHandle.snapshot` is a plain field).
    pub fn take_snapshot(&mut self) -> T {
        self.snapshot.take().expect("watch snapshot already taken")
    }

    /// Install the live listener and flush any buffered events to it in order.
    /// Mirrors TS `WatchHandle.start`. Reentrancy-safe: `started` stays false
    /// during the flush so reentrant emissions re-buffer and are drained by the
    /// next loop iteration; the watch goes live only after the buffer is empty.
    pub fn start(&mut self, listener: WatchListener) {
        let l = listener.clone();
        // Drain buffered events first (started still false → reentrant emits
        // during delivery re-buffer and are picked up by the next iteration).
        loop {
            let pending: Vec<HarnessEvent> = {
                let mut del = self.slot.lock().unwrap();
                std::mem::take(&mut del.buffer)
            };
            if pending.is_empty() {
                break;
            }
            for ev in &pending {
                l(ev);
            }
        }
        // Now go live.
        {
            let mut del = self.slot.lock().unwrap();
            del.listener = Some(listener);
            del.started = true;
        }
    }

    /// Remove this watch from the bus and clear its buffer. Mirrors TS
    /// `WatchHandle.unsubscribe`. Idempotent.
    pub fn unsubscribe(&mut self) {
        if self.unsubscribed {
            return;
        }
        if let Some(inner) = self.bus.upgrade() {
            let mut inner = inner.lock().unwrap();
            inner.watch_listeners.retain(|s| !Arc::ptr_eq(s, &self.slot));
        }
        {
            let mut del = self.slot.lock().unwrap();
            del.buffer.clear();
            del.listener = None;
            del.started = false;
        }
        self.unsubscribed = true;
    }
}

impl<T> Drop for WatchHandle<T> {
    fn drop(&mut self) {
        // Mirrors TS: the handle does NOT auto-unsubscribe on drop (the bus
        // keeps the watch alive until `unsubscribe` is called). We intentionally
        // do nothing here. Callers must call `unsubscribe`.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_start() -> HarnessEvent {
        HarnessEvent::RunStart(RunStartEvent { lane: "main".into(), run_id: "run-1".into() })
    }

    fn run_end() -> HarnessEvent {
        HarnessEvent::RunEnd(RunEndEvent {
            lane: "main".into(),
            run_id: "run-1".into(),
            outcome: RunOutcome::Completed,
            leaf_id: "entry-1".into(),
        })
    }

    #[test]
    fn delivers_matching_events_to_direct_listeners_and_watchers() {
        let events = HarnessEventBus::new();
        let direct: Arc<Mutex<Vec<RunStartEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let watch_events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let direct_clone = direct.clone();
        let off = events.on::<RunStartEvent, _>(move |e| direct_clone.lock().unwrap().push(e.clone()));
        let watch_events_clone = watch_events.clone();
        let mut watch = events.watch(|| ());
        watch.start(Arc::new(move |event| watch_events_clone.lock().unwrap().push(event.clone())));

        events.emit(&run_start());
        events.emit(&run_end());
        drop(off);
        events.emit(&run_start());

        assert_eq!(direct.lock().unwrap().len(), 1);
        assert_eq!(
            watch_events.lock().unwrap().as_slice(),
            &[run_start(), run_end(), run_start()]
        );
    }

    #[test]
    fn snapshot_then_flush_then_live() {
        // Mirrors TS "captures a snapshot without an event gap, then flushes
        // and delivers live events": the snapshot closure emits run_start,
        // which must be buffered; start() flushes it; subsequent emits go live.
        let events = HarnessEventBus::new();
        let expected_snapshot = Arc::new(42i32);
        let events_for_closure = events_clone_for_closure(&events);
        let mut watch = events.watch(|| {
            events_for_closure.emit(&run_start());
            expected_snapshot.clone()
        });
        let received: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        // Snapshot was captured (the TS test asserts `watch.snapshot === expected`).
        assert_eq!(**watch.snapshot(), *expected_snapshot);
        assert!(received.lock().unwrap().is_empty());
        watch.start(Arc::new(move |event| received_clone.lock().unwrap().push(event.clone())));
        assert_eq!(received.lock().unwrap().as_slice(), &[run_start()]);
        events.emit(&run_end());
        assert_eq!(received.lock().unwrap().as_slice(), &[run_start(), run_end()]);
        watch.unsubscribe();
        events.emit(&run_start());
        assert_eq!(received.lock().unwrap().as_slice(), &[run_start(), run_end()]);
    }

    /// `HarnessEventBus` is not `Clone`, but the snapshot-closure test needs to
    /// emit from within the closure. We expose a thin emitter handle that
    /// shares the inner `Arc`. (In the TS test `events` is captured directly;
    /// Rust's borrow checker can't prove the borrow is safe, so we use this.)
    struct EmitterHandle {
        inner: Arc<Mutex<BusInner>>,
    }
    impl EmitterHandle {
        fn emit(&self, event: &HarnessEvent) {
            let (direct, watches): (Vec<DirectListener>, Vec<Arc<Mutex<WatchDelivery>>>) = {
                let inner = self.inner.lock().unwrap();
                let direct = inner
                    .listeners
                    .get(event.type_tag())
                    .map(|v| v.iter().map(|e| e.f.clone()).collect())
                    .unwrap_or_default();
                let watches = inner.watch_listeners.clone();
                (direct, watches)
            };
            for f in &direct {
                f(event);
            }
            for slot in &watches {
                let live = {
                    let mut del = slot.lock().unwrap();
                    if del.started {
                        del.listener.clone()
                    } else {
                        del.buffer.push(event.clone());
                        None
                    }
                };
                if let Some(l) = live {
                    l(event);
                }
            }
        }
    }

    fn events_clone_for_closure(bus: &HarnessEventBus) -> EmitterHandle {
        EmitterHandle { inner: bus.inner.clone() }
    }
}
