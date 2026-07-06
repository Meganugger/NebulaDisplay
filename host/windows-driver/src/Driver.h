// NebulaDisplay Indirect Display Driver (UMDF v2 + IddCx).
//
// Clean-room implementation written against Microsoft's public IddCx API
// documentation (https://learn.microsoft.com/windows-hardware/drivers/display/
// indirect-display-driver-model-overview). No third-party driver code was
// consulted or copied.
//
// Responsibilities:
//  * expose 1–4 virtual monitors (count from the device registry key) with a
//    configurable mode table (defaults + registry-supplied custom modes)
//  * synthesize a valid EDID per monitor at runtime (VESA E-EDID 1.4 base
//    block, distinct serial numbers so Windows persists per-monitor layout)
//  * process swap-chain frames on a dedicated MMCSS thread per monitor and
//    publish them into that monitor's shared-memory frame ring consumed by
//    the nebulad service (see ../include/ndsp_frame_ring.h)
//  * render on the GPU the OS asks for (RenderAdapterLuid) — multi-GPU safe
//  * report HDR10 / 10-bit wire-format capability when built against and
//    running on IddCx 1.10+ (Windows 11 22H2+)
//
// ---------------------------------------------------------------------------
// IddCx version / WDK compatibility matrix
// ---------------------------------------------------------------------------
// API                                   | introduced in | header gate
// --------------------------------------+---------------+---------------------
// IddCxSwapChainFinishedProcessingFrame | IddCx 1.4     | IDDCX_VERSION_MINOR
//                                       | (Win10 2004,  |   >= 4
//                                       |  WDK 10.19041)|
// IDDCX_ENDPOINT_VERSION (struct used   | IddCx 1.0     | always present
//   for pFirmware/pHardwareVersion)     |               |
// EvtIddCxParseMonitorDescription2 /    | IddCx 1.10    | IDDCX_VERSION_MINOR
//   QueryTargetModes2 / CommitModes2 /  | (Win11 22H2,  |   >= 10
//   AdapterQueryTargetInfo / HDR types  |  WDK 10.22621)|
//
// The vcxproj compiles with IDDCX_VERSION_MINOR set from the WDK in use
// (default 10; pass /p:IddcxMinor=4 for older WDKs) and
// IDDCX_MINIMUM_VERSION_REQUIRED=4, i.e. the built driver loads on
// Windows 10 2004+ and Windows 11. On IddCx >= 1.10 hosts the OS calls the
// *2 mode callbacks (HDR-capable); on 1.4–1.9 hosts it calls the classic
// ones — both are implemented below and delegate to shared helpers.

#pragma once

#include <windows.h>
#include <wdf.h>
#include <iddcx.h>

#include <d3d11.h>
#include <dxgi1_4.h>
#include <wrl.h>

#include <array>
#include <memory>
#include <vector>

#include "../include/ndsp_frame_ring.h"

// Compile-time feature detection derived from the IddCx headers in use.
#if defined(IDDCX_VERSION_MINOR) && (IDDCX_VERSION_MINOR >= 4)
#define NDSP_IDDCX_HAS_FINISHED_PROCESSING_FRAME 1
#else
#define NDSP_IDDCX_HAS_FINISHED_PROCESSING_FRAME 0
#endif

#if defined(IDDCX_VERSION_MINOR) && (IDDCX_VERSION_MINOR >= 10)
#define NDSP_IDDCX_HAS_HDR 1
#else
#define NDSP_IDDCX_HAS_HDR 0
#endif

// ---------------------------------------------------------------------------
// Mode table offered by every virtual monitor.
// ---------------------------------------------------------------------------
struct NdspMode {
    UINT32 width;
    UINT32 height;
    UINT32 vsync_hz;
};

// Built-in defaults (registry can add more; see LoadConfiguredModes). All
// fit within the frame-ring slot size (NDSP_MAX_WIDTH × NDSP_MAX_HEIGHT).
inline constexpr std::array<NdspMode, 14> kDefaultModes = {{
    {1920, 1080, 60},
    {1920, 1080, 120},
    {1920, 1080, 30},
    {2560, 1440, 60},
    {2560, 1440, 120},
    {3840, 2160, 60},
    {3840, 2160, 30},
    {2560, 1080, 60},
    {3440, 1440, 60},
    {1680, 1050, 60},
    {1600, 900, 60},
    {1366, 768, 60},
    {1280, 720, 60},
    {1024, 768, 60},
}};
inline constexpr UINT32 kPreferredModeIndex = 0;
inline constexpr UINT32 kMaxMonitors = 4;

// ---------------------------------------------------------------------------
// Frame ring producer (driver → service). One ring per monitor.
// ---------------------------------------------------------------------------
class FrameRing {
public:
    FrameRing() = default;
    ~FrameRing();
    FrameRing(const FrameRing&) = delete;
    FrameRing& operator=(const FrameRing&) = delete;

    HRESULT Initialize(UINT32 monitorIndex);
    void SetMode(UINT32 width, UINT32 height, UINT32 refreshHz);
    void SetConnected(bool connected);
    // Copy one BGRA frame (CPU pointer, given pitch) into the next slot.
    void PublishFrame(const void* data, UINT32 width, UINT32 height, UINT32 srcPitch,
                      UINT64 timestampQpc);

private:
    HANDLE m_mapping = nullptr;
    HANDLE m_frameEvent = nullptr;
    ndsp::RingHeader* m_header = nullptr;
    BYTE* m_base = nullptr;
    UINT64 m_frameNumber = 0;
};

// ---------------------------------------------------------------------------
// Swap-chain processing thread. One per assigned swap chain.
// ---------------------------------------------------------------------------
class SwapChainProcessor {
public:
    // `renderAdapter` — LUID of the GPU the OS renders this monitor's
    // desktop on; the D3D device MUST be created on that adapter (multi-GPU
    // systems render different monitors on different GPUs).
    SwapChainProcessor(IDDCX_SWAPCHAIN swapChain, HANDLE newFrameEvent, LUID renderAdapter,
                       FrameRing* ring);
    ~SwapChainProcessor();
    SwapChainProcessor(const SwapChainProcessor&) = delete;
    SwapChainProcessor& operator=(const SwapChainProcessor&) = delete;

private:
    static DWORD CALLBACK ThreadProc(LPVOID arg);
    void Run();
    HRESULT CreateDeviceOnRenderAdapter();
    HRESULT ProcessFrames();

    IDDCX_SWAPCHAIN m_swapChain;
    HANDLE m_newFrameEvent;    // owned by IddCx runtime
    LUID m_renderAdapter;
    FrameRing* m_ring;
    HANDLE m_thread = nullptr;
    HANDLE m_stopEvent = nullptr;

    // D3D device the swap chain runs on (created on m_renderAdapter).
    Microsoft::WRL::ComPtr<ID3D11Device> m_device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> m_context;
    Microsoft::WRL::ComPtr<IDXGIDevice> m_dxgiDevice;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_staging;
    UINT32 m_stagingW = 0, m_stagingH = 0;
};

// ---------------------------------------------------------------------------
// Per-object contexts wired into WDF/IddCx handles.
// ---------------------------------------------------------------------------
struct DeviceContext;

// Per-monitor state (up to kMaxMonitors per adapter).
struct MonitorState {
    IDDCX_MONITOR monitor = nullptr;
    std::unique_ptr<FrameRing> ring;
    std::unique_ptr<SwapChainProcessor> processor;
    // 128-byte EDID for this connector (distinct serial per monitor).
    std::array<BYTE, 128> edid{};
    UINT32 currentWidth = 1920;
    UINT32 currentHeight = 1080;
    UINT32 currentHz = 60;
};

struct DeviceContext {
    WDFDEVICE wdfDevice = nullptr;
    IDDCX_ADAPTER adapter = nullptr;
    UINT32 monitorCount = 1;                 // from registry, clamped 1..kMaxMonitors
    std::array<MonitorState, kMaxMonitors> monitors;
    std::vector<NdspMode> modes;             // defaults + registry extras

    MonitorState* FindMonitor(IDDCX_MONITOR monitor) {
        for (UINT32 i = 0; i < monitorCount; ++i) {
            if (monitors[i].monitor == monitor) return &monitors[i];
        }
        return nullptr;
    }
};

WDF_DECLARE_CONTEXT_TYPE(DeviceContext);

// IddCx objects are WDF objects; we attach a thin wrapper pointing back to
// the owning device's context (standard IddCx pattern).
struct AdapterContextWrapper {
    DeviceContext* ctx;
};
struct MonitorContextWrapper {
    DeviceContext* ctx;
    MonitorState* state;
};
WDF_DECLARE_CONTEXT_TYPE(AdapterContextWrapper);
WDF_DECLARE_CONTEXT_TYPE(MonitorContextWrapper);

// EDID synthesis (128-byte base block, checksum computed at runtime).
// `serial` distinguishes multiple NebulaDisplay monitors so the OS persists
// independent layout/scale settings for each.
std::array<BYTE, 128> BuildNdspEdid(UINT32 serial);

// WDF/IddCx callbacks.
extern "C" DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD NdspEvtDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY NdspEvtD0Entry;

EVT_IDD_CX_ADAPTER_INIT_FINISHED NdspAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES NdspAdapterCommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION NdspParseMonitorDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES NdspMonitorGetDefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES NdspMonitorQueryModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN NdspMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN NdspMonitorUnassignSwapChain;

#if NDSP_IDDCX_HAS_HDR
// IddCx 1.10+ variants: identical mode tables with wire-format metadata
// (8-bit SDR always; 10-bit reported so HDR-capable pipelines can engage).
EVT_IDD_CX_ADAPTER_QUERY_TARGET_INFO NdspAdapterQueryTargetInfo;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION2 NdspParseMonitorDescription2;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES2 NdspMonitorQueryModes2;
EVT_IDD_CX_ADAPTER_COMMIT_MODES2 NdspAdapterCommitModes2;
EVT_IDD_CX_MONITOR_SET_DEFAULT_HDR_METADATA NdspMonitorSetDefaultHdrMetadata;
#endif
