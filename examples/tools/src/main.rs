//! `examples/tools` — a one-file agent run that wires the built-in `write` and
//! `bash` tools to a real `OsExecutionEnv` and drives them with a faux provider
//! scripted to issue a single `write` tool call. Mirrors the plan's M4
//! `examples/tools/src/main.rs`:
//!
//! - Build an `OsExecutionEnv` rooted at a fresh tempdir.
//! - Construct `write` + `bash` tools via `ExecutionToolContext`.
//! - A faux provider's first scripted step is a tool call to `write` with the
//!   args `{ path: "out.txt", content: "hello from the tools example" }`.
//! - The agent loop executes the tool against the real FS, feeds the result
//!   back, and the faux provider's second step replies with a short text turn.
//!
//! No real LLM is contacted. The point is to exercise the tool → env → FS path
//! end-to-end against the OS backend.

use std::sync::Arc;

use rpi_agent::AgentBuilder;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;
use rpi_tools::{create_bash_tool, create_write_tool, ExecutionToolContext, OsExecutionEnv};

#[tokio::main]
async fn main() {
    // A tempdir workspace for the run. `OsExecutionEnv` resolves relative paths
    // against its cwd, so `out.txt` lands inside this dir.
    let tmp = std::env::temp_dir().join(format!("pi-tools-example-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.clone()));

    // The built-in tools take an `ExecutionToolContext`. `OsExecutionEnv`
    // implements both `ExecutionEnv` and `MutatingEnv`.
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let write_tool = create_write_tool(&ctx);
    let bash_tool = create_bash_tool(&ctx, None);

    // A faux provider scripted to: (1) call `write`, (2) after the tool result
    // comes back, emit a short text turn. The agent loop handles the round trip.
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "out.txt", "content": "hello from the tools example" }),
        )
        .with_text("Done — wrote the file.");
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();

    let agent = AgentBuilder::new()
        .model(model)
        .stream_fn(make_stream_fn(provider))
        .tools(vec![write_tool, bash_tool])
        .build()
        .expect("agent builds");

    let mut rx = agent.subscribe();
    agent
        .prompt("Write 'hello from the tools example' to out.txt.")
        .await
        .expect("prompt completes");

    // Drain lifecycle events.
    while let Ok(_ev) = rx.try_recv() {}

    let state = agent.state();
    println!("tools example: {} messages after run", state.messages.len());

    // Confirm the tool actually wrote the file on the real FS.
    let out_path = tmp.join("out.txt");
    let written =
        std::fs::read_to_string(&out_path).expect("out.txt was written by the write tool");
    println!("wrote out.txt ({} bytes): {:?}", written.len(), written);
    assert_eq!(written, "hello from the tools example");

    // Clean up.
    let _ = std::fs::remove_dir_all(&tmp);
    println!("AgentEnd observed; tool ran against OsExecutionEnv.");
}

/// Same sync-over-async StreamFn bridge as `examples/minimal` — the faux
/// provider spawns its producer task before `stream_simple` resolves, so the
/// returned stream is immediately live.
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
