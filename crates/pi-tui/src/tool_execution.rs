//! Tool execution component for displaying tool calls.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/tool-execution.ts

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::ansi::{bold, strip_ansi};
use crate::theme::theme;
use crate::utils::{apply_background_to_line, truncate_to_width};

/// Maximum diff lines rendered inline when expanded before collapsing the rest.
const DIFF_LINE_CAP: usize = 40;
/// Diff lines shown when collapsed — a preview density that keeps a big edit
/// from dumping 40 lines into the transcript while still showing what changed.
/// Ctrl+T expands to [`DIFF_LINE_CAP`].
const DIFF_PREVIEW_LINES: usize = 6;

/// Tool execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool is pending execution
    Pending,
    /// Tool is running
    Running,
    /// Tool completed successfully
    Completed,
    /// Tool failed
    Failed,
}

/// Component that displays a tool execution.
///
/// Mirrors TypeScript ToolExecutionComponent class (simplified).
pub struct ToolExecutionComponent {
    /// Tool name
    name: Mutex<String>,
    /// Tool arguments (displayed)
    args: Mutex<String>,
    /// Tool result (displayed after execution)
    result: Mutex<Option<String>>,
    /// Execution status
    status: Mutex<ToolStatus>,
    /// Whether expanded
    expanded: Mutex<bool>,
    /// Optional pre-rendered colored diff lines (from `render_diff`). When
    /// present the diff is always shown regardless of `expanded` — the diff
    /// IS the useful content for an `edit` tool.
    diff_lines: Mutex<Option<Vec<String>>>,
}

impl ToolExecutionComponent {
    /// Create a new tool execution component.
    pub fn new(name: &str, args: &str) -> Self {
        Self {
            name: Mutex::new(name.to_string()),
            args: Mutex::new(args.to_string()),
            result: Mutex::new(None),
            status: Mutex::new(ToolStatus::Pending),
            expanded: Mutex::new(false),
            diff_lines: Mutex::new(None),
        }
    }

    /// Set the tool arguments.
    pub fn set_args(&self, args: &str) {
        if let Ok(mut a) = self.args.lock() {
            *a = args.to_string();
        }
    }

    /// Set the tool result.
    pub fn set_result(&self, result: &str, is_error: bool) {
        if let Ok(mut r) = self.result.lock() {
            // The header glyph + bg tint already convey status (✗/red for
            // failure, ✓/green for success), so the result text is stored
            // raw — no redundant emoji prefix (pi doesn't double these up).
            *r = Some(result.to_string());
        }
        if let Ok(mut s) = self.status.lock() {
            *s = if is_error { ToolStatus::Failed } else { ToolStatus::Completed };
        }
    }

    /// Mark as running.
    pub fn set_running(&self) {
        if let Ok(mut s) = self.status.lock() {
            *s = ToolStatus::Running;
        }
    }

    /// Set expanded state.
    pub fn set_expanded(&self, expanded: bool) {
        if let Ok(mut e) = self.expanded.lock() {
            *e = expanded;
        }
    }

    /// Whether the component is currently expanded (for toggle helpers).
    pub fn is_expanded(&self) -> bool {
        *self.expanded.lock().unwrap()
    }

    /// Get the status.
    pub fn status(&self) -> ToolStatus {
        *self.status.lock().unwrap()
    }

    /// Get the tool name.
    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }

    /// Attach a pre-rendered colored diff (from [`crate::diff::render_diff`]).
    /// When set, the diff lines are always shown (regardless of `expanded`)
    /// so an edit's changes are visible directly in the transcript.
    pub fn set_diff(&self, lines: Vec<String>) {
        if let Ok(mut d) = self.diff_lines.lock() {
            *d = Some(lines);
        }
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let mut lines = Vec::new();

        // Top spacer — pi's tool-execution.ts adds a Spacer(1) child inside
        // the component so each tool panel is visually separated from the
        // surrounding transcript (the old rpi drain path had no gap between
        // consecutive tool panels).
        lines.push(String::new());

        let name = self.name.lock().unwrap();
        let status = self.status.lock().unwrap();
        let args = self.args.lock().unwrap();
        let result = self.result.lock().unwrap();
        let expanded = self.expanded.lock().unwrap();
        let diff_lines = self.diff_lines.lock().unwrap();

        // Status indicator — a colored glyph (no emoji), pi style.
        let (status_icon, status_color) = match *status {
            ToolStatus::Pending => ("●", colors.muted),
            ToolStatus::Running => ("●", colors.accent),
            ToolStatus::Completed => ("✓", colors.success),
            ToolStatus::Failed => ("✗", colors.error),
        };

        // Background tint for the header row reflects status (pi tool*Bg).
        let bg = match *status {
            ToolStatus::Pending | ToolStatus::Running => colors.tool_pending_bg,
            ToolStatus::Failed => colors.tool_error_bg,
            ToolStatus::Completed => colors.tool_success_bg,
        };

        // Tool header line.
        // For args JSON we display a compact signature (e.g. `read src/foo.rs`)
        // instead of the raw `{"path":"..."}` blob — see `parse_args_summary`.
        // The verbose JSON still shows in the expanded args block below.
        let chevron = if *expanded { "▾" } else { "▸" };
        let label = tools::tool_label(&name);
        let summary = tools::parse_args_summary(&name, &args);
        let head_parts = if summary.is_empty() {
            format!(
                "{} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.muted.fg(chevron),
            )
        } else {
            format!(
                "{} {} {} {}",
                status_color.fg(status_icon),
                colors.tool_title.fg(&bold(&label)),
                colors.tool_output.fg(&summary),
                colors.muted.fg(chevron),
            )
        };
        let header_line = apply_background_to_line(&format!(" {}", head_parts), width, |s| bg.bg(s));
        lines.push(header_line);

        // Expanded: the full args JSON (indented, dim) above the result. The
        // compact summary already lives in the header, so this is the
        // "show me everything" path for debugging a tool call.
        if *expanded {
            if !args.is_empty() && args.trim() != "{}" {
                let args_line = format!("  {} {}",
                    colors.muted.fg("args:"),
                    colors.dim.fg(&pretty_args(&args)));
                let args_line = apply_background_to_line(&args_line, width, |s| bg.bg(s));
                lines.push(args_line);
            }
        }

        // Show result if available (always — a failure's error text is the
        // payload, not a detail to hide behind expand).
        if let Some(ref r) = *result {
            for line in r.lines() {
                let result_line = format!("  {}", colors.tool_output.fg(line));
                let result_line = apply_background_to_line(&result_line, width, |s| bg.bg(s));
                lines.push(result_line);
            }
        } else if diff_lines.as_ref().is_none_or(|d| d.is_empty()) && !*expanded {
            // No result yet, no diff, and collapsed: pad one bg-tinted row so
            // the tool block still reads as a block (pi keeps the bg band).
            let pad = apply_background_to_line("", width, |s| bg.bg(s));
            lines.push(pad);
        }

        // A colored diff (from `render_diff`) is shown collapsed by default
        // (a small preview) and fully when expanded — an edit's diff is the
        // useful payload, but dumping 40 lines into the transcript by default
        // swamped the conversation. Cap at DIFF_LINE_CAP even when expanded.
        if let Some(diff) = diff_lines.as_ref() {
            let total = diff.len();
            let cap = if *expanded { DIFF_LINE_CAP } else { DIFF_PREVIEW_LINES };
            let shown = diff.iter().take(cap);
            for dl in shown {
                // Diff lines are already complete (colors + content). Indent
                // by 2 cols so the `+`/`-` gutter lines up under the header.
                let body = strip_ansi(dl);
                if body.is_empty() {
                    lines.push(String::new());
                } else {
                    let indented = format!("  {}", dl);
                    lines.push(truncate_to_width(&indented, width, "…"));
                }
            }
            if total > cap {
                let more = total - cap;
                let hint = if *expanded {
                    format!("… {} more diff lines (Ctrl+T to collapse)", more)
                } else {
                    format!("… {} more diff lines (Ctrl+T to expand)", more)
                };
                lines.push(format!("  {}", colors.muted.fg(&hint)));
            }
        }

        lines
    }

    fn invalidate(&self) {
        // Tool has no cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Best-effort pretty-print of a tool args JSON blob for the expanded args
/// block. Falls back to the raw string if it isn't valid JSON. Kept inline
/// rather than reaching for `serde_json::to_string_pretty` so `rpi-tui` stays
/// free of a serde dep (project constraint) — this is display-only and a
/// compact one-liner is more useful in a terminal than a multi-line dump.
fn pretty_args(args: &str) -> String {
    // Compact single-line: `{"path":"src/foo.rs","offset":1}` →
    // `path=src/foo.rs offset=1`. Strings drop quotes, numbers/bools as-is.
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return "{}".to_string();
    }
    // Lightweight parse: must start with `{`. If not, show raw.
    if !trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    let inner = tools::flatten_json_object(trimmed);
    if inner.is_empty() {
        trimmed.to_string()
    } else {
        inner
    }
}

/// Tool-display helpers: turn a raw tool name + args JSON into a compact,
/// human-readable signature for the tool-block header (e.g.
/// `read src/foo.rs`, `grep pattern · src`, `bash ls -la`).
///
/// Lives in `rpi-tui` (no `rpi-tools` dep) so the render crate can stay
/// standalone; the mapping is purely lexical over the known builtin tool
/// arg shapes. Unknown tools fall back to the raw args.
mod tools {
    /// A short, uppercase label for the tool name in the header. The raw
    /// `name` is already lowercase (`read`, `edit`, …); uppercasing it makes
    /// the header read as a tag rather than a word, distinguishing it from
    /// the path/argument summary that follows.
    pub fn tool_label(name: &str) -> String {
        name.to_ascii_uppercase()
    }

    /// Parse a known builtin tool's args JSON into a compact arg summary.
    /// Returns "" when there's nothing useful to show (e.g. unknown tool or
    /// empty args) so the caller can fall back to a header with no summary.
    pub fn parse_args_summary(tool: &str, args_json: &str) -> String {
        let map = match parse_json_object(args_json) {
            Some(m) => m,
            None => return String::new(),
        };
        let get = |k: &str| field(&map, k);
        match tool {
            "read"  => get("path"),
            "write" => get("path"),
            "ls"    => get("path"),
            "find"  => {
                let p = get("pattern");
                let dir = get("path");
                match dir.as_str() {
                    "" => p,
                    d => format!("{} · {}", p, d),
                }
            }
            "grep" => {
                let p = get("pattern");
                let path = get("path");
                let glob = get("glob");
                let mut parts = vec![p];
                if !glob.is_empty() { parts.push(format!("glob {}", glob)); }
                if !path.is_empty() { parts.push(path); }
                parts.join(" · ")
            }
            "edit" => {
                // path + count of edits (oldText is too long to inline).
                let path = get("path");
                if let Some(arr) = parse_json_array_len(args_json, "edits") {
                    if arr == 1 { path } else { format!("{} ({} edits)", path, arr) }
                } else { path }
            }
            "bash" => get("command"),
            _ => {
                // Unknown tool: show the first string-valued field if any.
                map.into_iter().next().map(|(_, v)| v).unwrap_or_default()
            }
        }
    }

    /// Look up a field's value (unquoted) in a parsed object map.
    fn field(map: &[(String, String)], key: &str) -> String {
        map.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| unquote(v))
            .unwrap_or_default()
    }

    /// Flatten a JSON object string into `k=v k=v` (strings unquoted).
    /// Returns "" if the input isn't a `{...}` object or has no fields.
    /// This is a display-only best-effort parser — it does NOT handle nested
    /// objects/arrays (those stay as their raw JSON substring) or escapes;
    /// it's good enough for the expanded args block where the compact header
    /// already showed the important bit.
    pub fn flatten_json_object(json: &str) -> String {
        let Some(map) = parse_json_object(json) else { return String::new(); };
        map.into_iter()
            .map(|(k, v)| format!("{}={}", k, unquote(&v)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Parse a flat JSON object `{"k":"v",...}` into an ordered (k,v) list.
    /// Values are kept as their raw JSON substring (strings keep quotes;
    /// numbers/bools are bare). Returns None if the input isn't a flat object.
    ///
    /// This is a tiny hand-rolled scanner — `rpi-tui` deliberately has no
    /// serde dep. It handles the arg shapes the builtin tools emit (flat,
    /// string/number/bool values); nested objects/arrays are returned as
    /// their raw substring (the caller treats them opaquely).
    fn parse_json_object(json: &str) -> Option<Vec<(String, String)>> {
        let s = json.trim();
        if !s.starts_with('{') || !s.ends_with('}') { return None; }
        let inner = &s[1..s.len() - 1];
        let mut out = Vec::new();
        let mut chars = inner.chars().peekable();
        loop {
            skip_ws(&mut chars);
            if chars.peek().is_none() { break; }
            // key (must be a quoted string)
            if chars.peek() != Some(&'"') { return None; }
            let key = read_string(&mut chars)?;
            skip_ws(&mut chars);
            if chars.next() != Some(':') { return None; }
            skip_ws(&mut chars);
            // value: string, number, true/false/null, or nested (raw).
            let val = read_value(&mut chars);
            out.push((key, val));
            skip_ws(&mut chars);
            match chars.peek() {
                Some(&',') => { chars.next(); }
                Some(_) => { return None; }   // malformed
                None => break,
            }
        }
        Some(out)
    }

    /// Count the elements of a JSON array field `name` in `json`. Used by the
    /// edit summary to show `(N edits)` without holding the array contents.
    fn parse_json_array_len(json: &str, name: &str) -> Option<usize> {
        // Find `"name":` then the `[...]` that follows.
        let pat = format!("\"{}\":", name);
        let idx = json.find(&pat)?;
        let rest = &json[idx + pat.len()..];
        let rest = rest.trim_start();
        if !rest.starts_with('[') { return None; }
        // Scan the array body respecting nested brackets + strings.
        let mut depth = 0isize;
        let mut in_str = false;
        let mut esc = false;
        let mut count = 0usize;
        let mut saw_any = false;
        for c in rest.chars() {
            if in_str {
                if esc { esc = false; }
                else if c == '\\' { esc = true; }
                else if c == '"' { in_str = false; }
                continue;
            }
            match c {
                '"' => in_str = true,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth == 0 { break; }
                }
                ',' if depth == 1 => count += 1,
                _ if !c.is_whitespace() && depth >= 1 => saw_any = true,
                _ => {}
            }
        }
        if !saw_any { Some(0) } else { Some(count + 1) }
    }

    fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars>) {
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() { chars.next(); } else { break; }
        }
    }

    fn read_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
        if chars.next()? != '"' { return None; }
        let mut s = String::new();
        let mut esc = false;
        while let Some(c) = chars.next() {
            if esc { esc = false; s.push(c); continue; }
            match c {
                '\\' => esc = true,
                '"' => return Some(s),
                _ => s.push(c),
            }
        }
        None
    }

    fn read_value(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
        match chars.peek() {
            Some(&'"') => {
                // Quoted string — include the quotes in the raw value (so the
                // caller can distinguish strings from numbers); `unquote`
                // strips them for display.
                let mut s = String::from("\"");
                chars.next();
                let mut esc = false;
                while let Some(c) = chars.next() {
                    s.push(c);
                    if esc { esc = false; continue; }
                    if c == '\\' { esc = true; continue; }
                    if c == '"' { break; }
                }
                s
            }
            Some(&'[') | Some(&'{') => read_nested(chars),
            _ => {
                // Bare token (number/true/false/null) — read until , or }.
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ',' || c == '}' { break; }
                    s.push(c);
                    chars.next();
                }
                s.trim().to_string()
            }
        }
    }

    fn read_nested(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
        // Copy the raw substring for a nested array/object, tracking depth +
        // string state so commas inside don't terminate it early.
        let mut s = String::new();
        let mut depth = 0isize;
        let mut in_str = false;
        let mut esc = false;
        while let Some(c) = chars.next() {
            s.push(c);
            if in_str {
                if esc { esc = false; } else if c == '\\' { esc = true; } else if c == '"' { in_str = false; }
                continue;
            }
            match c {
                '"' => in_str = true,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth == 0 { break; }
                }
                _ => {}
            }
        }
        s
    }

    fn unquote(v: &str) -> String {
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v[1..v.len() - 1].to_string()
        } else {
            v.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_execution_basic() {
        let tool = ToolExecutionComponent::new("read", "file.txt");
        let lines = tool.render(80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_tool_execution_status() {
        let tool = ToolExecutionComponent::new("bash", "ls -la");
        
        assert_eq!(tool.status(), ToolStatus::Pending);
        
        tool.set_running();
        assert_eq!(tool.status(), ToolStatus::Running);
        
        tool.set_result("output", false);
        assert_eq!(tool.status(), ToolStatus::Completed);
    }

    #[test]
    fn test_tool_execution_expanded() {
        let tool = ToolExecutionComponent::new("edit", "file.rs");
        tool.set_expanded(true);
        tool.set_result("success", false);
        
        let lines = tool.render(80);
        assert!(lines.len() > 1); // Should have more lines when expanded
    }

    #[test]
    fn test_tool_execution_error() {
        let tool = ToolExecutionComponent::new("bash", "invalid_command");
        tool.set_result("command not found", true);

        let lines = tool.render(80);
        let joined = lines.join("\n");
        // Failed state: error-colored ✗ glyph + the result text echoed
        // (expanded path includes the `❌ command not found` result line).
        assert!(joined.contains("command not found"), "result text missing: {joined}");
        assert!(joined.contains('✗'), "error glyph missing: {joined}");
    }

    /// The header summarizes the args JSON into a compact signature instead
    /// of dumping the raw `{"path":"..."}` blob. Each builtin tool maps its
    /// key fields to a one-liner.
    #[test]
    fn test_tool_header_args_summary() {
        // `read` → just the path.
        let t = ToolExecutionComponent::new("read", r#"{"path":"src/foo.rs"}"#);
        let joined = t.render(80).join("\n");
        let plain = crate::ansi::strip_ansi(&joined);
        assert!(plain.contains("READ"), "tool label uppercase: {plain}");
        assert!(plain.contains("src/foo.rs"), "path in summary: {plain}");
        // The raw JSON must NOT leak into the header.
        assert!(!plain.contains("\"path\""), "raw JSON leaked: {plain}");

        // `grep` → pattern · path (no path field → just pattern).
        let t = ToolExecutionComponent::new("grep", r#"{"pattern":"TODO","path":"src"}"#);
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("TODO · src"), "grep summary: {plain}");

        // `edit` with multiple edits → `(N edits)` suffix.
        let t = ToolExecutionComponent::new("edit",
            r#"{"path":"a.rs","edits":[{"oldText":"x"},{"oldText":"y"}]}"#);
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("a.rs (2 edits)"), "edit count: {plain}");
    }

    /// Unknown tools fall back to the first string-valued arg field rather
    /// than showing nothing — keeps the header useful for plugin tools whose
    /// arg shapes the render crate doesn't know.
    #[test]
    fn test_tool_header_unknown_tool_falls_back() {
        let t = ToolExecutionComponent::new("mytool", r#"{"query":"hai"}"#);
        let plain = crate::ansi::strip_ansi(&t.render(80).join("\n"));
        assert!(plain.contains("hai"), "unknown tool summary: {plain}");
    }

    /// The collapsed diff preview caps at DIFF_PREVIEW_LINES and teases the
    /// remaining count; expanding lifts the cap to DIFF_LINE_CAP.
    #[test]
    fn test_tool_diff_preview_then_expand() {
        // Build a diff with 12 changed lines — over the 6-line preview, under
        // the 40-line expanded cap.
        let mut diff = String::new();
        for i in 1..=6 {
            diff.push_str(&format!("-{} old line {}\n", i, i));
            diff.push_str(&format!("+{} new line {}\n", i, i));
        }
        let t = ToolExecutionComponent::new("edit", r#"{"path":"f"}"#);
        t.set_result("ok", false);
        t.set_diff(crate::diff::render_diff(&diff, 80));

        let collapsed = t.render(80);
        // Preview cap = 6 lines + 1 header + 1 result + 1 hint.
        let diff_body = collapsed.iter().filter(|l| {
            let p = crate::ansi::strip_ansi(l);
            p.trim_start().starts_with('-') || p.trim_start().starts_with('+')
        }).count();
        assert_eq!(diff_body, 6, "collapsed preview should cap at 6: {collapsed:?}");
        let hint = collapsed.iter().map(|l| crate::ansi::strip_ansi(l))
            .find(|l| l.contains("more diff lines"));
        assert!(hint.is_some(), "preview hint missing");
        assert!(hint.as_ref().unwrap().contains("Ctrl+T to expand"), "hint text: {hint:?}");

        t.set_expanded(true);
        let expanded = t.render(80);
        let diff_body = expanded.iter().filter(|l| {
            let p = crate::ansi::strip_ansi(l);
            p.trim_start().starts_with('-') || p.trim_start().starts_with('+')
        }).count();
        assert_eq!(diff_body, 12, "expanded should show all 12: {expanded:?}");
    }
}