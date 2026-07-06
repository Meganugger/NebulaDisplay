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
use std::time::{Duration, Instant};

const RING_MAGIC: u32 = 0x4E44_5352; // "NDSR"
const RING_VERSION: u32 = 2;
const RING_SLOTS: u32 = 3;
const MAX_W: u32 = 4096;
const MAX_H: u32 = 2304;

fn ring_name(index: u32) -> String {
    format!("Local\\NebulaDisplay.FrameRing.v2.{index}")
}
fn frame_event_name(index: u32) -> String {
    format!("Local\\NebulaDisplay.FrameReady.v2.{index}")
}

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
    monitor_index: u32,
    reserved: [u32; 6],
    slot_headers: [SlotHeader; RING_SLOTS as usize],
}

pub struct WindowsIddSource {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
    last_frame_number: u64,
    monitor_index: u32,
    /// Cached desktop rect from QueryDisplayConfig (refreshed lazily).
    desktop_rect: std::cell::Cell<Option<(i32, i32, i32, i32)>>,
    rect_refreshed: std::cell::Cell<Option<Instant>>,
}

// SAFETY: owned by the single capture thread; raw pointers derive from the
// mapping which lives as long as `self`.
unsafe impl Send for WindowsIddSource {}

impl WindowsIddSource {
    /// Attach to the driver's ring for virtual monitor `index`. Fails
    /// cleanly when the driver isn't installed/running — callers fall back
    /// to DXGI mirror mode.
    pub fn new(index: u32) -> anyhow::Result<Self> {
        unsafe {
            let mapping =
                OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(ring_name(index)))
                    .context(
                        "virtual display driver ring not found (driver not installed/active)",
                    )?;
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
                &HSTRING::from(frame_event_name(index)),
            )
            .context("driver frame event not found")?;
            Ok(Self {
                mapping,
                view,
                event,
                last_frame_number: 0,
                monitor_index: index,
                desktop_rect: std::cell::Cell::new(None),
                rect_refreshed: std::cell::Cell::new(None),
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

    /// Desktop-space rect of this virtual monitor, resolved through
    /// QueryDisplayConfig by matching the monitor friendly name our EDID
    /// carries ("NebulaDsply"); the Nth match (in stable target-id order)
    /// corresponds to ring index N. This is what maps viewer input onto the
    /// extend-mode monitor at its actual position in the desktop layout.
    /// Refreshed at most every 2 s so live layout changes are picked up.
    fn desktop_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let stale = self
            .rect_refreshed
            .get()
            .is_none_or(|t| t.elapsed() > Duration::from_secs(2));
        if stale {
            self.desktop_rect
                .set(query_virtual_monitor_rect(self.monitor_index));
            self.rect_refreshed.set(Some(Instant::now()));
        }
        self.desktop_rect.get()
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
                if !seq1.is_multiple_of(2) {
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

/// Find the desktop rect of the `index`-th NebulaDisplay virtual monitor via
/// QueryDisplayConfig (active paths only). Matching key: the EDID display
/// name descriptor ("NebulaDsply") surfaced as monitorFriendlyDeviceName.
fn query_virtual_monitor_rect(index: u32) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    unsafe {
        let (mut n_paths, mut n_modes) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes)
            != ERROR_SUCCESS
        {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        paths.truncate(n_paths as usize);
        modes.truncate(n_modes as usize);

        // Collect (target_id, source_rect) of every path whose monitor name
        // matches ours, then pick the index-th in stable order.
        let mut matches: Vec<(u32, (i32, i32, i32, i32))> = Vec::new();
        for path in &paths {
            let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            name.header.adapterId = path.targetInfo.adapterId;
            name.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut name.header) != 0 {
                continue;
            }
            let friendly = String::from_utf16_lossy(
                &name.monitorFriendlyDeviceName[..name
                    .monitorFriendlyDeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(64)],
            );
            if !friendly.starts_with("NebulaDsply") {
                continue;
            }
            // Source mode carries this monitor's desktop position + size.
            let mode_idx = path.sourceInfo.Anonymous.modeInfoIdx as usize;
            let Some(mode) = modes.get(mode_idx) else {
                continue;
            };
            if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                continue;
            }
            let sm = mode.Anonymous.sourceMode;
            matches.push((
                path.targetInfo.id,
                (
                    sm.position.x,
                    sm.position.y,
                    sm.position.x + sm.width as i32,
                    sm.position.y + sm.height as i32,
                ),
            ));
        }
        matches.sort_by_key(|(id, _)| *id);
        matches.get(index as usize).map(|(_, rect)| *rect)
    }
}
