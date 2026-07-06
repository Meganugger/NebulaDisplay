//! Frame acquisition.
//!
//! * Windows: DXGI Desktop Duplication (mirror mode; the IddCx driver adds
//!   true extend mode — see `host/windows-driver/`).
//! * Everywhere else / `--test-pattern`: a synthetic animated source so the
//!   whole pipeline is exercisable in CI and on dev machines.

mod test_pattern;
#[cfg(windows)]
mod windows_dxgi;
#[cfg(windows)]
mod windows_idd;

use ndsp_protocol::messages::DisplayMode;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::state::{AppState, CapturedFrame, CursorShapeData};
use crate::util::now_us;

/// A cursor change reported by a frame source since the previous poll.
pub struct CursorUpdate {
    /// Normalized (0..1) hotspot position against the captured surface.
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    /// Present only when the shape changed.
    pub shape: Option<Arc<CursorShapeData>>,
}

/// A source of BGRA frames. Implementations may block briefly inside
/// `next_frame` (it runs on a dedicated blocking thread).
pub trait FrameSource: Send {
    fn name(&self) -> &'static str;
    fn mode(&self) -> DisplayMode;
    /// Produce the next frame into `out` (tightly packed BGRA). Returns
    /// `Ok(false)` if nothing changed / timed out (caller just retries).
    fn next_frame(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool>;
    /// Desktop-space rectangle of the captured surface (left, top, right,
    /// bottom) when the platform has one — used to map viewer input onto the
    /// correct monitor of a multi-monitor virtual desktop.
    fn desktop_rect(&self) -> Option<(i32, i32, i32, i32)> {
        None
    }
    /// Cursor state change since the last call (None = unchanged). Polled by
    /// the capture loop after every `next_frame`, including `Ok(false)`
    /// returns — cursor-only movement must flow even when pixels don't.
    fn cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    /// Whether the source should blend the cursor into captured frames.
    /// Off while every client renders the cursor from its own channel.
    fn set_composite_cursor(&mut self, _on: bool) {}
}

/// Test helper: expose the synthetic source to other modules' unit tests.
#[cfg(test)]
pub fn test_pattern_for_tests(width: u32, height: u32) -> impl FrameSource {
    test_pattern::TestPatternSource::new(width, height)
}

/// Choose the best available source for this platform.
/// Priority on Windows: IddCx virtual-display ring (extend mode; ring index
/// from `display_index`) → DXGI duplication (mirror mode) → test pattern.
pub fn create_source(
    force_test_pattern: bool,
    width: u32,
    height: u32,
    display_index: u32,
) -> Box<dyn FrameSource> {
    #[cfg(windows)]
    {
        if !force_test_pattern {
            match windows_idd::WindowsIddSource::new(display_index) {
                Ok(src) if src.is_connected() => {
                    info!(
                        "using IddCx virtual display (extend mode, {}x{})",
                        src.mode().width,
                        src.mode().height
                    );
                    return Box::new(src);
                }
                Ok(_) => {
                    info!("driver ring present but no monitor attached yet; using mirror mode");
                }
                Err(e) => {
                    info!("virtual display driver not active ({e:#}); using mirror mode");
                }
            }
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
    let _ = (force_test_pattern, display_index); // silence unused on non-windows
    info!("using synthetic test-pattern source ({width}x{height})");
    Box::new(test_pattern::TestPatternSource::new(width, height))
}

/// How many recent frames to keep for buffer recycling. The watch channel
/// holds one and sessions borrow clones briefly during encode, so a small
/// ring recovers nearly every allocation in steady state.
const RECYCLE_RING: usize = 4;

/// Capture loop: pull frames from the source at the configured cadence and
/// publish them on the frame watch channel. Runs the blocking source on a
/// dedicated thread via `spawn_blocking`.
///
/// Frame buffers are recycled through a small ring of previously published
/// frames — once every consumer has dropped its `Arc`, the (multi-megabyte)
/// BGRA allocation is reused instead of hitting the allocator per frame.
pub async fn run_capture_loop(state: Arc<AppState>, mut source: Box<dyn FrameSource>) {
    let mode = source.mode();
    *state.mode.lock().unwrap() = mode;
    *state.capture_rect.lock().unwrap() = source.desktop_rect();
    let max_fps = state.cfg.file.max_fps.clamp(1, 240);
    let tick = Duration::from_secs_f64(1.0 / max_fps as f64);
    info!(source = source.name(), max_fps, "capture loop started");

    let state2 = state.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let mut seq: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut recycle: VecDeque<Arc<CapturedFrame>> = VecDeque::new();
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
                // Idle: skip capture entirely (zero CPU/GPU) but poll often
                // enough that a connecting client sees a frame within ~50 ms.
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let composited = !state2.cursor_channel_active();
            source.set_composite_cursor(composited);
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
                    recycle.push_back(frame.clone());
                    let _ = state2.frame_tx.send(Some(frame));
                    // Reclaim the oldest ring entry nobody references anymore.
                    if recycle.len() > RECYCLE_RING {
                        if let Some(old) = recycle.pop_front() {
                            match Arc::try_unwrap(old) {
                                Ok(inner) => {
                                    buf = inner.bgra;
                                    buf.clear();
                                }
                                // Still referenced by a slow session: put it
                                // back unless the ring is ballooning (then
                                // just let the consumer's drop free it).
                                Err(still_shared) if recycle.len() < RECYCLE_RING * 2 => {
                                    recycle.push_front(still_shared)
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    if buf.capacity() == 0 {
                        buf.reserve((m.width * m.height * 4) as usize);
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    warn!("capture error: {e:#}; retrying in 1s");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            // Cursor updates flow even when no pixels changed — that's the
            // whole point of the dedicated cursor channel.
            if let Some(cu) = source.cursor() {
                state2.cursor_tx.send_modify(|cs| {
                    cs.seq += 1;
                    cs.x = cu.x;
                    cs.y = cu.y;
                    cs.visible = cu.visible;
                    cs.composited = composited;
                    if let Some(shape) = cu.shape {
                        cs.shape_seq += 1;
                        cs.shape = Some(shape);
                    }
                });
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
