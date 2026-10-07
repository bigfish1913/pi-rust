//! Passive top-right monitor; the plugin owns measurements and command state.
use super::*;
use rpi_tui::ansi::visible_width;
pub(super) const KEY: &str = "rpi.run-stats";
pub(super) struct Panel(pub rpi_extensions::ExtensionStatusMailbox);
fn number(v: &serde_json::Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}
fn metric(v: &serde_json::Value, key: &str, suffix: &str) -> String {
    v[key]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| format!("{n:.2}{suffix}"))
        .unwrap_or_else(|| "—".into())
}
pub(super) fn active(mailbox: &rpi_extensions::ExtensionStatusMailbox) -> bool {
    mailbox
        .get(KEY)
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["version"] == 1)
}
impl Component for Panel {
    fn render(&self, width: usize) -> Vec<String> {
        let Some(v) = self
            .0
            .get(KEY)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(|v| v["version"] == 1)
        else {
            return vec![];
        };
        if width < 4 {
            return vec![];
        }
        let rounds = number(&v, "rounds");
        let steps = number(&v, "steps");
        let status = if v["running"] == true {
            "运行中"
        } else {
            "就绪"
        };
        let rows = vec![
            format!("监控 · {status}          /stats off"),
            format!("{rounds} 轮  {steps} 步  {} tok/s", metric(&v, "speed", "")),
            format!(
                "首 token {}  耗时 {}",
                metric(&v, "ttft", "s"),
                metric(&v, "latency", "s")
            ),
            format!(
                "TPM {}   等待 {}",
                number(&v, "tpm"),
                metric(&v, "waiting", "s")
            ),
            format!(
                "输入 {}  输出 {}",
                number(&v, "input"),
                number(&v, "output")
            ),
            format!(
                "缓存读 {}  写 {}",
                number(&v, "cacheRead"),
                number(&v, "cacheWrite")
            ),
            format!(
                "错误/中止 {}  USD {}",
                number(&v, "errors"),
                metric(&v, "cost", "")
            ),
        ];
        let colors = current_theme().colors;
        let inner = width - 4;
        let mut lines = vec![colors.dim.fg(&format!("┌{}┐", "─".repeat(width - 2)))];
        for row in rows {
            let text = rpi_tui::utils::truncate_to_width(&row, inner, "");
            lines.push(colors.dim.fg(&format!(
                "│ {text}{} │",
                " ".repeat(inner.saturating_sub(visible_width(&text)))
            )));
        }
        lines.push(colors.dim.fg(&format!("└{}┘", "─".repeat(width - 2))));
        lines
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn invalidate(&self) {}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_validates_payload_and_fits_unicode_at_all_widths() {
        let m = rpi_extensions::ExtensionStatusMailbox::new();
        let p = Panel(m.clone());
        assert!(p.render(40).is_empty());
        m.set(KEY,&serde_json::json!({"version":1,"rounds":1,"steps":87,"speed":252,"ttft":0.5,"tpm":1000}).to_string());
        assert!(active(&m));
        assert!(p
            .render(42)
            .join("\n")
            .contains("1 轮  87 步  252.00 tok/s"));
        for width in [1, 4, 12, 30, 42] {
            assert!(p.render(width).iter().all(|r| visible_width(r) <= width));
        }
        m.set(KEY, "broken");
        assert!(!active(&m));
        assert!(p.render(42).is_empty());
        m.set(KEY, r#"{"version":2}"#);
        assert!(p.render(42).is_empty());
    }
}
