//! Session export functionality.
//!
//! Supports multiple export formats: Markdown (current), HTML, and JSONL.

use std::path::Path;

use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::types::{Entry, EntryOrder, EntryQuery};

/// Export format options.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportFormat {
    Markdown,
    Html,
    Jsonl,
}

/// Export a JSONL session file to the specified format.
///
/// The input is a real JSONL v4 session file (header line + one mutation per
/// line), so it is parsed with the harness codec rather than assuming raw
/// `Entry` JSON. Only entry mutations contribute to the rendered transcript;
/// lane/fact/record lines are skipped (they are not conversational content).
pub fn export_file(input: &Path, output: &Path) -> Result<(), String> {
    // Determine format from output extension
    let format = if output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
    {
        ExportFormat::Html
    } else if output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        ExportFormat::Jsonl
    } else {
        ExportFormat::Markdown
    };

    let entries = read_jsonl_session_entries(input)?;

    match format {
        ExportFormat::Markdown => export_markdown(&entries, output),
        ExportFormat::Html => export_html(&entries, output),
        ExportFormat::Jsonl => export_jsonl(&entries, output),
    }
}

/// Parse a JSONL v4 session file into its entry list (oldest first).
fn read_jsonl_session_entries(input: &Path) -> Result<Vec<Entry>, String> {
    use rpi_harness::session::jsonl::parse_mutation;
    use rpi_harness::session::types::SessionMutation;

    let content = std::fs::read_to_string(input)
        .map_err(|e| format!("Could not read input file {}: {e}", input.display()))?;

    let mut entries = Vec::new();
    let mut saw_header = false;
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            // A valid session file starts with the v4 header line. Validate it
            // so a plain JSON/old file fails with a clear message.
            rpi_harness::session::jsonl::codec::parse_header(line).map_err(|e| {
                format!(
                    "{} is not a JSONL v4 session file (line {}): {e}",
                    input.display(),
                    index + 1
                )
            })?;
            saw_header = true;
            continue;
        }
        let mutation = parse_mutation(line).map_err(|e| {
            format!(
                "could not parse mutation at {}:{}: {e}",
                input.display(),
                index + 1
            )
        })?;
        if let SessionMutation::Entry { entry, .. } = mutation {
            entries.push(entry);
        }
    }
    if !saw_header {
        return Err(format!("{} is empty", input.display()));
    }
    Ok(entries)
}

/// Export the session to the specified format.
pub async fn export_session(
    harness: &AgentHarness,
    format: ExportFormat,
    output_path: &Path,
) -> Result<(), String> {
    let tree = harness.session().view("main");
    let entries = tree
        .find_entries(&EntryQuery {
            entry_type: None,
            custom_type: None,
            order: Some(EntryOrder::OldestFirst),
            limit: None,
            cursor: None,
        })
        .await
        .map_err(|e| format!("Could not read session: {e}"))?;

    match format {
        ExportFormat::Markdown => export_markdown(&entries, output_path),
        ExportFormat::Html => export_html(&entries, output_path),
        ExportFormat::Jsonl => export_jsonl(&entries, output_path),
    }
}

fn export_markdown(entries: &[Entry], output_path: &Path) -> Result<(), String> {
    let mut md = String::from("# Session\n\n");
    for e in entries {
        if let Entry::Message(me) = e {
            match &me.message {
                rpi_agent::AgentMessage::User(u) => {
                    let text = user_message_text(u);
                    md.push_str(&format!("## User\n\n{}\n\n", text));
                }
                rpi_agent::AgentMessage::Assistant(a) => {
                    let text = assistant_text(a);
                    if !text.is_empty() {
                        md.push_str(&format!("## Assistant\n\n{}\n\n", text));
                    }
                }
                _ => {}
            }
        }
    }
    std::fs::write(output_path, md).map_err(|e| format!("Could not write markdown export: {e}"))
}

fn export_html(entries: &[Entry], output_path: &Path) -> Result<(), String> {
    let mut html = String::from(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Session Export</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; margin: 2rem; line-height: 1.6; }
        .message { margin-bottom: 2rem; padding: 1rem; border-radius: 8px; }
        .user { background-color: #f0f9ff; border-left: 4px solid #3b82f6; }
        .assistant { background-color: #f3f4f6; border-left: 4px solid #6b7280; }
        .role { font-weight: bold; margin-bottom: 0.5rem; color: #1f2937; }
        pre { background-color: #1e293b; color: #f8fafc; padding: 1rem; border-radius: 6px; overflow-x: auto; }
        code { font-family: 'SFMono-Regular', Consolas, 'Liberation Mono', Menlo, monospace; }
    </style>
</head>
<body>
    <h1>Session Export</h1>
"#,
    );

    for e in entries {
        if let Entry::Message(me) = e {
            match &me.message {
                rpi_agent::AgentMessage::User(u) => {
                    let text = user_message_text(u);
                    html.push_str(&format!(
                        r#"<div class="message user">
    <div class="role">User</div>
    <div>{}</div>
</div>"#,
                        escape_html(&text)
                    ));
                }
                rpi_agent::AgentMessage::Assistant(a) => {
                    let text = assistant_text(a);
                    if !text.is_empty() {
                        html.push_str(&format!(
                            r#"<div class="message assistant">
    <div class="role">Assistant</div>
    <div>{}</div>
</div>"#,
                            escape_html(&text)
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    html.push_str("\n</body>\n</html>");

    std::fs::write(output_path, html).map_err(|e| format!("Could not write HTML export: {e}"))
}

fn export_jsonl(entries: &[Entry], output_path: &Path) -> Result<(), String> {
    let mut jsonl = String::new();
    for e in entries {
        let json = serde_json::to_string(e)
            .map_err(|e| format!("Could not serialize entry to JSON: {e}"))?;
        jsonl.push_str(&json);
        jsonl.push('\n');
    }
    std::fs::write(output_path, jsonl).map_err(|e| format!("Could not write JSONL export: {e}"))
}

fn user_message_text(user: &rpi_ai::types::UserMessage) -> String {
    match &user.content {
        rpi_ai::types::UserContent::Text(s) => s.clone(),
        rpi_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|c| match c {
                rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
                rpi_ai::types::Content::Image(_) => Some("[Image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn assistant_text(assistant: &rpi_ai::types::AssistantMessage) -> String {
    assistant
        .content
        .iter()
        .filter_map(|c| match c {
            rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
            rpi_ai::types::Content::Thinking(t) => {
                Some(format!("<thinking>{}</thinking>", t.thinking))
            }
            rpi_ai::types::Content::ToolCall(tc) => Some(format!("[Tool Call: {}]", tc.name)),
            rpi_ai::types::Content::Image(_) => Some("[Image]".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_harness::session::jsonl::codec::{encode_header, encode_mutation};
    use rpi_harness::session::jsonl::{HeaderKind, JsonlV4Header};
    use rpi_harness::session::types::{Entry, EntryBase, MessageEntry, SessionMutation};

    #[test]
    fn test_escape_html() {
        assert_eq!(
            escape_html("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("Hello & World"), "Hello &amp; World");
    }

    /// Build a real JSONL v4 session file containing one user + one assistant
    /// message, then export it to each format via the `--export` code path.
    #[test]
    fn export_file_renders_a_real_jsonl_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("session.jsonl");

        let header = JsonlV4Header {
            kind: HeaderKind,
            version: 4,
            id: "s-export".to_string(),
            created_at: 0,
            cwd: "/tmp".to_string(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };

        let mut lines = vec![encode_header(&header)];
        for (seq, message) in [
            (
                1u64,
                rpi_agent::message::AgentMessage::User(rpi_ai::types::UserMessage::new(
                    rpi_ai::types::UserContent::Text("hello <world>".to_string()),
                    0,
                )),
            ),
            (
                2u64,
                rpi_agent::message::AgentMessage::Assistant(Box::new({
                    let mut a = rpi_ai::types::AssistantMessage::empty(
                        rpi_ai::Api::AnthropicMessages,
                        "anthropic",
                        "m",
                        0,
                    );
                    a.content = vec![rpi_ai::types::Content::Text(rpi_ai::types::TextContent {
                        kind: rpi_ai::types::TextContentType,
                        text: "hi there".to_string(),
                        text_signature: None,
                    })];
                    a
                })),
            ),
        ] {
            let entry = Entry::Message(MessageEntry {
                base: EntryBase {
                    entry_type: "message".to_string(),
                    id: format!("e{seq}"),
                    seq,
                    parent_id: None,
                    timestamp: 0,
                },
                message,
                terminate: None,
            });
            lines.push(encode_mutation(&SessionMutation::Entry {
                seq,
                timestamp: 0,
                lane: Some("main".to_string()),
                entry,
            }));
        }
        std::fs::write(&session_path, lines.concat()).unwrap();

        // Markdown
        let md_path = dir.path().join("out.md");
        export_file(&session_path, &md_path).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        assert!(md.contains("hello <world>"), "{md}");
        assert!(md.contains("hi there"), "{md}");

        // HTML escapes the angle brackets in the user text.
        let html_path = dir.path().join("out.html");
        export_file(&session_path, &html_path).unwrap();
        let html = std::fs::read_to_string(&html_path).unwrap();
        assert!(html.contains("&lt;world&gt;"), "{html}");
        assert!(html.contains("hi there"), "{html}");
        assert!(html.trim_end().ends_with("</html>"), "{html}");

        // JSONL re-emits the parsed entries (not the original wrapper lines).
        let jsonl_path = dir.path().join("out.jsonl");
        export_file(&session_path, &jsonl_path).unwrap();
        let jsonl = std::fs::read_to_string(&jsonl_path).unwrap();
        let parsed: Vec<Entry> = jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn export_file_rejects_a_non_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("not-a-session.jsonl");
        std::fs::write(&bogus, "{\"type\":\"message\"}\n").unwrap();
        let out = dir.path().join("out.md");
        let err = export_file(&bogus, &out).unwrap_err();
        assert!(err.contains("not a JSONL v4 session file"), "{err}");
    }
}
