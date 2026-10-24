//! Bounded synchronous-producer/async-consumer bridge. Payload is accounted in
//! incoming JSON bytes, not allocator overhead or total process memory.
use super::{ProviderControlState, StreamChunk};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

#[cfg(test)]
static PEAK_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(test, feature = "provider-performance"))]
pub(super) fn peak_occupancy() -> (usize, usize) {
    use std::sync::atomic::Ordering;
    (
        PEAK_COUNT.load(Ordering::Relaxed),
        PEAK_BYTES.load(Ordering::Relaxed),
    )
}

struct State {
    chunks: VecDeque<(StreamChunk, usize)>,
    bytes: usize,
    producer_open: bool,
    consumer_open: bool,
    cancelled: bool,
    peak_count: usize,
    peak_bytes: usize,
}
pub(super) struct Queue {
    state: Mutex<State>,
    capacity: usize,
    byte_limit: usize,
    writable: Condvar,
    readable: Notify,
}
pub(super) struct Sender(Arc<Queue>);
pub(super) struct Receiver {
    queue: Arc<Queue>,
    control: Arc<ProviderControlState>,
}

pub(super) fn channel(
    capacity: usize,
    byte_limit: usize,
    control: Arc<ProviderControlState>,
) -> (Sender, Receiver) {
    assert!(capacity > 0 && byte_limit > 0);
    let queue = Arc::new(Queue {
        state: Mutex::new(State {
            chunks: VecDeque::new(),
            bytes: 0,
            producer_open: true,
            consumer_open: true,
            cancelled: false,
            peak_count: 0,
            peak_bytes: 0,
        }),
        capacity,
        byte_limit,
        writable: Condvar::new(),
        readable: Notify::new(),
    });
    *control.queue.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&queue));
    (Sender(Arc::clone(&queue)), Receiver { queue, control })
}

impl Queue {
    pub(super) fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        self.writable.notify_all();
        self.readable.notify_one();
    }
}
impl Sender {
    pub(super) fn send(&self, chunk: StreamChunk, bytes: usize) -> Result<(), ()> {
        let weight = bytes.max(1).min(self.0.byte_limit);
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.consumer_open
            && !state.cancelled
            && (state.chunks.len() == self.0.capacity || weight > self.0.byte_limit - state.bytes)
        {
            state = self
                .0
                .writable
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if !state.consumer_open || state.cancelled {
            return Err(());
        }
        state.bytes += weight;
        state.chunks.push_back((chunk, weight));
        state.peak_count = state.peak_count.max(state.chunks.len());
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            PEAK_COUNT.fetch_max(state.chunks.len(), Ordering::Relaxed);
            PEAK_BYTES.fetch_max(state.bytes, Ordering::Relaxed);
        }
        self.0.readable.notify_one();
        Ok(())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .producer_open = false;
        self.0.readable.notify_one();
    }
}
impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<StreamChunk> {
        loop {
            let ready = self.queue.readable.notified();
            {
                let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.cancelled || !state.consumer_open {
                    return None;
                }
                if let Some((chunk, bytes)) = state.chunks.pop_front() {
                    state.bytes -= bytes;
                    self.queue.writable.notify_all();
                    return Some(chunk);
                }
                if !state.producer_open {
                    return None;
                }
            }
            ready.await;
        }
    }
    pub(super) fn close(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.consumer_open = false;
        state.chunks.clear();
        state.bytes = 0;
        self.queue.writable.notify_all();
    }
    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<StreamChunk, ()> {
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        let (chunk, bytes) = state.chunks.pop_front().ok_or(())?;
        state.bytes -= bytes;
        self.queue.writable.notify_all();
        Ok(chunk)
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.close();
        self.control.cancel();
        let state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        tracing::debug!(
            peak_events = state.peak_count,
            peak_payload_bytes = state.peak_bytes,
            "plugin provider bridge queue closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn capacity_one_dequeues_losslessly_and_wakes_blocking_producer() {
        let control = Arc::new(ProviderControlState::default());
        let (tx, mut rx) = channel(1, 4, control);
        tx.send(serde_json::json!({"i":0}), 10).unwrap(); // oversized occupies the budget exclusively
        let thread = std::thread::spawn(move || {
            tx.send(serde_json::json!({"i":1}), 1).unwrap();
        });
        assert_eq!(rx.recv().await.unwrap()["i"], 0);
        assert_eq!(rx.recv().await.unwrap()["i"], 1);
        thread.join().unwrap();
        assert!(rx.recv().await.is_none());
        let state = rx.queue.state.lock().unwrap();
        assert_eq!(state.peak_count, 1);
        assert_eq!(state.peak_bytes, 4);
    }

    #[test]
    fn cancellation_and_receiver_drop_release_capacity_waits() {
        for cancel in [false, true] {
            let control = Arc::new(ProviderControlState::default());
            let (tx, rx) = channel(1, 4, Arc::clone(&control));
            tx.send(serde_json::json!({}), 4).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                done_tx.send(tx.send(serde_json::json!({}), 1)).unwrap()
            });
            assert!(done_rx.recv_timeout(Duration::from_millis(10)).is_err());
            if cancel {
                control.cancel();
            } else {
                drop(rx);
            }
            assert!(done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err());
            thread.join().unwrap();
        }
    }
}
