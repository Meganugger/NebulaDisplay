//! Audio pipeline: capture → Opus → encrypted channel 3 (ROADMAP P2.8).
//!
//! Privacy model (docs/SECURITY.md): audio is **off by default** at three
//! independent levels — the host config (`audio = false`), the panel's live
//! switch, and each viewer's explicit opt-in (`set_audio`). The panel shows
//! a live indicator whenever any client is receiving audio, plus a per-client
//! mute.
//!
//! Pipeline shape mirrors video: one capture + one encode per host (not per
//! client), fanned out through a broadcast channel; sessions forward packets
//! only for opted-in, unmuted clients. 48 kHz stereo, 20 ms Opus frames at
//! 96 kbps ≈ 12 KiB/s — negligible next to video.
//!
//! * Windows: WASAPI loopback of the default render device (what the user
//!   hears), converted to 48 kHz stereo f32.
//! * Non-Windows / tests: a 440 Hz test tone source keeps the entire
//!   pipeline (encode, framing, grants, fan-out, web playback) verifiable
//!   in CI — same philosophy as the test-pattern video source.

#[cfg(windows)]
mod windows_wasapi;

mod tone;

use std::sync::Arc;
#[cfg(feature = "audio")]
use tracing::info;
use tracing::warn;

use crate::state::AppState;
#[cfg(feature = "audio")]
use crate::util::now_us;
#[cfg(feature = "audio")]
use ndsp_protocol::media::{AudioFrame, AUDIO_CODEC_OPUS};

/// Fixed output format: everything is converted to this before encoding.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
/// Opus frame duration in ms (20 ms = 960 samples/channel at 48 kHz).
pub const FRAME_MS: u32 = 20;
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;
/// Encoder bitrate — transparent for desktop audio, tiny on the wire.
pub const BITRATE_BPS: i32 = 96_000;

/// A blocking PCM source. `next_chunk` returns interleaved stereo f32 at
/// 48 kHz, blocking until samples are available (sources pace themselves to
/// real time). Chunk sizes are arbitrary; the pipeline reframes to 20 ms.
pub trait AudioSource: Send {
    fn next_chunk(&mut self) -> anyhow::Result<Vec<f32>>;
    fn name(&self) -> &'static str;
}

/// Pick the platform capture source. `test_tone` forces the synthetic source
/// (used by tests/CI and `--audio-test-tone`).
pub fn create_source(test_tone: bool) -> anyhow::Result<Box<dyn AudioSource>> {
    if test_tone {
        return Ok(Box::new(tone::ToneSource::new()));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows_wasapi::WasapiLoopback::new()?))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("no system audio capture on this OS (use --audio-test-tone for testing)")
    }
}

/// Run capture + Opus encode on a blocking thread, publishing encoded frames
/// into `state.audio_tx`. Returns when the host shuts down or the source
/// fails permanently.
#[cfg(feature = "audio")]
pub async fn run_audio_pipeline(state: Arc<AppState>, mut source: Box<dyn AudioSource>) {
    let name = source.name();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .map_err(|e| anyhow::anyhow!("opus encoder init: {e}"))?;
        enc.set_bitrate(opus::Bitrate::Bits(BITRATE_BPS))
            .map_err(|e| anyhow::anyhow!("opus bitrate: {e}"))?;

        info!(
            source = name,
            "audio pipeline running (idle until a viewer opts in)"
        );
        state.set_audio_ready(true);

        let frame_len = FRAME_SAMPLES * CHANNELS as usize;
        let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 2);
        let mut acc_started_us = now_us();
        let mut seq: u32 = 0;
        loop {
            if state.is_shutdown() {
                return Ok(());
            }
            let chunk = source.next_chunk()?;
            if acc.is_empty() {
                acc_started_us = now_us();
            }
            acc.extend_from_slice(&chunk);
            while acc.len() >= frame_len {
                let frame: Vec<f32> = acc.drain(..frame_len).collect();
                let ts = acc_started_us;
                acc_started_us += (FRAME_MS as u64) * 1000;
                // Nobody listening (host switch off or zero subscribers)?
                // Keep consuming the source but skip the encode entirely.
                if !state.audio_available() || state.audio_tx.receiver_count() == 0 {
                    seq = seq.wrapping_add(1);
                    continue;
                }
                let payload = enc
                    .encode_vec_float(&frame, 1500)
                    .map_err(|e| anyhow::anyhow!("opus encode: {e}"))?;
                seq = seq.wrapping_add(1);
                let _ = state.audio_tx.send(Arc::new(AudioFrame {
                    codec: AUDIO_CODEC_OPUS,
                    channels: CHANNELS,
                    seq,
                    timestamp_us: ts,
                    sample_rate: SAMPLE_RATE,
                    payload,
                }));
            }
        }
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("audio pipeline stopped: {e:#}"),
        Err(e) => warn!("audio pipeline panicked: {e}"),
    }
}

/// Built without the `audio` feature: pipeline is unavailable.
#[cfg(not(feature = "audio"))]
pub async fn run_audio_pipeline(_state: Arc<AppState>, _source: Box<dyn AudioSource>) {
    warn!("this build has no `audio` feature; audio disabled");
}
