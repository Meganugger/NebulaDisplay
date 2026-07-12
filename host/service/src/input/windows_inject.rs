//! Windows input injection via `SendInput`.
//!
//! Coordinates arrive normalized (0..1) relative to the *streamed monitor*.
//! They are mapped through the captured output's desktop rectangle into the
//! full virtual-desktop space (`MOUSEEVENTF_VIRTUALDESK`), so taps land on
//! the correct pixel even when the captured monitor is not the primary one
//! or sits at a non-zero desktop offset in a multi-monitor layout.

use ndsp_protocol::messages::{InputEvent, TouchPhase};
use std::sync::{Arc, Mutex};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_CHAR, MAPVK_VK_TO_VSC,
    MAPVK_VSC_TO_VK, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY, VK_LSHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use super::InputSink;
use crate::state::AppState;

pub struct WindowsInputSink {
    state: Arc<AppState>,
    /// Touch state so single-finger touch maps to press-drag-release.
    touch_down: Mutex<bool>,
    /// Viewer shift state (tracked from ShiftLeft/ShiftRight key events) —
    /// needed to compensate shift when translating layout-mismatched keys.
    shift_down: Mutex<bool>,
    /// Windows Ink pen injection (pressure/tilt) when the platform supports
    /// `InjectSyntheticPointerInput`; `None` → pen falls back to mouse.
    pen: Option<Mutex<super::windows_pen::PenInjector>>,
    /// Gamepad injection via `Windows.UI.Input.Preview.Injection`; lazily
    /// initialized on the first gamepad event, `None` after a failed init.
    gamepad: Mutex<Option<Option<super::windows_gamepad::GamepadInjector>>>,
}

impl WindowsInputSink {
    pub fn new(state: Arc<AppState>) -> Self {
        let pen = super::windows_pen::PenInjector::new()
            .map(Mutex::new)
            .map_err(|e| {
                tracing::info!("pen injection unavailable ({e:#}); pen maps to mouse");
                e
            })
            .ok();
        Self {
            state,
            touch_down: Mutex::new(false),
            shift_down: Mutex::new(false),
            pen,
            gamepad: Mutex::new(None),
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

    /// Layout-aware key dispatch (roadmap "send both `code` and `key`; host
    /// picks"). Policy, in order:
    ///
    /// 1. Named / non-printable keys (Enter, arrows, modifiers, F-keys, and
    ///    anything without a `key` char) → **scancode** injection: exact
    ///    physical semantics, works for shortcuts and games.
    /// 2. Printable keys whose scancode already produces the same base
    ///    character on the host layout → scancode injection (fast path when
    ///    the layouts agree).
    /// 3. Layout mismatch (AZERTY/Dvorak/…): translate the character through
    ///    `VkKeyScanW` on the *host* layout and inject that VK, temporarily
    ///    compensating Shift when the two layouts disagree about needing it.
    ///    AltGr-reachable characters skip to (4) — synthesizing Ctrl+Alt is
    ///    more likely to trigger app shortcuts than to help.
    /// 4. Characters not on the host layout at all → `KEYEVENTF_UNICODE`
    ///    text injection on key-down (no release event exists for unicode
    ///    injection pairs; auto-repeat is handled viewer-side).
    fn push_key(&self, batch: &mut Vec<INPUT>, code: &str, key: Option<&str>, pressed: bool) {
        let printable = printable_char(key);
        if let Some(sc) = code_to_scancode(code) {
            let Some(ch) = printable else {
                batch.push(scancode_input(sc, pressed));
                return;
            };
            let vk_from_sc = unsafe { MapVirtualKeyW((sc & 0xFF) as u32, MAPVK_VSC_TO_VK) } as u16;
            if host_base_char(vk_from_sc) == Some(ch.to_ascii_uppercase()) {
                batch.push(scancode_input(sc, pressed));
                return;
            }
            // Layout mismatch — translate through the host layout.
            let scan = unsafe { VkKeyScanW(ch as u16) };
            if scan != -1 {
                let vk = (scan & 0xFF) as u16;
                let shift_state = ((scan >> 8) & 0xFF) as u8;
                if shift_state & 0b110 == 0 {
                    let needs_shift = shift_state & 1 != 0;
                    let shift_held = *self.shift_down.lock().unwrap();
                    if pressed && needs_shift != shift_held {
                        // Compensate: flip shift around the translated key.
                        batch.push(vk_key_input(VK_LSHIFT.0, needs_shift));
                        batch.push(vk_key_input(vk, true));
                        batch.push(vk_key_input(vk, false));
                        batch.push(vk_key_input(VK_LSHIFT.0, !needs_shift));
                        return; // release already synthesized
                    }
                    batch.push(vk_key_input(vk, pressed));
                    return;
                }
                // AltGr combination → fall through to unicode.
            }
            if pressed {
                let mut buf = [0u16; 2];
                for &unit in ch.encode_utf16(&mut buf).iter() {
                    batch.push(unicode_input(unit, true));
                    batch.push(unicode_input(unit, false));
                }
            }
            return;
        }
        // Unknown physical code — inject the character if we have one.
        if let Some(ch) = printable {
            if pressed {
                let mut buf = [0u16; 2];
                for &unit in ch.encode_utf16(&mut buf).iter() {
                    batch.push(unicode_input(unit, true));
                    batch.push(unicode_input(unit, false));
                }
            }
        } else {
            tracing::debug!(code, "unmapped key code ignored");
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

fn scancode_input(sc: u16, pressed: bool) -> INPUT {
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

/// VK-based injection (no `KEYEVENTF_SCANCODE`) — the system resolves it
/// through the *host's* active layout, which is exactly what a translated
/// key needs.
fn vk_key_input(vk: u16, pressed: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let flags = if pressed {
        Default::default()
    } else {
        KEYEVENTF_KEYUP
    };
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

fn unicode_input(unit: u16, pressed: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// The layout-resolved `KeyboardEvent.key` as a single printable character.
/// Named keys ("Enter", "Shift", "ArrowLeft", …) are multi-char and return
/// `None`, as do control characters.
fn printable_char(key: Option<&str>) -> Option<char> {
    let k = key?;
    let mut chars = k.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_control() {
        return None;
    }
    Some(c)
}

/// Base (unshifted) character the host layout produces for a VK, uppercased
/// for caseless comparison. `None` for dead keys and non-character VKs.
fn host_base_char(vk: u16) -> Option<char> {
    let r = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_CHAR) };
    if r == 0 || r & 0x8000_0000 != 0 {
        return None; // no char / dead key
    }
    char::from_u32(r & 0xFFFF).map(|c| c.to_ascii_uppercase())
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
                    if code == "ShiftLeft" || code == "ShiftRight" {
                        *self.shift_down.lock().unwrap() = *pressed;
                    }
                    self.push_key(&mut batch, code, key.as_deref(), *pressed);
                }
                InputEvent::Text { text } => {
                    for u in text.encode_utf16() {
                        batch.push(unicode_input(u, true));
                        batch.push(unicode_input(u, false));
                    }
                }
                InputEvent::Pen {
                    phase,
                    x,
                    y,
                    pressure,
                    tilt_x,
                    tilt_y,
                } if self.pen.is_some() => {
                    // Real Windows Ink injection with pressure/tilt. Flush
                    // any queued SendInput events first so ordering between
                    // the two APIs is preserved.
                    self.send(&batch);
                    batch.clear();
                    let rect = *self.state.capture_rect.lock().unwrap();
                    if let Some(pen) = &self.pen {
                        pen.lock()
                            .unwrap()
                            .inject(*phase, *x, *y, *pressure, *tilt_x, *tilt_y, rect);
                    }
                }
                InputEvent::Gamepad { .. } => {
                    // Lazy init: most sessions never send gamepad events.
                    let mut slot = self.gamepad.lock().unwrap();
                    let injector = slot.get_or_insert_with(|| {
                        match super::windows_gamepad::GamepadInjector::new() {
                            Ok(g) => Some(g),
                            Err(e) => {
                                tracing::info!("gamepad injection unavailable: {e:#}");
                                None
                            }
                        }
                    });
                    if let Some(g) = injector {
                        g.inject(e);
                    }
                }
                InputEvent::Touch { phase, x, y, .. } | InputEvent::Pen { phase, x, y, .. } => {
                    // Single-pointer mapping to mouse (pen lands here only
                    // when synthetic pointer devices are unavailable).
                    let (ax, ay, flags) = self.map_coords(*x, *y);
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
        }
        self.send(&batch);
    }
}
