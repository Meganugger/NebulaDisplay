//! Adaptive quality controller.
//!
//! Inputs (per client):
//! * send-queue pressure (frames dropped because the socket can't keep up),
//! * client feedback ([`nebula_proto::ClientFeedback`]: decode time, client
//!   drops, jitter-buffer depth),
//! * measured RTT from ping/pong.
//!
//! Output: [`QualitySettings`] — JPEG quality and FPS cap — adjusted with an
//! AIMD-style loop (multiplicative decrease on congestion, slow additive
//! recovery), which is well-behaved on shared Wi-Fi links.

use std::time::{Duration, Instant};

use nebula_proto::{ClientFeedback, Profile};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualitySettings {
    /// JPEG quality (or QP-mapped equivalent for future codecs), 1..=100.
    pub quality: u8,
    /// Target frames per second.
    pub fps: u32,
}

/// Per-profile bounds for the controller.
#[derive(Debug, Clone, Copy)]
pub struct ProfileBounds {
    pub min_quality: u8,
    pub max_quality: u8,
    pub start_quality: u8,
    pub min_fps: u32,
    pub max_fps: u32,
    pub start_fps: u32,
}

pub fn bounds_for(profile: Profile) -> ProfileBounds {
    match profile {
        Profile::Office => ProfileBounds {
            min_quality: 40,
            max_quality: 85,
            start_quality: 70,
            min_fps: 5,
            max_fps: 30,
            start_fps: 20,
        },
        Profile::Video => ProfileBounds {
            min_quality: 45,
            max_quality: 90,
            start_quality: 75,
            min_fps: 24,
            max_fps: 60,
            start_fps: 30,
        },
        Profile::Drawing => ProfileBounds {
            min_quality: 55,
            max_quality: 95,
            start_quality: 85,
            min_fps: 30,
            max_fps: 90,
            start_fps: 60,
        },
        Profile::Gaming => ProfileBounds {
            min_quality: 30,
            max_quality: 85,
            start_quality: 60,
            min_fps: 30,
            max_fps: 120,
            start_fps: 60,
        },
        Profile::Balanced => ProfileBounds {
            min_quality: 40,
            max_quality: 90,
            start_quality: 75,
            min_fps: 10,
            max_fps: 60,
            start_fps: 30,
        },
    }
}

pub struct AdaptiveController {
    bounds: ProfileBounds,
    quality: f32,
    fps: f32,
    last_recovery: Instant,
    /// Congestion events observed in the current observation window.
    congestion_events: u32,
    window_start: Instant,
    rtt_ms: f32,
    baseline_rtt_ms: f32,
}

/// How often quality creeps back up when the link is healthy.
const RECOVERY_INTERVAL: Duration = Duration::from_millis(800);
/// Multiplicative decrease factor on congestion.
const DECREASE: f32 = 0.75;
/// Additive quality recovery per interval.
const QUALITY_STEP: f32 = 3.0;
const FPS_STEP: f32 = 4.0;

impl AdaptiveController {
    pub fn new(profile: Profile) -> Self {
        let bounds = bounds_for(profile);
        Self {
            bounds,
            quality: bounds.start_quality as f32,
            fps: bounds.start_fps as f32,
            last_recovery: Instant::now(),
            congestion_events: 0,
            window_start: Instant::now(),
            rtt_ms: 0.0,
            baseline_rtt_ms: f32::MAX,
        }
    }

    pub fn set_profile(&mut self, profile: Profile) {
        let b = bounds_for(profile);
        self.bounds = b;
        self.quality = self
            .quality
            .clamp(b.min_quality as f32, b.max_quality as f32);
        self.fps = self.fps.clamp(b.min_fps as f32, b.max_fps as f32);
    }

    /// The socket send queue was full and a frame was dropped host-side.
    pub fn on_send_drop(&mut self) {
        self.congest();
    }

    /// Periodic client feedback arrived.
    pub fn on_feedback(&mut self, fb: &ClientFeedback) {
        // A deep client-side queue or client drops mean we outrun the
        // decoder/network even though our socket kept up.
        if fb.dropped_frames > 0 || fb.queue_depth > 3 {
            self.congest();
        }
        // Slow decoders (weak tablets) get a lower FPS cap rather than
        // quality reduction: decode cost scales with pixel rate.
        if fb.decode_ms > 0.0 {
            let sustainable = (1000.0 / fb.decode_ms) * 0.8;
            if sustainable < self.fps {
                self.fps = sustainable.max(self.bounds.min_fps as f32);
            }
        }
    }

    /// A ping/pong RTT sample arrived.
    pub fn on_rtt(&mut self, rtt_ms: f32) {
        self.rtt_ms = rtt_ms;
        self.baseline_rtt_ms = self.baseline_rtt_ms.min(rtt_ms.max(0.1));
        // Bufferbloat detection: RTT ballooning past 3× baseline + 40ms means
        // queues are filling somewhere on the path.
        if rtt_ms > self.baseline_rtt_ms * 3.0 + 40.0 {
            self.congest();
        }
    }

    fn congest(&mut self) {
        // At most one multiplicative decrease per 250ms window so a burst of
        // signals doesn't crater quality.
        if self.window_start.elapsed() > Duration::from_millis(250) {
            self.window_start = Instant::now();
            self.congestion_events = 0;
        }
        self.congestion_events += 1;
        if self.congestion_events == 1 {
            self.quality = (self.quality * DECREASE).max(self.bounds.min_quality as f32);
            self.fps = (self.fps * DECREASE).max(self.bounds.min_fps as f32);
        }
    }

    /// Called once per pipeline tick; applies slow recovery when healthy.
    pub fn tick(&mut self) -> QualitySettings {
        if self.last_recovery.elapsed() >= RECOVERY_INTERVAL {
            self.last_recovery = Instant::now();
            let recently_congested =
                self.window_start.elapsed() < Duration::from_secs(2) && self.congestion_events > 0;
            if !recently_congested {
                self.quality = (self.quality + QUALITY_STEP).min(self.bounds.max_quality as f32);
                self.fps = (self.fps + FPS_STEP).min(self.bounds.max_fps as f32);
            }
        }
        self.current()
    }

    pub fn current(&self) -> QualitySettings {
        QualitySettings {
            quality: self.quality.round() as u8,
            fps: self.fps.round().max(1.0) as u32,
        }
    }

    pub fn rtt_ms(&self) -> f32 {
        self.rtt_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_profile_defaults() {
        let c = AdaptiveController::new(Profile::Balanced);
        let q = c.current();
        assert_eq!(q.quality, 75);
        assert_eq!(q.fps, 30);
    }

    #[test]
    fn congestion_reduces_quality_and_fps() {
        let mut c = AdaptiveController::new(Profile::Balanced);
        let before = c.current();
        c.on_send_drop();
        let after = c.current();
        assert!(after.quality < before.quality);
        assert!(after.fps < before.fps);
    }

    #[test]
    fn repeated_congestion_hits_floor_not_zero() {
        let mut c = AdaptiveController::new(Profile::Gaming);
        for _ in 0..100 {
            // Space windows out artificially by resetting the window.
            c.window_start = Instant::now() - Duration::from_millis(300);
            c.on_send_drop();
        }
        let q = c.current();
        let b = bounds_for(Profile::Gaming);
        assert_eq!(q.quality, b.min_quality);
        assert_eq!(q.fps, b.min_fps);
    }

    #[test]
    fn recovers_when_healthy() {
        let mut c = AdaptiveController::new(Profile::Balanced);
        c.window_start = Instant::now() - Duration::from_millis(300);
        c.on_send_drop();
        let degraded = c.current();
        // Simulate time passing with no congestion.
        c.window_start = Instant::now() - Duration::from_secs(10);
        c.last_recovery = Instant::now() - Duration::from_secs(10);
        let recovered = c.tick();
        assert!(recovered.quality > degraded.quality);
    }

    #[test]
    fn slow_decoder_lowers_fps() {
        let mut c = AdaptiveController::new(Profile::Video);
        c.on_feedback(&ClientFeedback {
            last_presented_frame: 10,
            dropped_frames: 0,
            decode_ms: 50.0, // can only decode ~20fps
            queue_depth: 1,
        });
        assert!(c.current().fps <= 24.max(bounds_for(Profile::Video).min_fps));
    }

    #[test]
    fn rtt_inflation_triggers_backoff() {
        let mut c = AdaptiveController::new(Profile::Balanced);
        c.on_rtt(5.0); // baseline
        let before = c.current();
        c.on_rtt(200.0); // bufferbloat
        assert!(c.current().quality < before.quality);
    }

    #[test]
    fn profile_switch_clamps() {
        let mut c = AdaptiveController::new(Profile::Gaming);
        c.set_profile(Profile::Office);
        let q = c.current();
        let b = bounds_for(Profile::Office);
        assert!(q.fps <= b.max_fps && q.fps >= b.min_fps);
        assert!(q.quality <= b.max_quality && q.quality >= b.min_quality);
    }
}
