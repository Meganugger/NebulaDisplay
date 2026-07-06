// NebulaDisplay Indirect Display Driver (UMDF v2 + IddCx).
//
// Clean-room implementation written against Microsoft's public IddCx API
// documentation (https://learn.microsoft.com/windows-hardware/drivers/display/
// indirect-display-driver-model-overview). No third-party driver code was
// consulted or copied.
//
// Responsibilities:
//  * expose one virtual monitor (hot-pluggable) with a set of modes
//  * synthesize a valid EDID at runtime (VESA E-EDID 1.4 base block)
//  * process swap-chain frames on a dedicated thread and publish them into
//    the shared-memory frame ring consumed by the nebulad service
//    (see ../include/ndsp_frame_ring.h)

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

// ---------------------------------------------------------------------------
// Mode table offered by the virtual monitor.
// ---------------------------------------------------------------------------
struct NdspMode {
    UINT32 width;
    UINT32 height;
    UINT32 vsync_hz;
};

inline constexpr std::array<NdspMode, 8> kModes = {{
    {1920, 1080, 60},
    {1920, 1080, 30},
    {2560, 1440, 60},
    {3840, 2160, 30},
    {1680, 1050, 60},
    {1366, 768, 60},
    {1280, 720, 60},
    {1024, 768, 60},
}};
inline constexpr UINT32 kPreferredModeIndex = 0;

// ---------------------------------------------------------------------------
// Frame ring producer (driver → service).
// ---------------------------------------------------------------------------
class FrameRing {
public:
    FrameRing() = default;
    ~FrameRing();
    FrameRing(const FrameRing&) = delete;
    FrameRing& operator=(const FrameRing&) = delete;

    HRESULT Initialize();
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
    SwapChainProcessor(IDDCX_SWAPCHAIN swapChain, HANDLE newFrameEvent, FrameRing* ring);
    ~SwapChainProcessor();
    SwapChainProcessor(const SwapChainProcessor&) = delete;
    SwapChainProcessor& operator=(const SwapChainProcessor&) = delete;

private:
    static DWORD CALLBACK ThreadProc(LPVOID arg);
    void Run();
    HRESULT ProcessFrames();

    IDDCX_SWAPCHAIN m_swapChain;
    HANDLE m_newFrameEvent;    // owned by IddCx runtime
    FrameRing* m_ring;
    HANDLE m_thread = nullptr;
    HANDLE m_stopEvent = nullptr;

    // D3D device the swap chain runs on.
    Microsoft::WRL::ComPtr<ID3D11Device> m_device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> m_context;
    Microsoft::WRL::ComPtr<IDXGIDevice> m_dxgiDevice;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_staging;
    UINT32 m_stagingW = 0, m_stagingH = 0;
};

// ---------------------------------------------------------------------------
// Per-object contexts wired into WDF/IddCx handles.
// ---------------------------------------------------------------------------
struct DeviceContext {
    WDFDEVICE wdfDevice = nullptr;
    IDDCX_ADAPTER adapter = nullptr;
    IDDCX_MONITOR monitor = nullptr;
    std::unique_ptr<FrameRing> ring;
    std::unique_ptr<SwapChainProcessor> processor;
    UINT32 currentWidth = 1920;
    UINT32 currentHeight = 1080;
    UINT32 currentHz = 60;
};

WDF_DECLARE_CONTEXT_TYPE(DeviceContext);

// IddCx objects are WDF objects; we attach a thin wrapper pointing back to
// the owning device's context (standard IddCx pattern).
struct AdapterContextWrapper {
    DeviceContext* ctx;
};
struct MonitorContextWrapper {
    DeviceContext* ctx;
};
WDF_DECLARE_CONTEXT_TYPE(AdapterContextWrapper);
WDF_DECLARE_CONTEXT_TYPE(MonitorContextWrapper);

// EDID synthesis (128-byte base block, checksum computed at runtime).
std::array<BYTE, 128> BuildNdspEdid();

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
