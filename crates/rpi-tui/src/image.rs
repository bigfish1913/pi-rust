//! Image component for terminal display.
//!
//! Displays images using Kitty or iTerm2 protocols.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::{Component, RenderedDocument};
use super::terminal_image::{
    allocate_image_id, encode_kitty_placement, frame_holds_image, get_capabilities,
    get_image_dimensions, register_kitty_image, render_image, ImageProtocol, ImageRenderOptions,
    KittyImageMetadata,
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

/// The cell geometry a cached render was computed for.
///
/// `Image::render_image` divides the source image's pixel size by the cell size
/// to decide how many rows the placement occupies, so a cached string is only
/// valid for the geometry it was built with. Keying the cache on width alone
/// kept a stale geometry after the terminal's cell size changed (font zoom, a
/// move to a monitor with another DPI) and made old images disagree with new
/// ones in the same frame.
///
/// `uploaded` is part of the key because it changes the row's *form*, not just
/// its spelling: the frame that first shows an image has to carry the payload,
/// every later frame only places an image the terminal already holds (see
/// [`frame_holds_image`]). Caching one form under the other would either resend
/// megabytes per frame or paint an id the terminal does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    width: usize,
    cell_width: u16,
    cell_height: u16,
    protocol: ImageProtocol,
    uploaded: bool,
}

/// Image component.
pub struct Image {
    data: Mutex<Option<Vec<u8>>>,
    url: Mutex<Option<String>>,
    options: ImageOptions,
    theme: ImageTheme,
    image_id: Mutex<Option<u32>>,
    /// Rendered rows, shared with the frame being assembled.
    ///
    /// A kitty payload row is megabytes. Handing out owned rows made every
    /// repaint of the transcript copy (and re-split) those megabytes even when
    /// nothing changed, which pinned a core on sessions that read a handful of
    /// screenshots. The rows are cached behind an `Arc` and
    /// [`Component::render_document`] hands out the handle, so a repeat frame
    /// costs a refcount bump.
    cached_render: Mutex<Option<(RenderKey, Arc<Vec<String>>)>>,
    inline_visible: Mutex<bool>,
    /// Number of times `render_image` had to derive a placement instead of
    /// returning the cached one.
    ///
    /// A test that only compares the rendered strings cannot tell a cache hit
    /// from a re-derivation that produced identical output, so the assertions
    /// on "the cache still works" read this counter instead.
    #[cfg(test)]
    derives: std::sync::atomic::AtomicUsize,
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
            inline_visible: Mutex::new(true),
            #[cfg(test)]
            derives: std::sync::atomic::AtomicUsize::new(0),
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
            inline_visible: Mutex::new(true),
            #[cfg(test)]
            derives: std::sync::atomic::AtomicUsize::new(0),
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
            inline_visible: Mutex::new(true),
            #[cfg(test)]
            derives: std::sync::atomic::AtomicUsize::new(0),
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

    /// How many times a render had to be derived instead of served from the
    /// cache. Test-only: it exists so a "the cache still hits" assertion can
    /// fail when the cache is dropped on every render.
    #[cfg(test)]
    pub(crate) fn derive_count(&self) -> usize {
        self.derives.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Render the image as rows.
    ///
    /// Returns shared rows so a frame can place them without copying. The cache
    /// is keyed on the width, the polled cell geometry and whether the terminal
    /// already holds the payload.
    fn render_rows(&self, width: usize) -> Arc<Vec<String>> {
        if width == 0 {
            return Arc::new(vec![String::new()]);
        }
        if !*self.inline_visible.lock().unwrap() {
            return Arc::new(vec![crate::utils::truncate_to_width("[Image]", width, "")]);
        }
        let caps = get_capabilities();
        // The frame being painted was laid out for the geometry its renderer
        // polled; the process-wide cache is only the fallback outside a frame.
        let cell_dims = crate::terminal_image::effective_cell_dimensions();
        let uploaded = self.uploaded(caps.protocol);
        let key = RenderKey {
            width,
            cell_width: cell_dims.width,
            cell_height: cell_dims.height,
            protocol: caps.protocol,
            uploaded,
        };

        // Check if we have cached rows for this width, geometry and upload state.
        if let Ok(cached) = self.cached_render.lock() {
            if let Some((cached_key, rows)) = cached.as_ref() {
                if *cached_key == key {
                    return rows.clone();
                }
            }
        }

        let rows = Arc::new(self.derive_rows(width, caps.protocol, cell_dims, uploaded));

        // Cache the rows against the inputs they were computed for.
        if let Ok(mut cached) = self.cached_render.lock() {
            *cached = Some((key, rows.clone()));
        }
        rows
    }

    /// Whether the terminal being painted already holds this image's payload.
    ///
    /// Only the Kitty protocol can place an image by id; the iTerm2 protocol
    /// carries the data inline in every frame.
    fn uploaded(&self, protocol: ImageProtocol) -> bool {
        if protocol != ImageProtocol::Kitty {
            return false;
        }
        self.image_id
            .lock()
            .ok()
            .and_then(|id| *id)
            .is_some_and(frame_holds_image)
    }

    /// Derive the rows for one (width, geometry, upload state) combination.
    fn derive_rows(
        &self,
        width: usize,
        protocol: ImageProtocol,
        cell_dims: crate::terminal_image::CellDimensions,
        uploaded: bool,
    ) -> Vec<String> {
        #[cfg(test)]
        self.derives
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dimensions = self.get_dimensions();
        let (img_width, img_height) = if let Some((w, h)) = dimensions {
            // Calculate cell dimensions
            let cell_width = self
                .options
                .width
                .unwrap_or_else(|| {
                    ((w as f32 / cell_dims.width.max(1) as f32).ceil() as u16).min(60)
                })
                .min(width.saturating_sub(2).max(1).min(u16::MAX as usize) as u16)
                .max(1);
            let cell_height = self
                .options
                .height
                .unwrap_or_else(|| {
                    ((h as f64 / w.max(1) as f64) * cell_width as f64 * cell_dims.width as f64
                        / cell_dims.height.max(1) as f64)
                        .ceil() as u16
                })
                .max(1);
            let max_height = self
                .options
                .height
                .unwrap_or_else(|| {
                    (cell_width as f64 * cell_dims.width as f64 / cell_dims.height.max(1) as f64)
                        .ceil() as u16
                })
                .max(1);
            if self.options.preserve_aspect && cell_height > max_height {
                (
                    ((cell_width as f64 * max_height as f64 / cell_height as f64).floor() as u16)
                        .max(1),
                    max_height,
                )
            } else {
                (cell_width, cell_height)
            }
        } else {
            (
                self.options.width.unwrap_or(width as u16),
                self.options.height.unwrap_or(5),
            )
        };

        // Render based on protocol
        let result = if protocol == ImageProtocol::None {
            // Fallback: render placeholder
            crate::utils::truncate_to_width(
                self.options.alt_text.as_deref().unwrap_or("[Image]"),
                width,
                "",
            )
        } else if let Ok(data) = self.data.lock() {
            if let Some(ref img_data) = *data {
                let id = self.image_id.lock().ok().and_then(|id| *id);
                // The layout crops a placement to the rows that are on screen,
                // which needs the pixel geometry behind the cell rectangle.
                if protocol == ImageProtocol::Kitty {
                    if let (Some(id), Some((w, h))) = (id, dimensions) {
                        register_kitty_image(KittyImageMetadata {
                            id,
                            columns: img_width,
                            rows: img_height,
                            width_px: w,
                            height_px: h,
                        });
                    }
                }
                let options = ImageRenderOptions {
                    width: Some(img_width),
                    height: Some(img_height),
                    preserve_aspect: self.options.preserve_aspect,
                    x: 0,
                    y: 0,
                    z_index: None,
                    id,
                };
                // Keep cursor position stable and reserve the image's rows in
                // the retained layout so following text cannot overlap it.
                //
                // A terminal that already holds the payload only needs the
                // placement, which is a few dozen bytes instead of the whole
                // PNG: this is the difference between a transcript of
                // screenshots costing a few kilobytes or ~80 MB per repaint.
                let placement = (uploaded && protocol == ImageProtocol::Kitty)
                    .then(|| encode_kitty_placement(&options));
                let sequence = placement.unwrap_or_else(|| render_image(img_data, &options));
                let rows = img_height.saturating_sub(1) as usize;
                if protocol == ImageProtocol::Kitty {
                    format!("{sequence}{}", "\n".repeat(rows))
                } else {
                    let up = if rows > 0 {
                        format!("\x1b[{rows}A")
                    } else {
                        String::new()
                    };
                    format!("{}{up}{sequence}", "\n".repeat(rows))
                }
            } else {
                self.render_placeholder(img_width as usize, img_height as usize, width)
            }
        } else {
            self.render_placeholder(img_width as usize, img_height as usize, width)
        };

        result.split('\n').map(|row| row.to_string()).collect()
    }

    /// Hide terminal graphics while retaining an attachment placeholder.
    pub fn set_inline_visible(&self, visible: bool) {
        let mut current = self.inline_visible.lock().unwrap();
        if *current == visible {
            return;
        }
        *current = visible;
        *self.cached_render.lock().unwrap() = None;
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
        self.render_rows(width).as_ref().clone()
    }

    /// Hand the cached rows to the frame instead of copying them.
    ///
    /// The scroll view builds its document from these handles, so a repaint of
    /// an unchanged transcript retains the same image rows instead of cloning
    /// megabytes of base64 per frame.
    fn render_document(&self, width: usize) -> RenderedDocument {
        RenderedDocument::from_shared(self.render_rows(width))
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

    /// A PNG header with the given pixel size.
    ///
    /// `Image` only reads the header to derive the row count, so 24 bytes are
    /// enough and the test needs no terminal to decode anything.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn image_visibility_retains_placeholder_and_resizing_rebuilds_layout() {
        let image = Image::new();
        image.set_inline_visible(false);
        assert_eq!(image.render(80), vec!["[Image]"]);
        assert_eq!(image.render(3), vec!["[Im"]);
        assert_eq!(image.render(0), vec![""]);
        image.set_inline_visible(true);
        let wide = image.render(80);
        let narrow = image.render(3);
        assert_ne!(wide, narrow);
        assert!(narrow.iter().all(|line| visible_width(line) <= 3));
    }

    /// The render cache must actually hit: a repeat render at the same width
    /// and geometry may not re-derive.
    ///
    /// The integration probe that compares rendered strings cannot prove this
    /// (a re-derivation of the same inputs produces the same bytes), so the
    /// assertion reads the per-image derive counter instead.
    #[test]
    fn a_repeat_render_is_served_from_the_cache() {
        let _g = crate::terminal_image::capability_test_lock();
        let saved = crate::terminal_image::get_capabilities();
        struct Restore(crate::terminal_image::TerminalCapabilities);
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::terminal_image::set_capabilities(self.0.clone());
            }
        }
        let _restore = Restore(saved);
        let mut caps = crate::terminal_image::get_capabilities();
        caps.protocol = ImageProtocol::Kitty;
        caps.cell_width = 10;
        caps.cell_height = 20;
        crate::terminal_image::set_capabilities(caps);

        // A PNG header is enough: only the pixel size is read.
        let image = Image::from_data(png_header(400, 200), ImageOptions::default());

        let first = image.render(80);
        assert_eq!(image.derive_count(), 1, "the first render derives");
        let again = image.render(80);
        assert_eq!(again, first);
        assert_eq!(
            image.derive_count(),
            1,
            "a repeat render at the same width and geometry must hit the cache"
        );

        // A geometry change must invalidate it, or an image already on screen
        // keeps the row count of the previous cell size.
        crate::terminal_image::set_cell_dimensions(10, 10);
        let rederived = image.render(80);
        assert_eq!(image.derive_count(), 2, "a geometry change must re-derive");
        assert_ne!(rederived.len(), first.len());

        // A width change must invalidate it too.
        image.render(40);
        assert_eq!(image.derive_count(), 3, "a width change must re-derive");
    }

    /// A repeat render must not re-derive *and* must hand out the same rows.
    ///
    /// Sharing the row allocation is what makes a repaint of an unchanged
    /// transcript cheap: the scroll view builds its document from these
    /// handles, so a kitty payload row (megabytes) is never copied again.
    #[test]
    fn a_repeat_render_shares_its_rows() {
        let _g = crate::terminal_image::capability_test_lock();
        let saved = crate::terminal_image::get_capabilities();
        struct Restore(crate::terminal_image::TerminalCapabilities);
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::terminal_image::set_capabilities(self.0.clone());
            }
        }
        let _restore = Restore(saved);
        let mut caps = crate::terminal_image::get_capabilities();
        caps.protocol = ImageProtocol::Kitty;
        caps.cell_width = 10;
        caps.cell_height = 20;
        crate::terminal_image::set_capabilities(caps);

        let image = Image::from_data(png_header(400, 200), ImageOptions::default());
        let first = image.render_document(80);
        let second = image.render_document(80);
        assert_eq!(
            image.derive_count(),
            1,
            "a repeat document render must hit the cache"
        );
        assert_eq!(first.len(), second.len());
        assert_eq!(first.line(0), second.line(0));
    }

    /// Once the terminal holds the payload, the row must place the image
    /// instead of carrying the PNG again.
    ///
    /// This is the difference between an idle repaint touching a few kilobytes
    /// and touching every megabyte of every screenshot in the transcript.
    #[test]
    fn an_uploaded_image_is_rendered_as_a_placement_only_row() {
        let _g = crate::terminal_image::capability_test_lock();
        let saved = crate::terminal_image::get_capabilities();
        struct Restore(crate::terminal_image::TerminalCapabilities);
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::terminal_image::set_capabilities(self.0.clone());
            }
        }
        let _restore = Restore(saved);
        let mut caps = crate::terminal_image::get_capabilities();
        caps.protocol = ImageProtocol::Kitty;
        caps.cell_width = 10;
        caps.cell_height = 20;
        crate::terminal_image::set_capabilities(caps);

        let mut png = png_header(400, 200);
        png.resize(256 * 1024, 0);
        let image = Image::from_data(png, ImageOptions::default());
        let id = image.image_id.lock().unwrap().unwrap();

        // No renderer published a ledger: the payload has to be sent.
        let transmitted = image.render(80);
        assert!(
            transmitted[0].contains("a=T"),
            "unknown terminal must be sent the payload"
        );
        assert!(
            transmitted[0].len() > 200 * 1024,
            "the payload row is the multi-megabyte case: {}",
            transmitted[0].len()
        );

        // The renderer reports the upload: the same image now costs a placement.
        let uploaded = Arc::new(std::collections::HashSet::from([id]));
        let placed =
            crate::terminal_image::with_frame_uploaded_images(uploaded, || image.render(80));
        assert_eq!(
            placed.len(),
            transmitted.len(),
            "the placement must reserve the same rows"
        );
        assert!(
            placed[0].contains("a=p"),
            "an uploaded image is placed, not transmitted: {:?}",
            placed[0]
        );
        assert!(
            !placed[0].contains("a=T"),
            "the payload must not be sent twice"
        );
        assert!(
            placed[0].len() < 200,
            "a placement row is bytes, not megabytes: {}",
            placed[0].len()
        );
        assert!(
            image.derive_count() == 2,
            "the upload form is derived once and then cached"
        );
    }

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
