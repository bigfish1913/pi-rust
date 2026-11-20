//! 启动标志：`docs/images/rpi-logo.jpg` 那枚齿轮 + rpi 的终端版本。
//!
//! 标志由 [`render`] 的静态表格给出（32 列 × 16 行半块字符，用主题默认前景色），
//! 不做 3D 扫描、不做自转动画：欢迎页只需要一张确定、紧凑、和源图同构的图，
//! 首屏行数也不该被装饰吃掉。

pub(crate) mod render;

use rpi_tui::Component;

/// 终端窗口/标签标题里的紧凑标记。
pub const MARK: &str = "@";

/// 把标志压成一行终端窗口标题。
pub fn window_title(status: &str) -> String {
    if status.is_empty() {
        format!("{MARK} rpi")
    } else {
        format!("{MARK} rpi {status}")
    }
}

/// 启动标志。加到欢迎页 header 里。
pub struct BrandLockup;

impl BrandLockup {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrandLockup {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for BrandLockup {
    fn render(&self, _width: usize) -> Vec<String> {
        render::lockup()
    }

    fn invalidate(&self) {
        // Nothing is cached: the frame is a static table plus the theme colour.
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::ansi::{strip_ansi, visible_width};

    #[test]
    fn lockup_fits_the_welcome_header() {
        let rows = BrandLockup::new().render(80);
        assert_eq!(rows.len(), render::ROWS);
        assert_eq!(rows.len(), 16);
        assert!(rows.iter().all(|row| visible_width(row) == render::COLS));
        assert!(strip_ansi(&rows[7]).contains('█'));
    }

    #[test]
    fn title_keeps_the_logo_identity_and_status_last() {
        assert_eq!(window_title(""), "@ rpi");
        assert_eq!(window_title("⟳"), "@ rpi ⟳");
        assert_eq!(window_title("↻"), "@ rpi ↻");
        assert!(!window_title("⟳").contains('\n'));
    }
}
