//! Terminal image support.
//!
//! Provides support for displaying images in terminals using Kitty and iTerm2 protocols.

use std::sync::Mutex;

/// Image protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// iTerm2 inline images.
    ITerm2,
    /// No image support.
    None,
}

/// Terminal capabilities for image display.
#[derive(Debug, Clone)]
pub struct TerminalCapabilities {
    /// Supported image protocol.
    pub protocol: ImageProtocol,
    /// Cell dimensions in pixels.
    pub cell_width: u16,
    pub cell_height: u16,
    /// Screen dimensions in cells.
    pub columns: u16,
    pub rows: u16,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            protocol: ImageProtocol::None,
            cell_width: 8,
            cell_height: 16,
            columns: 80,
            rows: 24,
        }
    }
}

/// Image dimensions.
#[derive(Debug, Clone, Copy)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

/// Cell dimensions in pixels.
#[derive(Debug, Clone, Copy)]
pub struct CellDimensions {
    pub width: u16,
    pub height: u16,
}

/// Options for rendering an image.
#[derive(Debug, Clone)]
pub struct ImageRenderOptions {
    /// Width in cells (None = auto).
    pub width: Option<u16>,
    /// Height in cells (None = auto).
    pub height: Option<u16>,
    /// Whether to preserve aspect ratio.
    pub preserve_aspect: bool,
    /// X offset in cells.
    pub x: u16,
    /// Y offset in cells.
    pub y: u16,
    /// Z-index for overlapping images (Kitty only).
    pub z_index: Option<i32>,
    /// Image ID for manipulation (Kitty only).
    pub id: Option<u32>,
}

impl Default for ImageRenderOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            preserve_aspect: true,
            x: 0,
            y: 0,
            z_index: None,
            id: None,
        }
    }
}

/// Global capabilities cache.
static CAPABILITIES_CACHE: std::sync::OnceLock<Mutex<TerminalCapabilities>> = std::sync::OnceLock::new();

/// Get cached capabilities.
pub fn get_capabilities() -> TerminalCapabilities {
    let cache = CAPABILITIES_CACHE.get_or_init(|| Mutex::new(TerminalCapabilities::default()));
    cache.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Set capabilities.
pub fn set_capabilities(capabilities: TerminalCapabilities) {
    if let Some(cache) = CAPABILITIES_CACHE.get() {
        if let Ok(mut c) = cache.lock() {
            *c = capabilities;
        }
    }
}

/// Reset capabilities cache.
pub fn reset_capabilities_cache() {
    set_capabilities(TerminalCapabilities::default());
}

/// Detect terminal capabilities.
/// This queries the terminal for supported protocols.
pub fn detect_capabilities() -> TerminalCapabilities {
    let mut caps = TerminalCapabilities::default();

    // Try to detect Kitty protocol
    // In a real implementation, we would query the terminal
    // For now, we check environment variables
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        caps.protocol = ImageProtocol::Kitty;
    } else if std::env::var("ITERM_SESSION_ID").is_ok() {
        caps.protocol = ImageProtocol::ITerm2;
    }

    // Get terminal size
    if let Ok(size) = crossterm::terminal::size() {
        caps.columns = size.0;
        caps.rows = size.1;
    }

    caps
}

/// Set cell dimensions.
pub fn set_cell_dimensions(width: u16, height: u16) {
    let mut caps = get_capabilities();
    caps.cell_width = width;
    caps.cell_height = height;
    set_capabilities(caps);
}

/// Get cell dimensions.
pub fn get_cell_dimensions() -> CellDimensions {
    let caps = get_capabilities();
    CellDimensions {
        width: caps.cell_width,
        height: caps.cell_height,
    }
}

/// Allocate a unique image ID.
pub fn allocate_image_id() -> u32 {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Encode image data for Kitty protocol.
pub fn encode_kitty(data: &[u8], options: &ImageRenderOptions) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    
    let mut parts = Vec::new();

    // Build control string
    let mut control = String::new();
    control.push_str("a=T,f=100"); // Transmit and display

    if let Some(id) = options.id {
        control.push_str(&format!(",i={}", id));
    }

    if let Some(z) = options.z_index {
        control.push_str(&format!(",z={}", z));
    }

    // Calculate dimensions
    if let (Some(w), Some(h)) = (options.width, options.height) {
        let caps = get_capabilities();
        let px_width = w as u32 * caps.cell_width as u32;
        let px_height = h as u32 * caps.cell_height as u32;
        control.push_str(&format!(",s={},v={}", px_width, px_height));
    }

    // Encode image data as base64
    let encoded = STANDARD.encode(data);

    // Chunk the data (Kitty has a 4096 byte limit per chunk)
    let chunk_size = 4000;
    let chunks: Vec<&str> = encoded.as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let _m = if i == chunks.len() - 1 { 0 } else { 1 };
        parts.push(format!("\x1b_G{};{}\x1b\\", control, chunk));
        // After first chunk, don't repeat control
        if i == 0 {
            control.clear();
        }
    }

    parts.join("")
}

/// Encode image data for iTerm2 protocol.
pub fn encode_iterm2(data: &[u8], options: &ImageRenderOptions) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    
    let encoded = STANDARD.encode(data);

    let mut name = String::new();
    if let Some(id) = options.id {
        name = format!("name=id{}", id);
    }

    let mut dims = String::new();
    if let Some(w) = options.width {
        dims = format!("width={}", w);
    }
    if let Some(h) = options.height {
        if !dims.is_empty() {
            dims.push(';');
        }
        dims.push_str(&format!("height={}", h));
    }

    format!(
        "\x1b]1337;File={};{}:{}\x07",
        name,
        dims,
        encoded
    )
}

/// Delete a Kitty image by ID.
pub fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={}\x1b\\", id)
}

/// Delete all Kitty images.
pub fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A\x1b\\".to_string()
}

/// Create an OSC 8 hyperlink.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", url, text)
}

/// Image dimensions for common formats.

/// Get PNG dimensions from header.
pub fn get_png_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 24 {
        return None;
    }

    // PNG signature: 89 50 4E 47 0D 0A 1A 0A
    if &data[0..8] != &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return None;
    }

    // Width at offset 16 (4 bytes, big endian)
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    // Height at offset 20 (4 bytes, big endian)
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);

    Some(ImageDimensions { width, height })
}

/// Get JPEG dimensions from header.
pub fn get_jpeg_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }

    let mut pos = 2;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }

        let marker = data[pos + 1];

        // SOF markers (Start of Frame)
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if pos + 9 > data.len() {
                return None;
            }
            let height = u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u32;
            let width = u16::from_be_bytes([data[pos + 7], data[pos + 8]]) as u32;
            return Some(ImageDimensions { width, height });
        }

        // Skip to next marker
        if pos + 4 > data.len() {
            return None;
        }
        let length = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 2 + length;
    }

    None
}

/// Get GIF dimensions from header.
pub fn get_gif_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 10 {
        return None;
    }

    // GIF signature: GIF87a or GIF89a
    if &data[0..6] != b"GIF87a" && &data[0..6] != b"GIF89a" {
        return None;
    }

    // Width at offset 6 (2 bytes, little endian)
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    // Height at offset 8 (2 bytes, little endian)
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;

    Some(ImageDimensions { width, height })
}

/// Get WebP dimensions from header.
pub fn get_webp_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    if data.len() < 30 {
        return None;
    }

    // RIFF....WEBP
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }

    // Check for VP8 chunk
    if &data[12..16] == b"VP8 " {
        // VP8 format
        let width = u16::from_le_bytes([data[26], data[27]]) as u32 & 0x3FFF;
        let height = u16::from_le_bytes([data[28], data[29]]) as u32 & 0x3FFF;
        return Some(ImageDimensions { width, height });
    }

    // Check for VP8L (lossless)
    if &data[12..16] == b"VP8L" {
        // VP8L format - dimensions are in a 28-bit field
        if data.len() < 21 {
            return None;
        }
        let bits = u32::from_le_bytes([data[17], data[18], data[19], data[20]]);
        let width = (bits & 0x3FFF) + 1;
        let height = ((bits >> 14) & 0x3FFF) + 1;
        return Some(ImageDimensions { width, height });
    }

    None
}

/// Get image dimensions for common formats.
pub fn get_image_dimensions(data: &[u8]) -> Option<ImageDimensions> {
    // Try PNG first
    if let Some(dims) = get_png_dimensions(data) {
        return Some(dims);
    }

    // Try JPEG
    if let Some(dims) = get_jpeg_dimensions(data) {
        return Some(dims);
    }

    // Try GIF
    if let Some(dims) = get_gif_dimensions(data) {
        return Some(dims);
    }

    // Try WebP
    if let Some(dims) = get_webp_dimensions(data) {
        return Some(dims);
    }

    None
}

/// Calculate number of rows needed for an image.
pub fn calculate_image_rows(height: u32, cell_height: u16) -> u16 {
    ((height + cell_height as u32 - 1) / cell_height as u32) as u16
}

/// Render image with fallback.
pub fn render_image(data: &[u8], options: &ImageRenderOptions) -> String {
    let caps = get_capabilities();

    match caps.protocol {
        ImageProtocol::Kitty => encode_kitty(data, options),
        ImageProtocol::ITerm2 => encode_iterm2(data, options),
        ImageProtocol::None => {
            // Fallback: show placeholder
            let alt_text = format!("[Image {}x{}]", 
                options.width.unwrap_or(1),
                options.height.unwrap_or(1)
            );
            alt_text
        }
    }
}

/// Image fallback for unsupported terminals.
pub fn image_fallback(_alt_text: &str, width: usize) -> String {
    let placeholder = "█".repeat(width.saturating_sub(2));
    format!("╭{}╮\n│{}│\n╰{}╯", 
        "─".repeat(width.saturating_sub(2)),
        placeholder,
        "─".repeat(width.saturating_sub(2))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_default() {
        let caps = TerminalCapabilities::default();
        assert_eq!(caps.protocol, ImageProtocol::None);
    }

    #[test]
    fn test_allocate_image_id() {
        let id1 = allocate_image_id();
        let id2 = allocate_image_id();
        assert!(id2 > id1);
    }

    #[test]
    fn test_hyperlink() {
        let link = hyperlink("https://example.com", "Click here");
        assert!(link.contains("https://example.com"));
        assert!(link.contains("Click here"));
    }

    #[test]
    fn test_png_dimensions() {
        // Minimal PNG header
        let data = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x10, // width = 16
            0x00, 0x00, 0x00, 0x20, // height = 32
            0x08, 0x02, 0x00, 0x00, 0x00, // bit depth, color type, etc.
        ];
        let dims = get_png_dimensions(&data).unwrap();
        assert_eq!(dims.width, 16);
        assert_eq!(dims.height, 32);
    }
}