//! Video encoder controls (`ICodecAPI`).
//!
//! Media Foundation exposes the vendor-specific knobs — rate control, forced
//! keyframes, GOP length — through `ICodecAPI`, as `VARIANT`-valued properties
//! identified by GUID. None of it is required to encode, so every call here is
//! best-effort: an encoder that does not support a property is reported once and
//! otherwise ignored.

use log::debug;
use windows::core::Interface;
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::IMFTransform;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_PeakConstrainedVBR, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

/// A `VARIANT` holding a boolean.
fn variant_bool(value: bool) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: `vt` is set to match the union member that is written.
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    variant
}

/// A `VARIANT` holding an unsigned 32-bit value.
fn variant_u32(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: `vt` is set to match the union member that is written.
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

/// The encoder's optional `ICodecAPI` surface.
pub struct CodecControls {
    api: ICodecAPI,
    /// Whether the encoder will honour a forced keyframe.
    can_force_keyframe: bool,
}

impl CodecControls {
    /// Queries `ICodecAPI` on an encoder and configures what it supports.
    ///
    /// `bitrate` is in bits per second. Returns `None` when the encoder has no
    /// `ICodecAPI` at all, which is not an error.
    pub fn configure(transform: &IMFTransform, bitrate: u32) -> Option<Self> {
        // SAFETY: a plain interface query on an owned transform.
        let api: ICodecAPI = match transform.cast() {
            Ok(api) => api,
            Err(err) => {
                debug!("the encoder has no ICodecAPI: {err}");
                return None;
            }
        };

        // Capped variable bitrate: it follows the content, but never exceeds the
        // link's budget. Quality and latency matter more than a flat rate here.
        let rate_control = eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32;
        if let Err(err) = api_set(
            &api,
            &CODECAPI_AVEncCommonRateControlMode,
            &variant_u32(rate_control),
        ) {
            debug!("could not set the rate control mode: {err}");
        }
        if let Err(err) = api_set(
            &api,
            &CODECAPI_AVEncCommonMeanBitRate,
            &variant_u32(bitrate),
        ) {
            debug!("could not set the mean bitrate: {err}");
        }

        let can_force_keyframe = is_supported(&api, &CODECAPI_AVEncVideoForceKeyFrame);
        debug!(
            "encoder controls: rate control {}, separate bitrate {}, forced keyframe {}",
            is_supported(&api, &CODECAPI_AVEncCommonRateControlMode),
            is_supported(&api, &CODECAPI_AVEncCommonMeanBitRate),
            can_force_keyframe,
        );

        Some(Self {
            api,
            can_force_keyframe,
        })
    }

    /// Whether the encoder will honour [`Self::force_keyframe`].
    pub fn can_force_keyframe(&self) -> bool {
        self.can_force_keyframe
    }

    /// Asks the encoder for an IDR on the next frame.
    ///
    /// Needed whenever a viewer joins a stream that is already running: without
    /// it the encoder keeps emitting predicted frames, and a decoder that has
    /// just connected has nothing to predict from, so the screen stays blank
    /// until the next scheduled keyframe.
    pub fn force_keyframe(&self) -> Result<(), String> {
        api_set(
            &self.api,
            &CODECAPI_AVEncVideoForceKeyFrame,
            &variant_bool(true),
        )
        .map_err(|err| err.to_string())
    }
}

/// Sets a codec property, treating an unsupported one as a soft failure.
fn api_set(
    api: &ICodecAPI,
    property: &windows::core::GUID,
    value: &VARIANT,
) -> windows::core::Result<()> {
    // SAFETY: `property` and `value` outlive the call.
    unsafe { api.SetValue(property, value) }
}

/// Whether the encoder advertises a property.
fn is_supported(api: &ICodecAPI, property: &windows::core::GUID) -> bool {
    // SAFETY: `property` outlives the call.
    unsafe { api.IsSupported(property).is_ok() }
}
