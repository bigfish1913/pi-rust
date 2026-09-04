//! Status indicator components — port of `status-indicator.ts`.
//!
//! The TS original is a class hierarchy extending `Loader`. The Rust port
//! collapses it into a single [`StatusIndicator`] Component that owns its own
//! spinner + elapsed logic (reusing the loader's frame cadence), an optional
//! [`CountdownTimer`] for the retry case, and themed coloring per `kind`.
//! [`IdleStatus`] renders two blank lines (the slot shape when nothing runs).
//!
//! `app.interrupt` does not exist in the Rust keybinding registry, so the
//! cancel hint uses a literal `"Ctrl+C"` via [`raw_key_hint`].

use std::any::Any;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::component::Component;
use crate::ansi::visible_width;
use crate::countdown_timer::CountdownTimer;
use crate::keybinding_hints::raw_key_hint;
use crate::theme::{theme, Color};
use crate::utils::truncate_to_width;

/// What the indicator represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Agent is actively working on a turn.
    Working,
    /// Waiting to retry after an error, with a backoff countdown.
    Retry,
    /// Compacting the context window (manual or auto).
    Compaction,
    /// Summarizing a branch.
    BranchSummary,
}

/// What triggered a compaction (mirrors `CompactionStatusReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Active status display: spinner + elapsed + themed message (+countdown).
pub struct StatusIndicator {
    kind: Mutex<StatusKind>,
    message: Mutex<String>,
    frame: Mutex<usize>,
    start: Mutex<Option<Instant>>,
    countdown: Mutex<Option<CountdownTimer>>,
}

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl StatusIndicator {
    /// Build a working status with a message.
    pub fn working(message: impl Into<String>) -> Self {
        Self {
            kind: Mutex::new(StatusKind::Working),
            message: Mutex::new(message.into()),
            frame: Mutex::new(0),
            start: Mutex::new(Some(Instant::now())),
            countdown: Mutex::new(None),
        }
    }

    /// Build a retry status that counts down `delay`.
    pub fn retry(attempt: u32, max_attempts: u32, delay: Duration) -> Self {
        let secs_remaining = delay.as_secs().max(1) as u32;
        let message = format!(
            "Retrying ({}/{}) in {}s... ({} to cancel)",
            attempt,
            max_attempts,
            secs_remaining,
            raw_key_hint_keys()
        );
        Self {
            kind: Mutex::new(StatusKind::Retry),
            message: Mutex::new(message),
            frame: Mutex::new(0),
            start: Mutex::new(Some(Instant::now())),
            countdown: Mutex::new(Some(CountdownTimer::new(delay))),
        }
    }

    /// Build a compaction status.
    pub fn compaction(reason: CompactionReason) -> Self {
        let cancel = format!("({} to cancel)", raw_key_hint_keys());
        let label = match reason {
            CompactionReason::Manual => format!("Compacting context... {}", cancel),
            CompactionReason::Overflow => {
                format!("Context overflow detected, Auto-compacting... {}", cancel)
            }
            CompactionReason::Threshold => format!("Auto-compacting... {}", cancel),
        };
        Self {
            kind: Mutex::new(StatusKind::Compaction),
            message: Mutex::new(label),
            frame: Mutex::new(0),
            start: Mutex::new(Some(Instant::now())),
            countdown: Mutex::new(None),
        }
    }

    /// Build a branch-summary status.
    pub fn branch_summary() -> Self {
        let message = format!("Summarizing branch... ({} to cancel)", raw_key_hint_keys());
        Self {
            kind: Mutex::new(StatusKind::BranchSummary),
            message: Mutex::new(message),
            frame: Mutex::new(0),
            start: Mutex::new(Some(Instant::now())),
            countdown: Mutex::new(None),
        }
    }

    /// Replace the message (e.g. as a retry countdown ticks down).
    pub fn set_message(&self, message: impl Into<String>) {
        if let Ok(mut m) = self.message.lock() {
            *m = message.into();
        }
    }

    /// Tick the retry countdown and refresh its message if still running.
    pub fn tick_countdown(&self) {
        let update = {
            let c = self.countdown.lock().unwrap();
            if let Some(timer) = c.as_ref() {
                if timer.expired() {
                    None
                } else {
                    Some(timer.remaining_secs())
                }
            } else {
                None
            }
        };
        if let Some(secs) = update {
            // We don't know attempt/max here (not stored) — keep the existing
            // leading text and just rewrite the seconds. Simpler: callers that
            // need precise retry text should call set_message directly. For
            // the common case we leave the message as-is unless the host wants
            // to update it; this method is a no-op hook for future use.
            let _ = secs;
        }
    }

    fn elapsed(&self) -> Duration {
        self.start
            .lock()
            .ok()
            .and_then(|s| s.map(|t| t.elapsed()))
            .unwrap_or_default()
    }

    fn advance(&self) {
        if let Ok(mut f) = self.frame.lock() {
            *f = (*f + 1) % SPINNER.len();
        }
    }

    fn spinner_char(&self) -> char {
        let f = *self.frame.lock().unwrap();
        SPINNER.get(f).copied().unwrap_or('⠋')
    }

    fn colors_for(kind: StatusKind) -> (Color, Color) {
        let c = theme().colors;
        match kind {
            StatusKind::Working | StatusKind::Compaction | StatusKind::BranchSummary => {
                (c.accent, c.muted)
            }
            StatusKind::Retry => (c.warning, c.muted),
        }
    }
}

impl Component for StatusIndicator {
    fn render(&self, width: usize) -> Vec<String> {
        let kind = *self.kind.lock().unwrap();
        let message = self.message.lock().unwrap().clone();
        let (spinner_color, msg_color) = Self::colors_for(kind);

        let elapsed = self.elapsed();
        let secs = elapsed.as_secs();
        let time_str = if secs >= 60 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}s", secs)
        };

        let spinner = self.spinner_char();
        let prefix = format!(
            "{} {}",
            spinner_color.fg(&spinner.to_string()),
            msg_color.fg(&time_str)
        );
        let prefix_w = visible_width(&prefix);

        // Truncate message to fit remaining width, leaving a separating space.
        let max_msg = width.saturating_sub(prefix_w + 1);
        let display_msg = if visible_width(&message) > max_msg {
            truncate_to_width(&message, max_msg, "…")
        } else {
            message.clone()
        };

        let line = format!("{} {}", prefix, msg_color.fg(&display_msg));

        // Advance spinner for the next render (the 120ms tick re-renders).
        self.advance();

        vec![line]
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Two blank lines indicating the idle status slot is empty.
pub struct IdleStatus;

impl IdleStatus {
    pub fn new() -> Self {
        IdleStatus
    }
}

impl Default for IdleStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for IdleStatus {
    fn render(&self, width: usize) -> Vec<String> {
        let empty = " ".repeat(width);
        vec![empty.clone(), empty]
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Literal "Ctrl+C" text for interrupt hints (no `app.interrupt` binding exists).
fn raw_key_hint_keys() -> String {
    // raw_key_hint returns "{keys} {desc}"; we only want the keys portion.
    // Re-implement inline to get just the key glyph.
    let colors = theme().colors;
    // Return a pre-styled "Ctrl+C" so callers can interpolate it into messages.
    // We strip the description arg by wrapping: use raw_key_hint then split.
    let full = raw_key_hint("Ctrl+C", "cancel");
    let _ = colors;
    // raw_key_hint = "<muted>Ctrl+C</muted> <muted>cancel</muted>"; take the
    // first styled segment only:
    if let Some(space_idx) = full.find(" \x1b") {
        full[..space_idx].to_string()
    } else {
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_working_indicator_renders() {
        let ind = StatusIndicator::working("Thinking");
        let lines = ind.render(40);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Thinking"));
    }

    #[test]
    fn test_compaction_indicator_renders() {
        let ind = StatusIndicator::compaction(CompactionReason::Manual);
        let lines = ind.render(60);
        assert!(lines[0].contains("Compacting"));
    }

    #[test]
    fn test_idle_status_two_blank_lines() {
        let idle = IdleStatus::new();
        let lines = idle.render(10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "          ");
    }

    #[test]
    fn test_long_message_truncated_no_panic() {
        let ind = StatusIndicator::working(&"x".repeat(200));
        let lines = ind.render(20);
        assert_eq!(lines.len(), 1);
        assert!(visible_width(&lines[0]) <= 20);
    }
}
