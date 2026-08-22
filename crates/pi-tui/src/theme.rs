//! Theme system for TUI customization.
//!
//! Provides color schemes and styling options.

use std::sync::Mutex;

/// Theme colors.
#[derive(Debug, Clone)]
pub struct ThemeColors {
    /// Primary text color.
    pub text: Color,
    /// Muted/dimmed text color.
    pub muted: Color,
    /// Accent/highlight color.
    pub accent: Color,
    /// Error color.
    pub error: Color,
    /// Success color.
    pub success: Color,
    /// Warning color.
    pub warning: Color,
    /// Info color.
    pub info: Color,
    /// Background color.
    pub background: Color,
    /// Primary surface color.
    pub surface: Color,
    /// Border color.
    pub border: Color,
    /// Selection background color.
    pub selection: Color,
    /// Cursor color.
    pub cursor: Color,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            text: Color::Default,
            muted: Color::Ansi256(240),
            accent: Color::Ansi256(39),  // Bright blue
            error: Color::Ansi256(196),  // Red
            success: Color::Ansi256(46), // Green
            warning: Color::Ansi256(226), // Yellow
            info: Color::Ansi256(81),    // Cyan
            background: Color::Default,
            surface: Color::Ansi256(235),
            border: Color::Ansi256(238),
            selection: Color::Ansi256(24),
            cursor: Color::Ansi256(81),
        }
    }
}

/// Color representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Default terminal color.
    Default,
    /// ANSI 16-color palette.
    Ansi16(u8),
    /// ANSI 256-color palette.
    Ansi256(u8),
    /// RGB true color.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Convert to ANSI foreground escape sequence.
    pub fn to_fg(&self) -> String {
        match self {
            Color::Default => "\x1b[39m".to_string(),
            Color::Ansi16(c) => format!("\x1b[{}m", c),
            Color::Ansi256(c) => format!("\x1b[38;5;{}m", c),
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{};{};{}m", r, g, b),
        }
    }

    /// Convert to ANSI background escape sequence.
    pub fn to_bg(&self) -> String {
        match self {
            Color::Default => "\x1b[49m".to_string(),
            Color::Ansi16(c) => format!("\x1b[{}m", c + 10),
            Color::Ansi256(c) => format!("\x1b[48;5;{}m", c),
            Color::Rgb(r, g, b) => format!("\x1b[48;2;{};{};{}m", r, g, b),
        }
    }

    /// Apply color to text (foreground).
    pub fn fg(&self, text: &str) -> String {
        format!("{}{}\x1b[0m", self.to_fg(), text)
    }

    /// Apply color to text (background).
    pub fn bg(&self, text: &str) -> String {
        format!("{}{}\x1b[0m", self.to_bg(), text)
    }
}

/// Theme settings.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Color scheme.
    pub colors: ThemeColors,
    /// Border style.
    pub border_style: BorderStyle,
    /// Corner style.
    pub corner_style: CornerStyle,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            colors: ThemeColors::default(),
            border_style: BorderStyle::Rounded,
            corner_style: CornerStyle::Rounded,
        }
    }
}

/// Border style for boxes.
#[derive(Debug, Clone, Copy, Default)]
pub enum BorderStyle {
    #[default]
    Rounded,
    Sharp,
    Double,
    Thick,
    None,
}

impl BorderStyle {
    /// Get the border characters.
    pub fn chars(&self) -> BorderChars {
        match self {
            BorderStyle::Rounded => BorderChars {
                top_left: "╭",
                top_right: "╮",
                bottom_left: "╰",
                bottom_right: "╯",
                horizontal: "─",
                vertical: "│",
            },
            BorderStyle::Sharp => BorderChars {
                top_left: "┌",
                top_right: "┐",
                bottom_left: "└",
                bottom_right: "┘",
                horizontal: "─",
                vertical: "│",
            },
            BorderStyle::Double => BorderChars {
                top_left: "╔",
                top_right: "╗",
                bottom_left: "╚",
                bottom_right: "╝",
                horizontal: "═",
                vertical: "║",
            },
            BorderStyle::Thick => BorderChars {
                top_left: "┏",
                top_right: "┓",
                bottom_left: "┗",
                bottom_right: "┛",
                horizontal: "━",
                vertical: "┃",
            },
            BorderStyle::None => BorderChars {
                top_left: " ",
                top_right: " ",
                bottom_left: " ",
                bottom_right: " ",
                horizontal: " ",
                vertical: " ",
            },
        }
    }
}

/// Border characters.
pub struct BorderChars {
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
}

/// Corner style.
#[derive(Debug, Clone, Copy, Default)]
pub enum CornerStyle {
    #[default]
    Rounded,
    Sharp,
}

/// Global theme manager.
pub struct ThemeManager {
    current: Mutex<Theme>,
}

impl ThemeManager {
    /// Create a new theme manager.
    pub fn new() -> Self {
        Self {
            current: Mutex::new(Theme::default()),
        }
    }

    /// Get the current theme.
    pub fn get(&self) -> Theme {
        self.current.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Set the theme.
    pub fn set(&self, theme: Theme) {
        if let Ok(mut current) = self.current.lock() {
            *current = theme;
        }
    }

    /// Apply a preset theme.
    pub fn apply_preset(&self, preset: ThemePreset) {
        let theme = match preset {
            ThemePreset::Dark => Theme::default(),
            ThemePreset::Light => Theme {
                colors: ThemeColors {
                    text: Color::Default,
                    muted: Color::Ansi256(244),
                    accent: Color::Ansi256(27),
                    error: Color::Ansi256(124),
                    success: Color::Ansi256(34),
                    warning: Color::Ansi256(178),
                    info: Color::Ansi256(31),
                    background: Color::Default,
                    surface: Color::Ansi256(254),
                    border: Color::Ansi256(249),
                    selection: Color::Ansi256(153),
                    cursor: Color::Ansi256(31),
                },
                ..Default::default()
            },
            ThemePreset::Monochrome => Theme {
                colors: ThemeColors {
                    text: Color::Default,
                    muted: Color::Ansi256(244),
                    accent: Color::Ansi256(15),
                    error: Color::Ansi256(9),
                    success: Color::Ansi256(15),
                    warning: Color::Ansi256(15),
                    info: Color::Ansi256(15),
                    background: Color::Default,
                    surface: Color::Ansi256(236),
                    border: Color::Ansi256(244),
                    selection: Color::Ansi256(244),
                    cursor: Color::Ansi256(15),
                },
                ..Default::default()
            },
        };
        self.set(theme);
    }
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Theme presets.
#[derive(Debug, Clone, Copy)]
pub enum ThemePreset {
    Dark,
    Light,
    Monochrome,
}

/// Global theme helper functions.
pub fn theme() -> Theme {
    THEME_MANAGER.get_or_init(ThemeManager::new).get()
}

/// Global theme manager.
static THEME_MANAGER: std::sync::OnceLock<ThemeManager> = std::sync::OnceLock::new();

/// Apply color from theme.
pub fn themed_text(text: &str, color_fn: fn(&ThemeColors) -> Color) -> String {
    let theme = theme();
    let color = color_fn(&theme.colors);
    color.fg(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_ansi() {
        let color = Color::Ansi256(196);
        let text = color.fg("Hello");
        assert!(text.contains("Hello"));
    }

    #[test]
    fn test_theme_manager() {
        let manager = ThemeManager::new();
        manager.apply_preset(ThemePreset::Dark);
        let theme = manager.get();
        assert!(matches!(theme.colors.accent, Color::Ansi256(39)));
    }
}