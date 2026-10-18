//! The shared Direct3D 11 device.
//!
//! Capture and encoding must use the *same* device: DXGI hands out textures on
//! it, and a hardware encoder can only take textures it can see (`D3D11` device
//! manager). Creating it once and passing it around also avoids a GPU↔GPU copy.

use log::info;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};

/// A Direct3D 11 device and its immediate context.
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

/// Creates a hardware D3D11 device with BGRA and video support.
pub fn create_device() -> Result<GpuDevice, String> {
    let mut device = None;
    let mut context = None;

    // SAFETY: a plain device creation call; the returned interfaces are owned
    // by the returned value.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // BGRA is needed by DXGI capture; VIDEO_SUPPORT is needed before a
            // hardware encoder will accept this device as its manager.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|err| format!("D3D11CreateDevice: {err}"))?;
    }

    let device = device.ok_or_else(|| "no D3D11 device was created".to_owned())?;
    let context = context.ok_or_else(|| "no D3D11 context was created".to_owned())?;
    info!("Direct3D 11 device created (shared by capture and encoding)");

    Ok(GpuDevice { device, context })
}
