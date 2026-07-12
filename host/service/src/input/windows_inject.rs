//! Windows input injection via `SendInput` + synthetic pen pointers.
//!
//! Coordinates arrive normalized (0..1) relative to the *streamed monitor*.
//! They are mapped through the captured output's desktop rectangle into the
//! full virtual-desktop space (`MOUSEEVENTF_VIRTUALDESK`), so taps land on
//! the correct pixel even when the captured monitor is not the primary one
//! or sits at a non-zero desktop offset in a multi-monitor layout.
//!
//! ## Layout-aware keyboard mapping
//!
//! Viewers send both the physical `code` ("KeyQ") and, when known, the
//! layout-resolved `key` ("a" on AZERTY). The host picks per event:
//!
//! * modifier chords (Ctrl/Alt/Win held) and named keys → **scan codes**
//!   (positional, what games and shortcuts expect);
//! * printable characters whose `VkKeyScanW` mapping matches the currently
//!   held Shift state → that **virtual key** (host-layout-correct, supports
//!   auto-repeat and key-up);
//! * anything else printable → **`KEYEVENTF_UNICODE`** (exact character,
//!   layout-proof) injected on key-down only.
//!
//! Key-ups always replay whatever strategy the matching key-down used, so a
//! layout switch mid-hold can't wedge a key.
//!
//! ## True stylus injection
//!
//! Pen events use `CreateSyntheticPointerDevice(PT_PEN)` +
//! `InjectSyntheticPointerInput`, delivering real pressure/tilt/hover to
//! Windows Ink apps. If the synthetic pointer API is unavailable (< Win10
//! 1809 or a locked-down session), the sink falls back to the mouse mapping
//! permanently for the process lifetime.

use ndsp_protocol::messages::{InputEvent, TouchPhase};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT,
    POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MAPVK_VSC_TO_VK,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use super::InputSink;
use crate::state::AppState;

/// How a key-down was injected — its key-up must mirror it exactly.
#[derive(Debug, Clone, Copy)]
enum PressAction {
    /// Positional scan-code injection.
    Scan,
    /// Host-layout virtual key from `VkKeyScanW`.
    Vk(u16),
    /// `KEYEVENTF_UNICODE` down+up already sent on press; up is a no-op.
    UnicodeDone,
}

#[derive(Default)]
struct KeyState {
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
    /// Per-`code` action of the most recent key-down.
    pressed: HashMap<String, PressAction>,
}

#[derive(Default)]
struct PenState {
    device: Option<HSYNTHETICPOINTERDEVICE>,
    /// Synthetic pointer API unavailable — use the mouse fallback forever.
    unavailable: bool,
    in_contact: bool,
}

// HSYNTHETICPOINTERDEVICE is a plain handle owned by this sink only.
unsafe impl Send for PenState {}

pub struct WindowsInputSink {
    state: Arc<AppState>,
    /// Touch state so single-finger touch maps to press-drag-release.
    touch_down: Mutex<bool>,
    keys: Mutex<KeyState>,
    pen: Mutex<PenState>,
}

impl WindowsInputSink {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            touch_down: Mutex::new(false),
            keys: Mutex::new(KeyState::default()),
            pen: Mutex::new(PenState::default()),
        }
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

    /// Map a normalized (0..1) point on the streamed monitor to desktop
    /// pixel coordinates (for pointer injection, which is pixel-based).
    fn desktop_pixel(&self, x: f32, y: f32) -> (i32, i32) {
        let x = x.clamp(0.0, 1.0) as f64;
        let y = y.clamp(0.0, 1.0) as f64;
        if let Some((l, t, r, b)) = *self.state.capture_rect.lock().unwrap() {
            if r > l && b > t {
                return (
                    (l as f64 + x * (r - l - 1) as f64).round() as i32,
                    (t as f64 + y * (b - t - 1) as f64).round() as i32,
                );
            }
        }
        // SAFETY: GetSystemMetrics is always safe to call.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        (
            (x * (w - 1).max(1) as f64).round() as i32,
            (y * (h - 1).max(1) as f64).round() as i32,
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

    /// Layout-aware key event → INPUT list (see module docs for the policy).
    fn key_event(&self, code: &str, key: Option<&str>, pressed: bool) -> Vec<INPUT> {
        let mut ks = self.keys.lock().unwrap();
        // Track modifier state from the forwarded stream itself.
        match code {
            "ShiftLeft" | "ShiftRight" => ks.shift = pressed,
            "ControlLeft" | "ControlRight" => ks.ctrl = pressed,
            "AltLeft" | "AltRight" => ks.alt = pressed,
            "MetaLeft" | "MetaRight" => ks.meta = pressed,
            _ => {}
        }

        if !pressed {
            // Replay whatever the matching key-down did.
            let action = ks.pressed.remove(code).unwrap_or(PressAction::Scan);
            return match action {
                PressAction::Scan => key_input(code, false).into_iter().collect(),
                PressAction::Vk(vk) => vec![vk_input(vk, false)],
                PressAction::UnicodeDone => Vec::new(),
            };
        }

        let printable = key.filter(|k| {
            let mut chars = k.chars();
            matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control())
        });
        let is_modifier = matches!(
            code,
            "ShiftLeft"
                | "ShiftRight"
                | "ControlLeft"
                | "ControlRight"
                | "AltLeft"
                | "AltRight"
                | "MetaLeft"
                | "MetaRight"
        );

        // Shortcut chords and named/modifier keys are positional.
        if is_modifier || ks.ctrl || ks.alt || ks.meta || printable.is_none() {
            return match key_input(code, true) {
                Some(i) => {
                    ks.pressed.insert(code.to_string(), PressAction::Scan);
                    vec![i]
                }
                None => {
                    tracing::debug!(code, "unmapped key code ignored");
                    Vec::new()
                }
            };
        }

        // Printable character: prefer the host layout's virtual key when its
        // required shift state matches what the viewer is physically holding
        // (keeps auto-repeat + key-up semantics); otherwise inject the exact
        // character as Unicode.
        let ch = printable.unwrap().chars().next().unwrap();
        let mut units = [0u16; 2];
        let utf16 = ch.encode_utf16(&mut units);
        if utf16.len() == 1 {
            // SAFETY: VkKeyScanW is always safe to call.
            let scan = unsafe { VkKeyScanW(utf16[0]) };
            if scan != -1 {
                let vk = (scan & 0xFF) as u16;
                let mods = ((scan >> 8) & 0xFF) as u8;
                let needs_shift = mods & 0b001 != 0;
                let needs_ctrl_alt = mods & 0b110 != 0; // AltGr-style chars
                if !needs_ctrl_alt && needs_shift == ks.shift {
                    ks.pressed.insert(code.to_string(), PressAction::Vk(vk));
                    return vec![vk_input(vk, true)];
                }
            }
        }
        ks.pressed
            .insert(code.to_string(), PressAction::UnicodeDone);
        unicode_tap(printable.unwrap())
    }

    /// Inject a stylus event through the synthetic pen device. Returns false
    /// when the API is unavailable (caller falls back to the mouse path).
    fn pen_event(
        &self,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
    ) -> bool {
        let mut pen = self.pen.lock().unwrap();
        if pen.unavailable {
            return false;
        }
        if pen.device.is_none() {
            // SAFETY: plain API call; the handle is owned by this sink.
            match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
                Ok(dev) => pen.device = Some(dev),
                Err(e) => {
                    tracing::warn!(
                        "synthetic pen unavailable ({e}); stylus falls back to mouse mapping"
                    );
                    pen.unavailable = true;
                    return false;
                }
            }
        }
        let device = pen.device.expect("created above");

        let mut flags = POINTER_FLAG_PRIMARY | POINTER_FLAG_INRANGE;
        match phase {
            TouchPhase::Start => {
                flags |= POINTER_FLAG_DOWN | POINTER_FLAG_INCONTACT;
                pen.in_contact = true;
            }
            TouchPhase::Move => {
                flags |= POINTER_FLAG_UPDATE;
                if pen.in_contact {
                    flags |= POINTER_FLAG_INCONTACT;
                }
            }
            TouchPhase::End => {
                flags |= POINTER_FLAG_UP;
                pen.in_contact = false;
            }
            TouchPhase::Cancel => {
                flags |= POINTER_FLAG_UP | POINTER_FLAG_CANCELED;
                pen.in_contact = false;
            }
        }

        let (px, py) = self.desktop_pixel(x, y);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: windows::Win32::UI::Input::Pointer::POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerFlags: flags,
                        ptPixelLocation: windows::Win32::Foundation::POINT { x: px, y: py },
                        ..Default::default()
                    },
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    // Windows Ink pressure range is 0..1024.
                    pressure: (pressure.clamp(0.0, 1.0) * 1024.0) as u32,
                    tiltX: (tilt_x.clamp(-1.0, 1.0) * 90.0) as i32,
                    tiltY: (tilt_y.clamp(-1.0, 1.0) * 90.0) as i32,
                    ..Default::default()
                },
            },
        };
        // SAFETY: device is a live synthetic pointer handle; info is fully
        // initialized.
        if let Err(e) = unsafe { InjectSyntheticPointerInput(device, &[info]) } {
            tracing::debug!("pen injection failed: {e}");
        }
        true
    }
}

/// Virtual-key injection (layout-resolved path). The scan code is attached
/// for apps that read it.
fn vk_input(vk: u16, pressed: bool) -> INPUT {
    // SAFETY: MapVirtualKeyW is always safe to call.
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0);
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
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
    }
}

/// Exact-character injection: `KEYEVENTF_UNICODE` down+up per UTF-16 unit.
fn unicode_tap(text: &str) -> Vec<INPUT> {
    let mut out = Vec::new();
    for u in text.encode_utf16() {
        for &up in &[false, true] {
            let mut flags = KEYEVENTF_UNICODE;
            if up {
                flags |= KEYEVENTF_KEYUP;
            }
            out.push(INPUT {
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
    out
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
                    batch.extend(self.key_event(code, key.as_deref(), *pressed));
                }
                InputEvent::Text { text } => {
                    batch.extend(unicode_tap(text));
                }
                InputEvent::Pen {
                    phase,
                    x,
                    y,
                    pressure,
                    tilt_x,
                    tilt_y,
                } => {
                    // Real stylus injection (pressure/tilt/hover). Flush the
                    // SendInput batch first so ordering is preserved, then
                    // inject through the synthetic pointer device. On API
                    // failure fall through to the mouse mapping below.
                    self.send(&batch);
                    batch.clear();
                    if self.pen_event(*phase, *x, *y, *pressure, *tilt_x, *tilt_y) {
                        continue;
                    }
                    self.pointer_as_mouse(&mut batch, *phase, *x, *y);
                }
                InputEvent::Touch { phase, x, y, .. } => {
                    // Single-pointer mapping to mouse until InjectTouchInput
                    // integration (see docs/ROADMAP.md).
                    self.pointer_as_mouse(&mut batch, *phase, *x, *y);
                }
            }
        }
        self.send(&batch);
    }
}

impl WindowsInputSink {
    /// Single-pointer press-drag-release mapping to mouse events.
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
}
