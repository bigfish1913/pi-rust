//! Bash execution component — streaming display port of `bash-execution.ts`.
//!
//! This is a **display-only** consumer of the bash tool's existing `on_update`
//! stream: it renders a command header, a spinner while running, a tailed
//! preview of captured output, and a status/truncation line on completion.
//! There is no async/signal plumbing — the caller drives lifecycle via
//! `append_output` / `set_complete`.
//!
//! It backs both the `bash` tool panels and the user-initiated `!command` /
//! `!!command` shell mode. Commands submitted with `!!` are excluded from the
//! model context; those panels render with the dim border/header color so the
//! distinction is visible, matching `bash-execution.ts`.

use std::any::Any;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::component::Component;
use crate::ansi::bold;
use crate::ansi::strip_ansi;
use crate::spacer::Spacer;
use crate::theme::theme;
use crate::utils::{truncate_to_width, wrap_text_with_ansi};
use crate::visual_truncate::truncate_to_visual_lines;

/// Preview line limit when collapsed (matches the TS `PREVIEW_LINES`).
const PREVIEW_LINES: usize = 20;

/// After this long without a result, a running bash panel shows the
/// "Esc / Ctrl+C 中止" hint (the tool has no default timeout — the model must
/// pass one, and a stuck command otherwise looks frozen).
const LONG_RUNNING_HINT_AFTER: Duration = Duration::from_secs(60);

/// Bash execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashStatus {
    Running,
    Complete,
    Cancelled,
    Error,
}

/// Truncation info extracted from `BashToolDetails` at completion.
#[derive(Debug, Clone, Default)]
pub struct BashTruncation {
    /// Whether the captured output was truncated for context limits.
    pub truncated: bool,
    /// Path to the full output file, if truncation wrote one.
    pub full_output_path: Option<String>,
}

/// Component that displays a bash command execution with streaming output.
pub struct BashExecutionComponent {
    command: Mutex<String>,
    output_lines: Mutex<Vec<String>>,
    status: Mutex<BashStatus>,
    exit_code: Mutex<Option<i32>>,
    expanded: Mutex<bool>,
    truncation: Mutex<BashTruncation>,
    /// `true` for a `!!`-prefixed user command: render dim and keep out of the
    /// model context (`bash-execution.ts` `excludeFromContext`).
    exclude_from_context: Mutex<bool>,
    /// When the command started / finished, for the live elapsed readout while
    /// running and the total elapsed after completion.
    started_at: Mutex<Option<Instant>>,
    finished_at: Mutex<Option<Instant>>,
}

impl BashExecutionComponent {
    /// Create a new component for the given command (without the leading `$`).
    pub fn new(command: impl Into<String>) -> Self {
        Self::new_with_context(command, false)
    }

    /// Create a component whose command is excluded from the model context
    /// (the `!!` prefix). Renders with the dim border/header color.
    pub fn new_excluded(command: impl Into<String>) -> Self {
        Self::new_with_context(command, true)
    }

    /// Shared constructor. `exclude_from_context` selects the dim color key.
    pub fn new_with_context(command: impl Into<String>, exclude_from_context: bool) -> Self {
        Self {
            command: Mutex::new(command.into()),
            output_lines: Mutex::new(Vec::new()),
            status: Mutex::new(BashStatus::Running),
            exit_code: Mutex::new(None),
            expanded: Mutex::new(false),
            truncation: Mutex::new(BashTruncation::default()),
            exclude_from_context: Mutex::new(exclude_from_context),
            started_at: Mutex::new(Some(Instant::now())),
            finished_at: Mutex::new(None),
        }
    }

    /// Total elapsed time, or the elapsed time so far while still running.
    fn elapsed(&self) -> Duration {
        let started = self.started_at.lock().ok().and_then(|g| *g);
        let Some(started) = started else {
            return Duration::default();
        };
        match self.finished_at.lock().ok().and_then(|g| *g) {
            Some(finished) => finished.saturating_duration_since(started),
            None => started.elapsed(),
        }
    }

    /// Set whether this command is excluded from the model context. The border
    /// and header recolor on the next render.
    pub fn set_exclude_from_context(&self, exclude: bool) {
        if let Ok(mut e) = self.exclude_from_context.lock() {
            *e = exclude;
        }
    }

    /// Whether this command is excluded from the model context.
    pub fn is_excluded_from_context(&self) -> bool {
        *self.exclude_from_context.lock().unwrap()
    }

    /// Apply the authoritative command from `ToolExecutionStart`. A panel may
    /// have been created from an earlier streaming snapshot, so this must
    /// replace a partial command as well as an empty one.
    pub fn set_command(&self, command: &str) {
        if !command.trim().is_empty() {
            let mut c = self.command.lock().unwrap();
            *c = command.to_string();
        }
    }

    /// Replace the displayed output with the latest streamed snapshot.
    ///
    /// The bash tool's `on_update` contract sends the complete captured output
    /// so far, not a delta. Appending each update duplicated all prior lines
    /// (`a`, then `a\nb` became `aa\nb`). Keep the latest snapshot instead.
    pub fn append_output(&self, chunk: &str) {
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");
        if let Ok(mut out) = self.output_lines.lock() {
            *out = if clean.is_empty() {
                Vec::new()
            } else {
                clean
                    .split('\n')
                    .map(|line| line.replace('\t', "    "))
                    .collect()
            };
        }
    }

    /// Mark execution complete.
    pub fn set_complete(
        &self,
        exit_code: Option<i32>,
        cancelled: bool,
        truncation: BashTruncation,
    ) {
        if let Ok(mut e) = self.exit_code.lock() {
            *e = exit_code;
        }
        let new_status = if cancelled {
            BashStatus::Cancelled
        } else if exit_code.map(|c| c != 0).unwrap_or(false) {
            BashStatus::Error
        } else {
            BashStatus::Complete
        };
        if let Ok(mut s) = self.status.lock() {
            *s = new_status;
        }
        if let Ok(mut t) = self.truncation.lock() {
            *t = truncation;
        }
        if let Ok(mut f) = self.finished_at.lock() {
            *f = Some(Instant::now());
        }
    }

    /// Toggle expanded (full output) vs collapsed (preview) display.
    pub fn set_expanded(&self, expanded: bool) {
        if let Ok(mut e) = self.expanded.lock() {
            *e = expanded;
        }
    }

    /// Whether currently expanded.
    pub fn is_expanded(&self) -> bool {
        *self.expanded.lock().unwrap()
    }

    /// Get the raw joined output.
    pub fn get_output(&self) -> String {
        self.output_lines
            .lock()
            .map(|o| o.join("\n"))
            .unwrap_or_default()
    }

    /// Build the trailing status/truncation line(s), if any.
    fn status_line(&self, hidden: usize) -> Vec<String> {
        let status = *self.status.lock().unwrap();
        let truncation = self.truncation.lock().unwrap().clone();
        let exit_code = *self.exit_code.lock().unwrap();
        let expanded = *self.expanded.lock().unwrap();
        let colors = theme().colors;

        let mut parts: Vec<String> = Vec::new();
        // Total elapsed up front: it is the first thing a reader looks for on a
        // finished command.
        let elapsed = self.elapsed();
        if elapsed > Duration::from_millis(100) {
            parts.push(
                colors
                    .muted
                    .fg(&crate::tool_execution::format_elapsed(elapsed)),
            );
        }
        if hidden > 0 {
            if expanded {
                parts.push(colors.muted.fg("(Ctrl+T to collapse)"));
            } else {
                parts.push(
                    colors
                        .muted
                        .fg(&format!("... {} more lines (Ctrl+T to expand)", hidden)),
                );
            }
        }
        match status {
            BashStatus::Cancelled => parts.push(colors.warning.fg("(cancelled)")),
            BashStatus::Error => {
                parts.push(
                    colors
                        .error
                        .fg(&format!("(exit {})", exit_code.unwrap_or(-1))),
                );
            }
            _ => {}
        }
        if truncation.truncated {
            let path = truncation.full_output_path.as_deref().unwrap_or("");
            parts.push(
                colors
                    .warning
                    .fg(&format!("Output truncated. Full output: {}", path)),
            );
        }
        if parts.is_empty() {
            Vec::new()
        } else {
            vec![format!("  {}", parts.join(" "))]
        }
    }
}

impl Component for BashExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let mut lines: Vec<String> = Vec::new();

        // `!!` commands are excluded from the model context, so they render
        // dim instead of the normal bash color (mirrors `bash-execution.ts`).
        let excluded = *self.exclude_from_context.lock().unwrap();
        let color_key = if excluded {
            colors.dim
        } else {
            colors.bash_mode
        };

        // Background tint for the bash panel — mirrors the tool execution
        // panel backgrounds so bash blocks visually match tool blocks.
        let bg = match *self.status.lock().unwrap() {
            BashStatus::Running => colors.tool_pending_bg,
            BashStatus::Error | BashStatus::Cancelled => colors.tool_error_bg,
            BashStatus::Complete => colors.tool_success_bg,
        };

        // Spacer
        lines.extend(Spacer::new(1).render(width));

        // Command header: "$ {command}" in accent.
        let command = self.command.lock().unwrap().clone();
        let header_text = format!("$ {command}");
        let header_width = width.saturating_sub(2).max(1);
        for part in wrap_text_with_ansi(&header_text, header_width) {
            let line = format!("  {}", color_key.fg(&bold(&part)));
            lines.push(crate::utils::apply_background_to_line(&line, width, |s| bg.bg(s)));
        }

        // Output preview
        let (display_lines, hidden) = {
            let out = self.output_lines.lock().unwrap();
            let expanded = *self.expanded.lock().unwrap();
            if out.is_empty() {
                (Vec::new(), 0usize)
            } else {
                let muted = colors.muted;
                let styled = format!(
                    "
{}",
                    out.iter()
                        .map(|l| muted.fg(l))
                        .collect::<Vec<_>>()
                        .join("
")
                );
                if expanded {
                    let all = crate::text::Text::new(&styled, 1, 0).render(width);
                    (all, 0)
                } else {
                    let r = truncate_to_visual_lines(&styled, PREVIEW_LINES, width, 1);
                    (r.visual_lines, r.skipped_count)
                }
            }
        };
        for dl in &display_lines {
            lines.push(crate::utils::apply_background_to_line(dl, width, |s| bg.bg(s)));
        }

        // Spinner (while running) or status line (when complete)
        let status = *self.status.lock().unwrap();
        if status == BashStatus::Running {
            // A live elapsed timer, not a spinner: tool panels report progress
            // the same way (static status glyph + elapsed), so two concurrent
            // operations never look like two different kinds of "busy".
            let elapsed = self.elapsed();
            let row = format!(
                "  {} {}",
                colors.accent.fg("●"),
                colors.muted.fg(&crate::tool_execution::format_elapsed(elapsed)),
            );
            let row = truncate_to_width(&row, width, "…");
            lines.push(crate::utils::apply_background_to_line(&row, width, |s| bg.bg(s)));
            if elapsed > LONG_RUNNING_HINT_AFTER {
                let hint = format!("  {} Esc / Ctrl+C 中止", colors.muted.fg("⏸"));
                let hint = truncate_to_width(&hint, width, "…");
                lines.push(crate::utils::apply_background_to_line(&hint, width, |s| bg.bg(s)));
            }
        } else {
            let sl = self.status_line(hidden);
            if !sl.is_empty() {
                lines.push(crate::utils::apply_background_to_line("", width, |s| bg.bg(s)));
                for sline in &sl {
                    lines.push(crate::utils::apply_background_to_line(sline, width, |s| bg.bg(s)));
                }
            }
        }

        lines
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_renders_header_and_live_elapsed() {
        let c = BashExecutionComponent::new("ls -la");
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains("$ ls -la"));
        // A running panel reports a live elapsed timer and a static status
        // glyph — no spinner, matching tool panels.
        let plain = strip_ansi(&joined);
        assert!(
            plain.contains('●'),
            "running panel needs the status glyph: {plain}"
        );
        assert!(
            plain.contains('s'),
            "running panel needs an elapsed readout: {plain}"
        );
        assert!(
            !plain.contains("Running"),
            "the `Running...` label was replaced by the elapsed timer: {plain}"
        );
    }

    #[test]
    fn completed_panel_reports_total_elapsed() {
        let c = BashExecutionComponent::new("sleep 1");
        c.append_output("done\n");
        std::thread::sleep(std::time::Duration::from_millis(120));
        c.set_complete(Some(0), false, BashTruncation::default());
        let plain = strip_ansi(&c.render(60).join("\n"));
        // The completion line leads with the total elapsed time.
        assert!(
            plain.contains("0.1s") || plain.contains("0.2s"),
            "total elapsed missing: {plain}"
        );
        // And it stops advancing once the command is finished.
        let first = c.render(60).join("\n");
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(first, c.render(60).join("\n"), "elapsed kept counting after completion");
    }

    #[test]
    fn test_append_and_complete() {
        let c = BashExecutionComponent::new("echo hi");
        c.append_output("hi\n");
        c.set_complete(Some(0), false, BashTruncation::default());
        let lines = c.render(40);
        let joined = lines.join("\n");
        // After completion the spinner is gone; output "hi" is visible.
        assert!(strip_ansi(&joined).contains("hi"), "joined: {joined}");
    }

    #[test]
    fn excluded_from_context_uses_dim_border_and_header() {
        // `!!cmd` renders dim rather than bashMode so the exclusion is visible
        // (mirrors `bash-execution.ts` `excludeFromContext`).
        let normal = BashExecutionComponent::new("echo hi");
        let excluded = BashExecutionComponent::new_excluded("echo hi");
        assert!(!normal.is_excluded_from_context());
        assert!(excluded.is_excluded_from_context());

        let colors = theme().colors;
        // The command header uses the bash accent (or dim when excluded), so
        // compare the foreground escape each color produces.
        let bash_start = colors.bash_mode.fg("X").split('X').next().unwrap().to_string();
        let dim_start = colors.dim.fg("X").split('X').next().unwrap().to_string();

        let normal_out = normal.render(40).join("\n");
        let excluded_out = excluded.render(40).join("\n");
        assert!(normal_out.contains(&bash_start), "normal: {normal_out:?}");
        assert!(excluded_out.contains(&dim_start), "excluded: {excluded_out:?}");

        // Both still show the command itself.
        assert!(strip_ansi(&excluded_out).contains("$ echo hi"));

        // Flipping the flag in place recolors without recreating the component.
        excluded.set_exclude_from_context(false);
        assert!(!excluded.is_excluded_from_context());
        assert!(excluded.render(40).join("\n").contains(&bash_start));
    }

    #[test]
    fn streamed_output_updates_replace_snapshots_instead_of_duplicating() {
        let c = BashExecutionComponent::new("printf test");
        c.append_output("one\n");
        c.append_output("one\ntwo\n");
        assert_eq!(c.get_output(), "one\ntwo\n");
    }

    #[test]
    fn authoritative_start_command_replaces_partial_streamed_command() {
        let c = BashExecutionComponent::new("printf");
        c.set_command("printf '\\n--- agent skills ---\\n' && find skills");
        let rendered = strip_ansi(&c.render(120).join("\n"));
        assert!(rendered.contains("printf '\\n--- agent skills ---\\n' && find skills"));
        assert!(!rendered.contains("$ printf\n"));
    }

    #[test]
    fn long_running_command_wraps_instead_of_truncating() {
        let command = "printf 'this command must remain fully visible while it runs'";
        let c = BashExecutionComponent::new(command);
        let lines = c.render(20);
        let rendered = strip_ansi(&lines.join("\n"));
        let compact: String = rendered.chars().filter(|ch| !ch.is_whitespace()).collect();
        let expected: String = command.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            compact.contains(&expected),
            "command was truncated: {rendered}"
        );
        assert!(lines
            .iter()
            .all(|line| crate::utils::visible_width(line) <= 20));
    }

    #[test]
    fn test_error_exit_status() {
        let c = BashExecutionComponent::new("false");
        c.set_complete(Some(1), false, BashTruncation::default());
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains("exit 1"));
    }

    #[test]
    fn test_truncation_path_shown() {
        let c = BashExecutionComponent::new("big-output");
        c.set_complete(
            Some(0),
            false,
            BashTruncation {
                truncated: true,
                full_output_path: Some("/tmp/out.log".to_string()),
            },
        );
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains("/tmp/out.log"), "joined: {joined}");
    }
}
