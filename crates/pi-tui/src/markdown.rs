//! Markdown component for rendering markdown text.
//!
//! Provides ANSI-aware markdown rendering that wraps to the available width
//! without panicking on multibyte (CJK/emoji/box-drawing) characters or
//! leaking escape sequences. Reuses the shared utilities in [`crate::utils`].

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::ansi::{bold, italic, underline};
use crate::theme::theme;
use crate::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

/// Markdown rendering options.
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Indentation for code blocks.
    pub code_block_indent: usize,
    /// Maximum width for wrapping (None = use the render width passed in).
    pub max_width: Option<usize>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            code_block_indent: 2,
            max_width: None,
        }
    }
}

/// Markdown - A component that renders markdown text.
pub struct Markdown {
    content: Mutex<String>,
    options: MarkdownOptions,
    padding_x: usize,
    padding_y: usize,
}

impl Markdown {
    /// Create a new markdown component.
    pub fn new(content: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options: MarkdownOptions::default(),
            padding_x,
            padding_y,
        }
    }

    /// Create with options.
    pub fn with_options(
        content: impl Into<String>,
        options: MarkdownOptions,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options,
            padding_x,
            padding_y,
        }
    }

    /// Set the content.
    pub fn set_content(&self, content: impl Into<String>) {
        if let Ok(mut c) = self.content.lock() {
            *c = content.into();
        }
    }

    /// Get the content.
    pub fn get_content(&self) -> String {
        self.content.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// The left-pad prefix applied to every rendered line.
    fn pad(&self) -> String {
        " ".repeat(self.padding_x)
    }

    /// Available content width accounting for horizontal padding.
    fn content_width(&self, width: usize) -> usize {
        width.saturating_sub(self.padding_x * 2)
    }

    /// Render markdown to lines.
    fn render_markdown(&self, width: usize) -> Vec<String> {
        let content = self.content.lock().map(|c| c.clone()).unwrap_or_default();
        let colors = theme().colors;
        let mut lines: Vec<String> = Vec::new();
        let indent = " ".repeat(self.options.code_block_indent);
        let pad = self.pad();
        let cwidth = self.content_width(width);

        // Add top padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        // Pre-scan for tables: collect consecutive table-ish lines.
        let raw_lines: Vec<&str> = content.lines().collect();
        let mut i = 0;

        // Code-fence state. Fenced blocks are rendered as a compact terminal
        // panel rather than echoing Markdown's literal ``` markers.
        let mut in_code_block = false;
        let mut code_fence_char = '`';
        while i < raw_lines.len() {
            let line = raw_lines[i];

            // Streamed partial closing fences (pi #5825): while the assistant
            // streams the final ```` ``` ````, the tail arrives one char at a
            // time (``, ```` ``` ````…). Rendering those would flicker the block
            // (content shrinks, then the real fence closes) — and a lone ``
            // or `````` would be mis-rendered as ordinary text. Trim any line
            // that is a partial fence run (1-2 fence chars, whole line).
            if is_partial_fence(line) {
                i += 1;
                continue;
            }

            // ---- Code fences ----
            let fence = fence_info(line);
            if let Some((fence_char, lang)) = fence {
                // Inside a block, only the matching fence closes it. This keeps
                // backticks embedded in a ~~~ block from collapsing the panel.
                if !in_code_block {
                    in_code_block = true;
                    code_fence_char = fence_char;
                    let label = if lang.is_empty() {
                        "─".to_string()
                    } else {
                        format!("─ {lang} ")
                    };
                    let chrome = format!("╭{label}");
                    lines.push(format!(
                        "{pad}{indent}{}",
                        colors.md_code_block_border.fg(&chrome)
                    ));
                } else if fence_char == code_fence_char {
                    in_code_block = false;
                    lines.push(format!(
                        "{pad}{indent}{}",
                        colors.md_code_block_border.fg("╰─")
                    ));
                } else {
                    // A non-matching fence is code content.
                    push_code_line(line, &pad, &indent, cwidth, &mut lines);
                }
                i += 1;
                continue;
            }

            if in_code_block {
                // Code is never reflowed: wrapping destroys indentation and
                // makes copied snippets invalid. Tabs are normalized and long
                // physical lines are clipped with an ellipsis.
                push_code_line(line, &pad, &indent, cwidth, &mut lines);
                i += 1;
                continue;
            }

            // ---- Tables ----
            if is_table_separator(raw_lines.get(i + 1)) && is_table_row(line) {
                // Collect the table block.
                let mut table_lines: Vec<&str> = Vec::new();
                table_lines.push(line);
                i += 1;
                // skip separator
                table_lines.push(raw_lines[i]);
                i += 1;
                while i < raw_lines.len() && is_table_row(raw_lines[i]) {
                    table_lines.push(raw_lines[i]);
                    i += 1;
                }
                render_table(&table_lines, &pad, cwidth, &mut lines);
                continue;
            }

            // ---- Headers ----
            // pi colors the whole heading (prefix + text) with mdHeading and
            // bolds it; h1/h2 are additionally underlined.
            if let Some(rest) = strip_header(line, "######") {
                let h = bold(&colors.md_heading.fg(&format!("###### {rest}")));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#####") {
                let h = bold(&colors.md_heading.fg(&format!("##### {rest}")));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "####") {
                let h = bold(&colors.md_heading.fg(&format!("#### {rest}")));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "###") {
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "##") {
                let inner = bold(&underline(&self.render_inline(rest)));
                let h = colors.md_heading.fg(&format!("{}{}", bold("## "), inner));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#") {
                let inner = bold(&underline(&bold(&self.render_inline(rest))));
                let h = colors.md_heading.fg(&format!("{}{}", bold("# "), inner));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if is_task_list_item(line) {
                let (marker, body) = task_list_item(line).unwrap();
                let bullet = colors.md_list_bullet.fg(&format!("{marker}"));
                let rendered = format!("  {bullet} {}", self.render_inline(body));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with("- ") || line.starts_with("* ") {
                // Unordered list — accent-colored bullet.
                let bullet = colors.md_list_bullet.fg("•");
                let rendered = format!("  {bullet} {}", self.render_inline(line[2..].trim()));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(". ") {
                // Ordered list — accent-colored marker.
                let (num, rest) = split_ordered(line);
                let marker = colors.md_list_bullet.fg(&num);
                let rendered = format!("  {marker}{}", self.render_inline(rest));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with("> ") {
                // Blockquote — gray border + italic muted body (pi quote style).
                let border = colors.md_quote_border.fg("│");
                let body = colors.md_quote.fg(&italic(&self.render_inline(&line[2..])));
                let rendered = format!("  {border} {body}");
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.trim().starts_with("---") || line.trim().starts_with("***") {
                // Horizontal rule (capped like the TS reference), mdHr colored.
                let rule_w = width.min(80).saturating_sub(self.padding_x * 2);
                lines.push(format!(
                    "{pad}{}",
                    colors.md_hr.fg(&"─".repeat(rule_w.max(1)))
                ));
            } else if line.trim().is_empty() {
                // Empty line — emit a (padded) blank so spacing is preserved.
                lines.push(String::new());
            } else {
                // Regular text with inline formatting.
                push_wrapped(&pad, &self.render_inline(line), cwidth, &mut lines);
            }

            i += 1;
        }

        // Add bottom padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        if lines.is_empty() {
            lines.push(String::new());
        }

        lines
    }

    /// Render inline markdown (bold, italic, code, links).
    fn render_inline(&self, text: &str) -> String {
        let colors = theme().colors;
        let mut result = String::new();
        let mut chars = text.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '*' {
                if chars.peek() == Some(&'*') {
                    chars.next(); // consume second *
                                  // Bold
                    let bold_text = self.consume_until(&mut chars, "**");
                    result.push_str(&bold(&bold_text));
                } else {
                    // Italic
                    let italic_text = self.consume_until(&mut chars, "*");
                    result.push_str(&italic(&italic_text));
                }
            } else if c == '_' {
                if chars.peek() == Some(&'_') {
                    chars.next();
                    // CommonMark strong emphasis: `__x__` is bold.
                    let bold_text = self.consume_until(&mut chars, "__");
                    result.push_str(&bold(&bold_text));
                } else {
                    let italic_text = self.consume_until(&mut chars, "_");
                    result.push_str(&italic(&italic_text));
                }
            } else if c == '`' {
                // Inline code
                let code_text = self.consume_until(&mut chars, "`");
                result.push_str(&colors.md_code.fg(&code_text));
            } else if c == '[' {
                // Link
                let link_text = self.consume_until(&mut chars, "]");
                if chars.next() == Some('(') {
                    let url = self.consume_until(&mut chars, ")");
                    // pi renders the link text underlined + md_link colored; the
                    // URL is shown inline (dimmed) only when it differs from
                    // the visible text.
                    let styled = colors.md_link.fg(&underline(&link_text));
                    if link_text == url {
                        result.push_str(&styled);
                    } else {
                        result.push_str(&styled);
                        result.push_str(&colors.md_link_url.fg(&format!(" ({})", url)));
                    }
                } else {
                    result.push('[');
                    result.push_str(&link_text);
                }
            } else if c == '~' && chars.peek() == Some(&'~') {
                // Strikethrough (~~text~~) — pi's StrictStrikethrough.
                chars.next(); // consume second ~
                let del_text = self.consume_until(&mut chars, "~~");
                result.push_str(&"\x1b[9m"); // strikethrough on
                result.push_str(&del_text);
                result.push_str(&"\x1b[29m"); // strikethrough off
            } else {
                result.push(c);
            }
        }

        result
    }

    /// Consume characters until the delimiter.
    fn consume_until(
        &self,
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        delimiter: &str,
    ) -> String {
        let mut result = String::new();
        let delim_chars: Vec<char> = delimiter.chars().collect();

        while let Some(c) = chars.peek() {
            if *c == delim_chars[0] {
                // Check if this is the delimiter
                let mut matches = true;
                let mut lookahead: Vec<char> = Vec::new();

                for (_i, dc) in delim_chars.iter().enumerate() {
                    if let Some(&next) = chars.peek() {
                        if next == *dc {
                            lookahead.push(next);
                            chars.next();
                        } else {
                            matches = false;
                            break;
                        }
                    } else {
                        matches = false;
                        break;
                    }
                }

                if matches {
                    break;
                } else {
                    // Put back consumed characters
                    result.extend(lookahead);
                }
            } else {
                result.push(*c);
                chars.next();
            }
        }

        result
    }
}

/// If `line` is a ```` ``` ```` (or ```` ~~~ ````) fence, return the language
/// label (possibly empty). Returns `None` for non-fence lines.
fn fence_info(line: &str) -> Option<(char, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some(('`', trimmed[3..].trim().to_string()))
    } else if trimmed.starts_with("~~~") {
        Some(('~', trimmed[3..].trim().to_string()))
    } else {
        None
    }
}

/// Render one physical code line with stable chrome and ANSI-safe clipping.
fn push_code_line(line: &str, pad: &str, indent: &str, cwidth: usize, out: &mut Vec<String>) {
    let colors = theme().colors;
    let rule = colors.md_code_block_border.fg("│");
    let prefix = format!("{pad}{indent}{rule} ");
    // `cwidth` excludes Markdown padding, so only subtract the code indent and
    // panel chrome here (not the already-accounted-for `pad`).
    let chrome_width = visible_width(indent) + 2;
    let body_width = cwidth.saturating_sub(chrome_width).max(1);
    let expanded = line.replace('\t', "    ");
    let clipped = truncate_to_width(&expanded, body_width, "…");
    out.push(format!("{prefix}{}", colors.md_code_block.fg(&clipped)));
}

/// True when the whole line is a run of 1-2 fence chars (``/````/`~`/`~~`)
/// — a streamed, not-yet-complete closing fence. pi trims these (issue
/// #5825) so code blocks don't flicker/shrink while the final fence char
/// streams in; rendering them as text would also misrender the block tail.
fn is_partial_fence(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 2 {
        return false;
    }
    t.chars().all(|c| c == '`' || c == '~')
}

/// Split an ordered-list line into its marker (`"1. "` / `"12) "`) and body.
/// `line` must already match the ordered-list shape checked by the caller.
fn split_ordered(line: &str) -> (String, &str) {
    let mut idx = 0;
    let bytes = line.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    // Include the following `.`/`)` and one space in the marker.
    if idx < bytes.len() && (bytes[idx] == b'.' || bytes[idx] == b')') {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }
    (line[..idx].to_string(), &line[idx..])
}

/// Strip a leading `prefix` of `#`s followed by a space from `line`.
fn strip_header<'a>(line: &'a str, hashes: &str) -> Option<&'a str> {
    if line.starts_with(hashes) {
        let rest = &line[hashes.len()..];
        if let Some(stripped) = rest.strip_prefix(' ') {
            return Some(stripped);
        }
    }
    None
}

/// True if the line looks like a markdown table row (`| … | … |`).
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() > 1
}

/// True if the line is a table separator (`|---|---|`, `:--:` allowed).
fn is_table_separator(maybe: Option<&&str>) -> bool {
    match maybe {
        Some(line) => {
            let t = line.trim();
            if !t.contains('|') {
                return false;
            }
            let cell = |s: &str| {
                s.chars()
                    .all(|c| c == '-' || c == ':' || c == ' ' || c == '\t')
                    && s.contains('-')
            };
            let inner = t.trim_matches('|');
            inner.split('|').all(cell)
        }
        None => false,
    }
}

/// Parse a table into rendered lines with box-drawing borders.
fn render_table(rows: &[&str], pad: &str, cwidth: usize, out: &mut Vec<String>) {
    // Split each row into trimmed cells.
    let parse = |row: &str| -> Vec<String> {
        row.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    };
    if rows.len() < 2 {
        return;
    }
    let header = parse(rows[0]);
    let _separator = parse(rows[1]);
    let body: Vec<Vec<String>> = rows[2..].iter().map(|r| parse(r)).collect();
    let ncols = header.len();
    if ncols == 0 {
        return;
    }

    // Compute column visible widths from header + body.
    let mut col_w: Vec<usize> = (0..ncols)
        .map(|c| {
            std::iter::once(&header)
                .chain(body.iter())
                .map(|r| r.get(c).map(|s| visible_width(s)).unwrap_or(0))
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Account for " | " separators + leading/trailing border usage.
    // We cap the total so the table fits within `cwidth`.
    let overhead = 3 * (ncols - 1) + 4; // approximate: "| " + " | "*n + " |"
    let budget = cwidth.saturating_sub(overhead);
    let total: usize = col_w.iter().sum();
    if total > budget {
        // Shrink proportionally (floor of 1 per column).
        let scale = budget as f64 / total as f64;
        for w in col_w.iter_mut() {
            *w = (*w as f64 * scale).round() as usize;
            if *w < 1 {
                *w = 1;
            }
        }
    }

    let join = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = col_w.get(i).copied().unwrap_or(0);
                let cell = truncate_to_width(c, w, "…");
                pad_cell(&cell, w)
            })
            .collect::<Vec<_>>()
            .join("│")
    };

    let border = |left: &str, mid: &str, joiner: &str, right: &str| -> String {
        let segs: Vec<String> = col_w.iter().map(|w| mid.repeat(*w)).collect();
        format!("{left}{}{right}", segs.join(joiner))
    };

    out.push(format!("{pad}{}", border("┌", "─", "┬", "┐")));
    out.push(format!("{pad}│{}│", join(&header)));
    out.push(format!("{pad}{}", border("├", "─", "┼", "┤")));
    for row in &body {
        let cells: Vec<String> = (0..ncols)
            .map(|c| row.get(c).cloned().unwrap_or_default())
            .collect();
        out.push(format!("{pad}│{}│", join(&cells)));
    }
    out.push(format!("{pad}{}", border("└", "─", "┴", "┘")));
}

/// Left-pad/content a cell string to `w` columns (ANSI-aware).
fn pad_cell(cell: &str, w: usize) -> String {
    let vw = visible_width(cell);
    if vw >= w {
        cell.to_string()
    } else {
        format!("{cell}{}", " ".repeat(w - vw))
    }
}

/// Is this a task-list item (`- [ ]` / `- [x]` / `- [X]`)?
fn is_task_list_item(line: &str) -> bool {
    task_list_item(line).is_some()
}

/// Parse a task-list item into (marker, body).
fn task_list_item(line: &str) -> Option<(char, &str)> {
    let t = line.trim_start();
    let after = t.get(2..)?;
    if let Some(body) = after.strip_prefix("[ ]") {
        return Some(('☐', body.trim_start()));
    }
    if let Some(body) = after.strip_prefix("[x]") {
        return Some(('☒', body.trim_start()));
    }
    if let Some(body) = after.strip_prefix("[X]") {
        return Some(('☒', body.trim_start()));
    }
    None
}

/// Wrap `rendered` (an already-styled line possibly with leading pad) into the
/// available width and push each sub-line into `out`.
fn push_wrapped(pad: &str, rendered: &str, cwidth: usize, out: &mut Vec<String>) {
    if cwidth == 0 {
        out.push(rendered.to_string());
        return;
    }
    // Padding is layout chrome, not part of `rendered`; applying it here keeps
    // first and continuation lines aligned (the old renderer omitted padding
    // from the first line of ordinary paragraphs and headings).
    let wrapped = wrap_text_with_ansi(rendered, cwidth);
    if wrapped.is_empty() {
        out.push(String::new());
        return;
    }
    for sub in wrapped {
        out.push(format!("{pad}{sub}"));
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_markdown(width)
    }

    fn invalidate(&self) {
        // Markdown caches content, no external cache to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_headers() {
        let md = Markdown::new("# Title\n## Subtitle\n### Heading", 0, 0);
        let lines = md.render(80);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_markdown_inline() {
        let md = Markdown::new("This is **bold** and *italic* text", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
    }

    #[test]
    fn test_markdown_code() {
        let md = Markdown::new("Use `code` here", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
    }

    #[test]
    fn test_markdown_no_panic_on_wide_cjk() {
        // A CJK + bold line at a narrow width used to panic via the byte-slice
        // truncation in the old renderer (markdown.rs:136).
        let md = Markdown::new("**你好世界**这是一段很长的中文内容需要自动换行", 1, 0);
        let lines = md.render(10);
        // Must not panic and must produce more than one wrapped line.
        assert!(lines.len() > 1);
    }

    #[test]
    fn test_markdown_wraps_long_line() {
        let long = "a".repeat(100);
        let md = Markdown::new(long, 0, 0);
        let lines = md.render(20);
        assert!(lines.len() > 1, "a 100-col line at width 20 should wrap");
    }

    #[test]
    fn test_markdown_code_block_has_closing_border() {
        let md = Markdown::new("```rust\nlet x = 1;\n```", 0, 0);
        let joined = md.render(80).join("\n");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(
            plain.contains("╭─ rust"),
            "missing language header: {plain}"
        );
        assert!(plain.contains("│ let x = 1;"), "missing code body: {plain}");
        assert!(plain.contains("╰─"), "missing closing border: {plain}");
        assert!(
            !plain.contains("```"),
            "literal fences should be hidden: {plain}"
        );
    }

    #[test]
    fn test_markdown_padding_applies_to_first_and_wrapped_lines() {
        let md = Markdown::new("abcdefgh", 2, 0);
        let lines = md.render(6);
        assert_eq!(lines, vec!["  ab", "  cd", "  ef", "  gh"]);
    }

    #[test]
    fn test_markdown_code_expands_tabs_and_clips() {
        let md = Markdown::new("```\n\tlet value = 123456;\n```", 0, 0);
        let lines = md.render(14);
        let plain: Vec<String> = lines
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();
        assert!(
            plain[1].starts_with("  │     let"),
            "tab/indent lost: {plain:?}"
        );
        assert!(
            plain[1].contains('…'),
            "long code line should clip: {plain:?}"
        );
        assert!(plain.iter().all(|line| visible_width(line) <= 14));
    }

    #[test]
    fn test_markdown_task_list_checkbox() {
        let md = Markdown::new("- [x] done\n- [ ] todo", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains('☒'), "checked marker missing");
        assert!(joined.contains('☐'), "unchecked marker missing");
    }

    #[test]
    fn test_markdown_table_renders_borders() {
        let md = Markdown::new("| a | b |\n|---|---|\n| 1 | 2 |", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains('┌'), "missing top-left");
        assert!(joined.contains('┬'), "missing top-mid");
        assert!(joined.contains('┐'), "missing top-right");
        assert!(joined.contains('└'), "missing bottom-left");
    }

    #[test]
    fn test_markdown_double_underscore_is_bold() {
        let md = Markdown::new("__strong__", 0, 0);
        let joined = md.render(80).join("\n");
        // `\x1b[1m` is the bold SGR; `\x1b[4m` is underline.
        assert!(joined.contains("\x1b[1m"), "`__` should render bold");
        assert!(
            !joined.contains("\x1b[4m"),
            "`__` should NOT render underline"
        );
    }

    #[test]
    fn test_markdown_hr_capped() {
        let md = Markdown::new("---", 0, 0);
        let lines = md.render(200);
        // HR is capped at 80 cols, not the full 200.
        assert_eq!(visible_width(&lines[0]), 80);
    }

    #[test]
    fn test_markdown_h4_does_not_fall_through() {
        let md = Markdown::new("#### nested", 0, 0);
        let joined = md.render(80).join("\n");
        assert!(joined.contains("\x1b[1m"), "h4 should be bold");
    }
}
