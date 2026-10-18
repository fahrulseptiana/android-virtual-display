//! Media Foundation helpers.
//!
//! Media Foundation is the right encode API on Windows: it exposes the GPU
//! vendor's hardware encoder (AMD AMF, NVENC, Quick Sync) behind a uniform
//! interface, and it can consume D3D11 textures directly, so the captured frame
//! never has to be copied to the CPU.
//!
//! This module currently starts Media Foundation and lists the available H.264
//! encoders; the encoder itself is built on top.

use std::ffi::c_void;
use std::sync::OnceLock;

use log::debug;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFTransform, MFMediaType_Video, MFStartup, MFTEnumEx,
    MFT_ENUM_HARDWARE_URL_Attribute, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264,
    MFSTARTUP_NOSOCKET, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_REGISTER_TYPE_INFO, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

/// One H.264 encoder Media Foundation can see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Friendly name, for logs.
    pub name: String,
    /// True when the transform advertises a hardware URL, i.e. it runs on the GPU.
    pub hardware: bool,
}

/// Starts COM and Media Foundation once per process.
///
/// Safe to call from any thread; repeated calls are cheap.
pub fn ensure_started() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED.get_or_init(start).clone()
}

fn start() -> Result<(), String> {
    unsafe {
        // Media Foundation works with either apartment model; a thread that has
        // already chosen one (RPC_E_CHANGED_MODE) is fine.
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            debug!("CoInitializeEx returned {hr:?}; continuing");
        }
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|err| format!("MFStartup: {err}"))
    }
}

/// Lists the available H.264 encoders, hardware ones first.
pub fn h264_encoders() -> Result<Vec<EncoderInfo>, String> {
    ensure_started()?;
    let mut encoders = Vec::new();
    for activate in unsafe { enumerate_activates() }? {
        if let Some(info) = unsafe { describe(&activate) } {
            if !encoders.contains(&info) {
                encoders.push(info);
            }
        }
    }
    Ok(encoders)
}

/// Activates the best available H.264 encoder.
///
/// `prefer_hardware` puts GPU transforms first; otherwise the software encoder
/// is tried first (useful to isolate driver problems).
pub fn activate_h264_encoder(prefer_hardware: bool) -> Result<(EncoderInfo, IMFTransform), String> {
    ensure_started()?;

    let mut candidates = unsafe { enumerate_activates() }?;
    if !prefer_hardware {
        candidates.sort_by_key(|activate| {
            usize::from(
                unsafe { describe(activate) }
                    .map(|info| info.hardware)
                    .unwrap_or(false),
            )
        });
    }

    for activate in candidates {
        let info = unsafe { describe(&activate) };
        let name = info
            .as_ref()
            .map(|info| info.name.clone())
            .unwrap_or_else(|| "unnamed encoder".to_owned());

        // Asynchronous MFTs must be unlocked before they can be used. The
        // attribute is harmless on synchronous ones, so it is always set.
        let is_async = unsafe { activate.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if let Err(err) = unsafe { activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) } {
            debug!("could not unlock {name} on the activation: {err}");
        }
        debug!("encoder candidate {name}: async={is_async}");

        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                // The unlock must also be set on the transform itself, otherwise
                // ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER) fails with
                // MF_E_TRANSFORM_ASYNC_LOCKED.
                match transform.cast::<IMFAttributes>() {
                    Ok(attributes) => {
                        if let Err(err) =
                            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                        {
                            debug!("could not unlock {name} on the transform: {err}");
                        }
                    }
                    Err(err) => debug!("could not query IMFAttributes on {name}: {err}"),
                }

                let info = info.unwrap_or(EncoderInfo {
                    name,
                    hardware: false,
                });
                return Ok((info, transform));
            }
            Err(err) => debug!("could not activate {name}: {err}"),
        }
    }

    Err("no H.264 encoder could be activated".to_owned())
}

/// Enumerates encoder activations, hardware ones first.
unsafe fn enumerate_activates() -> Result<Vec<IMFActivate>, String> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    // First pass asks for hardware encoders only; the second lists everything,
    // so a machine without a GPU encoder still finds the software fallback.
    let passes = [
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        MFT_ENUM_FLAG_SORTANDFILTER,
    ];

    let mut activates = Vec::new();
    for flags in passes {
        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if let Err(err) = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output),
            &mut raw,
            &mut count,
        ) {
            debug!("MFTEnumEx({flags:?}) failed: {err}");
            continue;
        }

        for index in 0..count as usize {
            // Take ownership of each entry; the array itself is freed below and
            // does not run destructors, so this is what releases the references.
            let activate: Option<IMFActivate> = std::ptr::read(raw.add(index));
            if let Some(activate) = activate {
                activates.push(activate);
            }
        }

        CoTaskMemFree(Some(raw as *const c_void));
    }

    Ok(activates)
}

unsafe fn describe(activate: &IMFActivate) -> Option<EncoderInfo> {
    Some(EncoderInfo {
        name: friendly_name(activate).unwrap_or_else(|| "unnamed encoder".to_owned()),
        // Hardware transforms publish a hardware URL attribute.
        hardware: activate
            .GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute)
            .is_ok(),
    })
}

unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let length = activate
        .GetStringLength(&MFT_FRIENDLY_NAME_Attribute)
        .ok()? as usize;
    if length == 0 {
        return None;
    }

    let mut buffer = vec![0u16; length + 1];
    let mut written = 0u32;
    activate
        .GetString(
            &MFT_FRIENDLY_NAME_Attribute,
            &mut buffer,
            Some(&mut written),
        )
        .ok()?;

    let written = (written as usize).min(buffer.len());
    String::from_utf16(&buffer[..written]).ok()
}
