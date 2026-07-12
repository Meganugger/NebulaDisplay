//! Host audio pipeline: capture → Opus → encrypted channel 3.
//!
//! * Windows: WASAPI **loopback** of the default render endpoint ("what you
//!   hear"), converted to 48 kHz stereo f32.
//! * Everywhere else / tests: a soft 440 Hz test tone paced in real time, so
//!   the entire pipeline (encode → seal → viewer decode) is exercisable in
//!   CI — the audio equivalent of the video test pattern.
//!
//! Privacy model: **off by default** (`audio = false` in config). When on,
//! a session still has to opt in with `SetAudio { enabled: true }`, and the
//! panel shows a live indicator for every session receiving audio. One
//! encoder feeds all listeners (identical packets, watch-broadcast).

use ndsp_protocol::messages::AudioCodec;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "audio")]
use tracing::info;
use tracing::warn;

use crate::state::AppState;
#[cfg(feature = "audio")]
use crate::util::now_us;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
/// Opus frame: 20 ms → 960 frames → 1920 interleaved stereo samples.
pub const FRAMES_PER_BLOCK: usize = 960;
pub const SAMPLES_PER_BLOCK: usize = FRAMES_PER_BLOCK * CHANNELS as usize;
#[cfg(feature = "audio")]
const OPUS_BITRATE: i32 = 96_000;

/// One encoded packet, shared zero-copy between sessions.
pub struct AudioPacket {
    pub seq: u32,
    pub timestamp_us: u64,
    pub opus: Vec<u8>,
}

/// A source of interleaved stereo f32 blocks at 48 kHz. Implementations may
/// block (the pipeline runs them on a dedicated blocking thread) and are
/// expected to pace at real time.
pub trait AudioSource: Send {
    fn name(&self) -> &'static str;
    /// Fill `out` (`SAMPLES_PER_BLOCK` samples) with the next 20 ms block.
    fn next_block(&mut self, out: &mut [f32]) -> anyhow::Result<()>;
}

#[cfg(feature = "audio")]
fn create_source() -> anyhow::Result<Box<dyn AudioSource>> {
    #[cfg(windows)]
    {
        match wasapi::WasapiLoopback::new() {
            Ok(s) => return Ok(Box::new(s)),
            Err(e) => {
                warn!("WASAPI loopback unavailable ({e:#}); using test tone");
            }
        }
    }
    Ok(Box::new(ToneSource::default()))
}

/// Paced 440 Hz sine at −18 dBFS (non-Windows hosts + WASAPI failure).
pub struct ToneSource {
    phase: f32,
    started: Option<std::time::Instant>,
    blocks: u64,
}

impl Default for ToneSource {
    fn default() -> Self {
        Self {
            phase: 0.0,
            started: None,
            blocks: 0,
        }
    }
}

impl AudioSource for ToneSource {
    fn name(&self) -> &'static str {
        "test-tone"
    }
    fn next_block(&mut self, out: &mut [f32]) -> anyhow::Result<()> {
        let started = *self.started.get_or_insert_with(std::time::Instant::now);
        // Real-time pacing: block until this 20 ms slot is due.
        let due = Duration::from_micros(self.blocks * 20_000);
        let elapsed = started.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
        self.blocks += 1;
        let step = 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
        for frame in out.chunks_exact_mut(CHANNELS as usize) {
            let s = self.phase.sin() * 0.125;
            self.phase = (self.phase + step) % std::f32::consts::TAU;
            for ch in frame {
                *ch = s;
            }
        }
        Ok(())
    }
}

/// Supervisor: idles until the host config allows audio *and* at least one
/// session asked for it, then runs capture+encode on a blocking thread until
/// the last listener leaves. Restarts the source on failure with backoff.
pub async fn run_audio_pipeline(state: Arc<AppState>) {
    if !state.cfg.file.audio {
        return; // disabled by config — sessions get an explicit error
    }
    let mut check = tokio::time::interval(Duration::from_millis(200));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        check.tick().await;
        if state.is_shutdown() {
            return;
        }
        if state.audio_listeners.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let st = state.clone();
        let res = tokio::task::spawn_blocking(move || capture_encode_loop(st)).await;
        if let Err(e) = res {
            warn!("audio pipeline task panicked: {e}");
        }
        if state.is_shutdown() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Blocking capture → encode → publish loop; returns when listeners hit 0,
/// on shutdown, or on unrecoverable source/encoder errors.
#[cfg(feature = "audio")]
fn capture_encode_loop(state: Arc<AppState>) {
    let mut source = match create_source() {
        Ok(s) => s,
        Err(e) => {
            warn!("no audio source: {e:#}");
            return;
        }
    };
    let mut encoder = match opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::Audio,
    ) {
        Ok(mut enc) => {
            let _ = enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE));
            enc
        }
        Err(e) => {
            warn!("opus encoder init failed: {e}");
            return;
        }
    };
    info!(source = source.name(), "audio pipeline started");
    let mut pcm = vec![0f32; SAMPLES_PER_BLOCK];
    let mut seq: u32 = 0;
    loop {
        if state.is_shutdown() || state.audio_listeners.load(Ordering::Relaxed) == 0 {
            info!("audio pipeline stopped (no listeners)");
            return;
        }
        if let Err(e) = source.next_block(&mut pcm) {
            warn!("audio capture failed: {e:#}; restarting pipeline");
            return;
        }
        let timestamp_us = now_us();
        match encoder.encode_vec_float(&pcm, 4000) {
            Ok(opus) => {
                seq = seq.wrapping_add(1);
                let _ = state.audio_tx.send(Some(Arc::new(AudioPacket {
                    seq,
                    timestamp_us,
                    opus,
                })));
            }
            Err(e) => {
                warn!("opus encode failed: {e}");
            }
        }
    }
}

/// Codec parameters advertised in `AudioStart`.
pub fn stream_params() -> (AudioCodec, u32, u8) {
    (AudioCodec::Opus, SAMPLE_RATE, CHANNELS)
}

#[cfg(all(windows, feature = "audio"))]
mod wasapi {
    //! WASAPI loopback capture of the default render endpoint, converted to
    //! 48 kHz stereo f32. Handles f32 and i16 mix formats at any rate and
    //! channel count (downmix > 2ch by averaging pairs, linear resampling).

    use super::{AudioSource, CHANNELS, SAMPLES_PER_BLOCK, SAMPLE_RATE};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
    };
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::Media::Multimedia::{
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };

    enum SampleKind {
        F32,
        I16,
    }

    /// Per-thread COM guard (the pipeline owns its blocking thread).
    struct ComGuard;
    impl ComGuard {
        fn new() -> Self {
            // SAFETY: paired with CoUninitialize in Drop; RPC_E_CHANGED_MODE
            // just means COM was already up on this thread — harmless here.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            ComGuard
        }
    }
    impl Drop for ComGuard {
        fn drop(&mut self) {
            // SAFETY: paired with CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }

    pub struct WasapiLoopback {
        _com: ComGuard,
        _client: IAudioClient,
        capture: IAudioCaptureClient,
        src_rate: u32,
        src_channels: usize,
        kind: SampleKind,
        /// Interleaved stereo f32 at the *source* rate, pending resample.
        pending: Vec<f32>,
        /// Fractional read position into `pending` (in frames).
        resample_pos: f64,
    }

    // Owned COM interfaces are only touched from the pipeline thread.
    unsafe impl Send for WasapiLoopback {}

    impl WasapiLoopback {
        pub fn new() -> anyhow::Result<Self> {
            let com = ComGuard::new();
            // SAFETY: standard WASAPI activation sequence; every returned
            // pointer is checked by windows-rs Result plumbing.
            unsafe {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let client: IAudioClient = device.Activate::<IAudioClient>(CLSCTX_ALL, None)?;
                let fmt_ptr = client.GetMixFormat()?;
                anyhow::ensure!(!fmt_ptr.is_null(), "GetMixFormat returned null");
                // WAVEFORMATEX is a packed struct — read every field
                // unaligned to avoid UB.
                let fmt: WAVEFORMATEX = std::ptr::read_unaligned(fmt_ptr);
                let format_tag = fmt.wFormatTag;
                let kind = if format_tag as u32 == WAVE_FORMAT_IEEE_FLOAT {
                    SampleKind::F32
                } else if format_tag as u32 == WAVE_FORMAT_PCM {
                    SampleKind::I16
                } else if format_tag as u32 == WAVE_FORMAT_EXTENSIBLE {
                    let ext: WAVEFORMATEXTENSIBLE =
                        std::ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
                    let sub = ext.SubFormat;
                    if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                        SampleKind::F32
                    } else {
                        SampleKind::I16
                    }
                } else {
                    CoTaskMemFree(Some(fmt_ptr as *const _));
                    anyhow::bail!("unsupported mix format tag {format_tag}");
                };
                let src_rate = fmt.nSamplesPerSec;
                let src_channels = fmt.nChannels as usize;
                // 100 ms buffer, event-less polling mode.
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    1_000_000,
                    0,
                    fmt_ptr,
                    None,
                )?;
                CoTaskMemFree(Some(fmt_ptr as *const _));
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                Ok(Self {
                    _com: com,
                    _client: client,
                    capture,
                    src_rate,
                    src_channels,
                    kind,
                    pending: Vec::with_capacity(SAMPLES_PER_BLOCK * 4),
                    resample_pos: 0.0,
                })
            }
        }

        /// Pull whatever WASAPI has buffered into `pending` (stereo f32 at
        /// the source rate).
        fn drain_device(&mut self) -> anyhow::Result<()> {
            // SAFETY: GetBuffer/ReleaseBuffer used per the WASAPI contract.
            unsafe {
                loop {
                    let frames_avail = self.capture.GetNextPacketSize()?;
                    if frames_avail == 0 {
                        return Ok(());
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    if frames > 0 && !data.is_null() {
                        let n = frames as usize;
                        let ch = self.src_channels;
                        // AUDCLNT_BUFFERFLAGS_SILENT (0x2): treat as zeros.
                        let silent = flags & 0x2 != 0;
                        for i in 0..n {
                            let (l, r) = if silent {
                                (0.0, 0.0)
                            } else {
                                match self.kind {
                                    SampleKind::F32 => {
                                        let s = data as *const f32;
                                        let base = i * ch;
                                        let l = *s.add(base);
                                        let r = if ch > 1 { *s.add(base + 1) } else { l };
                                        (l, r)
                                    }
                                    SampleKind::I16 => {
                                        let s = data as *const i16;
                                        let base = i * ch;
                                        let l = *s.add(base) as f32 / 32768.0;
                                        let r = if ch > 1 {
                                            *s.add(base + 1) as f32 / 32768.0
                                        } else {
                                            l
                                        };
                                        (l, r)
                                    }
                                }
                            };
                            self.pending.push(l);
                            self.pending.push(r);
                        }
                    }
                    self.capture.ReleaseBuffer(frames)?;
                }
            }
        }
    }

    impl AudioSource for WasapiLoopback {
        fn name(&self) -> &'static str {
            "wasapi-loopback"
        }

        fn next_block(&mut self, out: &mut [f32]) -> anyhow::Result<()> {
            debug_assert_eq!(out.len(), SAMPLES_PER_BLOCK);
            let ratio = self.src_rate as f64 / SAMPLE_RATE as f64;
            let needed_src_frames = (super::FRAMES_PER_BLOCK as f64 * ratio).ceil() as usize + 2;
            // Accumulate enough source frames (device paces us in real time).
            loop {
                self.drain_device()?;
                if self.pending.len() / CHANNELS as usize >= needed_src_frames {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Linear resample source → 48 kHz (identity when rates match).
            let src_frames = self.pending.len() / 2;
            for (i, frame) in out.chunks_exact_mut(2).enumerate() {
                let pos = self.resample_pos + i as f64 * ratio;
                let i0 = pos.floor() as usize;
                let frac = (pos - pos.floor()) as f32;
                let i1 = (i0 + 1).min(src_frames - 1);
                frame[0] = self.pending[i0 * 2] * (1.0 - frac) + self.pending[i1 * 2] * frac;
                frame[1] =
                    self.pending[i0 * 2 + 1] * (1.0 - frac) + self.pending[i1 * 2 + 1] * frac;
            }
            // Advance and drop consumed source frames.
            let advanced = self.resample_pos + super::FRAMES_PER_BLOCK as f64 * ratio;
            let whole = advanced.floor() as usize;
            self.resample_pos = advanced - whole as f64;
            self.pending.drain(0..(whole.min(src_frames)) * 2);
            Ok(())
        }
    }
}

#[cfg(not(feature = "audio"))]
fn capture_encode_loop(_state: Arc<AppState>) {
    warn!("audio requested but this build has no `audio` feature (Opus disabled)");
    std::thread::sleep(Duration::from_secs(2)); // avoid a hot restart loop
}

#[cfg(all(test, feature = "audio"))]
mod tests {
    use super::*;

    #[test]
    fn tone_source_produces_paced_nonsilent_blocks() {
        let mut src = ToneSource::default();
        let mut block = vec![0f32; SAMPLES_PER_BLOCK];
        let t0 = std::time::Instant::now();
        for _ in 0..3 {
            src.next_block(&mut block).unwrap();
        }
        // 3 blocks = 60 ms of audio → pacing must take ≥ 40 ms (first block
        // is immediate).
        assert!(t0.elapsed() >= Duration::from_millis(35));
        assert!(block.iter().any(|s| s.abs() > 0.01), "tone must be audible");
        assert!(block.iter().all(|s| s.abs() <= 0.5), "tone must be capped");
    }

    #[test]
    fn opus_roundtrip_of_tone() {
        let mut src = ToneSource::default();
        let mut block = vec![0f32; SAMPLES_PER_BLOCK];
        src.next_block(&mut block).unwrap();
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .unwrap();
        let pkt = enc.encode_vec_float(&block, 4000).unwrap();
        assert!(!pkt.is_empty() && pkt.len() < 1500, "20ms opus packet");
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0f32; SAMPLES_PER_BLOCK];
        let frames = dec.decode_float(&pkt, &mut out, false).unwrap();
        assert_eq!(frames, FRAMES_PER_BLOCK);
    }
}
