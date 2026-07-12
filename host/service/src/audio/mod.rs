//! Host audio pipeline: capture → Opus → encrypted channel 3.
//!
//! **Off by default** (`audio = false` in config). Even when enabled, audio
//! is *opt-in per session*: nothing is streamed to a viewer until it sends
//! `SetAudio { enabled: true }`, and the panel shows a live 🔊 indicator for
//! every session receiving audio.
//!
//! Pipeline shape mirrors video: one capture/encode producer publishes the
//! latest packet into a `watch` channel and every opted-in session forwards
//! it. A slow client therefore skips packets instead of building a backlog —
//! Opus packet-loss concealment turns a skipped 20 ms packet into a barely
//! audible artifact, whereas queueing would add permanent latency.
//!
//! * Windows: WASAPI shared-mode **loopback** of the default render device
//!   (what the speakers are playing), converted to 48 kHz stereo f32.
//! * Other hosts (and e2e tests): a synthetic test tone.
//!
//! Encoding runs only while at least one session is subscribed
//! ([`crate::state::AppState::audio_listeners`]) — an idle host pays zero
//! Opus cost. Capture keeps draining so no stale buffers burst out when a
//! listener appears.

// Compiled whenever targeting Windows (not just with the `audio` feature) so
// the cross-target CI check covers it; it feeds any [`PcmSink`].
#[cfg(windows)]
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
mod windows_wasapi;

use std::sync::Arc;

use crate::state::AppState;

/// Everything is fixed to Opus's native fullband rate; sources resample.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 20 ms packets — Opus's sweet spot for interactive streaming.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * 20;
/// Encoder bitrate. 96 kb/s stereo music-mode Opus is transparent for
/// desktop-audio purposes and negligible next to the video bitrate.
pub const BITRATE_BPS: i32 = 96_000;

/// Where capture sources deliver interleaved 48 kHz stereo f32 samples.
pub trait PcmSink {
    /// True while at least one session wants audio — sources may downshift
    /// to a cheap drain-only mode otherwise.
    fn has_listeners(&self) -> bool;
    fn push(&mut self, interleaved: &[f32]);
}

/// Spawn the capture+encode thread when audio is enabled in config.
/// Returns true when a pipeline was started (i.e. audio is *available*).
pub fn spawn_if_enabled(state: Arc<AppState>) -> bool {
    spawn_pipeline(state, false)
}

/// Like [`spawn_if_enabled`] but always uses the synthetic test-tone source
/// — for embedded/e2e hosts, which must not touch real audio devices (CI
/// runners have none).
pub fn spawn_test_tone_if_enabled(state: Arc<AppState>) -> bool {
    spawn_pipeline(state, true)
}

fn spawn_pipeline(state: Arc<AppState>, force_test_tone: bool) -> bool {
    if !state.cfg.file.audio {
        return false;
    }
    #[cfg(not(feature = "audio"))]
    {
        let _ = force_test_tone;
        tracing::warn!("audio requested in config but this build lacks the `audio` feature");
        false
    }
    #[cfg(feature = "audio")]
    {
        std::thread::Builder::new()
            .name("ndsp-audio".into())
            .spawn(move || {
                let encoder = match OpusPublisher::new(state.clone()) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!("opus encoder init failed: {e:#}");
                        return;
                    }
                };
                let result = if force_test_tone {
                    test_tone(&state, encoder)
                } else {
                    platform_source(&state, encoder)
                };
                if let Err(e) = result {
                    tracing::error!("audio pipeline stopped: {e:#}");
                }
            })
            .expect("spawn audio thread");
        true
    }
}

#[cfg(all(feature = "audio", windows))]
fn platform_source(state: &Arc<AppState>, publisher: OpusPublisher) -> anyhow::Result<()> {
    windows_wasapi::run(state, publisher)
}

/// Non-Windows hosts have no loopback capture backend yet (roadmap) — the
/// test tone doubles as a demo source.
#[cfg(all(feature = "audio", not(windows)))]
fn platform_source(state: &Arc<AppState>, publisher: OpusPublisher) -> anyhow::Result<()> {
    test_tone(state, publisher)
}

/// Accumulates interleaved 48 kHz stereo f32 samples, encodes 20 ms Opus
/// packets, and publishes them to every subscribed session.
#[cfg(feature = "audio")]
pub struct OpusPublisher {
    state: Arc<AppState>,
    encoder: opus::Encoder,
    pending: Vec<f32>,
    seq: u32,
    out: Vec<u8>,
}

#[cfg(feature = "audio")]
impl OpusPublisher {
    pub fn new(state: Arc<AppState>) -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE_BPS))?;
        Ok(Self {
            state,
            encoder,
            pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS as usize * 2),
            seq: 0,
            out: vec![0u8; 4000],
        })
    }

    /// Feed interleaved 48 kHz stereo samples; emits packets as they fill.
    fn push_samples(&mut self, interleaved: &[f32]) {
        if !self.has_listeners() {
            // Nobody listening: drop buffered audio so a new listener starts
            // fresh instead of hearing a stale burst.
            self.pending.clear();
            return;
        }
        self.pending.extend_from_slice(interleaved);
        let frame_len = FRAME_SAMPLES * CHANNELS as usize;
        let mut offset = 0;
        while self.pending.len() - offset >= frame_len {
            let chunk = &self.pending[offset..offset + frame_len];
            offset += frame_len;
            match self.encoder.encode_float(chunk, &mut self.out) {
                Ok(n) => {
                    self.seq = self.seq.wrapping_add(1);
                    let frame = ndsp_protocol::media::AudioFrame {
                        codec: ndsp_protocol::media::AudioCodec::Opus,
                        seq: self.seq,
                        timestamp_us: crate::util::now_us(),
                        sample_rate: SAMPLE_RATE,
                        channels: CHANNELS as u8,
                        payload: self.out[..n].to_vec(),
                    };
                    let _ = self.state.audio_tx.send(Some(Arc::new(frame)));
                }
                Err(e) => tracing::warn!("opus encode failed: {e}"),
            }
        }
        self.pending.drain(..offset);
    }
}

#[cfg(feature = "audio")]
impl PcmSink for OpusPublisher {
    fn has_listeners(&self) -> bool {
        self.state
            .audio_listeners
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    }

    fn push(&mut self, interleaved: &[f32]) {
        self.push_samples(interleaved);
    }
}

/// Synthetic source for tests, demos and non-Windows hosts: a gentle
/// 440 Hz tone with a slow tremolo (so waveforms visibly change).
#[cfg(feature = "audio")]
fn test_tone(state: &Arc<AppState>, mut publisher: OpusPublisher) -> anyhow::Result<()> {
    use std::f32::consts::TAU;
    tracing::info!("audio: synthetic test-tone source");
    let mut t: u64 = 0;
    let chunk = FRAME_SAMPLES; // one 20 ms chunk per iteration
    let mut buf = vec![0f32; chunk * CHANNELS as usize];
    loop {
        if state.is_shutdown() {
            return Ok(());
        }
        for i in 0..chunk {
            let n = (t + i as u64) as f32 / SAMPLE_RATE as f32;
            let s = (TAU * 440.0 * n).sin() * (0.15 + 0.1 * (TAU * 0.5 * n).sin());
            buf[i * 2] = s;
            buf[i * 2 + 1] = s;
        }
        t += chunk as u64;
        publisher.push(&buf);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(all(test, feature = "audio"))]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};

    async fn test_state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("ndsp-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(
            AppState::new(Config {
                name: "audio-test".into(),
                data_dir: dir,
                web_dir: None,
                file: FileConfig {
                    audio: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn packets_only_flow_with_listeners_and_decode() {
        let state = test_state().await;
        let mut publisher = OpusPublisher::new(state.clone()).unwrap();
        let mut rx = state.audio_tx.subscribe();
        let pcm = vec![0.01f32; FRAME_SAMPLES * 2 * 3]; // 3 packets worth

        // No listeners → nothing published.
        publisher.push(&pcm);
        assert!(!rx.has_changed().unwrap());

        // With a listener, packets appear and are valid Opus.
        state
            .audio_listeners
            .store(1, std::sync::atomic::Ordering::Relaxed);
        publisher.push(&pcm);
        assert!(rx.has_changed().unwrap());
        let frame = rx.borrow_and_update().clone().expect("packet");
        assert_eq!(frame.sample_rate, SAMPLE_RATE);
        assert_eq!(frame.channels, 2);
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES * 2];
        let n = dec.decode_float(&frame.payload, &mut out, false).unwrap();
        assert_eq!(n, FRAME_SAMPLES, "one 20 ms packet per frame");
    }
}
