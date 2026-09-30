//! Mirrors `packages/ai/src/utils/event-stream.ts` — the SPSC async event
//! stream that carries `AssistantMessageEvent`s from a provider to the agent
//! loop, plus the `result()` future resolving to the final `AssistantMessage`.
//!
//! The TS `EventStream<T,R>` is a single-producer/single-consumer async queue
//! with an attached final-result promise. The Rust port models it as:
//! - an `mpsc::unbounded_channel::<AssistantMessageEvent>` for the events;
//! - a `oneshot::channel::<AssistantMessage>` for the terminal result.
//!
//! `StreamFn` returns synchronously (no `await`); the producer spawns its own
//! task that pushes events and — on the first `Done`/`Error` — sends the
//! carried `AssistantMessage` via the oneshot (first wins; later sends are
//! dropped). Failures are encoded as `Error` events, never panics.

use crate::types::{AssistantMessage, AssistantMessageEvent};
use tokio::sync::{mpsc, oneshot};

/// The consumer side of an `AssistantMessageEventStream`. Mirrors the
/// `AssistantMessageEventStream` class: an async iterator over
/// `AssistantMessageEvent` plus a `result()` future.
///
/// Cloneable so multiple subscribers can drain independently via
/// `tokio::sync::broadcast` at a higher layer; the raw channel is SPSC but the
/// agent loop wraps it so that's rarely needed.
pub struct AssistantMessageEventStream {
    rx: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    result_rx: oneshot::Receiver<AssistantMessage>,
    /// Set true once a terminal `Done`/`Error` event is delivered. Mirrors the
    /// TS `done` flag: subsequent `next()` calls return `None` immediately, so a
    /// still-alive producer can't keep the consumer's `recv()`Blocked forever.
    done: bool,
}

impl AssistantMessageEventStream {
    /// Asynchronously pull the next event, or `None` once the stream is
    /// exhausted (after the terminal `Done`/`Error`). Mirrors the TS
    /// async-iterator's `next()`.
    pub async fn next(&mut self) -> Option<AssistantMessageEvent> {
        if self.done {
            return None;
        }
        let event = self.rx.recv().await?;
        if event.is_terminal() {
            self.done = true;
        }
        Some(event)
    }

    /// Resolve to the final `AssistantMessage` — the message carried by the
    /// terminal `Done` (success) or `Error` (failure). Mirrors TS `result()`.
    ///
    /// Cancellation / producer-drop surfaces as a `RecvError`, mapped to a
    /// terminal error message. A well-behaved producer always sends one
    /// terminal event, so the happy path never hits that branch.
    pub async fn result(self) -> Result<AssistantMessage, RecvError> {
        self.result_rx.await.map_err(|_| RecvError)
    }

    /// Borrow both channels for ad-hoc awaiting (used by the agent loop when
    /// it needs to race the event queue against a cancellation token).
    pub fn split(
        self,
    ) -> (
        mpsc::UnboundedReceiver<AssistantMessageEvent>,
        oneshot::Receiver<AssistantMessage>,
    ) {
        (self.rx, self.result_rx)
    }
}

/// Failure to receive the terminal result: the producer dropped its `result`
/// sender without ever pushing a `Done`/`Error` event (task panic / bug).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvError;

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "assistant-message event stream ended without a terminal Done/Error event"
        )
    }
}

impl std::error::Error for RecvError {}

/// The producer side. Mirrors the TS pusher: `push(ev)` until a terminal event,
/// at which point the result oneshot is fulfilled (first terminal wins; `push`
/// after a terminal event is a no-op matching `if (this.done) return`).
pub struct AssistantMessageEventStreamProducer {
    tx: mpsc::UnboundedSender<AssistantMessageEvent>,
    result_tx: Option<oneshot::Sender<AssistantMessage>>,
    /// Mirrors the TS `done` flag: flipped true once a terminal `Done`/`Error`
    /// has been PUSHED. Subsequent `push` calls short-circuit (no delivery, no
    /// overwrite of the already-fulfilled result oneshot).
    done: bool,
}

impl AssistantMessageEventStreamProducer {
    /// Push an event. Returns `false` if the consumer has dropped the stream
    /// (back-pressure / cancellation). After a terminal `Done`/`Error` is
    /// pushed, further `push` calls are no-ops returning `true` (the stream is
    /// "done" but alive), mirroring TS `push` semantics. The terminal event
    /// itself is always delivered (so the consumer sees it) before the flag
    /// takes effect.
    pub fn push(&mut self, event: AssistantMessageEvent) -> bool {
        // TS: `if (this.done) return;` — a post-terminal push is a no-op.
        if self.done {
            return true;
        }

        // TS: on a terminal event, set `done` + resolve the result oneshot
        // FIRST (the terminal event is still delivered below).
        if event.is_terminal() {
            self.done = true;
            if let AssistantMessageEvent::Done { message, .. } = &event {
                self.fulfill_result(message.clone());
            } else if let AssistantMessageEvent::Error { error, .. } = &event {
                self.fulfill_result(error.clone());
            }
        }

        match self.tx.send(event) {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    fn fulfill_result(&mut self, message: AssistantMessage) {
        if let Some(rx) = self.result_tx.take() {
            // First terminal wins; later `take()` yields None so subsequent
            // terminal events cannot overwrite the result (matches the TS
            // one-shot `resolveFinalResult`).
            let _ = rx.send(message);
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Drop the producer without delivering a terminal event. The consumer's
    /// `next()` returns `None` and `result()` yields `RecvError`. Providers
    /// should push `Error` instead of relying on this; it exists for the
    /// "producer task panicked" safety net.
    pub fn close(self) {
        // Drop closes the mpsc sender; result_tx drop makes result() error.
        drop(self);
    }
}

/// Create a connected producer/consumer pair. Mirrors TS
/// `createAssistantMessageEventStream()`.
pub fn create_assistant_message_event_stream() -> (
    AssistantMessageEventStreamProducer,
    AssistantMessageEventStream,
) {
    let (tx, rx) = mpsc::unbounded_channel::<AssistantMessageEvent>();
    let (result_tx, result_rx) = oneshot::channel::<AssistantMessage>();
    (
        AssistantMessageEventStreamProducer {
            tx,
            result_tx: Some(result_tx),
            done: false,
        },
        AssistantMessageEventStream {
            rx,
            result_rx,
            done: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, DoneReason};
    use std::sync::Arc;

    fn empty_partial() -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage::empty(Api::Faux, "faux", "faux", 0))
    }

    #[tokio::test]
    async fn drain_deltas_then_done() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();

        let partial = empty_partial();
        prod.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        });
        prod.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: partial.clone(),
        });
        prod.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hi".into(),
            partial: partial.clone(),
        });
        prod.push(AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "hi".into(),
            partial: partial.clone(),
        });

        let mut final_msg = (*partial).clone();
        final_msg.stop_reason = crate::types::StopReason::Stop;
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: final_msg.clone(),
        });

        let mut tags = Vec::new();
        while let Some(ev) = stream.next().await {
            tags.push(ev.type_tag());
        }
        assert_eq!(
            tags,
            vec!["start", "text_start", "text_delta", "text_end", "done"]
        );

        let result = stream.result().await.unwrap();
        assert!(matches!(result.stop_reason, crate::types::StopReason::Stop));
    }

    #[tokio::test]
    async fn error_path_resolves_to_error_message() {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let err_msg = AssistantMessage::terminal(
            Api::Faux,
            "faux",
            "faux",
            crate::types::StopReason::Aborted,
            "cancelled",
            0,
        );
        prod.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorReason::Aborted,
            error: err_msg.clone(),
        });
        let result = stream.result().await.unwrap();
        assert!(matches!(
            result.stop_reason,
            crate::types::StopReason::Aborted
        ));
        assert_eq!(result.error_message.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn push_after_terminal_is_noop() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();
        let msg = AssistantMessage::terminal(
            Api::Faux,
            "faux",
            "faux",
            crate::types::StopReason::Stop,
            "",
            0,
        );
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: msg,
        });
        // Post-terminal push should not deliver.
        prod.push(AssistantMessageEvent::Start {
            partial: empty_partial(),
        });

        let first = stream.next().await.unwrap();
        assert!(matches!(first, AssistantMessageEvent::Done { .. }));
        // Stream ends after the terminal event (producer won't send more that land).
        assert!(stream.next().await.is_none());
    }
}
