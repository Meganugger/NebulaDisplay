//! Map W3C `KeyboardEvent.code` values (what browsers and our mobile SDKs
//! send) to Windows virtual-key codes.
//!
//! Using `code` (physical key) rather than `key` (translated character)
//! keeps the mapping layout-independent on the wire; Windows applies the
//! host keyboard layout when translating the virtual key.

#![cfg(windows)]

use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub fn code_to_vk(code: &str) -> Option<VIRTUAL_KEY> {
    // Letters and digits.
    if let Some(rest) = code.strip_prefix("Key") {
        let c = rest.chars().next()?;
        if rest.len() == 1 && c.is_ascii_uppercase() {
            return Some(VIRTUAL_KEY(c as u16));
        }
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        let c = rest.chars().next()?;
        if rest.len() == 1 && c.is_ascii_digit() {
            return Some(VIRTUAL_KEY(c as u16));
        }
    }
    if let Some(rest) = code.strip_prefix("Numpad") {
        if let Ok(n) = rest.parse::<u16>() {
            if n <= 9 {
                return Some(VIRTUAL_KEY(VK_NUMPAD0.0 + n));
            }
        }
    }
    if let Some(rest) = code.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(VIRTUAL_KEY(VK_F1.0 + n - 1));
            }
        }
    }

    Some(match code {
        "Enter" => VK_RETURN,
        "NumpadEnter" => VK_RETURN,
        "Space" => VK_SPACE,
        "Tab" => VK_TAB,
        "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "Insert" => VK_INSERT,
        "Escape" => VK_ESCAPE,
        "Home" => VK_HOME,
        "End" => VK_END,
        "PageUp" => VK_PRIOR,
        "PageDown" => VK_NEXT,
        "ArrowUp" => VK_UP,
        "ArrowDown" => VK_DOWN,
        "ArrowLeft" => VK_LEFT,
        "ArrowRight" => VK_RIGHT,
        "ShiftLeft" => VK_LSHIFT,
        "ShiftRight" => VK_RSHIFT,
        "ControlLeft" => VK_LCONTROL,
        "ControlRight" => VK_RCONTROL,
        "AltLeft" => VK_LMENU,
        "AltRight" => VK_RMENU,
        "MetaLeft" => VK_LWIN,
        "MetaRight" => VK_RWIN,
        "CapsLock" => VK_CAPITAL,
        "NumLock" => VK_NUMLOCK,
        "ScrollLock" => VK_SCROLL,
        "PrintScreen" => VK_SNAPSHOT,
        "Pause" => VK_PAUSE,
        "ContextMenu" => VK_APPS,
        "Minus" => VK_OEM_MINUS,
        "Equal" => VK_OEM_PLUS,
        "BracketLeft" => VK_OEM_4,
        "BracketRight" => VK_OEM_6,
        "Backslash" => VK_OEM_5,
        "Semicolon" => VK_OEM_1,
        "Quote" => VK_OEM_7,
        "Backquote" => VK_OEM_3,
        "Comma" => VK_OEM_COMMA,
        "Period" => VK_OEM_PERIOD,
        "Slash" => VK_OEM_2,
        "NumpadAdd" => VK_ADD,
        "NumpadSubtract" => VK_SUBTRACT,
        "NumpadMultiply" => VK_MULTIPLY,
        "NumpadDivide" => VK_DIVIDE,
        "NumpadDecimal" => VK_DECIMAL,
        "AudioVolumeUp" => VK_VOLUME_UP,
        "AudioVolumeDown" => VK_VOLUME_DOWN,
        "AudioVolumeMute" => VK_VOLUME_MUTE,
        "MediaPlayPause" => VK_MEDIA_PLAY_PAUSE,
        "MediaTrackNext" => VK_MEDIA_NEXT_TRACK,
        "MediaTrackPrevious" => VK_MEDIA_PREV_TRACK,
        _ => return None,
    })
}
