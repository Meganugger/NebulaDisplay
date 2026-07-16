//! Input bridge: applies viewer input events to the host.
//!
//! Grant model: events are only *accepted into a session* when the device's
//! input grant is on (deny by default, toggled in the panel); this module is
//! the last line and re-checks nothing — callers enforce grants.
//!
//! * Windows: `SendInput` for mouse/keyboard; touch is mapped to mouse until
//!   the InjectTouchInput path lands (roadmap).
//! * Non-Windows hosts: structured log sink (useful for tests/CI).

#[cfg(windows)]
mod windows_inject;

use ndsp_protocol::messages::InputEvent;
use std::sync::Arc;

use crate::state::AppState;

pub trait InputSink: Send + Sync {
    fn apply(&self, events: &[InputEvent]);
}

pub fn create_sink(state: Arc<AppState>) -> Box<dyn InputSink> {
    #[cfg(windows)]
    {
        Box::new(windows_inject::WindowsInputSink::new(state))
    }
    #[cfg(not(windows))]
    {
        let _ = state;
        Box::new(LogSink)
    }
}

#[cfg(not(windows))]
struct LogSink;

#[cfg(not(windows))]
impl InputSink for LogSink {
    fn apply(&self, events: &[InputEvent]) {
        for e in events {
            tracing::info!(event = ?e, "input event (no injection backend on this OS)");
        }
    }
}

/// How a key event should be injected (ROADMAP P2.13: layout-aware mapping).
#[derive(Debug, PartialEq, Eq)]
pub enum KeyPlan {
    /// Positional: translate the W3C `code` to a hardware scancode. Right
    /// for non-printable keys (Enter, arrows, F-keys, modifiers) and for
    /// printable keys when the viewer didn't tell us what character its
    /// layout produces.
    Scancode,
    /// Layout-aware: the viewer's layout produced this character; inject
    /// whatever the *host* layout needs to type it (falling back to Unicode
    /// injection when the host layout can't). Keeps an AZERTY tablet typing
    /// "a" on a QWERTY host instead of "q".
    Char(char),
}

/// Physical-position codes whose *character* is layout-dependent. Everything
/// else (Space, Enter, arrows, Numpad…) is injected positionally.
fn is_layout_sensitive_code(code: &str) -> bool {
    matches!(
        code,
        "Backquote"
            | "Minus"
            | "Equal"
            | "BracketLeft"
            | "BracketRight"
            | "Backslash"
            | "Semicolon"
            | "Quote"
            | "Comma"
            | "Period"
            | "Slash"
            | "IntlBackslash"
            | "IntlRo"
            | "IntlYen"
    ) || (code.len() == 4 && code.starts_with("Key"))
        || (code.len() == 6 && code.starts_with("Digit"))
}

/// Decide how to inject a key, given the positional `code` and the optional
/// layout-aware `key` string from the viewer.
pub fn plan_key(code: &str, key: Option<&str>) -> KeyPlan {
    let Some(key) = key else {
        return KeyPlan::Scancode;
    };
    // Only single printable characters are layout candidates; named keys
    // ("Enter", "Shift", "Dead"…) and empty strings stay positional.
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return KeyPlan::Scancode;
    };
    if ch.is_control() || !is_layout_sensitive_code(code) {
        return KeyPlan::Scancode;
    }
    KeyPlan::Char(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_keys_use_layout_char() {
        assert_eq!(plan_key("KeyQ", Some("a")), KeyPlan::Char('a')); // AZERTY
        assert_eq!(plan_key("KeyA", Some("A")), KeyPlan::Char('A')); // shifted
        assert_eq!(plan_key("Digit2", Some("é")), KeyPlan::Char('é'));
        assert_eq!(plan_key("Semicolon", Some("ö")), KeyPlan::Char('ö'));
    }

    #[test]
    fn non_printable_and_positional_keys_stay_scancode() {
        assert_eq!(plan_key("Enter", Some("Enter")), KeyPlan::Scancode);
        assert_eq!(plan_key("ShiftLeft", Some("Shift")), KeyPlan::Scancode);
        assert_eq!(plan_key("Space", Some(" ")), KeyPlan::Scancode);
        assert_eq!(plan_key("ArrowUp", Some("ArrowUp")), KeyPlan::Scancode);
        assert_eq!(plan_key("Numpad1", Some("1")), KeyPlan::Scancode);
        assert_eq!(plan_key("KeyA", None), KeyPlan::Scancode); // legacy client
        assert_eq!(plan_key("KeyA", Some("Dead")), KeyPlan::Scancode);
        assert_eq!(plan_key("KeyA", Some("")), KeyPlan::Scancode);
    }
}
