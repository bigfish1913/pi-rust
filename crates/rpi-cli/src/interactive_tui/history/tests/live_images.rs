//! Live image previews belong in the growing transcript slot, not the
//! assembled document root (the #275 class of bug, for images).
use super::*;
use rpi_agent::types::{AgentToolResult, TextContentOrImage};
use rpi_ai::types::{ImageContent, ImageContentType};
use rpi_tui::Image;

fn png_tool_result(tag: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![
            TextContentOrImage::text(format!("Read image file [image/png] {tag}")),
            TextContentOrImage::Image(ImageContent {
                kind: ImageContentType,
                // A PNG signature is enough: the preview only base64-decodes it.
                data: "iVBORw0KGgo=".into(),
                mime_type: "image/png".into(),
            }),
        ],
        ..Default::default()
    }
}

fn count_images(container: &Arc<Container>) -> usize {
    container
        .get_children()
        .iter()
        .filter(|child| child.as_any().downcast_ref::<Image>().is_some())
        .count()
}

fn image_positions(container: &Arc<Container>) -> Vec<usize> {
    container
        .get_children()
        .iter()
        .enumerate()
        .filter_map(|(index, child)| child.as_any().downcast_ref::<Image>().map(|_| index))
        .collect()
}

#[tokio::test]
async fn tool_images_land_in_the_live_slot_not_the_assembled_root() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let (_, state, tui) = fixture(0).await;
    state.history_page.mount_active(&state.chat_container);

    // Two `read` calls in one run: each image must join the streaming tool
    // panels instead of collecting at the end of the document (just above the
    // editor) until `AgentEnd` rebuilds the page.
    for tag in ["first.png", "second.png"] {
        handle_agent_event(
            AgentEvent::ToolExecutionEnd {
                tool_call_id: format!("call-{tag}"),
                tool_name: "read".into(),
                result: png_tool_result(tag),
                is_error: false,
            },
            &tui,
            &state,
            &state.chat_container,
            &lane,
        )
        .await;
    }

    assert_eq!(
        count_images(&state.history_page.active),
        2,
        "both previews must sit in the live slot that grows with the event stream"
    );
    assert_eq!(
        count_images(&state.chat_container),
        0,
        "the assembled root must not collect live image previews"
    );
    // Ordering: each preview follows its own tool panel inside the live slot.
    let positions = image_positions(&state.history_page.active);
    let tool_positions: Vec<usize> = state
        .history_page
        .active
        .get_children()
        .iter()
        .enumerate()
        .filter_map(|(index, child)| {
            child
                .as_any()
                .downcast_ref::<ToolExecutionComponent>()
                .map(|_| index)
        })
        .collect();
    assert_eq!(tool_positions.len(), 2, "{tool_positions:?}");
    assert!(
        tool_positions[0] < positions[0] && positions[0] < tool_positions[1],
        "the first image must follow its own read panel, not the run's tail: \
         tools={tool_positions:?} images={positions:?}"
    );
}

#[tokio::test]
async fn queued_user_images_land_in_the_live_slot_not_the_assembled_root() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let (_, state, tui) = fixture(0).await;
    state.history_page.mount_active(&state.chat_container);

    let image = ImageContent {
        kind: ImageContentType,
        data: "iVBORw0KGgo=".into(),
        mime_type: "image/png".into(),
    };
    let message = AgentMessage::User(UserMessage::new(
        rpi_ai::types::UserContent::Blocks(vec![
            rpi_ai::types::Content::text("look at this"),
            rpi_ai::types::Content::Image(image),
        ]),
        0,
    ));
    handle_agent_event(
        AgentEvent::MessageStart {
            message_id: None,
            message,
        },
        &tui,
        &state,
        &state.chat_container,
        &lane,
    )
    .await;

    assert_eq!(count_images(&state.history_page.active), 1);
    assert_eq!(
        count_images(&state.chat_container),
        0,
        "a queued user image must not be pinned below the growing live slot"
    );
}
