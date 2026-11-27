use super::*;
use serde_json::{json, Value};
use std::sync::Barrier;

static LOCK: Mutex<()> = Mutex::new(());
static EVENTS: Mutex<Vec<(EventTag, Value)>> = Mutex::new(Vec::new());
static TRANSFORMS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

rpi_plugin_sdk::event_handler! { fn capture(event, _data) {
    if let Some(text) = event.data_text() {
        if let Ok(value) = serde_json::from_str(&text) {
            EVENTS.lock().unwrap().push((event.tag(), value));
        }
    }
    0
} }
rpi_plugin_sdk::string_handler! { fn transform(input, output, _data) {
    let Ok(value) = serde_json::from_str::<Value>(input) else { return 1; };
    let prompt = format!("scope:{}", value["agentId"].as_str().unwrap_or("none"));
    TRANSFORMS.lock().unwrap().push(value);
    i32::from(!output.write(json!({"systemPrompt":prompt}).to_string()))
} }
use rpi_plugin_sdk::callbacks::free_string as free;

fn registry() -> Arc<RegistrySnapshot> {
    let mut registry = crate::registry::ExtensionRegistry::new();
    for tag in [
        EventTag::BeforeAgentStart,
        EventTag::BeforeProviderRequest,
        EventTag::BeforeProviderHeaders,
        EventTag::AfterProviderResponse,
    ] {
        registry.register_event_handler(
            "scoped-context".into(),
            tag,
            capture,
            std::ptr::null_mut(),
        );
    }
    registry.register_before_agent_start(transform, free, std::ptr::null_mut());
    Arc::new(registry.snapshot())
}
fn hooks(
    snapshot: Arc<RegistrySnapshot>,
    identity: ExtensionProviderContext,
) -> ExtensionProviderHooks {
    ExtensionProviderHooks {
        snapshot,
        _keepalive: Arc::new(PluginKeepalive::new(Vec::new(), None)),
        context: None,
        last_before_agent_start: Mutex::new(None),
        last_forced_prompt: Mutex::new(None),
    }
    .with_context(identity)
}
fn model() -> Model {
    Model::new(
        "fixture-model",
        "fixture-model",
        rpi_ai::Api::AnthropicMessages,
        "fixture-provider",
        "https://example.invalid",
    )
}
fn prompt() -> Context {
    Context::new(vec![Message::User(rpi_ai::types::UserMessage::new(
        "fixture prompt",
        42,
    ))])
}

#[test]
fn shared_registry_concurrent_bindings_keep_transform_and_all_event_identities_separate() {
    let _lock = LOCK.lock().unwrap();
    EVENTS.lock().unwrap().clear();
    TRANSFORMS.lock().unwrap().clear();
    let registry = registry();
    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        for index in 0..8 {
            let hook = hooks(
                registry.clone(),
                ExtensionProviderContext {
                    agent_id: Some(format!("agent-{index}")),
                    session_id: (index % 2 == 0).then(|| format!("session-{index}")),
                },
            );
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                let model = model();
                let mut context = prompt();
                let expected_prompt = format!("scope:agent-{index}");
                assert_eq!(
                    hook.transform_context(&model, &context)
                        .unwrap()
                        .system_prompt,
                    Some(expected_prompt.clone())
                );
                // Repeated provider turns retain the single transform result.
                assert_eq!(
                    hook.transform_context(&model, &context)
                        .unwrap()
                        .system_prompt,
                    Some(expected_prompt.clone())
                );
                context.system_prompt = Some(expected_prompt);
                hook.before_request(&model, &context, &SimpleStreamOptions::default());
                hook.before_request(&model, &context, &SimpleStreamOptions::default());
                let message = AssistantMessage::empty(
                    model.api.clone(),
                    "fixture-provider",
                    "fixture-model",
                    43,
                );
                hook.after_response(&model, &message);
            });
        }
    });
    let transforms = TRANSFORMS.lock().unwrap();
    assert_eq!(
        transforms.len(),
        8,
        "same prompt timestamp in other Agents cannot deduplicate this Agent"
    );
    let events = EVENTS.lock().unwrap();
    assert_eq!(events.len(), 8 * 6);
    for index in 0..8 {
        let id = format!("agent-{index}");
        let session = if index % 2 == 0 {
            json!(format!("session-{index}"))
        } else {
            Value::Null
        };
        let own: Vec<_> = events
            .iter()
            .filter(|(_, value)| value["agentId"] == id)
            .collect();
        assert_eq!(own.len(), 6);
        assert_eq!(
            own.iter()
                .filter(|(tag, _)| *tag == EventTag::BeforeAgentStart)
                .count(),
            1
        );
        for (_, value) in own {
            assert_eq!(value["sessionId"], session);
        }
        let transformed = transforms
            .iter()
            .find(|value| value["agentId"] == id)
            .unwrap();
        assert_eq!(transformed["sessionId"], session);
    }
}

#[test]
fn explicit_sessionless_context_never_adopts_an_ambient_cli_session() {
    // Isolate the deliberately nonempty environment in a test child, without
    // racing process-global env mutation against the rest of the test suite.
    if std::env::var_os("RPI_TEST_SCOPED_PROVIDER_CONTEXT").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("provider_hooks::context_tests::explicit_sessionless_context_never_adopts_an_ambient_cli_session")
            .env("RPI_TEST_SCOPED_PROVIDER_CONTEXT", "1")
            .env(rpi_plugin_sdk::SESSION_ID_ENV, "ambient-single-session-must-not-leak")
            .output().unwrap();
        assert!(
            output.status.success(),
            "isolated identity regression failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    assert!(std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).is_ok());
    let identity = ExtensionProviderContext::default();
    let explicit = before_agent_start_event("p", 0, &prompt(), Some(&identity));
    assert!(explicit["sessionId"].is_null());
    assert!(explicit["agentId"].is_null());
    let legacy = before_agent_start_event("p", 0, &prompt(), None);
    assert_eq!(legacy["sessionId"], "ambient-single-session-must-not-leak");
    let hook = hooks(registry(), identity);
    hook.before_request(&model(), &prompt(), &SimpleStreamOptions::default());
    assert!(EVENTS
        .lock()
        .unwrap()
        .iter()
        .all(|(_, value)| value["sessionId"].is_null()));
}
