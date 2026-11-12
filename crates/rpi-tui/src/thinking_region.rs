//! Per-message thinking expand/collapse hit regions.
//!
//! The transcript lives inside one primary [`ScrollView`], so the layout tree
//! carries no per-component boxes to hit-test against (`layout_scroll_view`
//! flattens the document into one box). Instead, an assistant message embeds a
//! zero-width private APC marker on every row it renders — the same protocol
//! [`crate::ansi::CURSOR_MARKER`] uses for the hardware cursor — carrying the
//! message id and the clickable column span. The renderer scans each frame's
//! composited lines, resolves every marker to a screen rectangle, and strips
//! the marker before diffing; mouse handlers then test click coordinates
//! against those rectangles without touching the selection tracker.
//!
//! Marker shape: `ESC _ pi:z <id> x <width> BEL`. The id is opaque (no `x`);
//! `width` is the visible cell span the marker's row offers for clicking.

/// The zero-width marker emitted on thinking rows.
pub const THINKING_REGION_MARKER_PREFIX: &str = "\x1b_pi:z";

/// One clickable region resolved from a frame: `row` is the screen row,
/// `start..end` the half-open column span of the message's rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingRegion {
    pub message_id: String,
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

/// Parse one marker occurrence into `(id, width, chars_consumed)`.
/// Returns `None` on a malformed tail (missing `x`/BEL or a non-numeric
/// width). `start` is the byte offset of the escape introducer.
pub(crate) fn parse_marker(line: &str, start: usize) -> Option<(String, usize, usize)> {
    let rest = &line[start + THINKING_REGION_MARKER_PREFIX.len()..];
    let x = rest.find('x')?;
    if x == 0 {
        return None;
    }
    let id = &rest[..x];
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let after_x = &rest[x + 1..];
    // Terminators: BEL (legacy shape) or ST (ESC \) — the render pipeline's
    // control filter drops BEL, so the marker ships with ST.
    // The FIRST terminator wins: later line content (e.g. an OSC8 link's
    // BEL) must not be mistaken for this marker's terminator.
    let bel = after_x.find('\x07');
    let st = after_x.find("\u{1b}\\");
    let (terminator_len, width) = match (bel, st) {
        (Some(b), Some(t)) if t < b => (t + 2, &after_x[..t]),
        (Some(b), _) => (b + 1, &after_x[..b]),
        (None, Some(t)) => (t + 2, &after_x[..t]),
        (None, None) => return None,
    };
    let width: usize = width.parse().ok()?;
    Some((
        id.to_string(),
        width,
        THINKING_REGION_MARKER_PREFIX.len() + x + 1 + terminator_len,
    ))
}

/// Resolve every thinking region on the given composited lines.
///
/// Rows already carry absolute screen geometry (the frame is fully
/// composited, including scrolled viewport slices), so a region's rectangle
/// is simply `(row, 0, width)`. The renderer composites the message's line at
/// column `output_pad` with nothing to its left, and the transcript is
/// full-width, so a click anywhere on the row belongs to the message.
pub fn collect_regions(lines: &[String]) -> Vec<ThinkingRegion> {
    let mut regions = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let mut search = 0;
        while let Some(found) = line[search..].find(THINKING_REGION_MARKER_PREFIX) {
            let start = search + found;
            match parse_marker(line, start) {
                Some((id, width, consumed)) => {
                    regions.push(ThinkingRegion {
                        message_id: id,
                        row,
                        start: 0,
                        end: width,
                    });
                    search = start + consumed;
                }
                None => {
                    // Skip the introducer and keep scanning: a malformed tail
                    // must not loop forever nor hide a later valid marker.
                    search = start + THINKING_REGION_MARKER_PREFIX.len();
                }
            }
        }
    }
    regions
}

/// Whether a left-button press hits any region.
pub fn hit(regions: &[ThinkingRegion], column: u16, row: u16) -> Option<&ThinkingRegion> {
    let (column, row) = (column as usize, row as usize);
    regions
        .iter()
        .find(|r| r.row == row && column >= r.start && column < r.end)
}

/// Strip every thinking marker from the given lines, in place.
pub fn strip_markers(lines: &mut [String]) {
    for line in lines {
        if line.contains(THINKING_REGION_MARKER_PREFIX) {
            *line = strip_line(line);
        }
    }
}

fn strip_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(found) = rest.find(THINKING_REGION_MARKER_PREFIX) {
        out.push_str(&rest[..found]);
        let tail = &rest[found..];
        rest = match parse_marker(tail, 0) {
            Some((_, _, consumed)) => &tail[consumed..],
            None => {
                // Keep the bytes: not ours to drop.
                out.push_str(tail);
                return out;
            }
        };
    }
    out.push_str(rest);
    out
}

/// Test/`pub` wrapper around [`parse_marker`] (`(id, width, consumed)`).
#[doc(hidden)]
pub fn parse_marker_public(line: &str, start: usize) -> Option<(String, usize, usize)> {
    parse_marker(line, start)
}

/// Build the marker string for a message row.
pub fn marker(message_id: &str, width: usize) -> String {
    // ST (ESC \) terminates the APC: a BEL here would be dropped by the
    // render pipeline's control-character filter (`wrap_text_with_ansi`),
    // leaving the marker unterminated for `strip_ansi`.
    format!("{THINKING_REGION_MARKER_PREFIX}{message_id}x{width}\u{1b}\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_through_collect() {
        let line = format!("  dim label{}", marker("m1", 12));
        let regions = collect_regions(&[line]);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].message_id, "m1");
        assert_eq!(regions[0].row, 0);
        assert_eq!(regions[0].end, 12);
        assert!(hit(&regions, 5, 0).is_some());
        assert!(hit(&regions, 12, 0).is_none());
        assert!(hit(&regions, 5, 1).is_none());
    }

    #[test]
    fn strip_removes_wellformed_and_keeps_malformed() {
        let good = format!("a{}b", marker("m", 3));
        assert_eq!(strip_line(&good), "ab");
        let bad = format!("a{THINKING_REGION_MARKER_PREFIX}nox\x07b");
        assert_eq!(strip_line(&bad.clone()), bad);
        assert_eq!(
            collect_regions(&[bad]).len(),
            0,
            "malformed tails never resolve to a region"
        );
    }

    #[test]
    fn multiple_markers_on_one_line_and_across_rows() {
        let line = format!("{}mid{}", marker("a", 4), marker("b", 9));
        let regions = collect_regions(&[line.clone(), line.clone()]);
        assert_eq!(regions.len(), 4);
        assert_eq!(regions[0].message_id, "a");
        assert_eq!(regions[1].message_id, "b");
        assert_eq!(regions[2].row, 1);
        // Stripping drops both occurrences exactly once.
        assert_eq!(strip_line(&line), "mid");
    }

    #[test]
    fn wrap_and_render_pipeline_keep_the_marker() {
        // The marker must survive `wrap_text_with_ansi`'s control filtering
        // and `isolate_wrapped_lines`' per-row restyle (both are part of every
        // Text render), or the label row would never resolve to a region.
        let line = format!("{}{}", crate::ansi::italic("Thinking..."), marker("t1", 12));
        let wrapped = crate::utils::wrap_text_with_ansi(&line, 80);
        assert!(
            wrapped
                .iter()
                .any(|row| row.contains(THINKING_REGION_MARKER_PREFIX)),
            "wrap dropped the marker: {wrapped:?}"
        );
    }
}
