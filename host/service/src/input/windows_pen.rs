//! True stylus injection via Windows Ink synthetic pointer devices
//! (`CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`).
//!
//! Delivers real `WM_POINTER` pen input with **pressure and tilt**, which
//! drawing apps (Photoshop, Krita, OneNote, …) treat exactly like a physical
//! stylus. Available on Windows 10 1809+; when device creation fails the
//! caller keeps the legacy pen→mouse mapping.

use ndsp_protocol::messages::TouchPhase;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct PenInjector {
    device: HSYNTHETICPOINTERDEVICE,
    in_contact: bool,
}

// The handle is only used behind a Mutex in the sink.
unsafe impl Send for PenInjector {}

impl Drop for PenInjector {
    fn drop(&mut self) {
        // SAFETY: device came from CreateSyntheticPointerDevice.
        unsafe { DestroySyntheticPointerDevice(self.device) };
    }
}

impl PenInjector {
    pub fn new() -> anyhow::Result<Self> {
        // SAFETY: plain API call; the handle is released in Drop.
        let device = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .map_err(|e| anyhow::anyhow!("CreateSyntheticPointerDevice: {e}"))?;
        Ok(Self {
            device,
            in_contact: false,
        })
    }

    /// Map normalized (0..1) capture coordinates to desktop pixels, honoring
    /// the captured output's desktop rect for multi-monitor correctness.
    fn to_pixels(x: f32, y: f32, rect: Option<(i32, i32, i32, i32)>) -> POINT {
        let x = x.clamp(0.0, 1.0) as f64;
        let y = y.clamp(0.0, 1.0) as f64;
        // SAFETY: GetSystemMetrics is always safe to call.
        unsafe {
            if let Some((l, t, r, b)) = rect {
                if r > l && b > t {
                    // Clamp into the virtual desktop for robustness.
                    let (vx, vy, vw, vh) = (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    );
                    let px = (l as f64 + x * (r - l - 1) as f64).round() as i32;
                    let py = (t as f64 + y * (b - t - 1) as f64).round() as i32;
                    return POINT {
                        x: px.clamp(vx, vx + vw.max(1) - 1),
                        y: py.clamp(vy, vy + vh.max(1) - 1),
                    };
                }
            }
            POINT {
                x: (x * (GetSystemMetrics(SM_CXSCREEN) - 1).max(1) as f64).round() as i32,
                y: (y * (GetSystemMetrics(SM_CYSCREEN) - 1).max(1) as f64).round() as i32,
            }
        }
    }

    /// Inject one pen event. `pressure` is 0..1, tilts are degrees (-90..90).
    #[allow(clippy::too_many_arguments)]
    pub fn inject(
        &mut self,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        rect: Option<(i32, i32, i32, i32)>,
    ) {
        let flags: POINTER_FLAGS = match phase {
            TouchPhase::Start => {
                self.in_contact = true;
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN
            }
            TouchPhase::Move if self.in_contact => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE
            }
            // Hover move (proximity without contact).
            TouchPhase::Move => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
            TouchPhase::End | TouchPhase::Cancel => {
                self.in_contact = false;
                POINTER_FLAG_INRANGE | POINTER_FLAG_UP
            }
        };
        let pt = Self::to_pixels(x, y, rect);
        let pen = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 1,
                pointerFlags: flags,
                ptPixelLocation: pt,
                ..Default::default()
            },
            penFlags: 0,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            // WM_POINTER pen pressure range is 0..1024.
            pressure: (pressure.clamp(0.0, 1.0) * 1024.0).round() as u32,
            rotation: 0,
            tiltX: tilt_x.clamp(-90.0, 90.0).round() as i32,
            tiltY: tilt_y.clamp(-90.0, 90.0).round() as i32,
        };
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen },
        };
        // SAFETY: `info` is fully initialized for PT_PEN.
        if let Err(e) = unsafe { InjectSyntheticPointerInput(self.device, &[info]) } {
            tracing::debug!("pen injection failed: {e}");
        }
    }
}
