//! Tool execution component for displaying tool calls.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/tool-execution.ts

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::ansi::strip_ansi;
use crate::utils::truncate_to_width;

/// Maximum diff lines rendered inline before collapsing the rest.
const DIFF_LINE_CAP: usize = 40;

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
            let prefix = if is_error { "❌ " } else { "✓ " };
            *r = Some(format!("{}{}", prefix, result));
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
        let mut lines = Vec::new();

        let name = self.name.lock().unwrap();
        let status = self.status.lock().unwrap();
        let args = self.args.lock().unwrap();
        let result = self.result.lock().unwrap();
        let expanded = self.expanded.lock().unwrap();
        let diff_lines = self.diff_lines.lock().unwrap();

        // Status indicator
        let status_icon = match *status {
            ToolStatus::Pending => "⏳",
            ToolStatus::Running => "🔄",
            ToolStatus::Completed => "✓",
            ToolStatus::Failed => "❌",
        };

        // Tool header line
        let header = format!("{} Tool: {} {}", status_icon, name,
            if *expanded { "▼" } else { "▶" });

        let mut header_line = " ".repeat(1);
        header_line.push_str(&header);
        header_line = truncate_to_width(&header_line, width, "…");
        lines.push(header_line);

        // If expanded, show args and result
        if *expanded {
            // Show args
            if !args.is_empty() {
                let args_line = format!("  Args: {}", args);
                lines.push(args_line);
            }

            // Show result if available
            if let Some(ref r) = *result {
                for line in r.lines() {
                    let result_line = format!("  {}", line);
                    lines.push(result_line);
                }
            }
        }

        // A colored diff (from `render_diff`) is always shown — the diff IS
        // the useful content for an edit; the summary header + (optional)
        // expanded args sit above it. Cap to keep large diffs readable.
        if let Some(diff) = diff_lines.as_ref() {
            let total = diff.len();
            let shown = diff.iter().take(DIFF_LINE_CAP);
            for dl in shown {
                // Strip any leading ANSI-styled padding-less body: diff lines
                // are already complete (colors + content). Indent by 2 cols.
                let body = strip_ansi(dl);
                if body.is_empty() {
                    lines.push(String::new());
                } else {
                    let indented = format!("  {}", dl);
                    lines.push(truncate_to_width(&indented, width, "…"));
                }
            }
            if total > DIFF_LINE_CAP {
                let more = total - DIFF_LINE_CAP;
                lines.push(format!("  … {} more diff lines hidden", more));
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
        assert!(lines.join("\n").contains("❌"));
    }
}