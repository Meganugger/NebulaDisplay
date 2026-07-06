//! Frame acquisition.
//!
//! * Windows: DXGI Desktop Duplication (mirror mode; the IddCx driver adds
//!   true extend mode — see `host/windows-driver/`).
//! * Everywhere else / `--test-pattern`: a synthetic animated source so the
//!   whole pipeline is exercisable in CI and on dev machines.

mod test_pattern;
#[cfg(windows)]
mod windows_dxgi;

use ndsp_protocol::messages::DisplayMode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::state::{AppState, CapturedFrame};
use crate::util::now_us;

/// A source of BGRA frames. Implementations may block briefly inside
/// `next_frame` (it runs on a dedicated blocking thread).
pub trait FrameSource: Send {
    fn name(&self) -> &'static str;
    fn mode(&self) -> DisplayMode;
    /// Produce the next frame into `out` (tightly packed BGRA). Returns
    /// `Ok(false)` if nothing changed / timed out (caller just retries).
    fn next_frame(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool>;
}

/// Test helper: expose the synthetic source to other modules' unit tests.
#[cfg(test)]
pub fn test_pattern_for_tests(width: u32, height: u32) -> impl FrameSource {
    test_pattern::TestPatternSource::new(width, height)
}

/// Choose the best available source for this platform.
pub fn create_source(force_test_pattern: bool, width: u32, height: u32) -> Box<dyn FrameSource> {
    #[cfg(windows)]
    {
        if !force_test_pattern {
            match windows_dxgi::DxgiDuplicationSource::new() {
                Ok(src) => {
                    info!(
                        "using DXGI desktop duplication ({}x{})",
                        src.mode().width,
                        src.mode().height
                    );
                    return Box::new(src);
                }
                Err(e) => {
                    warn!("DXGI duplication unavailable ({e:#}); falling back to test pattern");
                }
            }
        }
    }
    let _ = force_test_pattern; // silence unused on non-windows
    info!("using synthetic test-pattern source ({width}x{height})");
    Box::new(test_pattern::TestPatternSource::new(width, height))
}

/// Capture loop: pull frames from the source at the configured cadence and
/// publish them on the frame watch channel. Runs the blocking source on a
/// dedicated thread via `spawn_blocking`.
pub async fn run_capture_loop(state: Arc<AppState>, mut source: Box<dyn FrameSource>) {
    let mode = source.mode();
    *state.mode.lock().unwrap() = mode;
    let max_fps = state.cfg.file.max_fps.clamp(1, 240);
    let tick = Duration::from_secs_f64(1.0 / max_fps as f64);
    info!(source = source.name(), max_fps, "capture loop started");

    let state2 = state.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut seq: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut fps_window_start = Instant::now();
        let mut fps_frames: u32 = 0;
        loop {
            if state2.is_shutdown() {
                tracing::info!("capture loop stopping (shutdown)");
                break;
            }
            let loop_start = Instant::now();
            let has_clients = !state2.clients.lock().unwrap().is_empty();
            if !has_clients {
                // Idle: capture at 2 fps to keep the "latest frame" warm
                // without burning CPU.
                std::thread::sleep(Duration::from_millis(500));
            }
            match source.next_frame(&mut buf) {
                Ok(true) => {
                    seq += 1;
                    fps_frames += 1;
                    let m = source.mode();
                    let frame = Arc::new(CapturedFrame {
                        seq,
                        timestamp_us: now_us(),
                        width: m.width,
                        height: m.height,
                        bgra: std::mem::take(&mut buf),
                    });
                    // Reuse allocation next round.
                    buf = Vec::with_capacity((m.width * m.height * 4) as usize);
                    let _ = state2.frame_tx.send(Some(frame));
                }
                Ok(false) => {}
                Err(e) => {
                    warn!("capture error: {e:#}; retrying in 1s");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            if fps_window_start.elapsed() >= Duration::from_secs(2) {
                let fps = fps_frames as f32 / fps_window_start.elapsed().as_secs_f32();
                state2.host_stats.lock().unwrap().capture_fps = fps;
                fps_window_start = Instant::now();
                fps_frames = 0;
            }
            if let Some(rem) = tick.checked_sub(loop_start.elapsed()) {
                std::thread::sleep(rem);
            }
        }
    });
    let _ = handle.await;
}
