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

use vd_core::clock::Clock;

use log::{debug, info};
use vd_core::capture::{CaptureError, CapturedFrame, FrameSource};
use vd_core::video::FrameFormat;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MORE_DATA, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

/// How long to wait for a new frame before reporting "nothing changed".
const ACQUIRE_TIMEOUT_MS: u32 = 50;

/// Bytes per pixel of the dupicated desktop (`DXGI_FORMAT_B8G8R8A8_UNORM`).
const BYTES_PER_PIXEL: u32 = 4;

/// A cursor bitmap.
struct CursorShape {
    /// A `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*` value.
    shape_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hotspot: (i32, i32),
    pixels: Vec<u8>,
}

/// A display output DXGI can see, and the adapter that drives it.
///
/// The adapter matters because DXGI enumerates outputs *per adapter*, and an
/// indirect display driver reports an adapter of its own. A virtual monitor
/// therefore does not appear among the outputs of the GPU the rest of the
/// pipeline uses, which is what makes "capture it like any other screen" a
/// question rather than an assumption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayOutput {
    /// Index into the adapter list.
    pub adapter: u32,
    /// Index into that adapter's own outputs.
    pub output: u32,
    /// The adapter's description, e.g. "AMD Radeon(TM) 8060S Graphics".
    pub adapter_name: String,
    /// The name Windows gives the monitor, e.g. `\.\DISPLAY2`.
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    /// Whether the output has a desktop on it at all.
    pub attached: bool,
}

/// An adapter DXGI will enumerate, and how many outputs it reports.
///
/// Separated from [`enumerate_outputs`] because "the adapter is not there" and
/// "the adapter is there but has no outputs" mean different things, and an
/// indirect display driver can produce either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayAdapter {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Whether DXGI flags this as a software adapter.
    pub software: bool,
    pub outputs: u32,
}

/// Every adapter DXGI will enumerate, whether or not it has outputs.
pub fn enumerate_adapters() -> Result<Vec<DisplayAdapter>, CaptureError> {
    // SAFETY: plain enumeration; nothing outlives the factory.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|err| CaptureError::Unavailable(format!("CreateDXGIFactory1: {err}")))?;

        let mut found = Vec::new();
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            let mut outputs = 0;
            while adapter.EnumOutputs(outputs).is_ok() {
                outputs += 1;
            }
            match adapter.GetDesc1() {
                Ok(desc) => found.push(DisplayAdapter {
                    index,
                    name: wide_to_string(&desc.Description),
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
                    outputs,
                }),
                Err(err) => debug!("adapter {index} has no description: {err}"),
            }
            index += 1;
        }
        Ok(found)
    }
}

/// Every output of every adapter, in enumeration order.
pub fn enumerate_outputs() -> Result<Vec<DisplayOutput>, CaptureError> {
    // SAFETY: plain enumeration; each interface is used within the call that
    // obtained it and nothing outlives the factory.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|err| CaptureError::Unavailable(format!("CreateDXGIFactory1: {err}")))?;

        let mut found = Vec::new();
        let mut adapter_index = 0;
        // Running off the end of either list is how DXGI says "no more".
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            let adapter_name = adapter_description(&adapter);
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                match output.GetDesc() {
                    Ok(desc) => found.push(DisplayOutput {
                        adapter: adapter_index,
                        output: output_index,
                        adapter_name: adapter_name.clone(),
                        device_name: wide_to_string(&desc.DeviceName),
                        width: (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0)
                            as u32,
                        height: (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top)
                            .max(0) as u32,
                        attached: desc.AttachedToDesktop.as_bool(),
                    }),
                    Err(err) => {
                        debug!("output {adapter_index}:{output_index} has no description: {err}")
                    }
                }
                output_index += 1;
            }
            adapter_index += 1;
        }
        Ok(found)
    }
}

/// Whether Desktop Duplication can be created for one output, and at what size.
///
/// The result decides whether a virtual monitor is just another screen to the
/// capture path or needs a different backend. The failure is returned rather than
/// logged because "it does not work" is not much use without the reason.
pub fn probe_duplication(
    adapter_index: u32,
    output_index: u32,
) -> Result<(u32, u32), CaptureError> {
    // SAFETY: enumeration and a device created on the adapter that owns the
    // output; both are used only here.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|err| CaptureError::Unavailable(format!("CreateDXGIFactory1: {err}")))?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(adapter_index).map_err(|err| {
            CaptureError::Unavailable(format!("EnumAdapters1({adapter_index}): {err}"))
        })?;
        let output = adapter.EnumOutputs(output_index).map_err(|err| {
            CaptureError::Unavailable(format!("EnumOutputs({output_index}): {err}"))
        })?;
        let output: IDXGIOutput1 = output
            .cast()
            .map_err(|err| CaptureError::Unavailable(format!("IDXGIOutput1: {err}")))?;

        // A device of its own, on this adapter: duplication belongs to the
        // adapter that owns the output, so the shared device cannot be reused.
        // Handing D3D11CreateDevice an adapter requires DRIVER_TYPE_UNKNOWN,
        // since a driver type and an adapter are mutually exclusive.
        // D3D11CreateDevice takes the base interface, not `1`.
        let adapter_for_device: IDXGIAdapter = adapter
            .cast()
            .map_err(|err| CaptureError::Unavailable(format!("IDXGIAdapter: {err}")))?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            Some(&adapter_for_device),
            D3D_DRIVER_TYPE_UNKNOWN,
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
        .map_err(|err| {
            CaptureError::Unavailable(format!(
                "D3D11CreateDevice on adapter {adapter_index}: {err}"
            ))
        })?;

        let device =
            device.ok_or_else(|| CaptureError::Unavailable("no D3D11 device".to_owned()))?;
        let duplication = output
            .DuplicateOutput(&device)
            .map_err(|err| CaptureError::Unavailable(format!("DuplicateOutput: {err}")))?;
        let desc = duplication.GetDesc();
        Ok((desc.ModeDesc.Width, desc.ModeDesc.Height))
    }
}

/// An adapter's friendly name, from its wide-string description.
fn adapter_description(adapter: &IDXGIAdapter1) -> String {
    // SAFETY: GetDesc fills a struct this function owns.
    unsafe {
        adapter
            .GetDesc1()
            .map(|desc| wide_to_string(&desc.Description))
            .unwrap_or_else(|_| "<unknown adapter>".to_owned())
    }
}

/// Trims a NUL-terminated wide string out of a fixed-size field.
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// The mouse pointer.
///
/// Desktop Duplication deliberately leaves the cursor *out* of the desktop
/// image — it is a separate overlay — so it has to be fetched and composited
/// back in, or the remote screen would show no pointer at all.
#[derive(Default)]
struct Pointer {
    x: i32,
    y: i32,
    visible: bool,
    shape: Option<CursorShape>,
}

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
    clock: Clock,
    pointer: Pointer,
}

impl DxgiCapture {
    /// Attaches to `output_index` (0 is the primary monitor) on the shared device.
    pub fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output_index: u32,
        clock: Clock,
    ) -> Result<Self, CaptureError> {
        // SAFETY: every call below is a documented D3D/DXGI call; the returned
        // interfaces are owned for the lifetime of `self` and only used from the
        // capture thread.
        unsafe { Self::create(device, context, output_index, clock) }
    }

    unsafe fn create(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output_index: u32,
        clock: Clock,
    ) -> Result<Self, CaptureError> {
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
            clock,
            pointer: Pointer::default(),
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

    /// Picks up the cursor position and, when it changed, its bitmap.
    ///
    /// Must run while the frame is still acquired.
    fn update_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.pointer.x = info.PointerPosition.Position.x;
            self.pointer.y = info.PointerPosition.Position.y;
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
        }

        let mut size = info.PointerShapeBufferSize;
        if size == 0 {
            return;
        }

        let mut buffer = vec![0u8; size as usize];
        let mut required = 0u32;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();

        // SAFETY: the buffer is at least `size` bytes.
        let fetch = |size: u32,
                     buffer: &mut Vec<u8>,
                     required: &mut u32,
                     shape: &mut DXGI_OUTDUPL_POINTER_SHAPE_INFO| unsafe {
            self.duplication
                .GetFramePointerShape(size, buffer.as_mut_ptr().cast(), required, shape)
        };

        if let Err(err) = fetch(size, &mut buffer, &mut required, &mut shape) {
            if err.code() != DXGI_ERROR_MORE_DATA {
                debug!("cursor shape unavailable: {err}");
                return;
            }
            // The shape grew between the two calls; ask again for the size it wants.
            size = required;
            buffer = vec![0u8; size as usize];
            if let Err(err) = fetch(size, &mut buffer, &mut required, &mut shape) {
                debug!("cursor shape unavailable: {err}");
                return;
            }
        }

        debug!(
            "cursor shape: {}x{} type {} pitch {} hotspot ({}, {})",
            shape.Width, shape.Height, shape.Type, shape.Pitch, shape.HotSpot.x, shape.HotSpot.y
        );
        self.pointer.shape = Some(CursorShape {
            shape_type: shape.Type,
            width: shape.Width,
            height: shape.Height,
            pitch: shape.Pitch,
            hotspot: (shape.HotSpot.x, shape.HotSpot.y),
            pixels: buffer,
        });
    }

    /// Draws the cursor onto a captured frame.
    fn draw_pointer(&self, frame: &mut CapturedFrame) {
        let Some(cursor) = self.pointer.shape.as_ref() else {
            return;
        };
        if !self.pointer.visible || frame.format != FrameFormat::RawBgra {
            return;
        }

        let origin = (
            self.pointer.x - cursor.hotspot.0,
            self.pointer.y - cursor.hotspot.1,
        );
        blend_cursor(&mut frame.data, frame.width, frame.height, cursor, origin);
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
            timestamp_us: self.clock.now_us(),
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
            Ok(()) => self.update_pointer(&info),
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                debug!("desktop duplication access lost; rebuilding");
                self.recreate()?;
                return Ok(None);
            }
            Err(err) => return Err(CaptureError::Failed(format!("AcquireNextFrame: {err}"))),
        }

        // Once acquired, the frame must be released on every path.
        let mut copied = self.copy_desktop(resource);
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
        if let Ok(frame) = copied.as_mut() {
            self.draw_pointer(frame);
        }
        copied.map(Some)
    }
}

/// Composites a cursor bitmap onto a BGRA frame.
///
/// Handles all three shapes Windows hands out: a colour bitmap with alpha
/// (ordinary cursors), a masked-colour bitmap that is XORed (inverted cursors),
/// and the legacy monochrome AND/XOR pair.
fn blend_cursor(
    frame: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    cursor: &CursorShape,
    origin: (i32, i32),
) {
    let (frame_width_i, frame_height_i) = (frame_width as i32, frame_height as i32);
    let monochrome = cursor.shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    // A monochrome shape stacks its AND and XOR masks, so it is twice as tall
    // as the cursor it draws.
    let cursor_height = if monochrome {
        cursor.height / 2
    } else {
        cursor.height
    };

    for row in 0..cursor_height as i32 {
        let y = origin.1 + row;
        if y < 0 || y >= frame_height_i {
            continue;
        }

        for column in 0..cursor.width as i32 {
            let x = origin.0 + column;
            if x < 0 || x >= frame_width_i {
                continue;
            }

            let destination = ((y as u32 * frame_width + x as u32) * BYTES_PER_PIXEL) as usize;
            if destination + 3 >= frame.len() {
                continue;
            }

            if cursor.shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
                let source = (row as u32 * cursor.pitch + column as u32 * 4) as usize;
                if source + 3 >= cursor.pixels.len() {
                    continue;
                }
                let alpha = u32::from(cursor.pixels[source + 3]);
                if alpha == 0 {
                    continue;
                }
                for channel in 0..3 {
                    let value = u32::from(cursor.pixels[source + channel]);
                    let existing = u32::from(frame[destination + channel]);
                    // Straight-alpha blend.
                    frame[destination + channel] =
                        ((value * alpha + existing * (255 - alpha) + 127) / 255) as u8;
                }
            } else if cursor.shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                let source = (row as u32 * cursor.pitch + column as u32 * 4) as usize;
                if source + 3 >= cursor.pixels.len() {
                    continue;
                }
                for channel in 0..3 {
                    frame[destination + channel] ^= cursor.pixels[source + channel];
                }
            } else if monochrome {
                // Two 1bpp masks stacked: AND first, then XOR.
                let byte = (column as u32 / 8) as usize;
                let bit = 7 - (column as u32 % 8);
                let and_row = row as usize;
                let xor_row = cursor_height as usize + row as usize;

                let mask = |row: usize| -> u8 {
                    cursor
                        .pixels
                        .get(row * cursor.pitch as usize + byte)
                        .copied()
                        .unwrap_or(0)
                };
                let and = (mask(and_row) >> bit) & 1;
                let xor = (mask(xor_row) >> bit) & 1;

                // AND 0 paints black; XOR 1 inverts.
                if and == 0 {
                    frame[destination] = 0;
                    frame[destination + 1] = 0;
                    frame[destination + 2] = 0;
                }
                if xor == 1 {
                    frame[destination] ^= 0xff;
                    frame[destination + 1] ^= 0xff;
                    frame[destination + 2] ^= 0xff;
                }
            }
        }
    }
}
