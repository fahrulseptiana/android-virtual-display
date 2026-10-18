//! Desktop capture through DXGI Desktop Duplication.
//!
//! This is the real [`FrameSource`]: it duplicates a monitor's output on the GPU
//! and copies each new frame into a CPU-readable staging texture.
//!
//! It is intentionally the only Windows-specific capture code. Everything
//! downstream (encoding, transport) only sees [`CapturedFrame`]s.
//!
//! Two DXGI behaviours are handled explicitly:
//!
//! * `DXGI_ERROR_WAIT_TIMEOUT` — the desktop has not changed; report "no frame".
//! * `DXGI_ERROR_ACCESS_LOST` — the duplication died (mode change, UAC, lock,
//!   display switch); rebuild it and carry on.

use std::time::Instant;

use log::{debug, info};
use vd_core::capture::{CaptureError, CapturedFrame, FrameSource};
use vd_core::video::FrameFormat;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

/// How long to wait for a new frame before reporting "nothing changed".
const ACQUIRE_TIMEOUT_MS: u32 = 50;

/// Bytes per pixel of the dupicated desktop (`DXGI_FORMAT_B8G8R8A8_UNORM`).
const BYTES_PER_PIXEL: u32 = 4;

/// Captures a monitor through Desktop Duplication.
pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Kept so the duplication can be rebuilt after `ACCESS_LOST`.
    output: IDXGIOutput1,
    duplication: IDXGIOutputDuplication,
    /// CPU-readable copy target, created lazily and dropped on recreation.
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    started: Instant,
}

impl DxgiCapture {
    /// Attaches to `output_index` (0 is the primary monitor).
    pub fn new(output_index: u32) -> Result<Self, CaptureError> {
        // SAFETY: every call below is a documented D3D/DXGI call; the returned
        // interfaces are owned for the lifetime of `self` and only used from the
        // capture thread.
        unsafe { Self::create(output_index) }
    }

    unsafe fn create(output_index: u32) -> Result<Self, CaptureError> {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
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
        .map_err(|err| CaptureError::Unavailable(format!("D3D11CreateDevice: {err}")))?;

        let device = device
            .ok_or_else(|| CaptureError::Unavailable("no D3D11 device was created".to_owned()))?;
        let context = context.ok_or_else(|| {
            CaptureError::Unavailable("no D3D11 immediate context was created".to_owned())
        })?;

        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|err| CaptureError::Unavailable(format!("IDXGIDevice: {err}")))?;
        let adapter: IDXGIAdapter = dxgi_device
            .GetAdapter()
            .map_err(|err| CaptureError::Unavailable(format!("GetAdapter: {err}")))?;
        let output = adapter.EnumOutputs(output_index).map_err(|err| {
            CaptureError::Unavailable(format!("EnumOutputs({output_index}): {err}"))
        })?;
        let output: IDXGIOutput1 = output
            .cast()
            .map_err(|err| CaptureError::Unavailable(format!("IDXGIOutput1: {err}")))?;
        let duplication = output
            .DuplicateOutput(&device)
            .map_err(|err| CaptureError::Unavailable(format!("DuplicateOutput: {err}")))?;

        let desc = duplication.GetDesc();
        let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        info!("DXGI desktop duplication on output {output_index}: {width}x{height}");

        Ok(Self {
            device,
            context,
            output,
            duplication,
            staging: None,
            width,
            height,
            started: Instant::now(),
        })
    }

    /// Rebuilds the duplication after `DXGI_ERROR_ACCESS_LOST`.
    fn recreate(&mut self) -> Result<(), CaptureError> {
        let (duplication, width, height) = unsafe {
            let duplication = self
                .output
                .DuplicateOutput(&self.device)
                .map_err(|err| CaptureError::Failed(format!("DuplicateOutput: {err}")))?;
            let desc = duplication.GetDesc();
            (duplication, desc.ModeDesc.Width, desc.ModeDesc.Height)
        };

        self.width = width;
        self.height = height;
        self.duplication = duplication;
        // The desktop may have changed size.
        self.staging = None;
        debug!(
            "desktop duplication rebuilt: {}x{}",
            self.width, self.height
        );
        Ok(())
    }

    /// Creates the staging texture on first use (or after a resize).
    fn ensure_staging(&mut self) -> Result<(), CaptureError> {
        if self.staging.is_some() {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let mut staging = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|err| CaptureError::Failed(format!("CreateTexture2D: {err}")))?;
        }
        self.staging = staging;
        Ok(())
    }

    /// Copies the acquired desktop texture into CPU memory.
    fn copy_desktop(
        &mut self,
        resource: Option<IDXGIResource>,
    ) -> Result<CapturedFrame, CaptureError> {
        let resource = resource
            .ok_or_else(|| CaptureError::Failed("desktop frame had no resource".to_owned()))?;
        let source: ID3D11Texture2D = resource.cast().map_err(|err| {
            CaptureError::Failed(format!("desktop resource is not a texture: {err}"))
        })?;
        let staging = self
            .staging
            .clone()
            .ok_or_else(|| CaptureError::Failed("staging texture is missing".to_owned()))?;

        let source: ID3D11Resource = source
            .cast()
            .map_err(|err| CaptureError::Failed(format!("ID3D11Resource: {err}")))?;
        let destination: ID3D11Resource = staging
            .cast()
            .map_err(|err| CaptureError::Failed(format!("ID3D11Resource: {err}")))?;

        let row_bytes = (self.width * BYTES_PER_PIXEL) as usize;
        let mut data = Vec::with_capacity(row_bytes * self.height as usize);

        unsafe {
            self.context.CopyResource(&destination, &source);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&destination, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|err| CaptureError::Failed(format!("Map: {err}")))?;

            // The staging texture is padded to `RowPitch`, so copy row by row.
            for row in 0..self.height as usize {
                let start = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                data.extend_from_slice(std::slice::from_raw_parts(start, row_bytes));
            }

            self.context.Unmap(&destination, 0);
        }

        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            format: FrameFormat::RawBgra,
            timestamp_us: self.started.elapsed().as_micros() as u64,
            data,
        })
    }
}

impl FrameSource for DxgiCapture {
    fn describe(&self) -> String {
        format!("DXGI desktop duplication {}x{}", self.width, self.height)
    }

    fn next_frame(&mut self) -> Result<Option<CapturedFrame>, CaptureError> {
        self.ensure_staging()?;

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource)
        };

        match acquired {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                debug!("desktop duplication access lost; rebuilding");
                self.recreate()?;
                return Ok(None);
            }
            Err(err) => return Err(CaptureError::Failed(format!("AcquireNextFrame: {err}"))),
        }

        // Once acquired, the frame must be released on every path.
        let copied = self.copy_desktop(resource);
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
        copied.map(Some)
    }
}
