//! Exercise the real renderer callback adapter and history projection, without
//! providers, global configuration or dynamically loaded libraries.
use super::*;
use rpi_extensions::{
    ExtensionRegistry, ExtensionSession, PluginKeepalive, RegisteredRenderer,
    RegisteredRendererKind,
};
use rpi_harness::session::types::{CustomEntry, EntryBase};
use rpi_plugin_sdk::{StbString, StbStringRef, StepHandle};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Reply {
    rc: i32,
    body: String,
    calls: AtomicUsize,
}

static REPLIES: rpi_plugin_sdk::handles::ToolHandles<Arc<Reply>> =
    rpi_plugin_sdk::handles::ToolHandles::new();

extern "C" fn render(_: StbStringRef, out: *mut StbString, data: *mut std::ffi::c_void) -> i32 {
    let reply = REPLIES.get(data).expect("live renderer fixture");
    let reply = reply.lock().clone();
    reply.calls.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the host supplies a live output slot; the registered SDK
    // deallocator reclaims exactly this plugin-owned string after the callback.
    unsafe { *out = StbString::from_string(reply.body.clone()) };
    reply.rc
}

struct Renderers {
    session: crate::session::ExtensionSessionCell,
    replies: Vec<(StepHandle, Arc<Reply>)>,
}

impl Renderers {
    fn new(responses: &[(i32, &str)]) -> Self {
        let mut registry = ExtensionRegistry::new();
        let mut replies = Vec::new();
        for (index, (rc, body)) in responses.iter().enumerate() {
            let reply = Arc::new(Reply {
                rc: *rc,
                body: body.to_string(),
                calls: AtomicUsize::new(0),
            });
            let handle = REPLIES.insert(reply.clone());
            registry.register_renderer(RegisteredRenderer {
                name: format!("fixture-{index}"),
                kind: RegisteredRendererKind::Entry,
                render_fn: render,
                plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                user_data: handle,
            });
            replies.push((handle, reply));
        }
        Self {
            session: Arc::new(std::sync::Mutex::new(ExtensionSession::from_parts(
                Arc::new(registry.snapshot()),
                PluginKeepalive::empty(),
                vec![],
                None,
            ))),
            replies,
        }
    }

    fn project(&self) -> Arc<Container> {
        let chat = Arc::new(Container::new());
        render_history_entries(
            vec![custom_entry(
                "private-memory",
                json!({"text": "raw fallback"}),
            )],
            &chat,
            None,
            Some(self.session.clone()),
            false,
            false,
        );
        chat
    }
}

impl Drop for Renderers {
    fn drop(&mut self) {
        for (handle, _) in &self.replies {
            REPLIES.remove(*handle);
        }
    }
}

fn custom_entry(custom_type: &str, data: Value) -> Entry {
    Entry::Custom(CustomEntry {
        base: EntryBase {
            entry_type: "custom".into(),
            id: "entry-1".into(),
            seq: 1,
            parent_id: None,
            timestamp: 0,
        },
        custom_type: custom_type.into(),
        data: Some(data),
    })
}

#[test]
fn entry_display_false_hides_without_fallback_spacers_or_later_renderers() {
    for body in [
        r#"{"display":false}"#,
        r#"{"display":false,"text":"must not render","lines":["also hidden"]}"#,
    ] {
        let renderers = Renderers::new(&[(0, body), (0, r#"{"text":"later renderer"}"#)]);
        let chat = renderers.project();
        assert_eq!(
            chat.child_count(),
            0,
            "hidden entries must not mount even a spacer"
        );
        assert!(chat.render(100).is_empty());
        assert_eq!(renderers.replies[0].1.calls.load(Ordering::SeqCst), 1);
        assert_eq!(renderers.replies[1].1.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn entry_display_false_requires_success_and_a_literal_boolean() {
    for (rc, body) in [
        (1, r#"{"display":false}"#),
        (0, "not JSON"),
        (0, "{}"),
        (0, "null"),
        (0, r#"{"display":true}"#),
        (0, r#"{"display":"false"}"#),
        (0, r#"{"display":null}"#),
        (0, r#"{"display":0}"#),
    ] {
        let renderers = Renderers::new(&[(rc, body)]);
        let chat = renderers.project();
        let output = strip_ansi(&chat.render(100).join("\n"));
        assert!(
            output.contains("private-memory: raw fallback"),
            "rc={rc} {body}: {output}"
        );
    }
}

#[test]
fn entry_decline_or_invalid_json_still_tries_the_next_renderer() {
    for (rc, body) in [(1, r#"{"display":false}"#), (0, "not JSON")] {
        let renderers = Renderers::new(&[(rc, body), (0, r#"{"text":"later renderer"}"#)]);
        let chat = renderers.project();
        let output = strip_ansi(&chat.render(100).join("\n"));
        assert!(output.contains("later renderer"));
        assert!(!output.contains("raw fallback"));
        assert_eq!(renderers.replies[1].1.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn entry_text_lines_markdown_and_empty_text_keep_legacy_behavior() {
    for body in [
        r#"{"text":"visible"}"#,
        r#"{"lines":["visible","second line"]}"#,
        r#"{"text":"visible","markdown":true}"#,
        r#"{"display":true,"text":"visible"}"#,
        r#"{"display":"false","text":"visible"}"#,
    ] {
        let renderers = Renderers::new(&[(0, body)]);
        let chat = renderers.project();
        assert_eq!(
            chat.child_count(),
            2,
            "rendered component plus legacy spacer"
        );
        let output = strip_ansi(&chat.render(100).join("\n"));
        assert!(output.contains("visible"), "{body}: {output}");
        assert!(!output.contains("raw fallback"));
    }
    let renderers = Renderers::new(&[(0, r#"{"text":""}"#)]);
    assert_eq!(
        renderers.project().child_count(),
        2,
        "empty text is not an opt-in to hiding"
    );
}

#[test]
fn entry_without_plugin_keeps_raw_fallback_and_builtin_panel_skips() {
    let chat = Arc::new(Container::new());
    render_history_entries(
        vec![custom_entry(
            "private-memory",
            json!({"text":"raw fallback"}),
        )],
        &chat,
        None,
        None,
        false,
        false,
    );
    assert!(strip_ansi(&chat.render(100).join("\n")).contains("private-memory: raw fallback"));

    let renderers = Renderers::new(&[(0, r#"{"text":"must stay skipped"}"#)]);
    let chat = Arc::new(Container::new());
    render_history_entries(
        ["plan-mode", "todo", "todo-state"]
            .into_iter()
            .map(|kind| custom_entry(kind, json!({"text":"skip"})))
            .collect(),
        &chat,
        None,
        Some(renderers.session.clone()),
        false,
        false,
    );
    assert_eq!(chat.child_count(), 0);
    assert_eq!(renderers.replies[0].1.calls.load(Ordering::SeqCst), 0);
}
