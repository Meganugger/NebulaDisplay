//! Frame sources.
//!
//! A [`FrameSource`] produces BGRA frames for the pipeline. Implementations:
//!
//! * [`test_pattern::TestPatternSource`] — synthetic animated pattern, used on
//!   non-Windows dev machines, in CI, and as a last-resort diagnostic source.
//! * `dxgi::DesktopDuplicationSource` (Windows) — mirrors a physical monitor
//!   using the DXGI Desktop Duplication API (capture-only fallback mode).
//! * `idd::VirtualMonitorSource` (Windows) — reads frames the NebulaDisplay
//!   IddCx driver writes into a shared-memory ring, giving a *real* extra
//!   virtual monitor (extend mode).

pub mod test_pattern;

#[cfg(windows)]
pub mod dxgi;
#[cfg(windows)]
pub mod idd;

use std::time::Instant;

/// One captured frame. Pixel format is BGRA8 (the native DXGI/GDI layout),
/// tightly packed (`stride == width * 4`).
pub struct Frame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured_at: Instant,
}

pub trait FrameSource: Send {
    /// Current source dimensions.
    fn size(&self) -> (u32, u32);

    /// Block until the next frame is available (or a pacing timeout elapses)
    /// and return it. Returning `Ok(None)` means "no change since last frame"
    /// which lets the pipeline skip encoding entirely.
    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<Frame>>;

    /// Human-readable description for diagnostics ("DXGI monitor 0", "Test pattern").
    fn describe(&self) -> String;
}

/// Pick the best available source for this platform and configuration.
pub fn create_source(kind: &str, width: u32, height: u32) -> anyhow::Result<Box<dyn FrameSource>> {
    match kind {
        "test" => Ok(Box::new(test_pattern::TestPatternSource::new(
            width, height,
        ))),
        #[cfg(windows)]
        "screen" | "auto" => match dxgi::DesktopDuplicationSource::new(0) {
            Ok(s) => Ok(Box::new(s)),
            Err(e) => {
                tracing::warn!("DXGI duplication unavailable ({e}); using test pattern");
                Ok(Box::new(test_pattern::TestPatternSource::new(
                    width, height,
                )))
            }
        },
        #[cfg(windows)]
        "virtual" => Ok(Box::new(idd::VirtualMonitorSource::connect()?)),
        #[cfg(not(windows))]
        "screen" | "auto" => {
            tracing::info!("screen capture is Windows-only in this build; using test pattern");
            Ok(Box::new(test_pattern::TestPatternSource::new(
                width, height,
            )))
        }
        other => anyhow::bail!("unknown frame source '{other}' (expected auto|screen|test)"),
    }
}
