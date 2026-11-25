//! Deterministic child for the cross-repository extcore admission test.
//! Uses the production JSONL dispatcher and the same send_message execution
//! path as plugin ActionHost. No model endpoint, credentials or plugin DLL.
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::{
    agent_harness::AgentHarness,
    events::RunEndEvent,
    types::{AgentHarnessOptions, SendMessageOptions},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[tokio::main]
async fn main() {
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("first RPC reply")
            .with_text("autonomous plugin reply")
            .with_text("next RPC reply"),
    );
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    let fired = AtomicBool::new(false);
    let pending = Arc::new(Mutex::new(None));
    let task = pending.clone();
    let plugin_harness = harness.clone();
    let _callback = harness.events().on::<RunEndEvent, _>(move |_| {
        if fired.swap(true, Ordering::SeqCst) {
            return;
        }
        let lane = plugin_harness.lane("main");
        *task.lock().unwrap() = Some(tokio::spawn(async move {
            lane.wait_for_idle().await.unwrap();
            lane.send_message(
                rpi_ai::UserMessage::new("plugin kickoff", 0).into(),
                SendMessageOptions {
                    trigger_turn: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }));
    });
    let exit = rpi_cli::modes::rpc(
        &harness,
        None,
        vec![],
        rpi_extensions::UiDialogMailbox::new(),
        None,
        rpi_cli::args::RpcMessageUpdates::CompactDeltas,
    )
    .await;
    let task = pending.lock().unwrap().take();
    if let Some(task) = task {
        task.await.unwrap();
    }
    assert_eq!(exit, 0);
}
