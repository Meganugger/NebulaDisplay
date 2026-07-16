//! WASAPI loopback capture of the default render endpoint — i.e. "what the
//! user hears", regardless of which app is playing.
//!
//! The shared-mode mix format is device-dependent (typically float32 at
//! 44.1/48 kHz with 2+ channels); everything is normalized here to the
//! pipeline's fixed 48 kHz stereo f32 (front-left/front-right downmix +
//! linear resampling — perfectly adequate for desktop-audio streaming).

use anyhow::{bail, Context};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use super::{AudioSource, CHANNELS, SAMPLE_RATE};

// Constants kept local to avoid extra `windows` feature surface.
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
/// 200 ms device buffer (in 100 ns units) — ample for a 10 ms poll loop.
const BUFFER_DURATION_HNS: i64 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleKind {
    F32,
    I16,
}

struct SourceFormat {
    kind: SampleKind,
    channels: usize,
    sample_rate: u32,
}

pub struct WasapiLoopback {
    _client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: SourceFormat,
    /// Fractional resampler position between the last chunk's source frames.
    resample_pos: f64,
    /// Last source frame carried across chunk boundaries for interpolation.
    carry: Option<[f32; 2]>,
}

// The COM interfaces are used from the single blocking pipeline thread only.
unsafe impl Send for WasapiLoopback {}

fn parse_format(fmt: &WAVEFORMATEX) -> anyhow::Result<SourceFormat> {
    let tag = fmt.wFormatTag;
    let bits = fmt.wBitsPerSample;
    let kind = if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
        SampleKind::F32
    } else if tag == WAVE_FORMAT_PCM && bits == 16 {
        SampleKind::I16
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        // SubFormat GUID: Data1 == 1 → PCM, 3 → IEEE float.
        let ext = unsafe { &*(fmt as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
        match (ext.SubFormat.data1, bits) {
            (3, 32) => SampleKind::F32,
            (1, 16) => SampleKind::I16,
            (sub, bits) => bail!("unsupported extensible mix format (sub={sub}, bits={bits})"),
        }
    } else {
        bail!("unsupported mix format (tag={tag}, bits={bits})");
    };
    Ok(SourceFormat {
        kind,
        channels: fmt.nChannels as usize,
        sample_rate: fmt.nSamplesPerSec,
    })
}

impl WasapiLoopback {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            // Match the MF encoder's model: initialize MTA once per thread;
            // RPC_E_CHANGED_MODE etc. are tolerable (already initialized).
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("MMDeviceEnumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("default render endpoint")?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;
            let fmt_ptr = client.GetMixFormat().context("GetMixFormat")?;
            let fmt = parse_format(&*fmt_ptr);
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    BUFFER_DURATION_HNS,
                    0,
                    fmt_ptr,
                    None,
                )
                .context("IAudioClient::Initialize(loopback)")?;
            CoTaskMemFree(Some(fmt_ptr as *const _));
            let fmt = fmt?;
            let capture: IAudioCaptureClient = client.GetService().context("capture client")?;
            client.Start().context("IAudioClient::Start")?;
            tracing::info!(
                rate = fmt.sample_rate,
                channels = fmt.channels,
                kind = ?fmt.kind,
                "WASAPI loopback capturing"
            );
            Ok(Self {
                _client: client,
                capture,
                fmt,
                resample_pos: 0.0,
                carry: None,
            })
        }
    }

    /// Convert one device packet to stereo f32 frames at the *source* rate.
    unsafe fn convert(
        &self,
        data: *const u8,
        frames: usize,
        silent: bool,
        out: &mut Vec<[f32; 2]>,
    ) {
        if silent {
            out.extend(std::iter::repeat_n([0.0f32; 2], frames));
            return;
        }
        let ch = self.fmt.channels;
        match self.fmt.kind {
            SampleKind::F32 => {
                let s = std::slice::from_raw_parts(data as *const f32, frames * ch);
                for f in 0..frames {
                    let l = s[f * ch];
                    let r = if ch > 1 { s[f * ch + 1] } else { l };
                    out.push([l, r]);
                }
            }
            SampleKind::I16 => {
                let s = std::slice::from_raw_parts(data as *const i16, frames * ch);
                for f in 0..frames {
                    let l = s[f * ch] as f32 / 32768.0;
                    let r = if ch > 1 {
                        s[f * ch + 1] as f32 / 32768.0
                    } else {
                        l
                    };
                    out.push([l, r]);
                }
            }
        }
    }

    /// Linear resample `src` (stereo frames at the source rate) to 48 kHz,
    /// interleaving into `out`. Keeps sub-frame position + one carry frame
    /// across calls so chunk boundaries are seamless.
    fn resample_into(&mut self, src: &[[f32; 2]], out: &mut Vec<f32>) {
        if self.fmt.sample_rate == SAMPLE_RATE {
            for f in src {
                out.push(f[0]);
                out.push(f[1]);
            }
            return;
        }
        let step = self.fmt.sample_rate as f64 / SAMPLE_RATE as f64;
        // Work on carry + src as a virtual buffer starting at index 0.
        let carry = self.carry;
        let get = move |i: usize| -> [f32; 2] {
            if let Some(c) = carry {
                if i == 0 {
                    return c;
                }
                return src[(i - 1).min(src.len() - 1)];
            }
            src[i.min(src.len() - 1)]
        };
        let virtual_len = src.len() + usize::from(carry.is_some());
        if virtual_len < 2 {
            // Not enough to interpolate; stash and wait for more.
            if let Some(&last) = src.last() {
                self.carry = Some(last);
            }
            return;
        }
        let mut pos = self.resample_pos;
        while pos + 1.0 < virtual_len as f64 {
            let i = pos as usize;
            let frac = (pos - i as f64) as f32;
            let a = get(i);
            let b = get(i + 1);
            out.push(a[0] + (b[0] - a[0]) * frac);
            out.push(a[1] + (b[1] - a[1]) * frac);
            pos += step;
        }
        // Keep the last source frame for interpolation continuity.
        self.carry = Some(get(virtual_len - 1));
        self.resample_pos = pos - (virtual_len - 1) as f64;
    }
}

impl AudioSource for WasapiLoopback {
    fn next_chunk(&mut self) -> anyhow::Result<Vec<f32>> {
        debug_assert_eq!(CHANNELS, 2);
        let mut stereo: Vec<[f32; 2]> = Vec::with_capacity(960);
        loop {
            let packet = unsafe { self.capture.GetNextPacketSize() }.context("packet size")?;
            if packet == 0 {
                if !stereo.is_empty() {
                    break;
                }
                // Loopback produces data only while something renders; poll
                // gently and emit silence keepalive after ~40 ms of nothing
                // so the Opus stream (and viewer clocks) keep flowing.
                std::thread::sleep(std::time::Duration::from_millis(10));
                stereo.extend(std::iter::repeat_n(
                    [0.0f32; 2],
                    (self.fmt.sample_rate as usize) / 100, // 10 ms of silence
                ));
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("GetBuffer")?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT != 0;
                self.convert(data, frames as usize, silent, &mut stereo);
                self.capture
                    .ReleaseBuffer(frames)
                    .context("ReleaseBuffer")?;
            }
        }
        let mut out = Vec::with_capacity(stereo.len() * 2);
        self.resample_into(&stereo, &mut out);
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "wasapi-loopback"
    }
}
