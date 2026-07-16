//! Hardware H.264/HEVC encoding via Media Foundation Transforms (Windows
//! only).
//!
//! `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, HARDWARE)` finds whatever the
//! machine has — NVENC, Intel Quick Sync, AMD AMF — behind a single vendor-
//! neutral interface, always preferring hardware over software (software
//! MFTs are not enumerated at all; OpenH264 is our software fallback for
//! H.264; HEVC has no software fallback and is only offered when a hardware
//! MFT exists).
//!
//! Latency/design notes:
//! * Hardware H.264 MFTs are **async** transforms: they must be unlocked
//!   (`MF_TRANSFORM_ASYNC_UNLOCK`) and driven by `METransformNeedInput` /
//!   `METransformHaveOutput` events. This implementation drives them
//!   synchronously-with-events from the session's encode slot (one frame in
//!   → drain events until output), which keeps the queue depth at ≤1 frame —
//!   the lowest latency an MFT can operate at.
//! * `MF_LOW_LATENCY` + `CODECAPI_AVLowLatencyMode` + zero-B-frames are set
//!   so the encoder never buffers frames for lookahead.
//! * Rate control: `eAVEncCommonRateControlMode_CBR` with runtime
//!   `CODECAPI_AVEncCommonMeanBitRate` updates through ICodecAPI (all three
//!   vendors support runtime bitrate in CBR without reconfiguration).
//! * Keyframes: `CODECAPI_AVEncVideoForceKeyFrame`.
//! * Input is NV12 (converted from capture BGRA with the same integer BT.601
//!   coefficients as the OpenH264 path, single pass, dirty-row aware).
//! * Output is Annex-B (MFT default for MFVideoFormat_H264/HEVC) —
//!   identical downstream handling to the software encoder.
//! * **ROI hints (ROADMAP P0.3)**: when the encoder supports
//!   `CODECAPI_AVEncVideoROIEnabled`, the dirty-row bounds already computed
//!   for partial color conversion are attached to each input sample as an
//!   `MFSampleExtension_ROIRectangle` with a negative QP delta — the rate
//!   control spends its budget on the region that actually changed instead
//!   of re-polishing static content.

use ndsp_protocol::messages::Codec;
use windows::core::PWSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoROIEnabled, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFSample, IMFTransform, METransformHaveOutput,
    METransformNeedInput, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSampleExtension_ROIRectangle, MFStartup, MFTEnumEx,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFSTARTUP_NOSOCKET, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NONE, MF_EVENT_TYPE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, ROI_AREA,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use super::dirty::{DirtyMap, DirtyTracker};
use super::{Encoded, Encoder};
use crate::state::CapturedFrame;

/// One-time MF runtime init for the process (ref-counted by MF itself; we
/// keep it trivially by never shutting down until exit).
fn ensure_mf_started() -> anyhow::Result<()> {
    use std::sync::OnceLock;
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| unsafe {
            // COM first (MFTs are COM objects); MTA fits our thread model.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            // S_OK or S_FALSE (already initialized) are both fine; RPC_E_CHANGED_MODE
            // means an STA thread — also usable for free-threaded MFTs.
            if hr.is_err() && hr.0 as u32 != 0x8001_0106 {
                return Err(format!("CoInitializeEx: {hr:?}"));
            }
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| format!("MFStartup: {e}"))?;
            // MF stays initialized for the process lifetime (no MFShutdown
            // until exit — the OS reclaims it).
            Ok(())
        })
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

/// The MF output subtype for a negotiated codec (H.264/HEVC only).
fn subtype_of(codec: Codec) -> anyhow::Result<windows::core::GUID> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::Hevc => Ok(MFVideoFormat_HEVC),
        other => anyhow::bail!("no Media Foundation mapping for {other:?}"),
    }
}

/// Enumerate hardware encoder MFTs for `codec`; returns the best (first
/// after MFT_ENUM_FLAG_SORTANDFILTER ordering) activation + friendly name.
fn find_hw_encoder(codec: Codec) -> anyhow::Result<(IMFActivate, String)> {
    ensure_mf_started()?;
    unsafe {
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype_of(codec)?,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
        if count == 0 || activates.is_null() {
            anyhow::bail!("no hardware {codec:?} encoder MFT on this machine");
        }
        // Take the first; free the rest (CoTaskMemFree'd array of AddRef'd
        // activates per MFTEnumEx contract).
        let slice = std::slice::from_raw_parts_mut(activates, count as usize);
        let chosen = slice[0].take();
        for a in slice.iter_mut().skip(1) {
            drop(a.take());
        }
        CoTaskMemFree(Some(activates as *const _));
        let activate = chosen.ok_or_else(|| anyhow::anyhow!("null activate from MFTEnumEx"))?;

        let mut name_ptr = PWSTR::null();
        let mut name_len = 0u32;
        let name = if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name_ptr, &mut name_len)
            .is_ok()
            && !name_ptr.is_null()
        {
            let s = name_ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name_ptr.as_ptr() as *const _));
            s
        } else {
            format!("hardware {codec:?} MFT")
        };
        Ok((activate, name))
    }
}

/// True if this machine has a hardware encoder for `codec` (cached).
pub fn hw_encoder_available(codec: Codec) -> bool {
    use std::sync::OnceLock;
    static H264: OnceLock<bool> = OnceLock::new();
    static HEVC: OnceLock<bool> = OnceLock::new();
    let cell = match codec {
        Codec::H264 => &H264,
        Codec::Hevc => &HEVC,
        _ => return false,
    };
    *cell.get_or_init(|| match find_hw_encoder(codec) {
        Ok((_, name)) => {
            tracing::info!(encoder = %name, ?codec, "hardware encoder available");
            true
        }
        Err(e) => {
            tracing::info!("no hardware {codec:?} encoder ({e})");
            false
        }
    })
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: writing the discriminant + matching union member of a zeroed
    // VARIANT (the documented way to build one by hand).
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as above; VARIANT_TRUE is -1.
    unsafe {
        let inner = &mut var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    }
    var
}

pub struct MfVideoEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    /// Negotiated output codec (H264 or Hevc).
    codec: Codec,
    /// Encoder supports per-sample ROI rectangles (probed at init).
    roi_supported: bool,
    in_stream: u32,
    out_stream: u32,
    /// NV12 staging buffer (Y plane then interleaved UV).
    nv12: Nv12Buffer,
    dirty: DirtyTracker,
    size: (usize, usize),
    current_bitrate_kbps: u32,
    current_fps: u32,
    frame_index: u64,
    /// Pending NeedInput credits from the event stream.
    need_input_credits: u32,
    name: String,
}

// SAFETY: the encoder is owned by one session's video task and never shared;
// MF hardware MFTs are free-threaded COM objects.
unsafe impl Send for MfVideoEncoder {}

#[derive(Default)]
struct Nv12Buffer {
    data: Vec<u8>,
    w: usize,
    h: usize,
}

impl Nv12Buffer {
    fn ensure(&mut self, w: usize, h: usize) {
        if self.w != w || self.h != h {
            self.w = w;
            self.h = h;
            self.data.resize(w * h * 3 / 2, 0);
        }
    }

    /// Single-pass BGRA → NV12 (BT.601 limited range, same coefficients as
    /// the I420 path). `dirty` limits work to changed row pairs.
    fn fill_from_bgra(&mut self, bgra: &[u8], w: usize, h: usize, dirty: Option<&DirtyMap>) {
        self.ensure(w, h);
        debug_assert_eq!(bgra.len(), w * h * 4);
        let row = w * 4;
        #[inline(always)]
        fn luma(p: &[u8]) -> u8 {
            (((66 * p[2] as u32 + 129 * p[1] as u32 + 25 * p[0] as u32) >> 8) + 16) as u8
        }
        let (y_plane, uv_plane) = self.data.split_at_mut(w * h);
        let src_pairs = bgra.chunks_exact(row * 2);
        let y_pairs = y_plane.chunks_exact_mut(w * 2);
        let uv_rows = uv_plane.chunks_exact_mut(w);
        for (i, ((src2, y2), uv_row)) in src_pairs.zip(y_pairs).zip(uv_rows).enumerate() {
            if let Some(d) = dirty {
                if !d.pairs.get(i).copied().unwrap_or(true) {
                    continue;
                }
            }
            let (src0, src1) = src2.split_at(row);
            let (y0, y1) = y2.split_at_mut(w);
            let it = src0
                .chunks_exact(8)
                .zip(src1.chunks_exact(8))
                .zip(y0.chunks_exact_mut(2))
                .zip(y1.chunks_exact_mut(2))
                .zip(uv_row.chunks_exact_mut(2));
            for ((((s0, s1), yo0), yo1), uv) in it {
                let (p00, p01) = (&s0[0..4], &s0[4..8]);
                let (p10, p11) = (&s1[0..4], &s1[4..8]);
                yo0[0] = luma(p00);
                yo0[1] = luma(p01);
                yo1[0] = luma(p10);
                yo1[1] = luma(p11);
                let r = (p00[2] as i32 + p01[2] as i32 + p10[2] as i32 + p11[2] as i32 + 2) / 4;
                let g = (p00[1] as i32 + p01[1] as i32 + p10[1] as i32 + p11[1] as i32 + 2) / 4;
                let b = (p00[0] as i32 + p01[0] as i32 + p10[0] as i32 + p11[0] as i32 + 2) / 4;
                uv[0] = (((-38 * r - 74 * g + 112 * b) >> 8) + 128) as u8; // U
                uv[1] = (((112 * r - 94 * g - 18 * b) >> 8) + 128) as u8; // V
            }
        }
    }
}

impl MfVideoEncoder {
    pub fn new(
        codec: Codec,
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        fps: u32,
    ) -> anyhow::Result<Self> {
        let (activate, name) = find_hw_encoder(codec)?;
        unsafe {
            let mft: IMFTransform = activate.ActivateObject()?;

            // Async MFTs must be explicitly unlocked before use.
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;

            let events: IMFMediaEventGenerator = windows::core::Interface::cast(&mft)?;
            let codec_api: ICodecAPI = windows::core::Interface::cast(&mft)?;

            // Stream ids (hardware MFTs commonly use non-zero ids).
            let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
            let (in_stream, out_stream) = match mft.GetStreamIDs(&mut in_ids, &mut out_ids) {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0), // E_NOTIMPL → fixed streams 0/0
            };

            // Output type FIRST (encoders require output before input).
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &subtype_of(codec)?)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.max(100) * 1000)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, ((fps.max(1) as u64) << 32) | 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetOutputType(out_stream, &out_type, 0)?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, ((fps.max(1) as u64) << 32) | 1)?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(in_stream, &in_type, 0)?;

            // Low-latency rate control (runtime-adjustable CBR, no B-frames).
            let _ = codec_api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            let _ = codec_api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(bitrate_kbps.max(100) * 1000),
            );
            let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
            let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));

            // ROI hints (ROADMAP P0.3): opt in when the encoder supports it;
            // per-sample rectangles are attached in `encode`.
            let roi_supported = codec_api
                .IsSupported(&CODECAPI_AVEncVideoROIEnabled)
                .is_ok()
                && codec_api
                    .SetValue(&CODECAPI_AVEncVideoROIEnabled, &variant_bool(true))
                    .is_ok();
            if roi_supported {
                tracing::info!(encoder = %name, "encoder ROI rate-control hints enabled");
            }

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            tracing::info!(encoder = %name, ?codec, width, height, "hardware encoder initialized");
            Ok(Self {
                mft,
                events,
                codec_api,
                codec,
                roi_supported,
                in_stream,
                out_stream,
                nv12: Nv12Buffer::default(),
                dirty: DirtyTracker::default(),
                size: (width as usize, height as usize),
                current_bitrate_kbps: bitrate_kbps,
                current_fps: fps,
                frame_index: 0,
                need_input_credits: 0,
                name,
            })
        }
    }

    /// Pump the MFT's event queue until we can submit input and collect any
    /// output. Returns encoded bytes if a full output sample was produced.
    /// `roi` is an optional dirty-region rectangle attached as a rate-control
    /// hint (quality budget concentrated where pixels changed).
    fn submit_and_drain(
        &mut self,
        timestamp_100ns: i64,
        roi: Option<RECT>,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        unsafe {
            // Wait for a NeedInput credit (hardware encoders grant them
            // almost immediately at queue depth ≤ 1).
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while self.need_input_credits == 0 {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "{}: timed out waiting for encoder input credit",
                    self.name
                );
                // Blocking GetEvent (flag 0). MF returns each event in order.
                let event = self.events.GetEvent(MF_EVENT_FLAG_NONE)?;
                match MF_EVENT_TYPE(event.GetType()? as i32) {
                    t if t == METransformNeedInput => self.need_input_credits += 1,
                    t if t == METransformHaveOutput => {
                        // Output before our input (stale credit) — drain it.
                        if let Some(out) = self.drain_one_output()? {
                            // Deliver it; the caller's frame will ride the
                            // next credit. (Rare: only after a flush.)
                            return Ok(Some(out));
                        }
                    }
                    _ => {}
                }
            }

            // Build the input sample around our NV12 buffer.
            let buf = MFCreateMemoryBuffer(self.nv12.data.len() as u32)?;
            {
                let mut ptr = std::ptr::null_mut();
                buf.Lock(&mut ptr, None, None)?;
                std::ptr::copy_nonoverlapping(self.nv12.data.as_ptr(), ptr, self.nv12.data.len());
                buf.Unlock()?;
                buf.SetCurrentLength(self.nv12.data.len() as u32)?;
            }
            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(timestamp_100ns)?;
            sample.SetSampleDuration(10_000_000i64 / self.current_fps.max(1) as i64)?;
            if let Some(rect) = roi {
                // Spend quality on the changed region: a modest negative QP
                // delta inside the ROI (static content is already encoded as
                // skip blocks and costs nothing to leave alone).
                let area = ROI_AREA { rect, QPDelta: -4 };
                let bytes = std::slice::from_raw_parts(
                    (&raw const area).cast::<u8>(),
                    std::mem::size_of::<ROI_AREA>(),
                );
                let _ = sample.SetBlob(&MFSampleExtension_ROIRectangle, bytes);
            }
            self.mft.ProcessInput(self.in_stream, &sample, 0)?;
            self.need_input_credits -= 1;

            // Drain events until the matching output arrives (or the encoder
            // asks for more input first — pipeline depth 1 on first frames).
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "{}: timed out waiting for encoder output",
                    self.name
                );
                let event = self.events.GetEvent(MF_EVENT_FLAG_NONE)?;
                match MF_EVENT_TYPE(event.GetType()? as i32) {
                    t if t == METransformNeedInput => {
                        self.need_input_credits += 1;
                        // Encoder wants more input before yielding output —
                        // that's its (short) internal pipeline. Report no
                        // output for this frame; the next frame flushes it.
                        return Ok(None);
                    }
                    t if t == METransformHaveOutput => {
                        if let Some(out) = self.drain_one_output()? {
                            return Ok(Some(out));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Pull one output sample; None on NEED_MORE_INPUT.
    fn drain_one_output(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        unsafe {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_stream,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let hr = self
                .mft
                .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);
            let result = match hr {
                Ok(()) => {
                    let sample = std::mem::ManuallyDrop::take(&mut out.pSample);
                    if let Some(sample) = sample {
                        let buf = sample.ConvertToContiguousBuffer()?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        buf.Lock(&mut ptr, None, Some(&mut len))?;
                        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                        buf.Unlock()?;
                        Ok(Some(bytes))
                    } else {
                        Ok(None)
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) => Err(anyhow::anyhow!("{}: ProcessOutput: {e}", self.name)),
            };
            // Release any event collection the MFT attached.
            drop(std::mem::ManuallyDrop::take(&mut out.pEvents));
            result
        }
    }

    fn apply_targets(&mut self, bitrate_kbps: u32, fps: u32) {
        if bitrate_kbps != self.current_bitrate_kbps {
            let ok = unsafe {
                self.codec_api
                    .SetValue(
                        &CODECAPI_AVEncCommonMeanBitRate,
                        &variant_u32(bitrate_kbps.max(100) * 1000),
                    )
                    .is_ok()
            };
            if ok {
                self.current_bitrate_kbps = bitrate_kbps;
            } // else: keep encoding at the old rate; retried next change.
        }
        // FPS is a rate-control hint only; MFTs take it at init. The bitrate
        // budget is what actually constrains output size per frame.
        self.current_fps = fps;
    }
}

/// Scan Annex-B NAL unit headers, testing each first header byte with `f`.
fn annexb_any_nal(data: &[u8], f: impl Fn(u8) -> bool) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            let (nal_at, step) = if data[i + 2] == 1 {
                (i + 3, 3)
            } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                (i + 4, 4)
            } else {
                i += 1;
                continue;
            };
            if nal_at < data.len() && f(data[nal_at]) {
                return true;
            }
            i = nal_at + step;
        } else {
            i += 1;
        }
    }
    false
}

/// H.264 Annex-B: is this access unit an IDR? (NAL type 5).
fn annexb_has_idr(data: &[u8]) -> bool {
    annexb_any_nal(data, |b| b & 0x1F == 5)
}

/// H.265 Annex-B: does this access unit contain an IRAP picture?
/// (nal_unit_type 16..=21 — BLA/IDR/CRA — all self-contained sync points.)
fn annexb_hevc_has_irap(data: &[u8]) -> bool {
    annexb_any_nal(data, |b| {
        let ty = (b >> 1) & 0x3F;
        (16..=21).contains(&ty)
    })
}

impl Encoder for MfVideoEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_keyframe: bool,
        target_bitrate_kbps: u32,
        fps_hint: u32,
    ) -> anyhow::Result<Encoded> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        anyhow::ensure!(frame.bgra.len() == w * h * 4, "frame buffer size mismatch");
        anyhow::ensure!(
            self.size == (w, h),
            "resolution change requires a new encoder (session recreates it)"
        );
        self.apply_targets(target_bitrate_kbps, fps_hint);

        let t_conv = std::time::Instant::now();
        let dirty = self.dirty.update(&frame.bgra, w, h);
        if dirty.is_static() && !force_keyframe {
            return Ok(Encoded {
                payload: Vec::new(),
                keyframe: false,
                codec: self.codec,
                convert_us: t_conv.elapsed().as_micros() as u32,
            });
        }
        if force_keyframe {
            // Applies to the next frame submitted.
            let _ = unsafe {
                self.codec_api
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1))
            };
        }
        self.nv12.fill_from_bgra(&frame.bgra, w, h, Some(&dirty));
        let convert_us = t_conv.elapsed().as_micros() as u32;

        // ROI hint: only for partial updates on delta frames (a keyframe or
        // full-frame change has no "rest of the frame" to deprioritize).
        let roi = if self.roi_supported && !force_keyframe && !dirty.all_dirty() {
            dirty.row_bounds().map(|(y0, y1)| RECT {
                left: 0,
                top: y0 as i32,
                right: w as i32,
                bottom: y1 as i32,
            })
        } else {
            None
        };

        self.frame_index += 1;
        let ts_100ns = (self.frame_index as i64) * 10_000_000 / self.current_fps.max(1) as i64;
        let payload = self.submit_and_drain(ts_100ns, roi)?.unwrap_or_default();
        let keyframe = !payload.is_empty()
            && match self.codec {
                Codec::Hevc => annexb_hevc_has_irap(&payload),
                _ => annexb_has_idr(&payload),
            };
        Ok(Encoded {
            payload,
            keyframe,
            codec: self.codec,
            convert_us,
        })
    }
}

impl Drop for MfVideoEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}
