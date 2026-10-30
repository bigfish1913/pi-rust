use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_reverse_bindings_fail_closed_but_still_reclaim_their_children() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("before harness"));
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    );
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    assert!(host.get_execution_context(json!({})).await.is_ok());
    let child = host
        .child_agent(7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = &child["childId"];
    drop(parent);
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("after harness"));
    assert!(host
        .child_agent(7, json!({"op":"start","task":"stale"}))
        .await
        .unwrap_err()
        .contains("after harness"));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = host
                .child_agent(7, json!({"op":"poll","childId":id}))
                .await
                .unwrap();
            if value["status"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(host
        .child_agent(8, json!({"op":"dispose","childId":id}))
        .await
        .is_err());
    host.child_agent(7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(host
        .child_agent(7, json!({"op":"poll","childId":id}))
        .await
        .is_err());
}

/// End-to-end: the images a plugin attaches actually reach the transcript as
/// image content blocks (not just "the args parsed"). This is the assertion the
/// unit tests above cannot make — `prompt_text`'s `images` argument, not our
/// parsing, decides the stored shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_user_message_stores_attached_images_in_the_transcript() {
    use base64::Engine as _;

    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    let provider = FauxProvider::new(FauxScript::new().with_text("got it"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    let png = base64::engine::general_purpose::STANDARD.encode([
        0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]);

    // Pure-image send (no `text` at all) plus one unusable entry, which must be
    // skipped rather than failing the whole message.
    let result = host
        .send_user_message(json!({
            "images": [
                {"mediaType":"image/png","data": png},
                {"mediaType":"image/png","data": "not-base64!!"},
            ]
        }))
        .await
        .unwrap();
    assert_eq!(result["images"], 1);
    assert_eq!(result["skippedImages"], 1);

    let log = harness
        .session()
        .get_log(&rpi_harness::session::types::LogOptions {
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    let user_message = log
        .iter()
        .find_map(|item| match item {
            rpi_harness::session::types::LogItem::Entry {
                entry: rpi_harness::session::types::Entry::Message(entry),
                ..
            } => match &entry.message {
                rpi_agent::AgentMessage::User(user) => Some(user),
                _ => None,
            },
            _ => None,
        })
        .expect("the user message must be in the transcript");
    let images: Vec<_> = match &user_message.content {
        rpi_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                rpi_ai::types::Content::Image(image) => Some(image),
                _ => None,
            })
            .collect(),
        rpi_ai::types::UserContent::Text(_) => vec![],
    };
    assert_eq!(images.len(), 1, "exactly the valid image is stored");
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(images[0].data, png);
}
