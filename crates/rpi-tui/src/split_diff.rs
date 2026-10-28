//! Two-pane ("old │ new") rendering for display diffs.
//!
//! The `edit` tool already produces a display diff string shaped
//! `+12 text` / `- 7 text` / `  3 text`. [`crate::diff::render_diff`] renders it
//! unified; this module renders the same information side by side with line
//! numbers on both panes, so a block replacement reads as a replacement instead
//! of two separate runs.
//!
//! Used only when a transcript style pack asks for it and the box is wide
//! enough; otherwise the caller keeps the unified diff.

use crate::ansi::visible_width;
use crate::theme::{theme, Color};
use crate::utils::truncate_to_width;

/// One side of a split row.
#[derive(Debug, Clone)]
struct Cell {
    line_num: Option<usize>,
    text: String,
    changed: bool,
}

#[derive(Debug, Clone)]
enum Row {
    Pair(Option<Cell>, Option<Cell>),
    Skip,
}

/// Parse `+123 text` / `-5 text` / ` 7 text` / `  ...`.
fn parse(line: &str) -> Option<(char, String, String)> {
    let mut chars = line.chars();
    let prefix = chars.next()?;
    if prefix != '+' && prefix != '-' && prefix != ' ' {
        return None;
    }
    let rest = &line[prefix.len_utf8()..];
    let mut rest = rest.trim_start_matches(' ');
    let mut num = String::new();
    for c in rest.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            break;
        }
    }
    rest = &rest[num.len()..];
    if let Some(stripped) = rest.strip_prefix(' ') {
        rest = stripped;
    }
    Some((prefix, num, rest.to_string()))
}

fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Ansi16(index) => BASIC.get(index as usize).copied(),
        Color::Ansi256(index) => {
            if index < 16 {
                return BASIC.get(index as usize).copied();
            }
            if index < 232 {
                const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let value = (index - 16) as usize;
                return Some((STEPS[value / 36], STEPS[(value % 36) / 6], STEPS[value % 6]));
            }
            let grey = 8u16 + (index as u16 - 232) * 10;
            Some((grey as u8, grey as u8, grey as u8))
        }
        Color::Default => None,
    }
}

/// A row tint: the diff hue over a dimmed box base. Hue carries the meaning,
/// luminance has to stay low, so the fill is mixed rather than blended.
fn tint(base: (u8, u8, u8), color: (u8, u8, u8), hue: f32) -> Color {
    Color::Rgb(
        (color.0 as f32 * hue + base.0 as f32 * 0.35).round() as u8,
        (color.1 as f32 * hue + base.1 as f32 * 0.35).round() as u8,
        (color.2 as f32 * hue + base.2 as f32 * 0.35).round() as u8,
    )
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// Render a display diff as two panes, or `None` when `width` cannot hold two
/// readable columns.
///
/// `max_lines` caps the body rows and anchors the window at the first change.
pub fn render_split_diff(
    diff_text: &str,
    width: usize,
    max_lines: Option<usize>,
    language: Option<&str>,
) -> Option<Vec<String>> {
    const SEPARATOR: &str = " │ ";
    const MIN_CONTENT: usize = 16;

    let colors = &theme().colors;
    let mut highlighter = language
        .filter(|language| !language.trim().is_empty())
        .map(crate::syntax_highlight::Highlighter::new);
    let parsed: Vec<(char, String, String)> = diff_text.split('\n').filter_map(parse).collect();
    if parsed.is_empty() {
        return None;
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut delta: isize = 0;
    let mut i = 0usize;
    while i < parsed.len() {
        let (prefix, num, text) = &parsed[i];
        if *prefix == ' ' && num.is_empty() && text.trim() == "..." {
            rows.push(Row::Skip);
            i += 1;
            continue;
        }
        if *prefix == ' ' {
            let left = num.parse::<usize>().ok();
            let right = left.map(|n| (n as isize + delta).max(0) as usize);
            rows.push(Row::Pair(
                Some(Cell {
                    line_num: left,
                    text: text.clone(),
                    changed: false,
                }),
                Some(Cell {
                    line_num: right,
                    text: text.clone(),
                    changed: false,
                }),
            ));
            i += 1;
            continue;
        }
        let mut removed: Vec<&(char, String, String)> = Vec::new();
        while i < parsed.len() && parsed[i].0 == '-' {
            removed.push(&parsed[i]);
            i += 1;
        }
        let mut added: Vec<&(char, String, String)> = Vec::new();
        while i < parsed.len() && parsed[i].0 == '+' {
            added.push(&parsed[i]);
            i += 1;
        }
        if removed.is_empty() && added.is_empty() {
            i += 1;
            continue;
        }
        delta += added.len() as isize - removed.len() as isize;
        for pair in 0..removed.len().max(added.len()) {
            let left = removed.get(pair).map(|(_, num, text)| Cell {
                line_num: num.parse::<usize>().ok(),
                text: text.clone(),
                changed: true,
            });
            let right = added.get(pair).map(|(_, num, text)| Cell {
                line_num: num.parse::<usize>().ok(),
                text: text.clone(),
                changed: true,
            });
            rows.push(Row::Pair(left, right));
        }
    }
    if rows.is_empty() {
        return None;
    }

    let max_number = rows
        .iter()
        .filter_map(|row| match row {
            Row::Pair(left, right) => left
                .iter()
                .chain(right.iter())
                .filter_map(|cell| cell.line_num)
                .max(),
            Row::Skip => None,
        })
        .max()
        .unwrap_or(0);
    let num_width = max_number.to_string().len().max(3);
    let available = width.checked_sub(2 + SEPARATOR.chars().count())?;
    if available < (num_width + 4) * 2 + MIN_CONTENT * 2 {
        return None;
    }
    let pane = available / 2;
    let content = pane.checked_sub(num_width + 4)?;
    if content < MIN_CONTENT {
        return None;
    }

    // Insertions and deletions have one empty side, so anchor the window on
    // *any* changed cell, not only on a paired row: otherwise a preview of an
    // insertion-only diff starts at row zero and can show only context.
    let first_change = rows
        .iter()
        .position(|row| match row {
            Row::Pair(left, right) => left.iter().chain(right.iter()).any(|cell| cell.changed),
            Row::Skip => false,
        })
        .unwrap_or(0);
    let (start, end) = match max_lines {
        Some(max) if rows.len() > max && max > 0 => {
            let start = first_change.saturating_sub(max / 4).min(rows.len() - max);
            (start, start + max)
        }
        _ => (0, rows.len()),
    };

    let base = rgb_of(colors.tool_success_bg).or_else(|| rgb_of(colors.tool_pending_bg));
    let added_fg = colors.tool_diff_added;
    let removed_fg = colors.tool_diff_removed;
    let context_fg = colors.tool_diff_context;
    let added_bg = rgb_of(added_fg).zip(base).map(|(c, b)| tint(b, c, 0.16));
    let removed_bg = rgb_of(removed_fg).zip(base).map(|(c, b)| tint(b, c, 0.16));

    let mut render_cell = |cell: &Option<Cell>, added: bool| -> String {
        let Some(cell) = cell else {
            return " ".repeat(pane);
        };
        let fg = if !cell.changed {
            context_fg
        } else if added {
            added_fg
        } else {
            removed_fg
        };
        let bar = if cell.changed { "▌" } else { " " };
        let num = cell.line_num.map(|n| n.to_string()).unwrap_or_default();
        let plain = sanitize(&cell.text);
        // Syntax highlighting turns the pane into real code; without a
        // language the plain diff colour still reads correctly.
        let body = match highlighter.as_mut() {
            Some(highlighter) if !plain.trim().is_empty() => {
                truncate_to_width(&highlighter.highlight(&plain, colors), content, "")
            }
            _ => truncate_to_width(&plain, content, ""),
        };
        let body_pad = " ".repeat(content.saturating_sub(visible_width(&body)));
        let mut line = format!(
            "{} {} {}{}",
            fg.fg(bar),
            fg.fg(&format!("{:>width$}", num, width = num_width)),
            body,
            body_pad,
        );
        // Trailing pad goes before the tint so the fill covers the pane.
        let pad = pane.saturating_sub(visible_width(&line));
        if pad > 0 {
            line.push_str(&" ".repeat(pad));
        }
        if cell.changed {
            if let Some(bg) = if added { added_bg } else { removed_bg } {
                return bg.bg(&line);
            }
        }
        line
    };

    let mut out: Vec<String> = Vec::with_capacity(end - start + 2);
    if start > 0 {
        out.push(truncate_to_width(
            &context_fg.fg(&format!("  ⋮ {start} earlier rows")),
            width,
            "…",
        ));
    }
    for row in rows.iter().take(end).skip(start) {
        let line = match row {
            Row::Skip => format!(
                "{} {}",
                context_fg.fg(&" ".repeat(pane)),
                context_fg.fg("  ...")
            ),
            Row::Pair(left, right) => format!(
                " {} {} {}",
                render_cell(left, false),
                context_fg.fg(SEPARATOR),
                render_cell(right, true)
            ),
        };
        out.push(truncate_to_width(&line, width, ""));
    }
    if end < rows.len() {
        out.push(truncate_to_width(
            &context_fg.fg(&format!("  ⋮ {} more rows", rows.len() - end)),
            width,
            "…",
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    #[test]
    fn pairs_removed_and_added_rows() {
        let diff = "+1 fn main() {\n-2     old();\n+2     new();\n 3     tail();";
        let lines = render_split_diff(diff, 100, None, None).expect("wide enough");
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain
            .iter()
            .any(|l| l.contains("old();") && l.contains("new();")));
        for line in &lines {
            assert!(visible_width(line) <= 100);
        }
    }

    #[test]
    fn refuses_narrow_boxes() {
        assert!(render_split_diff("+1 x", 40, None, None).is_none());
    }

    #[test]
    fn window_is_anchored_at_the_first_change() {
        let mut diff = String::new();
        for n in 1..=60 {
            diff.push_str(&format!(" {n} context\n"));
        }
        diff.push_str("-61 old\n+61 new\n");
        let lines = render_split_diff(&diff, 100, Some(10), None).expect("wide");
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(
            plain.iter().any(|l| l.contains("old") && l.contains("new")),
            "window lost the change: {plain:?}"
        );
        assert!(plain[0].contains("earlier rows"));
    }
}
