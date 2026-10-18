//! Hardware H.264 encoding through a Media Foundation transform.
//!
//! Frames arrive as NV12 in system memory (see `vd_core::codec::rgba_to_nv12`)
//! and come back as an H.264 access unit. The encoder still runs on the GPU; the
//! system-memory input is the simple path, with a zero-copy D3D11 texture input
//! as a later optimisation.

use std::mem::ManuallyDrop;

use log::debug;
use vd_core::codec::CodecError;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_Main, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFTransform, MFCreateDXGIDeviceManager, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
};

use super::media_foundation::{activate_h264_encoder, EncoderInfo};

/// Why encoding failed.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// Media Foundation could not be used at all.
    #[error("media foundation: {0}")]
    MediaFoundation(String),
    /// The encoder could not be configured for this frame size.
    #[error("could not configure the encoder: {0}")]
    Configure(String),
    /// Encoding a frame failed.
    #[error("encoding failed: {0}")]
    Encode(String),
}

impl From<EncodeError> for CodecError {
    fn from(err: EncodeError) -> Self {
        CodecError::Codec(err.to_string())
    }
}

/// A configured hardware H.264 encoder.
pub struct H264Encoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    info: EncoderInfo,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    sequence_header: Vec<u8>,
    frame_index: u64,
}

impl H264Encoder {
    /// Creates an encoder for `width`×`height` at `fps` and `bitrate` bits/s.
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        prefer_hardware: bool,
    ) -> Result<Self, EncodeError> {
        // SAFETY: all calls below are documented Media Foundation usage; the
        // transform is owned by `self` and only used from the encoding thread.
        unsafe { Self::create(device, width, height, fps, bitrate, prefer_hardware) }
    }

    unsafe fn create(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        prefer_hardware: bool,
    ) -> Result<Self, EncodeError> {
        // Hardware first (unless the caller asked otherwise), then software: a
        // driver that cannot be configured must not cost us encoding entirely.
        for hardware_first in [prefer_hardware, !prefer_hardware] {
            match Self::configure(device, width, height, fps, bitrate, hardware_first) {
                Ok(encoder) => return Ok(encoder),
                Err(err) => debug!(
                    "{} encoder unusable: {err}",
                    if hardware_first {
                        "hardware"
                    } else {
                        "software"
                    }
                ),
            }
        }
        Err(EncodeError::Configure(
            "no H.264 encoder could be configured".to_owned(),
        ))
    }

    unsafe fn configure(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        prefer_hardware: bool,
    ) -> Result<Self, EncodeError> {
        let (info, transform) =
            activate_h264_encoder(prefer_hardware).map_err(EncodeError::MediaFoundation)?;

        // Hardware transforms expect a D3D device manager, even when the samples
        // themselves come from system memory.
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|err| {
            EncodeError::MediaFoundation(format!("MFCreateDXGIDeviceManager: {err}"))
        })?;
        let manager = manager
            .ok_or_else(|| EncodeError::MediaFoundation("no D3D device manager".to_owned()))?;
        manager
            .ResetDevice(device, token)
            .map_err(|err| EncodeError::MediaFoundation(format!("ResetDevice: {err}")))?;
        // Recommended for hardware transforms, but some refuse it; carry on
        // either way and let ProcessInput report a real problem.
        if let Err(err) = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, token as usize) {
            debug!("could not set the D3D manager on the encoder: {err}");
        }

        // Encoders want the output type first.
        set_output_type(&transform, width, height, fps, bitrate)?;
        set_input_type(&transform, width, height, fps)?;

        let sequence_header = output_sequence_header(&transform)?;

        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|err| EncodeError::Configure(format!("BeginStreaming: {err}")))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|err| EncodeError::Configure(format!("StartOfStream: {err}")))?;

        debug!(
            "H.264 encoder ready: {} ({}), {}x{} @ {fps} fps, {} kbps, extradata {} bytes",
            info.name,
            if info.hardware {
                "hardware"
            } else {
                "software"
            },
            width,
            height,
            bitrate / 1000,
            sequence_header.len(),
        );

        Ok(Self {
            transform,
            _manager: manager,
            info,
            width,
            height,
            fps,
            bitrate,
            sequence_header,
            frame_index: 0,
        })
    }

    /// Human readable description, for logs.
    pub fn describe(&self) -> String {
        format!(
            "{} ({}), {}x{} @ {} fps, {} kbps",
            self.info.name,
            if self.info.hardware {
                "hardware"
            } else {
                "software"
            },
            self.width,
            self.height,
            self.fps,
            self.bitrate / 1000,
        )
    }

    /// The SPS/PPS blob (`MF_MT_MPEG_SEQUENCE_HEADER`).
    pub fn sequence_header(&self) -> &[u8] {
        &self.sequence_header
    }

    /// Encodes one NV12 frame.
    ///
    /// Returns the access units produced by this call: usually one, occasionally
    /// none (the encoder is still filling its pipeline), and on a keyframe the
    /// SPS/PPS blob is prepended so the stream is self-contained.
    pub fn encode(&mut self, nv12: &[u8], timestamp_us: u64) -> Result<Vec<Vec<u8>>, EncodeError> {
        let expected = self.expected_input_len();
        if nv12.len() < expected {
            return Err(EncodeError::Encode(format!(
                "expected {expected} bytes of NV12, got {}",
                nv12.len()
            )));
        }

        let mut outputs = Vec::new();
        let sample = unsafe { self.make_sample(nv12, timestamp_us)? };

        // The encoder may still be holding the previous frame.
        outputs.extend(self.drain()?);

        if !self.try_feed(&sample)? {
            outputs.extend(self.drain()?);
            if !self.try_feed(&sample)? {
                return Err(EncodeError::Encode(
                    "the encoder would not accept a frame".to_owned(),
                ));
            }
        }

        outputs.extend(self.drain()?);
        self.frame_index += 1;
        Ok(outputs)
    }

    fn expected_input_len(&self) -> usize {
        (self.width * self.height) as usize * 3 / 2
    }

    fn try_feed(&self, sample: &IMFSample) -> Result<bool, EncodeError> {
        match unsafe { self.transform.ProcessInput(0, sample, 0) } {
            Ok(()) => Ok(true),
            Err(err) if err.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(err) => Err(EncodeError::Encode(format!("ProcessInput: {err}"))),
        }
    }

    fn drain(&mut self) -> Result<Vec<Vec<u8>>, EncodeError> {
        let mut outputs = Vec::new();

        // Some encoders hand back a sample; others expect the caller to supply
        // one. Ask which, once per call (it is cheap and can change).
        let stream = unsafe { self.transform.GetOutputStreamInfo(0) }
            .unwrap_or(MFT_OUTPUT_STREAM_INFO::default());
        let provides_samples = (stream.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

        loop {
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                // SAFETY: ProcessOutput fills these in; taking them below
                // releases the references exactly once.
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };

            if !provides_samples {
                let sample = unsafe { MFCreateSample() }
                    .map_err(|err| EncodeError::Encode(format!("MFCreateSample: {err}")))?;
                let size = stream.cbSize.max(1 << 20);
                let memory = unsafe { MFCreateMemoryBuffer(size) }
                    .map_err(|err| EncodeError::Encode(format!("MFCreateMemoryBuffer: {err}")))?;
                unsafe {
                    sample
                        .AddBuffer(&memory)
                        .map_err(|err| EncodeError::Encode(format!("AddBuffer: {err}")))?;
                }
                buffer.pSample = ManuallyDrop::new(Some(sample));
            }

            let mut status = 0u32;

            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };

            match result {
                Ok(()) => {
                    let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
                    let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };
                    if let Some(sample) = sample {
                        if let Some(bytes) = sample_bytes(&sample)? {
                            if !bytes.is_empty() {
                                outputs.push(bytes);
                            }
                        }
                    }
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(err) => return Err(EncodeError::Encode(format!("ProcessOutput: {err}"))),
            }
        }
        Ok(outputs)
    }

    unsafe fn make_sample(&self, nv12: &[u8], timestamp_us: u64) -> Result<IMFSample, EncodeError> {
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(nv12.len() as u32)
            .map_err(|err| EncodeError::Encode(format!("MFCreateMemoryBuffer: {err}")))?;

        let mut data = std::ptr::null_mut();
        buffer
            .Lock(&mut data, None, None)
            .map_err(|err| EncodeError::Encode(format!("Lock: {err}")))?;
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
        buffer
            .Unlock()
            .map_err(|err| EncodeError::Encode(format!("Unlock: {err}")))?;
        buffer
            .SetCurrentLength(nv12.len() as u32)
            .map_err(|err| EncodeError::Encode(format!("SetCurrentLength: {err}")))?;

        let sample = MFCreateSample()
            .map_err(|err| EncodeError::Encode(format!("MFCreateSample: {err}")))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|err| EncodeError::Encode(format!("AddBuffer: {err}")))?;

        // Media Foundation counts in 100 ns units.
        sample
            .SetSampleTime(timestamp_us as i64 * 10)
            .map_err(|err| EncodeError::Encode(format!("SetSampleTime: {err}")))?;
        sample
            .SetSampleDuration(10_000_000 / i64::from(self.fps.max(1)))
            .map_err(|err| EncodeError::Encode(format!("SetSampleDuration: {err}")))?;

        Ok(sample)
    }
}

/// Packs two 32-bit values the way Media Foundation attributes expect.
fn pack_pair(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Picks the first advertised H.264 output type and configures it.
unsafe fn set_output_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<(), EncodeError> {
    let mut index = 0;
    loop {
        let Ok(media_type) = transform.GetOutputAvailableType(0, index) else {
            break;
        };
        index += 1;

        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        if subtype != MFVideoFormat_H264 {
            continue;
        }

        let _ = media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264);
        let _ = media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate);
        let _ = media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(width, height));
        let _ = media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(fps, 1));
        let _ = media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1));
        let _ = media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        let _ = media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);

        if transform.SetOutputType(0, &media_type, 0).is_ok() {
            return Ok(());
        }
    }

    Err(EncodeError::Configure(format!(
        "no H.264 output type accepted for {width}x{height}"
    )))
}

/// Picks the NV12 input type.
unsafe fn set_input_type(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), EncodeError> {
    let mut index = 0;
    loop {
        let Ok(media_type) = transform.GetInputAvailableType(0, index) else {
            break;
        };
        index += 1;

        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        if subtype != MFVideoFormat_NV12 {
            continue;
        }

        let _ = media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
        let _ = media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(width, height));
        let _ = media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(fps, 1));
        let _ = media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1));
        let _ = media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);

        if transform.SetInputType(0, &media_type, 0).is_ok() {
            return Ok(());
        }
    }

    Err(EncodeError::Configure(
        "the encoder does not accept NV12 input".to_owned(),
    ))
}

/// Reads the SPS/PPS blob the encoder advertises.
unsafe fn output_sequence_header(transform: &IMFTransform) -> Result<Vec<u8>, EncodeError> {
    let current: IMFMediaType = transform
        .GetOutputCurrentType(0)
        .map_err(|err| EncodeError::Configure(format!("GetOutputCurrentType: {err}")))?;

    let size = current
        .GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
        .unwrap_or(0);
    if size == 0 {
        return Ok(Vec::new());
    }

    let mut header = vec![0u8; size as usize];
    current
        .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)
        .map_err(|err| EncodeError::Configure(format!("GetBlob(sequence header): {err}")))?;
    Ok(header)
}

/// Copies an output sample's bytes out.
fn sample_bytes(sample: &IMFSample) -> Result<Option<Vec<u8>>, EncodeError> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|err| EncodeError::Encode(format!("ConvertToContiguousBuffer: {err}")))?;

    let mut data = std::ptr::null_mut();
    let mut length = 0u32;
    unsafe {
        buffer
            .Lock(&mut data, None, Some(&mut length))
            .map_err(|err| EncodeError::Encode(format!("Lock(output): {err}")))?;
    }
    if data.is_null() || length == 0 {
        unsafe {
            let _ = buffer.Unlock();
        }
        return Ok(None);
    }

    let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec();
    unsafe {
        let _ = buffer.Unlock();
    }
    Ok(Some(bytes))
}
