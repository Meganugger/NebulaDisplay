//! Windows input injection via `SendInput` + synthetic pen pointers.
//!
//! Coordinates arrive normalized (0..1) relative to the *streamed monitor*.
//! They are mapped through the captured output's desktop rectangle into the
//! full virtual-desktop space (`MOUSEEVENTF_VIRTUALDESK`), so taps land on
//! the correct pixel even when the captured monitor is not the primary one
//! or sits at a non-zero desktop offset in a multi-monitor layout.
//!
//! # Keyboard: layout-aware mapping
//!
//! Viewers send the physical W3C `code` and (when available) the
//! layout-aware `key` character. Policy:
//!
//! * codes that map to a scancode are injected as scancodes — preserving
//!   shortcuts, games, and modifier semantics;
//! * except when the *host's* layout would produce a different character
//!   for that physical key (checked via `VkKeyScanW`) and no Ctrl/Alt/Win
//!   modifier is held — then the `key` character is injected as Unicode so
//!   typed text comes out exactly as the viewer typed it (e.g. QWERTY
//!   viewer → AZERTY host);
//! * unmapped codes fall back to Unicode injection of `key`.
//!
//! # Pen: true stylus injection
//!
//! Pen events go through `CreateSyntheticPointerDevice(PT_PEN)` +
//! `InjectSyntheticPointerInput` with real pressure/tilt, so Ink-aware apps
//! get genuine stylus strokes. If the synthetic-pointer API is unavailable
//! (pre-1809) or device creation fails, pens degrade to the mouse mapping.

use ndsp_protocol::messages::{InputEvent, TouchPhase};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT,
    POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VSC_TO_VK, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1,
    XBUTTON2,
};

use super::InputSink;
use crate::state::AppState;

/// `HSYNTHETICPOINTERDEVICE` is an opaque token; the synthetic-pointer APIs
/// are not documented as thread-affine and injection happens from whichever
/// session task received the events.
struct PenDevice(HSYNTHETICPOINTERDEVICE);
// SAFETY: opaque handle, only used with InjectSyntheticPointerInput.
unsafe impl Send for PenDevice {}

#[derive(Default)]
struct PenState {
    device: Option<PenDevice>,
    /// Creation failed once (old Windows) — don't retry every event.
    unavailable: bool,
    in_contact: bool,
}

#[derive(Default)]
struct ModState {
    ctrl: bool,
    alt: bool,
    meta: bool,
}

pub struct WindowsInputSink {
    state: Arc<AppState>,
    /// Touch state so single-finger touch maps to press-drag-release.
    touch_down: Mutex<bool>,
    /// Synthetic pen pointer device (lazily created on first pen event).
    pen: Mutex<PenState>,
    /// Live modifier state, tracked from the key events we inject.
    mods: Mutex<ModState>,
    /// Codes whose *press* was injected as Unicode — their release must be
    /// swallowed instead of emitting a stray scancode key-up.
    unicode_active: Mutex<HashSet<String>>,
}

impl WindowsInputSink {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            touch_down: Mutex::new(false),
            pen: Mutex::new(PenState::default()),
            mods: Mutex::new(ModState::default()),
            unicode_active: Mutex::new(HashSet::new()),
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

    /// Normalized (0..1) → desktop pixel coordinates of the captured
    /// monitor (falls back to the virtual screen if no rect is known).
    fn map_pixels(&self, x: f32, y: f32) -> (i32, i32) {
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
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        (
            vx + (x * (vw.max(1) - 1) as f64).round() as i32,
            vy + (y * (vh.max(1) - 1) as f64).round() as i32,
        )
    }

    /// True pen injection. Returns false when the synthetic-pointer API is
    /// unavailable, telling the caller to fall back to the mouse mapping.
    fn inject_pen(
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
            // SAFETY: plain user32 call; failure handled below.
            match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
                Ok(dev) => pen.device = Some(PenDevice(dev)),
                Err(e) => {
                    tracing::info!(
                        "synthetic pen unavailable ({e}); stylus falls back to mouse mapping"
                    );
                    pen.unavailable = true;
                    return false;
                }
            }
        }

        let flags: POINTER_FLAGS = match phase {
            TouchPhase::Start => {
                pen.in_contact = true;
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN
            }
            TouchPhase::Move if pen.in_contact => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE
            }
            // Hover move (no contact yet).
            TouchPhase::Move => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
            TouchPhase::End | TouchPhase::Cancel => {
                pen.in_contact = false;
                POINTER_FLAG_UP
            }
        };
        let (px, py) = self.map_pixels(x, y);
        let in_contact = pen.in_contact || matches!(phase, TouchPhase::End | TouchPhase::Cancel);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 1,
                        ptPixelLocation: POINT { x: px, y: py },
                        pointerFlags: flags,
                        ..Default::default()
                    },
                    penFlags: 0,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    // Windows pen pressure is 0..1024; keep ≥1 while touching
                    // so zero-pressure viewers still draw.
                    pressure: if in_contact {
                        ((pressure.clamp(0.0, 1.0) * 1024.0) as u32).max(1)
                    } else {
                        0
                    },
                    rotation: 0,
                    tiltX: tilt_x.clamp(-90.0, 90.0) as i32,
                    tiltY: tilt_y.clamp(-90.0, 90.0) as i32,
                },
            },
        };
        let dev = pen.device.as_ref().expect("device created above");
        // SAFETY: fully initialized POINTER_TYPE_INFO for a device we own.
        if let Err(e) = unsafe { InjectSyntheticPointerInput(dev.0, &[info]) } {
            tracing::warn!("InjectSyntheticPointerInput failed: {e}");
        }
        true
    }

    /// Layout-aware key handling — see the module docs for the policy.
    fn handle_key(&self, code: &str, key: Option<&str>, pressed: bool, batch: &mut Vec<INPUT>) {
        // Track modifiers from the stream itself.
        {
            let mut mods = self.mods.lock().unwrap();
            match code {
                "ControlLeft" | "ControlRight" => mods.ctrl = pressed,
                "AltLeft" | "AltRight" => mods.alt = pressed,
                "MetaLeft" | "MetaRight" => mods.meta = pressed,
                _ => {}
            }
        }
        // Swallow the release of a press we turned into Unicode.
        if !pressed && self.unicode_active.lock().unwrap().remove(code) {
            return;
        }

        let printable: Option<char> = key.and_then(|k| {
            let mut chars = k.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_control() => Some(c),
                _ => None,
            }
        });

        if let Some(sc) = code_to_scancode(code) {
            // Would the host layout produce the viewer's character on this
            // physical key? If not (and no shortcut modifier is held),
            // inject the exact character instead.
            let layout_mismatch = pressed
                && printable.is_some_and(|ch| {
                    let mods = self.mods.lock().unwrap();
                    if mods.ctrl || mods.alt || mods.meta {
                        return false; // shortcuts follow physical position
                    }
                    let mut buf = [0u16; 2];
                    let Some(&unit) = ch.encode_utf16(&mut buf).first() else {
                        return false;
                    };
                    // SAFETY: plain user32 lookups.
                    let scan_vk = unsafe { MapVirtualKeyW((sc & 0xFF) as u32, MAPVK_VSC_TO_VK) };
                    let want = unsafe { VkKeyScanW(unit) };
                    if want == -1 {
                        return true; // no host key produces it at all
                    }
                    (want as u16 & 0xFF) as u32 != scan_vk
                });
            if layout_mismatch {
                if let Some(ch) = printable {
                    push_unicode(batch, ch);
                    self.unicode_active.lock().unwrap().insert(code.to_string());
                }
                return;
            }
            if let Some(i) = key_input(code, pressed) {
                batch.push(i);
            }
            return;
        }

        // Unmapped physical code — fall back to the character itself.
        if pressed {
            if let Some(ch) = printable {
                push_unicode(batch, ch);
            } else {
                tracing::debug!(code, ?key, "unmapped key ignored");
            }
        }
    }

    /// Single-pointer press-drag-release mapping onto the mouse.
    fn touch_as_mouse(&self, phase: TouchPhase, x: f32, y: f32, batch: &mut Vec<INPUT>) {
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

/// Down+up pair of KEYEVENTF_UNICODE inputs for one character.
fn push_unicode(batch: &mut Vec<INPUT>, ch: char) {
    let mut units = [0u16; 2];
    for &u in ch.encode_utf16(&mut units).iter() {
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
                InputEvent::Key { code, key, pressed } => {
                    self.handle_key(code, key.as_deref(), *pressed, &mut batch)
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
                    // Flush queued SendInput events first so ordering between
                    // the two injection APIs is preserved.
                    self.send(&batch);
                    batch.clear();
                    if !self.inject_pen(*phase, *x, *y, *pressure, *tilt_x, *tilt_y) {
                        // Pre-1809 fallback: pen behaves like touch-as-mouse.
                        self.touch_as_mouse(*phase, *x, *y, &mut batch);
                    }
                }
                InputEvent::Touch { phase, x, y, .. } => {
                    // Single-pointer mapping to mouse until InjectTouchInput
                    // integration (see docs/ROADMAP.md).
                    self.touch_as_mouse(*phase, *x, *y, &mut batch);
                }
            }
        }
        self.send(&batch);
    }
}
