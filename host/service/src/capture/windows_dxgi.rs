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
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};

use super::FrameSource;

pub struct DxgiDuplicationSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
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
            result
        }
    }
}
