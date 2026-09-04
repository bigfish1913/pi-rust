//! `examples/minimal` — a one-file agent run against the faux provider.
//!
//! Mirrors the plan's M2 `examples/minimal/src/main.rs`: build an `Agent` whose
//! `stream_fn` delegates to a `FauxProvider` scripted with a single text reply,
//! prompt it with "hi", and print the resulting transcript. No real LLM is
//! contacted — the faux provider is a deterministic in-process stand-in.

use std::sync::Arc;

use rpi_agent::AgentBuilder;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

#[tokio::main]
async fn main() {
    // A faux provider scripted to reply with exactly one text turn. It streams
    // Start → per-block deltas → Done, exercising the same
    // MessageStart/MessageUpdate*/MessageEnd path a real provider would.
    let script = FauxScript::new().with_text("Hello from the faux provider!");
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();

    // Build a minimal agent whose stream_fn delegates to the faux provider.
    let agent = AgentBuilder::new()
        .model(model)
        .stream_fn(make_stream_fn(provider))
        .build()
        .expect("agent builds");

    // Subscribe to the event stream before prompting so the run's lifecycle
    // events are observed.
    let mut rx = agent.subscribe();

    // Prompt and wait for the run to finish.
    agent.prompt("hi").await.expect("prompt completes");

    // Drain any events that landed in the broadcast buffer.
    let mut saw_end = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, rpi_agent::AgentEvent::AgentEnd { .. }) {
            saw_end = true;
        }
    }

    let state = agent.state();
    println!(
        "minimal example: {} messages after run",
        state.messages.len()
    );
    for m in &state.messages {
        println!("  - {}", m.role().as_str());
    }
    if let Some(rpi_agent::AgentMessage::Assistant(a)) = state.messages.last() {
        let text: String = a
            .content
            .iter()
            .filter_map(|c| match c {
                rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect();
        println!("assistant reply: {text}");
    }
    assert!(saw_end, "run should have emitted AgentEnd");
    println!("AgentEnd observed.");
}

/// Build a `StreamFn` that delegates to a faux `Provider::stream_simple`.
///
/// `StreamFn` must return synchronously; `Provider::stream_simple` is `async`.
/// The faux implementation spawns its producer task before the async call
/// resolves, so the returned `AssistantMessageEventStream` is immediately
/// usable. We bridge the sync/async gap with `block_in_place` + `block_on` —
/// acceptable in an example `main`, and the pattern the `tokio` docs endorse
/// for a sync wrapper over an async producer whose result is a live stream.
fn make_stream_fn(provider: Arc<FauxProvider>) -> rpi_agent::StreamFn {
    rpi_agent::stream_fn(move |model, ctx, opts| {
        let p = Arc::clone(&provider);
        let model = model.clone();
        let ctx = ctx.clone();
        let opts = opts.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { p.stream_simple(&model, &ctx, &opts).await })
        })
    })
}
