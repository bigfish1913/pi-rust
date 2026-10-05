//! Transcript messages, welcome panels, errors, and session resume hints.

use super::*;

// ===========================================================================
// Transcript message helpers
// ===========================================================================

/// Add the welcome header to the chat container.
pub(super) fn add_welcome_message(container: &Arc<Container>) {
    add_welcome_message_with_capabilities(container, &[], &[]);
}

/// Add the startup welcome header and a compact snapshot of active tools and
/// discovered skills. The snapshot reflects the harness configuration used by
/// the first turn, including tools contributed by extensions.
pub(super) fn add_welcome_message_with_capabilities(
    container: &Arc<Container>,
    active_tools: &[String],
    skills: &[String],
) {
    let c = current_theme().colors;
    // The mark, wordmark and language mark live in `crate::brand`. See that
    // module for why the layout is built from single-width characters only.
    container.add_child(Arc::new(crate::brand::BrandLockup::new()));
    container.add_child(Arc::new(Spacer::new(1)));
    // The mark carries the product name, so the line under it is the tagline
    // rather than a second logotype.
    let title = c.muted.fg("interactive TUI · library-first agent runtime");
    container.add_child(Arc::new(Text::new(title, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        c.dim.fg("Type your message and press Enter to send."),
        1,
        0,
    )));
    let hint = c
        .dim
        .fg("Enter send · Shift+Enter newline · Ctrl+C clear · Esc abort · /help");
    container.add_child(Arc::new(Text::new(hint, 1, 0)));
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Tools", active_tools),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        welcome_capability_line("Skills", skills),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::new()));
}

/// Append update notices inside the live transcript. Fullscreen mode clears
/// pre-TUI stdout/stderr, so update state must be represented by components.
pub(super) fn add_update_notices(
    container: &Arc<Container>,
    report: &crate::updates::UpdateReport,
) {
    let colors = current_theme().colors;
    let group = Arc::new(Container::new());

    for warning in &report.warnings {
        let body = format!(
            "{}\n{} {}{}",
            colors.error.fg(&warning.message),
            colors.muted.fg("Run"),
            colors.accent.fg(&warning.command),
            colors.muted.fg(" to retry.")
        );
        add_update_panel(&group, "Update Failed", &body, colors.error);
    }

    if let Some(notice) = report.notices.iter().find(|notice| notice.name == "rpi") {
        let body = format!(
            "{} {}{}",
            colors
                .muted
                .fg(&format!("New version {} is available. Run", notice.latest)),
            colors.accent.fg(&notice.command),
            colors.muted.fg(".")
        );
        add_update_panel(&group, "Update Available", &body, colors.warning);
    }

    // Other transcript producers append concurrently. Add the fully built
    // group in one operation so card borders and content cannot interleave
    // with user, assistant, tool, or extension messages.
    if group.child_count() > 0 {
        container.add_child(group);
    }
}

pub(super) fn add_update_panel(
    container: &Arc<Container>,
    title: &str,
    body: &str,
    color: rpi_tui::Color,
) {
    container.add_child(Arc::new(Spacer::new(1)));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
    container.add_child(Arc::new(Text::new(
        format!("{}\n{body}", color.fg(&tui_bold(title))),
        1,
        0,
    )));
    container.add_child(Arc::new(DynamicBorder::with_color(color)));
}

pub(super) fn welcome_capability_line(label: &str, names: &[String]) -> String {
    let c = current_theme().colors;
    let value = if names.is_empty() {
        "none".to_string()
    } else {
        names.join(" · ")
    };
    format!(
        "{} {}",
        c.accent.fg(&format!("{label} ({})", names.len())),
        c.muted.fg(&value)
    )
}

/// Add the `/help` command listing to the chat container.
pub(super) fn add_help_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    // Section header + a thin themed rule, then a two-column command table:
    // `cmd` in accent, `— desc` in muted. The old single-space layout made
    // the description column wander depending on command length.
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📚 Available Commands")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));

    let cmds: &[(&str, &str)] = &[
        ("/help, /?", "Show this help message"),
        ("/clear, /new", "Clear the conversation"),
        ("/exit, /quit, /q", "Exit the application"),
        ("/version, /v", "Show version information"),
        ("/changelog", "Show recent release changes"),
        ("/model, /m", "Choose a model (live switch)"),
        ("/thinking, /think", "Set reasoning depth (selector)"),
        ("/tools", "Toggle built-in tools on/off"),
        ("/images", "Toggle inline image rendering"),
        ("/session", "List saved sessions"),
        ("/theme", "Choose a theme (selector)"),
        ("/compact", "Compact the conversation"),
        ("/copy", "Copy last reply to clipboard"),
        ("/hotkeys", "Show keyboard shortcuts"),
    ];
    let cmd_w = cmds.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (cmd, desc) in cmds {
        let row = format!(
            "  {:<cmd_w$}  {}  {}",
            c.accent.fg(cmd),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/version` block to the chat container.
pub(super) fn add_version_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("📦 Version Information")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    // Use the crate version (kept in sync via `version.workspace = true`)
    // instead of the stale hardcoded "v0.1.2".
    container.add_child(Arc::new(Text::new(
        format!(
            "  {} {}",
            c.muted.fg("rpi-cli"),
            c.text.fg(&format!("v{}", crate::VERSION))
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Text::new(
        format!(
            "  {}",
            c.dim.fg("Rust implementation of pi coding agent TUI")
        ),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a compact `/changelog` block to the chat container. Keep this local to
/// the binary so the command remains useful in installed builds without a
/// source checkout or a network request.
pub(super) fn add_changelog_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("Recent Changes")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let entries = [
        (
            "Native parity phase 1",
            "models, images, trust, export, and JSON events",
        ),
        (
            "TUI controls",
            "external editor, thinking levels, and tool output toggles",
        ),
        (
            "Provider auth",
            "OpenAI-compatible API key aliases and gateway headers",
        ),
    ];
    for (release, summary) in entries {
        let row = format!("  {}  {}", c.accent.fg(release), c.muted.fg(summary));
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.dim.fg("Version"), c.text.fg(crate::VERSION)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add the `/hotkeys` block to the chat container.
pub(super) fn add_hotkeys_message(container: &Arc<Container>) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        c.md_heading.fg(&tui_bold("⌨️  Keyboard Shortcuts")),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
    let keys: &[(&str, &str)] = &[
        ("Enter", "Send message"),
        ("Shift+Enter", "New line"),
        ("Tab", "Accept autocomplete suggestion"),
        ("Ctrl+A / Ctrl+E", "Line start / end"),
        (
            "Ctrl+K / Ctrl+U",
            "Kill to end / start of line (Ctrl+Y yanks)",
        ),
        ("Ctrl+- / Ctrl+R", "Undo / redo"),
        ("Ctrl+Y / Alt+Y", "Yank / yank-pop"),
        ("Alt+Backspace", "Kill previous word"),
        ("Ctrl+C", "Clear the editor (press twice quickly to exit)"),
        ("Ctrl+X", "Copy the editor selection or the last reply"),
        ("Esc", "Abort a running prompt"),
        ("Ctrl+L", "Open model selector"),
        ("Ctrl+P", "Cycle to the next model (live)"),
        ("Ctrl+O", "Expand/collapse all tool output"),
        ("Ctrl+T", "Show/hide reasoning blocks"),
        ("PageUp/Down", "Scroll transcript by one page"),
        ("Home / End", "Jump to transcript start / latest output"),
    ];
    let key_w = keys.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, desc) in keys {
        let row = format!(
            "  {:<key_w$}  {}  {}",
            c.accent.fg(key),
            c.dim.fg("—"),
            c.muted.fg(desc)
        );
        container.add_child(Arc::new(Text::new(row, 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add a user message echo to the chat container — a bordered `UserMessageComponent`
/// (surface-colored box with OSC133 prompt-boundary markers) replacing the old
/// plain `> text` echo. A trailing Spacer(1) separates it from the next
// transcript entry (every entry contributes one trailing spacer so
// consecutive turns are separated by exactly one blank line).
pub(super) fn add_user_message(container: &Arc<Container>, text: &str) {
    container.add_child(Arc::new(UserMessageComponent::new(text.to_string())));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Add an error message to the chat container.
/// Collect all rendered lines from the chat container for search.
pub(super) fn collect_transcript_lines(chat_container: &Arc<Container>) -> Vec<String> {
    let width = 80; // Default width for search; actual width varies by terminal
    chat_container.render(width)
}

/// Build the shell command that resumes `session_id` in this project, or
/// `None` when there is nothing to resume. Mirrors upstream's
/// `formatResumeCommand`: `rpi --session <id>`, prefixing
/// `--session-dir <dir>` only when a non-default directory was requested.
pub(super) fn format_resume_command(
    session_id: &str,
    cwd: &std::path::Path,
    session_dir: Option<&std::path::Path>,
) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let mut args = vec![crate::APP_NAME.to_string()];
    if let Some(dir) = session_dir {
        let default = crate::session::default_session_dir(cwd);
        if dir != default.as_path() {
            args.push("--session-dir".to_string());
            args.push(quote_shell_arg(&dir.to_string_lossy()));
        }
    }
    args.push("--session".to_string());
    args.push(session_id.to_string());
    Some(args.join(" "))
}

/// Quote a shell argument when it contains whitespace or shell metacharacters
/// (mirrors upstream's `quoteIfNeeded`).
pub(super) fn quote_shell_arg(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || "\"'`$&|;<>()*?[]{}!~#\\".contains(c));
    if needs_quoting {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

pub(super) fn add_error_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    let text = sanitize_error_message(text);
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.error.fg("✗"), c.error.fg(&text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Keep provider diagnostics printable in the transcript. HTTP error bodies
/// may contain carriage returns, terminal escapes, or an unexpectedly large
/// JSON payload; letting those bytes reach the renderer can corrupt the input
/// row or make the whole frame exceed terminal limits.
pub(super) fn sanitize_error_message(text: &str) -> String {
    const MAX_ERROR_CHARS: usize = 16 * 1024;
    let mut result = String::with_capacity(text.len().min(MAX_ERROR_CHARS));
    let mut count = 0;
    // 0 = normal, 1 = escape introducer, 2 = CSI, 3 = OSC.
    let mut escape_mode = 0u8;
    for ch in text.chars() {
        if escape_mode != 0 {
            match escape_mode {
                1 if ch == '[' => escape_mode = 2,
                1 if ch == ']' => escape_mode = 3,
                1 if ch == '\x07' || ('@'..='~').contains(&ch) => escape_mode = 0,
                2 if ('@'..='~').contains(&ch) => escape_mode = 0,
                3 if ch == '\x07' => escape_mode = 0,
                _ => {}
            }
            continue;
        }
        if ch == '\x1b' {
            escape_mode = 1;
            continue;
        }
        if count >= MAX_ERROR_CHARS {
            result.push_str("…");
            break;
        }
        match ch {
            '\n' | '\t' => {
                result.push(ch);
                count += 1;
            }
            '\r' => {}
            c if c.is_control() => {}
            c => {
                result.push(c);
                count += 1;
            }
        }
    }
    if result.trim().is_empty() {
        "Provider request failed.".to_string()
    } else {
        result
    }
}

/// Add a neutral note (e.g. unsupported-command message) to the chat container.
pub(super) fn add_note_message(container: &Arc<Container>, text: &str) {
    let c = current_theme().colors;
    container.add_child(Arc::new(Text::new(
        format!("  {} {}", c.info.fg("ℹ"), c.muted.fg(text)),
        1,
        0,
    )));
    container.add_child(Arc::new(Spacer::new(1)));
}

/// Render the `/context` panel: a transcript message listing the discovered
/// context files, skills, and prompt templates loaded for this session
/// (Part A resource discovery). Reads the harness resources snapshot captured
/// at TUI startup (the blocking submit handler can't `await get_resources()`.
///
/// Mirrors pi's context-panel intent (pi surfaces loaded resources on startup +
/// via `/reload`); here it's a transcript note rather than an overlay since the
/// resource set is session-static between `/reload`s (deferred).
pub(super) fn show_context_panel(
    chat: &Arc<Container>,
    resources: &Arc<rpi_harness::types::AgentHarnessResources>,
) {
    let skills = resources.skills.as_deref().unwrap_or(&[]);
    let templates = resources.prompt_templates.as_deref().unwrap_or(&[]);
    let mut lines: Vec<String> = Vec::new();
    lines.push("📂 Discovered resources for this session:".into());

    if skills.is_empty() {
        lines.push(
            "  Skills: (none discovered — create .rpi/skills/ or ~/.rpi/agent/skills/)".into(),
        );
    } else {
        lines.push(format!("  Skills ({}):", skills.len()));
        for s in skills {
            let marker = if s.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            let desc: String = s.description.chars().take(72).collect();
            lines.push(format!("    • {}{marker} — {desc}", s.name));
        }
    }

    if templates.is_empty() {
        lines.push(
            "  Prompt templates: (none — create .rpi/prompts/ or ~/.rpi/agent/prompts/)".into(),
        );
    } else {
        lines.push(format!("  Prompt templates ({}):", templates.len()));
        for t in templates {
            let desc = t
                .description
                .as_deref()
                .unwrap_or("(no description)")
                .chars()
                .take(72)
                .collect::<String>();
            lines.push(format!("    • /{} — {desc}", t.name));
        }
    }
    lines.push("  Context files (AGENTS.md/CLAUDE.md) are injected from the ancestor walk;".into());
    lines.push("  SYSTEM.md / APPEND_SYSTEM.md feed the base + append prompt sections.".into());
    lines.push(
        "  Use --no-skills/-ns, --no-prompt-templates/-np, --no-context-files/-nc to suppress."
            .into(),
    );
    let body = lines.join("\n");
    container_note_block(chat, &body);
}

/// Append a multi-line neutral note (header line + body) to the chat container.
pub(super) fn container_note_block(container: &Arc<Container>, body: &str) {
    for line in body.lines() {
        container.add_child(Arc::new(Text::new(line.to_string(), 1, 0)));
    }
    container.add_child(Arc::new(Spacer::new(1)));
}
