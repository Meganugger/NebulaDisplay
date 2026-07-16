//! Synthetic audio source: a quiet dual-tone (so stereo separation is
//! audible/verifiable) used on non-Windows hosts, in CI, and under
//! `--test-pattern`. Pacing is owned by the audio loop — this just
//! synthesizes the next block.

use super::{AudioSource, BLOCK_LEN, CHANNELS, SAMPLE_RATE};

pub struct TestToneSource {
    phase_l: f32,
    phase_r: f32,
}

impl TestToneSource {
    pub fn new() -> Self {
        Self {
            phase_l: 0.0,
            phase_r: 0.0,
        }
    }
}

const FREQ_L: f32 = 440.0; // A4 on the left
const FREQ_R: f32 = 554.37; // C#5 on the right
const AMPLITUDE: f32 = 0.15; // quiet — this can reach real speakers

impl AudioSource for TestToneSource {
    fn name(&self) -> &'static str {
        "test-tone"
    }

    fn fill(&mut self, out: &mut [i16; BLOCK_LEN]) -> anyhow::Result<bool> {
        let step_l = std::f32::consts::TAU * FREQ_L / SAMPLE_RATE as f32;
        let step_r = std::f32::consts::TAU * FREQ_R / SAMPLE_RATE as f32;
        for frame in out.chunks_exact_mut(CHANNELS as usize) {
            frame[0] = super::f32_to_i16(self.phase_l.sin() * AMPLITUDE);
            frame[1] = super::f32_to_i16(self.phase_r.sin() * AMPLITUDE);
            self.phase_l = (self.phase_l + step_l) % std::f32::consts::TAU;
            self.phase_r = (self.phase_r + step_r) % std::f32::consts::TAU;
        }
        Ok(true)
    }
}
