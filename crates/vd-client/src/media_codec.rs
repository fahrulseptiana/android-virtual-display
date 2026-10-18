//! H.264 decoding through Android's NDK media API (`libmediandk`).
//!
//! MediaCodec is the platform's hardware decoder. The NDK exposes it as a C API
//! (`AMediaCodec`), which the `ndk` crate does not wrap, so the handful of
//! functions used here are declared directly.
//!
//! Output is requested as NV12 (`COLOR_FormatYUV420SemiPlanar`). When a decoder
//! insists on its own layout the raw planes are still handled, as long as it is
//! planar or semi-planar; `COLOR_FormatYUV420Flexible` needs the image API and
//! is reported rather than guessed at.

#![cfg(target_os = "android")]

use std::ffi::{c_char, c_void, CString};
use std::ptr;

use vd_core::codec::{CodecError, RgbaImage};

const AMEDIA_OK: i32 = 0;

const AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

const AMEDIACODEC_INFO_TRY_AGAIN_LATER: isize = -1;
const AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
const AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;

/// `COLOR_FormatYUV420Planar` — three separate planes.
const COLOR_FORMAT_YUV420_PLANAR: i32 = 19;
/// `COLOR_FormatYUV420SemiPlanar` — NV12/NV21.
const COLOR_FORMAT_YUV420_SEMI_PLANAR: i32 = 21;
/// `COLOR_FormatYUV420Flexible` — layout must be asked for via `AMediaCodec_getOutputImage`.
const COLOR_FORMAT_YUV420_FLEXIBLE: i32 = 2135033992;

/// How long to wait for a buffer before giving up on this call.
const DEQUEUE_TIMEOUT_US: i64 = 1_000;

#[repr(C)]
struct AMediaCodec {
    _private: [u8; 0],
}

#[repr(C)]
struct AMediaFormat {
    _private: [u8; 0],
}

/// Mirrors `AMediaCodecBufferInfo`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AMediaCodecBufferInfo {
    offset: i32,
    size: i32,
    presentation_time_us: i64,
    flags: u32,
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_configure(
        codec: *mut AMediaCodec,
        format: *mut AMediaFormat,
        surface: *mut c_void,
        crypto: *mut c_void,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> i32;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> i32;
    fn AMediaCodec_delete(codec: *mut AMediaCodec) -> i32;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    fn AMediaCodec_getInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        out_size: *mut usize,
    ) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        offset: usize,
        size: usize,
        time_us: u64,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut AMediaCodec,
        info: *mut AMediaCodecBufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        out_size: *mut usize,
    ) -> *mut u8;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, index: usize, render: bool) -> i32;
    fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(format: *mut AMediaFormat);
    fn AMediaFormat_setString(
        format: *mut AMediaFormat,
        key: *const c_char,
        value: *const c_char,
    ) -> i32;
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, key: *const c_char, value: i32) -> i32;
    fn AMediaFormat_setBuffer(
        format: *mut AMediaFormat,
        key: *const c_char,
        data: *const c_void,
        size: usize,
    ) -> i32;
    fn AMediaFormat_getInt32(format: *mut AMediaFormat, key: *const c_char, out: *mut i32) -> bool;
}

/// Splits an Annex-B byte stream into NAL units, without start codes.
///
/// The encoder hands us SPS and PPS this way; MediaCodec wants them separately
/// as `csd-0` and `csd-1`.
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;

    let mut index = 0;
    while index + 3 <= data.len() {
        let three = &data[index..index + 3] == [0, 0, 1];
        let four = index + 4 <= data.len() && &data[index..index + 4] == [0, 0, 0, 1];
        if three || four {
            if let Some(previous) = start {
                let end = index;
                if end > previous {
                    units.push(&data[previous..end]);
                }
            }
            index += if four { 4 } else { 3 };
            start = Some(index);
            continue;
        }
        index += 1;
    }

    if let Some(previous) = start {
        if previous < data.len() {
            units.push(&data[previous..]);
        }
    }
    units
}

/// A configured H.264 decoder.
pub struct H264Decoder {
    codec: *mut AMediaCodec,
    width: u32,
    height: u32,
    color_format: i32,
    configured: bool,
}

// The decoder lives on the connection thread; nothing else touches it.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    /// Creates a decoder for the given stream. `extradata` is the Annex-B SPS/PPS blob.
    pub fn new(width: u32, height: u32, extradata: &[u8]) -> Result<Self, CodecError> {
        let mime = CString::new("video/avc").expect("static string");
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::Codec(
                "no H.264 decoder is available".to_owned(),
            ));
        }

        let mut decoder = Self {
            codec,
            width,
            height,
            color_format: COLOR_FORMAT_YUV420_SEMI_PLANAR,
            configured: false,
        };
        decoder.configure(width, height, extradata)?;
        Ok(decoder)
    }

    fn configure(&mut self, width: u32, height: u32, extradata: &[u8]) -> Result<(), CodecError> {
        let format = unsafe { AMediaFormat_new() };
        if format.is_null() {
            return Err(CodecError::Codec("AMediaFormat_new failed".to_owned()));
        }

        let mime = CString::new("video/avc").expect("static string");
        let mime_key = CString::new("mime").expect("static string");
        let width_key = CString::new("width").expect("static string");
        let height_key = CString::new("height").expect("static string");
        let color_key = CString::new("color-format").expect("static string");

        let ok = unsafe {
            AMediaFormat_setString(format, mime_key.as_ptr(), mime.as_ptr());
            AMediaFormat_setInt32(format, width_key.as_ptr(), width as i32);
            AMediaFormat_setInt32(format, height_key.as_ptr(), height as i32);
            // Ask for NV12 explicitly: then the output layout is known.
            AMediaFormat_setInt32(format, color_key.as_ptr(), COLOR_FORMAT_YUV420_SEMI_PLANAR);

            // SPS and PPS travel as csd-0 / csd-1.
            let units = split_annex_b(extradata);
            for (index, key) in ["csd-0", "csd-1"].iter().enumerate() {
                if let Some(unit) = units.get(index) {
                    let key = CString::new(*key).expect("static string");
                    AMediaFormat_setBuffer(format, key.as_ptr(), unit.as_ptr().cast(), unit.len());
                }
            }

            AMediaCodec_configure(self.codec, format, ptr::null_mut(), ptr::null_mut(), 0)
        };

        unsafe { AMediaFormat_delete(format) };

        if ok != AMEDIA_OK {
            return Err(CodecError::Codec(format!(
                "AMediaCodec_configure failed ({ok})"
            )));
        }

        let started = unsafe { AMediaCodec_start(self.codec) };
        if started != AMEDIA_OK {
            return Err(CodecError::Codec(format!(
                "AMediaCodec_start failed ({started})"
            )));
        }

        self.width = width;
        self.height = height;
        self.configured = true;
        Ok(())
    }

    /// Feeds one access unit and returns whatever frames came out.
    pub fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp_us: u64,
    ) -> Result<Vec<RgbaImage>, CodecError> {
        if !self.configured {
            return Err(CodecError::Codec("decoder is not configured".to_owned()));
        }

        // Give it the access unit (retrying briefly if it has no room yet).
        if !access_unit.is_empty() {
            let index = unsafe { AMediaCodec_dequeueInputBuffer(self.codec, DEQUEUE_TIMEOUT_US) };
            if index >= 0 {
                let mut capacity = 0usize;
                let buffer = unsafe {
                    AMediaCodec_getInputBuffer(self.codec, index as usize, &mut capacity)
                };
                if !buffer.is_null() && capacity >= access_unit.len() {
                    unsafe {
                        ptr::copy_nonoverlapping(access_unit.as_ptr(), buffer, access_unit.len());
                    }
                    unsafe {
                        AMediaCodec_queueInputBuffer(
                            self.codec,
                            index as usize,
                            0,
                            access_unit.len(),
                            timestamp_us * 10,
                            0,
                        );
                    }
                }
            }
        }

        self.drain()
    }

    /// Collects everything the decoder has ready.
    fn drain(&mut self) -> Result<Vec<RgbaImage>, CodecError> {
        let mut frames = Vec::new();
        loop {
            let mut info = AMediaCodecBufferInfo::default();
            let index = unsafe {
                AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, DEQUEUE_TIMEOUT_US)
            };

            match index {
                AMEDIACODEC_INFO_TRY_AGAIN_LATER => break,
                AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED => continue,
                AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED => {
                    self.read_output_format()?;
                    continue;
                }
                index if index >= 0 => {
                    // Codec-config buffers carry no picture.
                    if info.flags & AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG != 0 {
                        unsafe {
                            AMediaCodec_releaseOutputBuffer(self.codec, index as usize, false)
                        };
                        continue;
                    }
                    let index = index as usize;
                    let frame = self.read_output(index, &info);
                    unsafe { AMediaCodec_releaseOutputBuffer(self.codec, index, false) };
                    if let Some(frame) = frame? {
                        frames.push(frame);
                    }
                }
                _ => break,
            }
        }
        Ok(frames)
    }

    /// Picks up size and colour format after the decoder re-negotiates.
    fn read_output_format(&mut self) -> Result<(), CodecError> {
        let format = unsafe { AMediaCodec_getOutputFormat(self.codec) };
        if format.is_null() {
            return Ok(());
        }

        let width_key = CString::new("width").expect("static string");
        let height_key = CString::new("height").expect("static string");
        let color_key = CString::new("color-format").expect("static string");

        let mut width = 0i32;
        let mut height = 0i32;
        let mut color_format = self.color_format;
        unsafe {
            if AMediaFormat_getInt32(format, width_key.as_ptr(), &mut width) && width > 0 {
                self.width = width as u32;
            }
            if AMediaFormat_getInt32(format, height_key.as_ptr(), &mut height) && height > 0 {
                self.height = height as u32;
            }
            if AMediaFormat_getInt32(format, color_key.as_ptr(), &mut color_format) {
                self.color_format = color_format;
            }
            AMediaFormat_delete(format);
        }

        log::debug!(
            "H.264 decoder output: {}x{}, colour format {color_format}",
            self.width,
            self.height
        );

        if self.color_format == COLOR_FORMAT_YUV420_FLEXIBLE {
            log::warn!(
                "decoder produced flexible YUV, which needs the image API; frames will be skipped"
            );
        }
        Ok(())
    }

    /// Reads one decoded frame and converts it to RGBA.
    fn read_output(
        &self,
        index: usize,
        info: &AMediaCodecBufferInfo,
    ) -> Result<Option<RgbaImage>, CodecError> {
        if info.size <= 0 {
            return Ok(None);
        }

        let mut capacity = 0usize;
        let buffer = unsafe { AMediaCodec_getOutputBuffer(self.codec, index, &mut capacity) };
        if buffer.is_null() {
            return Ok(None);
        }

        let start = info.offset.max(0) as usize;
        let end = (start + info.size as usize).min(capacity);
        if end <= start {
            return Ok(None);
        }
        let data = unsafe { std::slice::from_raw_parts(buffer.add(start), end - start) };

        let (width, height) = (self.width as usize, self.height as usize);
        let frame = match self.color_format {
            COLOR_FORMAT_YUV420_SEMI_PLANAR => yuv420_semi_planar_to_rgba(data, width, height),
            COLOR_FORMAT_YUV420_PLANAR => yuv420_planar_to_rgba(data, width, height),
            other => {
                return Err(CodecError::Codec(format!(
                    "unsupported decoder output colour format {other}"
                )))
            }
        }?;
        Ok(Some(frame))
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            AMediaCodec_stop(self.codec);
            AMediaCodec_delete(self.codec);
        }
    }
}

/// Converts NV12 (Y plane then interleaved UV) to RGBA, BT.709 limited range.
fn yuv420_semi_planar_to_rgba(
    data: &[u8],
    width: usize,
    height: usize,
) -> Result<RgbaImage, CodecError> {
    let luma = width * height;
    let chroma = luma / 2;
    if width == 0 || height == 0 || data.len() < luma + chroma {
        return Err(CodecError::Codec("NV12 buffer is too small".to_owned()));
    }

    let mut pixels = vec![0u8; luma * 4];
    for row in 0..height {
        for column in 0..width {
            let y = data[row * width + column];
            // Both U and V are shared by a 2x2 block.
            let uv = luma + (row / 2) * width + (column / 2) * 2;
            let u = data[uv];
            let v = data[uv + 1];
            let (red, green, blue) = yuv_to_rgb(y, u, v);

            let out = (row * width + column) * 4;
            pixels[out] = red;
            pixels[out + 1] = green;
            pixels[out + 2] = blue;
            pixels[out + 3] = 255;
        }
    }

    Ok(RgbaImage {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// Converts I420 (separate U and V planes) to RGBA, BT.709 limited range.
fn yuv420_planar_to_rgba(
    data: &[u8],
    width: usize,
    height: usize,
) -> Result<RgbaImage, CodecError> {
    let luma = width * height;
    let chroma = luma / 4;
    if width == 0 || height == 0 || data.len() < luma + chroma * 2 {
        return Err(CodecError::Codec("I420 buffer is too small".to_owned()));
    }

    let u_plane = luma;
    let v_plane = luma + chroma;
    let mut pixels = vec![0u8; luma * 4];

    for row in 0..height {
        for column in 0..width {
            let y = data[row * width + column];
            let chroma_index = (row / 2) * (width / 2) + column / 2;
            let u = data[u_plane + chroma_index];
            let v = data[v_plane + chroma_index];
            let (red, green, blue) = yuv_to_rgb(y, u, v);

            let out = (row * width + column) * 4;
            pixels[out] = red;
            pixels[out + 1] = green;
            pixels[out + 2] = blue;
            pixels[out + 3] = 255;
        }
    }

    Ok(RgbaImage {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// BT.709 limited-range YUV to RGB, matching what the host encoder was fed.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;

    let red = (298 * c + 459 * e + 128) >> 8;
    let green = (298 * c - 55 * d - 136 * e + 128) >> 8;
    let blue = (298 * c + 541 * d + 128) >> 8;

    (
        red.clamp(0, 255) as u8,
        green.clamp(0, 255) as u8,
        blue.clamp(0, 255) as u8,
    )
}
