//! Bridge to the NebulaDisplay IddCx virtual-monitor driver.
//!
//! The driver (see `host/windows-driver/`) exposes each virtual monitor's
//! frames through a named shared-memory section plus a "frame ready" event:
//!
//! ```text
//! Section: Global\NebulaDisplay.Frame.<index>
//!   [0..4)   magic       = 0x4E44_4653 ("NDFS")
//!   [4..8)   version     = 1
//!   [8..12)  width       u32
//!   [12..16) height      u32
//!   [16..20) stride      u32 (bytes per row)
//!   [20..24) format      u32 (1 = BGRA8)
//!   [24..32) frame_seq   u64 (incremented after each complete write)
//!   [32..]   pixel data  (stride * height bytes)
//! Event:   Global\NebulaDisplay.FrameReady.<index>   (auto-reset)
//! ```
//!
//! The driver writes a frame, bumps `frame_seq`, and signals the event.
//! Torn reads are detected by re-checking `frame_seq` after copying; a torn
//! frame is simply skipped (the next one arrives within a frame interval).
//! This is a deliberately simple, robust cross-privilege transport that
//! avoids custom IOCTL surface area.

#![cfg(windows)]

use std::time::Instant;

use anyhow::{bail, Context};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Threading::{
    OpenEventW, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

use super::{Frame, FrameSource};

const MAGIC: u32 = 0x4E44_4653; // "NDFS"
const HEADER_LEN: usize = 32;

pub struct VirtualMonitorSource {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
    last_seq: u64,
    width: u32,
    height: u32,
}

unsafe impl Send for VirtualMonitorSource {}

impl VirtualMonitorSource {
    /// Attach to virtual monitor 0 exposed by the driver.
    pub fn connect() -> anyhow::Result<Self> {
        Self::connect_index(0)
    }

    pub fn connect_index(index: u32) -> anyhow::Result<Self> {
        unsafe {
            let section_name = HSTRING::from(format!("Global\\NebulaDisplay.Frame.{index}"));
            let event_name = HSTRING::from(format!("Global\\NebulaDisplay.FrameReady.{index}"));

            let mapping = OpenFileMappingW(FILE_MAP_READ.0, false, &section_name)
                .context("virtual display driver shared memory not found — is the NebulaDisplay driver installed and a monitor attached?")?;
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(mapping).ok();
                bail!("MapViewOfFile failed");
            }
            let event = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, &event_name)
                .context("driver frame event not found")?;

            let header = std::slice::from_raw_parts(view.Value as *const u8, HEADER_LEN);
            let u32at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
            if u32at(0) != MAGIC {
                bail!("driver shared memory has wrong magic");
            }
            let width = u32at(8);
            let height = u32at(12);

            Ok(Self {
                mapping,
                view,
                event,
                last_seq: 0,
                width,
                height,
            })
        }
    }

    unsafe fn header_u64(&self, offset: usize) -> u64 {
        let p = (self.view.Value as *const u8).add(offset) as *const u64;
        p.read_volatile()
    }

    unsafe fn header_u32(&self, offset: usize) -> u32 {
        let p = (self.view.Value as *const u8).add(offset) as *const u32;
        p.read_volatile()
    }
}

impl FrameSource for VirtualMonitorSource {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout_ms: u32) -> anyhow::Result<Option<Frame>> {
        unsafe {
            if WaitForSingleObject(self.event, timeout_ms) != WAIT_OBJECT_0 {
                return Ok(None); // timeout — no new frame
            }
            let seq_before = self.header_u64(24);
            if seq_before == self.last_seq {
                return Ok(None);
            }
            let width = self.header_u32(8);
            let height = self.header_u32(12);
            let stride = self.header_u32(16) as usize;
            let (w, h) = (width as usize, height as usize);
            if w == 0 || h == 0 || stride < w * 4 {
                return Ok(None);
            }

            let mut bgra = vec![0u8; w * h * 4];
            let data = std::slice::from_raw_parts(
                (self.view.Value as *const u8).add(HEADER_LEN),
                stride * h,
            );
            for row in 0..h {
                bgra[row * w * 4..(row + 1) * w * 4]
                    .copy_from_slice(&data[row * stride..row * stride + w * 4]);
            }

            // Torn-frame detection: if the driver overwrote the buffer while
            // we copied, drop this frame and wait for the next signal.
            let seq_after = self.header_u64(24);
            if seq_after != seq_before {
                return Ok(None);
            }
            self.last_seq = seq_before;
            self.width = width;
            self.height = height;

            Ok(Some(Frame {
                bgra,
                width,
                height,
                captured_at: Instant::now(),
            }))
        }
    }

    fn describe(&self) -> String {
        format!("IddCx virtual monitor ({}x{})", self.width, self.height)
    }
}

impl Drop for VirtualMonitorSource {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view).ok();
            CloseHandle(self.mapping).ok();
            CloseHandle(self.event).ok();
        }
    }
}
