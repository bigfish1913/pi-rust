//! Mirrors `packages/agent/src/stream-fn.ts` + the `StreamFn` type alias in
//! `packages/agent/src/types.ts`.
//!
//! TS `StreamFn = (model, context, options?) => AssistantMessageEventStream |
//! Promise<AssistantMessageEventStream>` — note it may return the stream
//! *synchronously*. The original Rust [`StreamFn`] retains that contract.
//! [`AsyncStreamFn`] additionally allows a borrowed asynchronous open without
//! blocking a runtime worker or cloning the complete request context.
//! [`StreamSource`] selects either form; both feed the same event/tool loop.
//!
//! `AgentBuilder` lets callers install an explicit `StreamFn`. When omitted,
//! `get_default_stream_fn` is used; like TS, it errors if no default has been
//! installed via `set_default_stream_fn` — `pi-ai` providers are not silently
//! picked so the agent crate stays free of provider wiring.

use futures::future::BoxFuture;
use rpi_ai::provider::SimpleStreamOptions;
use rpi_ai::types::Context;
use rpi_ai::{AssistantMessageEventStream, Model};
use std::sync::{Arc, OnceLock};

/// The provider-boundary callable. Arc'd so `Agent`/`AgentLoopConfig` can clone
/// it cheaply and pass it by reference into `run_agent_loop`.
///
/// Implementations MUST NOT panic or return `Err`: request/model/runtime
/// failures are encoded as `Error` events on the returned stream (mirrors the
/// TS contract). The closure returns synchronously; the producer task it
/// spawns is already detached.
pub type StreamFn = Arc<
    dyn Fn(&Model, &Context, &SimpleStreamOptions) -> AssistantMessageEventStream + Send + Sync,
>;

/// Asynchronous provider opening. The future may borrow model/context/options
/// until it returns the stream; the stream must not retain those borrows.
/// Like [`StreamFn`], encode failures as terminal Error events, not panics.
/// Factories must propagate `options.signal` to any work they spawn. Dropping
/// the opening future on cancellation must release its owned resources.
pub type AsyncStreamFn = Arc<
    dyn for<'a> Fn(
            &'a Model,
            &'a Context,
            &'a SimpleStreamOptions,
        ) -> BoxFuture<'a, AssistantMessageEventStream>
        + Send
        + Sync,
>;

/// A synchronous compatibility factory or native asynchronous factory.
#[derive(Clone)]
pub enum StreamSource {
    Sync(StreamFn),
    Async(AsyncStreamFn),
}

impl From<StreamFn> for StreamSource {
    fn from(factory: StreamFn) -> Self {
        Self::Sync(factory)
    }
}

impl From<AsyncStreamFn> for StreamSource {
    fn from(factory: AsyncStreamFn) -> Self {
        Self::Async(factory)
    }
}

impl StreamSource {
    /// Open the stream without retaining a context clone. Native async opening
    /// is cancellation-aware even if a factory never returns. A synthetic
    /// aborted terminal stream preserves MessageEnd/TurnEnd/AgentEnd lifecycle.
    /// The synchronous branch preserves its original call contract verbatim.
    pub async fn open(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        match self {
            Self::Sync(factory) => factory(model, context, options),
            Self::Async(factory) => {
                tokio::select! {
                    biased;
                    _ = options.signal.cancelled() => {
                        let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
                        let message = rpi_ai::AssistantMessage::terminal(
                            model.api.clone(), &model.provider, &model.id,
                            rpi_ai::StopReason::Aborted, "Aborted while opening provider stream", crate::clock::now_ms(),
                        );
                        producer.push(rpi_ai::AssistantMessageEvent::Error {
                            reason: rpi_ai::ErrorReason::Aborted, error: message,
                        }).await;
                        stream
                    }
                    // Keep factory construction lazy too: pre-cancellation
                    // must not run hooks or begin external work.
                    stream = async { factory(model, context, options).await } => stream,
                }
            }
        }
    }
}

/// Construct a borrowed asynchronous factory. Return `Box::pin(async move { ... })`
/// from the closure, cloning only shared service handles needed by its future.
pub fn async_stream_fn<F>(factory: F) -> AsyncStreamFn
where
    F: for<'a> Fn(
            &'a Model,
            &'a Context,
            &'a SimpleStreamOptions,
        ) -> BoxFuture<'a, AssistantMessageEventStream>
        + Send
        + Sync
        + 'static,
{
    Arc::new(factory)
}

/// Build a `StreamFn` from any `Fn(&Model,&Context,&SimpleStreamOptions) ->
/// AssistantMessageEventStream + Send + Sync`.
pub fn stream_fn<F>(f: F) -> StreamFn
where
    F: Fn(&Model, &Context, &SimpleStreamOptions) -> AssistantMessageEventStream
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

static DEFAULT_STREAM_FN: OnceLock<StreamFn> = OnceLock::new();

/// Install the process-wide fallback used by `Agent`/`AgentBuilder` when no
/// `StreamFn` is supplied. Mirrors TS `setDefaultStreamFn`.
pub fn set_default_stream_fn(stream_fn: Option<StreamFn>) {
    // OnceLock: first writer wins. A `None` install is treated as "leave as-is"
    // if already set, matching TS where the field can be cleared by setting
    // undefined but only before first use; here we simply no-op a None install
    // when a default already exists.
    if let Some(f) = stream_fn {
        let _ = DEFAULT_STREAM_FN.set(f);
    }
}

/// Return the installed default, or `None` if none has been installed. Used by
/// `AgentBuilder::build` to decide whether to defer resolution to call time.
pub fn try_get_default_stream_fn() -> Option<&'static StreamFn> {
    DEFAULT_STREAM_FN.get()
}

/// Return the installed default. Mirrors TS `getDefaultStreamFn` — but instead
/// of throwing we return `None`; the caller (`run_agent_loop`) surfaces a
/// clean `AgentError::State` when a default is required and absent.
pub fn get_default_stream_fn() -> Result<StreamFn, crate::AgentError> {
    DEFAULT_STREAM_FN
        .get()
        .map(|f| Arc::clone(f))
        .ok_or_else(|| {
            crate::AgentError::State(
                "no default stream fn configured — pass one to AgentBuilder or call \
                 set_default_stream_fn"
                    .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_fn_is_arc_clone() {
        let f = stream_fn(|_, _, _| {
            let (_prod, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
            stream
        });
        let _clone: StreamFn = Arc::clone(&f);
    }
}
