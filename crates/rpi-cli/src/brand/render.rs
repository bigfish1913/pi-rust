//! 启动标志：`docs/images/rpi-logo.jpg` 的线稿齿轮 + rpi，压成 24×12 的半块字符。
//!
//! 标志以文本形式内置，而不是加载 README 里的 JPEG，这样纯文本终端、远程会话、
//! 没有图片协议的终端都能显示；也不需要为灰度图再引一个解码依赖。
//!
//! 只使用 `█ ▀ ▄`（`unicode-width` 记为 1 格）和空格，因此每行宽度恒为
//! [`COLS`]，不依赖 emoji 或 East-Asian Ambiguous 的 `π` 这类字形。
//!
//! 尺寸是首屏预算定下来的：80×24 的终端里转录视口只有 17 行，欢迎页除标志
//! 之外还要 5 行（tagline、按键提示、Tools、Skills、分隔线），所以标志上限
//! 是 12 行 × 24 列——再大就要滚动，首屏看不到完整标志。
//!
//! 重新生成的步骤（改动 logo 时）：
//! 1. 把 `docs/images/rpi-logo.jpg` 缩到 160×160；
//! 2. 以灰度 140 为阈值二值化，裁到墨迹的外接方框（126×128）；
//! 3. 铺 24 列 × 12 行字符格，每格上下两个半块，4×4 子采样求墨迹覆盖率；
//! 4. 覆盖率 ≥ 48% 记为墨点，两半都有 → `█`，只有上半 → `▀`，只有下半 → `▄`。

use rpi_tui::theme as current_theme;

/// 输出列数。
pub const COLS: usize = 24;
/// 输出行数，也就是欢迎页里占的终端高度。
pub const ROWS: usize = 12;

/// 标志本体，顺序即自上而下的行；每行不超过 [`COLS`] 列。
const LOGO: [&str; ROWS] = [
    "      ▄▄  █▀▀█  ▄▄",
    "     █  ▀▀    ▀▀  █",
    "  ▄▄▄▀ ▄▀▀▀  ▀▀▀▄ ▀▄▄▄",
    " █   ▄▀          ▀▄   █",
    "  █ ▄     ▄▄▄▄     ▄ █",
    "▄▀▀ █   ▄▀ ▄▄ ▀▄   █ ▀▀▄",
    "█▄▄ █     █▄▄▀ ▀ ▄ █ ▄▄█",
    "  █ █ █▀▀ █ ▄▄▀  ▄ █ █",
    " █   ▀█   █ █    █▀   █",
    "  ▀▀▀▄  ▄▄█ █▄▄▄  ▄▀▀▀",
    "     █  ▄▄▄  ▄▄▄  █",
    "      ▀▀  ▀▄▄▀  ▀▀",
];

/// 把标志渲染成 [`ROWS`] 行、每行 [`COLS`] 个单宽字符。
///
/// 用主题的默认前景色（`Color::Default`，dark 主题下是白、light 主题下是黑），
/// 不占用 accent 色，标志就不会和正文的高亮抢同一种颜色。
pub fn lockup() -> Vec<String> {
    let colors = current_theme().colors;
    LOGO.iter()
        .map(|row| {
            let padded = format!(
                "{row}{}",
                " ".repeat(COLS.saturating_sub(visible_cols(row)))
            );
            colors.text.fg(&padded)
        })
        .collect()
}

/// 标志行的显示宽度。标志里只有半块字符和空格，都是 1 格，但用可见宽度计算
/// 才能保证以后换字符时右边界还对齐。
fn visible_cols(row: &str) -> usize {
    rpi_tui::ansi::visible_width(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::ansi::{strip_ansi, visible_width};

    #[test]
    fn lockup_has_stable_geometry() {
        let rows = lockup();
        assert_eq!(rows.len(), ROWS);
        for row in &rows {
            assert_eq!(visible_width(row), COLS);
        }
    }

    #[test]
    fn lockup_is_a_line_drawing_not_a_solid_blob() {
        let art: String = lockup().iter().map(|row| strip_ansi(row)).collect();
        // 线稿：齿轮环、内圆和 r/p/i 笔画之间必须留白，实心盘不会有空格。
        let blank = art.chars().filter(|c| *c == ' ').count();
        let total = art.chars().count();
        assert!(
            blank * 100 / total > 40,
            "标志应当是线稿，空白只占 {}%",
            blank * 100 / total
        );
        for c in ['█', '▀', '▄'] {
            assert!(art.contains(c), "标志缺少半块字符 {c:?}");
        }
    }
}
