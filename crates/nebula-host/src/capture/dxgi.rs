//! Windows screen capture via the DXGI Desktop Duplication API.
//!
//! This is the "capture-only mirror" fallback used when the NebulaDisplay
//! IddCx virtual-monitor driver is not installed: it duplicates a *physical*
//! monitor's image. It is also used for mirror-mode sessions.
//!
//! Implementation notes:
//! * Desktop Duplication delivers frames only when the desktop image
//!   changes, which naturally rate-limits capture for static content.
//! * `AcquireNextFrame` gives us a GPU texture; we copy it into a staging
//!   texture and map it for CPU access. A future zero-copy path would hand
//!   the GPU texture straight to a hardware encoder (Media Foundation),
//!   which is the planned H.264 upgrade.
//! * Duplication is invalidated on mode switches / fullscreen transitions
//!   (`DXGI_ERROR_ACCESS_LOST`); we transparently re-create it.

#![cfg(windows)]

use std::time::Instant;

use anyhow::{bail, Context};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use super::{Frame, FrameSource};

pub struct DesktopDuplicationSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    monitor_index: u32,
}

// SAFETY: the source is used from a single dedicated capture thread at a
// time (the pipeline owns it exclusively); D3D11 immediate contexts must not
// be shared across threads concurrently and we never do.
unsafe impl Send for DesktopDuplicationSource {}

impl DesktopDuplicationSource {
    pub fn new(monitor_index: u32) -> anyhow::Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0).context("no DXGI adapter")?;
            let output = adapter
                .EnumOutputs(monitor_index)
                .with_context(|| format!("no output #{monitor_index}"))?;
            let output: IDXGIOutput1 = output.cast().context("IDXGIOutput1 unsupported")?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("device not created")?;
            let context = context.context("context not created")?;

            let mut src = Self {
                device,
                context,
                output,
                duplication: None,
                staging: None,
                width: 0,
                height: 0,
                monitor_index,
            };
            src.recreate_duplication()?;
            Ok(src)
        }
    }

    fn recreate_duplication(&mut self) -> anyhow::Result<()> {
        unsafe {
            let dup = self
                .output
                .DuplicateOutput(&self.device)
                .context("DuplicateOutput (is another duplication session running?)")?;
            let desc = dup.GetDesc();
            self.width = desc.ModeDesc.Width;
            self.height = desc.ModeDesc.Height;
            self.duplication = Some(dup);
            self.staging = None; // size may have changed
            tracing::info!(
                "desktop duplication active on monitor {}: {}x{}",
                self.monitor_index,
                self.width,
                self.height
            );
            Ok(())
        }
    }

    fn ensure_staging(&mut self, w: u32, h: u32) -> anyhow::Result<&ID3D11Texture2D> {
        if self.staging.is_none() || self.width != w || self.height != h {
            unsafe {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(staging)")?;
                self.staging = tex;
                self.width = w;
                self.height = h;
            }
        }
        Ok(self.staging.as_ref().unwrap())
    }
}

impl FrameSource for DesktopDuplicationSource {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<Frame>> {
        unsafe {
            let Some(dup) = self.duplication.clone() else {
                self.recreate_duplication()?;
                return Ok(None);
            };

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    tracing::warn!("desktop duplication lost (mode change?); recreating");
                    self.duplication = None;
                    self.recreate_duplication()?;
                    return Ok(None);
                }
                Err(e) => bail!("AcquireNextFrame: {e}"),
            }

            // Frames with no image update (cursor-only movement) have
            // LastPresentTime == 0; skip encoding those for now.
            if info.LastPresentTime == 0 {
                dup.ReleaseFrame().ok();
                return Ok(None);
            }

            let resource = resource.context("no resource from AcquireNextFrame")?;
            let tex: ID3D11Texture2D = resource.cast().context("frame is not a texture")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);

            let staging = self.ensure_staging(desc.Width, desc.Height)?.clone();
            self.context.CopyResource(&staging, &tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map staging texture")?;

            let w = desc.Width as usize;
            let h = desc.Height as usize;
            let src_pitch = mapped.RowPitch as usize;
            let mut bgra = vec![0u8; w * h * 4];
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, src_pitch * h);
            for row in 0..h {
                let s = &src[row * src_pitch..row * src_pitch + w * 4];
                bgra[row * w * 4..(row + 1) * w * 4].copy_from_slice(s);
            }

            self.context.Unmap(&staging, 0);
            dup.ReleaseFrame().ok();

            Ok(Some(Frame {
                bgra,
                width: desc.Width,
                height: desc.Height,
                captured_at: Instant::now(),
            }))
        }
    }

    fn describe(&self) -> String {
        format!(
            "DXGI desktop duplication, monitor {} ({}x{})",
            self.monitor_index, self.width, self.height
        )
    }
}
