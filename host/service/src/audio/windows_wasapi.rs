//! WASAPI shared-mode loopback capture of the default render device.
//!
//! Delivers whatever the host is playing (the "what you hear" stream),
//! converted to 48 kHz stereo f32 for the Opus publisher. The mix format is
//! whatever the audio engine runs at — commonly 48 kHz float stereo, but
//! 44.1 kHz and >2 channels exist, so this module downmixes and linearly
//! resamples as needed (linear interpolation is inaudible for a 44.1→48
//! conversion of desktop audio and costs almost nothing).
//!
//! Device changes (default output switched, device unplugged) surface as
//! capture errors; the loop then re-opens the current default endpoint.

use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::{PcmSink, CHANNELS, SAMPLE_RATE};
use crate::state::AppState;

const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

struct SourceFormat {
    rate: u32,
    channels: u16,
    bits: u16,
    float: bool,
    block_align: u16,
}

/// Parse WAVEFORMATEX(TENSIBLE) into what the converter needs.
///
/// # Safety
/// `fmt` must point at a valid WAVEFORMATEX returned by `GetMixFormat`.
unsafe fn parse_format(fmt: *const WAVEFORMATEX) -> anyhow::Result<SourceFormat> {
    // WAVEFORMATEX(TENSIBLE) are packed(1): copy fields to locals instead of
    // taking (UB-prone unaligned) references.
    let f = unsafe { *fmt };
    let tag = f.wFormatTag;
    let bits = f.wBitsPerSample;
    let mut float = tag as u32 == WAVE_FORMAT_IEEE_FLOAT;
    if tag == WAVE_FORMAT_EXTENSIBLE_TAG {
        let ext = unsafe { *(fmt as *const WAVEFORMATEXTENSIBLE) };
        let sub = ext.SubFormat;
        float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    }
    if !float && bits != 16 {
        anyhow::bail!("unsupported mix format: tag={tag} bits={bits}");
    }
    Ok(SourceFormat {
        rate: f.nSamplesPerSec,
        channels: f.nChannels,
        bits,
        float,
        block_align: f.nBlockAlign,
    })
}

/// Downmix to stereo + linear-resample to 48 kHz, appending to `out`.
struct Converter {
    src: SourceFormat,
    /// Fractional read position (in source frames) carried between packets.
    pos: f64,
    stereo: Vec<f32>,
}

impl Converter {
    fn new(src: SourceFormat) -> Self {
        Self {
            src,
            pos: 0.0,
            stereo: Vec::new(),
        }
    }

    fn frame_to_stereo(&self, raw: &[u8], frame: usize) -> [f32; 2] {
        let ch = self.src.channels as usize;
        let sample = |c: usize| -> f32 {
            let idx = frame * ch + c.min(ch - 1);
            if self.src.float {
                let off = idx * 4;
                f32::from_le_bytes(raw[off..off + 4].try_into().unwrap())
            } else {
                let off = idx * 2;
                i16::from_le_bytes(raw[off..off + 2].try_into().unwrap()) as f32 / 32768.0
            }
        };
        match ch {
            1 => {
                let m = sample(0);
                [m, m]
            }
            2 => [sample(0), sample(1)],
            // Front-left/front-right of multichannel layouts; a full matrix
            // downmix is not worth it for desktop capture.
            _ => [sample(0), sample(1)],
        }
    }

    fn push(&mut self, raw: &[u8], frames: usize, publisher: &mut dyn PcmSink) {
        if frames == 0 {
            return;
        }
        // Collect this packet as stereo f32 at the source rate.
        self.stereo.clear();
        self.stereo.reserve(frames * 2);
        for i in 0..frames {
            let [l, r] = self.frame_to_stereo(raw, i);
            self.stereo.push(l);
            self.stereo.push(r);
        }

        if self.src.rate == SAMPLE_RATE {
            publisher.push(&self.stereo);
            return;
        }

        // Linear resample: `pos` walks the source timeline in source frames
        // and carries its fractional remainder across packets, so the ratio
        // stays exact over time. The <1-sample interpolation error at each
        // packet boundary (clamped upper neighbor) is inaudible.
        let step = self.src.rate as f64 / SAMPLE_RATE as f64;
        let mut out: Vec<f32> = Vec::with_capacity(
            (frames * SAMPLE_RATE as usize).div_ceil(self.src.rate as usize) * 2 + 2,
        );
        let last = frames - 1;
        while self.pos < frames as f64 {
            let base = self.pos.floor() as usize;
            let frac = (self.pos - base as f64) as f32;
            let i0 = base.min(last);
            let i1 = (base + 1).min(last);
            let (a0, a1) = (self.stereo[i0 * 2], self.stereo[i1 * 2]);
            let (b0, b1) = (self.stereo[i0 * 2 + 1], self.stereo[i1 * 2 + 1]);
            out.push(a0 + (a1 - a0) * frac);
            out.push(b0 + (b1 - b0) * frac);
            self.pos += step;
        }
        self.pos -= frames as f64;
        publisher.push(&out);
    }
}

pub(super) fn run(state: &Arc<AppState>, mut publisher: impl PcmSink) -> anyhow::Result<()> {
    // SAFETY: dedicated thread; COM initialized once for its lifetime.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            anyhow::bail!("CoInitializeEx failed: {hr:?}");
        }
    }
    tracing::info!("audio: WASAPI loopback capture of the default output device");
    loop {
        if state.is_shutdown() {
            return Ok(());
        }
        if let Err(e) = capture_session(state, &mut publisher) {
            if state.is_shutdown() {
                return Ok(());
            }
            tracing::warn!("audio capture interrupted ({e:#}); re-opening in 1 s");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn capture_session(state: &Arc<AppState>, publisher: &mut dyn PcmSink) -> anyhow::Result<()> {
    // SAFETY: standard WASAPI loopback bring-up; all pointers come from the
    // API itself and buffers are only read between GetBuffer/ReleaseBuffer.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt_ptr = client.GetMixFormat()?;
        let fmt = parse_format(fmt_ptr)?;
        // 100 ms engine buffer: far larger than our 10 ms poll, so nothing
        // drops even under scheduling hiccups.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            1_000_000, // 100 ms in 100 ns units
            0,
            fmt_ptr,
            None,
        )?;
        CoTaskMemFree(Some(fmt_ptr as *const _));
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        tracing::info!(
            rate = fmt.rate,
            channels = fmt.channels,
            bits = fmt.bits,
            float = fmt.float,
            "audio: loopback stream open"
        );

        let mut conv = Converter::new(fmt);
        let mut silence_scratch: Vec<u8> = Vec::new();
        loop {
            if state.is_shutdown() {
                let _ = client.Stop();
                return Ok(());
            }
            loop {
                let packet = capture.GetNextPacketSize()?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if frames > 0 {
                    let bytes = frames as usize * conv.src.block_align as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        // Keep the stream continuous during silence.
                        silence_scratch.clear();
                        silence_scratch.resize(bytes, 0);
                        conv.push(&silence_scratch, frames as usize, publisher);
                    } else {
                        let raw = std::slice::from_raw_parts(data, bytes);
                        conv.push(raw, frames as usize, publisher);
                    }
                }
                capture.ReleaseBuffer(frames)?;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

// Silence the unused-constant warning on the CHANNELS re-export path: the
// converter always produces exactly CHANNELS-interleaved output.
const _: () = assert!(CHANNELS == 2);
