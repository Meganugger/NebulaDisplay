//! Frame source that consumes the IddCx driver's shared-memory ring
//! (true *extend* mode). ABI mirror of
//! `host/windows-driver/include/ndsp_frame_ring.h` — keep in sync.

use anyhow::{bail, Context};
use ndsp_protocol::messages::DisplayMode;
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

use super::FrameSource;

const RING_MAGIC: u32 = 0x4E44_5352; // "NDSR"
const RING_VERSION: u32 = 1;
const RING_SLOTS: u32 = 3;
const RING_NAME: &str = "Local\\NebulaDisplay.FrameRing.v1";
const FRAME_EVENT: &str = "Local\\NebulaDisplay.FrameReady.v1";
const MAX_W: u32 = 4096;
const MAX_H: u32 = 2304;

#[repr(C, align(8))]
struct SlotHeader {
    seq: u32,
    width: u32,
    height: u32,
    pitch_bytes: u32,
    timestamp_qpc: u64,
    frame_number: u64,
}

#[repr(C, align(8))]
struct RingHeader {
    magic: u32,
    version: u32,
    slots: u32,
    slot_stride: u32,
    latest_slot: u32,
    connected: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    reserved: [u32; 7],
    slot_headers: [SlotHeader; RING_SLOTS as usize],
}

pub struct WindowsIddSource {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
    last_frame_number: u64,
}

// SAFETY: owned by the single capture thread; raw pointers derive from the
// mapping which lives as long as `self`.
unsafe impl Send for WindowsIddSource {}

impl WindowsIddSource {
    /// Attach to the driver's ring. Fails cleanly when the driver isn't
    /// installed/running — callers fall back to DXGI mirror mode.
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let mapping = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(RING_NAME))
                .context("virtual display driver ring not found (driver not installed/active)")?;
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(mapping);
                bail!("MapViewOfFile failed for driver ring");
            }
            let header = &*(view.Value as *const RingHeader);
            if header.magic != RING_MAGIC {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(mapping);
                bail!("driver ring has wrong magic (stale mapping?)");
            }
            if header.version != RING_VERSION {
                let ver = header.version;
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(mapping);
                bail!("driver ring version {ver} != service version {RING_VERSION}; update driver/service together");
            }
            let event = OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &HSTRING::from(FRAME_EVENT),
            )
            .context("driver frame event not found")?;
            Ok(Self {
                mapping,
                view,
                event,
                last_frame_number: 0,
            })
        }
    }

    fn header(&self) -> &RingHeader {
        // SAFETY: validated in new(); mapping outlives self.
        unsafe { &*(self.view.Value as *const RingHeader) }
    }

    pub fn is_connected(&self) -> bool {
        let h = self.header();
        unsafe { std::ptr::read_volatile(&h.connected) != 0 }
    }
}

impl Drop for WindowsIddSource {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.mapping);
            let _ = CloseHandle(self.event);
        }
    }
}

impl FrameSource for WindowsIddSource {
    fn name(&self) -> &'static str {
        "iddcx-ring"
    }

    fn mode(&self) -> DisplayMode {
        let h = self.header();
        let (w, hgt, hz) = unsafe {
            (
                std::ptr::read_volatile(&h.width),
                std::ptr::read_volatile(&h.height),
                std::ptr::read_volatile(&h.refresh_hz),
            )
        };
        DisplayMode {
            width: if w == 0 { 1920 } else { w },
            height: if hgt == 0 { 1080 } else { hgt },
            refresh_hz: if hz == 0 { 60 } else { hz },
        }
    }

    fn next_frame(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        unsafe {
            // Wait briefly for the driver to publish a new frame.
            if WaitForSingleObject(self.event, 16) != WAIT_OBJECT_0 {
                return Ok(false);
            }
            let header = self.view.Value as *const RingHeader;
            let latest = std::ptr::read_volatile(&(*header).latest_slot);
            if latest >= RING_SLOTS {
                return Ok(false);
            }
            let slot = &(*header).slot_headers[latest as usize];

            // Seqlock read: retry on torn frames (driver mid-write).
            for _ in 0..3 {
                let seq1 = std::ptr::read_volatile(&slot.seq);
                if seq1 % 2 != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                let (w, h, pitch, frame_number) = (
                    std::ptr::read_volatile(&slot.width),
                    std::ptr::read_volatile(&slot.height),
                    std::ptr::read_volatile(&slot.pitch_bytes),
                    std::ptr::read_volatile(&slot.frame_number),
                );
                if frame_number == self.last_frame_number {
                    return Ok(false); // duplicate wakeup
                }
                if w == 0 || h == 0 || w > MAX_W || h > MAX_H || pitch < w * 4 {
                    bail!("driver ring slot has implausible geometry {w}x{h} pitch {pitch}");
                }
                let stride = std::ptr::read_volatile(&(*header).slot_stride) as usize;
                let payload = (self.view.Value as *const u8)
                    .add(std::mem::size_of::<RingHeader>() + latest as usize * stride);
                out.resize((w * h * 4) as usize, 0);
                let row = (w * 4) as usize;
                for y in 0..h as usize {
                    std::ptr::copy_nonoverlapping(
                        payload.add(y * pitch as usize),
                        out.as_mut_ptr().add(y * row),
                        row,
                    );
                }
                let seq2 = std::ptr::read_volatile(&slot.seq);
                if seq1 == seq2 {
                    self.last_frame_number = frame_number;
                    return Ok(true);
                }
                // Torn — retry.
            }
            Ok(false)
        }
    }
}
