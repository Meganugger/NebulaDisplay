//! Per-client adaptive quality controller.
//!
//! Signals (all *measured*, none guessed):
//! * **send backlog** — the writer couldn't drain a video frame within its
//!   pacing budget (primary congestion signal; reacts within frames).
//! * **RTT trend** — bufferbloat detection from Ping/Pong (a *sustained*
//!   rise of smoothed RTT well above the observed minimum means network
//!   queues are filling).
//! * **client decode queue** — a slow decoder needs fewer/lighter frames
//!   even on a perfect network.
//!
//! Design goals (learned from v0.2's oscillation problems):
//! * **Hysteresis everywhere.** A decrease requires *repeated* signals, and
//!   after acting the controller holds still for a cooldown so the effect of
//!   the change can actually be observed before reacting again.
//! * **FPS is sticky.** Frame rate only drops after bitrate has already hit
//!   the profile floor *and* congestion persists; it recovers in one step
//!   after a long clean period. Bitrate — not FPS — absorbs normal network
//!   variance, so pacing stays even and the encoder is never reconfigured
//!   for FPS flapping.
//! * **Increases are gentle** (≈8%/s multiplicative probe) so recovery does
//!   not immediately re-trigger congestion.

use ndsp_protocol::messages::{Profile, ViewerStats};
use std::time::{Duration, Instant};

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

/// Ignore further decrease requests for this long after acting, so the
/// effect of the previous cut is observable before cutting again.
const DECREASE_COOLDOWN: Duration = Duration::from_millis(1_500);
/// Minimum spacing between additive increases.
const INCREASE_INTERVAL: Duration = Duration::from_millis(1_000);
/// The link must be congestion-free for this long before probing upward.
const CLEAN_BEFORE_INCREASE: Duration = Duration::from_millis(2_000);
/// The link must be clean this long before a reduced FPS is restored.
const CLEAN_BEFORE_FPS_RESTORE: Duration = Duration::from_secs(8);
/// Consecutive high-RTT samples required to call it bufferbloat.
const RTT_STREAK_FOR_DECREASE: u32 = 4;
/// Consecutive deep-queue stats reports required to react.
const QUEUE_STREAK_FOR_DECREASE: u32 = 2;
/// Backlogged sends within one window required to react.
const BACKLOG_FOR_DECREASE: u32 = 3;
/// Decrease attempts while already at the bitrate floor before FPS drops.
const FLOOR_PRESSURE_FOR_FPS_DROP: u32 = 2;

pub struct AdaptiveController {
    env: ProfileEnvelope,
    bitrate_kbps: u32,
    fps: u32,
    rtt_min_ms: f32,
    rtt_smooth_ms: f32,
    rtt_high_streak: u32,
    queue_high_streak: u32,
    backlog_recent: u32,
    floor_pressure: u32,
    last_decrease: Instant,
    last_increase: Instant,
    clean_since: Instant,
}

impl AdaptiveController {
    pub fn new(profile: Profile) -> Self {
        let env = envelope(profile);
        let now = Instant::now();
        Self {
            env,
            bitrate_kbps: env.start_kbps,
            fps: env.max_fps,
            rtt_min_ms: f32::MAX,
            rtt_smooth_ms: 0.0,
            rtt_high_streak: 0,
            queue_high_streak: 0,
            backlog_recent: 0,
            floor_pressure: 0,
            // Allow an immediate first reaction; cooldowns apply *between*
            // actions, not before the first one.
            last_decrease: now - DECREASE_COOLDOWN,
            last_increase: now,
            clean_since: now,
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

    /// The writer failed to drain a video frame within its pacing budget.
    pub fn on_send_backlog(&mut self) {
        self.backlog_recent += 1;
        if self.backlog_recent >= BACKLOG_FOR_DECREASE {
            self.backlog_recent = 0;
            self.try_decrease("send backlog");
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
        let bloated = self.rtt_min_ms.is_finite()
            && self.rtt_smooth_ms > self.rtt_min_ms + 60.0
            && self.rtt_smooth_ms > self.rtt_min_ms * 2.0;
        if bloated {
            self.rtt_high_streak += 1;
            if self.rtt_high_streak >= RTT_STREAK_FOR_DECREASE {
                self.rtt_high_streak = 0;
                self.try_decrease("rtt bufferbloat");
            }
        } else {
            self.rtt_high_streak = 0;
        }
    }

    pub fn on_viewer_stats(&mut self, stats: &ViewerStats) {
        if stats.queue_depth > 3 {
            self.queue_high_streak += 1;
            if self.queue_high_streak >= QUEUE_STREAK_FOR_DECREASE {
                self.queue_high_streak = 0;
                self.try_decrease("client decode queue");
            }
        } else {
            self.queue_high_streak = 0;
        }
    }

    /// Call once per pacing tick (whether or not a frame was sent): performs
    /// the gentle upward probe when the link has been clean for a while.
    pub fn on_tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.clean_since) < CLEAN_BEFORE_INCREASE
            || now.duration_since(self.last_increase) < INCREASE_INTERVAL
        {
            return;
        }
        self.last_increase = now;
        self.backlog_recent = 0; // decay stale one-off backlog counts
        if self.bitrate_kbps < self.env.max_kbps {
            let step = (self.bitrate_kbps / 12).max(100); // ≈8%/s
            self.bitrate_kbps = (self.bitrate_kbps + step).min(self.env.max_kbps);
        } else if self.fps < self.env.max_fps
            && now.duration_since(self.clean_since) >= CLEAN_BEFORE_FPS_RESTORE
        {
            // Bitrate fully recovered and the link stayed clean for a long
            // time — restore full frame rate in a single step.
            self.fps = self.env.max_fps;
            self.floor_pressure = 0;
            tracing::debug!(fps = self.fps, "adaptive fps restored");
        }
    }

    fn try_decrease(&mut self, reason: &'static str) {
        let now = Instant::now();
        self.clean_since = now;
        if now.duration_since(self.last_decrease) < DECREASE_COOLDOWN {
            return;
        }
        self.last_decrease = now;
        if self.bitrate_kbps > self.env.min_kbps {
            self.bitrate_kbps = (((self.bitrate_kbps as f32) * 0.7) as u32).max(self.env.min_kbps);
            self.floor_pressure = 0;
        } else {
            // Bitrate floor reached — only now does frame rate take the hit,
            // and only after sustained pressure (big step, applied rarely).
            self.floor_pressure += 1;
            if self.floor_pressure >= FLOOR_PRESSURE_FOR_FPS_DROP {
                self.floor_pressure = 0;
                self.fps = (self.fps / 2).max(self.env.min_fps);
            }
        }
        tracing::debug!(
            reason,
            bitrate_kbps = self.bitrate_kbps,
            fps = self.fps,
            "adaptive decrease"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewind_cooldowns(c: &mut AdaptiveController) {
        c.last_decrease = Instant::now() - DECREASE_COOLDOWN;
    }

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
    fn decrease_cooldown_prevents_oscillation() {
        let mut c = AdaptiveController::new(Profile::Video);
        for _ in 0..3 {
            c.on_send_backlog();
        }
        let after_first = c.bitrate_kbps();
        // A flood of further signals within the cooldown must not cut again.
        for _ in 0..30 {
            c.on_send_backlog();
        }
        assert_eq!(c.bitrate_kbps(), after_first, "cooldown must hold the rate");
    }

    #[test]
    fn floor_shifts_pressure_to_fps_slowly() {
        let mut c = AdaptiveController::new(Profile::Office);
        // Drive to the floor (rewinding the cooldown to simulate time).
        for _ in 0..20 {
            rewind_cooldowns(&mut c);
            for _ in 0..3 {
                c.on_send_backlog();
            }
        }
        assert_eq!(c.bitrate_kbps(), envelope(Profile::Office).min_kbps);
        assert!(c.fps() < envelope(Profile::Office).max_fps);
        assert!(c.fps() >= envelope(Profile::Office).min_fps);
    }

    #[test]
    fn fps_does_not_drop_before_bitrate_floor() {
        let mut c = AdaptiveController::new(Profile::Video);
        rewind_cooldowns(&mut c);
        for _ in 0..3 {
            c.on_send_backlog();
        }
        assert_eq!(
            c.fps(),
            envelope(Profile::Video).max_fps,
            "fps must stay at max while bitrate can still absorb congestion"
        );
    }

    #[test]
    fn bufferbloat_needs_sustained_rtt_rise() {
        let mut c = AdaptiveController::new(Profile::Gaming);
        let start = c.bitrate_kbps();
        c.on_rtt_sample(5.0);
        // A couple of outliers must NOT trigger a cut...
        c.on_rtt_sample(200.0);
        c.on_rtt_sample(200.0);
        c.on_rtt_sample(5.0);
        c.on_rtt_sample(200.0);
        assert_eq!(c.bitrate_kbps(), start, "isolated spikes must be ignored");
        // ...but a sustained rise must.
        for _ in 0..8 {
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
        c.clean_since = Instant::now() - CLEAN_BEFORE_INCREASE;
        c.last_increase = Instant::now() - INCREASE_INTERVAL;
        c.on_tick();
        assert!(c.bitrate_kbps() > cut);
    }

    #[test]
    fn fps_restores_after_long_clean_period() {
        let mut c = AdaptiveController::new(Profile::Video);
        // Force fps down.
        for _ in 0..20 {
            rewind_cooldowns(&mut c);
            for _ in 0..3 {
                c.on_send_backlog();
            }
        }
        assert!(c.fps() < envelope(Profile::Video).max_fps);
        // Recover bitrate fully.
        for _ in 0..200 {
            c.clean_since = Instant::now() - CLEAN_BEFORE_FPS_RESTORE;
            c.last_increase = Instant::now() - INCREASE_INTERVAL;
            c.on_tick();
        }
        assert_eq!(c.bitrate_kbps(), envelope(Profile::Video).max_kbps);
        assert_eq!(c.fps(), envelope(Profile::Video).max_fps);
    }
}
