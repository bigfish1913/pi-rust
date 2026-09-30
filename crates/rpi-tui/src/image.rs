//! Image component for terminal display.
//!
//! Displays images using Kitty or iTerm2 protocols.

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use super::terminal_image::{
    allocate_image_id, get_capabilities, get_image_dimensions, render_image, ImageProtocol,
    ImageRenderOptions,
};
use crate::utils::visible_width;

/// Image theme.
#[derive(Debug, Clone)]
pub struct ImageTheme {
    /// Placeholder background color (ANSI escape).
    pub placeholder_bg: &'static str,
    /// Placeholder border color (ANSI escape).
    pub border_color: &'static str,
    /// Alt text color function.
    pub alt_text_color: fn(&str) -> String,
}

impl Default for ImageTheme {
    fn default() -> Self {
        Self {
            placeholder_bg: "\x1b[48;5;238m", // Dark gray background
            border_color: "\x1b[38;5;244m",   // Gray border
            alt_text_color: |s| format!("\x1b[38;5;250m{}\x1b[0m", s),
        }
    }
}

/// Image options.
#[derive(Debug, Clone)]
pub struct ImageOptions {
    /// Width in cells (None = auto).
    pub width: Option<u16>,
    /// Height in cells (None = auto).
    pub height: Option<u16>,
    /// Preserve aspect ratio.
    pub preserve_aspect: bool,
    /// Alt text for fallback.
    pub alt_text: Option<String>,
    /// Title for the image.
    pub title: Option<String>,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            preserve_aspect: true,
            alt_text: None,
            title: None,
        }
    }
}

/// Image component.
pub struct Image {
    data: Mutex<Option<Vec<u8>>>,
    url: Mutex<Option<String>>,
    options: ImageOptions,
    theme: ImageTheme,
    image_id: Mutex<Option<u32>>,
    cached_render: Mutex<Option<String>>,
}

impl Image {
    /// Create a new empty image.
    pub fn new() -> Self {
        Self {
            data: Mutex::new(None),
            url: Mutex::new(None),
            options: ImageOptions::default(),
            theme: ImageTheme::default(),
            image_id: Mutex::new(None),
            cached_render: Mutex::new(None),
        }
    }

    /// Create an image from data.
    pub fn from_data(data: Vec<u8>, options: ImageOptions) -> Self {
        let id = allocate_image_id();
        Self {
            data: Mutex::new(Some(data)),
            url: Mutex::new(None),
            options,
            theme: ImageTheme::default(),
            image_id: Mutex::new(Some(id)),
            cached_render: Mutex::new(None),
        }
    }

    /// Create an image from a URL.
    pub fn from_url(url: &str, options: ImageOptions) -> Self {
        Self {
            data: Mutex::new(None),
            url: Mutex::new(Some(url.to_string())),
            options,
            theme: ImageTheme::default(),
            image_id: Mutex::new(None),
            cached_render: Mutex::new(None),
        }
    }

    /// Set image data.
    pub fn set_data(&self, data: Vec<u8>) {
        if let Ok(mut d) = self.data.lock() {
            *d = Some(data);
        }
        if let Ok(mut id) = self.image_id.lock() {
            *id = Some(allocate_image_id());
        }
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = None;
        }
    }

    /// Set image URL.
    pub fn set_url(&self, url: &str) {
        if let Ok(mut u) = self.url.lock() {
            *u = Some(url.to_string());
        }
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = None;
        }
    }

    /// Set options.
    pub fn set_options(&mut self, options: ImageOptions) {
        self.options = options;
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = None;
        }
    }

    /// Set alt text.
    pub fn set_alt_text(&mut self, alt_text: &str) {
        self.options.alt_text = Some(alt_text.to_string());
    }

    /// Set theme.
    pub fn set_theme(&mut self, theme: ImageTheme) {
        self.theme = theme;
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = None;
        }
    }

    /// Check if image is loaded.
    pub fn is_loaded(&self) -> bool {
        self.data.lock().map(|d| d.is_some()).unwrap_or(false)
            || self.url.lock().map(|u| u.is_some()).unwrap_or(false)
    }

    /// Get image dimensions if available.
    pub fn get_dimensions(&self) -> Option<(u32, u32)> {
        let data = self.data.lock().ok()?;
        let d = data.as_ref()?;
        let dims = get_image_dimensions(d)?;
        Some((dims.width, dims.height))
    }

    /// Render the image.
    fn render_image(&self, width: usize) -> String {
        let caps = get_capabilities();

        // Check if we have cached render
        if let Ok(cached) = self.cached_render.lock() {
            if cached.is_some() {
                return cached.clone().unwrap();
            }
        }

        // Calculate dimensions
        let (img_width, img_height) = if let Some((w, h)) = self.get_dimensions() {
            // Calculate cell dimensions
            let cell_dims = super::terminal_image::get_cell_dimensions();
            let cell_width = self.options.width.unwrap_or_else(|| {
                ((w as f32 / cell_dims.width as f32).ceil() as u16).min(width as u16)
            });
            let cell_height = self
                .options
                .height
                .unwrap_or_else(|| (h as f32 / cell_dims.height as f32).ceil() as u16);
            (cell_width, cell_height)
        } else {
            (
                self.options.width.unwrap_or(width as u16),
                self.options.height.unwrap_or(5),
            )
        };

        // Render based on protocol
        let result = if caps.protocol == ImageProtocol::None {
            // Fallback: render placeholder
            self.render_placeholder(img_width as usize, img_height as usize, width)
        } else if let Ok(data) = self.data.lock() {
            if let Some(ref img_data) = *data {
                let id = self.image_id.lock().ok().and_then(|id| *id);
                let options = ImageRenderOptions {
                    width: Some(img_width),
                    height: Some(img_height),
                    preserve_aspect: self.options.preserve_aspect,
                    x: 0,
                    y: 0,
                    z_index: None,
                    id,
                };
                render_image(img_data, &options)
            } else {
                self.render_placeholder(img_width as usize, img_height as usize, width)
            }
        } else {
            self.render_placeholder(img_width as usize, img_height as usize, width)
        };

        // Cache the result
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = Some(result.clone());
        }

        result
    }

    /// Render a placeholder when image can't be displayed.
    fn render_placeholder(&self, img_width: usize, img_height: usize, max_width: usize) -> String {
        let width = img_width.min(max_width);
        let height = img_height.max(3);

        let border_h = "─".repeat(width.saturating_sub(2));
        let inner_width = width.saturating_sub(2);

        let mut lines = Vec::new();

        // Top border
        lines.push(format!("{}╭{}╮\x1b[0m", self.theme.border_color, border_h));

        // Content
        let alt_text = self.options.alt_text.as_deref().unwrap_or("[Image]");
        let title = self.options.title.as_deref().unwrap_or("");

        // Calculate content lines
        let content_height = height.saturating_sub(2);
        let alt_text_line = (self.theme.alt_text_color)(alt_text);
        let title_line = if !title.is_empty() {
            Some((self.theme.alt_text_color)(title))
        } else {
            None
        };

        for i in 0..content_height {
            let content = if i == 0 {
                if let Some(ref t) = title_line {
                    t.clone()
                } else {
                    alt_text_line.clone()
                }
            } else if i == 1 && title_line.is_some() {
                alt_text_line.clone()
            } else {
                " ".repeat(inner_width)
            };

            let content_padded = if visible_width(&content) < inner_width {
                format!(
                    "{}{}",
                    content,
                    " ".repeat(inner_width - visible_width(&content))
                )
            } else {
                crate::utils::truncate_to_width(&content, inner_width, "")
            };

            lines.push(format!(
                "{}│{}{}\x1b[0m│\x1b[0m",
                self.theme.border_color, self.theme.placeholder_bg, content_padded
            ));
        }

        // Bottom border
        lines.push(format!("{}╰{}╯\x1b[0m", self.theme.border_color, border_h));

        lines.join("\n")
    }
}

impl Default for Image {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Image {
    fn render(&self, width: usize) -> Vec<String> {
        let rendered = self.render_image(width);
        rendered.split('\n').map(|s| s.to_string()).collect()
    }

    fn invalidate(&self) {
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = None;
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_new() {
        let image = Image::new();
        assert!(!image.is_loaded());
    }

    #[test]
    fn test_image_from_data() {
        let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG header start
        let image = Image::from_data(data, ImageOptions::default());
        assert!(image.is_loaded());
    }

    #[test]
    fn test_image_placeholder() {
        let image = Image::new();
        let lines = image.render(20);
        assert!(!lines.is_empty());
    }
}
