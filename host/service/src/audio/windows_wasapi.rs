//! WASAPI loopback capture of the default render device (what the PC is
//! playing), converted to the pipeline's fixed 48 kHz stereo s16 format.
//!
//! Compiled only on Windows; validated by the Windows CI job (clippy + unit
//! tests), runtime-validated on real hardware per docs/TESTING.md.
//!
//! Notes:
//! * Loopback capture runs in shared mode against the device **mix format**
//!   (usually 32-bit float, 44.1/48 kHz, ≥2 channels) — we downmix and
//!   resample to the pipeline format.
//! * When nothing is playing, loopback delivers no packets; the audio loop
//!   substitutes silence to keep the stream cadence.
//! * `AUDCLNT_BUFFERFLAGS_SILENT` packets carry no valid data by contract —
//!   zeros are synthesized for them.

use anyhow::{bail, Context};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::{f32_to_i16, to_stereo, AudioSource, LinearResampler, BLOCK_LEN};

/// 200 ms device buffer (in 100 ns units) — generous slack for scheduling
/// hiccups without adding latency (we drain everything available per poll).
const BUFFER_DURATION_HNS: i64 = 2_000_000;

enum SampleFormat {
    F32,
    I16,
}

pub struct WasapiLoopbackSource {
    _client: IAudioClient,
    capture: IAudioCaptureClient,
    format: SampleFormat,
    device_channels: usize,
    resampler: LinearResampler,
    /// Interleaved 48 kHz stereo f32 samples waiting to be emitted.
    ready: Vec<f32>,
    /// Scratch buffers reused across polls.
    scratch_f32: Vec<f32>,
    scratch_stereo: Vec<f32>,
}

// SAFETY: owned by the single audio thread for its entire lifetime.
unsafe impl Send for WasapiLoopbackSource {}

impl WasapiLoopbackSource {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            // Idempotent per thread; RPC_E_CHANGED_MODE would only occur if
            // this thread were already STA, which our blocking thread is not.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() && hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
                bail!("CoInitializeEx failed: {hr:?}");
            }

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("MMDeviceEnumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no default render device")?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;

            let mix_ptr = client.GetMixFormat().context("GetMixFormat")?;
            anyhow::ensure!(!mix_ptr.is_null(), "GetMixFormat returned null");
            let mix: WAVEFORMATEX = *mix_ptr;
            let device_rate = mix.nSamplesPerSec;
            let device_channels = mix.nChannels as usize;
            let bits = mix.wBitsPerSample;
            let format = detect_format(mix_ptr)?;

            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_HNS,
                0,
                mix_ptr,
                None,
            );
            CoTaskMemFree(Some(mix_ptr as *const _));
            init.context("IAudioClient::Initialize(loopback)")?;

            let capture: IAudioCaptureClient =
                client.GetService().context("IAudioCaptureClient")?;
            client.Start().context("IAudioClient::Start")?;

            tracing::info!(
                rate = device_rate,
                channels = device_channels,
                bits,
                "WASAPI loopback initialized"
            );
            anyhow::ensure!(device_channels > 0, "device reports zero channels");
            Ok(Self {
                _client: client,
                capture,
                format,
                device_channels,
                resampler: LinearResampler::new(device_rate, 2),
                ready: Vec::with_capacity(BLOCK_LEN * 4),
                scratch_f32: Vec::new(),
                scratch_stereo: Vec::new(),
            })
        }
    }

    /// Drain every packet WASAPI has buffered into `self.ready`.
    fn drain(&mut self) -> anyhow::Result<()> {
        unsafe {
            loop {
                let pending = self
                    .capture
                    .GetNextPacketSize()
                    .context("GetNextPacketSize")?;
                if pending == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let n = frames as usize * self.device_channels;
                self.scratch_f32.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    self.scratch_f32.resize(n, 0.0);
                } else {
                    match self.format {
                        SampleFormat::F32 => {
                            let src = std::slice::from_raw_parts(data as *const f32, n);
                            self.scratch_f32.extend_from_slice(src);
                        }
                        SampleFormat::I16 => {
                            let src = std::slice::from_raw_parts(data as *const i16, n);
                            self.scratch_f32
                                .extend(src.iter().map(|&s| s as f32 / 32768.0));
                        }
                    }
                }
                self.capture
                    .ReleaseBuffer(frames)
                    .context("ReleaseBuffer")?;
                self.scratch_stereo.clear();
                let scratch = std::mem::take(&mut self.scratch_f32);
                to_stereo(&scratch, self.device_channels, &mut self.scratch_stereo);
                self.scratch_f32 = scratch;
                let stereo = std::mem::take(&mut self.scratch_stereo);
                self.resampler.process(&stereo, &mut self.ready);
                self.scratch_stereo = stereo;
            }
        }
    }
}

fn detect_format(fmt: *const WAVEFORMATEX) -> anyhow::Result<SampleFormat> {
    unsafe {
        let tag = (*fmt).wFormatTag as u32;
        let bits = (*fmt).wBitsPerSample;
        if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
            return Ok(SampleFormat::F32);
        }
        if tag == WAVE_FORMAT_PCM && bits == 16 {
            return Ok(SampleFormat::I16);
        }
        if tag == WAVE_FORMAT_EXTENSIBLE {
            // WAVEFORMATEXTENSIBLE is packed(1): read the GUID unaligned.
            let ext = fmt as *const WAVEFORMATEXTENSIBLE;
            let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                return Ok(SampleFormat::F32);
            }
            if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
                return Ok(SampleFormat::I16);
            }
        }
        bail!("unsupported mix format (tag {tag}, {bits} bits)")
    }
}

impl AudioSource for WasapiLoopbackSource {
    fn name(&self) -> &'static str {
        "wasapi-loopback"
    }

    fn fill(&mut self, out: &mut [i16; BLOCK_LEN]) -> anyhow::Result<bool> {
        self.drain()?;
        if self.ready.len() < BLOCK_LEN {
            // Nothing (or not enough) playing — the loop substitutes silence.
            out.fill(0);
            return Ok(false);
        }
        for (dst, src) in out.iter_mut().zip(self.ready.drain(..BLOCK_LEN)) {
            *dst = f32_to_i16(src);
        }
        // Latency guard: if the backlog exceeds ~60 ms (scheduler stall,
        // paused consumer), drop the oldest — live audio must stay live.
        let max_backlog = BLOCK_LEN * 6;
        if self.ready.len() > max_backlog {
            let excess = self.ready.len() - max_backlog;
            self.ready.drain(..excess);
        }
        Ok(true)
    }
}
