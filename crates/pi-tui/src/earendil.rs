//! Earendil announcement block — port of `earendil-announcement.ts`, minus
//! the bundled `clankolas.png` image (rpi-cli doesn't ship the asset; the TS
//! version loads it from a bundled path and renders it inline when present).
//!
//! Renders a DynamicBorder (accent) + bold "pi has joined Earendil" + the blog
//! URL + a closing border. The host triggers it via `/earendil` or on first
//! launch (gated by a `~/.rpi/.earendil_seen` sentinel — handled in the cli
//! wiring, not here). Image omitted intentionally; noted in the plan.

use std::any::Any;

use super::component::Component;
use crate::dynamic_border::DynamicBorder;
use crate::spacer::Spacer;
use crate::text::Text;
use crate::theme::theme;

const BLOG_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";

/// Static announcement block. The TS original is a `Container` subclass that
/// composes children; rpi-tui composes via the same primitives (DynamicBorder,
/// Text, Spacer) but renders them itself for a self-contained Component.
pub struct EarendilAnnouncementComponent {
    top: DynamicBorder,
    bottom: DynamicBorder,
}

impl EarendilAnnouncementComponent {
    pub fn new() -> Self {
        let accent = theme().colors.accent;
        Self {
            top: DynamicBorder::with_color(accent),
            bottom: DynamicBorder::with_color(accent),
        }
    }
}

impl Default for EarendilAnnouncementComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for EarendilAnnouncementComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let accent = theme().colors.accent;
        let muted = theme().colors.muted;
        let mut lines = Vec::new();
        lines.extend(self.top.render(width));
        lines.push(
            Text::new(accent.fg(&bold("pi has joined Earendil")), 1, 0)
                .render(width)
                .remove(0),
        );
        lines.extend(Spacer::new(1).render(width));
        lines.push(
            Text::new(muted.fg("Read the blog post:"), 1, 0)
                .render(width)
                .remove(0),
        );
        lines.push(Text::new(accent.fg(BLOG_URL), 1, 0).render(width).remove(0));
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.bottom.render(width));
        lines
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Minimal bold wrapper for the title (theme has no `bold` color slot, so we
/// wrap the string in the ANSI bold SGR). Kept inline rather than reaching for
/// `ansi::bold` to avoid a cross-module import just for one line.
fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[22m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    #[test]
    fn test_earendil_renders_url_and_title() {
        let comp = EarendilAnnouncementComponent::new();
        let lines = comp.render(80);
        let joined: String = lines.join("\n");
        assert!(
            joined.contains("pi has joined Earendil"),
            "title missing: {joined}"
        );
        assert!(joined.contains(BLOG_URL), "url missing: {joined}");
        // Borders present (─ run at width).
        assert!(joined.contains('─'), "no border drawn");
    }

    #[test]
    fn test_earendil_narrow_no_panic() {
        let comp = EarendilAnnouncementComponent::new();
        let _ = comp.render(20);
    }
}
