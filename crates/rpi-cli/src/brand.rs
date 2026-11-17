//! 启动时的品牌标志：一枚会自转的 3D 齿轮徽章。
//!
//! 形状由 [`shape`] 描述，[`render`] 负责光栅化，[`spin`] 提供片头动画的时钟。
//! 输出是纯 ASCII（单宽字符 + 主题色），所以纯文本终端、远程会话、
//! 没有图片协议的终端里都能显示。

pub(crate) mod render;
mod shape;
mod spin;

use rpi_tui::Component;

pub use spin::{animating, replay, set_enabled};

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
        render::lockup(spin::angle())
    }

    fn invalidate(&self) {
        // Nothing is cached.
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockup_fits_the_welcome_header() {
        let rows = BrandLockup::new().render(80);
        assert_eq!(rows.len(), render::ROWS);
    }

    #[test]
    fn title_keeps_the_logo_identity_and_status_last() {
        assert_eq!(window_title(""), "@ rpi");
        assert_eq!(window_title("⟳"), "@ rpi ⟳");
        assert_eq!(window_title("↻"), "@ rpi ↻");
        assert!(!window_title("⟳").contains('\n'));
    }
}
