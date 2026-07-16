//! Windows input injection: `SendInput` for mouse/keyboard, and **Windows
//! Ink synthetic pointers** (`CreateSyntheticPointerDevice` +
//! `InjectSyntheticPointerInput`, Win10 1809+) for the stylus — real
//! **pressure and tilt** into ink-aware apps — and for **multi-touch**
//! (ROADMAP P2.14): every viewer finger becomes a real touch contact, so
//! pinch-zoom / two-finger scroll / multi-finger gestures work. If the
//! synthetic-pointer API is unavailable or fails, pen falls back to the
//! mouse mapping and touch to a primary-finger mouse mapping.
//!
//! Coordinates arrive normalized (0..1) relative to the *streamed monitor*.
//! They are mapped through the captured output's desktop rectangle into the
//! full virtual-desktop space (`MOUSEEVENTF_VIRTUALDESK` for mouse; desktop
//! pixels for pen), so taps land on the correct pixel even when the captured
//! monitor is not the primary one or sits at a non-zero desktop offset in a
//! multi-monitor layout.

use ndsp_protocol::messages::{InputEvent, TouchPhase};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_CHAR, MAPVK_VSC_TO_VK,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
    POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH,
    SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_PRESSURE, XBUTTON1, XBUTTON2,
};

use super::touch::{ContactAction, FrameContact, TouchTracker, MAX_TOUCH_CONTACTS};
use super::{pen_pressure_1024, pen_tilt_deg, InputSink};
use crate::state::AppState;

pub struct WindowsInputSink {
    state: Arc<AppState>,
    /// Touch state so single-finger touch maps to press-drag-release.
    touch_down: Mutex<bool>,
    /// Non-Shift modifiers currently held by the viewer (bitmask: 1 = Ctrl,
    /// 2 = Alt, 4 = Meta). While any is down, printable keys go through the
    /// scancode path — Ctrl+C must be a *position*, not a character.
    modifiers: Mutex<u8>,
    /// Lazily-created Windows Ink synthetic pen (None = untried, Some(None)
    /// = unavailable on this system → mouse fallback).
    pen: Mutex<Option<Option<PenDevice>>>,
    /// Lazily-created synthetic touch device + its frame tracker (same
    /// tri-state as `pen`).
    touch: Mutex<Option<Option<TouchInjector>>>,
    /// In mouse-fallback mode, the wire id of the finger driving the mouse
    /// (only the first finger does — a second finger must not fight over
    /// the single cursor).
    fallback_finger: Mutex<Option<u32>>,
}

impl WindowsInputSink {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            touch_down: Mutex::new(false),
            modifiers: Mutex::new(0),
            pen: Mutex::new(None),
            touch: Mutex::new(None),
            fallback_finger: Mutex::new(None),
        }
    }

    /// Map a normalized (0..1) point on the streamed monitor to desktop
    /// pixel coordinates (what synthetic pointer injection expects).
    fn desktop_pixel(&self, x: f32, y: f32) -> POINT {
        let x = x.clamp(0.0, 1.0) as f64;
        let y = y.clamp(0.0, 1.0) as f64;
        if let Some((l, t, r, b)) = *self.state.capture_rect.lock().unwrap() {
            if r > l && b > t {
                return POINT {
                    x: (l as f64 + x * (r - l - 1) as f64).round() as i32,
                    y: (t as f64 + y * (b - t - 1) as f64).round() as i32,
                };
            }
        }
        // SAFETY: GetSystemMetrics is always safe to call.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        POINT {
            x: (x * (w.max(1) - 1) as f64).round() as i32,
            y: (y * (h.max(1) - 1) as f64).round() as i32,
        }
    }

    /// Single-pointer press/drag/release mapped to mouse events (touch, and
    /// pen when the synthetic-pointer API is unavailable).
    fn pointer_as_mouse(&self, batch: &mut Vec<INPUT>, phase: TouchPhase, x: f32, y: f32) {
        let (ax, ay, flags) = self.map_coords(x, y);
        batch.push(mouse_input(ax, ay, 0, flags));
        let mut down = self.touch_down.lock().unwrap();
        match phase {
            TouchPhase::Start if !*down => {
                *down = true;
                batch.push(mouse_input(0, 0, 0, MOUSEEVENTF_LEFTDOWN));
            }
            TouchPhase::End | TouchPhase::Cancel if *down => {
                *down = false;
                batch.push(mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP));
            }
            _ => {}
        }
    }

    /// Inject one pen event through Windows Ink. Returns false when the
    /// synthetic-pointer path is unavailable (caller falls back to mouse).
    fn inject_pen(
        &self,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
    ) -> bool {
        let mut slot = self.pen.lock().unwrap();
        let device = slot.get_or_insert_with(|| match PenDevice::create() {
            Ok(d) => {
                tracing::info!("Windows Ink synthetic pen active (pressure/tilt enabled)");
                Some(d)
            }
            Err(e) => {
                tracing::info!("synthetic pen unavailable ({e:#}); pen maps to mouse");
                None
            }
        });
        let Some(device) = device else { return false };
        let pos = self.desktop_pixel(x, y);
        if let Err(e) = device.inject(phase, pos, pressure, tilt_x, tilt_y) {
            // A failing injection (session switch, device loss) downgrades
            // to the mouse path permanently rather than erroring per event.
            tracing::warn!("pen injection failed ({e:#}); falling back to mouse");
            *slot = Some(None);
            return false;
        }
        true
    }

    /// Inject one touch event through the synthetic-pointer API. Returns
    /// false when that path is unavailable (caller falls back to mouse).
    fn inject_touch(&self, id: u32, phase: TouchPhase, x: f32, y: f32, pressure: f32) -> bool {
        let mut slot = self.touch.lock().unwrap();
        let injector = slot.get_or_insert_with(|| match TouchDevice::create() {
            Ok(device) => {
                tracing::info!("synthetic touch device active (multi-touch enabled)");
                Some(TouchInjector {
                    device,
                    tracker: TouchTracker::default(),
                })
            }
            Err(e) => {
                tracing::info!("synthetic touch unavailable ({e:#}); touch maps to mouse");
                None
            }
        });
        let Some(injector) = injector else {
            return false;
        };
        let Some(frame) = injector.tracker.event(id, phase, x, y, pressure) else {
            return true; // nothing to inject (overflow / unknown lift) — handled
        };
        if let Err(e) = injector
            .device
            .inject(&frame, |x, y| self.desktop_pixel(x, y))
        {
            // A failing injection (session switch, device loss) downgrades
            // to the mouse path permanently rather than erroring per event.
            tracing::warn!("touch injection failed ({e:#}); falling back to mouse");
            *slot = Some(None);
            return false;
        }
        true
    }

    /// Mouse-fallback touch mapping: only the first finger drives the
    /// cursor; additional fingers are ignored until it lifts.
    fn touch_as_mouse(&self, batch: &mut Vec<INPUT>, id: u32, phase: TouchPhase, x: f32, y: f32) {
        {
            let mut finger = self.fallback_finger.lock().unwrap();
            match phase {
                TouchPhase::Start => {
                    if finger.is_some() && *finger != Some(id) {
                        return;
                    }
                    *finger = Some(id);
                }
                TouchPhase::Move => {
                    // Unknown finger = stream began mid-gesture; adopt it.
                    if finger.is_none() {
                        *finger = Some(id);
                    } else if *finger != Some(id) {
                        return;
                    }
                }
                TouchPhase::End | TouchPhase::Cancel => {
                    if *finger != Some(id) {
                        return;
                    }
                    *finger = None;
                }
            }
        }
        self.pointer_as_mouse(batch, phase, x, y);
    }

    /// Map a normalized (0..1) point on the streamed monitor to the 0..65535
    /// absolute space SendInput expects. Uses the captured output's desktop
    /// rect + the virtual-screen metrics when available (multi-monitor
    /// correct); falls back to primary-monitor mapping otherwise.
    fn map_coords(
        &self,
        x: f32,
        y: f32,
    ) -> (
        i32,
        i32,
        windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    ) {
        let x = x.clamp(0.0, 1.0) as f64;
        let y = y.clamp(0.0, 1.0) as f64;
        if let Some((l, t, r, b)) = *self.state.capture_rect.lock().unwrap() {
            // SAFETY: GetSystemMetrics is always safe to call.
            let (vx, vy, vw, vh) = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            if vw > 0 && vh > 0 && r > l && b > t {
                // Pixel on the captured monitor (desktop coordinates)...
                let px = l as f64 + x * (r - l - 1) as f64;
                let py = t as f64 + y * (b - t - 1) as f64;
                // ...normalized over the whole virtual desktop.
                let ax = ((px - vx as f64) * 65535.0 / (vw - 1).max(1) as f64).round() as i32;
                let ay = ((py - vy as f64) * 65535.0 / (vh - 1).max(1) as f64).round() as i32;
                return (
                    ax.clamp(0, 65535),
                    ay.clamp(0, 65535),
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                );
            }
        }
        (
            (x * 65535.0).round() as i32,
            (y * 65535.0).round() as i32,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        )
    }

    fn send(&self, inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        // SAFETY: INPUT structs are fully initialized below.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent != inputs.len() as u32 {
            tracing::warn!("SendInput injected {sent}/{} events", inputs.len());
        }
    }
}

fn mouse_input(
    dx: i32,
    dy: i32,
    data: i32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Map W3C `KeyboardEvent.code` values to Windows scan codes (set 1).
/// Covers the practical desktop set; unknown codes are ignored with a log.
fn code_to_scancode(code: &str) -> Option<u16> {
    let sc: u16 = match code {
        "Escape" => 0x01,
        "Digit1" => 0x02,
        "Digit2" => 0x03,
        "Digit3" => 0x04,
        "Digit4" => 0x05,
        "Digit5" => 0x06,
        "Digit6" => 0x07,
        "Digit7" => 0x08,
        "Digit8" => 0x09,
        "Digit9" => 0x0A,
        "Digit0" => 0x0B,
        "Minus" => 0x0C,
        "Equal" => 0x0D,
        "Backspace" => 0x0E,
        "Tab" => 0x0F,
        "KeyQ" => 0x10,
        "KeyW" => 0x11,
        "KeyE" => 0x12,
        "KeyR" => 0x13,
        "KeyT" => 0x14,
        "KeyY" => 0x15,
        "KeyU" => 0x16,
        "KeyI" => 0x17,
        "KeyO" => 0x18,
        "KeyP" => 0x19,
        "BracketLeft" => 0x1A,
        "BracketRight" => 0x1B,
        "Enter" => 0x1C,
        "ControlLeft" => 0x1D,
        "KeyA" => 0x1E,
        "KeyS" => 0x1F,
        "KeyD" => 0x20,
        "KeyF" => 0x21,
        "KeyG" => 0x22,
        "KeyH" => 0x23,
        "KeyJ" => 0x24,
        "KeyK" => 0x25,
        "KeyL" => 0x26,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "ShiftLeft" => 0x2A,
        "Backslash" => 0x2B,
        "KeyZ" => 0x2C,
        "KeyX" => 0x2D,
        "KeyC" => 0x2E,
        "KeyV" => 0x2F,
        "KeyB" => 0x30,
        "KeyN" => 0x31,
        "KeyM" => 0x32,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "ShiftRight" => 0x36,
        "NumpadMultiply" => 0x37,
        "AltLeft" => 0x38,
        "Space" => 0x39,
        "CapsLock" => 0x3A,
        "F1" => 0x3B,
        "F2" => 0x3C,
        "F3" => 0x3D,
        "F4" => 0x3E,
        "F5" => 0x3F,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "NumLock" => 0x45,
        "ScrollLock" => 0x46,
        "Numpad7" => 0x47,
        "Numpad8" => 0x48,
        "Numpad9" => 0x49,
        "NumpadSubtract" => 0x4A,
        "Numpad4" => 0x4B,
        "Numpad5" => 0x4C,
        "Numpad6" => 0x4D,
        "NumpadAdd" => 0x4E,
        "Numpad1" => 0x4F,
        "Numpad2" => 0x50,
        "Numpad3" => 0x51,
        "Numpad0" => 0x52,
        "NumpadDecimal" => 0x53,
        "F11" => 0x57,
        "F12" => 0x58,
        // Extended keys (E0 prefix encoded in the high byte convention).
        "NumpadEnter" => 0xE01C,
        "ControlRight" => 0xE01D,
        "NumpadDivide" => 0xE035,
        "AltRight" => 0xE038,
        "Home" => 0xE047,
        "ArrowUp" => 0xE048,
        "PageUp" => 0xE049,
        "ArrowLeft" => 0xE04B,
        "ArrowRight" => 0xE04D,
        "End" => 0xE04F,
        "ArrowDown" => 0xE050,
        "PageDown" => 0xE051,
        "Insert" => 0xE052,
        "Delete" => 0xE053,
        "MetaLeft" => 0xE05B,
        "MetaRight" => 0xE05C,
        "ContextMenu" => 0xE05D,
        _ => return None,
    };
    Some(sc)
}

/// Layout-aware key selection (ROADMAP P2.13): viewers send both the
/// physical `code` and the layout-resolved `key`. Rules:
///
/// 1. Named keys ("Enter", "ArrowUp", …) and anything pressed while a
///    non-Shift modifier is held → **scancode** injection: shortcuts are
///    positions, and the host resolves them against its own layout.
/// 2. A single printable `key` whose character matches what the host layout
///    produces for that physical position → scancode too (cheapest, plays
///    nicest with games and key-repeat).
/// 3. A printable `key` the host layout would render *differently* (AZERTY
///    viewer on a QWERTY host, ü on a US layout, …) → **Unicode** injection
///    of the exact character, so what the user typed is what appears.
fn layout_aware_key_input(
    code: &str,
    key: Option<&str>,
    pressed: bool,
    shortcut_held: bool,
) -> Option<INPUT> {
    if !shortcut_held {
        if let Some(k) = key {
            let mut chars = k.chars();
            if let (Some(ch), None) = (chars.next(), chars.next()) {
                if !ch.is_control() && host_char_for_code(code) != Some(normalize_char(ch)) {
                    return Some(unicode_key_input(ch, pressed));
                }
            }
        }
    }
    key_input(code, pressed)
}

/// Character the *host's* active layout produces for the physical key, in
/// normalized (uppercase base) form. `None` for non-printables/unknowns.
fn host_char_for_code(code: &str) -> Option<char> {
    let sc = code_to_scancode(code)?;
    if sc & 0xE000 == 0xE000 {
        return None; // extended keys are never printable
    }
    // SAFETY: MapVirtualKeyW is always safe to call.
    let vk = unsafe { MapVirtualKeyW((sc & 0xFF) as u32, MAPVK_VSC_TO_VK) };
    if vk == 0 {
        return None;
    }
    let ch = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) };
    // High bit set = dead key; low 16 bits = the character.
    let ch = char::from_u32(ch & 0xFFFF).filter(|c| *c != '\0')?;
    Some(normalize_char(ch))
}

/// Case-fold for comparison: MAPVK_VK_TO_CHAR reports the *unshifted base*
/// character in uppercase for letters.
fn normalize_char(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

/// Inject an exact character irrespective of the host keyboard layout.
fn unicode_key_input(ch: char, pressed: bool) -> INPUT {
    let mut buf = [0u16; 2];
    let units = ch.encode_utf16(&mut buf);
    let mut flags = KEYEVENTF_UNICODE;
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    // Characters outside the BMP need surrogate pairs; those are so rare on
    // keyboards that sending the first unit only would corrupt them — route
    // them through the Text path instead by picking the replacement char.
    let scan = if units.len() == 1 { buf[0] } else { 0xFFFD };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(code: &str, pressed: bool) -> Option<INPUT> {
    let sc = code_to_scancode(code)?;
    let extended = sc & 0xE000 == 0xE000;
    let scan = sc & 0xFF;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    // Also resolve the VK for apps that ignore scancodes.
    let vk = unsafe { MapVirtualKeyW(scan as u32, MAPVK_VSC_TO_VK) } as u16;
    Some(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

/// Owned Windows Ink synthetic pen device.
struct PenDevice {
    handle: HSYNTHETICPOINTERDEVICE,
    in_contact: bool,
}

// SAFETY: the device handle is only used under the sink's mutex.
unsafe impl Send for PenDevice {}

impl PenDevice {
    fn create() -> windows::core::Result<Self> {
        // SAFETY: plain API call; the returned handle is owned by us.
        let handle = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }?;
        Ok(Self {
            handle,
            in_contact: false,
        })
    }

    fn inject(
        &mut self,
        phase: TouchPhase,
        pos: POINT,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
    ) -> windows::core::Result<()> {
        let (flags, contact) = match phase {
            // A Start while already down (missed End) is treated as a move.
            TouchPhase::Start if !self.in_contact => (
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN,
                true,
            ),
            TouchPhase::Start | TouchPhase::Move => (
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE,
                true,
            ),
            TouchPhase::End => (POINTER_FLAG_INRANGE | POINTER_FLAG_UP, false),
            TouchPhase::Cancel => (POINTER_FLAG_UP | POINTER_FLAG_CANCELED, false),
        };
        // Moves without a preceding down are hover updates (in range, no
        // contact) — real tablets emit these and apps show hover cursors.
        let (flags, contact) = if matches!(phase, TouchPhase::Move) && !self.in_contact {
            (POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE, false)
        } else {
            (flags, contact)
        };
        self.in_contact = contact;

        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: flags,
                        ptPixelLocation: pos,
                        ..Default::default()
                    },
                    penFlags: 0,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure: pen_pressure_1024(pressure, contact),
                    rotation: 0,
                    tiltX: pen_tilt_deg(tilt_x),
                    tiltY: pen_tilt_deg(tilt_y),
                },
            },
        };
        // SAFETY: `info` is fully initialized; the handle is live.
        unsafe { InjectSyntheticPointerInput(self.handle, &[info]) }
    }
}

impl Drop for PenDevice {
    fn drop(&mut self) {
        // SAFETY: handle owned by this struct, dropped exactly once.
        unsafe { DestroySyntheticPointerDevice(self.handle) };
    }
}

/// Owned synthetic touch device + the tracker that turns per-finger wire
/// events into the full frames the API requires.
struct TouchInjector {
    device: TouchDevice,
    tracker: TouchTracker,
}

struct TouchDevice {
    handle: HSYNTHETICPOINTERDEVICE,
}

// SAFETY: the device handle is only used under the sink's mutex.
unsafe impl Send for TouchDevice {}

impl TouchDevice {
    fn create() -> windows::core::Result<Self> {
        // SAFETY: plain API call; the returned handle is owned by us.
        let handle = unsafe {
            CreateSyntheticPointerDevice(
                PT_TOUCH,
                MAX_TOUCH_CONTACTS as u32,
                POINTER_FEEDBACK_DEFAULT,
            )
        }?;
        Ok(Self { handle })
    }

    /// Inject one full frame (all active contacts).
    fn inject(
        &self,
        frame: &[FrameContact],
        to_pixel: impl Fn(f32, f32) -> POINT,
    ) -> windows::core::Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        let infos: Vec<POINTER_TYPE_INFO> = frame
            .iter()
            .map(|c| {
                let (flags, in_contact) = match c.action {
                    ContactAction::Down => (
                        POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN,
                        true,
                    ),
                    ContactAction::Update => (
                        POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE,
                        true,
                    ),
                    ContactAction::Up => (POINTER_FLAG_UP, false),
                    ContactAction::Cancel => (POINTER_FLAG_UP | POINTER_FLAG_CANCELED, false),
                };
                let pos = to_pixel(c.x, c.y);
                POINTER_TYPE_INFO {
                    r#type: PT_TOUCH,
                    Anonymous: POINTER_TYPE_INFO_0 {
                        touchInfo: POINTER_TOUCH_INFO {
                            pointerInfo: POINTER_INFO {
                                pointerType: PT_TOUCH,
                                pointerId: c.slot,
                                pointerFlags: flags,
                                ptPixelLocation: pos,
                                ..Default::default()
                            },
                            touchFlags: 0,
                            touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE,
                            // A small square around the point — apps use the
                            // contact area for touch-target disambiguation.
                            rcContact: RECT {
                                left: pos.x - 2,
                                top: pos.y - 2,
                                right: pos.x + 2,
                                bottom: pos.y + 2,
                            },
                            rcContactRaw: RECT::default(),
                            orientation: 0,
                            pressure: if in_contact {
                                pen_pressure_1024(c.pressure, true)
                            } else {
                                0
                            },
                        },
                    },
                }
            })
            .collect();
        // SAFETY: every info is fully initialized; the handle is live.
        unsafe { InjectSyntheticPointerInput(self.handle, &infos) }
    }
}

impl Drop for TouchDevice {
    fn drop(&mut self) {
        // SAFETY: handle owned by this struct, dropped exactly once.
        unsafe { DestroySyntheticPointerDevice(self.handle) };
    }
}

impl InputSink for WindowsInputSink {
    fn apply(&self, events: &[InputEvent]) {
        let mut batch: Vec<INPUT> = Vec::with_capacity(events.len() + 2);
        for e in events {
            match e {
                InputEvent::MouseMove { x, y } => {
                    let (ax, ay, flags) = self.map_coords(*x, *y);
                    batch.push(mouse_input(ax, ay, 0, flags));
                }
                InputEvent::MouseButton { button, pressed } => {
                    let flags = match (button, pressed) {
                        (0, true) => MOUSEEVENTF_LEFTDOWN,
                        (0, false) => MOUSEEVENTF_LEFTUP,
                        (1, true) => MOUSEEVENTF_MIDDLEDOWN,
                        (1, false) => MOUSEEVENTF_MIDDLEUP,
                        (2, true) => MOUSEEVENTF_RIGHTDOWN,
                        (2, false) => MOUSEEVENTF_RIGHTUP,
                        (3, true) | (4, true) => MOUSEEVENTF_XDOWN,
                        (3, false) | (4, false) => MOUSEEVENTF_XUP,
                        _ => continue,
                    };
                    let data = match button {
                        3 => XBUTTON1 as i32,
                        4 => XBUTTON2 as i32,
                        _ => 0,
                    };
                    batch.push(mouse_input(0, 0, data, flags));
                }
                InputEvent::Wheel { dx, dy } => {
                    if dy.abs() > f32::EPSILON {
                        batch.push(mouse_input(0, 0, (-dy * 120.0) as i32, MOUSEEVENTF_WHEEL));
                    }
                    if dx.abs() > f32::EPSILON {
                        batch.push(mouse_input(0, 0, (dx * 120.0) as i32, MOUSEEVENTF_HWHEEL));
                    }
                }
                InputEvent::Key { code, pressed, key } => {
                    // Track non-Shift modifier state (shortcut detection).
                    let modifier_bit = match code.as_str() {
                        "ControlLeft" | "ControlRight" => 1u8,
                        "AltLeft" | "AltRight" => 2,
                        "MetaLeft" | "MetaRight" => 4,
                        _ => 0,
                    };
                    if modifier_bit != 0 {
                        let mut m = self.modifiers.lock().unwrap();
                        if *pressed {
                            *m |= modifier_bit;
                        } else {
                            *m &= !modifier_bit;
                        }
                    }
                    let shortcut_held = *self.modifiers.lock().unwrap() != 0;
                    if let Some(i) =
                        layout_aware_key_input(code, key.as_deref(), *pressed, shortcut_held)
                    {
                        batch.push(i);
                    } else {
                        tracing::debug!(code, "unmapped key code ignored");
                    }
                }
                InputEvent::Text { text } => {
                    for u in text.encode_utf16() {
                        for &up in &[false, true] {
                            let mut flags = KEYEVENTF_UNICODE;
                            if up {
                                flags |= KEYEVENTF_KEYUP;
                            }
                            batch.push(INPUT {
                                r#type: INPUT_KEYBOARD,
                                Anonymous: INPUT_0 {
                                    ki: KEYBDINPUT {
                                        wVk: VIRTUAL_KEY(0),
                                        wScan: u,
                                        dwFlags: flags,
                                        time: 0,
                                        dwExtraInfo: 0,
                                    },
                                },
                            });
                        }
                    }
                }
                InputEvent::Pen {
                    phase,
                    x,
                    y,
                    pressure,
                    tilt_x,
                    tilt_y,
                } => {
                    // Preserve ordering: anything already queued for
                    // SendInput must land before the pen frame.
                    self.send(&batch);
                    batch.clear();
                    if self.inject_pen(*phase, *x, *y, *pressure, *tilt_x, *tilt_y) {
                        continue;
                    }
                    // Fallback: same single-pointer mouse mapping as touch.
                    self.pointer_as_mouse(&mut batch, *phase, *x, *y);
                }
                InputEvent::Touch {
                    id,
                    phase,
                    x,
                    y,
                    pressure,
                } => {
                    // Preserve ordering: anything already queued for
                    // SendInput must land before the touch frame.
                    self.send(&batch);
                    batch.clear();
                    if self.inject_touch(*id, *phase, *x, *y, *pressure) {
                        continue;
                    }
                    // Fallback: primary finger drives the mouse.
                    self.touch_as_mouse(&mut batch, *id, *phase, *x, *y);
                }
            }
        }
        self.send(&batch);
    }
}
