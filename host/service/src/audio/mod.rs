//! Host audio pipeline (ROADMAP P2.8).
//!
//! * Windows: WASAPI **loopback** capture of the default render device —
//!   whatever the PC plays, the viewer hears.
//! * Everywhere else / tests: a synthetic tone source so the full pipeline
//!   (capture → Opus → encrypt → channel 3 → decode → playback) is
//!   exercisable in CI.
//!
//! Design mirrors the video path: one global capture/encode loop publishes
//! [`AudioBlock`]s on a broadcast channel; each session forwards them to its
//! client in the client's preferred payload format (Opus, or raw PCM for
//! web viewers on insecure origins that have no WebCodecs Opus decoder).
//!
//! Privacy: the loop only *runs* while at least one connected client has
//! audio enabled **and** is permitted by the panel; with zero listeners the
//! capture device is released entirely (`state.audio_listeners`).

mod test_tone;
#[cfg(windows)]
mod windows_wasapi;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::state::AppState;
use crate::util::now_us;

/// Fixed output format: 48 kHz stereo s16 in 10 ms blocks. Opus only
/// supports 8/12/16/24/48 kHz — sources at other device rates resample.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const BLOCK_MS: u32 = 10;
/// Samples per channel per block.
pub const SAMPLES_PER_BLOCK: usize = (SAMPLE_RATE as usize / 1000) * BLOCK_MS as usize;
/// Interleaved i16 count per block.
pub const BLOCK_LEN: usize = SAMPLES_PER_BLOCK * CHANNELS as usize;

/// Opus target bitrate — transparent for desktop/system audio.
const OPUS_BITRATE: i32 = 128_000;

/// One captured+encoded 10 ms block, shared zero-copy across sessions.
pub struct AudioBlock {
    pub seq: u32,
    pub timestamp_us: u64,
    /// Interleaved 48 kHz stereo s16 ([`BLOCK_LEN`] samples).
    pub pcm: Vec<i16>,
    /// The same content, Opus-encoded.
    pub opus: Vec<u8>,
}

/// A source of interleaved 48 kHz stereo s16 audio. `fill` is called on a
/// 10 ms cadence from a dedicated blocking thread; it may block briefly.
/// Returning `Ok(false)` means "no data right now" — the loop substitutes
/// silence so the stream cadence never stalls.
pub trait AudioSource: Send {
    fn name(&self) -> &'static str;
    fn fill(&mut self, out: &mut [i16; BLOCK_LEN]) -> anyhow::Result<bool>;
}

/// Best available source for this platform.
pub fn create_source(force_test_tone: bool) -> anyhow::Result<Box<dyn AudioSource>> {
    #[cfg(windows)]
    {
        if !force_test_tone {
            match windows_wasapi::WasapiLoopbackSource::new() {
                Ok(src) => {
                    info!("audio: WASAPI loopback capture active");
                    return Ok(Box::new(src));
                }
                Err(e) => {
                    warn!("audio: WASAPI loopback unavailable ({e:#}); using test tone");
                }
            }
        }
    }
    let _ = force_test_tone;
    info!("audio: synthetic tone source");
    Ok(Box::new(test_tone::TestToneSource::new()))
}

/// Global audio loop: capture → Opus → broadcast. The source (and the OS
/// capture device with it) exists only while someone is listening.
///
/// `force_test_tone` pins the synthetic source (tests / `--test-pattern`).
pub async fn run_audio_loop(state: Arc<AppState>, force_test_tone: bool) {
    let state2 = state.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let block = Duration::from_millis(BLOCK_MS as u64);
        let mut source: Option<Box<dyn AudioSource>> = None;
        let mut encoder: Option<opus::Encoder> = None;
        let mut seq: u32 = 0;
        let mut next_deadline = Instant::now();
        loop {
            if state2.is_shutdown() {
                info!("audio loop stopping (shutdown)");
                break;
            }
            if state2.audio_listener_count() == 0 {
                if source.is_some() {
                    info!("audio: last listener left; releasing capture device");
                    source = None;
                    encoder = None;
                }
                std::thread::sleep(Duration::from_millis(100));
                next_deadline = Instant::now();
                continue;
            }
            if source.is_none() {
                match create_source(force_test_tone) {
                    Ok(s) => source = Some(s),
                    Err(e) => {
                        warn!("audio source init failed: {e:#}; retrying in 2s");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
                next_deadline = Instant::now();
            }
            if encoder.is_none() {
                match opus::Encoder::new(
                    SAMPLE_RATE,
                    opus::Channels::Stereo,
                    opus::Application::LowDelay,
                )
                .and_then(|mut e| {
                    e.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))?;
                    Ok(e)
                }) {
                    Ok(e) => encoder = Some(e),
                    Err(e) => {
                        warn!("opus encoder init failed: {e}; audio disabled");
                        break;
                    }
                }
            }

            let mut pcm = [0i16; BLOCK_LEN];
            match source
                .as_mut()
                .expect("source initialized above")
                .fill(&mut pcm)
            {
                Ok(_had_data) => {}
                Err(e) => {
                    warn!("audio capture error: {e:#}; reinitializing source");
                    source = None;
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }

            let mut opus_buf = vec![0u8; 1500];
            let opus_payload = match encoder
                .as_mut()
                .expect("encoder initialized above")
                .encode(&pcm, &mut opus_buf)
            {
                Ok(n) => {
                    opus_buf.truncate(n);
                    opus_buf
                }
                Err(e) => {
                    warn!("opus encode failed: {e}; dropping block");
                    Vec::new()
                }
            };
            if !opus_payload.is_empty() {
                seq = seq.wrapping_add(1);
                let _ = state2.audio_tx.send(Arc::new(AudioBlock {
                    seq,
                    timestamp_us: now_us(),
                    pcm: pcm.to_vec(),
                    opus: opus_payload,
                }));
            }

            // Fixed 10 ms cadence, immune to per-iteration jitter.
            next_deadline += block;
            let now = Instant::now();
            if let Some(rem) = next_deadline.checked_duration_since(now) {
                std::thread::sleep(rem);
            }
            if now.saturating_duration_since(next_deadline) > block * 10 {
                next_deadline = now; // fell far behind (suspend) — resync
            }
        }
    });
    let _ = handle.await;
}

/// Linear resampler: arbitrary input rate → 48 kHz, per channel, streaming.
/// Quality is plenty for system audio; it exists because WASAPI mix formats
/// are commonly 44.1 kHz while Opus requires one of its native rates.
pub struct LinearResampler {
    in_rate: u32,
    channels: usize,
    /// Fractional read position within the input stream, in input frames.
    pos: f64,
    /// Carry of the last input frame for interpolation across calls.
    last_frame: Vec<f32>,
    have_last: bool,
}

impl LinearResampler {
    pub fn new(in_rate: u32, channels: usize) -> Self {
        Self {
            in_rate,
            channels,
            pos: 0.0,
            last_frame: vec![0.0; channels],
            have_last: false,
        }
    }

    /// Push interleaved f32 input frames; append 48 kHz interleaved f32
    /// output frames to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.in_rate == SAMPLE_RATE {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        let in_frames = input.len() / ch;
        if in_frames == 0 {
            return;
        }
        let step = self.in_rate as f64 / SAMPLE_RATE as f64;
        // Virtual input timeline: frame -1 is `last_frame` (previous call).
        let frame_at = |idx: isize, c: usize| -> f32 {
            if idx < 0 {
                if self.have_last {
                    self.last_frame[c]
                } else {
                    input[c]
                }
            } else {
                input[(idx as usize).min(in_frames - 1) * ch + c]
            }
        };
        // self.pos is relative to the first frame of *this* input slice,
        // and may start negative (between last_frame and input[0]).
        while self.pos < (in_frames - 1) as f64 {
            let base = self.pos.floor();
            let frac = (self.pos - base) as f32;
            let i0 = base as isize;
            for c in 0..ch {
                let a = frame_at(i0, c);
                let b = frame_at(i0 + 1, c);
                out.push(a + (b - a) * frac);
            }
            self.pos += step;
        }
        // Rebase position for the next call and stash the final frame.
        self.pos -= in_frames as f64;
        for c in 0..ch {
            self.last_frame[c] = input[(in_frames - 1) * ch + c];
        }
        self.have_last = true;
    }
}

/// Downmix / upmix arbitrary channel counts to stereo, f32 → f32.
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => {
            for &s in input {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(input),
        n => {
            // Average everything beyond FL/FR into both (simple, artifact-free).
            for frame in input.chunks_exact(n) {
                let fl = frame[0];
                let fr = frame[1];
                let rest: f32 = frame[2..].iter().sum::<f32>() / (n as f32 - 2.0).max(1.0) * 0.5;
                out.push((fl + rest).clamp(-1.0, 1.0));
                out.push((fr + rest).clamp(-1.0, 1.0));
            }
        }
    }
}

pub fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_identity_at_48k() {
        let mut rs = LinearResampler::new(48_000, 2);
        let input: Vec<f32> = (0..96).map(|i| i as f32).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn resampler_441_to_48_produces_expected_rate() {
        let mut rs = LinearResampler::new(44_100, 2);
        let mut out = Vec::new();
        // 1 second of 44.1 kHz stereo in 10 ms slices.
        for _ in 0..100 {
            let input = vec![0.5f32; 441 * 2];
            rs.process(&input, &mut out);
        }
        let frames = out.len() / 2;
        assert!(
            (47_800..=48_100).contains(&frames),
            "expected ≈48000 output frames, got {frames}"
        );
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-4));
    }

    #[test]
    fn resampler_preserves_ramp_shape() {
        let mut rs = LinearResampler::new(24_000, 1);
        let input: Vec<f32> = (0..240).map(|i| i as f32 / 240.0).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        // Upsampled ramp must stay monotonic.
        assert!(out.windows(2).all(|w| w[1] >= w[0] - 1e-6));
        assert!(
            (out.len() as i64 - 478).abs() < 6,
            "≈2x frames, got {}",
            out.len()
        );
    }

    #[test]
    fn stereo_downmix_shapes() {
        let mut out = Vec::new();
        to_stereo(&[0.5], 1, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        out.clear();
        to_stereo(&[0.1, 0.2], 2, &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
        out.clear();
        to_stereo(&[0.1, 0.2, 0.4, 0.4, 0.4, 0.4], 6, &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn tone_source_fills_blocks() {
        let mut src = test_tone::TestToneSource::new();
        let mut block = [0i16; BLOCK_LEN];
        assert!(src.fill(&mut block).unwrap());
        assert!(block.iter().any(|&s| s != 0), "tone must be non-silent");
    }
}
