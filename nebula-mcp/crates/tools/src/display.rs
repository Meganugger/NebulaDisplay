//! Display tools.
//!
//! On Windows these call native Win32/DXGI/DWM APIs (QueryDisplayConfig, DXGI
//! adapter/output enumeration, DWM composition/timing, advanced-colour/HDR
//! detection, monitor topology and coordinate conversion). On other platforms
//! each returns [`nebula_mcp_core::ToolError::PlatformUnsupported`].
//!
//! The native implementations live in the `win` submodule, compiled only for
//! Windows targets. The cross-platform tool wrappers gate on the host OS first.

use std::sync::Arc;

use async_trait::async_trait;
use nebula_mcp_core::{Result, Tool, ToolContext};
use nebula_mcp_protocol::mcp::ToolAnnotations;
use nebula_mcp_protocol::CallToolResult;
use serde_json::Value;

use crate::common::platform::ensure_windows;
use crate::common::{Args, ObjectSchema};

const CATEGORY: &str = "display";

/// Build display tools.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(QueryConfig),
        Arc::new(DxgiAdapters),
        Arc::new(Monitors),
        Arc::new(DwmInfo),
        Arc::new(PresentStats),
        Arc::new(HdrDetection),
        Arc::new(MouseToMonitor),
        Arc::new(VirtualDisplays),
    ]
}

fn ro() -> Option<ToolAnnotations> {
    Some(ToolAnnotations {
        read_only_hint: Some(true),
        ..Default::default()
    })
}

/// Macro to define a read-only, Windows-gated display tool that delegates to a
/// native `win::` function. Keeps the wrappers uniform and free of duplicated
/// platform boilerplate.
macro_rules! display_tool {
    ($ty:ident, $name:literal, $desc:literal, $schema:expr, $win_call:expr) => {
        struct $ty;
        #[async_trait]
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn category(&self) -> &str {
                CATEGORY
            }
            fn description(&self) -> &str {
                $desc
            }
            fn input_schema(&self) -> Value {
                ($schema)()
            }
            fn annotations(&self) -> Option<ToolAnnotations> {
                ro()
            }
            #[allow(unused_variables)]
            async fn call(&self, ctx: &ToolContext, args: Value) -> Result<CallToolResult> {
                ensure_windows(self.name())?;
                let a = Args::new(&args)?;
                #[cfg(windows)]
                {
                    let f: fn(&Args) -> Result<Value> = $win_call;
                    return Ok(crate::common::output::json_value_result(f(&a)?));
                }
                #[cfg(not(windows))]
                {
                    let _ = &a;
                    Err(nebula_mcp_core::ToolError::PlatformUnsupported(
                        $name.to_string(),
                    ))
                }
            }
        }
    };
}

fn empty_schema() -> Value {
    ObjectSchema::new().build()
}

display_tool!(
    QueryConfig,
    "display.query_config",
    "Enumerate active display paths (source/target ids, resolution, position, refresh rate) via QueryDisplayConfig. Windows only.",
    empty_schema,
    win_query_config
);
display_tool!(
    DxgiAdapters,
    "display.dxgi_adapters",
    "Enumerate DXGI adapters and their outputs (description, dedicated VRAM, desktop coordinates). Windows only.",
    empty_schema,
    win_dxgi_adapters
);
display_tool!(
    Monitors,
    "display.monitors",
    "Enumerate the monitor topology (device names, work/monitor rects, primary flag). Windows only.",
    empty_schema,
    win_monitors
);
display_tool!(
    DwmInfo,
    "display.dwm_info",
    "Report DWM composition state and colorization. Windows only.",
    empty_schema,
    win_dwm_info
);
display_tool!(
    PresentStats,
    "display.present_stats",
    "Report DWM composition timing (refresh rate, refresh period, frame counts) as a proxy for present statistics. Windows only.",
    empty_schema,
    win_present_stats
);
display_tool!(
    HdrDetection,
    "display.hdr_detection",
    "Detect advanced colour / HDR capability and current state per display target. Windows only.",
    empty_schema,
    win_hdr_detection
);

fn mouse_schema() -> Value {
    ObjectSchema::new()
        .integer("x", "Screen X coordinate (virtual desktop).", true)
        .integer("y", "Screen Y coordinate (virtual desktop).", true)
        .build()
}
display_tool!(
    MouseToMonitor,
    "display.mouse_to_monitor",
    "Map a virtual-desktop coordinate to the monitor containing it and to monitor-local coordinates. Windows only.",
    mouse_schema,
    win_mouse_to_monitor
);
display_tool!(
    VirtualDisplays,
    "display.virtual_displays",
    "List display adapters/devices, flagging indirect (IddCx) virtual displays. Windows only.",
    empty_schema,
    win_virtual_displays
);

// The native implementations are only compiled for Windows.
#[cfg(windows)]
use win::*;

#[cfg(windows)]
mod win {
    //! Native Windows display implementations.
    use super::*;
    use nebula_mcp_core::ToolError;
    use serde_json::json;
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Foundation::{BOOL, LPARAM, POINT, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{
        DwmGetCompositionTimingInfo, DwmIsCompositionEnabled, DWM_TIMING_INFO,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
    };
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
        MONITOR_DEFAULTTONULL,
    };

    /// `MONITORINFOF_PRIMARY` is not exported by the `windows` crate in this
    /// version; the documented value is 1.
    const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

    fn wide_to_string(w: &[u16]) -> String {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    }

    fn win_err(op: &str, e: windows::core::Error) -> ToolError {
        ToolError::Execution(format!("{op} failed: {e}"))
    }

    /// QueryDisplayConfig enumeration of active paths.
    pub(super) fn win_query_config(_a: &Args) -> Result<Value> {
        unsafe {
            let mut path_count = 0u32;
            let mut mode_count = 0u32;
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                .ok()
                .map_err(|e| win_err("GetDisplayConfigBufferSizes", e))?;

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
            .ok()
            .map_err(|e| win_err("QueryDisplayConfig", e))?;

            let mut out = Vec::new();
            for path in paths.iter().take(path_count as usize) {
                let refresh = {
                    let r = path.targetInfo.refreshRate;
                    if r.Denominator != 0 {
                        r.Numerator as f64 / r.Denominator as f64
                    } else {
                        0.0
                    }
                };
                // Source mode gives resolution + position when available.
                let (width, height, x, y) = {
                    let idx = path.sourceInfo.Anonymous.modeInfoIdx as usize;
                    if idx < modes.len()
                        && modes[idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                    {
                        let sm = modes[idx].Anonymous.sourceMode;
                        (sm.width, sm.height, sm.position.x, sm.position.y)
                    } else {
                        (0, 0, 0, 0)
                    }
                };
                out.push(json!({
                    "sourceId": path.sourceInfo.id,
                    "targetId": path.targetInfo.id,
                    "adapterIdLow": path.targetInfo.adapterId.LowPart,
                    "width": width,
                    "height": height,
                    "positionX": x,
                    "positionY": y,
                    "refreshHz": refresh,
                }));
            }
            Ok(json!({ "pathCount": path_count, "paths": out }))
        }
    }

    /// DXGI adapter + output enumeration.
    pub(super) fn win_dxgi_adapters(_a: &Args) -> Result<Value> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| win_err("CreateDXGIFactory1", e))?;
            let mut adapters = Vec::new();
            let mut i = 0u32;
            loop {
                let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
                    Ok(a) => a,
                    Err(_) => break,
                };
                let desc = adapter.GetDesc1().map_err(|e| win_err("GetDesc1", e))?;
                let mut outputs = Vec::new();
                let mut j = 0u32;
                loop {
                    let output: IDXGIOutput = match adapter.EnumOutputs(j) {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    if let Ok(od) = output.GetDesc() {
                        let r: RECT = od.DesktopCoordinates;
                        outputs.push(json!({
                            "deviceName": wide_to_string(&od.DeviceName),
                            "attachedToDesktop": od.AttachedToDesktop.as_bool(),
                            "left": r.left, "top": r.top, "right": r.right, "bottom": r.bottom,
                        }));
                    }
                    j += 1;
                }
                adapters.push(json!({
                    "description": wide_to_string(&desc.Description),
                    "vendorId": desc.VendorId,
                    "deviceId": desc.DeviceId,
                    "dedicatedVideoMemory": desc.DedicatedVideoMemory as u64,
                    "sharedSystemMemory": desc.SharedSystemMemory as u64,
                    "outputs": outputs,
                }));
                i += 1;
            }
            Ok(json!({ "adapterCount": adapters.len(), "adapters": adapters }))
        }
    }

    /// Monitor topology via EnumDisplayMonitors.
    pub(super) fn win_monitors(_a: &Args) -> Result<Value> {
        unsafe extern "system" fn cb(
            hmon: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            let list = &mut *(data.0 as *mut Vec<serde_json::Value>);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                let m = mi.rcMonitor;
                let w = mi.rcWork;
                list.push(serde_json::json!({
                    "monitorLeft": m.left, "monitorTop": m.top, "monitorRight": m.right, "monitorBottom": m.bottom,
                    "workLeft": w.left, "workTop": w.top, "workRight": w.right, "workBottom": w.bottom,
                    "primary": mi.dwFlags & MONITORINFOF_PRIMARY != 0,
                }));
            }
            TRUE
        }

        let mut list: Vec<serde_json::Value> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                HDC::default(),
                None,
                Some(cb),
                LPARAM(&mut list as *mut _ as isize),
            );
        }
        Ok(json!({ "monitorCount": list.len(), "monitors": list }))
    }

    /// DWM composition state.
    pub(super) fn win_dwm_info(_a: &Args) -> Result<Value> {
        unsafe {
            let enabled = DwmIsCompositionEnabled()
                .map(|b| b.as_bool())
                .unwrap_or(false);
            Ok(json!({ "compositionEnabled": enabled }))
        }
    }

    /// DWM composition timing as a proxy for present statistics.
    pub(super) fn win_present_stats(_a: &Args) -> Result<Value> {
        unsafe {
            let mut info = DWM_TIMING_INFO {
                cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
                ..Default::default()
            };
            DwmGetCompositionTimingInfo(None, &mut info)
                .map_err(|e| win_err("DwmGetCompositionTimingInfo", e))?;
            // Copy fields out of the (packed) struct before referencing them.
            let num = info.rateRefresh.uiNumerator;
            let den = info.rateRefresh.uiDenominator;
            let qpc_refresh_period = info.qpcRefreshPeriod;
            let c_refresh = info.cRefresh;
            let c_dx_refresh = info.cDXRefresh;
            let c_frame = info.cFrame;
            let c_frames_pending = info.cFramesPending;
            let refresh = if den != 0 {
                num as f64 / den as f64
            } else {
                0.0
            };
            Ok(json!({
                "refreshHz": refresh,
                "qpcRefreshPeriod": qpc_refresh_period,
                "cRefresh": c_refresh,
                "cDXRefresh": c_dx_refresh,
                "cFrame": c_frame,
                "cFramesPending": c_frames_pending,
            }))
        }
    }

    /// Advanced colour / HDR detection per active target.
    pub(super) fn win_hdr_detection(_a: &Args) -> Result<Value> {
        unsafe {
            let mut path_count = 0u32;
            let mut mode_count = 0u32;
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                .ok()
                .map_err(|e| win_err("GetDisplayConfigBufferSizes", e))?;
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
            .ok()
            .map_err(|e| win_err("QueryDisplayConfig", e))?;

            let mut out = Vec::new();
            for path in paths.iter().take(path_count as usize) {
                let mut aci = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
                aci.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
                aci.header.size =
                    std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
                aci.header.adapterId = path.targetInfo.adapterId;
                aci.header.id = path.targetInfo.id;
                let rc = DisplayConfigGetDeviceInfo(&mut aci.header);
                if rc == 0 {
                    let bits = aci.Anonymous.value;
                    out.push(json!({
                        "targetId": path.targetInfo.id,
                        "advancedColorSupported": bits & 0x1 != 0,
                        "advancedColorEnabled": bits & 0x2 != 0,
                        "wideColorEnforced": bits & 0x4 != 0,
                    }));
                }
            }
            Ok(json!({ "targets": out }))
        }
    }

    /// Map a screen coordinate to its monitor.
    pub(super) fn win_mouse_to_monitor(a: &Args) -> Result<Value> {
        let x = a
            .opt_i64("x")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'x'".into()))?
            as i32;
        let y = a
            .opt_i64("y")?
            .ok_or_else(|| ToolError::InvalidArguments("missing 'y'".into()))?
            as i32;
        unsafe {
            let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL);
            if hmon.is_invalid() {
                return Ok(json!({ "x": x, "y": y, "monitor": null }));
            }
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return Err(ToolError::Execution("GetMonitorInfoW failed".into()));
            }
            let m = mi.rcMonitor;
            Ok(json!({
                "x": x, "y": y,
                "monitorLeft": m.left, "monitorTop": m.top,
                "monitorRight": m.right, "monitorBottom": m.bottom,
                "localX": x - m.left, "localY": y - m.top,
                "primary": mi.dwFlags & MONITORINFOF_PRIMARY != 0,
            }))
        }
    }

    /// Enumerate display devices, flagging indirect (virtual) displays.
    pub(super) fn win_virtual_displays(_a: &Args) -> Result<Value> {
        unsafe {
            let mut devices = Vec::new();
            let mut i = 0u32;
            loop {
                let mut dd = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(None, i, &mut dd, 0).as_bool() {
                    break;
                }
                let name = wide_to_string(&dd.DeviceString);
                let id = wide_to_string(&dd.DeviceID);
                let is_indirect = name.to_lowercase().contains("indirect")
                    || name.to_lowercase().contains("idd")
                    || id.to_lowercase().contains("iddcx");
                devices.push(json!({
                    "deviceName": wide_to_string(&dd.DeviceName),
                    "deviceString": name,
                    "deviceId": id,
                    "active": dd.StateFlags & 0x1 != 0,
                    "indirect": is_indirect,
                }));
                i += 1;
            }
            Ok(json!({ "deviceCount": devices.len(), "devices": devices }))
        }
    }

    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
}
