//! Input bridge: applies viewer input events to the host.
//!
//! Grant model: events are only *accepted into a session* when the device's
//! input grant is on (deny by default, toggled in the panel); this module is
//! the last line and re-checks nothing — callers enforce grants.
//!
//! * Windows: `SendInput` for mouse/keyboard; **synthetic pointer devices**
//!   (Windows Ink, Win10 1809+) for pen (pressure/tilt) and multi-touch
//!   (up to 10 contacts, pinch/rotate reach apps as real gestures), each
//!   falling back to the mouse mapping where the API is unavailable.
//! * Non-Windows hosts: structured log sink (useful for tests/CI).

pub mod touch_frame;
#[cfg(windows)]
mod windows_inject;

use ndsp_protocol::messages::InputEvent;
use std::sync::Arc;

use crate::state::AppState;

pub trait InputSink: Send + Sync {
    fn apply(&self, events: &[InputEvent]);

    /// Release anything still "held" (touch contacts, dragged mouse button,
    /// modifier keys). Called when a session's input stream ends so an
    /// abrupt viewer disconnect mid-gesture never leaves the host stuck in
    /// a drag or a half-finished pinch.
    fn release(&self) {}
}

/// NDSP pen pressure (0..1) → Windows Ink pressure (0..1024). While the pen
/// is in contact a zero would be interpreted as "no contact" by some apps,
/// so contact pressure has a floor of 1.
pub fn pen_pressure_1024(pressure: f32, in_contact: bool) -> u32 {
    let p = (pressure.clamp(0.0, 1.0) * 1024.0).round() as u32;
    if in_contact {
        p.clamp(1, 1024)
    } else {
        p.min(1024)
    }
}

/// NDSP tilt (normalized -1..1, where ±1 = ±90°) → Windows Ink degrees.
pub fn pen_tilt_deg(tilt: f32) -> i32 {
    (tilt.clamp(-1.0, 1.0) * 90.0).round() as i32
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_pressure_scaling() {
        assert_eq!(pen_pressure_1024(0.0, false), 0, "hover may report zero");
        assert_eq!(
            pen_pressure_1024(0.0, true),
            1,
            "contact pressure has a floor"
        );
        assert_eq!(pen_pressure_1024(0.5, true), 512);
        assert_eq!(pen_pressure_1024(1.0, true), 1024);
        assert_eq!(pen_pressure_1024(7.5, true), 1024, "clamped");
        assert_eq!(pen_pressure_1024(-1.0, true), 1, "clamped");
    }

    #[test]
    fn pen_tilt_normalized_to_degrees() {
        assert_eq!(pen_tilt_deg(0.0), 0);
        assert_eq!(pen_tilt_deg(0.5), 45);
        assert_eq!(pen_tilt_deg(1.0), 90);
        assert_eq!(pen_tilt_deg(-1.0), -90);
        assert_eq!(pen_tilt_deg(3.0), 90, "clamped");
        assert_eq!(pen_tilt_deg(-3.0), -90, "clamped");
    }
}
