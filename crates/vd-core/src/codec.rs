//! Frame codecs.
//!
//! JPEG is the interim codec: it is a few lines of code, compresses well enough
//! to be practical over Wi-Fi, and lets the capture → encode → transport →
//! decode → render pipeline be built and measured before hardware H.264 lands.
//! When it does, the stateful encoder/decoder will live behind traits next to
//! these helpers.

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, RgbaImage as ImageRgba};

use crate::capture::CapturedFrame;
use crate::video::{FrameFormat, VideoFrame};

/// JPEG quality passed to the encoder (0–100).
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

/// An uncompressed RGBA image, ready to be uploaded as a texture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// RGBA, four bytes per pixel, row-major.
    pub pixels: Vec<u8>,
}

/// An encoded image with its wire format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedImage {
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Why an image could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The format is not supported by this codec helper.
    #[error("unsupported frame format: {0:?}")]
    Unsupported(FrameFormat),
    /// The buffer size does not match the declared dimensions.
    #[error("buffer size does not match the image dimensions")]
    SizeMismatch,
    /// The underlying codec failed.
    #[error("codec failure: {0}")]
    Codec(String),
}

/// The size `width`×`height` shrinks to so that it fits inside `max_width`×`max_height`,
/// preserving the aspect ratio. Returns the input size when it already fits.
pub fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    fit_within_aligned(width, height, max_width, max_height, 1)
}

/// Like [`fit_within`], but rounds the result down to a multiple of `align`.
///
/// Video codecs want even dimensions (chroma is subsampled 2×2), so the encode
/// path asks for `align = 2`.
pub fn fit_within_aligned(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
    align: u32,
) -> (u32, u32) {
    let align = align.max(1);
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return (width, height);
    }

    let (mut scaled_width, mut scaled_height) = if width <= max_width && height <= max_height {
        (width, height)
    } else {
        let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
        (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        )
    };

    if align > 1 {
        scaled_width -= scaled_width % align;
        scaled_height -= scaled_height % align;
        scaled_width = scaled_width.max(align);
        scaled_height = scaled_height.max(align);
    }
    (scaled_width, scaled_height)
}

/// Downscales a captured frame to fit within `max_width`×`max_height`.
///
/// A full-resolution desktop is far more than a tablet can display and far more
/// than the encoder can keep up with, so the stream is bounded here rather than
/// at the client. Takes ownership so the common "already fits" case is free.
pub fn fit_frame(
    frame: CapturedFrame,
    max_width: u32,
    max_height: u32,
) -> Result<CapturedFrame, CodecError> {
    let (width, height) = fit_within(frame.width, frame.height, max_width, max_height);
    if (width, height) == (frame.width, frame.height) {
        return Ok(frame);
    }

    let data = box_downscale(
        &frame.data,
        frame.format,
        frame.width,
        frame.height,
        width,
        height,
    )?;

    Ok(CapturedFrame {
        width,
        height,
        format: FrameFormat::RawRgba,
        timestamp_us: frame.timestamp_us,
        data,
    })
}

/// Box-filter downscale that reads `format` pixels and writes RGBA.
///
/// The `image` crate's general resampler costs ~1.7 s on a 4K frame, which is
/// useless for a live stream. A box filter is O(source pixels) with integer math
/// — tens of milliseconds for 4K — and looks fine for a desktop.
fn box_downscale(
    data: &[u8],
    format: FrameFormat,
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Result<Vec<u8>, CodecError> {
    let bytes_per_pixel = format
        .bytes_per_pixel()
        .ok_or(CodecError::Unsupported(format))?;
    let expected = src_width as usize * src_height as usize * bytes_per_pixel;
    if data.len() < expected {
        return Err(CodecError::SizeMismatch);
    }
    let swap_red_blue = matches!(format, FrameFormat::RawBgra);

    let mut out = vec![0u8; dst_width as usize * dst_height as usize * 4];
    for dy in 0..dst_height {
        let (y0, y1) = span(dy, src_height, dst_height);
        for dx in 0..dst_width {
            let (x0, x1) = span(dx, src_width, dst_width);

            let (mut red, mut green, mut blue, mut alpha) = (0u32, 0u32, 0u32, 0u32);
            let mut count = 0u32;
            for y in y0..y1 {
                let row = y as usize * src_width as usize * bytes_per_pixel;
                for x in x0..x1 {
                    let i = row + x as usize * bytes_per_pixel;
                    if swap_red_blue {
                        blue += u32::from(data[i]);
                        green += u32::from(data[i + 1]);
                        red += u32::from(data[i + 2]);
                    } else {
                        red += u32::from(data[i]);
                        green += u32::from(data[i + 1]);
                        blue += u32::from(data[i + 2]);
                    }
                    alpha += u32::from(data[i + 3]);
                    count += 1;
                }
            }

            let count = count.max(1);
            let o = (dy as usize * dst_width as usize + dx as usize) * 4;
            out[o] = (red / count) as u8;
            out[o + 1] = (green / count) as u8;
            out[o + 2] = (blue / count) as u8;
            out[o + 3] = (alpha / count) as u8;
        }
    }
    Ok(out)
}

/// The half-open source range `[start, end)` covered by output index `index`.
fn span(index: u32, source: u32, destination: u32) -> (u32, u32) {
    let index = u64::from(index);
    let source = u64::from(source);
    let destination = u64::from(destination);
    let start = index * source / destination;
    let end = (index + 1) * source / destination + 1;
    (start as u32, end.min(source) as u32)
}

/// Like [`fit_frame`], but rounds the result to a multiple of `align`.
pub fn fit_frame_aligned(
    frame: CapturedFrame,
    max_width: u32,
    max_height: u32,
    align: u32,
) -> Result<CapturedFrame, CodecError> {
    let (width, height) =
        fit_within_aligned(frame.width, frame.height, max_width, max_height, align);
    if (width, height) == (frame.width, frame.height) {
        return Ok(frame);
    }

    let data = box_downscale(
        &frame.data,
        frame.format,
        frame.width,
        frame.height,
        width,
        height,
    )?;

    Ok(CapturedFrame {
        width,
        height,
        format: FrameFormat::RawRgba,
        timestamp_us: frame.timestamp_us,
        data,
    })
}

/// Encodes a captured image as JPEG.
pub fn encode_jpeg(frame: &CapturedFrame, quality: u8) -> Result<EncodedImage, CodecError> {
    let rgba = to_rgba(frame.width, frame.height, frame.format, &frame.data)?;

    // JPEG has no alpha channel.
    let rgb = DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut data = Vec::new();
    JpegEncoder::new_with_quality(&mut data, quality)
        .encode_image(&rgb)
        .map_err(|err| CodecError::Codec(err.to_string()))?;

    Ok(EncodedImage {
        format: FrameFormat::Jpeg,
        width: frame.width,
        height: frame.height,
        data,
    })
}

/// Decodes a received frame into RGBA pixels.
pub fn decode_rgba(frame: &VideoFrame) -> Result<RgbaImage, CodecError> {
    let (width, height) = (frame.header.width, frame.header.height);

    match frame.header.format {
        FrameFormat::Jpeg => {
            let image = image::load_from_memory_with_format(&frame.payload, ImageFormat::Jpeg)
                .map_err(|err| CodecError::Codec(err.to_string()))?
                .to_rgba8();
            Ok(RgbaImage {
                width: image.width(),
                height: image.height(),
                pixels: image.into_raw(),
            })
        }
        FrameFormat::RawRgba | FrameFormat::RawBgra => {
            let image = to_rgba(width, height, frame.header.format, &frame.payload)?;
            Ok(RgbaImage {
                width,
                height,
                pixels: image.into_raw(),
            })
        }
        other => Err(CodecError::Unsupported(other)),
    }
}

/// Normalises a raw buffer to RGBA.
fn to_rgba(
    width: u32,
    height: u32,
    format: FrameFormat,
    data: &[u8],
) -> Result<ImageRgba, CodecError> {
    match format {
        FrameFormat::RawRgba => {
            ImageRgba::from_raw(width, height, data.to_vec()).ok_or(CodecError::SizeMismatch)
        }
        FrameFormat::RawBgra => {
            let mut rgba = data.to_vec();
            if rgba.len() != (width as usize) * (height as usize) * 4 {
                return Err(CodecError::SizeMismatch);
            }
            for pixel in rgba.chunks_exact_mut(4) {
                pixel.swap(0, 2); // BGRA -> RGBA
            }
            ImageRgba::from_raw(width, height, rgba).ok_or(CodecError::SizeMismatch)
        }
        other => Err(CodecError::Unsupported(other)),
    }
}

/// Converts RGBA pixels to NV12 (BT.709, limited range), the format hardware
/// H.264 encoders want.
///
/// Dimensions must be even: NV12 subsamples chroma 2×2.
pub fn rgba_to_nv12(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, CodecError> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(CodecError::SizeMismatch);
    }
    if rgba.len() < width as usize * height as usize * 4 {
        return Err(CodecError::SizeMismatch);
    }

    let width = width as usize;
    let height = height as usize;
    let mut nv12 = vec![0u8; width * height * 3 / 2];

    // Luma plane.
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let (red, green, blue) = (
                i32::from(rgba[i]),
                i32::from(rgba[i + 1]),
                i32::from(rgba[i + 2]),
            );
            nv12[y * width + x] = clamp_u8(((47 * red + 157 * green + 16 * blue + 128) >> 8) + 16);
        }
    }

    // Interleaved chroma plane, one (U, V) pair per 2×2 block, averaged.
    let chroma = width * height;
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (mut red, mut green, mut blue) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((y + dy) * width + (x + dx)) * 4;
                    red += i32::from(rgba[i]);
                    green += i32::from(rgba[i + 1]);
                    blue += i32::from(rgba[i + 2]);
                }
            }
            let (red, green, blue) = (red / 4, green / 4, blue / 4);

            let index = chroma + ((y / 2) * (width / 2) + (x / 2)) * 2;
            nv12[index] = clamp_u8(((-26 * red - 87 * green + 112 * blue + 128) >> 8) + 128);
            nv12[index + 1] = clamp_u8(((112 * red - 102 * green - 10 * blue + 128) >> 8) + 128);
        }
    }

    Ok(nv12)
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::FrameHeader;

    fn gradient(width: u32, height: u32) -> CapturedFrame {
        let mut data = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                data[offset] = (x * 255 / width) as u8;
                data[offset + 1] = (y * 255 / height) as u8;
                data[offset + 2] = 128;
                data[offset + 3] = 255;
            }
        }
        CapturedFrame {
            width,
            height,
            format: FrameFormat::RawRgba,
            timestamp_us: 0,
            data,
        }
    }

    #[test]
    fn jpeg_round_trips_dimensions() {
        let frame = gradient(64, 32);
        let encoded = encode_jpeg(&frame, DEFAULT_JPEG_QUALITY).unwrap();
        assert_eq!(encoded.format, FrameFormat::Jpeg);
        assert_eq!((encoded.width, encoded.height), (64, 32));
        assert!(
            encoded.data.len() < frame.data.len(),
            "jpeg should compress a gradient"
        );

        let video = VideoFrame::new(
            FrameHeader {
                sequence: 0,
                width: encoded.width,
                height: encoded.height,
                format: encoded.format,
                timestamp_us: 0,
            },
            encoded.data,
        );
        let decoded = decode_rgba(&video).unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 32));
        assert_eq!(decoded.pixels.len(), 64 * 32 * 4);
    }

    #[test]
    fn raw_rgba_passes_through() {
        let frame = gradient(4, 2);
        let video = VideoFrame::new(
            FrameHeader {
                sequence: 0,
                width: 4,
                height: 2,
                format: FrameFormat::RawRgba,
                timestamp_us: 0,
            },
            frame.data.clone(),
        );
        let decoded = decode_rgba(&video).unwrap();
        assert_eq!(decoded.pixels, frame.data);
    }

    #[test]
    fn bgra_is_swapped_to_rgba() {
        let frame = CapturedFrame {
            width: 1,
            height: 1,
            format: FrameFormat::RawBgra,
            timestamp_us: 0,
            data: vec![10, 20, 30, 255], // B G R A
        };
        let video = VideoFrame::new(
            FrameHeader {
                sequence: 0,
                width: 1,
                height: 1,
                format: FrameFormat::RawBgra,
                timestamp_us: 0,
            },
            frame.data,
        );
        assert_eq!(decode_rgba(&video).unwrap().pixels, vec![30, 20, 10, 255]);
    }

    #[test]
    fn nv12_has_the_right_size_and_layout() {
        let width = 4;
        let height = 2;
        let rgba = vec![255u8; width as usize * height as usize * 4];
        let nv12 = rgba_to_nv12(width, height, &rgba).unwrap();
        assert_eq!(nv12.len(), (width * height * 3 / 2) as usize);

        // White maps to luma 235 in limited range.
        for y in &nv12[..(width * height) as usize] {
            assert_eq!(*y, 235);
        }
        // Neutral chroma is (within rounding of) 128.
        for c in &nv12[(width * height) as usize..] {
            assert!(
                (i32::from(*c) - 128).abs() <= 1,
                "chroma {c} should be ~128"
            );
        }
    }

    #[test]
    fn nv12_rejects_odd_dimensions() {
        assert!(rgba_to_nv12(3, 2, &[0; 24]).is_err());
        assert!(rgba_to_nv12(2, 3, &[0; 24]).is_err());
    }

    #[test]
    fn fit_within_preserves_aspect_ratio() {
        assert_eq!(fit_within(3840, 2160, 1280, 800), (1280, 720));
        assert_eq!(fit_within(1280, 800, 1280, 800), (1280, 800));
        assert_eq!(fit_within(800, 600, 1280, 800), (800, 600));
        // Zero means "no limit".
        assert_eq!(fit_within(3840, 2160, 0, 0), (3840, 2160));
    }

    #[test]
    fn aligned_fit_rounds_to_even() {
        // 3840x2160 into 1280x800 is 1280x720 (already even).
        assert_eq!(fit_within_aligned(3840, 2160, 1280, 800, 2), (1280, 720));
        // An odd result is rounded down.
        assert_eq!(fit_within_aligned(1001, 999, 1001, 999, 2), (1000, 998));
    }

    #[test]
    fn fit_frame_downscales_and_keeps_timestamps() {
        let frame = gradient(64, 32);
        let fitted = fit_frame(frame.clone(), 32, 32).unwrap();
        assert_eq!((fitted.width, fitted.height), (32, 16));
        assert_eq!(fitted.format, FrameFormat::RawRgba);
        assert_eq!(fitted.data.len(), 32 * 16 * 4);
        assert_eq!(fitted.timestamp_us, frame.timestamp_us);
    }

    #[test]
    fn rejects_a_mismatched_buffer() {
        let frame = CapturedFrame {
            width: 4,
            height: 4,
            format: FrameFormat::RawRgba,
            timestamp_us: 0,
            data: vec![0; 8],
        };
        assert!(matches!(
            encode_jpeg(&frame, 80).unwrap_err(),
            CodecError::SizeMismatch
        ));
    }

    #[test]
    fn rejects_an_unsupported_format() {
        let frame = CapturedFrame {
            width: 1,
            height: 1,
            format: FrameFormat::H264,
            timestamp_us: 0,
            data: vec![0],
        };
        assert!(matches!(
            encode_jpeg(&frame, 80).unwrap_err(),
            CodecError::Unsupported(FrameFormat::H264)
        ));
    }
}
