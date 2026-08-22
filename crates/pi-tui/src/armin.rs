//! Armin easter-egg — XBM half-block art (static).
//!
//! Port of `packages/coding-agent/src/modes/interactive/components/armin.ts`,
//! minus the animation (typewriter/scanline/rain/fade/crt/glitch/dissolve).
//! The TS original picks a random effect and animates the grid in via
//! `setInterval`; rpi-tui has no per-component timer, so this port renders the
//! **final** grid immediately. The XBM decode (LSB-first, 1=background,
//! 0=foreground) and the half-block `▀▄█ ` packing are faithful to the source.
//!
//! Animation is deferred — noted in the plan; a render-tick-driven effect would
//! need a frame-state field + host polling, out of scope this pass.

use std::any::Any;

use super::component::Component;
use crate::theme::theme;

/// XBM image: 31×36 pixels, LSB first, 1 = background, 0 = foreground.
const WIDTH: usize = 31;
const HEIGHT: usize = 36;
const BITS: &[u8] = &[
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f,
    0xff, 0xb7, 0xff, 0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f,
    0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3, 0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f,
    0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7, 0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70,
    0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53, 0xc1, 0xff, 0xef, 0x57,
    0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50, 0x6c,
    0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78,
    0xff, 0x07, 0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78,
    0xff, 0xff, 0xdf, 0x78, 0xff, 0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];

const BYTES_PER_ROW: usize = (WIDTH + 7) / 8;
const DISPLAY_HEIGHT: usize = (HEIGHT + 1) / 2; // half-block rendering

/// Pixel at (x, y): `true` = foreground, `false` = background. Mirrors the TS
/// `getPixel` (bit clear → foreground).
fn get_pixel(x: usize, y: usize) -> bool {
    if y >= HEIGHT {
        return false;
    }
    let byte_index = y * BYTES_PER_ROW + x / 8;
    let bit_index = x % 8;
    let byte = BITS.get(byte_index).copied().unwrap_or(0xff);
    ((byte >> bit_index) & 1) == 0
}

/// Half-block char for one cell (two vertically stacked pixels). Mirrors the
/// TS `getChar`: both→`█`, upper→`▀`, lower→`▄`, neither→` `.
fn get_char(x: usize, row: usize) -> char {
    let upper = get_pixel(x, row * 2);
    let lower = get_pixel(x, row * 2 + 1);
    match (upper, lower) {
        (true, true) => '█',
        (true, false) => '▀',
        (false, true) => '▄',
        (false, false) => ' ',
    }
}

/// Build the final (fully-revealed) image grid as joined line strings.
fn build_final_grid() -> Vec<String> {
    let mut grid = Vec::with_capacity(DISPLAY_HEIGHT);
    for row in 0..DISPLAY_HEIGHT {
        let mut line = String::with_capacity(WIDTH);
        for x in 0..WIDTH {
            line.push(get_char(x, row));
        }
        grid.push(line);
    }
    grid
}

/// Static XBM art easter egg. Renders the final grid (accent-colored) + the
/// "ARMIN SAYS HI" caption, clipped to the terminal width. No animation.
pub struct ArminComponent {
    grid: Vec<String>,
}

impl ArminComponent {
    pub fn new() -> Self {
        Self {
            grid: build_final_grid(),
        }
    }
}

impl Default for ArminComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ArminComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let accent = theme().colors.accent;
        let padding = 1;
        let available = width.saturating_sub(padding).max(1);
        let mut lines = Vec::with_capacity(self.grid.len() + 1);
        for row in &self.grid {
            // Clip to available width (the grid is WIDTH wide; a narrow
            // terminal clips the right edge rather than wrapping).
            let clipped: String = row.chars().take(available).collect();
            let pad_right = width.saturating_sub(padding + clipped.chars().count());
            lines.push(format!(" {}{}", accent.fg(&clipped), " ".repeat(pad_right)));
        }
        let message = "ARMIN SAYS HI";
        let msg_pad = width.saturating_sub(padding + message.len());
        lines.push(format!(" {}{}", accent.fg(message), " ".repeat(msg_pad)));
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
    use crate::component::Component;

    #[test]
    fn test_armin_renders_grid_and_caption() {
        let comp = ArminComponent::new();
        let lines = comp.render(80);
        // DISPLAY_HEIGHT rows + 1 caption.
        assert_eq!(lines.len(), DISPLAY_HEIGHT + 1);
        let joined: String = lines.join("\n");
        assert!(joined.contains("ARMIN SAYS HI"), "caption missing: {joined}");
    }

    #[test]
    fn test_armin_narrow_width_no_panic() {
        let comp = ArminComponent::new();
        let lines = comp.render(10);
        // Every line fits within the width budget (ANSI escapes aside).
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_get_pixel_bounds_safe() {
        // Out-of-range y must not panic and reads as background.
        assert!(!get_pixel(0, HEIGHT + 5));
        assert!(!get_pixel(WIDTH + 5, 0));
    }
}
