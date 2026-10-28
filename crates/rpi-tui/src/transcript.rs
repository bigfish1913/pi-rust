//! Generic transcript *style packs*.
//!
//! A style pack is host-side data (JSON) that a plugin discovers, the same way
//! plugins discover skills, prompts and themes. It changes **how** the native
//! transcript is drawn without the host knowing anything about the pack's
//! author:
//!
//! - fold one assistant segment's tool panels into a single summary row
//! - draw a rounded, transparent frame around tool / message boxes
//! - put an author-supplied glyph and colour on tool rows and file paths
//! - colour shell commands with an author-supplied token palette
//! - render edit diffs as two panes
//! - cap collapsed preview rows
//!
//! Nothing here is specific to any single pack: with no pack loaded the host
//! keeps its stock transcript, and the defaults below are the stock behavior.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::ansi::visible_width;
use crate::component::Component;
use crate::theme::{theme, Color, Theme, ThemeColors};
use crate::tool_execution::ToolStatus;
use crate::utils::truncate_to_width;

// ---------------------------------------------------------------------------
// Style pack
// ---------------------------------------------------------------------------

/// The active style pack, resolved once when the session's extensions load.
static ACTIVE_STYLE: std::sync::OnceLock<std::sync::RwLock<Option<Arc<TranscriptStyle>>>> =
    std::sync::OnceLock::new();

fn active_cell() -> &'static std::sync::RwLock<Option<Arc<TranscriptStyle>>> {
    ACTIVE_STYLE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Height of the transcript viewport, in rows. Expansion uses it as the
/// per-step budget so one click can never dump more than a screenful.
static WINDOW_ROWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(20);

/// Current expansion window (one screenful by default).
pub fn window_rows() -> usize {
    WINDOW_ROWS
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(4)
}

/// Publish the transcript viewport height. Called by the renderer each frame.
pub fn set_viewport_height(rows: usize) {
    if rows > 0 {
        WINDOW_ROWS.store(rows, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Install (or clear) the process-wide style pack.
///
/// A host has exactly one terminal, so the style is process-wide rather than
/// per-component: the plugin loader installs it after `declare`, and every
/// transcript component picks it up on its next paint.
pub fn set_active_style(style: Option<Arc<TranscriptStyle>>) {
    if let Ok(mut cell) = active_cell().write() {
        *cell = style;
    }
    // Panels memoize their rendered rows; a pack swap must invalidate them or a
    // completed card keeps the previous frame/glyph/budget look.
    STYLE_REVISION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Bumped whenever the active pack changes; part of every render cache key.
pub fn style_revision() -> u64 {
    STYLE_REVISION.load(std::sync::atomic::Ordering::Acquire)
}

static STYLE_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The active style pack, if a plugin declared one.
pub fn active_style() -> Option<Arc<TranscriptStyle>> {
    active_cell().read().ok().and_then(|cell| cell.clone())
}

/// Resolve the style a component should draw with: an explicit override wins,
/// otherwise the process-wide pack.
pub fn style_or_active(explicit: &Option<Arc<TranscriptStyle>>) -> Option<Arc<TranscriptStyle>> {
    explicit.clone().or_else(active_style)
}

/// One glyph, optionally with its own colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub glyph: String,
    pub color: Option<Color>,
}

fn parse_color(spec: &str) -> Option<Color> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Some(index) = spec.strip_prefix("ansi256:") {
        return index.parse::<u8>().ok().map(Color::Ansi256);
    }
    if let Some(index) = spec.strip_prefix("ansi:") {
        return index.parse::<u8>().ok().map(Color::Ansi16);
    }
    None
}

fn parse_icon(value: &Value) -> Option<Icon> {
    match value {
        Value::String(glyph) => Some(Icon {
            glyph: glyph.clone(),
            color: None,
        }),
        Value::Object(map) => {
            let glyph = map.get("icon").and_then(Value::as_str)?.to_string();
            let color = map
                .get("color")
                .and_then(Value::as_str)
                .and_then(parse_color);
            Some(Icon { glyph, color })
        }
        _ => None,
    }
}

fn icon_map(value: Option<&Value>) -> HashMap<String, Icon> {
    let Some(map) = value.and_then(Value::as_object) else {
        return HashMap::new();
    };
    map.iter()
        .filter_map(|(key, value)| parse_icon(value).map(|icon| (key.clone(), icon)))
        .collect()
}

fn string_map(value: Option<&Value>) -> HashMap<String, String> {
    let Some(map) = value.and_then(Value::as_object) else {
        return HashMap::new();
    };
    map.iter()
        .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
        .collect()
}

fn flag(value: &Value, key: &str, fallback: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

fn number(value: &Value, key: &str) -> Option<usize> {
    value.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

/// A parsed style pack.
#[derive(Debug, Clone)]
pub struct TranscriptStyle {
    /// Author-chosen pack name (diagnostics only).
    pub name: String,
    /// Fold a segment's tool panels into one summary row.
    pub group_tools: bool,
    /// Tools that keep their own box and end the current group.
    pub always_show: Vec<String>,
    pub frame_tools: bool,
    /// Draw the same rounded frame around assistant message bubbles. User
    /// messages already render a bordered bubble in stock rpi, so there is no
    /// `frameUserMessages` switch here; a pack that sends one is ignored on
    /// purpose rather than drawing a box inside a box.
    pub frame_messages: bool,
    /// Use the pack's glyphs.
    pub icons: bool,
    /// Colour shell commands with `token_colors`.
    pub command_highlight: bool,
    /// Collapsed preview budgets (`None` keeps the host default).
    pub bash_result_lines: Option<usize>,
    pub write_lines: Option<usize>,
    pub diff_lines: Option<usize>,
    pub split_diff: bool,
    pub split_diff_min_width: usize,
    /// pi-style colour *names* per token/slot; resolved against the theme first
    /// and `#rrggbb` / `ansi:N` literals second.
    pub palette: HashMap<String, String>,
    pub tool_colors: HashMap<String, String>,
    pub message_colors: HashMap<String, String>,
    pub tool_icons: HashMap<String, Icon>,
    pub file_ext_icons: HashMap<String, Icon>,
    pub file_name_icons: HashMap<String, Icon>,
    pub message_icons: HashMap<String, Icon>,
    /// Shell token → colour name (`syntaxKeyword`, `accent`, `#ff8800`, …).
    pub token_colors: HashMap<String, String>,
    pub thinking_color: String,
}

impl Default for TranscriptStyle {
    /// The stock look: no folding, no frames, no glyphs. A pack opts in.
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            group_tools: false,
            always_show: Vec::new(),
            frame_tools: false,
            frame_messages: false,
            icons: false,
            command_highlight: false,
            bash_result_lines: None,
            write_lines: None,
            diff_lines: None,
            split_diff: false,
            split_diff_min_width: 72,
            palette: HashMap::new(),
            tool_colors: HashMap::new(),
            message_colors: HashMap::new(),
            tool_icons: HashMap::new(),
            file_ext_icons: HashMap::new(),
            file_name_icons: HashMap::new(),
            message_icons: HashMap::new(),
            token_colors: HashMap::new(),
            thinking_color: "warning".to_string(),
        }
    }
}

impl TranscriptStyle {
    /// Parse one pack. `None` means "this pack is disabled".
    pub fn from_value(value: &Value) -> Option<Self> {
        if !flag(value, "enabled", true) {
            return None;
        }
        let base = Self::default();
        let always_show = value
            .get("alwaysShowTools")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or(base.always_show);
        let group = value
            .get("group")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| value.clone());
        let frame = value
            .get("frame")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| value.clone());
        let icons = value
            .get("icons")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(Value::Null);
        let budgets = value
            .get("budgets")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| value.clone());
        let diff = value
            .get("diff")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| value.clone());
        let command = value
            .get("command")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(Value::Null);

        Some(Self {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("style")
                .to_string(),
            group_tools: flag(&group, "groupTools", base.group_tools),
            always_show,
            frame_tools: flag(&frame, "frameTools", flag(&frame, "tools", false)),
            frame_messages: flag(&frame, "frameMessages", flag(&frame, "messages", false)),
            icons: flag(&icons, "enabled", icons.is_object()),
            command_highlight: flag(&command, "highlight", false),
            bash_result_lines: number(&budgets, "bashResultLines")
                .or_else(|| number(&budgets, "commandResultLines")),
            write_lines: number(&budgets, "writeLines"),
            diff_lines: number(&budgets, "diffLines"),
            // Both spellings are accepted: packs are hand-written, and the flat
            // form (`splitDiff`) is the one the settings documentation shows.
            split_diff: flag(&diff, "split", flag(&diff, "splitDiff", false)),
            split_diff_min_width: number(&diff, "minWidth")
                .or_else(|| number(&diff, "splitDiffMinWidth"))
                .unwrap_or(base.split_diff_min_width),
            palette: string_map(value.get("palette")),
            tool_colors: string_map(value.get("toolColors")),
            message_colors: string_map(value.get("messageBorderColors")),
            tool_icons: icon_map(icons.get("tool")),
            file_ext_icons: icon_map(icons.get("fileExtension")),
            file_name_icons: icon_map(icons.get("fileName")),
            message_icons: icon_map(icons.get("message")),
            token_colors: string_map(command.get("tokens")),
            thinking_color: value
                .get("thinkingColor")
                .and_then(Value::as_str)
                .unwrap_or("warning")
                .to_string(),
        })
    }

    /// Resolve a colour name for this pack: `palette` override, then a literal,
    /// then the theme slot of that name.
    pub fn color(&self, colors: &ThemeColors, name: &str) -> Color {
        if let Some(spec) = self.palette.get(name) {
            if let Some(color) = parse_color(spec) {
                return color;
            }
        }
        if let Some(color) = parse_color(name) {
            return color;
        }
        resolve_theme_color(colors, name)
    }

    /// Border colour for a tool panel.
    pub fn tool_border(&self, colors: &ThemeColors, tool: &str, status: ToolStatus) -> Color {
        match status {
            ToolStatus::Failed => colors.error,
            ToolStatus::Pending | ToolStatus::Running => colors.border_muted,
            ToolStatus::Completed => self
                .tool_colors
                .get(tool)
                .map(|name| self.color(colors, name))
                .unwrap_or(colors.success),
        }
    }

    /// Border colour for a message box kind (`user`, `skill`, `custom`, …).
    pub fn message_border(&self, colors: &ThemeColors, kind: &str) -> Color {
        self.message_colors
            .get(kind)
            .map(|name| self.color(colors, name))
            .unwrap_or(colors.border_muted)
    }

    pub fn always_shown(&self, tool: &str) -> bool {
        self.always_show.iter().any(|name| name == tool)
    }

    /// Glyph for a tool row (empty when icons are off or unknown).
    pub fn tool_icon(&self, tool: &str) -> Option<&Icon> {
        if !self.icons {
            return None;
        }
        self.tool_icons.get(tool)
    }

    pub fn message_icon(&self, kind: &str) -> Option<&Icon> {
        if !self.icons {
            return None;
        }
        self.message_icons.get(kind)
    }

    /// Glyph for a file path: exact basename first, then extension.
    pub fn file_icon(&self, path: &str) -> Option<&Icon> {
        if !self.icons {
            return None;
        }
        let base = path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(path)
            .to_ascii_lowercase();
        if let Some(icon) = self.file_name_icons.get(&base) {
            return Some(icon);
        }
        let ext = base.rsplit('.').next().unwrap_or("");
        if ext == base && !base.contains('.') {
            return self.file_ext_icons.get(ext);
        }
        self.file_ext_icons.get(ext)
    }

    fn token_color(&self, token: &str) -> Option<Color> {
        let spec = self.token_colors.get(token)?;
        parse_color(spec)
    }

    /// Collapsed preview budget for a tool result.
    pub fn result_budget(&self, tool: &str, is_error: bool) -> Option<usize> {
        if is_error {
            return None;
        }
        match tool {
            "bash" => self.bash_result_lines,
            "write" => self.write_lines,
            _ => None,
        }
    }
}

/// pi colour-name → theme slot.
pub fn resolve_theme_color(colors: &ThemeColors, name: &str) -> Color {
    match name {
        "text" => colors.text,
        "muted" => colors.muted,
        "dim" => colors.dim,
        "accent" => colors.accent,
        "error" => colors.error,
        "success" => colors.success,
        "warning" => colors.warning,
        "info" => colors.info,
        "border" => colors.border,
        "borderAccent" => colors.border_accent,
        "borderMuted" => colors.border_muted,
        "toolTitle" => colors.tool_title,
        "toolOutput" => colors.tool_output,
        "bashMode" => colors.bash_mode,
        "mdCode" => colors.md_code,
        "mdCodeBlock" => colors.md_code_block,
        "customMessageLabel" => colors.custom_message_label,
        "customMessageText" => colors.custom_message_text,
        "toolDiffAdded" => colors.tool_diff_added,
        "toolDiffRemoved" => colors.tool_diff_removed,
        "toolDiffContext" => colors.tool_diff_context,
        "thinkingText" => colors.thinking_text,
        _ => Color::Default,
    }
}

// ---------------------------------------------------------------------------
// Shell command highlighting
// ---------------------------------------------------------------------------

const FLOW_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "coproc", "{", "}", "!",
];

fn is_word_break(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\'' | '"' | '`' | '$' | '|' | '&' | ';' | '<' | '>' | '(' | ')'
    )
}

fn token_text(style: &TranscriptStyle, token: &str, text: &str, fallback: Color) -> String {
    if text.is_empty() {
        return String::new();
    }
    style.token_color(token).unwrap_or(fallback).fg(text)
}

fn variable_len(chars: &[char], i: usize) -> usize {
    let rest: String = chars[i..].iter().collect();
    if let Some(stripped) = rest.strip_prefix("$(") {
        return stripped.find(')').map(|end| end + 3).unwrap_or(1);
    }
    if let Some(stripped) = rest.strip_prefix("${") {
        return stripped.find('}').map(|end| end + 3).unwrap_or(1);
    }
    let mut len = 1;
    for (offset, c) in rest.chars().skip(1).enumerate() {
        let ok = if offset == 0 {
            c.is_ascii_alphabetic() || c == '_' || c.is_ascii_digit() || "@#?$!*-".contains(c)
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if !ok {
            break;
        }
        len += 1;
    }
    len
}

fn looks_like_flag(word: &str) -> bool {
    if word == "-" || word.len() < 2 || !word.starts_with('-') {
        return false;
    }
    word.trim_start_matches('-')
        .chars()
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
}

/// Colour one shell command line.
///
/// The tokenizer is the host's generic shell awareness (command position, flags,
/// operators, strings, variables, numbers, comments); the *colours* come from the
/// style pack's `command.tokens` map, with `colors.md_code_block` as the base.
pub fn highlight_command(command: &str, style: &TranscriptStyle, colors: &ThemeColors) -> String {
    let base = colors.md_code_block;
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(command.len() + 64);
    let mut i = 0usize;
    let mut expect_cmd = true;
    let slice = |start: usize, end: usize| -> String { chars[start..end].iter().collect() };

    while i < n {
        let c = chars[i];
        if c == ' ' || c == '\t' {
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\n' {
            out.push(c);
            expect_cmd = true;
            i += 1;
            continue;
        }
        if c == '#' && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n')) {
            let end = chars[i..]
                .iter()
                .position(|&ch| ch == '\n')
                .map(|offset| i + offset)
                .unwrap_or(n);
            out.push_str(&token_text(style, "comment", &slice(i, end), base));
            i = end;
            continue;
        }

        let four = slice(i, (i + 4).min(n));
        if four == "2>&1" || four == "1>&2" {
            out.push_str(&token_text(style, "operator", &four, base));
            i += 4;
            continue;
        }
        let three = slice(i, (i + 3).min(n));
        if three == "&>>" {
            out.push_str(&token_text(style, "operator", &three, base));
            i += 3;
            continue;
        }
        let two = slice(i, (i + 2).min(n));
        if two == "&&" || two == "||" {
            out.push_str(&token_text(style, "operator", &two, base));
            i += 2;
            expect_cmd = true;
            continue;
        }
        if matches!(two.as_str(), ">>" | "2>" | ">&" | "<&" | "&>" | "<<") {
            out.push_str(&token_text(style, "operator", &two, base));
            i += 2;
            continue;
        }
        if c == '|' || c == ';' {
            out.push_str(&token_text(style, "operator", &c.to_string(), base));
            i += 1;
            expect_cmd = true;
            continue;
        }
        if c == '>' || c == '<' {
            out.push_str(&token_text(style, "operator", &c.to_string(), base));
            i += 1;
            continue;
        }
        if c == '&' || c == '(' {
            out.push_str(&token_text(style, "operator", &c.to_string(), base));
            i += 1;
            expect_cmd = true;
            continue;
        }
        if c == ')' {
            out.push_str(&token_text(style, "operator", &c.to_string(), base));
            i += 1;
            expect_cmd = false;
            continue;
        }
        if c == '\'' {
            let end = chars[i + 1..]
                .iter()
                .position(|&ch| ch == '\'')
                .map(|offset| i + 1 + offset + 1)
                .unwrap_or(n);
            out.push_str(&token_text(style, "string", &slice(i, end), base));
            i = end;
            continue;
        }
        if c == '"' {
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == '"' {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let end = j.min(n);
            out.push_str(&token_text(style, "string", &slice(i, end), base));
            i = end;
            continue;
        }
        if c == '`' {
            let end = chars[i + 1..]
                .iter()
                .position(|&ch| ch == '`')
                .map(|offset| i + 1 + offset + 1)
                .unwrap_or(n);
            out.push_str(&token_text(style, "string", &slice(i, end), base));
            i = end;
            continue;
        }
        if c == '$' {
            let len = variable_len(&chars, i);
            if len > 1 {
                out.push_str(&token_text(
                    style,
                    "variable",
                    &slice(i, (i + len).min(n)),
                    base,
                ));
                i += len;
                continue;
            }
            out.push('$');
            i += 1;
            continue;
        }

        let mut j = i;
        while j < n && !is_word_break(chars[j]) {
            j += 1;
        }
        if j == i {
            out.push(c);
            i += 1;
            continue;
        }
        let word = slice(i, j);
        i = j;

        if expect_cmd {
            if let Some(eq) = word.find('=') {
                let name = &word[..eq];
                let assignment = !name.is_empty()
                    && name
                        .chars()
                        .next()
                        .map(|ch| ch.is_ascii_alphabetic() || ch == '_')
                        .unwrap_or(false)
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
                if assignment {
                    out.push_str(&token_text(style, "variable", name, base));
                    out.push_str(&token_text(style, "operator", "=", base));
                    out.push_str(&word[eq + 1..]);
                    continue;
                }
            }
            let token = if FLOW_KEYWORDS.contains(&word.as_str()) {
                "keyword"
            } else {
                "command"
            };
            out.push_str(&token_text(style, token, &word, base));
            expect_cmd = false;
            continue;
        }

        if looks_like_flag(&word) {
            if let Some(eq) = word.find('=') {
                out.push_str(&token_text(style, "flag", &word[..eq], base));
                out.push_str(&token_text(style, "operator", "=", base));
                out.push_str(&word[eq + 1..]);
            } else {
                out.push_str(&token_text(style, "flag", &word, base));
            }
            continue;
        }
        if !word.is_empty() && word.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
            out.push_str(&token_text(style, "number", &word, base));
            continue;
        }
        out.push_str(&word);
    }
    out
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Drop solid background fills so a framed box stays transparent. `\x1b[49m`
/// (reset to default background) is kept.
pub fn strip_background_fills(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '[' {
            let mut j = i + 2;
            while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j >= chars.len() {
                out.extend(chars[i..].iter());
                break;
            }
            let params: String = chars[i + 2..j].iter().collect();
            let reset = chars[j] == 'm';
            let fill = reset
                && (params.starts_with("48;")
                    || ((params.starts_with('4') || params.starts_with("10"))
                        && params.len() <= 3
                        && params != "49"));
            if fill {
                i = j + 1;
                continue;
            }
            out.extend(chars[i..=j].iter());
            i = j + 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn pad_to_width(line: &str, width: usize) -> String {
    let w = visible_width(line);
    if w == width {
        return line.to_string();
    }
    if w < width {
        let mut out = line.to_string();
        out.push_str(&" ".repeat(width - w));
        return out;
    }
    // Hard cut, no ellipsis: a frame row that is one column too wide should
    // lose that column, not print a tail of dots on every row.
    truncate_to_width(line, width, "")
}

/// Draw a rounded frame of exactly `width` columns around `lines`.
pub fn draw_frame(lines: &[String], width: usize, border: Color) -> Vec<String> {
    draw_frame_keep(lines, width, border, None)
}

/// Draw the frame while leaving background fills intact on `keep` rows.
///
/// Diff panes tint changed rows themselves; stripping those fills would erase
/// exactly the emphasis that makes an edit readable, so the caller marks the
/// diff rows and the frame only strips the panel's own tint elsewhere.
pub fn draw_frame_keep(
    lines: &[String],
    width: usize,
    border: Color,
    keep: Option<std::ops::Range<usize>>,
) -> Vec<String> {
    if width < 4 {
        return lines.to_vec();
    }
    let inner = width - 2;
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(border.fg(&format!("╭{}╮", "─".repeat(inner))));
    for (index, line) in lines.iter().enumerate() {
        let keep_bg = keep
            .as_ref()
            .map(|keep| keep.contains(&index))
            .unwrap_or(false);
        let content = if keep_bg {
            line.clone()
        } else {
            strip_background_fills(line)
        };
        out.push(format!(
            "{}{}{}",
            border.fg("│"),
            pad_to_width(&content, inner),
            border.fg("│")
        ));
    }
    out.push(border.fg(&format!("╰{}╯", "─".repeat(inner))));
    out
}

// ---------------------------------------------------------------------------
// Folded tool group
// ---------------------------------------------------------------------------

/// One assistant segment's tool panels, folded into a summary row.
///
/// Three states, cycled by Ctrl+O (or a click on the summary row):
/// `0` folded row → `1` one row per call → `2` every box expanded.
pub struct ToolGroup {
    members: Mutex<Vec<(String, Arc<dyn Component>)>>,
    /// The reasoning row for this segment. Kept typed so a streaming snapshot
    /// updates it in place instead of appending a row per delta.
    thinking: Mutex<Option<Arc<ThinkingPanel>>>,
    state: AtomicU8,
    style: Arc<TranscriptStyle>,
    /// Row under the mouse inside this group's own rendering
    /// (`usize::MAX` = nothing hovered). Drives the hover background that tells
    /// the user the row is clickable.
    hover: std::sync::atomic::AtomicUsize,
}

impl ToolGroup {
    pub fn new(style: Arc<TranscriptStyle>) -> Arc<Self> {
        Arc::new(Self {
            members: Mutex::new(Vec::new()),
            thinking: Mutex::new(None),
            state: AtomicU8::new(0),
            style,
            hover: std::sync::atomic::AtomicUsize::new(usize::MAX),
        })
    }

    /// Highlight one row while the pointer is over it (`None` clears).
    pub fn set_hover_row(&self, row: Option<usize>) {
        self.hover
            .store(row.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    /// Apply the host's hide-thinking preference to this group's reasoning rows.
    pub fn set_thinking_hidden(&self, hidden: bool) {
        if let Ok(members) = self.members.lock() {
            for (_, component) in members.iter() {
                if let Some(panel) = component.as_any().downcast_ref::<ThinkingPanel>() {
                    panel.set_hidden(hidden);
                }
            }
        }
    }

    /// Attach (or replace) this segment's reasoning row.
    pub fn set_thinking(&self, panel: Arc<ThinkingPanel>) {
        if let Ok(mut current) = self.thinking.lock() {
            *current = Some(panel);
        }
    }

    /// Start a new reasoning row: the previous one stays in the group, the next
    /// message's reasoning gets its own member.
    pub fn reset_thinking(&self) {
        if let Ok(mut current) = self.thinking.lock() {
            *current = None;
        }
    }

    pub fn thinking(&self) -> Option<Arc<ThinkingPanel>> {
        self.thinking
            .lock()
            .ok()
            .and_then(|current| current.clone())
    }

    pub fn add(&self, name: &str, component: Arc<dyn Component>) {
        if let Ok(mut members) = self.members.lock() {
            members.push((name.to_string(), component));
        }
    }

    /// The most recently added member, used to fold a streamed duplicate into
    /// the row already on screen.
    pub fn last_member(&self) -> Option<Arc<dyn Component>> {
        self.members
            .lock()
            .ok()
            .and_then(|members| members.last().map(|(_, component)| component.clone()))
    }

    pub fn len(&self) -> usize {
        self.members.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many real tool calls this group holds (the reasoning row excluded).
    pub fn tool_calls(&self) -> usize {
        self.members
            .lock()
            .map(|members| {
                members
                    .iter()
                    .filter(|(name, _)| name != "thinking")
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Sum of the members' own execution times, in milliseconds.
    /// A finished group reports one frozen span; a group with running work
    /// reports `None`, because a folded row must never animate a counter.
    ///
    /// Takes the already-locked member slice: `summary()` holds that same lock,
    /// and a second `self.members.lock()` here deadlocked the render thread.
    fn span_ms(members: &[(String, Arc<dyn Component>)]) -> Option<u128> {
        let mut first: Option<std::time::Instant> = None;
        let mut last_finish: Option<std::time::Instant> = None;
        let mut running = false;
        for (_, component) in members.iter() {
            let timing = if let Some(tool) = component
                .as_any()
                .downcast_ref::<crate::tool_execution::ToolExecutionComponent>(
            ) {
                tool.timing()
            } else if let Some(bash) = component
                .as_any()
                .downcast_ref::<crate::bash_execution::BashExecutionComponent>(
            ) {
                bash.timing()
            } else {
                None
            };
            let Some((started, finished)) = timing else {
                continue;
            };
            if first.map(|first| started < first).unwrap_or(true) {
                first = Some(started);
            }
            match finished {
                Some(finished) => {
                    if last_finish.map(|last| finished > last).unwrap_or(true) {
                        last_finish = Some(finished);
                    }
                }
                None => running = true,
            }
        }
        // While any call is still running the total keeps counting (the user
        // wants to see the work's elapsed time), and it freezes the moment the
        // last call ends.
        match (first, running) {
            (Some(started), true) => Some(started.elapsed().as_millis()),
            (Some(started), false) => Some(
                last_finish
                    .map(|finished| finished.saturating_duration_since(started).as_millis())
                    .unwrap_or(0),
            ),
            _ => None,
        }
    }

    pub fn set_state(&self, state: u8) {
        self.state.store(state.min(2), Ordering::Relaxed);
        let rows = self.state() == 1;
        if let Ok(members) = self.members.lock() {
            for (_, component) in members.iter() {
                if let Some(tool) = component
                    .as_any()
                    .downcast_ref::<crate::tool_execution::ToolExecutionComponent>()
                {
                    tool.set_row_only(rows);
                } else if let Some(bash) = component
                    .as_any()
                    .downcast_ref::<crate::bash_execution::BashExecutionComponent>(
                ) {
                    bash.set_row_only(rows);
                }
            }
        }
    }

    /// Folded line ↔ one row per call. Expanding every box at once is not a
    /// state: each row owns its expansion, so a click opens and closes exactly
    /// the row it landed on.
    pub fn cycle(&self) -> u8 {
        let next = if self.state() == 0 { 1 } else { 0 };
        self.set_state(next);
        next
    }

    /// Click on a row of the rendered group at `width`.
    ///
    /// Row 0 is the summary line (folded ↔ rows ↔ boxes). Any later row belongs
    /// to one of the rendered members and toggles that member's own expansion.
    pub fn click_at(&self, row: usize, width: usize) -> bool {
        // A single-call group draws its card directly (no summary row), so every
        // row of it belongs to that card.
        let foldable = self.is_foldable();
        // The summary may wrap onto several rows when many tools ran: any of them
        // toggles the fold.
        let summary_rows = if foldable {
            self.summary(width, &theme()).len()
        } else {
            0
        };
        if foldable && row < summary_rows {
            self.cycle();
            return true;
        }
        if foldable && self.state() == 0 {
            return false;
        }
        let inner = width.max(1);
        let members: Vec<Arc<dyn Component>> = self
            .members
            .lock()
            .map(|members| {
                members
                    .iter()
                    .map(|(_, component)| component.clone())
                    .collect()
            })
            .unwrap_or_default();
        // The summary can wrap onto several lines when many tools ran, so the
        // member offsets start after the real summary height (a fixed `1`
        // shifted every row and made clicks land on the wrong card).
        let mut offset = summary_rows;
        for member in members {
            let height = member.render(inner).len();
            if row < offset + height {
                let local = row - offset;
                if let Some(thinking) = member.as_any().downcast_ref::<ThinkingPanel>() {
                    return thinking.click(local);
                }
                if let Some(tool) = member
                    .as_any()
                    .downcast_ref::<crate::tool_execution::ToolExecutionComponent>()
                {
                    return tool.click(local);
                }
                if let Some(bash) = member
                    .as_any()
                    .downcast_ref::<crate::bash_execution::BashExecutionComponent>()
                {
                    return bash.click(local);
                }
                return toggle_component(&member);
            }
            offset += height;
        }
        false
    }

    /// Whether this group is worth a summary line: more than one tool call, or a
    /// reasoning row plus a call. A single call draws its own card instead.
    fn is_foldable(&self) -> bool {
        let Ok(members) = self.members.lock() else {
            return false;
        };
        let mut calls = 0usize;
        let mut has_thinking = false;
        for (name, _) in members.iter() {
            if name == "thinking" {
                has_thinking = true;
            } else {
                calls += 1;
            }
        }
        // Any content folds: a turn accumulates calls into one row, so a row with
        // a single call is still the right place for it (and for its thinking).
        calls > 0 || has_thinking
    }

    fn summary(&self, width: usize, theme: &Theme) -> Vec<String> {
        let colors = &theme.colors;
        // The active pack wins at render time: reloading or disabling a pack must
        // not leave an old transcript drawing with a stale style.
        let style = crate::transcript::active_style().unwrap_or_else(|| self.style.clone());
        let Ok(members) = self.members.lock() else {
            return Vec::new();
        };
        let mut order: Vec<(String, usize)> = Vec::new();
        let mut failed = 0usize;
        let mut calls = 0usize;
        for (name, component) in members.iter() {
            // The reasoning row is not a tool call: it has its own marker below
            // and must not inflate the call count or the breakdown.
            if name == "thinking" {
                continue;
            }
            calls += 1;
            // A skill invocation is rendered as `[skill] <name>` upstream, so the
            // fold must not report the raw tool name (skills are invoked through
            // a generic tool).
            let label = if let Some(tool) = component
                .as_any()
                .downcast_ref::<crate::tool_execution::ToolExecutionComponent>(
            ) {
                if tool.status() == ToolStatus::Failed {
                    failed += 1;
                }
                if tool.is_skill() {
                    "skill".to_string()
                } else {
                    name.clone()
                }
            } else {
                name.clone()
            };
            match order.iter_mut().find(|(existing, _)| *existing == label) {
                Some((_, count)) => *count += 1,
                None => order.push((label, 1)),
            }
        }
        let glyph = if self.state() == 0 { "▸" } else { "▾" };
        let status = if failed > 0 {
            colors.error.fg("✗")
        } else {
            colors.success.fg("✓")
        };
        let calls = if calls == 1 {
            "1 call".to_string()
        } else {
            format!("{calls} calls")
        };
        let breakdown = order
            .iter()
            .map(|(name, count)| {
                let icon = if name == "skill" {
                    style
                        .message_icon("skill")
                        .map(|icon| format!("{} ", icon.glyph))
                } else {
                    style.tool_icon(name).map(|icon| format!("{} ", icon.glyph))
                }
                .unwrap_or_default();
                format!("{}{}×{}", icon, name, count)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut parts: Vec<String> = Vec::new();
        if self
            .thinking()
            .map(|panel| !panel.is_empty())
            .unwrap_or(false)
        {
            let icon = style
                .message_icon("thinking")
                .map(|icon| format!("{} ", icon.glyph))
                .unwrap_or_default();
            parts.push(format!("{icon}thinking"));
        }
        if !breakdown.is_empty() {
            parts.push(breakdown);
        }
        if failed > 0 {
            parts.push(colors.error.fg(&format!("{failed} failed")));
        }
        if let Some(elapsed) = Self::span_ms(members.as_slice()) {
            if elapsed >= 100 {
                parts.push(format_duration(elapsed));
            }
        }
        let tail = if parts.is_empty() {
            String::new()
        } else {
            format!(" · {}", parts.join(&colors.muted.fg(" · ")))
        };
        let row = format!(
            " {} {} {}{}",
            colors.muted.fg(glyph),
            status,
            colors.tool_title.fg(&calls),
            tail
        );
        // A long tool list wraps onto further lines instead of being clipped:
        // every tool that ran has to stay visible in the fold.
        crate::utils::wrap_text_with_ansi(&row, width.max(1))
    }
}

impl Component for ToolGroup {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme();
        // With no pack active (fresh start / pack removed) a fold must not keep
        // drawing: emit the members as plain panels, in order, like stock rpi.
        if crate::transcript::active_style().is_none() {
            let mut out = Vec::new();
            if let Ok(members) = self.members.lock() {
                for (_, component) in members.iter() {
                    out.extend(component.render(width));
                }
            }
            return out;
        }
        // A fold of exactly one call is noise: the summary only restates what the
        // single card already says. Draw the card itself instead.
        if !self.is_foldable() {
            let mut out = Vec::new();
            if let Ok(members) = self.members.lock() {
                for (_, component) in members.iter() {
                    out.extend(component.render(width));
                }
            }
            out.push(String::new());
            return out;
        }
        let mut out = self.summary(width, &theme);
        if self.state() > 0 {
            if let Ok(members) = self.members.lock() {
                // Each card keeps its own outer frame and is drawn at the full
                // width: no second gutter, no truncation, so there is no stray
                // right border and no clipped-line dots.
                for (_, component) in members.iter() {
                    out.extend(component.render(width));
                }
            }
        }
        out.push(String::new());
        // Hover feedback: the row under the pointer gets a subtle band so a
        // click target is obvious before the click.
        let hover = self.hover.load(Ordering::Relaxed);
        if hover != usize::MAX {
            if let Some(line) = out.get_mut(hover) {
                *line = theme.colors.surface.bg(line);
            }
        }
        out
    }

    fn invalidate(&self) {
        if let Ok(members) = self.members.lock() {
            for (_, component) in members.iter() {
                component.invalidate();
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compact duration: `0.4s`, `1m05s`, `1h02m`.
pub fn format_duration(ms: u128) -> String {
    if ms < 1000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    if ms < 60_000 {
        return format!("{:.1}s", (ms as f64 / 100.0).round() / 10.0);
    }
    let seconds = ms / 1000;
    if seconds < 3600 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
}

/// A pack-declared two-pane diff at `width`, or `None` when it does not fit.
pub fn split_diff(
    raw: &str,
    width: usize,
    max_lines: Option<usize>,
    style: &TranscriptStyle,
    language: Option<&str>,
) -> Option<Vec<String>> {
    if !style.split_diff || width < style.split_diff_min_width {
        return None;
    }
    crate::split_diff::render_split_diff(raw, width, max_lines, language)
}

/// Toggle whatever kind of panel a style pack can fold.
pub fn toggle_component(component: &Arc<dyn Component>) -> bool {
    let any = component.as_any();
    if let Some(tool) = any.downcast_ref::<crate::tool_execution::ToolExecutionComponent>() {
        let next = !tool.is_expanded();
        tool.set_expanded(next);
        return true;
    }
    if let Some(bash) = any.downcast_ref::<crate::bash_execution::BashExecutionComponent>() {
        let next = !bash.is_expanded();
        bash.set_expanded(next);
        return true;
    }
    if let Some(thinking) = any.downcast_ref::<ThinkingPanel>() {
        return thinking.toggle();
    }
    if let Some(group) = any.downcast_ref::<ToolGroup>() {
        group.cycle();
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Thinking row
// ---------------------------------------------------------------------------

/// A segment's reasoning, collapsed to one row so it can live inside a fold.
///
/// pi-tidy keeps thinking in the same folded group as the tool calls; the host
/// still renders the assistant bubble from `AssistantBlock`s, so reasoning is
/// split out into its own component and can join a [`ToolGroup`].
pub struct ThinkingPanel {
    text: Mutex<String>,
    /// 0 = collapsed, `n` = `n` screenfuls of expanded reasoning.
    step: AtomicU8,
    /// The host's "hide thinking" preference: the row keeps its label but never
    /// shows the reasoning text (a style pack must not leak hidden reasoning).
    hidden: std::sync::atomic::AtomicBool,
    style: Arc<TranscriptStyle>,
}

impl ThinkingPanel {
    pub fn new(style: Arc<TranscriptStyle>) -> Arc<Self> {
        Arc::new(Self {
            text: Mutex::new(String::new()),
            step: AtomicU8::new(0),
            hidden: std::sync::atomic::AtomicBool::new(false),
            style,
        })
    }

    /// Match the host's hide-thinking preference.
    pub fn set_hidden(&self, hidden: bool) {
        self.hidden.store(hidden, Ordering::Relaxed);
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden.load(Ordering::Relaxed)
    }

    /// Replace the reasoning text (streaming snapshots send the whole block).
    pub fn set_text(&self, text: &str) {
        if let Ok(mut current) = self.text.lock() {
            *current = text.to_string();
        }
    }

    pub fn text(&self) -> String {
        self.text
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty()
    }

    pub fn is_expanded(&self) -> bool {
        self.step.load(Ordering::Relaxed) > 0
    }

    pub fn set_expanded(&self, expanded: bool) {
        self.step.store(u8::from(expanded), Ordering::Relaxed);
    }

    /// One more screenful (a body click while expanded).
    pub fn grow(&self) {
        let next = self.step.load(Ordering::Relaxed).saturating_add(1).min(8);
        self.step.store(next, Ordering::Relaxed);
    }

    pub fn toggle(&self) -> bool {
        let next = if self.is_expanded() { 0 } else { 1 };
        self.step.store(next, Ordering::Relaxed);
        next > 0
    }

    /// Click on the rendered row: the header toggles, the body extends.
    pub fn click(&self, _row: usize) -> bool {
        // Any click on an open row folds it back: an expanded card must never
        // trap the user in a state they cannot close.
        self.toggle()
    }

    /// Rows the expanded body may take right now.
    fn budget(&self) -> usize {
        window_rows() * self.step.load(Ordering::Relaxed).max(1) as usize
    }
}

/// The last `budget` characters of `text`, with a leading ellipsis when cut and
/// newlines flattened to single spaces (a flowing one-line preview).
fn tail_preview(text: &str, budget: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if budget == 0 || chars.is_empty() {
        return String::new();
    }
    if chars.len() <= budget {
        return flat;
    }
    let start = chars.len().saturating_sub(budget.saturating_sub(1));
    format!("…{}", chars[start..].iter().collect::<String>())
}

/// Cells reserved on the thinking row for the chevron, glyph, label and counter.
fn chars_label(icon_chars: usize, chars: usize) -> usize {
    6 + icon_chars + "thinking".chars().count() + chars.to_string().len()
}

impl Component for ThinkingPanel {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme();
        let colors = &theme.colors;
        // No pack active: the fold is gone, so show the reasoning as plain dim
        // text. The bubble keeps it hidden while a pack is active, and losing the
        // content on reload would be worse than showing it unfolded.
        if crate::transcript::active_style().is_none() {
            let text = self.text();
            if text.trim().is_empty() {
                return vec![String::new()];
            }
            let mut out: Vec<String> = crate::text::Text::new(text.trim_end(), 0, 0)
                .render(width)
                .into_iter()
                .map(|line| colors.thinking_text.fg(&line))
                .collect();
            out.push(String::new());
            return out;
        }
        // Resolve at render time so a reloaded/disabled pack is reflected in old
        // transcripts instead of keeping a stale style.
        let style = crate::transcript::active_style().unwrap_or_else(|| self.style.clone());
        let color = style.color(colors, &style.thinking_color);
        let text = self.text();
        if text.trim().is_empty() {
            return vec![String::new()];
        }
        let glyph = if self.is_expanded() { "▾" } else { "▸" };
        let icon = if style.icons {
            style
                .message_icon("thinking")
                .map(|icon| format!("{} ", icon.glyph))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let chars = text.chars().count();
        // The host asked for thinking to be hidden: keep the label so the fold
        // still shows a reasoning row, but never render the text (a style pack
        // must not leak reasoning the user chose to hide).
        if self.is_hidden() {
            let mut out = vec![truncate_to_width(
                &format!(
                    " {} {}",
                    colors.muted.fg("▸"),
                    color.fg(&format!("{icon}thinking"))
                ),
                width,
                "",
            )];
            out.push(String::new());
            return out;
        }
        // Collapsed: one flowing line — the tail of the reasoning, so the row
        // moves as the model thinks but never grows into a wall of text.
        let mut out = if self.is_expanded() {
            vec![truncate_to_width(
                &format!(
                    " {} {}",
                    colors.muted.fg(glyph),
                    color.fg(&format!("{icon}thinking"))
                ),
                width,
                "…",
            )]
        } else {
            let reserved = chars_label(icon.chars().count(), chars);
            let preview = tail_preview(&text, width.saturating_sub(reserved));
            vec![truncate_to_width(
                &format!(
                    " {} {} {} {}",
                    colors.muted.fg(glyph),
                    color.fg(&format!("{icon}thinking")),
                    colors.muted.fg(&format!("·{chars}")),
                    colors.thinking_text.fg(&preview)
                ),
                width,
                "…",
            )]
        };
        if self.is_expanded() {
            let body =
                crate::text::Text::new(text.trim_end(), 0, 0).render(width.saturating_sub(2));
            let budget = self.budget();
            for line in body.iter().take(budget) {
                out.push(format!("  {line}"));
            }
            if body.len() > budget {
                out.push(format!(
                    "  {} {}",
                    colors
                        .muted
                        .fg(&format!("… {} more lines", body.len() - budget)),
                    color.fg("click to continue")
                ));
            }
        }
        out.push(String::new());
        out
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(value: Value) -> TranscriptStyle {
        TranscriptStyle::from_value(&value).expect("pack enabled")
    }

    #[test]
    fn default_pack_keeps_the_stock_look() {
        let base = TranscriptStyle::default();
        assert!(!base.group_tools);
        assert!(!base.frame_tools);
        assert!(!base.icons);
    }

    #[test]
    fn nested_and_flat_layouts_parse_the_same() {
        let flat = serde_json::json!({
            "groupTools": true,
            "frameTools": true,
            "splitDiff": true,
            "splitDiffMinWidth": 60,
            "bashResultLines": 1,
            "command": { "highlight": true, "tokens": { "command": "accent" } }
        });
        let nested = serde_json::json!({
            "group": { "groupTools": true },
            "frame": { "frameTools": true },
            "diff": { "split": true, "minWidth": 60 },
            "budgets": { "bashResultLines": 1 },
            "command": { "highlight": true, "tokens": { "command": "accent" } }
        });
        for pack in [flat, nested] {
            let s = style(pack);
            assert!(s.group_tools);
            assert!(s.frame_tools);
            assert!(s.split_diff);
            assert_eq!(s.split_diff_min_width, 60);
            assert_eq!(s.bash_result_lines, Some(1));
            assert!(s.command_highlight);
        }
    }

    #[test]
    fn icon_tables_and_palette_reach_the_renderer() {
        let s = style(serde_json::json!({
            "icons": {
                "enabled": true,
                "tool": { "bash": { "icon": "T", "color": "#010203" } },
                "fileExtension": { "rs": { "icon": "R", "color": "#040506" } },
                "fileName": { "cargo.toml": "C" }
            },
            "palette": { "accent": "#0a0b0c" }
        }));
        assert_eq!(s.tool_icon("bash").unwrap().glyph, "T");
        assert_eq!(
            s.tool_icon("bash").unwrap().color,
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(s.file_icon("src/main.rs").unwrap().glyph, "R");
        assert_eq!(s.file_icon("Cargo.toml").unwrap().glyph, "C");
        assert_eq!(
            s.color(&ThemeColors::default(), "accent"),
            Color::Rgb(10, 11, 12)
        );
        assert_eq!(
            s.tool_border(&ThemeColors::default(), "bash", ToolStatus::Failed),
            ThemeColors::default().error
        );
    }

    #[test]
    fn command_highlighting_keeps_every_character() {
        let s = style(serde_json::json!({
            "command": {
                "highlight": true,
                "tokens": {
                    "command": "accent", "flag": "success", "string": "warning",
                    "operator": "error", "variable": "info", "number": "info",
                    "comment": "muted", "keyword": "accent"
                }
            }
        }));
        let colors = ThemeColors::default();
        for command in [
            "echo a#b",
            "git commit -m 'x' && ls | wc -l",
            "VAR=1 cmd --flag=value $HOME ${X} $(date)",
            "cat <<'EOF' > file.txt",
            "if [ -f x ]; then echo ok; fi",
            "printf \"%s\\n\" \"a\\\"b\"",
        ] {
            let out = highlight_command(command, &s, &colors);
            assert_eq!(
                crate::ansi::strip_ansi(&out),
                command,
                "tokenizer changed the command: {command}"
            );
        }
    }

    #[test]
    fn frame_is_exact_width_and_transparent() {
        let lines = vec![
            "\x1b[48;5;236m  hello  \x1b[49m".to_string(),
            "  second".to_string(),
        ];
        let framed = draw_frame(&lines, 20, Color::Ansi256(239));
        assert_eq!(framed.len(), 4);
        for line in &framed {
            assert_eq!(visible_width(line), 20, "line: {line:?}");
            assert!(!line.contains("\x1b[48;"));
        }
        assert!(framed[0].contains('╭'));
        assert!(framed[3].contains('╯'));
    }

    #[test]
    fn duration_is_compact() {
        assert_eq!(format_duration(120), "0.1s");
        assert_eq!(format_duration(1230), "1.2s");
        assert_eq!(format_duration(65_000), "1m05s");
        assert_eq!(format_duration(3_725_000), "1h02m");
    }
}
