//! Bounded image decoding and downscaling.
//!
//! Two independent caps, because they protect against different attacks:
//!
//! * **pixel count** — a 30 KB PNG can decode to 40 megapixels and exhaust memory,
//!   so the header is read first and an oversized image is refused *before* decoding,
//! * **longest edge** — the model only ever receives a downscaled copy, which bounds
//!   both the transfer and the prompt cost.
//!
//! Everything happens on a blocking thread: image decoding is CPU bound and must not
//! stall the runtime.

use image::{ImageReader, imageops::FilterType};

use super::download::MediaError;

/// Longest edge after downscaling.
pub const MAX_EDGE: u32 = 1_600;

/// A decoded, possibly downscaled image.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub width: u32,
    pub height: u32,
    /// Encoded PNG bytes, ready for transport. For an animated GIF this is a
    /// filmstrip of sampled frames, not a playable file.
    pub bytes: Vec<u8>,
    /// The source was an animated image (GIF with more than one frame).
    pub animated: bool,
    /// The untouched source bytes when they differ from `bytes` and are
    /// worth keeping — a GIF's filmstrip is for the vision model, but the
    /// sticker library must store (and later re-send) the real animation.
    pub original: Option<Vec<u8>>,
}

impl PreparedImage {
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Decode and downscale an image, refusing anything that exceeds `max_pixels`.
///
/// The pixel cap is checked against the *header* first, so a decompression bomb is
/// rejected without allocating the full buffer.
pub fn prepare(bytes: &[u8], max_pixels: u64) -> Result<PreparedImage, MediaError> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| MediaError::NotAnImage)?;
    let format = reader.format().ok_or(MediaError::NotAnImage)?;
    if format == image::ImageFormat::Gif {
        return prepare_gif(bytes, max_pixels);
    }
    // Only the formats the build enables.
    if !matches!(
        format,
        image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
    ) {
        return Err(MediaError::NotAnImage);
    }

    // Header-only pass: no pixel buffer is allocated yet.
    let (width, height) = dimensions(bytes, format)?;
    let pixels = u64::from(width) * u64::from(height);
    if max_pixels > 0 && pixels > max_pixels {
        return Err(MediaError::TooLarge);
    }

    let decoded = reader.decode().map_err(|_| MediaError::NotAnImage)?;
    let (width, height) = (decoded.width(), decoded.height());
    let scaled = if width.max(height) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / width.max(height) as f32;
        let target_w = ((width as f32 * scale).round() as u32).max(1);
        let target_h = ((height as f32 * scale).round() as u32).max(1);
        image::DynamicImage::ImageRgba8(image::imageops::resize(
            &decoded,
            target_w,
            target_h,
            FilterType::Triangle,
        ))
    } else {
        decoded
    };

    let (out_w, out_h) = (scaled.width(), scaled.height());
    let mut buffer = Vec::new();
    scaled
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|_| MediaError::NotAnImage)?;
    Ok(PreparedImage {
        width: out_w,
        height: out_h,
        bytes: buffer,
        animated: false,
        original: None,
    })
}

/// How many frames of an animation the vision model gets to see.
const GIF_STRIP_FRAMES: usize = 4;
/// Longest edge of one sampled frame inside the strip.
const GIF_FRAME_EDGE: u32 = 512;
/// Decode at most this many frames while sampling: a GIF that needs more
/// is pathological, and the first minutes of it are not worth the CPU.
const GIF_FRAME_SCAN_CAP: usize = 300;

/// Prepare a GIF. A single-frame GIF is just a picture. An animated one is
/// sampled into a filmstrip — up to [`GIF_STRIP_FRAMES`] frames, evenly
/// spaced, in playback order — because the vision endpoint takes stills:
/// before this, every animated sticker failed the format gate and the
/// model saw "image unavailable" where the group saw a looping sticker.
fn prepare_gif(bytes: &[u8], max_pixels: u64) -> Result<PreparedImage, MediaError> {
    let (width, height) = dimensions(bytes, image::ImageFormat::Gif)?;
    let pixels = u64::from(width) * u64::from(height);
    if max_pixels > 0 && pixels > max_pixels {
        return Err(MediaError::TooLarge);
    }
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| MediaError::NotAnImage)?;
    let frames: Vec<image::Frame> = decoder
        .into_frames()
        .take(GIF_FRAME_SCAN_CAP)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| MediaError::NotAnImage)?;
    if frames.len() <= 1 {
        // Not actually animated: decode the still through the normal path.
        let reader = ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| MediaError::NotAnImage)?;
        let decoded = reader.decode().map_err(|_| MediaError::NotAnImage)?;
        let mut prepared = prepare_static(decoded)?;
        prepared.original = Some(bytes.to_vec());
        return Ok(prepared);
    }
    let last = frames.len() - 1;
    let mut picks: Vec<usize> = (0..GIF_STRIP_FRAMES)
        .map(|k| k * last / (GIF_STRIP_FRAMES - 1).max(1))
        .collect();
    picks.dedup();
    let mut cells: Vec<image::RgbaImage> = Vec::new();
    for &index in &picks {
        let frame = frames[index].buffer();
        let (w, h) = (frame.width(), frame.height());
        let cell = if w.max(h) > GIF_FRAME_EDGE {
            let scale = GIF_FRAME_EDGE as f32 / w.max(h) as f32;
            image::imageops::resize(
                frame,
                ((w as f32 * scale).round() as u32).max(1),
                ((h as f32 * scale).round() as u32).max(1),
                FilterType::Triangle,
            )
        } else {
            frame.clone()
        };
        cells.push(cell);
    }
    let gap = 8u32;
    let strip_w: u32 =
        cells.iter().map(|c| c.width()).sum::<u32>() + gap * (cells.len() as u32 - 1);
    let strip_h: u32 = cells.iter().map(|c| c.height()).max().unwrap_or(0);
    let mut strip =
        image::RgbaImage::from_pixel(strip_w, strip_h, image::Rgba([255, 255, 255, 255]));
    let mut x = 0u32;
    for cell in &cells {
        image::imageops::overlay(&mut strip, cell, i64::from(x), 0);
        x += cell.width() + gap;
    }
    let mut buffer = Vec::new();
    image::DynamicImage::ImageRgba8(strip)
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|_| MediaError::NotAnImage)?;
    Ok(PreparedImage {
        width: strip_w,
        height: strip_h,
        bytes: buffer,
        animated: true,
        original: Some(bytes.to_vec()),
    })
}

/// Scale + PNG-encode one decoded still. The static arm of [`prepare`]
/// inlined; GIF's single-frame case reuses it.
fn prepare_static(decoded: image::DynamicImage) -> Result<PreparedImage, MediaError> {
    let (width, height) = (decoded.width(), decoded.height());
    let scaled = if width.max(height) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / width.max(height) as f32;
        image::DynamicImage::ImageRgba8(image::imageops::resize(
            &decoded,
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
            FilterType::Triangle,
        ))
    } else {
        decoded
    };
    let (out_w, out_h) = (scaled.width(), scaled.height());
    let mut buffer = Vec::new();
    scaled
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|_| MediaError::NotAnImage)?;
    Ok(PreparedImage {
        width: out_w,
        height: out_h,
        bytes: buffer,
        animated: false,
        original: None,
    })
}

/// Read only the dimensions from the header.
fn dimensions(bytes: &[u8], format: image::ImageFormat) -> Result<(u32, u32), MediaError> {
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.into_dimensions().map_err(|_| MediaError::NotAnImage)
}

/// Async wrapper that runs [`prepare`] on a blocking thread.
pub async fn prepare_async(bytes: Vec<u8>, max_pixels: u64) -> Result<PreparedImage, MediaError> {
    tokio::task::spawn_blocking(move || prepare(&bytes, max_pixels))
        .await
        .map_err(|_| MediaError::NotAnImage)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn animated_gif(frames: u32, width: u32, height: u32) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut encoder =
                image::codecs::gif::GifEncoder::new(std::io::Cursor::new(&mut buffer));
            let made: Vec<image::Frame> = (0..frames)
                .map(|i| {
                    let shade = (40 + i * 60) as u8;
                    image::Frame::new(image::RgbaImage::from_pixel(
                        width,
                        height,
                        image::Rgba([shade, shade, shade, 255]),
                    ))
                })
                .collect();
            encoder.encode_frames(made).expect("编码 GIF");
        }
        buffer
    }

    #[test]
    fn animated_gif_becomes_a_filmstrip_and_keeps_its_source() {
        let source = animated_gif(3, 40, 30);
        let prepared = prepare(&source, 0).expect("动图应当可制备");
        assert!(prepared.animated);
        assert_eq!(prepared.original.as_deref(), Some(source.as_slice()));
        // Three 40px frames plus two 8px gaps.
        assert_eq!(prepared.width, 3 * 40 + 2 * 8);
        assert_eq!(prepared.height, 30);
    }

    #[test]
    fn single_frame_gif_is_a_still_with_its_source_kept() {
        let source = animated_gif(1, 40, 30);
        let prepared = prepare(&source, 0).expect("单帧 GIF 应当可制备");
        assert!(!prepared.animated);
        assert_eq!(prepared.original.as_deref(), Some(source.as_slice()));
        assert_eq!((prepared.width, prepared.height), (40, 30));
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("编码");
        out
    }

    #[test]
    fn small_image_is_kept_at_its_size() {
        let prepared = prepare(&png(64, 48), 20_000_000).expect("应成功");
        assert_eq!((prepared.width, prepared.height), (64, 48));
        assert!(!prepared.bytes.is_empty());
    }

    #[test]
    fn oversized_pixels_are_refused_from_the_header() {
        // 6000 x 6000 = 36 MP > 20 MP cap. The header check must reject it without
        // decoding, which is why this test stays fast.
        let bytes = png(6_000, 6_000);
        assert_eq!(
            prepare(&bytes, 20_000_000).err(),
            Some(MediaError::TooLarge)
        );
    }

    #[test]
    fn longest_edge_is_capped() {
        let prepared = prepare(&png(3_200, 100), 20_000_000).expect("应成功");
        assert_eq!(prepared.width, MAX_EDGE);
        // Aspect ratio is preserved within rounding.
        assert!(prepared.height <= 100);
    }

    #[test]
    fn garbage_is_not_an_image() {
        assert_eq!(
            prepare(b"definitely not an image", 20_000_000).err(),
            Some(MediaError::NotAnImage)
        );
        assert_eq!(prepare(&[], 20_000_000).err(), Some(MediaError::NotAnImage));
    }

    #[test]
    fn a_zero_cap_disables_the_pixel_check_but_not_the_edge_cap() {
        let prepared = prepare(&png(2_000, 2_000), 0).expect("应成功");
        assert_eq!(prepared.width, MAX_EDGE);
    }

    #[tokio::test]
    async fn async_wrapper_matches_the_sync_path() {
        let prepared = prepare_async(png(32, 32), 20_000_000)
            .await
            .expect("应成功");
        assert_eq!(prepared.pixels(), 32 * 32);
    }
}
