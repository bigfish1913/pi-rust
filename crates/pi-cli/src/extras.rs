//! Extras & easter-egg wiring — thin host wrappers over the rpi-tui components.
//!
//! Hosts the chat-container push helpers for the armin XBM art and the earendil
//! announcement, plus the first-time-setup sentinel logic. These are the
//! `interactive_tui.rs`-side glue (`/armin`, `/earendil`, and the first-launch
//! gate) that depend on `rpi_tui::Container` + `Arc` — kept out of the library
//! crate so rpi-tui stays cli-free (project constraint).

use std::sync::Arc;

use rpi_tui::{ArminComponent, Container, EarendilAnnouncementComponent, Spacer};

use crate::config;

/// Push the armin XBM art block (+ a trailing spacer) into the chat transcript.
/// Triggered by `/armin`.
pub fn add_armin(chat: &Arc<Container>) {
    chat.add_child(Arc::new(ArminComponent::new()));
    chat.add_child(Arc::new(Spacer::new(1)));
}

/// Push the earendil announcement block (+ a trailing spacer) into the chat
/// transcript, and mark it seen via the `~/.rpi/agent/.earendil_seen` sentinel.
/// Triggered by `/earendil` or the first-launch gate.
pub fn add_earendil(chat: &Arc<Container>) {
    chat.add_child(Arc::new(EarendilAnnouncementComponent::new()));
    chat.add_child(Arc::new(Spacer::new(1)));
    let _ = mark_earendil_seen();
}

/// Path of the "earendil announcement seen" sentinel, under the agent dir
/// (`~/.rpi/agent/.earendil_seen`). Delegates to [`config::agent_dir`] so an
/// `RPI_CODING_AGENT_DIR` override is honored (the old `rpi_dir()` ignored it).
/// Returns `None` when the home dir can't be resolved.
pub fn earendil_seen_path() -> Option<std::path::PathBuf> {
    config::agent_dir().ok().map(|d| d.join(".earendil_seen"))
}

/// Whether the earendil announcement has already been shown (sentinel present).
pub fn earendil_seen() -> bool {
    earendil_seen_path().map(|p| p.exists()).unwrap_or(false)
}

/// Write the `~/.rpi/agent/.earendil_seen` sentinel so the announcement isn't
/// shown again on later launches. Best-effort: a missing agent dir is created.
fn mark_earendil_seen() -> std::io::Result<()> {
    let path = earendil_seen_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir for .rpi/agent")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b"1")
}

/// Path of the "first-time setup done" sentinel, under the agent dir
/// (`~/.rpi/agent/.setup_done`).
pub fn setup_done_path() -> Option<std::path::PathBuf> {
    config::agent_dir().ok().map(|d| d.join(".setup_done"))
}

/// Whether first-time setup has already been completed (sentinel present).
pub fn setup_done() -> bool {
    setup_done_path().map(|p| p.exists()).unwrap_or(false)
}

/// Mark first-time setup complete (write the sentinel). Best-effort.
pub fn mark_setup_done() -> std::io::Result<()> {
    let path = setup_done_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir for .rpi/agent")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b"1")
}

/// If first-time setup hasn't run yet, show a brief setup note + the earendil
/// announcement in the chat container. The TS original is a multi-step dialog
/// (theme picker, analytics opt-in); this v1 simplifies to a one-shot banner
/// + the theme remains pickable via `/theme`. Analytics is deferred (no
/// telemetry wiring). Returns `true` if anything was shown.
pub fn maybe_first_time_setup(chat: &Arc<Container>) -> bool {
    use rpi_tui::Component;
    use rpi_tui::{DynamicBorder, Spacer, Text};
    if setup_done() {
        return false;
    }
    let accent = rpi_tui::theme().colors.accent;
    let muted = rpi_tui::theme().colors.muted;
    let border = DynamicBorder::with_color(accent);
    // Use the Component trait method explicitly for the border/Text render.
    let mut lines: Vec<String> = Vec::new();
    lines.extend(border.render(80));
    lines.push(format!(
        " {} Welcome to rpi!",
        accent.fg(&bold("Welcome to rpi!"))
    ));
    lines.push(format!(
        " {} Pick a theme with /theme (dark/light/monochrome).",
        muted.fg("Pick a theme with /theme (dark/light/monochrome).")
    ));
    lines.push(format!(
        " {} Type /help for commands.",
        muted.fg("Type /help for commands.")
    ));
    lines.extend(border.render(80));
    for line in lines {
        chat.add_child(Arc::new(Text::new(line, 1, 0)));
    }
    chat.add_child(Arc::new(Spacer::new(1)));
    add_earendil(chat);
    let _ = mark_setup_done();
    true
}

fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[22m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_armin_pushes_component() {
        let chat = Arc::new(Container::new());
        let before = chat.child_count();
        add_armin(&chat);
        assert_eq!(chat.child_count(), before + 2); // component + spacer
    }

    #[test]
    fn test_add_earendil_pushes_component() {
        let chat = Arc::new(Container::new());
        let before = chat.child_count();
        add_earendil(&chat);
        assert_eq!(chat.child_count(), before + 2);
    }

    #[test]
    fn test_sentinel_paths_under_agent_dir() {
        // The sentinels live directly under the resolved agent dir and honor
        // the `RPI_CODING_AGENT_DIR` override (delegated to `config::agent_dir`).
        // With the override set, agent_dir() returns the override verbatim, so
        // the sentinel's parent must equal agent_dir() — not end in a literal
        // "agent" segment (that only holds for the default nested path).
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let prev = std::env::var_os(crate::config::CONFIG_DIR_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
        let agent = crate::config::agent_dir().unwrap();
        assert_eq!(agent.as_path(), tmp.path());
        if let Some(p) = earendil_seen_path() {
            assert!(p.ends_with(".earendil_seen"));
            assert_eq!(p.parent().unwrap(), agent);
        }
        if let Some(p) = setup_done_path() {
            assert!(p.ends_with(".setup_done"));
            assert_eq!(p.parent().unwrap(), agent);
        }
        match prev {
            Some(v) => std::env::set_var(crate::config::CONFIG_DIR_ENV, v),
            None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
        }
    }
}
