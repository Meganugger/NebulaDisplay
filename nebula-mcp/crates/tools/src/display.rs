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
        Arc::new(EnumModes),
        Arc::new(DuplicateFrame),
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

fn enum_modes_schema() -> Value {
    ObjectSchema::new()
        .string(
            "deviceName",
            "Adapter device name (e.g. \\\\.\\DISPLAY1); omit for the primary adapter.",
            false,
        )
        .integer("maxModes", "Maximum modes to return (default 300).", false)
        .build()
}
display_tool!(
    EnumModes,
    "display.enum_modes",
    "Enumerate the supported display modes (resolution, colour depth, refresh) for an adapter via EnumDisplaySettingsEx. Windows only.",
    enum_modes_schema,
    win_enum_modes
);

/// Capture a single desktop frame via DXGI Desktop Duplication.
struct DuplicateFrame;

#[async_trait]
impl Tool for DuplicateFrame {
    fn name(&self) -> &str {
        "display.duplicate_frame"
    }
    fn category(&self) -> &str {
        CATEGORY
    }
    fn description(&self) -> &str {
        "Capture one frame of the primary output via DXGI Desktop Duplication. Returns a PNG image \
         (optionally downscaled), or writes it to outputPath. Requires an interactive desktop session. Windows only."
    }
    fn input_schema(&self) -> Value {
        ObjectSchema::new()
            .integer("maxDimension", "Downscale so the longest side is at most this many pixels (default 1920; 0 = no downscale).", false)
            .integer("acquireTimeoutMs", "Timeout waiting for a new frame (default 700).", false)
            .string("outputPath", "If set, write the PNG here (within an allowed root) and return metadata instead of embedding the image.", false)
            .build()
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
            return win_duplicate_frame(ctx, &a);
        }
        #[cfg(not(windows))]
        {
            let _ = &a;
            Err(nebula_mcp_core::ToolError::PlatformUnsupported(
                "display.duplicate_frame".to_string(),
            ))
        }
    }
}

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

    /// Enumerate supported display modes for an adapter.
    pub(super) fn win_enum_modes(a: &Args) -> Result<Value> {
        use windows::core::PCWSTR;
        use windows::Win32::Graphics::Gdi::{
            EnumDisplaySettingsExW, DEVMODEW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
            ENUM_DISPLAY_SETTINGS_MODE,
        };

        let device = a.opt_str("deviceName")?.map(|s| s.to_string());
        let max_modes = a.opt_i64("maxModes")?.unwrap_or(300).max(1) as usize;

        // Build a wide, NUL-terminated device name if provided.
        let wide: Option<Vec<u16>> = device
            .as_ref()
            .map(|d| d.encode_utf16().chain(std::iter::once(0)).collect());
        let name_ptr = match &wide {
            Some(w) => PCWSTR(w.as_ptr()),
            None => PCWSTR::null(),
        };

        unsafe {
            // Current mode first.
            let mut current = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            let has_current = EnumDisplaySettingsExW(
                name_ptr,
                ENUM_CURRENT_SETTINGS,
                &mut current,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
            .as_bool();
            let current_json = if has_current {
                json!({
                    "width": current.dmPelsWidth,
                    "height": current.dmPelsHeight,
                    "bitsPerPel": current.dmBitsPerPel,
                    "frequency": current.dmDisplayFrequency,
                })
            } else {
                Value::Null
            };

            let mut modes = Vec::new();
            let mut i = 0u32;
            loop {
                if modes.len() >= max_modes {
                    break;
                }
                let mut dm = DEVMODEW {
                    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                if !EnumDisplaySettingsExW(
                    name_ptr,
                    ENUM_DISPLAY_SETTINGS_MODE(i),
                    &mut dm,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
                {
                    break;
                }
                modes.push(json!({
                    "width": dm.dmPelsWidth,
                    "height": dm.dmPelsHeight,
                    "bitsPerPel": dm.dmBitsPerPel,
                    "frequency": dm.dmDisplayFrequency,
                }));
                i += 1;
            }
            Ok(json!({
                "device": device,
                "current": current_json,
                "modeCount": modes.len(),
                "modes": modes,
            }))
        }
    }

    use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};

    /// Capture one desktop frame via DXGI Desktop Duplication and return it as a
    /// PNG (embedded base64 or written to `outputPath`).
    pub(super) fn win_duplicate_frame(
        ctx: &ToolContext,
        a: &Args,
    ) -> Result<nebula_mcp_protocol::CallToolResult> {
        use base64::Engine as _;
        use nebula_mcp_protocol::{CallToolResult, Content};
        use windows::core::Interface;
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
            D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::{
            IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        };

        let max_dim = a.opt_i64("maxDimension")?.unwrap_or(1920).max(0) as u32;
        let acquire_timeout = a.opt_i64("acquireTimeoutMs")?.unwrap_or(700).max(1) as u32;
        let output_path = match a.opt_str("outputPath")? {
            Some(p) => Some(ctx.resolve_path(p)?),
            None => None,
        };
        let max_output = ctx.policy.max_output_bytes();

        unsafe {
            // Create a D3D11 device.
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL::default();
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .map_err(|e| win_err("D3D11CreateDevice", e))?;
            let device = device.ok_or_else(|| ToolError::Execution("no D3D11 device".into()))?;
            let context = context.ok_or_else(|| ToolError::Execution("no D3D11 context".into()))?;

            // Reach the primary output and start duplication.
            let dxgi_device: IDXGIDevice =
                device.cast().map_err(|e| win_err("cast IDXGIDevice", e))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|e| win_err("GetAdapter", e))?;
            let output = adapter
                .EnumOutputs(0)
                .map_err(|e| win_err("EnumOutputs", e))?;
            let output1: IDXGIOutput1 =
                output.cast().map_err(|e| win_err("cast IDXGIOutput1", e))?;
            let dupl: IDXGIOutputDuplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| win_err("DuplicateOutput", e))?;

            // Acquire a frame (retry across timeouts up to the budget).
            let mut resource: Option<IDXGIResource> = None;
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut acquired = false;
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(acquire_timeout as u64 * 4);
            while std::time::Instant::now() < deadline {
                match dupl.AcquireNextFrame(acquire_timeout, &mut frame_info, &mut resource) {
                    Ok(()) => {
                        acquired = true;
                        break;
                    }
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                        let _ = dupl.ReleaseFrame();
                        continue;
                    }
                    Err(e) => return Err(win_err("AcquireNextFrame", e)),
                }
            }
            if !acquired {
                let _ = dupl.ReleaseFrame();
                return Err(ToolError::Execution(
                    "timed out waiting for a desktop frame (is anything rendering?)".into(),
                ));
            }
            let resource =
                resource.ok_or_else(|| ToolError::Execution("no frame resource".into()))?;
            let frame_tex: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| win_err("cast ID3D11Texture2D", e))?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame_tex.GetDesc(&mut desc);

            // Staging copy readable by the CPU.
            let mut staging_desc = desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|e| win_err("CreateTexture2D", e))?;
            let staging =
                staging.ok_or_else(|| ToolError::Execution("no staging texture".into()))?;
            context.CopyResource(&staging, &frame_tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| win_err("Map", e))?;

            let width = desc.Width as usize;
            let height = desc.Height as usize;
            let row_pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            let mut rgba = vec![0u8; width * height * 4];
            for y in 0..height {
                let row = std::slice::from_raw_parts(src.add(y * row_pitch), width * 4);
                for x in 0..width {
                    let i = x * 4;
                    let o = (y * width + x) * 4;
                    // Source is BGRA; store as RGBA.
                    rgba[o] = row[i + 2];
                    rgba[o + 1] = row[i + 1];
                    rgba[o + 2] = row[i];
                    rgba[o + 3] = 255;
                }
            }
            context.Unmap(&staging, 0);
            let _ = dupl.ReleaseFrame();

            // Build the image, optionally downscaling.
            let mut img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
                .ok_or_else(|| ToolError::Internal("failed to build image buffer".into()))?;
            let (mut out_w, mut out_h) = (width as u32, height as u32);
            if max_dim > 0 && out_w.max(out_h) > max_dim {
                let scale = max_dim as f64 / out_w.max(out_h) as f64;
                out_w = ((out_w as f64) * scale).round().max(1.0) as u32;
                out_h = ((out_h as f64) * scale).round().max(1.0) as u32;
                img = image::imageops::resize(
                    &img,
                    out_w,
                    out_h,
                    image::imageops::FilterType::Triangle,
                );
            }

            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| ToolError::Internal(format!("PNG encode failed: {e}")))?;

            if let Some(path) = output_path {
                std::fs::write(&path, &png)
                    .map_err(|e| ToolError::Io(format!("writing {}: {e}", path.display())))?;
                return Ok(CallToolResult {
                    content: vec![Content::text(
                        json!({
                            "outputPath": path.display().to_string(),
                            "width": out_w,
                            "height": out_h,
                            "sourceWidth": width,
                            "sourceHeight": height,
                            "bytes": png.len(),
                        })
                        .to_string(),
                    )],
                    is_error: Some(false),
                });
            }

            // Embed as base64. Guard against exceeding the output cap.
            if png.len() * 4 / 3 > max_output {
                return Err(ToolError::OutputTooLarge { limit: max_output });
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(CallToolResult {
                content: vec![
                    Content::Image {
                        data,
                        mime_type: "image/png".to_string(),
                    },
                    Content::text(
                        json!({"width": out_w, "height": out_h, "sourceWidth": width, "sourceHeight": height})
                            .to_string(),
                    ),
                ],
                is_error: Some(false),
            })
        }
    }
}
