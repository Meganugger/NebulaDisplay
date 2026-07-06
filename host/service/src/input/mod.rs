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
