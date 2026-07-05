//! The per-session streaming pipeline.
//!
//! Runs on a dedicated OS thread (capture and JPEG encoding are synchronous
//! CPU work; keeping them off the async runtime avoids starving the
//! executor) and pushes ready-to-send packets into a bounded channel drained
//! by the WebSocket task.
//!
//! The bounded channel doubles as the congestion signal: when the socket
//! can't drain packets fast enough the channel fills, frames are dropped
//! host-side (never queued into a growing latency balloon), and the adaptive
//! controller backs off quality/FPS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nebula_proto::{StreamStats, VideoPacket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::adaptive::AdaptiveController;
use crate::capture::FrameSource;
use crate::encode::{dirty::DirtyDetector, JpegRegionEncoder, RegionEncoder};

/// Force a full-frame refresh at least this often so late-joining decoders
/// and any missed dirty rects self-heal.
const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(4);

/// Depth of the packet channel. Small on purpose: this is our latency cap
/// (at 60fps, 3 packets ≈ 50ms of queued video).
const CHANNEL_DEPTH: usize = 3;

pub struct PipelineHandle {
    pub stop: Arc<AtomicBool>,
    pub adaptive: Arc<Mutex<AdaptiveController>>,
    pub stats: Arc<Mutex<StreamStats>>,
    /// Set to force the next frame to be sent as a full keyframe.
    pub refresh: Arc<AtomicBool>,
    pub packets: mpsc::Receiver<Vec<u8>>,
}

pub fn spawn(
    mut source: Box<dyn FrameSource>,
    adaptive: Arc<Mutex<AdaptiveController>>,
) -> PipelineHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let refresh = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(StreamStats::default()));
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHANNEL_DEPTH);

    let h_stop = stop.clone();
    let h_refresh = refresh.clone();
    let h_stats = stats.clone();
    let h_adaptive = adaptive.clone();

    std::thread::Builder::new()
        .name("nebula-pipeline".into())
        .spawn(move || {
            info!("pipeline started: {}", source.describe());
            let (w, h) = source.size();
            let mut detector = DirtyDetector::new(w, h);
            let mut encoder = JpegRegionEncoder::new();
            let mut frame_id: u32 = 0;
            let mut last_full = Instant::now();
            // Rolling stats.
            let mut sent_bytes_window: u64 = 0;
            let mut sent_frames_window: u32 = 0;
            let mut window_start = Instant::now();
            let mut encode_ms_ema = 0.0f32;
            let mut capture_ms_ema = 0.0f32;

            loop {
                if h_stop.load(Ordering::Relaxed) {
                    break;
                }
                let settings = h_adaptive.lock().unwrap().tick();
                let interval = Duration::from_secs_f64(1.0 / settings.fps.max(1) as f64);
                let tick_start = Instant::now();

                let capture_start = Instant::now();
                let frame = match source.next_frame(interval.as_millis() as u32) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        // No new content; idle briefly and continue.
                        std::thread::sleep(Duration::from_millis(4));
                        continue;
                    }
                    Err(e) => {
                        warn!("frame source error: {e}; retrying in 500ms");
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
                capture_ms_ema = ema(capture_ms_ema, capture_start.elapsed().as_secs_f32() * 1e3);

                let force_full = h_refresh.swap(false, Ordering::Relaxed)
                    || last_full.elapsed() >= FULL_REFRESH_INTERVAL;
                if force_full {
                    detector.invalidate();
                }
                let Some(rect) = detector.detect(&frame.bgra, frame.width, frame.height) else {
                    // Screen unchanged — skip encode entirely.
                    std::thread::sleep(Duration::from_millis(4));
                    continue;
                };
                let full_frame =
                    rect.x == 0 && rect.y == 0 && rect.w == frame.width && rect.h == frame.height;
                if full_frame {
                    last_full = Instant::now();
                }

                let enc_start = Instant::now();
                let payload = match encoder.encode_region(
                    &frame.bgra,
                    frame.width,
                    frame.height,
                    rect,
                    settings.quality,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("encode error: {e}");
                        continue;
                    }
                };
                encode_ms_ema = ema(encode_ms_ema, enc_start.elapsed().as_secs_f32() * 1e3);

                frame_id = frame_id.wrapping_add(1);
                let packet = VideoPacket {
                    codec: encoder.codec(),
                    full_frame,
                    keyframe: full_frame,
                    frame_id,
                    capture_ts_micros: frame.captured_at.elapsed().as_micros() as u64,
                    x: rect.x as u16,
                    y: rect.y as u16,
                    w: rect.w as u16,
                    h: rect.h as u16,
                    stream_w: frame.width as u16,
                    stream_h: frame.height as u16,
                    payload: &payload,
                }
                .encode();
                let packet_len = packet.len() as u64;

                match tx.try_send(packet) {
                    Ok(()) => {
                        sent_bytes_window += packet_len;
                        sent_frames_window += 1;
                        let mut s = h_stats.lock().unwrap();
                        s.frames_sent += 1;
                        s.width = frame.width;
                        s.height = frame.height;
                        s.quality = settings.quality;
                        s.encode_ms = encode_ms_ema;
                        s.capture_ms = capture_ms_ema;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!("send queue full — dropping frame {frame_id}");
                        h_adaptive.lock().unwrap().on_send_drop();
                        // The dropped frame's content is still marked clean in
                        // the detector, so force a refresh to resend it.
                        h_refresh.store(true, Ordering::Relaxed);
                        h_stats.lock().unwrap().frames_dropped += 1;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }

                // Publish rolling fps/bitrate once per second.
                if window_start.elapsed() >= Duration::from_secs(1) {
                    let secs = window_start.elapsed().as_secs_f32();
                    let mut s = h_stats.lock().unwrap();
                    s.fps = sent_frames_window as f32 / secs;
                    s.bitrate_kbps = (sent_bytes_window * 8) as f32 / secs / 1000.0;
                    s.rtt_ms = h_adaptive.lock().unwrap().rtt_ms();
                    sent_bytes_window = 0;
                    sent_frames_window = 0;
                    window_start = Instant::now();
                }

                // Frame pacing.
                let elapsed = tick_start.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }
            info!("pipeline stopped");
        })
        .expect("spawn pipeline thread");

    PipelineHandle {
        stop,
        adaptive,
        stats,
        refresh,
        packets: rx,
    }
}

fn ema(prev: f32, sample: f32) -> f32 {
    if prev == 0.0 {
        sample
    } else {
        prev * 0.9 + sample * 0.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::test_pattern::TestPatternSource;
    use nebula_proto::Profile;

    #[tokio::test]
    async fn pipeline_produces_decodable_packets() {
        let source = Box::new(TestPatternSource::new(320, 240));
        let adaptive = Arc::new(Mutex::new(AdaptiveController::new(Profile::Balanced)));
        let mut handle = spawn(source, adaptive);

        let packet = tokio::time::timeout(Duration::from_secs(5), handle.packets.recv())
            .await
            .expect("timed out waiting for a packet")
            .expect("channel open");
        let video = VideoPacket::decode(&packet).unwrap();
        assert!(video.full_frame, "first packet must be a full frame");
        assert_eq!(video.stream_w, 320);
        assert_eq!(video.stream_h, 240);
        assert_eq!(&video.payload[..2], &[0xFF, 0xD8], "payload must be JPEG");

        handle.stop.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn refresh_flag_forces_full_frame() {
        let source = Box::new(TestPatternSource::new(160, 120));
        let adaptive = Arc::new(Mutex::new(AdaptiveController::new(Profile::Balanced)));
        let mut handle = spawn(source, adaptive);

        // Drain first (full) frame.
        let _ = tokio::time::timeout(Duration::from_secs(5), handle.packets.recv()).await;
        // Ask for a refresh; within a few frames we must see a full frame again.
        handle.refresh.store(true, Ordering::Relaxed);
        let mut saw_full = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_secs(2), handle.packets.recv()).await {
                Ok(Some(p)) => {
                    if VideoPacket::decode(&p).unwrap().full_frame {
                        saw_full = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_full, "refresh must force a full frame");
        handle.stop.store(true, Ordering::Relaxed);
    }
}
