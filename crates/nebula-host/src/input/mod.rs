//! Input injection.
//!
//! Viewers send normalized (0..1) coordinates in stream space; the injector
//! maps them to host coordinates and synthesizes OS input events. Injection
//! only happens for devices the host user explicitly authorized (checked in
//! the server layer before events ever reach an injector).

#[cfg(windows)]
pub mod windows_inject;

#[cfg(windows)]
mod keymap;

use nebula_proto::InputEvent;

pub trait Injector: Send {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
    fn describe(&self) -> String;
}

/// Create the platform injector. On non-Windows builds this is a logging
/// injector (useful for development and integration tests).
pub fn create_injector() -> Box<dyn Injector> {
    #[cfg(windows)]
    {
        Box::new(windows_inject::SendInputInjector::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(LogInjector::default())
    }
}

/// Records events instead of injecting them. Used on non-Windows dev hosts
/// and by tests.
#[derive(Default)]
pub struct LogInjector {
    pub count: u64,
}

impl Injector for LogInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        self.count += 1;
        tracing::debug!("input (not injected on this platform): {event:?}");
        Ok(())
    }

    fn describe(&self) -> String {
        "log-only injector (non-Windows build)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_injector_counts() {
        let mut i = LogInjector::default();
        i.inject(&InputEvent::MouseMove { x: 0.5, y: 0.5 }).unwrap();
        i.inject(&InputEvent::Key {
            code: "KeyA".into(),
            down: true,
        })
        .unwrap();
        assert_eq!(i.count, 2);
    }
}
