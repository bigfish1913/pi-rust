//! Markdown component for rendering markdown text.
//!
//! Provides ANSI-aware markdown rendering that wraps to the available width
//! without panicking on multibyte (CJK/emoji/box-drawing) characters or
//! leaking escape sequences. Reuses the shared utilities in [`crate::utils`].

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::ansi::{bold, dim, italic, underline, fg_256};
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
    pub fn with_options(content: impl Into<String>, options: MarkdownOptions, padding_x: usize, padding_y: usize) -> Self {
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

        // Code-fence state
        let mut in_code_block = false;
        while i < raw_lines.len() {
            let line = raw_lines[i];

            // ---- Code fences ----
            let fence = fence_info(line);
            if let Some(lang) = fence {
                in_code_block = !in_code_block;
                if in_code_block {
                    // Opening border with language label.
                    let label = if lang.is_empty() {
                        String::new()
                    } else {
                        format!(" {lang}")
                    };
                    lines.push(format!(
                        "{pad}{indent}{}",
                        dim(&format!("┌─{label}"))
                    ));
                } else {
                    // Closing border.
                    lines.push(format!("{pad}{indent}{}", dim("└─")));
                }
                i += 1;
                continue;
            }

            if in_code_block {
                // Code body: no reflow; clip long lines ANSI-safely with a `…`.
                let prefix = format!("{pad}{indent}│ ");
                let body_width = cwidth.saturating_sub(visible_width(&prefix));
                let clipped = truncate_to_width(line, body_width.max(1), "…");
                lines.push(format!("{prefix}{clipped}"));
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
            if let Some(rest) = strip_header(line, "######") {
                push_wrapped(&pad, &bold(&format!("###### {rest}")), cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#####") {
                push_wrapped(&pad, &bold(&format!("##### {rest}")), cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "####") {
                push_wrapped(&pad, &bold(&format!("#### {rest}")), cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "###") {
                push_wrapped(&pad, &bold(&self.render_inline(rest)), cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "##") {
                let h = bold(&underline(&self.render_inline(rest)));
                let rendered = format!("{}{}", bold(&dim("## ")), h);
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#") {
                let h = bold(&underline(&bold(&self.render_inline(rest))));
                let rendered = format!("{}{}", bold(&dim("# ")), h);
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if is_task_list_item(line) {
                let (marker, body) = task_list_item(line).unwrap();
                let rendered = format!("{pad}  {marker} {}", self.render_inline(body));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with("- ") || line.starts_with("* ") {
                // Unordered list
                let rendered = format!("{pad}  • {}", self.render_inline(line[2..].trim()));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(". ") {
                // Ordered list
                let rendered = format!("{pad}  {}", line);
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.starts_with("> ") {
                // Blockquote
                let rendered = format!("{}  {} {}", pad, dim("│"), italic(&self.render_inline(&line[2..])));
                push_wrapped(&pad, &rendered, cwidth, &mut lines);
            } else if line.trim().starts_with("---") || line.trim().starts_with("***") {
                // Horizontal rule (capped like the TS reference).
                let rule_w = width.min(80).saturating_sub(self.padding_x * 2);
                lines.push(format!("{pad}{}", dim(&"─".repeat(rule_w.max(1)))));
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
                result.push_str(&fg_256(14, &code_text)); // Cyan
            } else if c == '[' {
                // Link
                let link_text = self.consume_until(&mut chars, "]");
                if chars.next() == Some('(') {
                    let _url = self.consume_until(&mut chars, ")");
                    result.push_str(&underline(&link_text));
                } else {
                    result.push('[');
                    result.push_str(&link_text);
                }
            } else {
                result.push(c);
            }
        }

        result
    }

    /// Consume characters until the delimiter.
    fn consume_until(&self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>, delimiter: &str) -> String {
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
fn fence_info(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some(trimmed[3..].trim().to_string())
    } else if trimmed.starts_with("~~~") {
        Some(trimmed[3..].trim().to_string())
    } else {
        None
    }
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
    // The first line already carries the left pad; wrapped continuation
    // lines must re-apply it.
    let wrapped = wrap_text_with_ansi(rendered, cwidth);
    if wrapped.is_empty() {
        out.push(String::new());
        return;
    }
    for (idx, sub) in wrapped.iter().enumerate() {
        if idx == 0 {
            out.push(sub.clone());
        } else {
            out.push(format!("{pad}{sub}"));
        }
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
        assert!(joined.contains('┌'), "missing opening border");
        assert!(joined.contains('└'), "missing closing border");
        assert!(joined.contains("rust"), "missing language label");
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
        assert!(!joined.contains("\x1b[4m"), "`__` should NOT render underline");
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
