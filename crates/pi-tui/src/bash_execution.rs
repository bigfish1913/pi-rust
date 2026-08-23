//! Bash execution component — streaming display port of `bash-execution.ts`.
//!
//! This is a **display-only** consumer of the bash tool's existing `on_update`
//! stream: it renders a command header, a spinner while running, a tailed
//! preview of captured output, and a status/truncation line on completion.
//! There is no `!`-prefix direct shell mode (per the session scope) and no
//! async/signal plumbing — the caller drives lifecycle via `append_output` /
//! `set_complete`.

use std::any::Any;
use std::sync::Mutex;
use std::time::Duration;

use super::component::Component;
use crate::ansi::strip_ansi;
use crate::ansi::bold;
use crate::dynamic_border::DynamicBorder;
use crate::loader::Loader;
use crate::spacer::Spacer;
use crate::theme::theme;
use crate::utils::truncate_to_width;
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
    command: String,
    output_lines: Mutex<Vec<String>>,
    status: Mutex<BashStatus>,
    exit_code: Mutex<Option<i32>>,
    expanded: Mutex<bool>,
    truncation: Mutex<BashTruncation>,
    loader: Loader,
}

impl BashExecutionComponent {
    /// Create a new component for the given command (without the leading `$`).
    pub fn new(command: impl Into<String>) -> Self {
        let loader = Loader::with_text("Running...");
        loader.start();
        Self {
            command: command.into(),
            output_lines: Mutex::new(Vec::new()),
            status: Mutex::new(BashStatus::Running),
            exit_code: Mutex::new(None),
            expanded: Mutex::new(false),
            truncation: Mutex::new(BashTruncation::default()),
            loader,
        }
    }

    /// Append a streamed chunk. ANSI is stripped and line endings normalized.
    pub fn append_output(&self, chunk: &str) {
        let clean = strip_ansi(chunk).replace("\r\n", "\n").replace('\r', "\n");
        let new_lines: Vec<&str> = clean.split('\n').collect();
        if let Ok(mut out) = self.output_lines.lock() {
            if !out.is_empty() && !new_lines.is_empty() {
                // Continuation: first new chunk appends to the last partial line.
                let last = out.last_mut().unwrap();
                last.push_str(new_lines[0]);
                out.extend(new_lines[1..].iter().map(|s| s.to_string()));
            } else {
                out.extend(new_lines.iter().map(|s| s.to_string()));
            }
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
        self.loader.stop();
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
        if hidden > 0 {
            if expanded {
                parts.push(colors.muted.fg("(Ctrl+T to collapse)"));
            } else {
                parts.push(colors.muted.fg(&format!(
                    "... {} more lines (Ctrl+T to expand)",
                    hidden
                )));
            }
        }
        match status {
            BashStatus::Cancelled => parts.push(colors.warning.fg("(cancelled)")),
            BashStatus::Error => {
                parts.push(colors.error.fg(&format!("(exit {})", exit_code.unwrap_or(-1))));
            }
            _ => {}
        }
        if truncation.truncated {
            let path = truncation.full_output_path.as_deref().unwrap_or("");
            parts.push(colors.warning.fg(&format!(
                "Output truncated. Full output: {}",
                path
            )));
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

        // Spacer + top border
        lines.extend(Spacer::new(1).render(width));
        lines.extend(DynamicBorder::with_color(colors.border).render(width));

        // Command header: "$ {command}" in accent.
        let header_text = format!("$ {}", self.command);
        let header_line = format!("  {}", colors.bash_mode.fg(&bold(&header_text)));
        lines.push(truncate_to_width(&header_line, width, "…"));

        // Output preview (collapsed: last PREVIEW_LINES visual lines;
        // expanded: all wrapped lines). Built from the captured raw output,
        // themed with the muted color and a leading newline for spacing.
        let (display_lines, hidden) = {
            let out = self.output_lines.lock().unwrap();
            let expanded = *self.expanded.lock().unwrap();
            if out.is_empty() {
                (Vec::new(), 0usize)
            } else {
                let muted = colors.muted;
                let styled = format!(
                    "\n{}",
                    out.iter().map(|l| muted.fg(l)).collect::<Vec<_>>().join("\n")
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
        lines.extend(display_lines);

        // Spinner (while running) or status line (when complete)
        let status = *self.status.lock().unwrap();
        if status == BashStatus::Running {
            lines.extend(self.loader.render(width));
            // Long-running hint: after a minute, tell the user how to abort so
            // a command without a tool-level timeout (model didn't pass one)
            // never looks stuck with no recourse.
            if self.loader.elapsed() > LONG_RUNNING_HINT_AFTER {
                let colors = theme().colors;
                let hint = format!(
                    "  {} Esc / Ctrl+C 中止",
                    colors.muted.fg("⏸")
                );
                lines.push(truncate_to_width(&hint, width, "…"));
            }
        } else {
            let sl = self.status_line(hidden);
            if !sl.is_empty() {
                lines.push(String::new());
                lines.extend(sl);
            }
        }

        // Bottom border
        lines.extend(DynamicBorder::with_color(colors.border).render(width));
        lines
    }

    fn invalidate(&self) {
        self.loader.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_renders_header_and_spinner() {
        let c = BashExecutionComponent::new("ls -la");
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains("ls -la"));
        assert!(joined.contains('─'));
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
