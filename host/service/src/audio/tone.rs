//! Synthetic audio source: a quiet 440 Hz sine, paced to real time.
//!
//! Serves the same purpose as the test-pattern video source — it makes the
//! whole audio path (Opus encode, framing, encryption, opt-in gating, viewer
//! playback) testable without a Windows audio stack.

use std::time::{Duration, Instant};

use super::{AudioSource, CHANNELS, SAMPLE_RATE};

pub struct ToneSource {
    phase: f32,
    started: Instant,
    produced_samples: u64,
}

impl ToneSource {
    pub fn new() -> Self {
        Self {
            phase: 0.0,
            started: Instant::now(),
            produced_samples: 0,
        }
    }
}

const CHUNK_SAMPLES: usize = 480; // 10 ms

impl AudioSource for ToneSource {
    fn next_chunk(&mut self) -> anyhow::Result<Vec<f32>> {
        // Pace to real time: never run ahead of the wall clock.
        let target = Duration::from_micros(self.produced_samples * 1_000_000 / SAMPLE_RATE as u64);
        let elapsed = self.started.elapsed();
        if target > elapsed {
            std::thread::sleep(target - elapsed);
        }
        let step = 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
        let mut out = Vec::with_capacity(CHUNK_SAMPLES * CHANNELS as usize);
        for _ in 0..CHUNK_SAMPLES {
            let s = self.phase.sin() * 0.1; // -20 dBFS, easy on ears
            self.phase = (self.phase + step) % std::f32::consts::TAU;
            for _ in 0..CHANNELS {
                out.push(s);
            }
        }
        self.produced_samples += CHUNK_SAMPLES as u64;
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "test-tone"
    }
}
