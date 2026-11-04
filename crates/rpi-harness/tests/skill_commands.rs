//! Manual commands must reach both provider context and durable history through
//! the configured execution environment, never the process filesystem.
use std::sync::{Arc, Mutex};

use rpi_agent::AgentMessage;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::{
    Content, ImageContent, ImageContentType, Message, StopReason, UserContent, UserMessage,
};
use rpi_harness::agent_harness::{AgentHarness, AgentLane};
use rpi_harness::session::types::{EntryOrder, EntryQuery};
use rpi_harness::skills::{format_skill_command, parse_skill_command};
use rpi_harness::types::{AgentHarnessOptions, AgentHarnessResources, CompactionSettings, Skill};
use rpi_tools::{FileSystem, InMemoryExecutionEnv};

type Requests = Arc<Mutex<Vec<Vec<Message>>>>;

fn skill() -> Skill {
    Skill {
        name: "manual".into(),
        description: "Only explicit use".into(),
        content: "STALE SNAPSHOT".into(),
        file_path: "skills/manual/SKILL.md".into(),
        disable_model_invocation: Some(true),
    }
}

async fn setup() -> (AgentHarness, Arc<InMemoryExecutionEnv>, Requests) {
    let env = Arc::new(InMemoryExecutionEnv::new());
    env.create_dir("skills/manual", true, None).await.unwrap();
    env.write_file(
        &skill().file_path,
        "---\nname: manual\n---\n\n Fresh body \n\n".into(),
        None,
    )
    .await
    .unwrap();
    let requests: Requests = Default::default();
    let script = FauxScript::new();
    script.set_responses(
        (0..24)
            .map(|_| {
                let requests = requests.clone();
                FauxStep::factory(move |ctx| {
                    requests.lock().unwrap().push(ctx.messages.clone());
                    faux_assistant_message("done", StopReason::Stop)
                })
            })
            .collect(),
    );
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        resources: AgentHarnessResources {
            skills: Some(vec![skill()]),
            ..Default::default()
        },
        compaction: CompactionSettings {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    harness.set_skill_execution_env(env.clone());
    (harness, env, requests)
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage::new(text, 0))
}

fn text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn provider_users(request: &[Message]) -> Vec<String> {
    request
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(text(&user.content)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_skill_reads_fresh_body_preserves_args_and_persists_expansion() {
    let (h, env, requests) = setup().await;
    let command = "/skill:manual  a  b\n  c \n";
    h.prompt_text(command, vec![]).await.unwrap();
    let expected = "<skill name=\"manual\" location=\"skills/manual/SKILL.md\">\nReferences are relative to skills/manual.\n\nFresh body\n</skill>\n\na  b\n  c";
    assert_eq!(provider_users(&requests.lock().unwrap()[0]), vec![expected]);
    let entries = h
        .session()
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .unwrap();
    let history = serde_json::to_string(&entries).unwrap();
    assert!(history.contains("Fresh body"));
    assert!(!history.contains("/skill:manual"));
    env.write_file(
        &skill().file_path,
        "---\nname: edited\n---\nUpdated body".into(),
        None,
    )
    .await
    .unwrap();
    h.prompt_message(user("/skill:manual")).await.unwrap();
    let seen = requests.lock().unwrap();
    assert!(provider_users(seen.last().unwrap())
        .last()
        .unwrap()
        .contains("Updated body"));
    assert!(!provider_users(seen.last().unwrap())
        .last()
        .unwrap()
        .contains("invalid:"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_nonleading_and_existing_blocks_pass_through_and_missing_file_falls_back() {
    let (h, _, requests) = setup().await;
    for input in [
        "/skill:unknown keep  spaces",
        "prefix /skill:manual",
        " /skill:manual",
        "<skill>\n/skill:manual\n</skill>",
    ] {
        h.prompt_text(input, vec![]).await.unwrap();
        assert_eq!(
            provider_users(requests.lock().unwrap().last().unwrap())
                .last()
                .unwrap(),
            input
        );
    }
    // An empty environment proves fallback never silently uses the stale body
    // or a local file with the same relative path.
    h.set_skill_execution_env(Arc::new(InMemoryExecutionEnv::new()));
    h.prompt_text("/skill:manual original", vec![])
        .await
        .unwrap();
    assert_eq!(
        provider_users(requests.lock().unwrap().last().unwrap())
            .last()
            .unwrap(),
        "/skill:manual original"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_current_frontmatter_falls_back_without_using_stale_content() {
    let (h, env, requests) = setup().await;
    env.write_file(
        &skill().file_path,
        "---\ninvalid: [\n---\nBad body".into(),
        None,
    )
    .await
    .unwrap();
    let input = "/skill:manual keep  this\n  request";
    h.prompt_text(input, vec![]).await.unwrap();
    assert_eq!(provider_users(&requests.lock().unwrap()[0]), vec![input]);
    assert!(format_skill_command(&skill(), "---\ninvalid: [\n---\nBad body", "").is_err());
}

#[test]
fn skill_body_normalizes_bom_and_windows_newlines_without_leaking_frontmatter() {
    let output = format_skill_command(
        &skill(),
        "\u{feff}---\r\nname: manual\r\n---\r\n Body\r\nnext\r\n",
        "args",
    )
    .unwrap();
    assert!(output.ends_with("\nBody\nnext\n</skill>\n\nargs"));
    assert!(!output.contains("name: manual"));
    assert!(!output.contains('\r'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attached_image_survives_expansion_on_another_lane() {
    let (h, _, requests) = setup().await;
    h.session().create_lane("child", None).await.unwrap();
    let image = ImageContent {
        kind: ImageContentType, mime_type: "image/png".into(),
        // Valid 1x1 PNG, so normal image preparation also runs.
        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".into(),
    };
    h.lane("child")
        .prompt_text("/skill:manual image request", vec![image])
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    let Message::User(user) = &requests[0][0] else {
        panic!("expected user")
    };
    assert!(text(&user.content).ends_with("</skill>\n\nimage request"));
    let UserContent::Blocks(blocks) = &user.content else {
        panic!("expected blocks")
    };
    assert!(blocks
        .iter()
        .any(|block| matches!(block, Content::Image(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_and_follow_up_expand_before_queueing_and_reach_the_provider() {
    let (h, env, requests) = setup().await;
    h.steer(user("/skill:manual steering")).await.unwrap();
    h.follow_up(user("/skill:manual follow up")).await.unwrap();
    let queued = h.queued_messages().await.unwrap();
    assert!(queued.steering[0]
        .text
        .contains("Fresh body\n</skill>\n\nsteering"));
    assert!(queued.follow_up[0]
        .text
        .contains("Fresh body\n</skill>\n\nfollow up"));
    // Queued input retains its admission-time expansion; drains do not recurse
    // or reread skills that changed while a run was active.
    env.write_file(&skill().file_path, "Later body".into(), None)
        .await
        .unwrap();
    h.prompt_text("start", vec![]).await.unwrap();
    let requests = requests.lock().unwrap();
    let users = provider_users(requests.last().unwrap());
    assert!(users
        .iter()
        .any(|text| text.ends_with("</skill>\n\nsteering")));
    assert!(users
        .iter()
        .any(|text| text.ends_with("</skill>\n\nfollow up")));
    assert!(!users.iter().any(|text| text.contains("Later body")));
}

#[test]
fn command_parsing_and_body_formatting_are_nonrecursive() {
    assert_eq!(
        parse_skill_command("/skill:manual  one\n  two  "),
        Some(("manual", "one\n  two"))
    );
    assert_eq!(
        parse_skill_command("/skill:manual\nargument"),
        Some(("manual\nargument", ""))
    );
    assert_eq!(parse_skill_command("<skill> /skill:manual </skill>"), None);
    let expanded =
        format_skill_command(&skill(), "---\nname: old\n---\n /skill:manual \n", "").unwrap();
    assert!(expanded.ends_with("\n/skill:manual\n</skill>"));
    assert_eq!(parse_skill_command(&expanded), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_content_is_not_expanded() {
    let (h, _, requests) = setup().await;
    let assistant = faux_assistant_message("/skill:manual", StopReason::Stop);
    h.prompt_messages(vec![
        AgentMessage::Assistant(Box::new(assistant)),
        user("continue"),
    ])
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert!(requests[0].iter().any(|message| matches!(message,
        Message::Assistant(assistant) if assistant.content.iter().any(|block|
            matches!(block, Content::Text(text) if text.text == "/skill:manual")))));
}
