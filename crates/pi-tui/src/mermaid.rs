//! Compact Mermaid rendering for terminal TUI.
//!
//! Terminals do not have a portable Mermaid canvas. This component renders the
//! useful structure of common flowcharts and sequence diagrams as aligned
//! Unicode text, and falls back to a readable source panel for unsupported
//! Mermaid constructs.

use std::any::Any;

use super::component::Component;
use super::theme::theme;
use super::utils::truncate_to_width;

pub struct Mermaid {
    source: String,
    padding_x: usize,
}

impl Mermaid {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            padding_x: 1,
        }
    }

    fn body_lines(&self, width: usize) -> Vec<String> {
        let source = self.source.trim();
        let mut lines = Vec::new();
        let first = source.lines().next().unwrap_or_default().trim();
        let is_sequence = first.eq_ignore_ascii_case("sequencediagram");
        let is_flow = first.eq_ignore_ascii_case("graph")
            || first.to_ascii_lowercase().starts_with("graph ")
            || first.eq_ignore_ascii_case("flowchart")
            || first.to_ascii_lowercase().starts_with("flowchart ");

        if is_sequence {
            lines.extend(self.render_sequence(source));
        } else if is_flow {
            lines.extend(self.render_flow(source));
        }

        if lines.is_empty() {
            lines = source
                .lines()
                .skip_while(|line| {
                    let trimmed = line.trim().to_ascii_lowercase();
                    trimmed == "mermaid"
                        || trimmed == "graph"
                        || trimmed.starts_with("graph ")
                        || trimmed == "flowchart"
                        || trimmed.starts_with("flowchart ")
                        || trimmed == "sequencediagram"
                })
                .map(|line| format!("  {line}"))
                .collect();
        }

        let max_width = width.saturating_sub(self.padding_x * 2).max(1);
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, max_width, "…"))
            .collect()
    }

    fn render_flow(&self, source: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in source.lines().skip(1) {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("%%") {
                continue;
            }
            let (connector, arrow) = if line.contains("-.->") {
                ("-.->", "╌╌▶")
            } else if line.contains("==>") {
                ("==>", "══▶")
            } else if line.contains("-->") {
                ("-->", "──▶")
            } else if line.contains("---") {
                ("---", "───")
            } else {
                continue;
            };
            let Some((left, right)) = line.split_once(connector) else {
                continue;
            };
            let left = node_label(left);
            let right = node_label(right);
            if left.is_empty() || right.is_empty() {
                continue;
            }
            let label = right
                .split_once(':')
                .map(|(_, label)| label.trim().trim_matches('"'))
                .filter(|label| !label.is_empty());
            let right = right
                .split_once(':')
                .map(|(node, _)| node)
                .unwrap_or(&right);
            out.push(format!("  {left} {arrow} {right}"));
            if let Some(label) = label {
                out.push(format!("       {label}"));
            }
        }
        out
    }

    fn render_sequence(&self, source: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in source.lines().skip(1) {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("%%") || line.starts_with("participant ") {
                continue;
            }
            let Some((left, rest)) = line.split_once("->>") else {
                continue;
            };
            let Some((right, message)) = rest.split_once(':') else {
                continue;
            };
            out.push(format!(
                "  {} ──▶ {}: {}",
                left.trim(),
                right.trim(),
                message.trim()
            ));
        }
        out
    }
}

fn node_label(raw: &str) -> String {
    let raw = raw.trim();
    let Some(open) = raw.find(['(', '[', '{']) else {
        return raw.trim_matches(';').trim().to_string();
    };
    let close = match raw.as_bytes().get(open) {
        Some(b'(') => ')',
        Some(b'[') => ']',
        Some(b'{') => '}',
        _ => return raw.to_string(),
    };
    raw[open + 1..]
        .find(close)
        .map(|end| raw[open + 1..open + 1 + end].trim_matches('"').to_string())
        .unwrap_or_else(|| raw.to_string())
}

impl Component for Mermaid {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let panel_width = width.max(1).saturating_sub(self.padding_x * 2).max(1);
        let mut lines = Vec::new();
        lines.push(format!(
            "{}{}",
            " ".repeat(self.padding_x),
            colors.md_code_block_border.fg("mermaid")
        ));
        lines.extend(
            self.body_lines(panel_width)
                .into_iter()
                .map(|line| format!("{}{}", " ".repeat(self.padding_x), line)),
        );
        lines
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strip_ansi;

    #[test]
    fn renders_flowchart_edges_as_terminal_arrows() {
        let output = strip_ansi(
            &Mermaid::new("flowchart LR\nA[Start] --> B{Done?}")
                .render(60)
                .join("\n"),
        );
        assert!(output.contains("Start ──▶ Done?"));
    }

    #[test]
    fn renders_sequence_messages() {
        let output = strip_ansi(
            &Mermaid::new("sequenceDiagram\nAlice->>Bob: Hello")
                .render(60)
                .join("\n"),
        );
        assert!(output.contains("Alice ──▶ Bob: Hello"));
    }

    #[test]
    fn falls_back_to_source_for_unknown_diagrams() {
        let output = strip_ansi(&Mermaid::new("pie\n  \"A\" : 50").render(60).join("\n"));
        assert!(output.contains("\"A\" : 50"));
    }
}
