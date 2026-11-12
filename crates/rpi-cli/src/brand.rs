//! The startup brand lockup, rendered from the README logo with the same
//! black-and-white ASCII palette as https://neucrack.com/tools/ascii.
//!
//! The logo is kept as text rather than loading the README JPEG so it works in
//! plain terminals, remote sessions, and terminals without image protocols.
//! Its rows use fixed single-cell geometry and are scaled down for the
//! terminal's character aspect ratio.

use rpi_tui::{ansi::visible_width, theme as current_theme, Component};

/// A compact one-line approximation used in the terminal window/tab title.
/// The full gear is rendered by [`render_lockup`].
pub const MARK: &str = "⚙";

const LOGO_ROWS: [&str; 25] = [
    "                                                  ",
    "                                                  ",
    "                      -+++=                       ",
    "               -+:    @   *=    +=.               ",
    "             #*: +*+++=   .++++#:.=#-             ",
    "             *+     .:-====-.     .%.             ",
    "        .:. -#=  -**+-:...:-=**+. .*+  .:         ",
    "       :%-=++  +#-             :*#: :*+-+*        ",
    "      .@-    -%-                 .**     %+       ",
    "       .+#  =%.      .:   .:       =%  -#:        ",
    "        ++ :@.      .*%*++#%-       ** .@         ",
    "     =++*. *+      =%: -+=: +%      .@. =++*.     ",
    "     %.    %-     .@. @=.:%= *+      @-    #-     ",
    "     #*+-  #=        .@+:-%- #+ .-   @. :=+#:     ",
    "        *= -%  *%#+* .@:-- .**  :+  +#  @.        ",
    "        -%  *# **    .@ .#*+:   -@ -%. =#         ",
    "       %+    =%%+    .@ .@.     -@+#.   :%-       ",
    "       -#.:=- .*+    .@ .@.     -%- .=-.+*        ",
    "        .+-.-#-  .**+=@ .@--=**-  .*+.:==         ",
    "             -#     .:-  ==-:     -#              ",
    "             **: -#+++=   .+++** .=%:             ",
    "               -+=    @.  *=   :*=:               ",
    "                      -+++=                       ",
    "                                                  ",
    "                                                  ",
];

/// The startup lockup flattened into a terminal window/tab title.
pub fn window_title(status: &str) -> String {
    if status.is_empty() {
        format!("{MARK} rpi")
    } else {
        format!("{MARK} rpi {status}")
    }
}

/// Apply the current theme to the monochrome ASCII output.
fn style_logo_row(row: &str) -> String {
    current_theme().colors.dim.fg(row)
}

fn render_lockup() -> Vec<String> {
    let width = LOGO_ROWS
        .iter()
        .map(|row| visible_width(row))
        .max()
        .unwrap_or(0);
    LOGO_ROWS
        .iter()
        .map(|row| {
            let padded = format!(
                "{row}{}",
                " ".repeat(width.saturating_sub(visible_width(row)))
            );
            style_logo_row(&padded)
        })
        .collect()
}

/// The startup logo. Add one to the welcome header.
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
        render_lockup()
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
    use rpi_tui::ansi::{strip_ansi, visible_width};

    fn plain() -> Vec<String> {
        render_lockup().iter().map(|r| strip_ansi(r)).collect()
    }

    #[test]
    fn logo_has_the_readme_lockup_shape() {
        let rows = plain();
        assert_eq!(rows.len(), 25);
        assert!(rows.iter().any(|row| row.contains("#")));
        assert!(rows.iter().any(|row| row.contains("+=")));
        assert!(rows[2].contains("-+++="));
        assert!(rows[22].contains("-+++="));
    }

    #[test]
    fn logo_rows_have_stable_terminal_geometry() {
        let rows = plain();
        let widths: Vec<usize> = rows.iter().map(|row| visible_width(row)).collect();
        assert!(widths.iter().all(|width| *width == widths[0]));
        assert_eq!(widths[0], 50);
    }

    #[test]
    fn title_keeps_the_logo_identity_and_status_last() {
        assert_eq!(window_title(""), "⚙ rpi");
        assert_eq!(window_title("⟳"), "⚙ rpi ⟳");
        assert_eq!(window_title("↻"), "⚙ rpi ↻");
        assert!(!window_title("⟳").contains('\n'));
    }
}
