//! System audio streaming (optional, off by default).
//!
//! On Windows, audio is captured with WASAPI loopback on the default render
//! endpoint — the documented, official way to record "what the PC plays".
//! Frames are converted to interleaved s16le PCM and shipped as
//! [`nebula_proto::AudioPacket`]s (codec `PcmS16le`). Opus encoding is a
//! planned upgrade (same packet format, codec id 2) once a vetted
//! pure-Rust/opus dependency is chosen; raw 48kHz stereo s16 is ~1.5Mbps,
//! acceptable on LAN.
//!
//! Non-Windows builds expose [`AudioCapture::unsupported`] so the server can
//! report a clear diagnostic instead of failing silently.

use nebula_proto::{AudioCodec, AudioPacket};

/// A chunk of captured audio ready for the wire.
pub struct AudioChunk {
    pub payload: Vec<u8>,
    pub channels: u8,
    pub sample_rate: u32,
    pub capture_ts_micros: u64,
}

impl AudioChunk {
    pub fn to_packet(&self, seq: u32) -> Vec<u8> {
        AudioPacket {
            codec: AudioCodec::PcmS16le,
            channels: self.channels,
            seq,
            capture_ts_micros: self.capture_ts_micros,
            sample_rate: self.sample_rate,
            payload: &self.payload,
        }
        .encode()
    }
}

#[cfg(windows)]
pub use win::WasapiLoopback;

#[cfg(windows)]
mod win {
    //! WASAPI loopback capture.
    use super::AudioChunk;
    use anyhow::Context;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    pub struct WasapiLoopback {
        _client: IAudioClient,
        capture: IAudioCaptureClient,
        channels: u16,
        sample_rate: u32,
        /// True when the mix format is float32 (the common case).
        is_float: bool,
        bits_per_sample: u16,
        started: std::time::Instant,
    }

    // SAFETY: used from a single dedicated audio thread.
    unsafe impl Send for WasapiLoopback {}

    impl WasapiLoopback {
        pub fn new() -> anyhow::Result<Self> {
            unsafe {
                // Idempotent per-thread COM init; ignore "already initialized".
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                        .context("MMDeviceEnumerator")?;
                let device = enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .context("default render endpoint")?;
                let client: IAudioClient =
                    device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;
                let fmt_ptr = client.GetMixFormat().context("GetMixFormat")?;
                let fmt: WAVEFORMATEX = *fmt_ptr;

                let mut is_float = fmt.wFormatTag == WAVE_FORMAT_IEEE_FLOAT;
                if fmt.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
                    let ext = &*(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
                    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT first Data1 = 3.
                    is_float = ext.SubFormat.data1 == WAVE_FORMAT_IEEE_FLOAT as u32;
                }

                // 200ms buffer, event-less polling mode.
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        2_000_000,
                        0,
                        fmt_ptr,
                        None,
                    )
                    .context("IAudioClient::Initialize(loopback)")?;
                let capture: IAudioCaptureClient = client.GetService().context("capture client")?;
                client.Start().context("IAudioClient::Start")?;

                Ok(Self {
                    channels: fmt.nChannels,
                    sample_rate: fmt.nSamplesPerSec,
                    bits_per_sample: fmt.wBitsPerSample,
                    is_float,
                    _client: client,
                    capture,
                    started: std::time::Instant::now(),
                })
            }
        }

        /// Drain currently buffered audio; returns `None` when nothing is
        /// pending (caller sleeps ~10ms between polls).
        pub fn poll(&mut self) -> anyhow::Result<Option<AudioChunk>> {
            unsafe {
                let pending = self.capture.GetNextPacketSize()?;
                if pending == 0 {
                    return Ok(None);
                }
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(&mut data_ptr, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let samples = (frames * self.channels as u32) as usize;

                let payload: Vec<u8> = if self.is_float && self.bits_per_sample == 32 {
                    let src = std::slice::from_raw_parts(data_ptr as *const f32, samples);
                    let mut out = Vec::with_capacity(samples * 2);
                    for &s in src {
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                    out
                } else if self.bits_per_sample == 16 {
                    std::slice::from_raw_parts(data_ptr, samples * 2).to_vec()
                } else {
                    self.capture.ReleaseBuffer(frames)?;
                    anyhow::bail!(
                        "unsupported mix format: {} bits, float={}",
                        self.bits_per_sample,
                        self.is_float
                    );
                };

                self.capture
                    .ReleaseBuffer(frames)
                    .context("ReleaseBuffer")?;
                Ok(Some(AudioChunk {
                    payload,
                    channels: self.channels as u8,
                    sample_rate: self.sample_rate,
                    capture_ts_micros: self.started.elapsed().as_micros() as u64,
                }))
            }
        }
    }

    #[allow(dead_code)]
    fn assert_wave_format_pcm_constant_used() -> u16 {
        WAVE_FORMAT_PCM
    }
}

/// Reason audio is unavailable on this build/host, for diagnostics.
pub fn availability() -> Result<(), &'static str> {
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err("system audio capture is implemented for Windows (WASAPI loopback) only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_packet_round_trip() {
        let chunk = AudioChunk {
            payload: vec![1, 2, 3, 4],
            channels: 2,
            sample_rate: 48_000,
            capture_ts_micros: 5,
        };
        let bytes = chunk.to_packet(9);
        let p = nebula_proto::AudioPacket::decode(&bytes).unwrap();
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.seq, 9);
        assert_eq!(p.payload, &[1, 2, 3, 4]);
    }
}
