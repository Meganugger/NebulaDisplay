//! Audio playback for the desktop viewer (ROADMAP P2.15): Opus (or raw PCM)
//! frames from NDSP channel 3 → the default output device via cpal.
//!
//! Design: a lock-guarded sample queue between the network thread (decode +
//! push) and the audio callback (pull). The queue acts as the jitter buffer;
//! underruns play silence (never glitchy stale data) and a hard cap bounds
//! added latency by dropping the *oldest* samples when the network briefly
//! bursts. 48 kHz stereo end-to-end — matching the host's fixed format.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ndsp_protocol::media::{AudioCodec, AudioFrame};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Cap the queue at ~200 ms of interleaved stereo — bounds worst-case
/// audio latency after a network burst.
const QUEUE_CAP: usize = (SAMPLE_RATE as usize / 5) * CHANNELS;
/// Decode scratch: opus frames are ≤ 120 ms.
const MAX_FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * 120 / 1000) * CHANNELS;

pub struct AudioPlayer {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<i16>>>,
    decoder: opus::Decoder,
    scratch: Vec<i16>,
}

impl AudioPlayer {
    /// Open the default output device at 48 kHz. Errors (no device, no
    /// 48 kHz support) are reported once; the viewer keeps running silent.
    pub fn new() -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
        let config = pick_config(&device)?;
        let out_channels = config.channels as usize;
        anyhow::ensure!(
            (1..=2).contains(&out_channels),
            "unsupported output channel count {out_channels}"
        );

        let queue: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cb_queue = queue.clone();
        let stream = device.build_output_stream(
            &config,
            move |out: &mut [f32], _| {
                let mut q = cb_queue.lock().unwrap();
                if out_channels == 2 {
                    for s in out.iter_mut() {
                        *s = q.pop_front().map(to_f32).unwrap_or(0.0);
                    }
                } else {
                    // Mono device: average the stereo pair.
                    for s in out.iter_mut() {
                        let l = q.pop_front().map(to_f32).unwrap_or(0.0);
                        let r = q.pop_front().map(to_f32).unwrap_or(0.0);
                        *s = (l + r) * 0.5;
                    }
                }
            },
            move |e| tracing::warn!("audio stream error: {e}"),
            None,
        )?;
        stream.play()?;
        tracing::info!("audio output open (48 kHz, {out_channels} ch)");
        Ok(Self {
            _stream: stream,
            queue,
            decoder: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?,
            scratch: vec![0i16; MAX_FRAME_SAMPLES],
        })
    }

    /// Decode + enqueue one NDSP audio frame.
    pub fn push(&mut self, frame: &AudioFrame) {
        if frame.sample_rate != SAMPLE_RATE || frame.channels != 2 {
            tracing::debug!(
                rate = frame.sample_rate,
                ch = frame.channels,
                "dropping audio frame with unexpected format"
            );
            return;
        }
        let samples: &[i16] = match frame.codec {
            AudioCodec::Opus => match self
                .decoder
                .decode(&frame.payload, &mut self.scratch, false)
            {
                Ok(per_ch) => &self.scratch[..per_ch * CHANNELS],
                Err(e) => {
                    tracing::debug!("opus decode failed: {e}");
                    return;
                }
            },
            AudioCodec::PcmS16le => {
                if !frame.payload.len().is_multiple_of(2) {
                    return;
                }
                self.scratch.clear();
                self.scratch.extend(
                    frame
                        .payload
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]])),
                );
                &self.scratch
            }
        };
        let mut q = self.queue.lock().unwrap();
        q.extend(samples.iter().copied());
        // Bound latency: drop the oldest samples past the cap (whole pairs).
        while q.len() > QUEUE_CAP {
            q.pop_front();
            q.pop_front();
        }
    }
}

#[inline]
fn to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

/// Find an f32 output config at 48 kHz (stereo preferred, mono accepted).
fn pick_config(device: &cpal::Device) -> anyhow::Result<cpal::StreamConfig> {
    let mut best: Option<cpal::SupportedStreamConfig> = None;
    for cfg in device.supported_output_configs()? {
        if cfg.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }
        if cfg.min_sample_rate().0 > SAMPLE_RATE || cfg.max_sample_rate().0 < SAMPLE_RATE {
            continue;
        }
        if !(1..=2).contains(&cfg.channels()) {
            continue;
        }
        let cand = cfg.with_sample_rate(cpal::SampleRate(SAMPLE_RATE));
        let better = match &best {
            None => true,
            Some(b) => cand.channels() > b.channels(), // prefer stereo
        };
        if better {
            best = Some(cand);
        }
    }
    best.map(|c| c.config())
        .ok_or_else(|| anyhow::anyhow!("output device has no 48 kHz f32 config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode path without a device: feed a real Opus packet through the
    /// same decode/mix logic `push` uses.
    #[test]
    fn opus_roundtrip_decodes_to_expected_length() {
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .unwrap();
        let pcm = vec![0i16; 960 * CHANNELS]; // one 20 ms block
        let packet = enc.encode_vec(&pcm, 4000).unwrap();

        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0i16; MAX_FRAME_SAMPLES];
        let per_ch = dec.decode(&packet, &mut out, false).unwrap();
        assert_eq!(per_ch, 960);
    }

    #[test]
    fn pcm_frames_parse_little_endian() {
        let payload: Vec<u8> = [1i16, -2, 300, -400]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let parsed: Vec<i16> = payload
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(parsed, vec![1, -2, 300, -400]);
    }
}
