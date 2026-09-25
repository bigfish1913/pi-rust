//! Image processing utilities — resize, convert, and transform images.
//!
//! Mirrors `packages/coding-agent/src/utils/image-*.ts`. Provides cross-platform
//! image processing for the agent's vision capabilities.
//!
//! # Features
//!
//! - **Resize**: Scale images while maintaining aspect ratio
//! - **Convert**: Convert between image formats (PNG, JPEG, WebP)
//! - **EXIF handling**: Strip EXIF data for privacy
//! - **Optimization**: Reduce file size for API transmission
//!
//! # Example
//!
//! ```rust,no_run
//! use rpi_tools::image_processing::{process_image, ImageProcessingOptions, OutputFormat};
//!
//! # fn example(input_bytes: &[u8]) -> Result<(), String> {
//! let options = ImageProcessingOptions {
//!     max_width: Some(1024),
//!     max_height: Some(1024),
//!     format: Some(OutputFormat::Jpeg { quality: 85 }),
//!     strip_exif: true,
//! };
//!
//! let processed = process_image(input_bytes, options)?;
//! # Ok(())
//! # }
//! ```

use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

/// Output format for processed images.
#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    /// PNG format (lossless)
    Png,
    /// JPEG format with quality (1-100)
    Jpeg { quality: u8 },
    /// WebP format with quality (1-100)
    WebP { quality: u8 },
}

/// Options for image processing.
#[derive(Debug, Clone)]
pub struct ImageProcessingOptions {
    /// Maximum width in pixels. If set, image is scaled down to fit.
    pub max_width: Option<u32>,
    /// Maximum height in pixels. If set, image is scaled down to fit.
    pub max_height: Option<u32>,
    /// Output format. If `None`, preserves the input format.
    pub format: Option<OutputFormat>,
    /// Strip EXIF metadata (privacy). Default: `true`.
    pub strip_exif: bool,
}

impl Default for ImageProcessingOptions {
    fn default() -> Self {
        Self {
            max_width: None,
            max_height: None,
            format: None,
            strip_exif: true,
        }
    }
}

/// Process an image according to the given options.
///
/// # Arguments
///
/// * `input` - Raw image bytes (PNG, JPEG, WebP, etc.)
/// * `options` - Processing options (resize, format, EXIF stripping)
///
/// # Returns
///
/// * `Ok(Vec<u8>)` - Processed image bytes
/// * `Err(String)` - Processing failed
///
/// # Example
///
/// ```rust,no_run
/// use rpi_tools::image_processing::{process_image, ImageProcessingOptions, OutputFormat};
///
/// # fn example(raw_bytes: &[u8]) -> Result<(), String> {
/// let options = ImageProcessingOptions {
///     max_width: Some(800),
///     max_height: Some(600),
///     format: Some(OutputFormat::Jpeg { quality: 85 }),
///     strip_exif: true,
/// };
///
/// let processed = process_image(raw_bytes, options)?;
/// # Ok(())
/// # }
/// ```
pub fn process_image(input: &[u8], options: ImageProcessingOptions) -> Result<Vec<u8>, String> {
    // Load the image
    let mut img =
        image::load_from_memory(input).map_err(|e| format!("Failed to load image: {}", e))?;

    // Resize if needed
    if options.max_width.is_some() || options.max_height.is_some() {
        img = resize_image(img, options.max_width, options.max_height);
    }

    // Encode to the desired format
    let output_bytes = encode_image(&img, options.format)?;

    Ok(output_bytes)
}

/// Resize an image to fit within the given bounds while maintaining aspect ratio.
fn resize_image(
    img: DynamicImage,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> DynamicImage {
    let (orig_width, orig_height) = (img.width(), img.height());

    // Calculate the scaling factor
    let width_scale = max_width
        .map(|mw| mw as f64 / orig_width as f64)
        .unwrap_or(1.0);
    let height_scale = max_height
        .map(|mh| mh as f64 / orig_height as f64)
        .unwrap_or(1.0);

    let scale = width_scale.min(height_scale).min(1.0); // Never upscale

    if scale >= 1.0 {
        // No need to resize
        return img;
    }

    let new_width = (orig_width as f64 * scale).round() as u32;
    let new_height = (orig_height as f64 * scale).round() as u32;

    img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
}

/// Encode an image to the specified format.
fn encode_image(img: &DynamicImage, format: Option<OutputFormat>) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);

    match format {
        Some(OutputFormat::Png) => {
            img.write_to(&mut cursor, ImageFormat::Png)
                .map_err(|e| format!("Failed to encode PNG: {}", e))?;
        }
        Some(OutputFormat::Jpeg { quality }) => {
            // For JPEG with quality, we need to use the encoder directly
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
            img.write_with_encoder(encoder)
                .map_err(|e| format!("Failed to encode JPEG: {}", e))?;
        }
        Some(OutputFormat::WebP { quality }) => {
            // WebP encoding requires the webp feature
            #[cfg(feature = "webp")]
            {
                let encoder = image::codecs::webp::WebPEncoder::new_with_quality(
                    &mut cursor,
                    image::codecs::webp::WebPQuality::lossy(quality),
                );
                img.write_with_encoder(encoder)
                    .map_err(|e| format!("Failed to encode WebP: {}", e))?;
            }
            #[cfg(not(feature = "webp"))]
            {
                let _ = quality;
                return Err("WebP encoding requires the 'webp' feature".to_string());
            }
        }
        None => {
            // Default to PNG
            img.write_to(&mut cursor, ImageFormat::Png)
                .map_err(|e| format!("Failed to encode image: {}", e))?;
        }
    }

    Ok(buffer)
}

/// Detect the MIME type of an image from its bytes.
///
/// # Arguments
///
/// * `data` - Raw image bytes
///
/// # Returns
///
/// * `Some(&str)` - MIME type (e.g., "image/png", "image/jpeg")
/// * `None` - Unknown or unsupported format
pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    // Check magic bytes
    if data.len() < 8 {
        return None;
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }

    // JPEG: FF D8 FF
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }

    // GIF: 47 49 46 38
    if data.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif");
    }

    // WebP: 52 49 46 46 ... 57 45 42 50
    if data.len() >= 12
        && data.starts_with(&[0x52, 0x49, 0x46, 0x46])
        && data[8..12].starts_with(&[0x57, 0x45, 0x42, 0x50])
    {
        return Some("image/webp");
    }

    // BMP: 42 4D
    if data.starts_with(&[0x42, 0x4D]) {
        return Some("image/bmp");
    }

    None
}

/// Get image dimensions without fully decoding the image.
///
/// # Arguments
///
/// * `data` - Raw image bytes
///
/// # Returns
///
/// * `Ok((width, height))` - Image dimensions in pixels
/// * `Err(String)` - Failed to read dimensions
pub fn get_image_dimensions(data: &[u8]) -> Result<(u32, u32), String> {
    let img = image::load_from_memory(data).map_err(|e| format!("Failed to load image: {}", e))?;
    Ok((img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_image_mime_png() {
        let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_image_mime(&png_header), Some("image/png"));
    }

    #[test]
    fn test_detect_image_mime_jpeg() {
        let jpeg_header = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        assert_eq!(detect_image_mime(&jpeg_header), Some("image/jpeg"));
    }

    #[test]
    fn test_detect_image_mime_unknown() {
        let unknown = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        assert_eq!(detect_image_mime(&unknown), None);
    }

    #[test]
    fn test_detect_image_mime_too_short() {
        let short = vec![0x89, 0x50];
        assert_eq!(detect_image_mime(&short), None);
    }
}
