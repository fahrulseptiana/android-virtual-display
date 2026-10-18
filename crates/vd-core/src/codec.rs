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
