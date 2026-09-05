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
use crate::utils::{
    apply_background_to_line, truncate_to_width, visible_width, wrap_text_with_ansi,
};

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
        // Markdown allows a list item's paragraph to continue on an indented
        // line. Keep the visual text offset so continuation lines don't jump
        // back to the left edge of the transcript.
        let mut list_continuation_indent: Option<usize> = None;

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
                list_continuation_indent = None;
                // Inside a block, only the matching fence closes it. This keeps
                // backticks embedded in a ~~~ block from collapsing the panel.
                if !in_code_block {
                    in_code_block = true;
                    code_fence_char = fence_char;
                    // No decorative frame: a small language label is enough
                    // chrome. For an unlabelled block, start directly with code.
                    if !lang.is_empty() {
                        let panel_width = cwidth.saturating_sub(visible_width(&indent)).max(1);
                        let header = colors.md_code_block_border.fg(&format!(" {lang}"));
                        let header = apply_background_to_line(&header, panel_width, |text| {
                            colors.md_code_block_bg.bg(text)
                        });
                        lines.push(format!("{pad}{indent}{header}"));
                    }
                } else if fence_char == code_fence_char {
                    in_code_block = false;
                    // The closing fence is structural only; do not draw a
                    // bottom border around the code surface.
                } else {
                    // A non-matching fence is code content.
                    push_code_line(line, &pad, &indent, cwidth, &mut lines);
                }
                i += 1;
                continue;
            }

            if in_code_block {
                list_continuation_indent = None;
                // Code is never reflowed: wrapping destroys indentation and
                // makes copied snippets invalid. Tabs are normalized and long
                // physical lines are clipped with an ellipsis.
                push_code_line(line, &pad, &indent, cwidth, &mut lines);
                i += 1;
                continue;
            }

            // ---- Tables ----
            if is_table_separator(raw_lines.get(i + 1)) && is_table_row(line) {
                list_continuation_indent = None;
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
                render_table(&table_lines, &pad, cwidth, &mut lines, |cell| {
                    self.render_inline(cell)
                });
                continue;
            }

            // ---- Headers ----
            // pi colors the whole heading (prefix + text) with mdHeading and
            // bolds it; h1/h2 are additionally underlined.
            if let Some(rest) = strip_header(line, "######") {
                list_continuation_indent = None;
                let h = bold(
                    &colors
                        .md_heading
                        .fg(&format!("###### {}", self.render_inline(rest))),
                );
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#####") {
                list_continuation_indent = None;
                let h = bold(
                    &colors
                        .md_heading
                        .fg(&format!("##### {}", self.render_inline(rest))),
                );
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "####") {
                list_continuation_indent = None;
                let h = bold(
                    &colors
                        .md_heading
                        .fg(&format!("#### {}", self.render_inline(rest))),
                );
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "###") {
                list_continuation_indent = None;
                let h = bold(&colors.md_heading.fg(&self.render_inline(rest)));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "##") {
                list_continuation_indent = None;
                let inner = bold(&underline(&self.render_inline(rest)));
                let h = colors.md_heading.fg(&format!("{}{}", bold("## "), inner));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(rest) = strip_header(line, "#") {
                list_continuation_indent = None;
                let inner = bold(&underline(&bold(&self.render_inline(rest))));
                let h = colors.md_heading.fg(&format!("{}{}", bold("# "), inner));
                push_wrapped(&pad, &h, cwidth, &mut lines);
            } else if let Some(item) = parse_list_item(line) {
                let marker = match item.marker {
                    ListMarker::Bullet => colors.md_list_bullet.fg("•"),
                    ListMarker::Ordered(marker) => colors.md_list_bullet.fg(&marker),
                    ListMarker::Task(marker) => colors.md_list_bullet.fg(&marker.to_string()),
                };
                let prefix = format!("{}{} ", "  ".repeat(item.depth + 1), marker);
                list_continuation_indent = Some(visible_width(&prefix));
                push_wrapped_with_prefix(
                    &pad,
                    &prefix,
                    &self.render_inline(item.body),
                    cwidth,
                    &mut lines,
                );
            } else if let Some((depth, body)) = parse_blockquote(line) {
                list_continuation_indent = None;
                let border = colors.md_quote_border.fg("│");
                let prefix = format!("  {}", format!("{border} ").repeat(depth));
                let body = colors.md_quote.fg(&italic(&self.render_inline(body)));
                push_wrapped_with_prefix(&pad, &prefix, &body, cwidth, &mut lines);
            } else if line.trim().starts_with("---") || line.trim().starts_with("***") {
                list_continuation_indent = None;
                // Horizontal rule (capped like the TS reference), mdHr colored.
                let rule_w = width.min(80).saturating_sub(self.padding_x * 2);
                lines.push(format!(
                    "{pad}{}",
                    colors.md_hr.fg(&"─".repeat(rule_w.max(1)))
                ));
            } else if line.trim().is_empty() {
                list_continuation_indent = None;
                // Empty line — emit a (padded) blank so spacing is preserved.
                lines.push(String::new());
            } else if let Some(indent) =
                list_continuation_indent.filter(|_| line.starts_with(' ') || line.starts_with('\t'))
            {
                push_wrapped_with_prefix(
                    &pad,
                    &" ".repeat(indent),
                    &self.render_inline(line.trim()),
                    cwidth,
                    &mut lines,
                );
            } else {
                list_continuation_indent = None;
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
        let mut offset = 0;

        while offset < text.len() {
            let rest = &text[offset..];

            if let Some(escaped) = rest.strip_prefix('\\') {
                if let Some(ch) = escaped.chars().next() {
                    result.push(ch);
                    offset += 1 + ch.len_utf8();
                } else {
                    result.push('\\');
                    offset += 1;
                }
                continue;
            }

            if rest.starts_with('`') {
                let ticks = rest.chars().take_while(|c| *c == '`').count();
                let delimiter = "`".repeat(ticks);
                if let Some(end) = rest[ticks..].find(&delimiter) {
                    let code = &rest[ticks..ticks + end];
                    let chip = format!(" {} ", colors.md_code.fg(code.trim()));
                    result.push_str(&colors.md_code_bg.bg(&chip));
                    offset += ticks + end + ticks;
                    continue;
                }
            }

            if rest.starts_with('[') {
                if let Some(label_end) = rest.find("](") {
                    let url_start = label_end + 2;
                    if let Some(url_end) = matching_paren(&rest[url_start..]) {
                        let label = &rest[1..label_end];
                        let url = &rest[url_start..url_start + url_end];
                        let styled = colors.md_link.fg(&underline(&self.render_inline(label)));
                        result.push_str(&styled);
                        if label != url {
                            result.push_str(&colors.md_link_url.fg(&format!(" ({url})")));
                        }
                        offset += url_start + url_end + 1;
                        continue;
                    }
                }
            }

            if let Some((delimiter, style)) = emphasis_delimiter(rest) {
                if let Some(end) = rest[delimiter.len()..].find(delimiter) {
                    let inner = &rest[delimiter.len()..delimiter.len() + end];
                    if !inner.is_empty() {
                        let rendered = self.render_inline(inner);
                        match style {
                            InlineStyle::Bold => result.push_str(&bold(&rendered)),
                            InlineStyle::Italic => result.push_str(&italic(&rendered)),
                            InlineStyle::Strike => {
                                result.push_str("\x1b[9m");
                                result.push_str(&rendered);
                                result.push_str("\x1b[29m");
                            }
                        }
                        offset += delimiter.len() + end + delimiter.len();
                        continue;
                    }
                }
            }

            let ch = rest.chars().next().expect("offset is a character boundary");
            result.push(ch);
            offset += ch.len_utf8();
        }

        result
    }
}

#[derive(Debug, Clone, Copy)]
enum InlineStyle {
    Bold,
    Italic,
    Strike,
}

/// Return an emphasis delimiter only when it can begin a formatting span.
/// This intentionally leaves identifiers such as `snake_case` untouched.
fn emphasis_delimiter(rest: &str) -> Option<(&'static str, InlineStyle)> {
    if rest.starts_with("**") {
        Some(("**", InlineStyle::Bold))
    } else if rest.starts_with("__") {
        Some(("__", InlineStyle::Bold))
    } else if rest.starts_with("~~") {
        Some(("~~", InlineStyle::Strike))
    } else if rest.starts_with('*') {
        Some(("*", InlineStyle::Italic))
    } else if rest.starts_with('_') {
        let next = rest[1..].chars().next();
        if next.is_some_and(|ch| !ch.is_whitespace()) {
            Some(("_", InlineStyle::Italic))
        } else {
            None
        }
    } else {
        None
    }
}

/// Return the byte offset of the close paren matching the opening paren that
/// follows a Markdown link label. Parentheses in URLs are common in docs.
fn matching_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (offset, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
    }
    None
}

enum ListMarker {
    Bullet,
    Ordered(String),
    Task(char),
}

struct ListItem<'a> {
    depth: usize,
    marker: ListMarker,
    body: &'a str,
}

/// Parse a Markdown list item, including indented/nested lists. The parser is
/// deliberately strict about the marker's following whitespace so ordinary
/// numeric text without a list marker is not rendered as a list.
fn parse_list_item(line: &str) -> Option<ListItem<'_>> {
    let leading = line.len() - line.trim_start_matches([' ', '\t']).len();
    let content = &line[leading..];
    let depth = line[..leading]
        .chars()
        .fold(0usize, |width, ch| width + if ch == '\t' { 4 } else { 1 })
        / 2;

    let (marker, body) = if let Some(rest) = content
        .strip_prefix("- ")
        .or_else(|| content.strip_prefix("* "))
        .or_else(|| content.strip_prefix("+ "))
    {
        if let Some(task_body) = rest.strip_prefix("[ ]") {
            (ListMarker::Task('☐'), task_body.trim_start())
        } else if let Some(task_body) = rest
            .strip_prefix("[x]")
            .or_else(|| rest.strip_prefix("[X]"))
        {
            (ListMarker::Task('☒'), task_body.trim_start())
        } else {
            (ListMarker::Bullet, rest)
        }
    } else {
        let marker_end = content
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if marker_end == 0 {
            return None;
        }
        let marker_char = content.as_bytes().get(marker_end)?;
        if !matches!(marker_char, b'.' | b')')
            || !content
                .as_bytes()
                .get(marker_end + 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            return None;
        }
        (
            ListMarker::Ordered(content[..marker_end + 1].to_string()),
            content[marker_end + 2..].trim_start(),
        )
    };

    Some(ListItem {
        depth,
        marker,
        body,
    })
}

/// Parse one or more quote markers, accepting `>quote`, `> quote`, and nested
/// `> > quote` forms used by CommonMark renderers.
fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut body = line.trim_start_matches([' ', '\t']);
    let mut depth = 0;
    while let Some(rest) = body.strip_prefix('>') {
        depth += 1;
        body = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (depth > 0).then_some((depth, body))
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

/// Render one physical code line on a subtle surface, without an outer frame.
fn push_code_line(line: &str, pad: &str, indent: &str, cwidth: usize, out: &mut Vec<String>) {
    let colors = theme().colors;
    // `cwidth` excludes Markdown padding. The code surface starts after the
    // configured indent and fills the remaining width, including blank rows.
    let panel_width = cwidth.saturating_sub(visible_width(indent)).max(1);
    let body_width = panel_width.saturating_sub(2).max(1);
    let expanded = line.replace('\t', "    ");
    let clipped = truncate_to_width(&expanded, body_width, "…");
    let body = format!(" {}", colors.md_code_block.fg(&clipped));
    let body =
        apply_background_to_line(&body, panel_width, |text| colors.md_code_block_bg.bg(text));
    out.push(format!("{pad}{indent}{body}"));
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
fn render_table<F>(rows: &[&str], pad: &str, cwidth: usize, out: &mut Vec<String>, render_inline: F)
where
    F: Fn(&str) -> String,
{
    // Split each row into trimmed cells.
    let parse = |row: &str| -> Vec<String> {
        split_table_cells(row)
            .into_iter()
            .map(|cell| render_inline(cell.trim()))
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

/// Split a table row without treating escaped pipes or pipes inside code spans
/// as column separators.
fn split_table_cells(row: &str) -> Vec<String> {
    let inner = row.trim().trim_matches('|');
    let mut cells = vec![String::new()];
    let mut escaped = false;
    let mut code_ticks = 0usize;

    for ch in inner.chars() {
        if escaped {
            cells.last_mut().expect("table has one cell").push(ch);
            escaped = false;
        } else if ch == '\\' {
            cells.last_mut().expect("table has one cell").push(ch);
            escaped = true;
        } else if ch == '`' {
            code_ticks ^= 1;
            cells.last_mut().expect("table has one cell").push(ch);
        } else if ch == '|' && code_ticks == 0 {
            cells.push(String::new());
        } else {
            cells.last_mut().expect("table has one cell").push(ch);
        }
    }
    cells
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

/// Wrap text after a semantic prefix (list bullet, quote border, etc.). The
/// first line gets the prefix; every continuation starts under the text rather
/// than under the marker, which makes long assistant responses much easier to
/// scan in a narrow terminal.
fn push_wrapped_with_prefix(
    pad: &str,
    prefix: &str,
    rendered: &str,
    cwidth: usize,
    out: &mut Vec<String>,
) {
    let prefix_width = visible_width(prefix);
    let body_width = cwidth.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_text_with_ansi(rendered, body_width);
    if wrapped.is_empty() {
        out.push(format!("{pad}{prefix}"));
        return;
    }
    let continuation = " ".repeat(prefix_width);
    for (index, line) in wrapped.iter().enumerate() {
        let line_prefix = if index == 0 { prefix } else { &continuation };
        // `wrap_text_with_ansi` preserves the separating space at a wrap
        // boundary. It is useful for paragraphs, but here it would make a
        // continuation one column farther right than the list's text.
        let body = if index == 0 {
            line.as_str()
        } else {
            line.trim_start()
        };
        out.push(format!("{pad}{line_prefix}{body}"));
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
        assert!(
            lines[0].contains("\x1b[48;2;48;52;59m"),
            "inline code should use the theme chip background"
        );
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
    fn test_markdown_code_block_has_background_without_frame() {
        let md = Markdown::new("```rust\nlet x = 1;\n```", 0, 0);
        let joined = md.render(80).join("\n");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(plain.contains(" rust"), "missing language label: {plain}");
        assert!(plain.contains(" let x = 1;"), "missing code body: {plain}");
        assert!(
            !plain.contains(['╭', '╰', '│']),
            "code block must not draw an outer frame: {plain}"
        );
        assert!(
            joined.contains("\x1b[48;2;40;44;52m"),
            "code panel background missing: {joined:?}"
        );
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
            plain[0].starts_with("       let"),
            "tab/indent lost: {plain:?}"
        );
        assert!(
            plain[0].contains('…'),
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

    #[test]
    fn test_markdown_nested_lists_keep_their_text_indent_when_wrapped() {
        let md = Markdown::new(
            "- parent item that wraps over several terminal columns\n  - child item\n    child continuation",
            0,
            0,
        );
        let plain: Vec<String> = md
            .render(20)
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();

        assert!(
            plain[0].starts_with("  • "),
            "root marker missing: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("    wraps")),
            "wrapped root text should align after its marker: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("    • child")),
            "nested marker missing: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.starts_with("      child")),
            "continuation should align after nested marker: {plain:?}"
        );
    }

    #[test]
    fn test_markdown_nested_blockquote_and_unclosed_inline_are_literal() {
        let md = Markdown::new(
            "> > nested quote\ntext with *an unfinished emphasis and `unfinished code",
            0,
            0,
        );
        let plain: Vec<String> = md
            .render(80)
            .iter()
            .map(|line| crate::ansi::strip_ansi(line))
            .collect();
        assert!(plain[0].starts_with("  │ │ nested quote"), "{plain:?}");
        assert_eq!(
            plain[1],
            "text with *an unfinished emphasis and `unfinished code"
        );
    }

    #[test]
    fn test_markdown_table_preserves_pipes_in_inline_code() {
        let md = Markdown::new("| Name | Value |\n|---|---|\n| **bold** | `a|b` |", 0, 0);
        let plain = crate::ansi::strip_ansi(&md.render(80).join("\n"));
        assert_eq!(
            plain.matches('┬').count(),
            1,
            "table columns split incorrectly: {plain}"
        );
        assert!(plain.contains("bold"), "inline formatting lost: {plain}");
        assert!(
            plain.contains("a|b"),
            "code pipe split a table cell: {plain}"
        );
    }
}
