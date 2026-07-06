//! Per-client adaptive quality controller (AIMD).
//!
//! Signals (all *measured*, none guessed):
//! * **send backlog** — frames we had to skip because the socket was busy
//!   (primary congestion signal; reacts within one frame).
//! * **RTT trend** — bufferbloat detection from Ping/Pong (rising smoothed
//!   RTT well above the observed minimum means queues are filling).
//! * **client decode queue** — a slow decoder needs fewer/lighter frames
//!   even on a perfect network.
//!
//! Response: multiplicative decrease on congestion, slow additive increase
//! when clean. Bitrate first, then FPS, per the active profile's envelope.

use ndsp_protocol::messages::{Profile, ViewerStats};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct ProfileEnvelope {
    pub min_kbps: u32,
    pub max_kbps: u32,
    pub start_kbps: u32,
    pub min_fps: u32,
    pub max_fps: u32,
}

pub fn envelope(profile: Profile) -> ProfileEnvelope {
    match profile {
        Profile::Office => ProfileEnvelope {
            min_kbps: 500,
            max_kbps: 6_000,
            start_kbps: 2_500,
            min_fps: 10,
            max_fps: 30,
        },
        Profile::Video => ProfileEnvelope {
            min_kbps: 2_000,
            max_kbps: 20_000,
            start_kbps: 8_000,
            min_fps: 24,
            max_fps: 60,
        },
        Profile::Drawing => ProfileEnvelope {
            min_kbps: 1_000,
            max_kbps: 10_000,
            start_kbps: 4_000,
            min_fps: 30,
            max_fps: 60,
        },
        Profile::Gaming => ProfileEnvelope {
            min_kbps: 3_000,
            max_kbps: 30_000,
            start_kbps: 10_000,
            min_fps: 30,
            max_fps: 60,
        },
    }
}

pub struct AdaptiveController {
    env: ProfileEnvelope,
    bitrate_kbps: u32,
    fps: u32,
    rtt_min_ms: f32,
    rtt_smooth_ms: f32,
    last_increase: Instant,
    skipped_recent: u32,
}

impl AdaptiveController {
    pub fn new(profile: Profile) -> Self {
        let env = envelope(profile);
        Self {
            env,
            bitrate_kbps: env.start_kbps,
            fps: env.max_fps,
            rtt_min_ms: f32::MAX,
            rtt_smooth_ms: 0.0,
            last_increase: Instant::now(),
            skipped_recent: 0,
        }
    }

    pub fn set_profile(&mut self, profile: Profile) {
        self.env = envelope(profile);
        self.bitrate_kbps = self
            .bitrate_kbps
            .clamp(self.env.min_kbps, self.env.max_kbps);
        self.fps = self.fps.clamp(self.env.min_fps, self.env.max_fps);
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// A frame had to be skipped because the transport was still busy.
    pub fn on_send_backlog(&mut self) {
        self.skipped_recent += 1;
        if self.skipped_recent >= 3 {
            self.decrease();
            self.skipped_recent = 0;
        }
    }

    pub fn on_rtt_sample(&mut self, rtt_ms: f32) {
        if rtt_ms <= 0.0 || !rtt_ms.is_finite() {
            return;
        }
        self.rtt_min_ms = self.rtt_min_ms.min(rtt_ms);
        self.rtt_smooth_ms = if self.rtt_smooth_ms == 0.0 {
            rtt_ms
        } else {
            self.rtt_smooth_ms * 0.8 + rtt_ms * 0.2
        };
        // Bufferbloat: smoothed RTT > min + 60ms and > 2x min.
        if self.rtt_min_ms.is_finite()
            && self.rtt_smooth_ms > self.rtt_min_ms + 60.0
            && self.rtt_smooth_ms > self.rtt_min_ms * 2.0
        {
            self.decrease();
            // Reset so we don't hammer decrease every sample.
            self.rtt_smooth_ms = self.rtt_min_ms;
        }
    }

    pub fn on_viewer_stats(&mut self, stats: &ViewerStats) {
        if stats.queue_depth > 4 {
            self.decrease();
        }
    }

    /// Call once per pacing tick; performs the additive increase when the
    /// link has been clean for a while.
    pub fn maybe_increase(&mut self) {
        if self.last_increase.elapsed().as_millis() < 1_000 {
            return;
        }
        self.last_increase = Instant::now();
        if self.fps < self.env.max_fps && self.bitrate_kbps >= self.env.max_kbps / 2 {
            self.fps = (self.fps + 5).min(self.env.max_fps);
        }
        let step = (self.env.max_kbps / 20).max(100);
        self.bitrate_kbps = (self.bitrate_kbps + step).min(self.env.max_kbps);
    }

    fn decrease(&mut self) {
        let new_bitrate = ((self.bitrate_kbps as f32) * 0.7) as u32;
        if new_bitrate >= self.env.min_kbps {
            self.bitrate_kbps = new_bitrate;
        } else {
            self.bitrate_kbps = self.env.min_kbps;
            // Bitrate floor reached — shed frames instead.
            self.fps = ((self.fps as f32 * 0.75) as u32).max(self.env.min_fps);
        }
        self.last_increase = Instant::now(); // hold increases after a cut
        tracing::debug!(
            bitrate_kbps = self.bitrate_kbps,
            fps = self.fps,
            "adaptive decrease"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlog_cuts_bitrate() {
        let mut c = AdaptiveController::new(Profile::Video);
        let start = c.bitrate_kbps();
        for _ in 0..3 {
            c.on_send_backlog();
        }
        assert!(c.bitrate_kbps() < start);
    }

    #[test]
    fn floor_shifts_pressure_to_fps() {
        let mut c = AdaptiveController::new(Profile::Office);
        for _ in 0..60 {
            c.on_send_backlog();
        }
        assert_eq!(c.bitrate_kbps(), envelope(Profile::Office).min_kbps);
        assert!(c.fps() < envelope(Profile::Office).max_fps);
        assert!(c.fps() >= envelope(Profile::Office).min_fps);
    }

    #[test]
    fn bufferbloat_detected() {
        let mut c = AdaptiveController::new(Profile::Gaming);
        let start = c.bitrate_kbps();
        c.on_rtt_sample(5.0);
        for _ in 0..30 {
            c.on_rtt_sample(200.0);
        }
        assert!(c.bitrate_kbps() < start);
    }

    #[test]
    fn clean_link_recovers() {
        let mut c = AdaptiveController::new(Profile::Video);
        for _ in 0..3 {
            c.on_send_backlog();
        }
        let cut = c.bitrate_kbps();
        // Simulate the passage of clean time.
        c.last_increase = Instant::now() - std::time::Duration::from_secs(2);
        c.maybe_increase();
        assert!(c.bitrate_kbps() > cut);
    }
}
