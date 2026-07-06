//! Windows screen capture via DXGI Desktop Duplication (mirror mode).
//!
//! Compiled only on Windows; validated by the Windows CI job. Notes:
//! * Duplication must be re-created after `DXGI_ERROR_ACCESS_LOST` (mode
//!   switches, UAC secure desktop, fullscreen exclusive) — handled below.
//! * `AcquireNextFrame` timeout means "nothing changed"; we surface that as
//!   `Ok(false)` so the pacing loop simply idles.

use anyhow::{bail, Context};
use ndsp_protocol::messages::DisplayMode;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
};

/// Last known mouse-pointer state; duplication frames never include the
/// cursor. Depending on the client mix it is either composited into the
/// BGRA buffer (legacy clients) or forwarded through the dedicated cursor
/// channel (see `FrameSource::cursor`).
#[derive(Default)]
struct PointerState {
    visible: bool,
    x: i32,
    y: i32,
    shape: Vec<u8>,
    shape_info: Option<DXGI_OUTDUPL_POINTER_SHAPE_INFO>,
    /// Position/visibility changed since the last `cursor()` poll.
    moved: bool,
    /// Shape changed since the last `cursor()` poll.
    shape_changed: bool,
}

use super::FrameSource;

pub struct DxgiDuplicationSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// Captured output's rect in desktop coordinates (multi-monitor input mapping).
    desktop_rect: (i32, i32, i32, i32),
    pointer: PointerState,
    /// Blend the cursor into frames (true while any legacy client needs it).
    composite_cursor: bool,
}

// SAFETY: the source is owned by a single capture thread for its entire
// lifetime; COM pointers are only touched from that thread.
unsafe impl Send for DxgiDuplicationSource {}

impl DxgiDuplicationSource {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).context("EnumAdapters1(0)")?;
            let output: IDXGIOutput = adapter.EnumOutputs(0).context("EnumOutputs(0)")?;
            let output: IDXGIOutput1 = output.cast().context("IDXGIOutput1 cast")?;

            // windows 0.62: IDXGIOutput::GetDesc returns the descriptor
            // instead of filling an out-parameter.
            let desc: DXGI_OUTPUT_DESC = output.GetDesc().context("GetDesc")?;
            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL::default();
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                // windows 0.62: `software` is a plain HMODULE, not Option.
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("no D3D11 device")?;
            let context = context.context("no D3D11 context")?;

            let mut src = Self {
                device,
                context,
                output,
                duplication: None,
                staging: None,
                width,
                height,
                desktop_rect: (
                    desc.DesktopCoordinates.left,
                    desc.DesktopCoordinates.top,
                    desc.DesktopCoordinates.right,
                    desc.DesktopCoordinates.bottom,
                ),
                pointer: PointerState::default(),
                composite_cursor: true,
            };
            src.recreate_duplication()?;
            Ok(src)
        }
    }

    fn recreate_duplication(&mut self) -> anyhow::Result<()> {
        self.duplication = None;
        self.staging = None;
        unsafe {
            let dup = self
                .output
                .DuplicateOutput(&self.device)
                .context("DuplicateOutput (is another duplication session active?)")?;
            self.duplication = Some(dup);
        }
        Ok(())
    }

    fn ensure_staging(&mut self, w: u32, h: u32) -> anyhow::Result<ID3D11Texture2D> {
        if let Some(s) = &self.staging {
            unsafe {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                s.GetDesc(&mut d);
                if d.Width == w && d.Height == h {
                    return Ok(s.clone());
                }
            }
        }
        unsafe {
            // Describe a CPU-readable staging copy matching the frame.
            let src_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&src_desc, None, Some(&mut tex))
                .context("CreateTexture2D(staging)")?;
            let tex = tex.context("no staging texture")?;
            self.staging = Some(tex.clone());
            Ok(tex)
        }
    }
}

impl FrameSource for DxgiDuplicationSource {
    fn name(&self) -> &'static str {
        "dxgi-duplication"
    }

    fn mode(&self) -> DisplayMode {
        DisplayMode {
            width: self.width,
            height: self.height,
            refresh_hz: 60,
        }
    }

    fn desktop_rect(&self) -> Option<(i32, i32, i32, i32)> {
        Some(self.desktop_rect)
    }

    fn next_frame(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        let Some(dup) = self.duplication.clone() else {
            self.recreate_duplication()?;
            return Ok(false);
        };
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match dup.AcquireNextFrame(16, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    tracing::info!(
                        "duplication access lost (mode change/secure desktop); recreating"
                    );
                    self.recreate_duplication()?;
                    return Ok(false);
                }
                Err(e) => bail!("AcquireNextFrame: {e}"),
            }
            // Track the pointer (position updates + shape changes) so we can
            // composite it — duplication frames don't include the cursor.
            if info.LastMouseUpdateTime != 0 {
                self.pointer.visible = info.PointerPosition.Visible.as_bool();
                self.pointer.x = info.PointerPosition.Position.x;
                self.pointer.y = info.PointerPosition.Position.y;
                self.pointer.moved = true;
            }
            if info.PointerShapeBufferSize > 0 {
                self.pointer
                    .shape
                    .resize(info.PointerShapeBufferSize as usize, 0);
                let mut required = 0u32;
                let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                if dup
                    .GetFramePointerShape(
                        info.PointerShapeBufferSize,
                        self.pointer.shape.as_mut_ptr() as *mut _,
                        &mut required,
                        &mut shape_info,
                    )
                    .is_ok()
                {
                    self.pointer.shape_info = Some(shape_info);
                    self.pointer.shape_changed = true;
                }
            }
            let result = (|| -> anyhow::Result<bool> {
                let resource = resource.as_ref().context("no frame resource")?;
                let tex: ID3D11Texture2D = resource.cast().context("frame texture cast")?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let (w, h) = (desc.Width, desc.Height);
                self.width = w;
                self.height = h;
                let staging = self.ensure_staging(w, h)?;
                self.context.CopyResource(&staging, &tex);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("Map staging")?;
                let row_pitch = mapped.RowPitch as usize;
                let row_bytes = (w * 4) as usize;
                out.resize((w * h * 4) as usize, 0);
                let src =
                    std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * h as usize);
                for y in 0..h as usize {
                    let s = &src[y * row_pitch..y * row_pitch + row_bytes];
                    out[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(s);
                }
                self.context.Unmap(&staging, 0);
                Ok(true)
            })();
            let _ = dup.ReleaseFrame();
            if matches!(result, Ok(true)) && self.composite_cursor {
                self.composite_pointer(out);
            }
            result
        }
    }

    fn cursor(&mut self) -> Option<super::CursorUpdate> {
        if !self.pointer.moved && !self.pointer.shape_changed {
            return None;
        }
        let shape = if self.pointer.shape_changed {
            self.pointer.shape_changed = false;
            self.shape_as_rgba().map(std::sync::Arc::new)
        } else {
            None
        };
        self.pointer.moved = false;
        let w = (self.desktop_rect.2 - self.desktop_rect.0).max(1) as f32;
        let h = (self.desktop_rect.3 - self.desktop_rect.1).max(1) as f32;
        Some(super::CursorUpdate {
            // DXGI pointer position is already relative to this output.
            x: (self.pointer.x as f32 / w).clamp(0.0, 1.0),
            y: (self.pointer.y as f32 / h).clamp(0.0, 1.0),
            visible: self.pointer.visible,
            shape,
        })
    }

    fn set_composite_cursor(&mut self, on: bool) {
        self.composite_cursor = on;
    }
}

impl DxgiDuplicationSource {
    /// Convert the last captured DXGI pointer shape to straight RGBA8 for
    /// the cursor channel. Monochrome and masked-color shapes XOR against
    /// screen content, which a client-side overlay cannot reproduce exactly;
    /// the standard approximation (white where inverted) is used — identical
    /// to what other remote-desktop stacks ship.
    fn shape_as_rgba(&self) -> Option<crate::state::CursorShapeData> {
        let si = self.pointer.shape_info?;
        let pitch = si.Pitch as usize;
        let sw = si.Width as usize;
        let mono = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let sh = if mono {
            si.Height as usize / 2
        } else {
            si.Height as usize
        };
        let shape = &self.pointer.shape;
        let mut rgba = vec![0u8; sw * sh * 4];
        for y in 0..sh {
            for x in 0..sw {
                let di = (y * sw + x) * 4;
                if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
                    let s = y * pitch + x * 4;
                    if s + 4 > shape.len() {
                        continue;
                    }
                    rgba[di] = shape[s + 2]; // R (BGRA → RGBA)
                    rgba[di + 1] = shape[s + 1];
                    rgba[di + 2] = shape[s];
                    rgba[di + 3] = shape[s + 3];
                } else if mono {
                    let byte = y * pitch + x / 8;
                    let bit = 7 - (x % 8);
                    let and_b = shape.get(byte).map_or(1, |b| (b >> bit) & 1);
                    let xor_b = shape.get(byte + sh * pitch).map_or(0, |b| (b >> bit) & 1);
                    // AND=0 → opaque (XOR selects black/white); AND=1,XOR=1 →
                    // screen-invert, approximated as opaque white.
                    if and_b == 0 {
                        let c = if xor_b == 1 { 255 } else { 0 };
                        rgba[di] = c;
                        rgba[di + 1] = c;
                        rgba[di + 2] = c;
                        rgba[di + 3] = 255;
                    } else if xor_b == 1 {
                        rgba[di] = 255;
                        rgba[di + 1] = 255;
                        rgba[di + 2] = 255;
                        rgba[di + 3] = 255;
                    }
                } else if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                    let s = y * pitch + x * 4;
                    if s + 4 > shape.len() {
                        continue;
                    }
                    let mask = shape[s + 3];
                    if mask == 0 {
                        rgba[di] = shape[s + 2];
                        rgba[di + 1] = shape[s + 1];
                        rgba[di + 2] = shape[s];
                        rgba[di + 3] = 255;
                    } else {
                        // XOR region — approximate as opaque white.
                        rgba[di] = 255;
                        rgba[di + 1] = 255;
                        rgba[di + 2] = 255;
                        rgba[di + 3] = 255;
                    }
                }
            }
        }
        Some(crate::state::CursorShapeData {
            width: sw as u16,
            height: sh as u16,
            hot_x: si.HotSpot.x.max(0) as u16,
            hot_y: si.HotSpot.y.max(0) as u16,
            rgba,
        })
    }

    /// Blend the last known cursor shape into the BGRA frame at its current
    /// position. Handles the three DXGI shape types.
    fn composite_pointer(&self, out: &mut [u8]) {
        let Some(si) = self.pointer.shape_info else {
            return;
        };
        if !self.pointer.visible {
            return;
        }
        let (fw, fh) = (self.width as i32, self.height as i32);
        let (px, py) = (self.pointer.x, self.pointer.y);
        let pitch = si.Pitch as usize;
        let sw = si.Width as i32;
        // Monochrome shapes pack AND+XOR masks stacked vertically.
        let mono = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let sh = if mono {
            si.Height as i32 / 2
        } else {
            si.Height as i32
        };
        let shape = &self.pointer.shape;

        for sy in 0..sh {
            let dy = py + sy;
            if dy < 0 || dy >= fh {
                continue;
            }
            for sx in 0..sw {
                let dx = px + sx;
                if dx < 0 || dx >= fw {
                    continue;
                }
                let di = ((dy * fw + dx) * 4) as usize;
                if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
                    let s = (sy as usize) * pitch + (sx as usize) * 4;
                    if s + 4 > shape.len() {
                        continue;
                    }
                    let a = shape[s + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    for c in 0..3 {
                        let src = shape[s + c] as u32;
                        let dst = out[di + c] as u32;
                        out[di + c] = ((src * a + dst * (255 - a)) / 255) as u8;
                    }
                } else if mono {
                    let byte = (sy as usize) * pitch + (sx / 8) as usize;
                    let bit = 7 - (sx % 8) as usize;
                    let and_b = shape.get(byte).map_or(1, |b| (b >> bit) & 1);
                    let xor_b = shape
                        .get(byte + sh as usize * pitch)
                        .map_or(0, |b| (b >> bit) & 1);
                    for c in 0..3 {
                        let mut v = out[di + c];
                        if and_b == 0 {
                            v = 0;
                        }
                        if xor_b == 1 {
                            v = !v;
                        }
                        out[di + c] = v;
                    }
                } else if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                    let s = (sy as usize) * pitch + (sx as usize) * 4;
                    if s + 4 > shape.len() {
                        continue;
                    }
                    let mask = shape[s + 3];
                    for c in 0..3 {
                        if mask == 0 {
                            out[di + c] = shape[s + c];
                        } else {
                            out[di + c] ^= shape[s + c];
                        }
                    }
                }
            }
        }
    }
}
