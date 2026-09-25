//! The startup brand lockup: the three-bar mark, the `rpi · rust` wordmark, and
//! a short one-shot crab animation walking in beside them.
//!
//! Two constraints shape this module.
//!
//! **Width must be unambiguous.** The lockup this replaced drew a crab emoji
//! above a box containing `π`. `π` is East-Asian *Ambiguous*: width 1 in most
//! locales and width 2 under a CJK locale, which walked the box borders out of
//! alignment depending on the user's system. The mark here is built from `█`,
//! which is single-width everywhere; the crab is the only wide glyph, and its
//! width is the one thing the layout has to account for.
//!
//! **No idle wakeups.** The render scheduler is parked on a condvar when no frame
//! is owed — "idle screens pay no wakeups at all". An animation that never ended
//! would force a repaint on every tick forever, for a decorative detail. So the
//! crab walks in once, for [`TOTAL_MS`], and then the header is static; the
//! render tick only asks for frames while [`brand_animation_active`] is true.

use std::sync::OnceLock;
use std::time::Instant;

use rpi_tui::{bold as tui_bold, theme as current_theme, Component};

use crate::args::is_truthy_env_flag;

/// Set to `1`/`true`/`yes` to render the lockup without the crab.
pub const NO_EMOJI_ENV: &str = "RPI_NO_EMOJI";

/// The Rust mascot. Emoji render correctly in every terminal we target, but the
/// rest of the UI deliberately avoids them (see the tool status indicator
/// comment in `rpi-tui`), so this one is opt-out rather than mandatory.
pub const CRAB: &str = "🦀";

/// The crab's leading padding, in columns, from far right to settled.
///
/// An easing table rather than a straight line: the crab covers most of the
/// distance quickly and then steps the last two columns, which reads as walking
/// rather than as a slide. The final `0` is the settled position, so the number
/// of entries is also the number of animated frames.
pub const CRAB_STEPS: &[usize] = &[10, 7, 5, 3, 2, 1, 0];

/// One animation frame. Twice the render tick's 80 ms interval, so each frame is
/// painted exactly twice instead of being skipped or doubled.
pub const STEP_MS: u64 = 160;

/// Columns between the wordmark and the settled crab.
pub const CRAB_GAP: usize = 2;

/// Total animation length: the walk plus the frame it settles on.
pub const TOTAL_MS: u64 = STEP_MS * CRAB_STEPS.len() as u64;

/// The walk's clock, started by the first paint rather than at construction.
///
/// Startup does real work before the first frame (config, session store, tool
/// registry), so a clock started when the header was *built* could have expired
/// before the user ever saw it. Starting it on the first render means the crab
/// always walks for its full duration, from the moment it is visible.
fn animation_clock() -> Option<Instant> {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get().copied()
}

/// Start the clock if this is the first paint; a no-op afterwards.
fn mark_first_paint() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Pure form of the opt-out check, so the logic is testable without mutating the
/// process environment (which is global state and races with parallel tests).
pub(crate) fn crab_enabled_from(value: Option<&str>) -> bool {
    !is_truthy_env_flag(value)
}

fn crab_enabled() -> bool {
    crab_enabled_from(std::env::var(NO_EMOJI_ENV).ok().as_deref())
}

/// How far right of its resting place the crab is, in columns, at `elapsed_ms`.
///
/// Returns `None` once the walk is over, which is what makes the animation
/// bounded rather than a permanent repaint trigger.
pub(crate) fn crab_pad_at(elapsed_ms: u64) -> Option<usize> {
    CRAB_STEPS.get((elapsed_ms / STEP_MS) as usize).copied()
}

/// Whether the walk still owes frames. The render tick uses this to decide
/// whether an otherwise idle session should repaint.
///
/// `false` before the first paint: nothing has been drawn yet, so there is
/// nothing to animate, and the initial frame is painted by startup anyway.
pub fn brand_animation_active() -> bool {
    if !crab_enabled() {
        return false;
    }
    animation_clock().is_some_and(|start| crab_pad_at(elapsed_ms(start)).is_some())
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// The five rows of the mark and wordmark, with the crab at `crab_pad` columns
/// right of its resting place (`None` renders no crab at all).
///
/// Kept free of the clock so the layout can be asserted directly in tests.
fn render_lockup(crab_pad: Option<usize>) -> Vec<String> {
    let c = current_theme().colors;
    let bar = c.dim.fg("██");
    let bar_accent = c.accent.fg("██");

    let mut rows = vec![
        format!("      {bar_accent}"),
        format!("      {bar_accent}"),
        format!("   {bar} {bar_accent}"),
        format!("   {bar} {bar_accent} {bar}"),
        format!(
            "   {bar} {bar_accent} {bar}   {} {}",
            c.accent.fg(&tui_bold("rpi")),
            c.muted.fg("· rust")
        ),
    ];

    if let Some(pad) = crab_pad {
        rows[4].push_str(&" ".repeat(CRAB_GAP + pad));
        rows[4].push_str(CRAB);
    }

    rows
}

/// The animated lockup. Add one to the welcome header; it needs no handle from
/// the render tick, which asks [`brand_animation_active`] instead.
pub struct BrandLockup;

impl BrandLockup {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrandLockup {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for BrandLockup {
    fn render(&self, _width: usize) -> Vec<String> {
        let crab_pad = if crab_enabled() {
            // First paint starts the walk; later rebuilds just read it.
            crab_pad_at(elapsed_ms(mark_first_paint()))
        } else {
            None
        };
        render_lockup(crab_pad)
    }

    fn invalidate(&self) {
        // Nothing is cached: the frame is derived from the clock at render time.
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::ansi::{strip_ansi, visible_width};

    #[test]
    fn mark_rows_match_the_site_brand() {
        let rows: Vec<String> = render_lockup(None).iter().map(|r| strip_ansi(r)).collect();

        // Three bottom-aligned bars: the tallest in the middle, one row taller
        // than the left bar and two taller than the right one.
        assert_eq!(rows[0], "      ██");
        assert_eq!(rows[1], "      ██");
        assert_eq!(rows[2], "   ██ ██");
        assert_eq!(rows[3], "   ██ ██ ██");
        assert!(rows[4].starts_with("   ██ ██ ██   rpi · rust"));
    }

    #[test]
    fn crab_is_the_only_wide_glyph_and_is_accounted_for() {
        // The padding maths below assumes the crab occupies two columns. If a
        // dependency bump changes that, the lockup would silently misalign.
        assert_eq!(visible_width(CRAB), 2, "crab width assumption changed");

        let settled: Vec<String> = render_lockup(Some(0)).iter().map(|r| strip_ansi(r)).collect();
        let animated: Vec<String> = render_lockup(Some(10)).iter().map(|r| strip_ansi(r)).collect();

        // Each column of padding must move the crab exactly one column, so the
        // line grows by exactly that much and nothing else shifts.
        assert_eq!(
            visible_width(&animated[4]) - visible_width(&settled[4]),
            10,
            "padding must translate 1:1 into columns"
        );
        assert!(settled[4].ends_with(CRAB));
        assert!(animated[4].ends_with(CRAB));
    }

    #[test]
    fn crab_walks_left_and_then_stops() {
        // Padding is non-increasing, so the crab only ever moves toward the
        // wordmark, never away from it.
        for pair in CRAB_STEPS.windows(2) {
            assert!(pair[0] >= pair[1], "crab moved backwards: {pair:?}");
        }
        assert_eq!(CRAB_STEPS.last(), Some(&0), "the crab must settle");

        // Bounded: once the table is exhausted there is nothing more to paint,
        // which is what keeps an idle header from repainting forever.
        assert_eq!(crab_pad_at(0), Some(10));
        assert_eq!(crab_pad_at(TOTAL_MS - 1), Some(0));
        assert_eq!(crab_pad_at(TOTAL_MS), None);
        assert_eq!(crab_pad_at(TOTAL_MS * 10), None);

        assert!(!brand_animation_active() || crab_enabled());
    }

    #[test]
    fn no_emoji_env_disables_only_the_crab() {
        assert!(crab_enabled_from(None));
        for value in ["1", "true", "yes", "TRUE", "Yes"] {
            assert!(!crab_enabled_from(Some(value)), "{value} should disable");
        }
        for value in ["", "0", "false", "no", "off"] {
            assert!(crab_enabled_from(Some(value)), "{value} should not disable");
        }

        // Without the crab the lockup is unchanged apart from its absence.
        let rows = render_lockup(None);
        assert_eq!(rows.len(), 5);
        assert!(!rows[4].contains(CRAB));
    }
}
